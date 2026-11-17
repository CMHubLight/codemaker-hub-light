//! SSE 帧扫描与 `data:` 抽取：`tee`（usage 观测）、`translate`（协议翻译）与
//! 直通流式的 usage 帧过滤共用。
//!
//! 帧分隔按 WHATWG SSE 规范支持 `CRLF`、`LF`、`CR` 三种行结束符，以及它们混用
//! （连续两个行结束符即帧边界）。[`SseBuffer`] 做增量扫描：每个字节只扫一次，
//! 避免旧实现"每个 chunk 从 0 重扫整段未消费缓冲"的 O(n²)。
//!
//! 两个上限数值相同但**语义不同**，不要合并：
//! - [`SSE_PARSE_LIMIT`]：观测侧降级——停止解析，但字节照常透传；
//! - [`SSE_FAIL_LIMIT`]：翻译侧显式失败——丢弃缓冲并给客户端发一条 `event: error`。

use std::borrow::Cow;

/// 观测侧解析缓冲上限：超过后停止解析（`SseTee`），但仍原样透传。
pub const SSE_PARSE_LIMIT: usize = 2 * 1024 * 1024;

/// 翻译侧缓冲上限：超过后清空缓冲并向客户端发 `event: error`（`StreamTranslator`）。
pub const SSE_FAIL_LIMIT: usize = 2 * 1024 * 1024;

/// 增量 SSE 帧缓冲。
///
/// 用法：`push(chunk)` 后反复 `next_frame()`；返回的帧**包含**尾部空行（调用方
/// 用 [`frame_data`] 取 `data:` 时不影响，直通过滤则可原样转发）。
pub struct SseBuffer {
    buffer: Vec<u8>,
    /// 已经扫描到但还不足以判定帧边界的偏移。
    scan: usize,
    /// 当前帧的起始偏移（上一帧之后）。
    frame_start: usize,
    /// 最近一个行结束符的起点与终点；下一行结束符与它相邻即"空行"。
    last_eol: Option<(usize, usize)>,
}

impl Default for SseBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl SseBuffer {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            scan: 0,
            frame_start: 0,
            last_eol: None,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    pub fn len(&self) -> usize {
        self.buffer.len().saturating_sub(self.frame_start)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 取出下一个完整帧（含尾部空行）；没有完整帧时返回 `None`。
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        let mut index = self.scan.max(self.frame_start);
        while index < self.buffer.len() {
            match self.buffer[index] {
                b'\r' => {
                    if index + 1 == self.buffer.len() {
                        // 末尾 CR 可能是 CRLF 的前半：只有它本身已经是"第二个行结束符"
                        // （与上一个 EOL 相邻）时才能立即收帧，否则等下一个 chunk。
                        if let Some((_, end)) = self.last_eol {
                            if end == index {
                                return Some(self.extract(index + 1));
                            }
                        }
                        self.scan = index;
                        return None;
                    }
                    let eol_len = if self.buffer[index + 1] == b'\n' {
                        2
                    } else {
                        1
                    };
                    if let Some((_, end)) = self.last_eol {
                        if end == index {
                            return Some(self.extract(index + eol_len));
                        }
                    }
                    self.last_eol = Some((index, index + eol_len));
                    index += eol_len;
                }
                b'\n' => {
                    if let Some((_, end)) = self.last_eol {
                        if end == index {
                            return Some(self.extract(index + 1));
                        }
                    }
                    self.last_eol = Some((index, index + 1));
                    index += 1;
                }
                _ => index += 1,
            }
        }
        self.scan = index;
        None
    }

    /// 结束：取出没有尾部空行的最后一帧（含任意残留字节）。
    pub fn take_rest(&mut self) -> Option<Vec<u8>> {
        if self.frame_start >= self.buffer.len() {
            return None;
        }
        let rest = self.buffer[self.frame_start..].to_vec();
        self.buffer.clear();
        self.reset();
        Some(rest)
    }

    /// 丢弃全部缓冲（超过上限时使用）。
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.reset();
    }

    fn reset(&mut self) {
        self.scan = 0;
        self.frame_start = 0;
        self.last_eol = None;
    }

    fn extract(&mut self, next: usize) -> Vec<u8> {
        let frame = self.buffer[self.frame_start..next].to_vec();
        self.buffer.drain(..next);
        self.reset();
        frame
    }
}

/// 找到最早的帧结束位置：返回 `(payload_end, next_frame_start)`。
///
/// 兼容 `CRLF` / `LF` / `CR` 行结束符与混用；保留该函数供纯函数测试与旧调用方使用。
pub fn find_frame_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    let mut last_eol: Option<(usize, usize)> = None;
    while index < buffer.len() {
        match buffer[index] {
            b'\r' => {
                if index + 1 == buffer.len() {
                    return None;
                }
                let eol_len = if buffer[index + 1] == b'\n' { 2 } else { 1 };
                if let Some((start, end)) = last_eol {
                    if end == index {
                        return Some((start, index + eol_len));
                    }
                }
                last_eol = Some((index, index + eol_len));
                index += eol_len;
            }
            b'\n' => {
                if let Some((start, end)) = last_eol {
                    if end == index {
                        return Some((start, index + 1));
                    }
                }
                last_eol = Some((index, index + 1));
                index += 1;
            }
            _ => index += 1,
        }
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
    // 帧可能以 CR/CRLF 结束：去掉行尾所有 CR（兼容 CR-only 的空行与同一行多 CR）。
    let line = raw.trim_end_matches('\r');
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
    fn split_finder_handles_all_eol_styles() {
        assert_eq!(find_frame_end(b"a\n\nb"), Some((1, 3)));
        assert_eq!(find_frame_end(b"a\r\n\r\nb"), Some((1, 5)));
        assert_eq!(find_frame_end(b"a\r\rb"), Some((1, 3)), "纯 CR 空行");
        assert_eq!(find_frame_end(b"a\r\n\nb"), Some((1, 4)), "CRLF + LF 混用");
        assert_eq!(find_frame_end(b"a\n\r\nb"), Some((1, 4)), "LF + CRLF 混用");
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

    #[test]
    fn buffer_handles_split_frames_and_mixed_eol() {
        let mut buffer = SseBuffer::new();
        buffer.push(b"data: 1\r");
        assert!(buffer.next_frame().is_none(), "CRLF 拆分时不得提前判帧");
        buffer.push(b"\n\r\n");
        let frame = buffer.next_frame().expect("应成帧");
        assert_eq!(frame_data(&frame).as_deref(), Some("1"));

        buffer.push(b"data: 2\n\ndata: 3\r\r");
        let second = buffer.next_frame().expect("第二帧");
        assert_eq!(frame_data(&second).as_deref(), Some("2"));
        let third = buffer.next_frame().expect("第三帧");
        assert_eq!(frame_data(&third).as_deref(), Some("3"));
        assert!(buffer.next_frame().is_none());
    }

    #[test]
    fn buffer_does_not_rescan_from_zero() {
        // 无分隔符的超大块只应扫描一次：这里只断言不 panic、长度正确，
        // 增量性由 `scan` 字段保证（性能用例见 proxy::perf_tests）。
        let mut buffer = SseBuffer::new();
        buffer.push(&vec![b'x'; 100_000]);
        assert!(buffer.next_frame().is_none());
        assert_eq!(buffer.len(), 100_000);
    }
}
