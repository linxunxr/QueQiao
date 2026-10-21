//! 斜杠 RPC 信封（移植自 Node 版 `src/dsh-client.js` 信封段，契约同步迁移）。
//!
//! 兼容 DSH 0.1.2-alpha.1 引入、并在 0.1.5-rc.1 上复核通过的协议：
//! 1. RPC 方法从点号改为斜杠（host.describe -> host/describe 等）；
//! 2. payload 包装为 `{ args: { <参数名>: 原payload } }`（session/list 用 `_request`，
//!    其余多为 request；无参方法不包装）；
//! 3. 新版 SessionPromptRequest 强制要求 requestId（`wrap_args` 自动补齐）。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

/// 点号方法名 -> 斜杠 endpoint。
pub fn endpoint_of(method: &str) -> String {
    method.replace('.', "/")
}

/// 各方法的 args 包装形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgWrapper {
    /// 表外方法：payload 原样作为 args。
    None,
    /// 显式无参（wrapper 为 null）：`{ args: {} }`。
    Empty,
    /// 按参数名包装：`{ args: { <name>: payload } }`。
    Named(&'static str),
}

fn arg_wrapper(endpoint: &str) -> ArgWrapper {
    match endpoint {
        "session/list" => ArgWrapper::Named("_request"),
        "session/create"
        | "session/prompt"
        | "session/cancel"
        | "session/selectModel"
        | "session/rename"
        | "session/fork"
        | "session/updateQueue"
        | "session/page"
        | "session/search"
        | "session/follow"
        | "session/projections" => ArgWrapper::Named("request"),
        "workspace/create"
        | "workspace/rename"
        | "workspace/delete"
        | "workspace/archiveSession"
        | "workspace/insertBefore"
        | "workspace/insertSessionBefore" => ArgWrapper::Named("request"),
        "agentPresets/list" | "settings/describe" => ArgWrapper::Empty,
        _ => ArgWrapper::None,
    }
}

/// 把旧 payload 包装成新协议要求的 `{ args }`，并补新版必填字段
/// （session/prompt 的 requestId：非字符串或缺失时自动补 UUID）。
pub fn wrap_args(endpoint: &str, payload: Option<Value>) -> Value {
    let mut body = payload.unwrap_or_else(|| json!({}));
    if endpoint == "session/prompt" && body.get("requestId").and_then(Value::as_str).is_none() {
        body["requestId"] = json!(Uuid::new_v4().to_string());
    }
    match arg_wrapper(endpoint) {
        ArgWrapper::None => json!({ "args": body }),
        ArgWrapper::Empty => json!({ "args": {} }),
        ArgWrapper::Named(name) => json!({ "args": { name: body } }),
    }
}

/// 出站请求信封（`type: client-request`）。
#[derive(Debug, Clone, Serialize)]
pub struct RpcRequest {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(rename = "rpcId")]
    pub rpc_id: String,
    pub method: String,
    pub payload: Value,
}

impl RpcRequest {
    /// 构造一条请求：点号方法名转斜杠、payload 按方法包装、rpcId 现生成。
    pub fn new(method: &str, payload: Option<Value>) -> Self {
        let payload = wrap_args(&endpoint_of(method), payload);
        Self {
            kind: "client-request",
            rpc_id: Uuid::new_v4().to_string(),
            method: endpoint_of(method),
            payload,
        }
    }
}

/// 服务端业务错误槽。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

/// 响应结果槽：ok 时带 value，否则带 error。
#[derive(Debug, Clone, Deserialize)]
pub struct RpcResult {
    pub ok: bool,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

/// 入站响应信封。
#[derive(Debug, Clone, Deserialize)]
pub struct RpcResponse {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "rpcId")]
    pub rpc_id: String,
    pub result: RpcResult,
}

/// 业务失败（`unwrap` 的错误形态，文案与 Node 版一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcFailure {
    pub label: String,
    pub error: RpcError,
}

