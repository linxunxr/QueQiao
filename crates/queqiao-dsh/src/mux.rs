//! remote.mux 事件流状态机（移植自 Node 版 `src/dsh-client.js` 的
//! `_remoteMuxGenerator` 及其 follow/forget 配套）。
//!
//! 「状态 + 事件 → 动作」纯状态机：连接生命周期由调用方（WS IO 壳）驱动，
//! 本模块只回答「给定连接事件，该发什么帧、上报什么信封、何时结束」。
//!
//! 实战契约（源码注释钉住的教训，全部保留）：
//! - **follow 记账跨重连保留**：重放**全部**期望 follow 而非本次连接排队的那些——
//!   少了这一步，DSH 一重启所有已存在的 QQ 会话就静默失联（turn/end 丢失 →
//!   QQ 上永远没有回复，且无报错）；
//! - **快照绝不混进 session/event**：开场快照里含历史 turn 的 turn/end，若走
//!   事件通道会让桥把「早已结束的回合」当新回合去发 QQ 回复——单独一种信封；
//! - **单条会话流被服务端结束必须重订阅**：旧代码不重订阅不打日志，断在回合
//!   中间会让 busy 永真、该群彻底哑掉且没有租约能救；只有 forget 过的会话
//!   （或 `session/not-found`）才让它走；
//! - **forget 防只增不减**：不删的话每次重连都为早已不存在的会话重开一条流、
//!   回一整份历史快照——内存与重连延迟随「历史会话数」线性增长；
//! - **断连（非主动中止）丢弃旧 Cookie**：重连会重新鉴权。

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use uuid::Uuid;

/// `session/follow` 开场快照携带的历史事件条数。DSH 默认 50，不足以回填逐轮花费；
/// 调大只影响首帧体积（每个已 follow 的会话一帧），不改变事件通道语义。
pub const FOLLOW_SNAPSHOT_MESSAGES: u32 = 200;

/// 状态机输出的动作（IO 壳解释执行）。
#[derive(Debug, Clone, PartialEq)]
pub enum MuxAction {
    /// 向 WS 发送一条 JSON 帧（open 等）。
    Send(String),
    /// 上层可见的信封。
    Enqueue(MuxEnvelope),
    /// 结束该 mux 连接（调用方负责按需重连）。
    End,
    /// 连接已断（非主动中止）：丢弃旧 Cookie，重连时重新鉴权。
    InvalidateAuth,
    /// 单条会话流被服务端结束但仍想订阅：延迟（Node 版 2s）后重开。
    RescheduleFollow(String),
}

/// 上层消费的信封（Node 版映射成旧 pumpMux 信封形态，Rust 版直接结构化）。
#[derive(Debug, Clone, PartialEq)]
pub enum MuxEnvelope {
    /// 会话事件帧（turn/start、assistant/message、turn/end、tool/* …）。
    SessionEvent { session_id: String, event: Value },
    /// 开场/重连快照：带 projections（token 累计权威）与 records（≤ maxMessages 条
    /// 历史事件）。**单独一种信封**，绝不混进 session/event（历史 turn/end 会污染）。
    SessionSnapshot { session_id: String, snapshot: Value },
    /// 审批请求（$events 通道）。
    ApprovalRequested {
        session_id: String,
        client_id: String,
        event_id: String,
        tool_name: Option<String>,
        call_id: Option<String>,
        reason: Option<String>,
    },
    /// 用户提问请求（$events 通道）。
    QuestionRequested {
        session_id: String,
        client_id: String,
        event_id: String,
        questions: Option<Value>,
    },
    /// 单条流报错（含 `session/not-found` 等业务错误码）。
    StreamError { stream_id: String, error: Value },
}

