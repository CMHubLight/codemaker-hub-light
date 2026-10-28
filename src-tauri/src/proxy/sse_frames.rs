//! SSE 帧扫描与 `data:` 抽取：`tee`（usage 观测）与 `translate`（协议翻译）共用。
//!
//! 帧分隔兼容 `\n\n` 与 `\r\n\r\n`；忽略注释行；支持多行 `data:`。
//! [`frame_data`] 对单行 `data:` 返回借用（零拷贝），只有多行时才拼接一次。
//!
//! 两个上限数值相同但**语义不同**，不要合并：
//! - [`SSE_PARSE_LIMIT`]：观测侧降级——停止解析，但字节照常透传；
//! - [`SSE_FAIL_LIMIT`]：翻译侧显式失败——丢弃缓冲并给客户端发一条 `event: error`。

use std::borrow::Cow;

/// 观测侧解析缓冲上限：超过后停止解析（`SseTee`），但仍原样透传。
pub const SSE_PARSE_LIMIT: usize = 2 * 1024 * 1024;

/// 翻译侧缓冲上限：超过后清空缓冲并向客户端发 `event: error`（`StreamTranslator`）。
pub const SSE_FAIL_LIMIT: usize = 2 * 1024 * 1024;

/// 找到最早的帧结束位置：返回 `(payload_end, next_frame_start)`。
pub fn find_frame_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < buffer.len() {
        if buffer.len() >= index + 4 && &buffer[index..index + 4] == b"\r\n\r\n" {
            return Some((index, index + 4));
        }
        if buffer.len() >= index + 2 && &buffer[index..index + 2] == b"\n\n" {
            return Some((index, index + 2));
        }
        index += 1;
    }
    None
}

/// 返回帧数据（多行 `data:` 以 `\n` 连接）；注释行/空帧返回 `None`。
pub fn frame_data(frame: &[u8]) -> Option<Cow<'_, str>> {
    if let Ok(text) = std::str::from_utf8(frame) {
        let mut lines = text.split('\n').filter_map(data_line);
        let first = lines.next()?;
        let Some(second) = lines.next() else {
            // 绝大多数帧只有一行 data：直接借用，不复制。
            return Some(Cow::Borrowed(first));
        };
        let mut joined = String::with_capacity(first.len() + second.len() + 8);
        joined.push_str(first);
        joined.push('\n');
        joined.push_str(second);
        for line in lines {
            joined.push('\n');
            joined.push_str(line);
        }
        return Some(Cow::Owned(joined));
    }

    // 非 UTF-8（异常上游）：退回 lossy 拼接，与旧实现一致。
    let text = String::from_utf8_lossy(frame);
    let mut joined: Option<String> = None;
    for line in text.split('\n').filter_map(data_line) {
        match joined.as_mut() {
            Some(buffer) => {
                buffer.push('\n');
                buffer.push_str(line);
            }
            None => joined = Some(line.to_string()),
        }
    }
    joined.map(Cow::Owned)
}

/// 从一行 SSE 文本里取出 `data:` 值；空行/注释行/非 data 行返回 `None`。
fn data_line(raw: &str) -> Option<&str> {
    let line = raw.strip_suffix('\r').unwrap_or(raw);
    if line.is_empty() || line.starts_with(':') {
        return None;
    }
    let value = line.strip_prefix("data:")?;
    Some(value.strip_prefix(' ').unwrap_or(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_finder_handles_both_delimiters() {
        assert_eq!(find_frame_end(b"a\n\nb"), Some((1, 3)));
        assert_eq!(find_frame_end(b"a\r\n\r\nb"), Some((1, 5)));
        assert_eq!(find_frame_end(b"a\nb"), None);
        assert_eq!(find_frame_end(b""), None);
    }

    #[test]
    fn single_data_line_is_borrowed() {
        let frame = b"event: message_start\ndata: {\"a\":1}";
        let data = frame_data(frame).expect("应取出 data");
        assert_eq!(data, "{\"a\":1}");
        assert!(matches!(data, Cow::Borrowed(_)), "单行 data 必须零拷贝借用");
    }

    #[test]
    fn multiline_data_is_joined_with_newline() {
        let frame = b"data: {\"a\":\ndata: 1}";
        assert_eq!(frame_data(frame).as_deref(), Some("{\"a\":\n1}"));
    }

    #[test]
    fn comments_and_empty_frames_yield_none() {
        assert!(frame_data(b"").is_none());
        assert!(frame_data(b": ping\n").is_none());
        assert!(frame_data(b"event: message_stop\n").is_none());
    }

    #[test]
    fn invalid_utf8_falls_back_to_lossy() {
        // 每个非法字节替换成一个 U+FFFD，与旧实现的 `from_utf8_lossy` 一致。
        let frame = b"data: \xff\xfe";
        assert_eq!(frame_data(frame).as_deref(), Some("\u{fffd}\u{fffd}"));
    }
}
