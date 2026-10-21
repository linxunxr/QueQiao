//! OneBot WS 事件流客户端（feature `io`）。
//!
//! 连接：`ws://<host>:<port>?access_token=<token>`（SDK `appendAccessToken` 语义）。
//! 本模块提供单连接收帧循环；断线重连与 token 自愈编排（重连失败时重新发现
//! 当前账号 token 并热更新——否则表现为「桥活着但收发全废」）归 bin 层（M4）。

use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::event::{OneBotEvent, parse_event};

/// WS 事件流错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventStreamError(pub String);

impl std::fmt::Display for EventStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for EventStreamError {}

/// 已连接的事件流。
pub struct EventStream {
    ws: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
}

/// 把 token 追加到 WS URL 查询串（HTTP 与 WS 是**两个不同 token**，别混用）。
/// path 为空时补 `/`——无路径的 `ws://host:port?…` 经 tungstenite 生成非法请求行，
/// 服务端回 400（Node 的 URL.toString() 会自动补斜杠，Rust 的手拼字符串不会）。
pub fn event_ws_url(ws_url: &str, token: &str) -> String {
    // url crate 会把空 path 规范化为 "/"。
    let url = url::Url::parse(ws_url).expect("ws_url 已在上游校验");
    let sep = if url.query().is_some() { '&' } else { '?' };
    format!("{url}{sep}access_token={token}")
}

impl EventStream {
    pub async fn connect(ws_url: &str, token: &str) -> Result<Self, EventStreamError> {
        let url = event_ws_url(ws_url, token);
        let req = url
            .as_str()
            .into_client_request()
            .map_err(|e| EventStreamError(format!("bad ws url {url}: {e}")))?;
        let (ws, _resp) = connect_async(req)
            .await
            .map_err(|e| EventStreamError(format!("连接 {ws_url} 失败：{e}")))?;
        Ok(Self { ws })
    }

    /// 收一条事件：文本帧解析为强类型 [`OneBotEvent`]；坏帧跳过继续收；
    /// 连接关闭返回 `None`。ping 由 tungstenite 层自动回应。
    pub async fn recv_event(&mut self) -> Option<Result<OneBotEvent, EventStreamError>> {
        loop {
            match self.ws.next().await {
                None => return None,
                Some(Ok(Message::Text(t))) => {
                    match parse_event(&serde_json::from_str(t.as_str()).unwrap_or_default()) {
                        // 坏 JSON 整帧跳过（单条坏帧不断连接）。
                        Ok(evt) => return Some(Ok(evt)),
                        Err(_) => continue,
                    }
                }
                Some(Ok(Message::Close(_))) => return None,
                // ping/pong/binary：OneBot 事件流是文本协议，跳过。
                Some(Ok(_)) => continue,
                Some(Err(e)) => {
                    return Some(Err(EventStreamError(format!("ws 帧错误：{e}"))));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// token 进查询串；空 path 规范化为 `/`（tungstenite 的 400 教训）；
    /// 已有查询参数时用 & 连接。
    #[test]
    fn ws_url_appends_token() {
        assert_eq!(
            event_ws_url("ws://127.0.0.1:3001", "tok"),
            "ws://127.0.0.1:3001/?access_token=tok"
        );
        assert_eq!(
            event_ws_url("ws://127.0.0.1:3001/x?a=1", "tok"),
            "ws://127.0.0.1:3001/x?a=1&access_token=tok"
        );
    }
}
