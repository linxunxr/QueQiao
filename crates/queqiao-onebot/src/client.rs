//! OneBot HTTP API 客户端（feature `io`）。
//!
//! 协议：`POST {base}/{action}`，`Authorization: Bearer <token>`，响应
//! `{status, retcode, data, wording?}`；`retcode == 1401` 表示 token 失效
//!（换账号后的典型症状——调用方应触发 [`crate::conn`] 的重新发现）。

use serde::Deserialize;
use serde_json::{Value, json};

/// OneBot 响应槽。
#[derive(Debug, Clone, Deserialize)]
pub struct OneBotResponse {
    pub status: String,
    pub retcode: i32,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub wording: Option<String>,
}

impl OneBotResponse {
    pub fn is_ok(&self) -> bool {
        self.status == "ok" && self.retcode == 0
    }
}

/// HTTP 调用错误。
#[derive(Debug, Clone, PartialEq)]
pub enum OneBotError {
    Transport(String),
    /// 业务失败（retcode 非 0）：`(action, retcode, wording)`。
    Api {
        action: String,
        retcode: i32,
        wording: String,
    },
    BadJson(String),
}

impl std::fmt::Display for OneBotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OneBotError::Transport(e) => write!(f, "{e}"),
            OneBotError::Api {
                action,
                retcode,
                wording,
            } => {
                write!(f, "{action} failed: retcode {retcode} {wording}")
            }
            OneBotError::BadJson(e) => write!(f, "invalid OneBot response: {e}"),
        }
    }
}

impl std::error::Error for OneBotError {}

/// token 失效的 retcode（换账号后旧 token 的典型症状）。
pub const RETCODE_UNAUTHORIZED: i32 = 1401;

/// OneBot HTTP 客户端（reqwest blocking；与 dsh 客户端同约束——不能在 tokio
/// 上下文里 drop，bin 编排时放同步线程或 spawn_blocking）。
#[derive(Debug, Clone)]
pub struct OneBotHttpClient {
    http: reqwest::blocking::Client,
    base_url: String,
    token: String,
}

impl OneBotHttpClient {
    pub fn new(base_url: &str, token: &str, timeout_ms: u64) -> Self {
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_millis(timeout_ms))
            .build()
            .expect("reqwest client 构造必然成功");
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        }
    }

    /// 调一个 action（POST JSON）；业务失败转 [`OneBotError::Api`]。
    pub fn call(&self, action: &str, params: Value) -> Result<Value, OneBotError> {
        let resp = self
            .http
            .post(format!("{}/{}", self.base_url, action))
            .header("authorization", format!("Bearer {}", self.token))
            .json(&params)
            .send()
            .map_err(|e| OneBotError::Transport(e.to_string()))?;
        let body: OneBotResponse = resp
            .json()
            .map_err(|e| OneBotError::BadJson(e.to_string()))?;
        if body.is_ok() {
            Ok(body.data)
        } else {
            Err(OneBotError::Api {
                action: action.to_owned(),
                retcode: body.retcode,
                wording: body.wording.unwrap_or_default(),
            })
        }
    }

    pub fn get_login_info(&self) -> Result<Value, OneBotError> {
        self.call("get_login_info", json!({}))
    }

    pub fn get_status(&self) -> Result<Value, OneBotError> {
        self.call("get_status", json!({}))
    }

    /// 发群消息（段数组形态）。
    pub fn send_group_msg(&self, group_id: u64, message: &Value) -> Result<i64, OneBotError> {
        let data = self.call(
            "send_group_msg",
            json!({ "group_id": group_id, "message": message }),
        )?;
        Ok(data.get("message_id").and_then(Value::as_i64).unwrap_or(0))
    }

    /// 发私聊消息。
    pub fn send_private_msg(&self, user_id: u64, message: &Value) -> Result<i64, OneBotError> {
        let data = self.call(
            "send_private_msg",
            json!({ "user_id": user_id, "message": message }),
        )?;
        Ok(data.get("message_id").and_then(Value::as_i64).unwrap_or(0))
    }

    /// 取图（`file` 必须先过 [`crate::file_guard::is_safe_cache_file_ref`] 白名单）。
    pub fn get_image(&self, file: &str) -> Result<Value, OneBotError> {
        debug_assert!(crate::file_guard::is_safe_cache_file_ref(file));
        self.call("get_image", json!({ "file": file }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 响应槽语义：ok/retcode/wording 解析与 is_ok 判定。
    #[test]
    fn response_slot_semantics() {
        let ok: OneBotResponse =
            serde_json::from_str(r#"{"status":"ok","retcode":0,"data":{"user_id":42}}"#).unwrap();
        assert!(ok.is_ok());
        assert_eq!(ok.data["user_id"], 42);
        let denied: OneBotResponse = serde_json::from_str(
            r#"{"status":"failed","retcode":1401,"wording":"unauthorized","data":null}"#,
        )
        .unwrap();
        assert!(!denied.is_ok());
        assert_eq!(denied.retcode, RETCODE_UNAUTHORIZED);
        assert_eq!(denied.wording.as_deref(), Some("unauthorized"));
    }
}