impl std::fmt::Display for RpcFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} failed: {}: {}",
            self.label, self.error.code, self.error.message
        )
    }
}

impl std::error::Error for RpcFailure {}

/// 把 RpcResponse 的结果槽解出来；业务错误直接以 [`RpcFailure`] 返回。
pub fn unwrap(response: &RpcResponse, label: &str) -> Result<Value, RpcFailure> {
    if response.result.ok {
        Ok(response.result.value.clone().unwrap_or(Value::Null))
    } else {
        Err(RpcFailure {
            label: label.to_owned(),
            error: response.result.error.clone().unwrap_or(RpcError {
                code: "unknown".to_owned(),
                message: String::new(),
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约与 Node 版 test-audit-protocol 一致：session/cancel 走 request 包装，
    /// 信封形状 {type, rpcId, method, payload:{args:{request:…}}}。
    #[test]
    fn session_cancel_uses_request_wrapper() {
        let req = RpcRequest::new("session.cancel", Some(json!({ "sessionId": "session-a" })));
        assert_eq!(req.method, "session/cancel");
        assert_eq!(req.kind, "client-request");
        assert_eq!(
            req.payload,
            json!({ "args": { "request": { "sessionId": "session-a" } } })
        );
        // rpcId 存在且非空。
        assert!(!req.rpc_id.is_empty());
    }

    /// session/prompt 自动补 requestId（缺失或非字符串都补）。
    #[test]
    fn session_prompt_gets_request_id() {
        let req = RpcRequest::new("session/prompt", Some(json!({ "text": "hi" })));
        let body = &req.payload["args"]["request"];
        assert!(body["requestId"].as_str().is_some_and(|s| !s.is_empty()));
        // 已有合法 requestId 时不覆盖。
        let kept = RpcRequest::new(
            "session/prompt",
            Some(json!({ "requestId": "keep-me", "text": "hi" })),
        );
        assert_eq!(
            kept.payload["args"]["request"]["requestId"],
            json!("keep-me")
        );
        // 非字符串 requestId 也补。
        let bad = RpcRequest::new("session/prompt", Some(json!({ "requestId": 42 })));
        assert!(
            bad.payload["args"]["request"]["requestId"]
                .as_str()
                .is_some()
        );
    }

    /// session/list 用 `_request` 包装；无参方法（agentPresets/list）args 为空对象。
    #[test]
    fn wrapper_variants() {
        let list = wrap_args("session/list", Some(json!({ "limit": 10 })));
        assert_eq!(list, json!({ "args": { "_request": { "limit": 10 } } }));
        let presets = wrap_args("agentPresets/list", None);
        assert_eq!(presets, json!({ "args": {} }));
        // 表外方法：payload 原样作 args；None 时为空对象。
        let other = wrap_args("host/describe", Some(json!({ "x": 1 })));
        assert_eq!(other, json!({ "args": { "x": 1 } }));
        assert_eq!(wrap_args("host/describe", None), json!({ "args": {} }));
    }

    /// unwrap：ok 取 value；业务错误带 label/code/message 文案。
    #[test]
    fn unwraps_result_slot() {
        let ok: RpcResponse = serde_json::from_value(json!({
            "type": "server-response", "rpcId": "r1",
            "result": { "ok": true, "value": { "accepted": true } }
        }))
        .unwrap();
        assert_eq!(
            unwrap(&ok, "session/cancel").unwrap(),
            json!({ "accepted": true })
        );

        let err: RpcResponse = serde_json::from_value(json!({
            "type": "server-response", "rpcId": "r2",
            "result": { "ok": false, "error": { "code": "session/not-found", "message": "gone" } }
        }))
        .unwrap();
        let failure = unwrap(&err, "session/prompt").unwrap_err();
        assert_eq!(failure.error.code, "session/not-found");
        assert_eq!(
            failure.to_string(),
            "session/prompt failed: session/not-found: gone"
        );
    }
}
