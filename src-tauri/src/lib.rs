//! CodeMaker Hub Light 后端入口（Tauri 2 + Tokio + Axum + SQLite）。
//!
//! 启动流程：数据目录 → 日志 → panic hook → 设置 → 数据库/迁移 → 共享状态 →
//! 托盘 → 深链监听（scheme 注册延后到 setup 返回后在线程池执行）→ 后台认证加载
//! + 本地代理 → 注册 IPC 命令。各阶段耗时见 `bootstrap::PhaseTimer`。
//!
//! 认证：深链/轮询完成登录、AES-256-GCM 凭据存储（主密钥在 OS 凭据库）、
//! `/validate` 与额度刷新、`/renew_token` 续期及后台循环。
//! 代理：127.0.0.1 上的 Axum 服务，三协议转发（Anthropic Messages 本地翻译为
//! OpenAI Chat）、SSE 透传、逐请求 token/费用落库。

// 模块可见性：本子系统（命令/设置/状态/托盘/启动装配）已收窄为 crate 内可见。
// `auth`/`db`/`proxy`/`usage` 暂保留 `pub`：它们仍有"仅测试使用"的公开辅助项，
// 直接收窄会触发一批 dead_code 告警；待这些辅助项清理或改为 `#[cfg(test)]`
// 后再统一改成 `pub(crate)`（届时只需改这几行）。
pub mod auth;
mod bootstrap;
pub(crate) mod commands;
mod compat;
pub mod db;
mod deep_link;
mod logging;
mod panic_log;
pub mod proxy;
pub(crate) mod settings;
pub(crate) mod settings_store;
mod shutdown;
mod startup;
pub(crate) mod state;
#[cfg(test)]
pub(crate) mod test_util;
pub(crate) mod tray;
pub mod usage;
mod window;

use std::process::ExitCode;

use tauri::{Emitter, WebviewWindowBuilder};

const APP_NAME: &str = "CodeMaker Hub Light";

/// 启动期致命失败：弹窗 + 落盘 + 以非零码退出（不再 `return Err` 触发 Tauri panic）。
fn fatal_exit(app: &tauri::AppHandle, data_dir: &std::path::Path, stage: &str, detail: &str) {
    startup::report(data_dir, stage, detail);
    shutdown::record_exit(data_dir, "setup-failed");
    app.exit(1);
}

/// 启动 Tauri 应用（被 `main.rs` 调用）。
pub fn run() -> ExitCode {
    // 数据目录不可用时窗口尚未创建：弹窗 + 落盘后直接返回非零退出码。
    let Some(data_dir) = bootstrap::prepare_data_dir() else {
        return ExitCode::FAILURE;
    };

    logging::init(&data_dir);
    // release 无控制台且 panic=abort：先把崩溃现场写进 logs\panic.log。
    panic_log::install(&data_dir);

    let version = env!("CARGO_PKG_VERSION");
    tracing::info!(
        version = version,
        data_dir = %data_dir.display(),
        "启动 CodeMaker Hub Light"
    );

    let setup_dir = data_dir.clone();
    let app = match tauri::Builder::default()
        // 单实例必须最先注册：二次启动聚焦已有窗口。
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            let _ = app.emit("app://second-instance", serde_json::json!({}));
            window::show_main(app);
            // Windows：浏览器回调会以“第二实例 + 深链参数”启动，
            // 深链插件只处理首实例 CLI，因此这里手动转发。
            deep_link::forward_second_instance(app, &argv);
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        // 关闭窗口只隐藏到托盘；托盘“退出”会先置 quitting=true 再 app.exit(0)。
        .on_window_event(window::handle_window_event)
        .setup(move |app| {
            let mut timer = bootstrap::PhaseTimer::new();

            // 单实例判定已经通过（二次实例在 setup 之前就退出），此时才登记运行中
            // 标记：否则每次"二次启动"都会写一遍 marker，下一次真正启动就会被误报成
            // 「上次运行未正常退出」。
            if let Some(previous) = shutdown::mark_running(&setup_dir, version) {
                tracing::warn!(
                    previous = previous.trim(),
                    "上次运行未正常退出（marker 未被清除），可能是崩溃或被外部终止"
                );
            }

            if let Err(err) = bootstrap::init_state(app, &setup_dir, &mut timer) {
                fatal_exit(app.handle(), &setup_dir, "初始化", &err.to_string());
                return Ok(());
            }

            // 主窗口由我们自己创建（tauri.conf.json 里 `create: false`）：
            // Tauri 默认在 setup 之前建窗，WebView2 缺失时会在我们的诊断代码之前
            // panic；自己建窗才能把失败变成「弹窗 + startup-error.log + exit(1)」。
            let Some(window_config) = app
                .config()
                .app
                .windows
                .iter()
                .find(|window| window.label == "main")
                .cloned()
            else {
                fatal_exit(
                    app.handle(),
                    &setup_dir,
                    "创建主窗口",
                    "tauri.conf.json 缺少 label=main 的窗口配置",
                );
                return Ok(());
            };
            match WebviewWindowBuilder::from_config(app.handle(), &window_config)
                .and_then(|builder| builder.build())
            {
                Ok(window) => {
                    timer.lap("window");
                    let _ = window;
                }
                Err(err) => {
                    fatal_exit(app.handle(), &setup_dir, "创建主窗口", &err.to_string());
                    return Ok(());
                }
            }

            // 托盘：关闭窗口只隐藏；额度事件会刷新 tooltip。
            let tray_result = tray::install(app.handle());
            timer.lap("tray");
            if let Err(err) = tray_result {
                fatal_exit(app.handle(), &setup_dir, "初始化托盘", &err.to_string());
                return Ok(());
            }

            // 深链监听必须在 manage(AppState) 之后；scheme 注册挪到线程池，
            // 与 argv 扫描 / 手动粘贴是三条互相独立的兜底路径。
            deep_link::listen(app.handle());
            deep_link::register_deferred(app.handle().clone());

            // 认证加载与代理常驻后台任务（Tauri 内置 Tokio runtime，不阻塞 setup）。
            bootstrap::spawn_background(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::app::app_info,
            commands::app::app_open_logs,
            commands::settings::settings_get,
            commands::settings::settings_update,
            commands::auth::auth_status,
            commands::auth::auth_start_login,
            commands::auth::auth_cancel_login,
            commands::auth::auth_login_with_auth_key,
            commands::auth::auth_poll_status,
            commands::auth::auth_logout,
            commands::auth::auth_refresh_validate,
            commands::auth::auth_retry_renewal,
            commands::auth::auth_submit_deep_link,
            commands::auth::quota_get,
            commands::proxy::proxy_status,
            commands::proxy::proxy_restart,
            commands::usage::usage_summary,
            commands::usage::cost_trend_get,
            commands::usage::usage_requests,
            commands::usage::usage_request_detail,
        ])
        .build(tauri::generate_context!())
    {
        Ok(app) => app,
        Err(err) => {
            tracing::error!(error = %err, "Tauri 应用构建失败");
            shutdown::record_exit(&data_dir, "build-failed");
            eprintln!("{APP_NAME} 启动失败：{err}");
            return ExitCode::FAILURE;
        }
    };

    // 事件循环退出时兜底记录正常退出并冲刷日志；托盘「退出」不再提前写记录。
    app.run(move |_app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            shutdown::record_exit_if_running(&data_dir, "run-event-exit");
            logging::flush();
        }
    });
    ExitCode::SUCCESS
}
