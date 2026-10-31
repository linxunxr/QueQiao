//! remote.mux 的 WS 运行壳（Node 版 `_remoteMuxGenerator` 的 IO 编排等价物）。
//!
//! 分层：[`MuxSession`]（纯状态机，见 [`crate::mux`]）回答「该做什么」；
//! 本模块的 [`MuxRunner`] 驱动连接生命周期——把 WS 事件与外部命令喂给状态机、
//! 解释执行它输出的动作（发帧 / 上报信封 / 排重订阅定时），结束时返回
//! [`RunOutcome`]（是否需要作废鉴权、剩余期望订阅——重连时转入新会话）。
//! 重连退避与循环归 bin 编排（M2 冒烟示例里有最小形态）。
//!
//! 传输抽象 [`WsStream`]（async fn in trait）：测试注入内存 mock，生产用
//! tokio-tungstenite 的 [`TungsteniteWs`]（feature `ws`）。

use std::collections::HashSet;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::client::ClientError;
use crate::inbox::{control_baseline_inbox_items, inbox_item_ids};
use crate::mux::{MuxAction, MuxEnvelope, MuxSession, control_open_frame, parse_mux_frame};
use serde_json::Value;

/// 单条会话流被服务端结束后的重订阅延迟（Node 版 setTimeout 2000）。
pub const RESUBSCRIBE_DELAY: Duration = Duration::from_secs(2);

/// 文本帧 WebSocket 传输抽象。
///
/// 实现者的 future 需要 Send 时由调用侧泛型约束检查（MuxRunner 已按
/// `tokio::spawn` 的要求验证）；本 trait 仅 crate 内消费。
#[allow(async_fn_in_trait)]
pub trait WsStream: Send {
    type Error: std::fmt::Display + Send;

    /// 发送一条文本帧。失败由随后的 recv 报错兜底（连接已死 → 断连清理）。
    async fn send_text(&mut self, text: &str) -> Result<(), Self::Error>;
    /// 收一条文本帧；`Ok(None)` 表示连接关闭。非文本帧（ping 等）由实现处理。
    async fn recv_text(&mut self) -> Result<Option<String>, Self::Error>;
}

/// mux 的 WS URL：`/api/remote.mux`，http(s) → ws(s)。
pub fn mux_ws_url(base_url: &str) -> String {
    let mut u = url::Url::parse(base_url).expect("base_url 已在上游校验");
    let scheme = match u.scheme() {
        "https" => "wss",
        _ => "ws",
    };
    u.set_scheme(scheme).expect("http(s) → ws(s) 必然合法");
    u.set_path("/api/remote.mux");
    u.set_query(None);
    u.set_fragment(None);
    u.to_string()
}

/// 外部命令（bin 编排 → mux 连接）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MuxCommand {
    Follow(String),
    Forget(String),
    Abort,
}

/// 一次 mux 连接生命周期的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// 断连（非主动中止）时应作废鉴权（重连重新交换/铸造）。
    pub invalidated_auth: bool,
    /// 结束时仍想订阅的会话集合（重连时转入新 [`MuxSession`]）。
    pub desired: HashSet<String>,
}

/// 驱动一条 mux 连接直到结束。
pub struct MuxRunner<W: WsStream> {
    ws: W,
    session: MuxSession,
    envelopes: mpsc::Sender<MuxEnvelope>,
    commands: mpsc::Receiver<MuxCommand>,
    reschedule: Vec<(tokio::time::Instant, String)>,
    resubscribe_delay: Duration,
    invalidated_auth: bool,
}

impl<W: WsStream> MuxRunner<W> {
    /// `envelopes` 是上层消费信封的出口；`commands` 是命令入口（所有 sender
    /// drop 时视为 Abort）。
    pub fn new(
        ws: W,
        desired: HashSet<String>,
        envelopes: mpsc::Sender<MuxEnvelope>,
        commands: mpsc::Receiver<MuxCommand>,
    ) -> Self {
        Self {
            ws,
            session: MuxSession::new(desired),
            envelopes,
            commands,
            reschedule: Vec::new(),
            resubscribe_delay: RESUBSCRIBE_DELAY,
            invalidated_auth: false,
        }
    }

    /// 测试钩子：缩短重订阅延迟。
    pub fn with_resubscribe_delay(mut self, delay: Duration) -> Self {
        self.resubscribe_delay = delay;
        self
    }

