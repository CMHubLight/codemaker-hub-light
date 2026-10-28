//! 云端额度：读取本地快照、手动/后台拉取并落库。

use super::*;

impl AuthService {
    /// 读取最近一次成功快照（不触发网络）。
    pub async fn latest_quota(&self) -> Result<Option<QuotaInfo>, AuthError> {
        let snapshot = blocking::run(&self.db, |db| {
            let conn = db.conn()?;
            let snapshot = repo::latest_quota_snapshot(&conn, "global")?;
            Ok(snapshot)
        })
        .await
        .map_err(|err| match err {
            DbError::Pool(_) => AuthError::Internal(format!("打开数据库失败：{err}")),
            _ => AuthError::Internal(format!("读取额度快照失败：{err}")),
        })?;
        Ok(snapshot.map(QuotaInfo::from_snapshot))
    }

    /// 读取账号云端最近 24 小时费用；不写数据库、不记录 token。
    pub async fn hourly_cost(
        &self,
        gateway_url: &str,
        start_time: &str,
        end_time: &str,
    ) -> Result<HourlyCost, AuthError> {
        self.ensure_loaded().await;
        let credentials = { self.state.read().await.credentials.clone() };
        let Some(credentials) = credentials else {
            return Err(AuthError::NotAuthenticated);
        };
        let gateway = self.gateway(gateway_url).await?;
        match gateway
            .hourly_cost(credentials.access_token(), start_time, end_time)
            .await
        {
            Ok(HourlyCostOutcome::Ok(cost)) => Ok(cost),
            Ok(HourlyCostOutcome::Forbidden) => Err(AuthError::Rejected(
                "无权限读取云端费用（HTTP 403）".to_string(),
            )),
            Ok(HourlyCostOutcome::NotFound) => Err(AuthError::Http(
                "云端费用接口不存在，请检查服务版本（HTTP 404）".to_string(),
            )),
            Err(err) => Err(AuthError::Http(format!("云端费用请求失败：{err}"))),
        }
    }

    /// 额度刷新；`force = false` 时只读本地快照。
    pub async fn refresh_quota(
        &self,
        gateway_url: &str,
        force: bool,
    ) -> Result<Option<QuotaInfo>, AuthError> {
        if !force {
            return self.latest_quota().await;
        }

        self.ensure_loaded().await;
        let epoch = self.epoch().await;
        let credentials = { self.state.read().await.credentials.clone() };
        let Some(credentials) = credentials else {
            return Err(AuthError::NotAuthenticated);
        };
        let gateway = self.gateway(gateway_url).await?;

        match self.fetch_quota(&gateway, credentials.access_token()).await {
            Ok(data) => {
                let profile_id = self.active_profile_id().await;
                Ok(Some(self.store_quota(epoch, profile_id, &data).await?))
            }
            Err(QuotaFailure::Forbidden) => Err(AuthError::Rejected(
                "无权限访问用量信息（HTTP 403）".to_string(),
            )),
            Err(QuotaFailure::NotFound) => Err(AuthError::Http(
                "用量接口不存在，请检查服务版本（HTTP 404）".to_string(),
            )),
            Err(QuotaFailure::Failed(err)) => {
                Err(AuthError::Http(format!("额度接口请求失败：{err}")))
            }
        }
    }

    /// 拉取额度并解析（唯一 HTTP 调用点，三种调用场景共用）。
    pub(super) async fn fetch_quota(
        &self,
        gateway: &AuthGateway,
        access_token: &str,
    ) -> Result<QuotaData, QuotaFailure> {
        match gateway.quota(access_token).await {
            Ok(QuotaOutcome::Ok(value)) => Ok(quota::parse_quota(&value)),
            Ok(QuotaOutcome::Forbidden) => Err(QuotaFailure::Forbidden),
            Ok(QuotaOutcome::NotFound) => Err(QuotaFailure::NotFound),
            Err(err) => Err(QuotaFailure::Failed(err)),
        }
    }

    /// 拉取 + 落库（登录完成与后台 tick 共用）：失败只记日志、保留上次快照。
    ///
    /// 返回值第二项是 `quota_updated`：只有真正写库成功才为 true。
    pub(super) async fn fetch_and_store_quota(
        &self,
        epoch: u64,
        gateway: &AuthGateway,
        access_token: &str,
        profile_id: Option<i64>,
        context: &str,
    ) -> (Option<QuotaInfo>, bool) {
        let data = match self.fetch_quota(gateway, access_token).await {
            Ok(data) => data,
            Err(QuotaFailure::Forbidden) => {
                warn!(context, "额度接口返回 403（无权限）");
                return (None, false);
            }
            Err(QuotaFailure::NotFound) => {
                warn!(context, "额度接口返回 404（接口不存在）");
                return (None, false);
            }
            Err(QuotaFailure::Failed(err)) => {
                warn!(error = %err, context, "额度拉取失败（保留上次快照）");
                return (None, false);
            }
        };
        match self.store_quota(epoch, profile_id, &data).await {
            Ok(info) => (Some(info), true),
            Err(err) => {
                warn!(error = %err, context, "额度快照写入失败");
                (None, false)
            }
        }
    }
}

/// 额度拉取的失败分类：前两类是"服务端不支持/无权限"，第三类是网络/协议错误。
pub(super) enum QuotaFailure {
    Forbidden,
    NotFound,
    Failed(GatewayError),
}
