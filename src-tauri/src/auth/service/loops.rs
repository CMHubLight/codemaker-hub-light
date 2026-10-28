//! 认证后台循环：启动/停止与单次 tick。

use super::*;

impl AuthService {
    /// 确保后台循环只启动一次；已启动则直接返回。
    pub async fn ensure_loops(self: &Arc<Self>, app: tauri::AppHandle) {
        let mut loops = self.loops.lock().await;
        if loops.task.is_some() {
            return;
        }
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let service = Arc::clone(self);
        let task = tokio::spawn(async move {
            service.run_loop(app, cancel_rx).await;
        });
        loops.cancel = Some(cancel_tx);
        loops.task = Some(task);
        info!("认证后台循环已启动");
    }

    async fn run_loop(self: Arc<Self>, app: tauri::AppHandle, mut cancel: oneshot::Receiver<()>) {
        loop {
            match app.try_state::<crate::state::AppState>() {
                Some(state) => {
                    let gateway_url = state.settings.read().await.gateway_url.clone();
                    if let Some(update) = self.tick(&gateway_url).await {
                        use tauri::Emitter;
                        let _ = app.emit("auth://state", &update.status);
                        if update.quota_updated {
                            if let Some(quota) = &update.quota {
                                let _ = app.emit("quota://updated", quota);
                            }
                        }
                    }
                }
                None => warn!("后台循环拿不到 AppState，跳过本轮"),
            }

            tokio::select! {
                _ = &mut cancel => {
                    info!("认证后台循环已停止");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(RENEW_CHECK_INTERVAL_SECS)) => {}
            }
        }
    }

    /// 停止后台循环。
    ///
    /// 注销是安全边界：不能像旧实现那样"超时就把 JoinHandle 丢掉"——那会让任务
    /// 变成游离写者，在 auth.bin 被删除之后再把凭据写回去。
    /// 这里先发取消信号给常规退出一个宽限窗口，超时则 abort，并且始终等待任务
    /// 真正结束；只有本函数返回后，调用方才允许删除凭据。
    pub(super) async fn stop_loops(&self) {
        let (cancel, task) = {
            let mut loops = self.loops.lock().await;
            (loops.cancel.take(), loops.task.take())
        };
        if let Some(cancel) = cancel {
            let _ = cancel.send(());
        }
        let Some(mut task) = task else {
            return;
        };
        if tokio::time::timeout(Duration::from_secs(LOOP_GRACEFUL_STOP_SECS), &mut task)
            .await
            .is_err()
        {
            warn!(
                "认证后台循环未在 {}s 内退出，强制中止",
                LOOP_GRACEFUL_STOP_SECS
            );
            task.abort();
        }
        // 等 abort 落地：abort 只在 await 点生效，同步写盘中的任务会先写完再停。
        let _ = task.await;
        info!("认证后台循环已停止");
    }

    /// 单次后台检查：按到期时间执行 validate / quota / renew。
    ///
    /// 只有**可观察状态真的变化**或额度确有更新时才返回 `Some`：让 `run_loop`
    /// 不再每 300/600 秒盲目广播 `auth://state`（前端每个事件都会整页重拉）。
    async fn tick(&self, gateway_url: &str) -> Option<AuthUpdate> {
        self.ensure_loaded().await;
        let epoch = self.epoch().await;
        let before = self.status().await;

        let (credentials, phase, last_validated_at, last_quota_at) = {
            let state = self.state.read().await;
            (
                state.credentials.clone(),
                state.phase,
                state.last_validated_at,
                state.last_quota_at,
            )
        };
        let credentials = credentials?;
        // 需要重新登录时不做无意义重试，等待用户操作。
        if phase == AuthPhase::ReauthRequired {
            return None;
        }

        let gateway = match self.gateway(gateway_url).await {
            Ok(gateway) => gateway,
            Err(err) => {
                warn!(error = %err, "后台循环网关无效");
                return None;
            }
        };

        let now = Utc::now();
        let mut acted = false;

        let validate_due = last_validated_at
            .map(|last| (now - last).num_seconds() >= VALIDATE_INTERVAL_SECS as i64)
            .unwrap_or(true);
        if validate_due {
            self.validate_with(epoch, &gateway, &credentials).await;
            acted = true;
        }

        if self.renew_if_due(epoch, &gateway, &credentials, now).await {
            acted = true;
        }

        let quota_due = last_quota_at
            .map(|last| (now - last).num_seconds() >= QUOTA_INTERVAL_SECS as i64)
            .unwrap_or(true);
        let mut quota_info = None;
        let mut quota_updated = false;
        if quota_due {
            let profile_id = self.active_profile_id().await;
            let (info, updated) = self
                .fetch_and_store_quota(
                    epoch,
                    &gateway,
                    credentials.access_token(),
                    profile_id,
                    "后台",
                )
                .await;
            quota_info = info;
            quota_updated = updated;
            acted = true;
        }

        if !acted {
            return None;
        }
        let update = self.make_update(quota_info, quota_updated).await;
        if update_is_publishable(&before, &update) {
            Some(update)
        } else {
            None
        }
    }
}