    async fn apply(&mut self, actions: Vec<MuxAction>) {
        for action in actions {
            match action {
                MuxAction::Send(frame) => {
                    // 发送失败不展开处理：连接已死，随后的 recv 会报错走断连清理。
                    let _ = self.ws.send_text(&frame).await;
                }
                MuxAction::Enqueue(env) => {
                    let _ = self.envelopes.send(env).await;
                }
                MuxAction::End => {}
                MuxAction::InvalidateAuth => self.invalidated_auth = true,
                MuxAction::RescheduleFollow(session_id) => {
                    let at = tokio::time::Instant::now() + self.resubscribe_delay;
                    self.reschedule.push((at, session_id));
                }
            }
        }
    }

    /// 主循环：连接建立即重放全部期望订阅（见 MuxSession 契约），随后三路复用
    /// （重订阅定时 / 外部命令 / WS 帧）直到 End / 断连 / Abort。
    pub async fn run(mut self) -> RunOutcome {
        let actions = self.session.on_socket_open();
        self.apply(actions).await;
        loop {
            if self.session.is_ended() {
                break;
            }
            let next_timer = self.reschedule.iter().map(|(at, _)| *at).min();
            let timer = async {
                match next_timer {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = timer => {
                    let now = tokio::time::Instant::now();
                    let due: Vec<String> = self
                        .reschedule
                        .iter()
                        .filter(|(at, _)| *at <= now)
                        .map(|(_, sid)| sid.clone())
                        .collect();
                    self.reschedule.retain(|(at, _)| *at > now);
                    for sid in due {
                        let actions = self.session.reopen_follow(&sid);
                        self.apply(actions).await;
                    }
                }
                cmd = self.commands.recv() => {
                    match cmd {
                        Some(MuxCommand::Follow(sid)) => {
                            let actions = self.session.follow(&sid);
                            self.apply(actions).await;
                        }
                        Some(MuxCommand::Forget(sid)) => self.session.forget(&sid),
                        Some(MuxCommand::Abort) | None => {
                            let actions = self.session.abort();
                            self.apply(actions).await;
                            break;
                        }
                    }
                }
                frame = self.ws.recv_text() => {
                    match frame {
                        Ok(Some(text)) => {
                            let actions = self.session.on_text_frame(&text);
                            self.apply(actions).await;
                        }
                        Ok(None) | Err(_) => {
                            let actions = self.session.on_disconnect();
                            self.apply(actions).await;
                            break;
                        }
                    }
                }
            }
        }
        RunOutcome {
            invalidated_auth: self.invalidated_auth,
            desired: self.session.desired(),
        }
    }
}

/// 退役前读队列的 WS 回退路径（`session/control` 开场 baseline）：
/// 打开控制流 → 等本流的 baseline item → 解析 inbox → 调用方负责关闭连接。
///
/// Node 版语义：baseline 只到或流被服务端结束前有效；超时/关闭/错误都显式失败
/// （"closed before baseline"），绝不静默当空队列。
pub async fn read_queue_via_control<W: WsStream>(
    ws: &mut W,
    session_id: &str,
    stream_id: &str,
    deadline: tokio::time::Instant,
) -> Result<Vec<String>, ClientError> {
    let open = control_open_frame(stream_id);
    ws.send_text(&open)
        .await
        .map_err(|e| ClientError::Transport(crate::client::TransportError(e.to_string())))?;
    loop {
        let frame = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                return Err(ClientError::Transport(crate::client::TransportError(
                    "session/control timed out before baseline".to_owned(),
                )));
            }
            frame = ws.recv_text() => frame,
        };
        match frame {
            Ok(Some(text)) => {
                let Some((kind, sid, value, error)) = parse_mux_frame(&text) else {
                    continue;
                };
                if sid != stream_id {
                    continue;
                }
                match kind.as_str() {
                    "item"
                        if value
                            .as_ref()
                            .and_then(|v| v.get("type"))
                            .and_then(Value::as_str)
                            == Some("baseline") =>
                    {
                        let items = control_baseline_inbox_items(value.as_ref(), session_id)?;
                        return Ok(inbox_item_ids(&items)?);
                    }
                    "end" | "error" => {
                        let code = error
                            .as_ref()
                            .and_then(|e| e.get("code"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let detail = if code.is_empty() {
                            String::new()
                        } else {
                            format!(" ({code})")
                        };
                        return Err(ClientError::Transport(crate::client::TransportError(
                            format!("session/control ended before baseline{detail}"),
                        )));
                    }
                    _ => {}
                }
            }
            Ok(None) => {
                return Err(ClientError::Transport(crate::client::TransportError(
                    "session/control connection closed before baseline".to_owned(),
                )));
            }
            Err(e) => {
                return Err(ClientError::Transport(crate::client::TransportError(
                    e.to_string(),
                )));
            }
        }
    }
}

// ── 生产实现（tokio-tungstenite）────────────────────────────────────────────

/// tokio-tungstenite 的 [`WsStream`] 实现（feature `ws`）。
#[cfg(feature = "ws")]
pub mod tungstenite {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

