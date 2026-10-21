//! OneBot v11 事件反序列化（强类型；形状按 SnowLuma SDK `types/events.d.ts` 与
//! OneBot v11 标准报文钉住，契约测试用真实样例）。
//!
//! 只建模桥实际消费的事件（规格 §3.2）：群/私聊消息、自发回显、拍一拍、心跳
//! （`status.good` 是 SnowLuma hook 健康的**权威信号**——SnowLuma 每 30s 无条件
//! 发一次，good = online && receiveHealthy 是它对「QQ → hook → 我」接收链路的
//! 自评；断流误诊教训见 Node 版 bridge.js 注释）、账号会话边沿（bot_status，
//! 1.14.17+ 加分项）。其余事件透传 post_type（健康记账用），不建模字段。

use serde::Deserialize;
use serde_json::Value;

/// 事件载荷解析失败（非对象 / 缺 post_type 等）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventParseError(pub String);

impl std::fmt::Display for EventParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid OneBot event: {}", self.0)
    }
}

impl std::error::Error for EventParseError {}

/// 消息体：OneBot 有 array（段数组）与 string（CQ 码）两种格式。
#[derive(Debug, Clone, PartialEq)]
pub enum MessageBody {
    /// 段数组（SnowLuma WS 的默认格式）；保留原始 `Value`，按需在 segment 模块解析。
    Array(Vec<Value>),
    /// CQ 码字符串格式。
    Text(String),
}

