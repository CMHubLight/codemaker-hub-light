//! 入口 Host 白名单：只接受回环 Host，防 DNS rebinding。
//!
//! 代理只监听回环地址，但浏览器里的页面可以把自己的域名重新解析到 127.0.0.1
//! （DNS rebinding），此时请求虽然打到回环端口，`Host` 头仍是攻击者域名。
//! 只校验来源 IP 不够——必须校验 `Host` 是回环。
//!
//! 纯函数便于单测；中间件在 [`super::routes`] 里。

/// Host 头是否指向回环（`bind` 为代理实际监听地址，已由 server 校验为回环）。
pub fn host_header_allowed(host: Option<&str>, bind: &str) -> bool {
    let Some((name, port)) = host.and_then(split_host_port) else {
        return false;
    };
    // 端口若出现，必须是合法端口（0 与越界都拒绝）。
    if matches!(port, Some(0)) {
        return false;
    }
    let bind = bind.trim().trim_end_matches('.').to_ascii_lowercase();
    name == "127.0.0.1" || name == "localhost" || name == "::1" || name == bind
}

/// 把 `host[:port]`（IPv6 形如 `[::1]:15722`）拆成小写主机名与可选端口。
///
/// 语法不合法时返回 `None`（调用方按拒绝处理）。
fn split_host_port(raw: &str) -> Option<(String, Option<u16>)> {
    // 尾点：`localhost.` 是合法的 FQDN 写法，容忍。
    let raw = raw.trim().trim_end_matches('.');
    if raw.is_empty() {
        return None;
    }

    // IPv6 必须带方括号（RFC 3986）；`[::1]` 或 `[::1]:port`。
    if let Some(rest) = raw.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = match after {
            "" => None,
            _ => {
                let digits = after.strip_prefix(':')?;
                Some(parse_port(digits)?)
            }
        };
        return Some((host.to_ascii_lowercase(), port));
    }

    // 非括号形式：端口只在「最后一段全是数字」时才当成端口。
    match raw.rsplit_once(':') {
        Some((host, digits))
            if !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit()) =>
        {
            Some((host.to_ascii_lowercase(), Some(parse_port(digits)?)))
        }
        _ => Some((raw.to_ascii_lowercase(), None)),
    }
}

fn parse_port(digits: &str) -> Option<u16> {
    let port: u16 = digits.parse().ok()?;
    (port > 0).then_some(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIND: &str = "127.0.0.1";

    fn allowed(host: &str) -> bool {
        host_header_allowed(Some(host), BIND)
    }

    #[test]
    fn allows_loopback_hosts() {
        assert!(allowed("127.0.0.1"));
        assert!(allowed("127.0.0.1:15722"));
        assert!(allowed("127.0.0.1:15722"));
        assert!(allowed("localhost"));
        assert!(allowed("localhost:15722"));
        assert!(allowed("LOCALHOST"));
        assert!(allowed("localhost."));
        assert!(allowed("[::1]"));
        assert!(allowed("[::1]:15722"));
        // bind 本身（回环 IP）也放行
        assert!(host_header_allowed(Some("127.0.0.1"), "127.0.0.1"));
    }

    #[test]
    fn rejects_rebinding_and_malformed_hosts() {
        // rebinding 的主目标：攻击者域名
        assert!(!allowed("evil.com"));
        assert!(!allowed("evil.com:15722"));
        assert!(!allowed("127.0.0.1.evil.com"));
        assert!(!allowed("127.0.0.1@evil.com"));
        // 非回环 IPv6 / 未加方括号
        assert!(!allowed("[::2]"));
        assert!(!allowed("::1"));
        // 非法端口
        assert!(!allowed("127.0.0.1:abc"));
        assert!(!allowed("127.0.0.1:0"));
        assert!(!allowed("127.0.0.1:99999"));
        assert!(!allowed("127.0.0.1:"));
        // 缺失/空
        assert!(!host_header_allowed(None, BIND));
        assert!(!allowed(""));
        assert!(!allowed("   "));
    }
}
