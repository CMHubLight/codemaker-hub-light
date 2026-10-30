//! 系统托盘：常驻图标、右键菜单与额度 tooltip。
//!
//! 行为约定：
//! - 右键菜单只有“打开主界面”“退出”；左键单击也打开主界面；
//! - 窗口关闭（X / Alt+F4）只隐藏，不退出进程（拦截见 [`crate::window`]）；
//! - tooltip 由本地 `quota_snapshots` 最新快照生成，不发起网络请求；
//! - token 不进入托盘模块，tooltip 只包含额度数字；
//! - 托盘句柄只在主线程读写：后台只负责算文本，写托盘经
//!   `AppHandle::run_on_main_thread` 回到主线程（见 [`refresh`]）。

mod refresh;
mod tooltip;

use std::sync::{atomic::AtomicBool, Mutex};

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    AppHandle, Listener, Manager, Wry,
};

pub(crate) use refresh::request_refresh;

/// 托盘图标唯一 id。
const TRAY_ID: &str = "main-tray";
/// 打开主界面菜单项 id。
const MENU_OPEN_MAIN: &str = "open-main";
/// 退出菜单项 id。
const MENU_QUIT: &str = "quit";

/// 托盘运行态；`TrayIcon` 必须被持有，否则图标会在 drop 时消失。
pub(crate) struct TrayState {
    pub(crate) icon: TrayIcon<Wry>,
    /// 托盘“退出”时置 true，关闭事件处理器据此放行窗口销毁。
    pub(crate) quitting: AtomicBool,
    /// 创建托盘的主线程 id；debug 构建据此断言托盘写入没有跑偏到后台线程。
    main_thread: std::thread::ThreadId,
    /// 上次成功写入的 tooltip；文本未变化时跳过 `set_tooltip`。
    last_tooltip: Mutex<Option<String>>,
}

/// 安装托盘图标、菜单与额度监听。`setup` 中在 `app.manage(AppState)` 之后调用。
pub(crate) fn install(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| std::io::Error::other("缺少默认窗口图标，无法创建托盘"))?;

    let open_main = MenuItem::with_id(app, MENU_OPEN_MAIN, "打开主界面", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, MENU_QUIT, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open_main, &quit_item])?;

    let tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .menu(&menu)
        .tooltip(tooltip::TOOLTIP_EMPTY)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            MENU_OPEN_MAIN => crate::window::show_main(app),
            MENU_QUIT => quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::window::show_main(tray.app_handle());
            }
        })
        .build(app)?;

    if !app.manage(TrayState {
        icon: tray,
        quitting: AtomicBool::new(false),
        main_thread: std::thread::current().id(),
        last_tooltip: Mutex::new(Some(tooltip::TOOLTIP_EMPTY.to_string())),
    }) {
        return Err(std::io::Error::other("托盘状态已被注册").into());
    }

    // 额度变化/登录态变化时刷新 tooltip；payload 不解析，直接读本地快照。
    let quota_handle = app.clone();
    let _ = app.listen("quota://updated", move |_| {
        request_refresh(quota_handle.clone());
    });
    let auth_handle = app.clone();
    let _ = app.listen("auth://state", move |_| {
        request_refresh(auth_handle.clone());
    });
    // 启动占位已由 builder 的 tooltip 设置；真实额度在认证加载完成后由
    // `crate::bootstrap` 触发刷新，这里不再多派一次必然被文本去重跳过的刷新。
    Ok(())
}

fn quit(app: &AppHandle) {
    if let Some(state) = app.try_state::<TrayState>() {
        state
            .quitting
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    // 先留正常退出记录再退出：托盘退出是应用内唯一能解释「应用消失」的路径，
    // 留下 exit.log 后，下一次启动就不会把它误报成"未正常退出"。
    if let Some(state) = app.try_state::<crate::state::AppState>() {
        crate::shutdown::record_exit(state.data_dir(), "tray-quit");
    }
    app.exit(0);
}