/// 出站打开帧：订阅一个会话（session/follow）。
pub fn follow_open_frame(stream_id: &str, session_id: &str, max_messages: u32) -> String {
    serde_json::to_string(&json!({
        "type": "open",
        "streamId": stream_id,
        "endpoint": "session/follow",
        "payload": {
            "args": {
                "request": {
                    "address": { "kind": "session", "sessionId": session_id },
                    "maxMessages": max_messages,
                }
            }
        }
    }))
    .expect("open 帧序列化必然成功")
}

/// 出站打开帧：全宿主事件通道（$events）。
pub fn events_open_frame(stream_id: &str) -> String {
    serde_json::to_string(&json!({
        "type": "open",
        "streamId": stream_id,
        "endpoint": "$events",
        "payload": { "args": {} }
    }))
    .expect("open 帧序列化必然成功")
}

/// 出站打开帧：全宿主控制流（session/control，stopSessionWork 的 baseline 回退路径）。
pub fn control_open_frame(stream_id: &str) -> String {
    serde_json::to_string(&json!({
        "type": "open",
        "streamId": stream_id,
        "endpoint": "session/control",
        "payload": { "args": {} }
    }))
    .expect("open 帧序列化必然成功")
}

/// 解析入站 mux 帧；非 JSON / 缺 type / 缺 streamId / 非文本语义 → `None`
///（调用方丢弃并记日志——单条坏帧不断连接）。
pub fn parse_mux_frame(text: &str) -> Option<(String, String, Option<Value>, Option<Value>)> {
    let msg: Value = serde_json::from_str(text).ok()?;
    let kind = msg.get("type").and_then(Value::as_str)?.to_owned();
    let stream_id = msg.get("streamId").and_then(Value::as_str)?.to_owned();
    let value = msg.get("value").cloned().filter(|v| !v.is_null());
    let error = msg.get("error").cloned().filter(|v| !v.is_null());
    Some((kind, stream_id, value, error))
}

/// 单条 mux 连接的状态机（`desired` 由上一条连接转移而来；跨重连保留）。
#[derive(Debug, Default)]
pub struct MuxSession {
    /// 期望订阅的会话集合（**跨重连保留**，forget/not-found 才移除）。
    desired: HashSet<String>,
    /// 本连接已开流的会话。
    followed: HashSet<String>,
    stream_to_session: HashMap<String, String>,
    session_to_stream: HashMap<String, String>,
    event_stream_id: Option<String>,
    event_client_id: Option<String>,
    socket_open: bool,
    ended: bool,
}

impl MuxSession {
    /// 新连接：从上一条连接（或空集）转入 desired follows。
    pub fn new(desired: HashSet<String>) -> Self {
        Self {
            desired,
            ..Default::default()
        }
    }

    pub fn is_ended(&self) -> bool {
        self.ended
    }

    /// 期望订阅集合的快照（重连时转入新会话实例）。
    pub fn desired(&self) -> HashSet<String> {
        self.desired.clone()
    }

    /// 声明想订阅一个会话：**先记账再发送**——即使此刻没有连接（或正在重连窗口），
    /// 重连时也会重放。返回要发送的帧（无连接时为空）。
    pub fn follow(&mut self, session_id: &str) -> Vec<MuxAction> {
        if session_id.is_empty() {
            return Vec::new();
        }
        self.desired.insert(session_id.to_owned());
        self.open_follow(session_id)
    }

    /// 取消一个会话的订阅（会话被退役/重置时调用）。
    /// 已经打开的流由 DSH 在会话消失时自行结束。
    pub fn forget(&mut self, session_id: &str) {
        self.desired.remove(session_id);
    }

    fn open_follow(&mut self, session_id: &str) -> Vec<MuxAction> {
        if self.ended
            || !self.socket_open
            || self.session_to_stream.contains_key(session_id)
            || self.followed.contains(session_id)
        {
            return Vec::new();
        }
        let stream_id = Uuid::new_v4().to_string();
        self.stream_to_session
            .insert(stream_id.clone(), session_id.to_owned());
        self.session_to_stream
            .insert(session_id.to_owned(), stream_id.clone());
        self.followed.insert(session_id.to_owned());
        vec![MuxAction::Send(follow_open_frame(
            &stream_id,
            session_id,
            FOLLOW_SNAPSHOT_MESSAGES,
        ))]
    }

