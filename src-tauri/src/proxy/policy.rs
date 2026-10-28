//! 代理运行策略（来自设置 + P2 安全上限）。

use crate::settings::Settings;

/// 每侧正文记录的默认上限（64KB，development-plan §6）。
pub const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024;
/// 请求体硬上限（32MB，development-plan §6）。
pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;
/// 非流式响应解析缓冲上限（超过则只透传不解析）。
pub const DEFAULT_MAX_BUFFER_BYTES: usize = 8 * 1024 * 1024;
/// 上游首个响应字节超时。
pub const FIRST_BYTE_TIMEOUT_SECS: u64 = 120;
/// 上游连接超时。
pub const CONNECT_TIMEOUT_SECS: u64 = 10;
/// 上游流空闲超时（相邻 chunk 间隔）。
pub const IDLE_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Clone, Copy)]
pub struct ProxyPolicy {
    /// 是否记录请求/响应正文（默认关闭）。
    pub record_body: bool,
    /// 单侧正文截断上限。
    pub max_body_bytes: usize,
    /// `aigw.cost.total_credit` → 元。
    pub credit_to_yuan: f64,
    /// 请求体硬上限。
    pub max_request_body_bytes: usize,
    /// 非流式解析缓冲上限。
    pub max_buffer_bytes: usize,
    /// 上游首字节超时（秒）。进策略是为了可测：集成测试用 1s 验证超时落库。
    pub first_byte_timeout_secs: u64,
}

/// 运行期可替换的代理策略单元。
///
/// 代理启动时创建，设置保存时写回；每个请求开头取一份快照（`Copy` 值），
/// 因此「记录请求正文」「积分换算」改完立即生效，无需重启代理。
/// 用同步 `RwLock` 是有意的：只做微秒级的纯值读写，不跨 await 持有。
#[derive(Debug)]
pub struct PolicyCell(std::sync::RwLock<ProxyPolicy>);

impl PolicyCell {
    pub fn new(policy: ProxyPolicy) -> Self {
        Self(std::sync::RwLock::new(policy))
    }

    /// 取当前策略快照；中毒时仍返回最后写入的值（纯值，无需 panic 传播）。
    pub fn load(&self) -> ProxyPolicy {
        *self
            .0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 写回新策略；下一次请求即生效。
    pub fn store(&self, policy: ProxyPolicy) {
        *self
            .0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = policy;
    }
}

impl Default for ProxyPolicy {
    fn default() -> Self {
        Self {
            record_body: false,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            credit_to_yuan: 0.01,
            max_request_body_bytes: DEFAULT_MAX_REQUEST_BODY_BYTES,
            max_buffer_bytes: DEFAULT_MAX_BUFFER_BYTES,
            first_byte_timeout_secs: FIRST_BYTE_TIMEOUT_SECS,
        }
    }
}

impl ProxyPolicy {
    /// 从设置生成；`credit_to_yuan` 沿用 P0 字段名（默认 0.01，total_credit/100）。
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            record_body: settings.record_body,
            credit_to_yuan: settings.credit_to_yuan,
            ..Self::default()
        }
    }

    /// 记录正文时截断（按 UTF-8 边界安全截断）。
    pub fn truncate_body(&self, bytes: &[u8]) -> (String, bool) {
        if bytes.len() <= self.max_body_bytes {
            return (String::from_utf8_lossy(bytes).into_owned(), false);
        }
        let mut end = self.max_body_bytes;
        while end > 0 && !is_char_boundary(bytes, end) {
            end -= 1;
        }
        (String::from_utf8_lossy(&bytes[..end]).into_owned(), true)
    }
}

fn is_char_boundary(bytes: &[u8], index: usize) -> bool {
    if index >= bytes.len() {
        return true;
    }
    // UTF-8 续字节形如 10xxxxxx；落在续字节上则不是字符边界。
    (bytes[index] & 0b1100_0000) != 0b1000_0000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_plan_limits() {
        let policy = ProxyPolicy::default();
        assert!(!policy.record_body);
        assert_eq!(policy.max_body_bytes, 64 * 1024);
        assert_eq!(policy.max_request_body_bytes, 32 * 1024 * 1024);
        assert_eq!(policy.max_buffer_bytes, 8 * 1024 * 1024);
        assert_eq!(policy.first_byte_timeout_secs, FIRST_BYTE_TIMEOUT_SECS);
    }

    #[test]
    fn truncation_respects_utf8_boundary() {
        let policy = ProxyPolicy {
            max_body_bytes: 5,
            ..ProxyPolicy::default()
        };
        let (text, truncated) = policy.truncate_body("中文abc".as_bytes());
        assert!(truncated);
        assert_eq!(text, "中"); // 3 字节字符，4/5 落在字符内部
    }

    #[test]
    fn short_body_is_not_truncated() {
        let policy = ProxyPolicy::default();
        let (text, truncated) = policy.truncate_body(b"hello");
        assert!(!truncated);
        assert_eq!(text, "hello");
    }

    /// 设置保存后写回策略：下一次 load 必须读到新值（代理无需重启）。
    #[test]
    fn policy_cell_store_is_visible_to_load() {
        let cell = PolicyCell::new(ProxyPolicy::default());
        assert!(!cell.load().record_body);

        cell.store(ProxyPolicy {
            record_body: true,
            credit_to_yuan: 0.02,
            ..ProxyPolicy::default()
        });

        let current = cell.load();
        assert!(current.record_body);
        assert!((current.credit_to_yuan - 0.02).abs() < f64::EPSILON);
    }
}
