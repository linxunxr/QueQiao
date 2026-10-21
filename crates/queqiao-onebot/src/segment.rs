//! OneBot v11 出站消息段构造（形状按 SnowLuma SDK `types/messages.d.ts`：
//! `{type, data}`；桥发送一律用段数组）。

use serde_json::{Value, json};

pub fn text(s: &str) -> Value {
    json!({ "type": "text", "data": { "text": s } })
}

/// @某人；`qq` 为 "all" 时 @全体成员。
pub fn at(qq: &str) -> Value {
    json!({ "type": "at", "data": { "qq": qq } })
}

/// 引用回复（message_id）。
pub fn reply(message_id: i64) -> Value {
    json!({ "type": "reply", "data": { "id": message_id.to_string() } })
}

/// 图片：URL 直链或缓存文件名（文件名须先过 [`crate::file_guard`] 白名单）。
pub fn image(file: &str) -> Value {
    json!({ "type": "image", "data": { "file": file } })
}

/// 语音。
pub fn record(file: &str) -> Value {
    json!({ "type": "record", "data": { "file": file } })
}

/// 视频。
pub fn video(file: &str) -> Value {
    json!({ "type": "video", "data": { "file": file } })
}

/// 表情（face id）。
pub fn face(id: i64) -> Value {
    json!({ "type": "face", "data": { "id": id.to_string() } })
}

/// 组装段数组消息体。
pub fn message(segments: &[Value]) -> Value {
    Value::Array(segments.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 段形状与 SDK messages.d.ts 逐字段一致。
    #[test]
    fn segment_shapes() {
        assert_eq!(
            text("你好"),
            json!({ "type": "text", "data": { "text": "你好" } })
        );
        assert_eq!(at("all"), json!({ "type": "at", "data": { "qq": "all" } }));
        assert_eq!(
            reply(-123),
            json!({ "type": "reply", "data": { "id": "-123" } })
        );
        assert_eq!(
            image("https://example.com/a.jpg"),
            json!({ "type": "image", "data": { "file": "https://example.com/a.jpg" } })
        );
        let msg = message(&[reply(42), at("10086"), text("看这条")]);
        assert_eq!(msg.as_array().map(Vec::len), Some(3));
    }
}