    fn open_events(&mut self) -> Vec<MuxAction> {
        if self.ended || !self.socket_open || self.event_stream_id.is_some() {
            return Vec::new();
        }
        let stream_id = Uuid::new_v4().to_string();
        self.event_stream_id = Some(stream_id.clone());
        vec![MuxAction::Send(events_open_frame(&stream_id))]
    }

    /// 连接建立：重放**全部**期望 follow（不只是本次连接排队的）+ 打开 $events。
    pub fn on_socket_open(&mut self) -> Vec<MuxAction> {
        if self.ended {
            return Vec::new();
        }
        self.socket_open = true;
        let desired: Vec<String> = self.desired.iter().cloned().collect();
        let mut actions = Vec::new();
        for sid in desired {
            actions.extend(self.open_follow(&sid));
        }
        actions.extend(self.open_events());
        actions
    }

    /// 收到一条文本帧（解析失败静默丢弃）。
    pub fn on_text_frame(&mut self, text: &str) -> Vec<MuxAction> {
        if self.ended {
            return Vec::new();
        }
        let Some((kind, stream_id, value, error)) = parse_mux_frame(text) else {
            return Vec::new();
        };
        let is_event_stream = self.event_stream_id.as_deref() == Some(stream_id.as_str());
        let session_id = self.stream_to_session.get(&stream_id).cloned();
        match kind.as_str() {
            "item" => self.on_item(is_event_stream, session_id, value),
            "end" => self.on_stream_end(is_event_stream, session_id, &stream_id),
            "error" => self.on_stream_error(is_event_stream, session_id, &stream_id, error),
            // 其他类型（assistant-stream 等）当前桥接不需要，忽略。
            _ => Vec::new(),
        }
    }

