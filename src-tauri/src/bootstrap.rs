//! 启动装配：数据目录、共享状态初始化、后台任务与阶段计时。
//!
//! setup 是主线程同步闭包：这里的每一步都会推迟首个 IPC 应答（前端挂载即调
//! `auth_status`）。因此能延后的（scheme 注册、托盘首刷）都不在关键路径上，
//! 必要步骤用 [`PhaseTimer`] 各记一行 `phase`/`elapsed_ms`。

use std::{path::Path, time::Duration, time::Instant};

use tauri::{AppHandle, Emitter, Manager};

use crate::{db, settings, startup, state::AppState, tray};

/// 保留期清理周期（启动后立即一次，之后每天一次）。
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// 解析并创建数据目录；失败时弹窗+落盘并返回 `None`（窗口尚未创建，调用方直接退出）。
pub(crate) fn prepare_data_dir() -> Option<std::path::PathBuf> {
    let data_dir = match settings::default_data_dir() {
        Ok(dir) => dir,
        Err(err) => {
            // 主目录都定位不到时无法使用标准日志目录：退到临时目录，保证弹窗里
            // 给出的日志路径可用。
            let fallback = std::env::temp_dir().join(settings::APP_DIR_NAME);
            startup::report(&fallback, "定位数据目录", &err.to_string());
            return None;
        }
    };
    if let Err(err) = settings::ensure_dir(&data_dir) {
        startup::report(
            &data_dir,
            "创建数据目录",
            &format!("无法创建 {}：{err}", data_dir.display()),
        );
        return None;
    }
    Some(data_dir)
}

/// 启动阶段计时：每次 [`PhaseTimer::lap`] 输出上一阶段耗时。
pub(crate) struct PhaseTimer {
    last: Instant,
}

impl PhaseTimer {
    pub(crate) fn new() -> Self {
        Self {
            last: Instant::now(),
        }
    }

    /// 结束上一阶段并记录 `info!(phase, elapsed_ms)`。
    pub(crate) fn lap(&mut self, phase: &'static str) {
        let started = self.last;
        self.last = Instant::now();
        log_phase(phase, started);
    }
}

/// 记录一个阶段耗时（毫秒，保留 3 位小数）。
pub(crate) fn log_phase(phase: &'static str, started: Instant) {
    tracing::info!(
        phase,
        elapsed_ms = elapsed_ms(started.elapsed()),
        "启动阶段耗时"
    );
}

fn elapsed_ms(elapsed: Duration) -> f64 {
    (elapsed.as_secs_f64() * 1_000_000.0).round() / 1_000.0
}

/// 设置 → 数据库/迁移 → 共享状态。失败必须可见：release 没有控制台。
pub(crate) fn init_state(
    app: &tauri::App,
    data_dir: &Path,
    timer: &mut PhaseTimer,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1) 设置：解析/校验失败时备份原文件并回退默认值，不让手改坏的文件卡死启动。
    let loaded_settings = settings::Settings::load_or_recover(data_dir);
    tracing::info!(
        gateway = %loaded_settings.gateway_url,
        proxy_bind = %loaded_settings.proxy_bind,
        proxy_port = loaded_settings.proxy_port,
        record_body = loaded_settings.record_body,
        "设置加载完成"
    );
    timer.lap("settings");

    // 2) 数据库 + 迁移。打开/迁移失败会弹窗并写 logs\startup-error.log。
    let db = match db::Db::open(&data_dir.join(db::DB_FILE_NAME)) {
        Ok(db) => db,
        Err(err) => {
            startup::report(data_dir, "打开数据库", &err.to_string());
            return Err(err.into());
        }
    };
    timer.lap("db_open");
    if let Err(err) = db::run_migrations(&db) {
        startup::report(data_dir, "数据库迁移", &err.to_string());
        return Err(err.into());
    }
    tracing::info!("数据库迁移完成");
    timer.lap("migrations");

    // 3) 共享状态：之后所有命令与后台任务都从这里取句柄。
    app.manage(AppState::new(data_dir.to_path_buf(), loaded_settings, db));
    timer.lap("state");
    Ok(())
}

/// 后台任务：严格加载凭据（含首个 tick）、本地代理常驻、保留期清理。
///
/// 都在 Tauri 内置 Tokio runtime 里跑，不阻塞 setup。
pub(crate) fn spawn_background(app: AppHandle) {
    spawn_auth_load(app.clone());
    spawn_proxy(app.clone());
    spawn_maintenance(app);
}

fn spawn_auth_load(app: AppHandle) {
    let auth = match app.try_state::<AppState>() {
        Some(state) => state.auth.clone(),
        None => {
            tracing::warn!("启动后台任务时 AppState 尚未注册");
            return;
        }
    };

    let auth_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        match auth.load_strict().await {
            Ok(()) => {
                if auth.has_credentials().await {
                    auth.ensure_loops(auth_handle).await;
                }
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "本地凭据加载失败（不降级为明文）；请重新登录"
                );
                let status = auth.status().await;
                let _ = auth_handle.emit("auth://state", &status);
            }
        }
        // 认证加载完成后再读一次本地额度，避免托盘一直显示启动占位。
        tray::request_refresh(app);
    });
}

/// 保留期清理：启动后立即跑一次，之后每 24h 一次。
///
/// `interval` 的首个 tick 立即返回；`MissedTickBehavior::Delay` 让机器休眠/卡顿
/// 恢复后最多补一次，不会连环补跑。`retention_days` 每轮从设置快照读取，
/// 用户改完下一轮即生效，无需重启。
fn spawn_maintenance(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(MAINTENANCE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let Some(state) = app.try_state::<AppState>() else {
                tracing::warn!("保留期清理时 AppState 尚未注册");
                return;
            };
            let retention_days = state.settings.snapshot().await.retention_days;
            match db::maintenance::run_maintenance(&state.db, retention_days).await {
                Ok(report) => tracing::info!(
                    retention_days,
                    removed_requests = report.requests_deleted,
                    removed_sessions = report.sessions_deleted,
                    removed_usage_days = report.usage_days_deleted,
                    removed_quota_snapshots = report.quota_snapshots_deleted,
                    "保留期清理完成"
                ),
                Err(err) => {
                    tracing::warn!(error = %err, "保留期清理失败，保留数据待下次重试")
                }
            }
        }
    });
}

fn spawn_proxy(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let Some(state) = app.try_state::<AppState>() else {
            tracing::warn!("启动代理时 AppState 尚未注册");
            return;
        };
        let settings = state.settings.read().await.clone();
        let proxy = state.proxy.clone();
        // 代理策略由 AppState 持有：设置保存后写回，下一次请求即生效。
        let policy = state.policy.clone();

        match proxy
            .start(
                &settings.proxy_bind,
                settings.proxy_port,
                &settings.gateway_url,
                policy,
            )
            .await
        {
            Ok(status) => {
                tracing::info!(
                    port = status.port,
                    health = %status.health_url,
                    "本地代理已就绪（三协议转发）"
                );
                let _ = app.emit("proxy://status", &status);
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "代理启动失败；应用继续运行，可调用 proxy_restart 重试"
                );
                let _ = app.emit(
                    "proxy://error",
                    serde_json::json!({ "message": err.to_string() }),
                );
            }
        }
    });
}
