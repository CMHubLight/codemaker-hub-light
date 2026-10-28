//! 深链 `codemakerhub-light://auth/callback`：scheme 注册、回调监听与第二实例转发。
//!
//! Windows/Linux 的 scheme 必须在**运行期**注册：NSIS 安装器不做这件事，插件也
//! 只有 macOS/iOS/Android 是构建期注册。`register_all` 幂等，每次启动写一遍
//! HKCU，exe 被移动后还会自动纠正；它不在启动关键路径上（见 [`register_deferred`]）。
//!
//! 浏览器回调在 Windows 上通常以"第二实例 + 深链参数"启动，插件只处理首实例
//! CLI，因此单实例回调里还要扫描 argv（[`forward_second_instance`]）。

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_deep_link::DeepLinkExt;
use tracing::warn;

use crate::{auth, state::AppState, window};

/// 判断一个命令行参数是否是本应用的深链 URL（Windows 第二实例回调）。
pub(crate) fn is_deep_link_arg(arg: &str) -> bool {
    arg.trim_matches('"')
        .split_once("://")
        .map(|(scheme, _)| scheme.eq_ignore_ascii_case(auth::DEEP_LINK_SCHEME))
        .unwrap_or(false)
}

/// 第二实例回调：从 argv 里找深链并转发给认证服务。
pub(crate) fn forward_second_instance(app: &AppHandle, argv: &[String]) {
    if let Some(url) = argv.iter().find(|arg| is_deep_link_arg(arg)) {
        handle_url(app, url.trim_matches('"').to_string());
    }
}

/// 注册回调监听，并补投"应用被深链直接启动"时插件已解析的 URL。
///
/// 必须在 `app.manage(AppState)` 之后调用：下游要读 `AppState`。
pub(crate) fn listen(app: &AppHandle) {
    let handle = app.clone();
    app.deep_link().on_open_url(move |event| {
        for url in event.urls() {
            handle_url(&handle, url.to_string());
        }
    });
    // 应用被深链直接启动时，插件在 setup 之前已解析 CLI 参数。
    if let Ok(Some(urls)) = app.deep_link().get_current() {
        for url in urls {
            handle_url(app, url.to_string());
        }
    }
}

/// 运行期注册 scheme；**不阻塞 setup**：交给线程池执行，失败只告警。
///
/// 与 [`listen`]/[`forward_second_instance`] 是互相独立的兜底：注册失败时，
/// 已在运行的首实例仍能收到第二实例转发，登录页也可手动粘贴回调链接。
pub(crate) fn register_deferred(app: AppHandle) {
    #[cfg(any(windows, target_os = "linux"))]
    {
        tauri::async_runtime::spawn_blocking(move || {
            let started = std::time::Instant::now();
            let result = app.deep_link().register_all();
            crate::bootstrap::log_phase("deep_link_register", started);
            if let Err(err) = result {
                warn!(error = %err, "注册深链 scheme 失败，可用登录页手动粘贴回调链接");
            }
        });
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        // macOS/iOS/Android 由构建期配置注册，无需运行期写注册表。
        let _app = app;
    }
}

/// 处理一条深链：含凭据时交给 [`auth::service::AuthService::handle_deep_link`]
/// 立即完成登录；无论成功与否都只向事件发送非敏感字段。
fn handle_url(app: &AppHandle, raw: String) {
    let payload = match auth::parse_deep_link(&raw) {
        Ok(payload) => payload,
        Err(err) => {
            warn!(error = %err, "深链校验失败，已忽略");
            return;
        }
    };

    // 事件只带非敏感字段：绝不把含 token 的原始 URL 发给前端或写入日志。
    let _ = app.emit(
        "auth://callback",
        serde_json::json!({
            "poll_uuid": payload.poll_uuid,
            "has_credentials": payload.has_credentials(),
        }),
    );

    if !payload.has_credentials() {
        tracing::info!("收到登录深链（无凭据），等待轮询确认");
        return;
    }
    tracing::info!("收到登录深链（含凭据），立即完成登录");

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(state) = app.try_state::<AppState>() else {
            warn!("深链到达时 AppState 尚未就绪");
            return;
        };
        let (gateway_url, auth_service) = (
            state.settings.read().await.gateway_url.clone(),
            state.auth.clone(),
        );
        match auth_service.handle_deep_link(&payload, &gateway_url).await {
            Ok(update) => {
                let _ = app.emit("auth://state", &update.status);
                if update.quota_updated {
                    if let Some(quota) = &update.quota {
                        let _ = app.emit("quota://updated", quota);
                    }
                }
                if update.status.state == "authenticated" {
                    auth_service.ensure_loops(app.clone()).await;
                }
            }
            Err(err) => {
                // 未绑定到当前登录会话的回调链接：不采纳其中的凭据，
                // 只把静态原因写进状态后广播，绝不回传 URL 或 token。
                warn!(error = %err, "深链回调被拒绝");
                let status = auth_service.status().await;
                let _ = app.emit("auth://state", &status);
            }
        }
        // 深链可能来自隐藏到托盘的窗口，完成后把主界面带回前台。
        window::show_main(&app);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows 发布版登录只靠单实例 argv 扫描兜底：带引号、大小写、夹杂其它参数
    /// 时必须识别本应用 scheme，同时不能误吞别的协议。
    #[test]
    fn is_deep_link_arg_accepts_quoted_and_case_insensitive() {
        assert!(is_deep_link_arg(
            "\"CODEMAKERHUB-LIGHT://auth/callback?pollUuid=abc&access_token=t\""
        ));
        assert!(is_deep_link_arg(
            "codemakerhub-light://auth/callback?pollUuid=abc"
        ));
        assert!(!is_deep_link_arg("https://example.test/auth/callback"));
        assert!(!is_deep_link_arg("codemakerhub://auth/callback"));
        assert!(!is_deep_link_arg("--flag"));
        assert!(!is_deep_link_arg("codemakerhub-light"));
        assert!(!is_deep_link_arg(""));
    }
}