/// 提取消息的纯文本（text 段拼接 / CQ 码字符串原样）。
pub fn extract_plain_text(body: &MessageBody) -> String {
    match body {
        MessageBody::Text(s) => s.clone(),
        MessageBody::Array(segments) => segments
            .iter()
            .filter(|s| s.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|s| {
                s.get("data")
                    .and_then(|d| d.get("text"))
                    .and_then(Value::as_str)
            })
            .collect(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Sender {
    pub user_id: u64,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub card: Option<String>,
    /// member / admin / owner。
    #[serde(default)]
    pub role: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupMessage {
    pub message_id: i64,
    pub group_id: u64,
    pub user_id: u64,
    pub raw_message: String,
    pub message: MessageBody,
    pub sender: Sender,
    /// `post_type == "message_sent"`：自己发出消息的回显。
    pub sent_by_self: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrivateMessage {
    pub message_id: i64,
    pub user_id: u64,
    pub raw_message: String,
    pub message: MessageBody,
    pub sender: Sender,
    pub sent_by_self: bool,
}

/// 解析后的桥消费事件。
#[derive(Debug, Clone, PartialEq)]
pub enum OneBotEvent {
    GroupMessage(Box<GroupMessage>),
    PrivateMessage(Box<PrivateMessage>),
    /// 群拍一拍（notice/notify/poke）。
    GroupPoke {
        group_id: u64,
        user_id: u64,
        target_id: u64,
    },
    /// 好友拍一拍（无 group_id）。
    FriendPoke {
        user_id: u64,
        target_id: u64,
    },
    /// 心跳：`status.online` 与 `status.good`（接收链路健康自评，可能缺字段）。
    Heartbeat {
        online: Option<bool>,
        good: Option<bool>,
    },
    /// 账号会话边沿（notice/bot_status，1.14.17+）。
    BotStatus {
        online: bool,
        user_id: u64,
    },
    /// 其余事件：桥暂不消费字段，透传 post_type 供健康记账。
    Other {
        post_type: String,
    },
}

fn parse_message_body(v: Option<&Value>) -> Result<MessageBody, EventParseError> {
    match v {
        Some(Value::Array(segments)) => Ok(MessageBody::Array(segments.clone())),
        Some(Value::String(s)) => Ok(MessageBody::Text(s.clone())),
        _ => Err(EventParseError("message must be array or string".into())),
    }
}

fn parse_sender(v: Option<&Value>) -> Result<Sender, EventParseError> {
    serde_json::from_value(v.cloned().unwrap_or(Value::Null))
        .map_err(|e| EventParseError(format!("sender: {e}")))
}

/// 解析一条事件载荷。
pub fn parse_event(event: &Value) -> Result<OneBotEvent, EventParseError> {
    let post_type = event
        .get("post_type")
        .and_then(Value::as_str)
        .ok_or_else(|| EventParseError("missing post_type".into()))?;
    let sent_by_self = post_type == "message_sent";
    match post_type {
        "message" | "message_sent" => {
            let message_type = event
                .get("message_type")
                .and_then(Value::as_str)
                .ok_or_else(|| EventParseError("missing message_type".into()))?;
            let message_id = event
                .get("message_id")
                .and_then(Value::as_i64)
                .ok_or_else(|| EventParseError("missing message_id".into()))?;
            let raw_message = event
                .get("raw_message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let message = parse_message_body(event.get("message"))?;
            let sender = parse_sender(event.get("sender"))?;
            let user_id = event
                .get("user_id")
                .and_then(Value::as_u64)
                .ok_or_else(|| EventParseError("missing user_id".into()))?;
            match message_type {
                "group" => {
                    let group_id = event
                        .get("group_id")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| EventParseError("missing group_id".into()))?;
                    Ok(OneBotEvent::GroupMessage(Box::new(GroupMessage {
                        message_id,
                        group_id,
                        user_id,
                        raw_message,
                        message,
                        sender,
                        sent_by_self,
                    })))
                }
                "private" => Ok(OneBotEvent::PrivateMessage(Box::new(PrivateMessage {
                    message_id,
                    user_id,
                    raw_message,
                    message,
                    sender,
                    sent_by_self,
                }))),
                other => Err(EventParseError(format!("unknown message_type: {other}"))),
            }
        }
        "notice" => {
            let notice_type = event
                .get("notice_type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if notice_type == "notify"
                && event.get("sub_type").and_then(Value::as_str) == Some("poke")
            {
                let user_id = event
                    .get("user_id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| EventParseError("poke: missing user_id".into()))?;
                let target_id = event.get("target_id").and_then(Value::as_u64).unwrap_or(0);
                return match event.get("group_id").and_then(Value::as_u64) {
                    Some(group_id) => Ok(OneBotEvent::GroupPoke {
                        group_id,
                        user_id,
                        target_id,
                    }),
                    None => Ok(OneBotEvent::FriendPoke { user_id, target_id }),
                };
            }
            if notice_type == "bot_status" {
                let online = event.get("sub_type").and_then(Value::as_str) == Some("online");
                let user_id = event.get("user_id").and_then(Value::as_u64).unwrap_or(0);
                return Ok(OneBotEvent::BotStatus { online, user_id });
            }
            Ok(OneBotEvent::Other {
                post_type: post_type.to_owned(),
            })
        }
        "meta_event" => {
            if event.get("meta_event_type").and_then(Value::as_str) == Some("heartbeat") {
                let status = event.get("status");
                let flag = |k: &str| status.and_then(|s| s.get(k)).and_then(Value::as_bool);
                return Ok(OneBotEvent::Heartbeat {
                    online: flag("online"),
                    good: flag("good"),
                });
            }
            Ok(OneBotEvent::Other {
                post_type: post_type.to_owned(),
            })
        }
        _ => Ok(OneBotEvent::Other {
            post_type: post_type.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 群消息：array 格式消息段 + sender（含 card/role）。
    #[test]
    fn parses_group_message() {
        let ev = json!({
            "time": 1729500000, "self_id": 10000,
            "post_type": "message", "message_type": "group", "sub_type": "normal",
            "message_id": -123456, "group_id": 4242, "user_id": 654321,
            "message": [
                { "type": "text", "data": { "text": "看看这只猫" } },
                { "type": "image", "data": { "file": "ABC123.image", "url": "https://example.com/a.jpg" } },
                { "type": "text", "data": { "text": " 哈哈" } }
            ],
            "raw_message": "看看这只猫[CQ:image,file=ABC123.image] 哈哈",
            "font": 0,
            "sender": { "user_id": 654321, "nickname": "群友", "card": "名片", "role": "member" }
        });
        match parse_event(&ev).unwrap() {
            OneBotEvent::GroupMessage(m) => {
                assert_eq!(m.message_id, -123456);
                assert_eq!(m.group_id, 4242);
                assert_eq!(m.user_id, 654321);
                assert!(!m.sent_by_self);
                assert_eq!(m.sender.card.as_deref(), Some("名片"));
                assert_eq!(extract_plain_text(&m.message), "看看这只猫 哈哈");
            }
            other => panic!("应为群消息，实际 {other:?}"),
        }
    }

    /// 私聊消息与 string 格式消息体；message_sent 标记自发回显。
    #[test]
    fn parses_private_and_sent() {
        let ev = json!({
            "time": 1, "self_id": 10000,
            "post_type": "message", "message_type": "private", "sub_type": "friend",
            "message_id": 77, "user_id": 654321,
            "message": "你好[CQ:face,id=1]",
            "raw_message": "你好[CQ:face,id=1]",
            "font": 0,
            "sender": { "user_id": 654321, "nickname": "好友" }
        });
        match parse_event(&ev).unwrap() {
            OneBotEvent::PrivateMessage(m) => {
                assert_eq!(m.user_id, 654321);
                assert_eq!(extract_plain_text(&m.message), "你好[CQ:face,id=1]");
            }
            other => panic!("应为私聊消息，实际 {other:?}"),
        }
        let sent = json!({
            "time": 1, "self_id": 10000,
            "post_type": "message_sent", "message_type": "group",
            "message_id": 78, "group_id": 4242, "user_id": 10000,
            "message": [ { "type": "text", "data": { "text": "我说的话" } } ],
            "raw_message": "我说的话", "font": 0,
            "sender": { "user_id": 10000, "nickname": "me", "role": "admin" }
        });
        match parse_event(&sent).unwrap() {
            OneBotEvent::GroupMessage(m) => assert!(m.sent_by_self),
            other => panic!("应为自发回显，实际 {other:?}"),
        }
    }

    /// 拍一拍：群内与好友两种；bot_status 边沿。
    #[test]
    fn parses_pokes_and_bot_status() {
        let poke = json!({
            "time": 1, "self_id": 10000, "post_type": "notice",
            "notice_type": "notify", "sub_type": "poke",
            "group_id": 4242, "user_id": 111, "target_id": 10000
        });
        assert_eq!(
            parse_event(&poke).unwrap(),
            OneBotEvent::GroupPoke {
                group_id: 4242,
                user_id: 111,
                target_id: 10000
            }
        );
        let friend_poke = json!({
            "time": 1, "self_id": 10000, "post_type": "notice",
            "notice_type": "notify", "sub_type": "poke",
            "user_id": 111, "target_id": 10000
        });
        assert_eq!(
            parse_event(&friend_poke).unwrap(),
            OneBotEvent::FriendPoke {
                user_id: 111,
                target_id: 10000
            }
        );
        let status = json!({
            "time": 1, "self_id": 10000, "post_type": "notice",
            "notice_type": "bot_status", "sub_type": "offline", "user_id": 10000
        });
        assert_eq!(
            parse_event(&status).unwrap(),
            OneBotEvent::BotStatus {
                online: false,
                user_id: 10000
            }
        );
    }

    /// 心跳：status.good 是接收链路健康的权威信号（可能缺字段——旧运行时）。
    #[test]
    fn parses_heartbeat_status() {
        let hb = json!({
            "time": 1, "self_id": 10000, "post_type": "meta_event",
            "meta_event_type": "heartbeat",
            "status": { "online": true, "good": false },
            "interval": 30000
        });
        assert_eq!(
            parse_event(&hb).unwrap(),
            OneBotEvent::Heartbeat {
                online: Some(true),
                good: Some(false)
            }
        );
        // 缺 status（老实现）→ 两个 None，不是错误。
        let bare = json!({
            "time": 1, "self_id": 10000, "post_type": "meta_event",
            "meta_event_type": "lifecycle", "sub_type": "connect"
        });
        assert!(matches!(
            parse_event(&bare).unwrap(),
            OneBotEvent::Other { .. }
        ));
    }

    /// 非消费事件透传 post_type；坏载荷显式失败。
    #[test]
    fn unknown_events_pass_through_and_bad_input_fails() {
        let friend_add = json!({
            "time": 1, "self_id": 10000, "post_type": "request",
            "request_type": "friend", "flag": "f1"
        });
        assert_eq!(
            parse_event(&friend_add).unwrap(),
            OneBotEvent::Other {
                post_type: "request".into()
            }
        );
        assert!(parse_event(&json!({ "time": 1 })).is_err());
        assert!(parse_event(&json!({ "post_type": "message", "message_type": "group" })).is_err());
        // 消息体既非数组也非字符串 → 失败。
        let bad = json!({
            "post_type": "message", "message_type": "private",
            "message_id": 1, "user_id": 2, "message": 42,
            "sender": { "user_id": 2 }
        });
        assert!(parse_event(&bad).is_err());
    }
}
