//! 托盘 tooltip 刷新：代际合并 + 主线程写入。
//!
//! 事件（`auth://state` / `quota://updated`）常常在极短时间内多次到达；这里
//! 用代际合并：150ms 窗口内的 N 次请求只执行一次刷新、只投递一次主线程，
//! 且窗口结束前到达的最后一次请求不会被丢弃（[`RefreshGate::finish`]）。

use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

use tauri::{AppHandle, Manager};
use tracing::{debug, warn};

use super::{tooltip, TrayState};
use crate::state::AppState;

/// 合并同一窗口内的多个刷新请求；tooltip 不是实时 UI，150ms 延迟不可感知。
const TOOLTIP_REFRESH_DEBOUNCE: Duration = Duration::from_millis(150);

/// 合并门：把"请求刷新"与"正在刷新"分开，保证同一时刻最多一个刷新任务。
#[derive(Default)]
pub(crate) struct RefreshGate {
    /// 已请求的代次；只增不减。
    generation: AtomicU64,
    /// 是否已有刷新任务在跑。
    running: AtomicBool,
}

impl RefreshGate {
    /// 记一次刷新请求；返回 true 表示本次请求需要启动刷新任务。
    pub(crate) fn request(&self) -> bool {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// 当前代次快照：刷新前取，完成后交给 [`RefreshGate::finish`]。
    pub(crate) fn snapshot(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// 一次刷新完成。返回 true 表示期间又来了请求（继续下一轮），false 表示可退出。
    pub(crate) fn finish(&self, seen: u64) -> bool {
        if self.generation.load(Ordering::SeqCst) != seen {
            return true;
        }
        self.running.store(false, Ordering::SeqCst);
        if self.generation.load(Ordering::SeqCst) != seen {
            // 释放瞬间来了新请求：没有别的任务接手时，由本任务继续持有。
            return !self.running.swap(true, Ordering::SeqCst);
        }
        false
    }
}

/// 进程内唯一合并门：所有刷新请求共用。
static REFRESH_GATE: RefreshGate = RefreshGate {
    generation: AtomicU64::new(0),
    running: AtomicBool::new(false),
};

/// 请求刷新一次托盘 tooltip：可从任意线程/事件回调调用。
///
/// 读凭据/数据库在后台任务里完成，**写托盘固定回主线程**：Tauri 的托盘 API
/// （`run_item_main_thread!`）会在调用线程克隆内部 `Rc<RefCell<…>>`，而
/// `TrayIcon` 是 `unsafe impl Send + Sync`，从后台线程直接调用属于未定义行为；
/// 历史上曾因此在托盘销毁路径上触发 `RefCell` 借用冲突，整个进程 abort。
pub(crate) fn request_refresh(app: AppHandle) {
    if !REFRESH_GATE.request() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(TOOLTIP_REFRESH_DEBOUNCE).await;
            let seen = REFRESH_GATE.snapshot();
            let text = collect_tooltip_text(&app).await;
            let handle = app.clone();
            if let Err(err) = app.run_on_main_thread(move || apply_tooltip(&handle, text)) {
                debug!(error = %err, "主线程不可用，跳过托盘 tooltip 刷新");
            }
            if !REFRESH_GATE.finish(seen) {
                break;
            }
        }
    });
}

/// 后台部分：读取本地额度快照并生成 tooltip 文本；不触碰托盘。
async fn collect_tooltip_text(app: &AppHandle) -> String {
    let Some(auth) = app.try_state::<AppState>().map(|state| state.auth.clone()) else {
        return tooltip::TOOLTIP_EMPTY.to_string();
    };
    let quota = if auth.has_credentials().await {
        match auth.latest_quota().await {
            Ok(quota) => quota,
            Err(err) => {
                warn!(error = %err, "读取额度快照失败，托盘 tooltip 显示占位");
                None
            }
        }
    } else {
        None
    };

    tooltip::format_tooltip(quota.as_ref())
}

/// 主线程部分：只在文本变化时写入托盘，并记录上次成功写入的文本。
fn apply_tooltip(app: &AppHandle, text: String) {
    let Some(state) = app.try_state::<TrayState>() else {
        return;
    };
    if state.quitting.load(Ordering::SeqCst) {
        debug!("正在退出，跳过托盘 tooltip 刷新");
        return;
    }
    debug_assert_eq!(
        std::thread::current().id(),
        state.main_thread,
        "托盘写入必须在主线程"
    );

    // 只短暂持有锁：`set_tooltip` 会同步派发窗口消息，避免在锁内触发回调重入。
    let previous = state
        .last_tooltip
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if !tooltip::should_apply(previous.as_deref(), &text) {
        debug!(tooltip = %text, "托盘 tooltip 未变化，跳过");
        return;
    }

    if let Err(err) = state.icon.set_tooltip(Some(text.clone())) {
        warn!(error = %err, "更新托盘 tooltip 失败");
        return;
    }
    *state
        .last_tooltip
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(text.clone());
    debug!(tooltip = %text, "托盘 tooltip 已更新");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同一窗口内的 N 次请求只启动一次任务；任务收尾后新请求仍能再次启动，
    /// 且**执行期间**到达的请求不会被丢弃（finish 返回 true 继续下一轮）。
    #[test]
    fn burst_within_window_runs_once_and_never_loses_a_request() {
        let gate = RefreshGate::default();

        // 一个 150ms 窗口内的 10 次事件：只启动 1 个任务。
        assert!(gate.request(), "首个请求应启动刷新任务");
        for _ in 0..9 {
            assert!(!gate.request(), "窗口内其余请求应被合并");
        }
        let seen = gate.snapshot();
        assert!(
            !gate.finish(seen),
            "窗口内请求都已计入本次执行，应释放合并门"
        );

        // 释放后的新请求重新启动任务；执行期间再来的请求必须触发下一轮。
        assert!(gate.request(), "释放后的新请求应重新启动任务");
        let seen = gate.snapshot();
        assert!(!gate.request(), "执行期间的请求应被合并进下一轮");
        assert!(
            gate.finish(seen),
            "执行期间有新请求时不得丢弃，必须继续下一轮"
        );

        let seen = gate.snapshot();
        assert!(!gate.finish(seen), "没有新请求时应释放合并门");
    }
}