    use super::WsStream;

    /// 已连接的 mux WebSocket。
    pub struct TungsteniteWs {
        ws: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    }

    impl TungsteniteWs {
        /// 连接 mux 端点（`base_url` 为 DSH HTTP 基址；cookie 为鉴权头）。
        pub async fn connect(base_url: &str, cookie: &str) -> Result<Self, String> {
            let url = super::mux_ws_url(base_url);
            let mut req = url
                .as_str()
                .into_client_request()
                .map_err(|e| format!("bad mux url {url}: {e}"))?;
            req.headers_mut().insert(
                "cookie",
                HeaderValue::from_str(cookie).map_err(|e| e.to_string())?,
            );
            let (ws, _resp) = connect_async(req).await.map_err(|e| e.to_string())?;
            Ok(Self { ws })
        }
    }

    impl WsStream for TungsteniteWs {
        type Error = String;

        /// 写超时 30s：对端停滞/发送缓冲满时 sink 的 send 可无限挂起——
        /// MuxRunner 任务卡在单次发送上，mux 事件流随之静默且无日志。
        async fn send_text(&mut self, text: &str) -> Result<(), Self::Error> {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.ws.send(Message::text(text.to_owned())),
            )
            .await
            .map_err(|_| "ws 写超时（30s）——对端停滞或缓冲满".to_string())?
            .map_err(|e| e.to_string())
        }