    fn on_item(
        &mut self,
        is_event_stream: bool,
        session_id: Option<String>,
        value: Option<Value>,
    ) -> Vec<MuxAction> {
        let Some(value) = value else {
            return Vec::new();
        };
        if is_event_stream {
            let vtype = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if vtype == "ready" {
                // 记住本连接的事件通道 client id（waterfall 事件的归属凭证）。
                if let Some(client_id) = value.get("clientId").and_then(Value::as_str) {
                    self.event_client_id = Some(client_id.to_owned());
                }
                return Vec::new();
            }
            if vtype == "waterfall" && self.event_client_id.is_some() {
                let event = value
                    .get("event")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let client_id = self.event_client_id.clone().unwrap_or_default();
                let get_str = |k: &str| {
                    value
                        .get("request")
                        .and_then(|r| r.get(k))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                };
                if event == "approval/request" {
                    return vec![MuxAction::Enqueue(MuxEnvelope::ApprovalRequested {
                        session_id: value
                            .get("agentId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        client_id,
                        event_id: value
                            .get("eventId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        tool_name: get_str("toolName"),
                        call_id: get_str("callId"),
                        reason: get_str("reason"),
                    })];
                }
                if event == "user-questions/request" {
                    return vec![MuxAction::Enqueue(MuxEnvelope::QuestionRequested {
                        session_id: value
                            .get("agentId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        client_id,
                        event_id: value
                            .get("eventId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        questions: value
                            .get("request")
                            .and_then(|r| r.get("questions"))
                            .cloned(),
                    })];
                }
                // 其他 emit/waterfall 事件当前桥接不需要，保持忽略。
            }
            return Vec::new();
        }
        let Some(session_id) = session_id else {
            return Vec::new();
        };
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "event" => vec![MuxAction::Enqueue(MuxEnvelope::SessionEvent {
                session_id,
                event: value.get("event").cloned().unwrap_or(Value::Null),
            })],
            "snapshot" => vec![MuxAction::Enqueue(MuxEnvelope::SessionSnapshot {
                session_id,
                snapshot: value,
            })],
            _ => Vec::new(),
        }
    }

    fn drop_stream_maps(&mut self, stream_id: &str, session_id: &str) {
        self.stream_to_session.remove(stream_id);
        self.session_to_stream.remove(session_id);
        self.followed.remove(session_id);
    }

    fn end_mux(&mut self) -> Vec<MuxAction> {
        if self.ended {
            return Vec::new();
        }
        self.ended = true;
        vec![MuxAction::End]
    }

    fn on_stream_end(
        &mut self,
        is_event_stream: bool,
        session_id: Option<String>,
        stream_id: &str,
    ) -> Vec<MuxAction> {
        if is_event_stream {
            // 仅清掉 id 会让提问/审批通道永久失联；结束 mux 由桥接重连并重开。
            self.event_stream_id = None;
            self.event_client_id = None;
            return self.end_mux();
        }
        let Some(session_id) = session_id else {
            return Vec::new();
        };
        self.drop_stream_maps(stream_id, &session_id);
        // DSH 正常结束了单条会话流（follow 被回收/会话归档/服务端空闲清理）。
        // 主动重开一次订阅；仍然想订阅的会话才重开（forget 过的就让它走）。
        if self.desired.contains(&session_id) {
            return vec![MuxAction::RescheduleFollow(session_id)];
        }
        Vec::new()
    }

    fn on_stream_error(
        &mut self,
        is_event_stream: bool,
        session_id: Option<String>,
        stream_id: &str,
        error: Option<Value>,
    ) -> Vec<MuxAction> {
        if is_event_stream {
            self.event_stream_id = None;
            self.event_client_id = None;
            return self.end_mux();
        }
        let Some(session_id) = session_id else {
            return Vec::new();
        };
        self.drop_stream_maps(stream_id, &session_id);
        let code = error
            .as_ref()
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        // 只有已不存在的会话才永久取消订阅；临时服务错误必须在重连后重试。
        if code == "session/not-found" {
            self.desired.remove(&session_id);
        }
        let mut actions = vec![MuxAction::Enqueue(MuxEnvelope::StreamError {
            stream_id: stream_id.to_owned(),
            error: error.unwrap_or(Value::Null),
        })];
        if code != "session/not-found" {
            actions.extend(self.end_mux());
        }
        actions
    }

    /// RescheduleFollow 的延迟到期重开（IO 壳的定时回调）。
    pub fn reopen_follow(&mut self, session_id: &str) -> Vec<MuxAction> {
        if !self.desired.contains(session_id) {
            return Vec::new();
        }
        self.open_follow(session_id)
    }

    /// 连接断开 / 传输错误（非主动中止）：丢弃旧 Cookie 并结束（重连重新鉴权）。
    pub fn on_disconnect(&mut self) -> Vec<MuxAction> {
        let mut actions = self.end_mux();
        if !actions.is_empty() {
            actions.insert(0, MuxAction::InvalidateAuth);
        }
        actions
    }

    /// 调用方主动中止：结束但**不**失效鉴权（正常关停不是「Cookie 失效」的证据）。
    pub fn abort(&mut self) -> Vec<MuxAction> {
        self.end_mux()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 打开重放：desired 全部 follow + $events；帧形状符合协议。
    #[test]
    fn socket_open_replays_all_desired_follows() {
        let mut s = MuxSession::new(HashSet::from(["s-1".to_owned(), "s-2".to_owned()]));
        // 连接前 follow 只记账不发。
        assert!(s.follow("s-3").is_empty());
        let actions = s.on_socket_open();
        let sends: Vec<&String> = actions
            .iter()
            .filter_map(|a| match a {
                MuxAction::Send(f) => Some(f),
                _ => None,
            })
            .collect();
        assert_eq!(sends.len(), 4, "3 条 follow + 1 条 $events");
        let parsed: Vec<Value> = sends
            .iter()
            .map(|f| serde_json::from_str(f).unwrap())
            .collect();
        for p in &parsed[..3] {
            assert_eq!(p["type"], "open");
            assert_eq!(p["endpoint"], "session/follow");
            assert_eq!(
                p["payload"]["args"]["request"]["maxMessages"],
                FOLLOW_SNAPSHOT_MESSAGES
            );
            assert_eq!(
                p["payload"]["args"]["request"]["address"]["kind"],
                "session"
            );
        }
        let session_ids: Vec<&str> = parsed[..3]
            .iter()
            .map(|p| {
                p["payload"]["args"]["request"]["address"]["sessionId"]
                    .as_str()
                    .unwrap()
            })
            .collect();
        assert!(
            session_ids.contains(&"s-1")
                && session_ids.contains(&"s-2")
                && session_ids.contains(&"s-3")
        );
        assert_eq!(parsed[3]["endpoint"], "$events");
        assert_eq!(parsed[3]["payload"], json!({ "args": {} }));
    }

    /// 会话事件与快照映射为不同信封（快照绝不混进 event——历史 turn/end 污染契约）。
    #[test]
    fn items_map_to_distinct_envelopes() {
        let mut s = MuxSession::new(HashSet::new());
        s.on_socket_open();
        // 拿一条 follow 流的 streamId（从发送的 open 帧抽）。
        let open = s.follow("s-1");
        let open_json: Value = serde_json::from_str(match &open[0] {
            MuxAction::Send(f) => f,
            _ => panic!("follow 应发送 open 帧"),
        })
        .unwrap();
        let stream_id = open_json["streamId"].as_str().unwrap().to_owned();

        let evt = format!(
            r#"{{"type":"item","streamId":"{stream_id}","value":{{"type":"event","event":{{"type":"turn/end","data":{{}}}}}}}}"#
        );
        let actions = s.on_text_frame(&evt);
        assert_eq!(
            actions,
            vec![MuxAction::Enqueue(MuxEnvelope::SessionEvent {
                session_id: "s-1".into(),
                event: json!({ "type": "turn/end", "data": {} }),
            })]
        );

        let snap = format!(
            r#"{{"type":"item","streamId":"{stream_id}","value":{{"type":"snapshot","records":[],"projections":{{}}}}}}"#
        );
        let actions = s.on_text_frame(&snap);
        assert!(matches!(
            actions.as_slice(),
            [MuxAction::Enqueue(MuxEnvelope::SessionSnapshot { session_id, snapshot })]
                if session_id == "s-1" && snapshot["records"].is_array()
        ));
    }

    /// $events：ready 记 clientId，waterfall 的 approval/question 转结构化信封。
    #[test]
    fn events_channel_maps_approval_and_question() {
        let mut s = MuxSession::new(HashSet::new());
        let actions = s.on_socket_open();
        let events_stream = actions
            .iter()
            .filter_map(|a| match a {
                MuxAction::Send(f) => Some(f.clone()),
                _ => None,
            })
            .map(|f| serde_json::from_str::<Value>(&f).unwrap())
            .find(|p| p["endpoint"] == "$events")
            .unwrap();
        let sid = events_stream["streamId"].as_str().unwrap().to_owned();

        // ready 之前 watermark 事件被忽略。
        let pre = format!(
            r#"{{"type":"item","streamId":"{sid}","value":{{"type":"waterfall","event":"approval/request","eventId":"e-0","agentId":"s-1"}}}}"#
        );
        assert!(s.on_text_frame(&pre).is_empty());

        let ready = format!(
            r#"{{"type":"item","streamId":"{sid}","value":{{"type":"ready","clientId":"cli-9"}}}}"#
        );
        assert!(s.on_text_frame(&ready).is_empty());

        let approval = format!(
            r#"{{"type":"item","streamId":"{sid}","value":{{"type":"waterfall","event":"approval/request","eventId":"e-1","agentId":"s-1","request":{{"toolName":"qq_send_message","callId":"c-1","reason":"needs review"}}}}}}"#
        );
        let actions = s.on_text_frame(&approval);
        assert_eq!(
            actions,
            vec![MuxAction::Enqueue(MuxEnvelope::ApprovalRequested {
                session_id: "s-1".into(),
                client_id: "cli-9".into(),
                event_id: "e-1".into(),
                tool_name: Some("qq_send_message".into()),
                call_id: Some("c-1".into()),
                reason: Some("needs review".into()),
            })]
        );

        let question = format!(
            r#"{{"type":"item","streamId":"{sid}","value":{{"type":"waterfall","event":"user-questions/request","eventId":"e-2","agentId":"s-2","request":{{"questions":[{{"id":"q1"}}]}}}}}}"#
        );
        let actions = s.on_text_frame(&question);
        assert!(matches!(
            actions.as_slice(),
            [MuxAction::Enqueue(MuxEnvelope::QuestionRequested { session_id, client_id, event_id, .. })]
                if session_id == "s-2" && client_id == "cli-9" && event_id == "e-2"
        ));
    }

    /// 单条会话流被服务端结束：仍想订阅 → 重排；reopen 后重开。
    #[test]
    fn server_ended_stream_resubscribes() {
        let mut s = MuxSession::new(HashSet::from(["s-1".to_owned()]));
        let open = s.on_socket_open();
        let follow_frame: Value = open
            .iter()
            .filter_map(|a| match a {
                MuxAction::Send(f) => serde_json::from_str::<Value>(f).ok(),
                _ => None,
            })
            .find(|p| p["endpoint"] == "session/follow")
            .unwrap();
        let stream_id = follow_frame["streamId"].as_str().unwrap().to_owned();

        let end = format!(r#"{{"type":"end","streamId":"{stream_id}"}}"#);
        assert_eq!(
            s.on_text_frame(&end),
            vec![MuxAction::RescheduleFollow("s-1".into())]
        );
        // 重排到期：重开一条新流（新 streamId）。
        let reopen = s.reopen_follow("s-1");
        assert!(matches!(&reopen[..], [MuxAction::Send(f)] if f.contains("session/follow")));
        // forget 后被结束：不重排（让它走）。
        s.forget("s-1");
        let end_again = format!(
            r#"{{"type":"end","streamId":"{}"}}"#,
            serde_json::from_str::<Value>(match &reopen[0] {
                MuxAction::Send(f) => f,
                _ => unreachable!(),
            })
            .unwrap()["streamId"]
                .as_str()
                .unwrap()
        );
        assert!(s.on_text_frame(&end_again).is_empty());
    }

    /// 流错误：not-found 永久取消订阅（从 desired 移除）并上报；其他错误结束整条 mux。
    #[test]
    fn stream_errors_not_found_vs_fatal() {
        let mut s = MuxSession::new(HashSet::from(["s-1".to_owned()]));
        let open = s.on_socket_open();
        let follow_frame: Value = open
            .iter()
            .filter_map(|a| match a {
                MuxAction::Send(f) => serde_json::from_str::<Value>(f).ok(),
                _ => None,
            })
            .find(|p| p["endpoint"] == "session/follow")
            .unwrap();
        let stream_id = follow_frame["streamId"].as_str().unwrap().to_owned();

        let not_found = format!(
            r#"{{"type":"error","streamId":"{stream_id}","error":{{"code":"session/not-found","message":"gone"}}}}"#
        );
        let actions = s.on_text_frame(&not_found);
        assert!(
            actions.contains(&MuxAction::Enqueue(MuxEnvelope::StreamError {
                stream_id: stream_id.clone(),
                error: json!({ "code": "session/not-found", "message": "gone" }),
            }))
        );
        assert!(!actions.contains(&MuxAction::End), "not-found 不结束 mux");
        assert!(!s.desired().contains("s-1"), "not-found 永久取消订阅");

        // 其他错误：上报 + 结束。
        let mut s2 = MuxSession::new(HashSet::from(["s-2".to_owned()]));
        let open2 = s2.on_socket_open();
        let f2: Value = open2
            .iter()
            .filter_map(|a| match a {
                MuxAction::Send(f) => serde_json::from_str::<Value>(f).ok(),
                _ => None,
            })
            .find(|p| p["endpoint"] == "session/follow")
            .unwrap();
        let sid2 = f2["streamId"].as_str().unwrap().to_owned();
        let fatal = format!(
            r#"{{"type":"error","streamId":"{sid2}","error":{{"code":"gateway/boom","message":"x"}}}}"#
        );
        let actions = s2.on_text_frame(&fatal);
        assert!(actions.contains(&MuxAction::End));
        assert!(s2.is_ended());
    }

    /// $events 流结束/报错 → 整条 mux 结束（提问/审批通道不能半死不活）。
    #[test]
    fn events_stream_ending_ends_mux() {
        for frame_kind in ["end", "error"] {
            let mut s = MuxSession::new(HashSet::new());
            let open = s.on_socket_open();
            let events: Value = open
                .iter()
                .filter_map(|a| match a {
                    MuxAction::Send(f) => serde_json::from_str::<Value>(f).ok(),
                    _ => None,
                })
                .find(|p| p["endpoint"] == "$events")
                .unwrap();
            let sid = events["streamId"].as_str().unwrap().to_owned();
            let frame = if frame_kind == "end" {
                format!(r#"{{"type":"end","streamId":"{sid}"}}"#)
            } else {
                format!(
                    r#"{{"type":"error","streamId":"{sid}","error":{{"code":"x","message":"y"}}}}"#
                )
            };
            let actions = s.on_text_frame(&frame);
            assert!(actions.contains(&MuxAction::End), "{frame_kind} 应结束 mux");
            assert!(s.is_ended());
        }
    }

    /// 断连（非中止）清 Cookie；主动中止不清。
    #[test]
    fn disconnect_invalidates_auth_but_abort_does_not() {
        let mut s = MuxSession::new(HashSet::new());
        let actions = s.on_disconnect();
        assert!(actions.contains(&MuxAction::InvalidateAuth));
        assert!(actions.contains(&MuxAction::End));

        let mut s2 = MuxSession::new(HashSet::new());
        let actions = s2.abort();
        assert!(actions.contains(&MuxAction::End));
        assert!(!actions.contains(&MuxAction::InvalidateAuth));
        // 已结束的会话再断连不再产出动作。
        assert!(s2.on_disconnect().is_empty());
    }

    /// 坏帧静默丢弃（非 JSON / 缺 type / 缺 streamId），连接不受影响。
    #[test]
    fn malformed_frames_are_dropped() {
        let mut s = MuxSession::new(HashSet::new());
        s.on_socket_open();
        for bad in [
            "not json",
            r#"{"streamId":"x"}"#,
            r#"{"type":"item"}"#,
            r#"{"type":42,"streamId":"x"}"#,
            "",
        ] {
            assert!(s.on_text_frame(bad).is_empty(), "bad={bad:?}");
        }
        assert!(!s.is_ended(), "坏帧不断连接");
    }

    /// control 打开帧形状（stopSessionWork 的 baseline 回退路径用）。
    #[test]
    fn control_open_frame_shape() {
        let frame: Value = serde_json::from_str(&control_open_frame("cs-1")).unwrap();
        assert_eq!(frame["type"], "open");
        assert_eq!(frame["streamId"], "cs-1");
        assert_eq!(frame["endpoint"], "session/control");
        assert_eq!(frame["payload"], json!({ "args": {} }));
    }
}
