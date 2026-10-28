//! SSE 事件写出：`event: <name>\ndata: <json>\n\n`。

use serde_json::Value;

pub(super) fn push_event(out: &mut String, event: &str, data: &Value) {
    out.push_str("event: ");
    out.push_str(event);
    out.push_str("\ndata: ");
    out.push_str(&data.to_string());
    out.push_str("\n\n");
}
