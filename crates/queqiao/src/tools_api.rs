//! MCP 工具 API 宿主（规格 §3.6 桥侧端点——`/api/socialV2/*`）。
//!
//! 本批实现端到端必需的核心端点子集与校验链的前三层：
//! ①模式门禁（会话白名单）②agent token ③工具开关；频率/静默/业务校验
//! 与其余 30+ 端点在后续批次逐域补全。agentToken 从请求头 `x-agent-token`
//! 或查询参数取（MCP server 转发时带头）。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use queqiao_onebot::client::OneBotHttpClient;
use serde_json::{Value, json};

use crate::config::Config;
use crate::social_store::SocialStore;

pub struct ToolApiState {
    pub social: Arc<Mutex<SocialStore>>,
    pub onebot: OneBotHttpClient,
    pub cfg: Config,
}

/// 校验失败响应（状态码 + 消息）。
fn denied(code: StatusCode, msg: &str) -> (StatusCode, Json<Value>) {
    (code, Json(json!({ "ok": false, "error": msg })))
}

impl ToolApiState {
    /// 六层校验链前三层：会话键合法 → 白名单（模式门禁）→ agent token 匹配
    /// → 工具开关。通过返回会话键。
    fn authorize(
        &self,
        headers: &HeaderMap,
        query_key: &str,
        tool_flag: bool,
    ) -> Result<String, (StatusCode, Json<Value>)> {
        let key = query_key.trim().to_owned();
        if !key.starts_with("group:") && !key.starts_with("private:") {
            return Err(denied(
                StatusCode::BAD_REQUEST,
                "conversationKey 应为 group:群号 或 private:QQ号",
            ));
        }
        // ① 模式门禁：白名单（M4 范围声明 reserved2 准入 = allow 列表）。
        let id: u64 = key
            .split(':')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let allowed = match key.split(':').next().unwrap_or_default() {
            "group" => self.cfg.allow.groups.contains(&id),
            _ => self.cfg.allow.private.contains(&id),
        };
        if !allowed {
            return Err(denied(
                StatusCode::FORBIDDEN,
                &format!("会话 {key} 不在白名单内（config.json allow.groups / allow.private）"),
            ));
        }
        // ② agent token：x-agent-token 头或 token 查询参数，须与持久态一致。
        let token = headers
            .get("x-agent-token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let store = self.social.lock().unwrap();
        let expected = store
            .conversation(&key)
            .and_then(|st| st.agent_token.clone())
            .unwrap_or_default();
        drop(store);
        let Some(token) = token else {
            return Err(denied(
                StatusCode::UNAUTHORIZED,
                "缺少 x-agent-token（会话令牌随唤醒 prompt 注入）",
            ));
        };
        if token.is_empty() || token != expected {
            return Err(denied(
                StatusCode::UNAUTHORIZED,
                "agent token 无效（会话可能已重置——按最新唤醒 prompt 中的令牌）",
            ));
        }
        // ③ 工具开关（socialV2.tools.*——MCP 侧注册期过滤之外的第二道）。
        if !tool_flag {
            return Err(denied(
                StatusCode::FORBIDDEN,
                "该工具已在 config.json socialV2.tools 中关闭",
            ));
        }
        Ok(key)
    }
}

type ApiResult = (StatusCode, Json<Value>);

fn ok_json(value: Value) -> ApiResult {
    (StatusCode::OK, Json(json!({ "ok": true, "value": value })))
}

/// 启动工具 API（控制台端口共用；本批仅工具端点）。
pub async fn serve(
    state: Arc<ToolApiState>,
    port: u16,
    console_token: &str,
    console: Option<Arc<crate::console::ConsoleState>>,
) -> Result<(), String> {
    let ct = console_token.to_owned();
    let app = Router::new()
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/socialV2/status", get(status))
        .route("/api/socialV2/prompt", get(prompt))
        .route("/api/socialV2/unread", get(unread))
        .route("/api/socialV2/send-message", post(send_message))
        .route("/api/socialV2/mark-read", post(mark_read))
        .route("/api/socialV2/set-wake-config", post(set_wake_config))
        .layer(axum::middleware::from_fn(
            move |req: axum::http::Request<axum::body::Body>, next: axum::middleware::Next| {
                let ct = ct.clone();
                async move {
                    // 控制台令牌：非空时所有 /api/socialV2/* 需带（控制台 token 与
                    // agent token 权限隔离——本批仅校验存在性，细粒度后续批次）。
                    if req.uri().path().starts_with("/api/socialV2") && !ct.is_empty() {
                        let auth = req
                            .headers()
                            .get("x-console-token")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default();
                        if auth != ct {
                            let mut resp = axum::response::Response::new(axum::body::Body::from(
                                "console token required",
                            ));
                            *resp.status_mut() = StatusCode::UNAUTHORIZED;
                            return Ok(resp);
                        }
                    }
                    Ok::<_, std::convert::Infallible>(next.run(req).await)
                }
            },
        ))
        .with_state(state);
    let app = match console {
        Some(c) => app.merge(crate::console::router(c)),
        None => app,
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("绑定 127.0.0.1:{port} 失败：{e}"))?;
    println!("工具 API 已监听 127.0.0.1:{port}（/api/socialV2/* + 控制台 /）");
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}

async fn status(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> ApiResult {
    let key = match state.authorize(
        &headers,
        q.get("conversationKey").map(String::as_str).unwrap_or(""),
        true,
    ) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let store = state.social.lock().unwrap();
    let st = store.conversation(&key);
    ok_json(json!({
        "conversationKey": key,
        "unread": st.map(|s| s.unread.len()).unwrap_or(0),
        "recentCount": st.map(|s| s.recent_messages.len()).unwrap_or(0),
        "lastUnreadSeq": st.map(|s| s.last_unread_seq).unwrap_or(0),
        "lastReadThroughSeq": st.map(|s| s.last_read_through_seq).unwrap_or(0),
    }))
}

/// qq_get_prompt：角色卡与状态总览（agent 引导第一步）。
async fn prompt(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> ApiResult {
    let key = match state.authorize(
        &headers,
        q.get("conversationKey").map(String::as_str).unwrap_or(""),
        state.cfg.social_v2.tools.get_prompt,
    ) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let store = state.social.lock().unwrap();
    let st = store.conversation(&key);
    ok_json(json!({
        "conversationKey": key,
        "role": { "name": "", "note": "角色卡加载在控制台批次接入" },
        "unread": st.map(|s| s.unread.len()).unwrap_or(0),
        "wakeConfig": st.map(|s| s.wake_config.clone()).unwrap_or_default(),
        "recommendations": state.cfg.social_v2.provide_recommendations,
    }))
}

/// qq_get_unread_messages：未读列表（支持 afterSeq 游标）。
async fn unread(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> ApiResult {
    let key = match state.authorize(
        &headers,
        q.get("conversationKey").map(String::as_str).unwrap_or(""),
        state.cfg.social_v2.tools.get_unread,
    ) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let after_seq: u64 = q.get("afterSeq").and_then(|s| s.parse().ok()).unwrap_or(0);
    let store = state.social.lock().unwrap();
    let st = store.conversation(&key);
    let messages: Vec<Value> = st
        .map(|s| {
            s.unread
                .iter()
                .filter(|m| m.seq.is_some_and(|seq| seq > after_seq))
                .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
                .collect()
        })
        .unwrap_or_default();
    ok_json(json!({ "messages": messages, "count": messages.len() }))
}

/// qq_send_message / qq_send_group_message / qq_send_private_message：
/// 发消息回 QQ（端到端闭环的关键端点）。
async fn send_message(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let flag = state.cfg.social_v2.tools.send_message
        || state.cfg.social_v2.tools.send_group
        || state.cfg.social_v2.tools.send_private;
    let key = match state.authorize(&headers, key, flag) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let text = body
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if text.trim().is_empty() {
        return denied(StatusCode::BAD_REQUEST, "text 不能为空");
    }
    // 发送长度上限（防 AI 出 bug 刷长文）。
    let max = state.cfg.social_v2.send.max_message_chars;
    let text: String = if text.chars().count() > max {
        text.chars().take(max).collect()
    } else {
        text
    };
    let mut segments = vec![queqiao_onebot::segment::text(&text)];
    if let Some(reply_to) = body.get("replyToMessageId").and_then(Value::as_i64) {
        segments.insert(0, queqiao_onebot::segment::reply(reply_to));
    }
    let message = queqiao_onebot::segment::message(&segments);
    let result = if let Some(group) = key.strip_prefix("group:") {
        state
            .onebot
            .send_group_msg(group.parse().unwrap_or(0), &message)
    } else {
        let user = key.strip_prefix("private:").unwrap_or("0");
        state
            .onebot
            .send_private_msg(user.parse().unwrap_or(0), &message)
    };
    match result {
        Ok(message_id) => {
            // 记账：lastActionAt 推进（noAction 判定的动作信号）。
            {
                let mut store = state.social.lock().unwrap();
                let now = crate::session_manager::SessionManager::now_ms();
                let st = store.conversation_mut(&key);
                st.last_action_at = now;
                st.last_ai_reply_at = now;
                store.save();
            }
            ok_json(json!({ "messageId": message_id }))
        }
        Err(e) => denied(StatusCode::BAD_GATEWAY, &format!("OneBot 发送失败：{e}")),
    }
}

/// qq_mark_read：确认读到 throughSeq（水位推进——新消息留待后续处理）。
async fn mark_read(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key = match state.authorize(&headers, key, state.cfg.social_v2.tools.mark_read) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let through: u64 = body.get("throughSeq").and_then(Value::as_u64).unwrap_or(0);
    {
        let mut store = state.social.lock().unwrap();
        let st = store.conversation_mut(&key);
        // 水位只能前进；through 超过 lastUnreadSeq 按 lastUnreadSeq 收（防跳过未读）。
        let capped = through.min(st.last_unread_seq);
        if capped >= st.last_read_through_seq {
            st.last_read_through_seq = capped;
        }
        // 已确认的从未读缓冲移除（仍在 recent 里供上下文查看）。
        st.unread.retain(|m| m.seq.is_none_or(|s| s > capped));
        st.last_action_at = crate::session_manager::SessionManager::now_ms();
        store.save();
    }
    ok_json(json!({ "readThroughSeq": through }))
}

/// qq_set_wake_config：AI 自主设置唤醒条件（infinite 潜水 / 有限 sleepUntil /
/// 触发器开关与关键词）。
async fn set_wake_config(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key = match state.authorize(&headers, key, state.cfg.social_v2.tools.set_wake_config) {
        Ok(k) => k,
        Err(e) => return e,
    };
    {
        let mut store = state.social.lock().unwrap();
        let st = store.conversation_mut(&key);
        if let Some(mode) = body.get("mode").and_then(Value::as_str) {
            st.wake_config.mode = mode.to_owned();
        }
        if let Some(infinite) = body.get("infinite").and_then(Value::as_bool) {
            st.wake_config.infinite = infinite;
        }
        if let Some(until) = body.get("sleepUntil").and_then(Value::as_i64) {
            st.wake_config.sleep_until = Some(until.max(0) as u64);
        } else if body.get("sleepUntil").map(Value::is_null).unwrap_or(false) {
            st.wake_config.sleep_until = None;
        }
        if let Some(tr) = body.get("triggers").and_then(Value::as_object) {
            let t = &mut st.wake_config.triggers;
            for flag in ["atMention", "nameMention", "question", "poke", "anyMessage"] {
                if let Some(v) = tr.get(flag).and_then(Value::as_bool) {
                    match flag {
                        "atMention" => t.at_mention = v,
                        "nameMention" => t.name_mention = v,
                        "question" => t.question = v,
                        "poke" => t.poke = v,
                        _ => t.any_message = v,
                    }
                }
            }
            if let Some(kws) = tr.get("keywords").and_then(Value::as_array) {
                t.keywords = kws
                    .iter()
                    .filter_map(|k| k.as_str().map(str::to_owned))
                    .filter(|k| !k.is_empty())
                    .collect();
            }
        }
        st.wake_config.confirmed_at = Some(crate::session_manager::SessionManager::now_ms());
        st.wake_config.confirmed_by = Some("set_wake_config".to_owned());
        st.last_action_at = crate::session_manager::SessionManager::now_ms();
        store.save();
    }
    ok_json(json!({ "updated": true }))
}