        async fn recv_text(&mut self) -> Result<Option<String>, Self::Error> {
            loop {
                match self.ws.next().await {
                    None => return Ok(None),
                    Some(Ok(Message::Text(t))) => return Ok(Some(t.to_string())),
                    // 文本协议：二进制帧丢弃；ping 回 pong；close 视为断开。
                    Some(Ok(Message::Ping(p))) => {
                        let _ = self.ws.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Close(_))) => return Ok(None),
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Err(e.to_string()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// 内存 WS：入站帧从共享队列吐出（可运行时注入），出站帧记入共享列表。
    /// `Arc<Mutex>` 以满足 `WsStream: Send`（tokio::spawn 需要）。
    #[derive(Clone)]
    struct MockWs {
        incoming: Arc<Mutex<VecDeque<String>>>,
        sent: Arc<Mutex<Vec<String>>>,
    }

    impl MockWs {
        fn closed() -> Self {
            Self {
                incoming: Arc::new(Mutex::new(VecDeque::new())),
                sent: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl WsStream for MockWs {
        type Error = String;

        async fn send_text(&mut self, text: &str) -> Result<(), Self::Error> {
            self.sent.lock().unwrap().push(text.to_owned());
            Ok(())
        }

        async fn recv_text(&mut self) -> Result<Option<String>, Self::Error> {
            // 空队列挂起而非立即断连，等测试注入帧或 Abort。
            loop {
                if let Some(frame) = self.incoming.lock().unwrap().pop_front() {
                    return Ok(Some(frame));
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }

    fn frame(kind: &str, stream_id: &str, value: Value) -> String {
        serde_json::to_string(&serde_json::json!({
            "type": kind, "streamId": stream_id, "value": value
        }))
        .unwrap()
    }

    /// mux URL：http(s) → ws(s) + /api/remote.mux。
    #[test]
    fn mux_url_conversion() {
        assert_eq!(
            mux_ws_url("http://127.0.0.1:3080"),
            "ws://127.0.0.1:3080/api/remote.mux"
        );
        assert_eq!(
            mux_ws_url("https://dsh.example.com/"),
            "wss://dsh.example.com/api/remote.mux"
        );
    }

    /// 打开重放 desired + $events；follow 命令即时开流；Abort 不作废鉴权。
    #[tokio::test]
    async fn runner_open_replay_follow_command_and_abort() {
        let ws = MockWs::closed();
        let (env_tx, _env_rx) = mpsc::channel(16);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let runner = MuxRunner::new(
            ws.clone(),
            HashSet::from(["s-1".to_owned()]),
            env_tx,
            cmd_rx,
        );
        let handle = tokio::spawn(runner.run());
        cmd_tx
            .send(MuxCommand::Follow("s-2".to_owned()))
            .await
            .unwrap();
        cmd_tx.send(MuxCommand::Abort).await.unwrap();
        let outcome = handle.await.unwrap();

        assert!(!outcome.invalidated_auth, "Abort 不作废鉴权");
        assert!(outcome.desired.contains("s-1") && outcome.desired.contains("s-2"));
        // 打开帧：s-1（重放）+ $events + s-2（命令 follow）。
        let sent: Vec<Value> = ws
            .sent
            .lock()
            .unwrap()
            .iter()
            .map(|f| serde_json::from_str(f).unwrap())
            .collect();
        let endpoints: Vec<&str> = sent
            .iter()
            .map(|p| p["endpoint"].as_str().unwrap_or_default())
            .collect();
        assert!(endpoints.contains(&"session/follow"));
        assert_eq!(
            endpoints.iter().filter(|e| **e == "session/follow").count(),
            2,
            "desired 重放 + 命令 follow"
        );
        assert!(endpoints.contains(&"$events"));
    }

    /// 事件帧 → 信封出口；单流 end → 重订阅定时生效。
    #[tokio::test]
    async fn runner_delivers_envelopes_and_resubscribes() {
        let ws = MockWs::closed();
        let (env_tx, mut env_rx) = mpsc::channel(16);
        let (_cmd_tx, cmd_rx) = mpsc::channel(16);
        let runner = MuxRunner::new(
            ws.clone(),
            HashSet::from(["s-1".to_owned()]),
            env_tx,
            cmd_rx,
        )
        .with_resubscribe_delay(Duration::from_millis(10));
        let handle = tokio::spawn(runner.run());

        // 等 follow open 帧发出，取它的 streamId。
        let follow_sid = loop {
            let parsed: Vec<Value> = ws
                .sent
                .lock()
                .unwrap()
                .iter()
                .filter_map(|f| serde_json::from_str::<Value>(f).ok())
                .filter(|p| p["endpoint"] == "session/follow")
                .collect();
            if let Some(p) = parsed.first() {
                break p["streamId"].as_str().unwrap().to_owned();
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        };

        // 事件帧 → SessionEvent 信封。
        ws.incoming.lock().unwrap().push_back(frame(
            "item",
            &follow_sid,
            serde_json::json!({ "type": "event", "event": { "type": "turn/end", "data": {} } }),
        ));
        match env_rx.recv().await.unwrap() {
            MuxEnvelope::SessionEvent { session_id, event } => {
                assert_eq!(session_id, "s-1");
                assert_eq!(event["type"], "turn/end");
            }
            other => panic!("应为 SessionEvent，实际 {other:?}"),
        }

        // 单流 end → 重订阅定时生效（重订阅帧落在 before 位置）。
        let before = ws.sent.lock().unwrap().len();
        ws.incoming
            .lock()
            .unwrap()
            .push_back(frame("end", &follow_sid, Value::Null));
        let reopened = loop {
            let parsed = {
                let sent = ws.sent.lock().unwrap();
                sent.get(before)
                    .and_then(|f| serde_json::from_str::<Value>(f).ok())
            };
            if let Some(v) = parsed {
                break v;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        assert_eq!(reopened["endpoint"], "session/follow", "重订阅帧");

        // 断连（注入通道关闭标记）：用 Abort 结束。
        // （recv 挂起型 mock 不会自然断连；Abort 路径已由上一条测试覆盖。）
        _cmd_tx.send(MuxCommand::Abort).await.unwrap();
        let outcome = handle.await.unwrap();
        assert!(outcome.desired.contains("s-1"), "重订阅后 desired 保留");
    }

    /// control baseline 回退：open → baseline item → itemId 列表。
    #[tokio::test]
    async fn control_fallback_reads_baseline() {
        let mut ws = MockWs::closed();
        ws.incoming.lock().unwrap().push_back(frame(
            "item",
            "cs-1",
            serde_json::json!({
                "type": "baseline",
                "value": { "projections": { "s-1": { "asOfSeq": 3, "values": {
                    "inbox": { "next-turn": [ { "id": "m-1" } ], "next-step": [] }
                } } } }
            }),
        ));
        let ids = read_queue_via_control(
            &mut ws,
            "s-1",
            "cs-1",
            tokio::time::Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(ids, ["m-1"]);
        // 发送的是 session/control 打开帧。
        let open: Value = serde_json::from_str(&ws.sent.lock().unwrap()[0]).unwrap();
        assert_eq!(open["endpoint"], "session/control");
    }

    /// control 回退：流 end 在 baseline 之前 → 显式失败。
    #[tokio::test]
    async fn control_fallback_fails_before_baseline() {
        let mut ws = MockWs::closed();
        ws.incoming
            .lock()
            .unwrap()
            .push_back(frame("end", "cs-1", Value::Null));
        let err = read_queue_via_control(
            &mut ws,
            "s-1",
            "cs-1",
            tokio::time::Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("ended before baseline"));
    }
}
