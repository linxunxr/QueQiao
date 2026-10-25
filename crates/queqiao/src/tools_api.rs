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
#[cfg(windows)]
use std::os::windows::process::CommandExt;

use crate::config::Config;
use crate::social_store::SocialStore;
use crate::update::{self, ReleaseInfo};

pub struct ToolApiState {
    pub social: Arc<Mutex<SocialStore>>,
    pub onebot: OneBotHttpClient,
    pub cfg: Config,
    /// 模型目录查询与控制台重置用（bin 注入）。
    pub dsh: Arc<Mutex<queqiao_dsh::client::DshClient<queqiao_dsh::client::ReqwestTransport>>>,
    pub sessions: Arc<Mutex<crate::session_manager::SessionManager>>,
    /// 检测到新版本的回投通道（与启动自动检测共用）：主循环收下后
    /// 打印更新内容 → 下载 staging → 全体空闲时应用并重启。
    pub update_tx: tokio::sync::mpsc::Sender<ReleaseInfo>,
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

async fn cors_mw(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, std::convert::Infallible> {
    use axum::http::Method;
    if req.method() == Method::OPTIONS {
        let mut resp = axum::response::Response::new(axum::body::Body::empty());
        *resp.status_mut() = StatusCode::NO_CONTENT;
        cors_headers(resp.headers_mut());
        return Ok(resp);
    }
    let mut resp = next.run(req).await;
    cors_headers(resp.headers_mut());
    Ok(resp)
}

fn cors_headers(headers: &mut axum::http::HeaderMap) {
    use axum::http::header::*;
    let _ = headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    let _ = headers.insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        "GET, POST, PUT, DELETE, OPTIONS".parse().unwrap(),
    );
    let _ = headers.insert(
        ACCESS_CONTROL_ALLOW_HEADERS,
        "content-type, x-agent-token, x-console-token"
            .parse()
            .unwrap(),
    );
}

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
        .route("/api/socialV2/reply", post(reply_message))
        .route("/api/socialV2/send-poke", post(send_poke))
        .route("/api/socialV2/react-message", post(react_message))
        .route("/api/socialV2/recall-message", post(recall_message))
        .route("/api/images/message", get(message_images))
        .route("/api/socialV2/forward-msg", get(forward_msg))
        .route("/api/socialV2/collect-sticker", post(collect_sticker))
        .route("/api/socialV2/fetch-sticker", post(fetch_sticker))
        .route("/api/socialV2/set-sticker-remark", post(set_sticker_remark))
        .route("/api/socialV2/write-diary", post(write_diary))
        .route("/api/socialV2/get-diary", get(get_diary))
        .route("/api/socialV2/group-members", get(group_members))
        .route("/api/socialV2/group-profile", get(group_profile))
        .route("/api/socialV2/group-history", get(group_history))
        .route("/api/socialV2/self-image", get(self_image))
        .route("/api/socialV2/wait-messages", get(wait_messages))
        .route("/api/socialV2/social-state", get(social_state))
        .route("/api/socialV2/my-recent", get(my_recent))
        .route("/api/socialV2/list-groups", get(list_groups))
        .route("/api/socialV2/report-feedback", post(report_feedback))
        .route("/api/socialV2/speak-voice", post(speak_voice))
        .route("/api/socialV2/send-voice", post(send_voice))
        .route("/api/socialV2/list-voices", get(list_voices))
        .route("/api/socialV2/mark-read", post(mark_read))
        .route("/api/socialV2/set-wake-config", post(set_wake_config))
        .route("/api/diagnostics", get(diagnostics))
        .route("/api/update/check", post(update_check))
        .route("/api/tokens/summary", get(tokens_summary))
        .route("/api/socialV2/slang/query", get(slang_query))
        .route("/api/socialV2/slang/submit", post(slang_submit))
        .route("/api/dsh/catalog", get(dsh_catalog))
        .route("/api/session/reset", post(session_reset))
        .route("/api/roles", get(roles_list))
        .route("/api/role", post(role_switch))
        .route("/api/socialV2/sticker-sync", post(sticker_sync))
        .route("/api/tokens/series", get(tokens_series))
        .route("/api/tokens/turns", get(tokens_turns))
        .route("/api/socialV2/active-members", get(active_members))
        .route("/api/socialV2/tool-log", get(tool_log))
        .route("/api/socialV2/tool-log/clear", post(tool_log_clear))
        .route("/api/socialV2/memory", get(memory_list))
        .route("/api/socialV2/memory-append", post(memory_append))
        .route("/api/socialV2/memory-remove", post(memory_remove))
        .route("/api/socialV2/memory-clear", post(memory_clear))
        .route("/api/socialV2/recent-messages", get(recent_messages))
        .route("/api/socialV2/my-recent-messages", get(recent_messages))
        .route("/api/socialV2/message-detail", get(message_detail))
        .route("/api/socialV2/sticker-list", get(sticker_list))
        .route("/api/socialV2/sticker-image", get(sticker_image))
        .route("/api/socialV2/sticker-note", post(sticker_note))
        .route("/api/socialV2/send-sticker", post(send_sticker))
        // CORS：桌面客户端（Tauri webview）与浏览器页面跨域访问本地桥 API。
        // 本服务只绑 127.0.0.1，放开 origin 无外网暴露面。
        .layer(axum::middleware::from_fn(cors_mw))
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
    // 角色卡（Node 同源）：唤醒 prompt 只注角色名，全文卡由此端点给 AI——
    // V2 装配（模式筛小节 + 一代指令兜底过滤 + 截断）。
    let home = crate::paths::queqiao_home();
    let role_name = crate::role_card_store::current_role_name(&home, &key);
    let role_hint = crate::role_card_store::hint_v2(&home, &key, state.cfg.role.max_inject_chars);
    ok_json(json!({
        "conversationKey": key,
        "role": { "name": role_name, "hint": role_hint },
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
    let messages: Vec<Value> = store
        .conversation(&key)
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
    let text = body.get("text").and_then(Value::as_str).unwrap_or_default();
    let reply_to = body.get("replyToMessageId").and_then(Value::as_i64);
    match send_text_with_reply(&state, &key, text, reply_to) {
        Ok(message_id) => {
            mark_reply_action(&state, &key);
            ok_json(json!({ "messageId": message_id }))
        }
        Err(e) => e,
    }
}

/// 发送/互动成功后的记账（lastActionAt 推进——noAction 判定的动作信号；
/// 仅发言类同时推进 lastAiReplyAt）。
fn mark_reply_action(state: &ToolApiState, key: &str) {
    mark_action(state, key, true);
}
fn mark_action(state: &ToolApiState, key: &str, is_reply: bool) {
    let mut store = state.social.lock().unwrap();
    let now = crate::session_manager::SessionManager::now_ms();
    let st = store.conversation_mut(key);
    st.last_action_at = now;
    if is_reply {
        st.last_ai_reply_at = now;
    }
    store.save();
}

/// 发送/互动类动作的频率护栏（Node 版 sendTimes 记账语义）：窗口内计数
/// 超上限即拒（429），通过则记一笔。上限 0 = 不限。发送、引用、连发逐条、
/// 表情包、拍一拍、表情回应共用。
fn check_send_rate(state: &ToolApiState, key: &str) -> Result<(), (StatusCode, axum::Json<Value>)> {
    let (max_min, max_hour) = (
        state.cfg.social_v2.send.max_send_per_minute as usize,
        state.cfg.social_v2.send.max_send_per_hour as usize,
    );
    if max_min == 0 && max_hour == 0 {
        return Ok(());
    }
    let mut store = state.social.lock().unwrap();
    let now = crate::session_manager::SessionManager::now_ms();
    let st = store.conversation_mut(key);
    let recent_min = st.send_times.iter().filter(|t| now - **t < 60_000).count();
    let recent_hour = st
        .send_times
        .iter()
        .filter(|t| now - **t < 3_600_000)
        .count();
    if (max_min > 0 && recent_min + 1 > max_min) || (max_hour > 0 && recent_hour + 1 > max_hour) {
        return Err(denied(
            StatusCode::TOO_MANY_REQUESTS,
            "操作频率超限，请稍后再试",
        ));
    }
    st.send_times.push(now);
    if st.send_times.len() > 500 {
        let cut = st.send_times.len() - 500;
        st.send_times.drain(0..cut);
    }
    store.save();
    Ok(())
}

/// 发送链复用体（send-message 与 reply 端点共用）：频率护栏 → 空校验 → 长度上限 →
/// 引用段 + 文本段 → 按会话类型走群/私聊。
fn send_text_with_reply(
    state: &ToolApiState,
    key: &str,
    text: &str,
    reply_to: Option<i64>,
) -> Result<i64, (StatusCode, axum::Json<Value>)> {
    check_send_rate(state, key)?;
    if text.trim().is_empty() {
        return Err(denied(StatusCode::BAD_REQUEST, "text 不能为空"));
    }
    let max = state.cfg.social_v2.send.max_message_chars;
    let text: String = if text.chars().count() > max {
        text.chars().take(max).collect()
    } else {
        text.to_owned()
    };
    let mut segments = vec![queqiao_onebot::segment::text(&text)];
    if let Some(reply_to) = reply_to {
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
    let message_id =
        result.map_err(|e| denied(StatusCode::BAD_GATEWAY, &format!("OneBot 发送失败：{e}")))?;
    record_self_send(state, key, text.as_str(), message_id);
    Ok(message_id)
}

/// 发送成功后把"我"的消息记入 recent（is_self，不进未读/水位）——
/// qq_get_my_recent_messages 的数据源（Node record-sent 同款语义）。
fn record_self_send(state: &ToolApiState, key: &str, text: &str, message_id: i64) {
    let mut store = state.social.lock().unwrap();
    let now = crate::session_manager::SessionManager::now_ms();
    store.append_message(
        key,
        &queqiao_core::action::IncomingMessage {
            key: key.to_owned(),
            seq: 0,
            message_id: message_id.to_string(),
            sender: "我".into(),
            user_id: String::new(),
            text: text.to_owned(),
            is_self: true,
            at_me: false,
            reply_to_me: false,
            time_ms: now,
        },
        now,
    );
    store.save();
}

/// qq_reply：群引用回复——参数名与 send-message 不同（groupId/message），
/// 转换后同一发送链。
async fn reply_message(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let group = body
        .get("groupId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key = format!("group:{group}");
    let flag = state.cfg.social_v2.tools.send_message || state.cfg.social_v2.tools.send_group;
    let key = match state.authorize(&headers, &key, flag) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let reply_to = body.get("replyToMessageId").and_then(Value::as_i64);
    let text = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match send_text_with_reply(&state, &key, text, reply_to) {
        Ok(message_id) => {
            mark_reply_action(&state, &key);
            ok_json(json!({ "messageId": message_id }))
        }
        Err(e) => e,
    }
}

/// qq_send_poke：拍一拍（群需 targetUserId；私聊默认拍对方）。
async fn send_poke(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key = match state.authorize(&headers, key, state.cfg.social_v2.tools.send_poke) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let target = body
        .get("targetUserId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if let Err(e) = check_send_rate(&state, &key) {
        return e;
    }
    let result = if let Some(group) = key.strip_prefix("group:") {
        if target.is_empty() {
            return denied(StatusCode::BAD_REQUEST, "群聊拍一拍必须传 targetUserId");
        }
        state.onebot.call(
            "group_poke",
            json!({ "group_id": group.parse::<u64>().unwrap_or(0), "user_id": target.parse::<u64>().unwrap_or(0) }),
        )
    } else {
        let user = if target.is_empty() {
            key.strip_prefix("private:").unwrap_or("0")
        } else {
            &target
        };
        state.onebot.call(
            "friend_poke",
            json!({ "user_id": user.parse::<u64>().unwrap_or(0) }),
        )
    };
    match result {
        Ok(_) => {
            mark_action(&state, &key, false);
            ok_json(json!({ "ok": true }))
        }
        Err(e) => denied(StatusCode::BAD_GATEWAY, &format!("拍一拍失败：{e}")),
    }
}

/// qq_react_message：QQ 表情回应（set_msg_emoji_like）。只能给近期消息点，
/// 平台与 NapCat 侧兜底校验。
async fn react_message(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key = match state.authorize(&headers, key, state.cfg.social_v2.tools.react_message) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let message_id = body.get("messageId").and_then(Value::as_i64).unwrap_or(0);
    let emoji = body
        .get("emojiId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if message_id == 0 || emoji.is_empty() {
        return denied(StatusCode::BAD_REQUEST, "messageId 与 emojiId 不能为空");
    }
    if let Err(e) = check_send_rate(&state, &key) {
        return e;
    }
    match state.onebot.call(
        "set_msg_emoji_like",
        json!({ "message_id": message_id, "emoji_id": emoji }),
    ) {
        Ok(_) => {
            mark_action(&state, &key, false);
            ok_json(json!({ "ok": true }))
        }
        Err(e) => denied(StatusCode::BAD_GATEWAY, &format!("表情回应失败：{e}")),
    }
}

/// qq_recall_message：撤回自己刚发的消息（2 分钟窗口）。桥不存自发消息
/// 映射，"只能撤自己的"由平台侧兜底校验。
async fn recall_message(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key = match state.authorize(&headers, key, state.cfg.social_v2.tools.recall_message) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let message_id = body.get("messageId").and_then(Value::as_i64).unwrap_or(0);
    if message_id == 0 {
        return denied(StatusCode::BAD_REQUEST, "messageId 不能为空");
    }
    match state
        .onebot
        .call("delete_msg", json!({ "message_id": message_id }))
    {
        Ok(_) => {
            mark_action(&state, &key, false);
            ok_json(json!({ "ok": true }))
        }
        Err(e) => denied(StatusCode::BAD_GATEWAY, &format!("撤回失败：{e}")),
    }
}

/// qq_get_message_images：取消息内图片/表情（get_msg 拿原始段 → 图片
/// 下载转 base64；face 转文本；视频暂不支持抽帧）。图片下载在
/// spawn_blocking（reqwest blocking）。
async fn message_images(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let key = q
        .get("conversationKey")
        .map(String::as_str)
        .unwrap_or_default();
    // authorize 即鉴权目的本身（会话键/白名单/令牌三查）；返回键此处不再用。
    if let Err(e) = state.authorize(&headers, key, state.cfg.social_v2.tools.get_images) {
        return e;
    }
    let message_id = q
        .get("messageId")
        .and_then(|m| m.parse::<i64>().ok())
        .unwrap_or(0);
    if message_id == 0 {
        return denied(StatusCode::BAD_REQUEST, "messageId 不能为空");
    }
    let raw = match state
        .onebot
        .call("get_msg", json!({ "message_id": message_id }))
    {
        Ok(v) => v,
        Err(e) => return denied(StatusCode::BAD_GATEWAY, &format!("get_msg 失败：{e}")),
    };
    let segments = raw
        .get("message")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // 先收集待下载图（url），下载转 base64 后组装。
    let mut images = Vec::new();
    let mut note = String::new();
    let mut urls = Vec::new();
    for seg in &segments {
        match seg.get("type").and_then(Value::as_str) {
            Some("image") | Some("mface") => {
                let url = seg.get("url").and_then(Value::as_str).unwrap_or_default();
                let text = seg
                    .get("summary")
                    .or_else(|| seg.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if url.is_empty() {
                    images.push(json!({ "kind": "image", "text": text, "note": "无 url（可能是闪照/过期）" }));
                } else {
                    urls.push((url.to_owned(), text.to_owned()));
                }
            }
            Some("face") => {
                let id = seg.get("id").and_then(Value::as_i64).unwrap_or(0);
                images.push(json!({ "kind": "face", "text": format!("[QQ 表情 #{id}]") }));
            }
            Some("video") | Some("record") => {
                note.push_str("含视频/语音，暂不支持提取（如需可后续版本补抽帧）；");
            }
            _ => {}
        }
    }
    let downloaded = tokio::task::spawn_blocking(move || {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build();
        urls.iter()
            .filter_map(|(url, text)| {
                let client = client.as_ref().ok()?;
                let resp = client.get(url).send().ok()?;
                let bytes = resp.bytes().ok()?;
                let mime = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
                    "image/png"
                } else {
                    "image/jpeg"
                };
                use base64::Engine;
                Some(json!({
                    "kind": "image", "mimeType": mime, "text": text,
                    "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
                }))
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    images.extend(downloaded);
    if images.is_empty() {
        return ok_json(json!({ "images": [], "note": note + "未找到图片/表情" }));
    }
    ok_json(json!({ "images": images, "note": note }))
}

/// qq_get_forward_msg：合并转发内容展开为文本树（嵌套转发给出 resid 供再查）。
async fn forward_msg(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let key = q
        .get("conversationKey")
        .map(String::as_str)
        .unwrap_or_default();
    // 同上：此处仅需完成鉴权三查。
    if let Err(e) = state.authorize(&headers, key, state.cfg.social_v2.tools.get_message_detail) {
        return e;
    }
    let id = q.get("id").map(String::as_str).unwrap_or_default();
    if id.is_empty() {
        return denied(StatusCode::BAD_REQUEST, "id 不能为空");
    }
    let raw = match state.onebot.call("get_forward_msg", json!({ "id": id })) {
        Ok(v) => v,
        Err(e) => {
            return denied(
                StatusCode::BAD_GATEWAY,
                &format!("get_forward_msg 失败：{e}"),
            );
        }
    };
    let messages = raw.as_array().cloned().unwrap_or_default();
    let mut out = String::new();
    for m in &messages {
        let sender = m
            .get("sender")
            .and_then(|s| s.get("card").or_else(|| s.get("nickname")))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let mut parts = Vec::new();
        for seg in m
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match seg.get("type").and_then(Value::as_str) {
                Some("text") => parts.push(
                    seg.get("data")
                        .and_then(|d| d.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ),
                Some("image") => parts.push("[图片]".to_owned()),
                Some("face") => parts.push("[表情]".to_owned()),
                Some("forward") => {
                    let rid = seg
                        .get("data")
                        .and_then(|d| d.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    parts.push(format!("[嵌套转发 id={rid}，可再查]"));
                }
                Some(other) => parts.push(format!("[{other}]")),
                None => {}
            }
        }
        out.push_str(&format!("{sender}: {}\n", parts.join(" ")));
    }
    if out.is_empty() {
        return ok_json(json!({ "messages": [], "note": "转发内容为空或已过期" }));
    }
    ok_json(json!({ "messages": out.trim_end() }))
}

/// 消息段数组 → 纯文本（图片/表情/转发等以占位标注；与 forward/history 共用）。
fn segments_to_text(segments: &[Value]) -> String {
    let mut parts = Vec::new();
    for seg in segments {
        match seg.get("type").and_then(Value::as_str) {
            Some("text") => parts.push(
                seg.get("data")
                    .and_then(|d| d.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ),
            Some("image") => parts.push("[图片]".to_owned()),
            Some("face") => parts.push("[表情]".to_owned()),
            Some("at") => parts.push(format!(
                "@{}",
                seg.get("data")
                    .and_then(|d| d.get("qq"))
                    .map(|v| v.to_string())
                    .unwrap_or_default()
            )),
            Some("forward") => parts.push(format!(
                "[转发消息 id={}]",
                seg.get("data")
                    .and_then(|d| d.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            )),
            Some(other) => parts.push(format!("[{other}]")),
            None => {}
        }
    }
    parts.join(" ")
}

/// qq_collect_sticker：偷图收藏——取消息里的图片段（url/md5/summary）入库。
/// 引用式入库（不落地文件，发送/查看走 url——与库内既有形态一致）。
async fn collect_sticker(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // authorize 即鉴权目的本身；返回键此处不再用。
    if let Err(e) = state.authorize(&headers, key, state.cfg.social_v2.tools.collect_sticker) {
        return e;
    }
    let message_id = body.get("messageId").and_then(Value::as_i64).unwrap_or(0);
    let note = body.get("note").and_then(Value::as_str).unwrap_or_default();
    if message_id == 0 {
        return denied(StatusCode::BAD_REQUEST, "messageId 不能为空");
    }
    let raw = match state
        .onebot
        .call("get_msg", json!({ "message_id": message_id }))
    {
        Ok(v) => v,
        Err(e) => return denied(StatusCode::BAD_GATEWAY, &format!("get_msg 失败：{e}")),
    };
    let Some(img) = raw
        .get("message")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|s| s.get("type").and_then(Value::as_str) == Some("image"))
        })
    else {
        return denied(
            StatusCode::NOT_FOUND,
            "这条消息里没有图片（只能收藏带图消息）",
        );
    };
    let url = img.get("url").and_then(Value::as_str).unwrap_or_default();
    if url.is_empty() {
        return denied(StatusCode::CONFLICT, "图片无 URL（闪照/过期），无法收藏");
    }
    let entry = queqiao_core::sticker::StickerEntry {
        id: format!("c{}", crate::session_manager::SessionManager::now_ms()),
        res_id: String::new(),
        url: url.to_owned(),
        md5: img
            .get("md5")
            .map(|m| m.to_string().trim_matches('"').to_owned())
            .unwrap_or_default(),
        desc: note.to_owned(),
        local_note: note.to_owned(),
        source: "ai".into(),
        ..Default::default()
    };
    let home = crate::paths::queqiao_home();
    let file = crate::sticker_store::sticker_file(&home);
    let mut entries = match crate::sticker_store::load(&file) {
        Ok(v) => v,
        Err(e) => return denied(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let id = entry.id.clone();
    entries.push(entry);
    if let Err(e) = crate::sticker_store::save(&file, &entries) {
        return denied(StatusCode::INTERNAL_SERVER_ERROR, &e);
    }
    ok_json(json!({ "ok": true, "stickerId": id, "count": entries.len() }))
}

/// qq_fetch_sticker：从 URL 入库（下载验证为图片字节后登记引用；不调用
/// QQ 收藏 API——Node 版的 add_custom_face 降级为库内引用，发送形态一致）。
async fn fetch_sticker(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // authorize 即鉴权目的本身；返回键此处不再用。
    if let Err(e) = state.authorize(&headers, key, state.cfg.social_v2.tools.fetch_sticker) {
        return e;
    }
    let url = body.get("url").and_then(Value::as_str).unwrap_or_default();
    let desc = body.get("desc").and_then(Value::as_str).unwrap_or_default();
    if url.is_empty() || !url.starts_with("http") {
        return denied(StatusCode::BAD_REQUEST, "url 必须是 http(s) 图片直链");
    }
    // 下载验证（前 512 字节判图）。reqwest blocking 在 spawn_blocking。
    let probe_url = url.to_owned();
    let head = tokio::task::spawn_blocking(move || {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        let resp = client
            .get(&probe_url)
            .send()
            .map_err(|e| format!("下载失败：{e}"))?;
        let bytes = resp.bytes().map_err(|e| format!("下载失败：{e}"))?;
        let is_img = bytes.starts_with(&[0x89, b'P', b'N', b'G'])
            || bytes.starts_with(&[0xFF, 0xD8])
            || bytes.starts_with(b"GIF8")
            || bytes.starts_with(&[0x52, 0x49, 0x46, 0x46]);
        Ok::<bool, String>(is_img)
    })
    .await
    .unwrap_or(Ok(false));
    match head {
        Ok(true) => {}
        Ok(false) => {
            return denied(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "URL 不是图片（PNG/JPEG/GIF/WebP 之外的内容）",
            );
        }
        Err(e) => return denied(StatusCode::BAD_GATEWAY, &e),
    }
    let entry = queqiao_core::sticker::StickerEntry {
        id: format!("f{}", crate::session_manager::SessionManager::now_ms()),
        url: url.to_owned(),
        desc: desc.to_owned(),
        local_note: desc.to_owned(),
        source: "manual".into(),
        ..Default::default()
    };
    let home = crate::paths::queqiao_home();
    let file = crate::sticker_store::sticker_file(&home);
    let mut entries = match crate::sticker_store::load(&file) {
        Ok(v) => v,
        Err(e) => return denied(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let id = entry.id.clone();
    entries.push(entry);
    if let Err(e) = crate::sticker_store::save(&file, &entries) {
        return denied(StatusCode::INTERNAL_SERVER_ERROR, &e);
    }
    ok_json(json!({ "ok": true, "stickerId": id, "count": entries.len() }))
}

/// qq_set_sticker_remark：改库内备注（localNote；QQ 官方收藏备注的
/// modify_custom_face 在 Node 版即默认禁用，此处仅本地）。
async fn set_sticker_remark(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // authorize 即鉴权目的本身；返回键此处不再用。
    if let Err(e) = state.authorize(&headers, key, state.cfg.social_v2.tools.set_sticker_remark) {
        return e;
    }
    let id = body
        .get("stickerId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let note = body
        .get("localNote")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let home = crate::paths::queqiao_home();
    let file = crate::sticker_store::sticker_file(&home);
    let mut entries = match crate::sticker_store::load(&file) {
        Ok(v) => v,
        Err(e) => return denied(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let Some(entry) = queqiao_core::sticker::find_sticker_mut(&mut entries, id) else {
        return denied(
            StatusCode::NOT_FOUND,
            "表情未找到（先 qq_list_stickers 查）",
        );
    };
    entry.local_note = note.to_owned();
    if let Err(e) = crate::sticker_store::save(&file, &entries) {
        return denied(StatusCode::INTERNAL_SERVER_ERROR, &e);
    }
    ok_json(json!({ "ok": true, "stickerId": id }))
}

/// qq_write_diary：提交今日日记（每天一次；格式与 Node 版 diary.jsonl 一致：
/// {date, content, key} 逐行）。
async fn write_diary(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key = match state.authorize(&headers, key, state.cfg.social_v2.tools.write_diary) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let content = body
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let chars = content.chars().count();
    if chars < 10 {
        return denied(StatusCode::BAD_REQUEST, "日记太短（至少 10 字）");
    }
    let today = local_date_today();
    let file = crate::paths::queqiao_home()
        .join("state")
        .join("diary.jsonl");
    if let Ok(text) = std::fs::read_to_string(&file) {
        let today_line = format!("\"date\":\"{today}\"");
        if text.contains(&today_line) {
            return denied(
                StatusCode::CONFLICT,
                &format!("今天（{today}）的日记已提交过，每天只能写一篇"),
            );
        }
    }
    let line = json!({ "date": today, "content": content, "key": key });
    use std::io::Write;
    let mut f = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
    {
        Ok(f) => f,
        Err(e) => {
            return denied(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("打开日记失败：{e}"),
            );
        }
    };
    if let Err(e) = writeln!(f, "{line}") {
        return denied(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("写日记失败：{e}"),
        );
    }
    ok_json(json!({ "ok": true, "date": today, "chars": chars }))
}

/// 本地日期（YYYY-MM-DD；桥本地时区——与唤醒/日记窗口判定同基准）。
fn local_date_today() -> String {
    let secs = crate::session_manager::SessionManager::now_ms() / 1000;
    let local = secs + 8 * 3600; // Asia/Shanghai 固定偏移（now_local 同款约定）
    let days = local / 86400;
    // 1970-01-01 起的 civil 日期换算（Howard Hinnant 算法简版）。
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// qq_get_diary：读最近 N 天日记（默认 7）。
async fn get_diary(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let key = q
        .get("conversationKey")
        .map(String::as_str)
        .unwrap_or_default();
    // authorize 即鉴权目的本身；返回键此处不再用。
    if let Err(e) = state.authorize(&headers, key, state.cfg.social_v2.tools.get_diary) {
        return e;
    }
    let days: usize = q
        .get("days")
        .and_then(|d| d.parse().ok())
        .unwrap_or(7)
        .min(90);
    let file = crate::paths::queqiao_home()
        .join("state")
        .join("diary.jsonl");
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let mut out = Vec::new();
    for line in text.lines().rev() {
        if out.len() >= days {
            break;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line.trim_start_matches('\u{feff}')) {
            out.push(json!({
                "date": v.get("date").cloned().unwrap_or(Value::Null),
                "content": v.get("content").cloned().unwrap_or(Value::Null),
            }));
        }
    }
    ok_json(json!({ "diaries": out, "count": out.len() }))
}

/// qq_get_group_members：群成员列表（精简字段）。
async fn group_members(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let key = q
        .get("conversationKey")
        .map(String::as_str)
        .unwrap_or_default();
    // authorize 即鉴权目的本身；返回键此处不再用。
    if let Err(e) = state.authorize(&headers, key, state.cfg.social_v2.tools.group_members) {
        return e;
    }
    let Some(group) = key.strip_prefix("group:") else {
        return denied(StatusCode::BAD_REQUEST, "仅群会话支持（group:群号）");
    };
    match state.onebot.call(
        "get_group_member_list",
        json!({ "group_id": group.parse::<u64>().unwrap_or(0) }),
    ) {
        Ok(list) => {
            let members: Vec<Value> = list
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| {
                            let uid = m.get("user_id").and_then(Value::as_u64)?;
                            Some(json!({
                                "userId": uid,
                                "nickname": m.get("nickname").and_then(Value::as_str).unwrap_or(""),
                                "card": m.get("card").and_then(Value::as_str).unwrap_or(""),
                            }))
                        })
                        .collect()
                })
                .unwrap_or_default();
            ok_json(json!({ "members": members, "count": members.len() }))
        }
        Err(e) => denied(StatusCode::BAD_GATEWAY, &format!("获取群成员失败：{e}")),
    }
}

/// qq_get_group_profile：群档案（名称/人数/公告——Node 版荣誉信息降级略去）。
async fn group_profile(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let key = q
        .get("conversationKey")
        .map(String::as_str)
        .unwrap_or_default();
    // authorize 即鉴权目的本身；返回键此处不再用。
    if let Err(e) = state.authorize(&headers, key, state.cfg.social_v2.tools.group_profile) {
        return e;
    }
    let Some(group) = key.strip_prefix("group:") else {
        return denied(StatusCode::BAD_REQUEST, "仅群会话支持（group:群号）");
    };
    let gid = group.parse::<u64>().unwrap_or(0);
    let info = match state
        .onebot
        .call("get_group_info", json!({ "group_id": gid }))
    {
        Ok(v) => v,
        Err(e) => return denied(StatusCode::BAD_GATEWAY, &format!("获取群信息失败：{e}")),
    };
    let notice = state
        .onebot
        .call("_get_group_notice", json!({ "group_id": gid }))
        .ok()
        .and_then(|n| {
            n.as_array()
                .and_then(|arr| arr.first())
                .and_then(|f| f.get("text"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    ok_json(json!({
        "groupId": gid,
        "name": info.get("group_name").and_then(Value::as_str).unwrap_or(""),
        "memberCount": info.get("member_count").and_then(Value::as_u64).unwrap_or(0),
        "maxMemberCount": info.get("max_member_count").and_then(Value::as_u64).unwrap_or(0),
        "notice": notice,
    }))
}

/// qq_get_group_history：群最近消息（文本化，含占位标注）。
async fn group_history(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let key = q
        .get("conversationKey")
        .map(String::as_str)
        .unwrap_or_default();
    // authorize 即鉴权目的本身；返回键此处不再用。
    if let Err(e) = state.authorize(&headers, key, state.cfg.social_v2.tools.group_history) {
        return e;
    }
    let Some(group) = key.strip_prefix("group:") else {
        return denied(StatusCode::BAD_REQUEST, "仅群会话支持（group:群号）");
    };
    let raw = match state.onebot.call(
        "get_group_msg_history",
        json!({ "group_id": group.parse::<u64>().unwrap_or(0), "count": 20 }),
    ) {
        Ok(v) => v,
        Err(e) => return denied(StatusCode::BAD_GATEWAY, &format!("获取群历史失败：{e}")),
    };
    let mut lines = Vec::new();
    for m in raw
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let sender = m
            .get("sender")
            .and_then(|s| s.get("card").or_else(|| s.get("nickname")))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let text = m
            .get("message")
            .and_then(Value::as_array)
            .map(|arr| segments_to_text(arr))
            .unwrap_or_default();
        if !text.trim().is_empty() {
            lines.push(format!("{sender}: {text}"));
        }
    }
    ok_json(json!({ "messages": lines, "count": lines.len() }))
}

/// qq_get_self_image：自己的 Q 版形象（登录号头像下载 base64 直供视觉模型）。
async fn self_image(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let key = q
        .get("conversationKey")
        .map(String::as_str)
        .unwrap_or_default();
    // authorize 即鉴权目的本身；返回键此处不再用。
    if let Err(e) = state.authorize(&headers, key, state.cfg.social_v2.tools.get_self_image) {
        return e;
    }
    let login = match state.onebot.get_login_info() {
        Ok(v) => v,
        Err(e) => {
            return denied(
                StatusCode::BAD_GATEWAY,
                &format!("get_login_info 失败：{e}"),
            );
        }
    };
    let qq = login.get("user_id").and_then(Value::as_u64).unwrap_or(0);
    let nickname = login
        .get("nickname")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let url = format!("https://q1.qlogo.cn/g?b=qq&nk={qq}&s=640");
    let img = tokio::task::spawn_blocking(move || {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .ok()?;
        let bytes = client.get(&url).send().ok()?.bytes().ok()?;
        use base64::Engine;
        Some(base64::engine::general_purpose::STANDARD.encode(&bytes))
    })
    .await
    .unwrap_or(None);
    match img {
        Some(data) => ok_json(json!({
            "qq": qq, "nickname": nickname,
            "image": { "mimeType": "image/jpeg", "data": data }
        })),
        None => denied(
            StatusCode::BAD_GATEWAY,
            "头像下载失败（q1.qlogo.cn 不可达？稍后再试）",
        ),
    }
}

/// qq_wait_for_messages：等待新消息（核心版语义——purpose=reply 按最后
/// 来信补足静默即返；messages 等新消息或超时）。preSleepWait 记账等沉睡
/// 观察状态机为简化点（字段保留未消费，后续批次对齐）。
async fn wait_messages(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let key = q
        .get("conversationKey")
        .map(String::as_str)
        .unwrap_or_default();
    let key = match state.authorize(&headers, key, state.cfg.social_v2.tools.wait_messages) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let purpose_reply = q.get("purpose").map(String::as_str) == Some("reply");
    let timeout_ms: u64 = q
        .get("timeoutMs")
        .and_then(|t| t.parse().ok())
        .unwrap_or(30_000)
        .min(600_000);
    let quiet_ms: u64 = q
        .get("quietMs")
        .and_then(|t| t.parse().ok())
        .unwrap_or(10_000);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        let now = crate::session_manager::SessionManager::now_ms();
        let (unread, last_incoming, my_last, snapshot) = {
            let mut store = state.social.lock().unwrap();
            let st = store.conversation_mut(&key);
            let my_last = st
                .recent_messages
                .iter()
                .rev()
                .find(|m| m.is_self)
                .map(|m| m.time)
                .unwrap_or(0);
            let msgs: Vec<Value> = st
                .unread
                .iter()
                .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
                .collect();
            (
                st.unread.len(),
                st.last_incoming_at,
                my_last,
                (st.last_unread_seq, msgs),
            )
        };
        if purpose_reply {
            // 回复前短静默：对方最后来信后已安静足够（或自己刚说过话）即返。
            let anchor_time = my_last.max(last_incoming);
            if now.saturating_sub(anchor_time) >= quiet_ms {
                return ok_json(json!({ "quiet": true, "unreadCount": unread }));
            }
        } else if unread > 0 {
            let (read_through, msgs) = snapshot;
            return ok_json(json!({
                "newMessages": msgs, "unreadCount": unread,
                "readThroughSeq": read_through,
            }));
        }
        if tokio::time::Instant::now() >= deadline {
            return ok_json(
                json!({ "timeout": true, "unreadCount": unread, "quiet": purpose_reply }),
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// qq_social_state：会话二代仿真状态摘要（只读）。
async fn social_state(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let key = q
        .get("conversationKey")
        .map(String::as_str)
        .unwrap_or_default();
    let key = match state.authorize(&headers, key, state.cfg.social_v2.tools.social_state) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let now = crate::session_manager::SessionManager::now_ms();
    let mut store = state.social.lock().unwrap();
    let st = store.conversation_mut(&key);
    ok_json(json!({
        "wakeConfig": serde_json::to_value(&st.wake_config).unwrap_or(Value::Null),
        "unreadCount": st.unread.len(),
        "lastIncomingAt": st.last_incoming_at,
        "lastAiReplyAt": st.last_ai_reply_at,
        "lastActionAt": st.last_action_at,
        "wakeTimesCount": st.wake_times.len(),
        "sendTimesRecentHour": st.send_times.iter().filter(|t| now.saturating_sub(**t) < 3_600_000).count(),
        "activeTopics": st.active_topics.len(),
        "pendingThoughts": st.pending_thoughts.len(),
        "impressions": st.member_impressions.len(),
    }))
}

/// qq_get_my_recent_messages：自己最近发的消息（is_self 记录）。
async fn my_recent(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let key = q
        .get("conversationKey")
        .map(String::as_str)
        .unwrap_or_default();
    let key = match state.authorize(&headers, key, state.cfg.social_v2.tools.get_my_recent) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let limit: usize = q
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(10)
        .min(50);
    let mut store = state.social.lock().unwrap();
    let st = store.conversation_mut(&key);
    let mine: Vec<Value> = st
        .recent_messages
        .iter()
        .filter(|m| m.is_self)
        .rev()
        .take(limit)
        .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
        .collect();
    ok_json(json!({ "messages": mine, "readThroughSeq": st.last_read_through_seq }))
}

/// qq_list_groups：机器人所在群列表（任意白名单会话令牌可用）。
async fn list_groups(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let key = q
        .get("conversationKey")
        .map(String::as_str)
        .unwrap_or_default();
    if let Err(e) = state.authorize(&headers, key, state.cfg.social_v2.tools.social_state) {
        return e;
    }
    match state.onebot.call("get_group_list", json!({})) {
        Ok(list) => {
            let groups: Vec<Value> = list
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|g| {
                            let gid = g.get("group_id").and_then(Value::as_u64)?;
                            Some(json!({
                                "groupId": gid,
                                "name": g.get("group_name").and_then(Value::as_str).unwrap_or(""),
                                "memberCount": g.get("member_count").and_then(Value::as_u64).unwrap_or(0),
                            }))
                        })
                        .collect()
                })
                .unwrap_or_default();
            ok_json(json!({ "groups": groups, "count": groups.len() }))
        }
        Err(e) => denied(StatusCode::BAD_GATEWAY, &format!("获取群列表失败：{e}")),
    }
}

/// qq_report_feedback：AI 上报用户反馈/异常（追加 feedback.jsonl + 活动日志）。
async fn report_feedback(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key = match state.authorize(&headers, key, state.cfg.social_v2.tools.feedback) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let text = body.get("text").and_then(Value::as_str).unwrap_or_default();
    if text.trim().is_empty() {
        return denied(StatusCode::BAD_REQUEST, "text 不能为空");
    }
    let line = json!({
        "time": crate::session_manager::SessionManager::now_ms(),
        "key": key, "text": text,
    });
    let file = crate::paths::queqiao_home()
        .join("state")
        .join("feedback.jsonl");
    use std::io::Write;
    let mut f = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
    {
        Ok(f) => f,
        Err(e) => {
            return denied(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("写反馈失败：{e}"),
            );
        }
    };
    if let Err(e) = writeln!(f, "{line}") {
        return denied(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("写反馈失败：{e}"),
        );
    }
    crate::logging::activity(&format!(
        "{key} 反馈上报：{}",
        text.chars().take(60).collect::<String>()
    ));
    ok_json(json!({ "ok": true }))
}

/// qq_speak：文字转语音直发（SAPI 合成 → record 段）。语音默认关（与 Node
/// 一致）；与 send_voice 共享 sendVoice 开关与发送频率额度。
async fn speak_voice(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key = match state.authorize(&headers, key, state.cfg.social_v2.tools.send_voice) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let text = body.get("text").and_then(Value::as_str).unwrap_or_default();
    let rate: i32 = body
        .get("rate")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .clamp(-10, 10) as i32;
    if text.trim().is_empty() {
        return denied(StatusCode::BAD_REQUEST, "text 不能为空");
    }
    if let Err(e) = check_send_rate(&state, &key) {
        return e;
    }
    let wav = crate::paths::queqiao_home().join("state").join(format!(
        "tts-{}.wav",
        crate::session_manager::SessionManager::now_ms()
    ));
    std::fs::create_dir_all(wav.parent().unwrap()).ok();
    use base64::Engine;
    let script = format!(
        "Add-Type -AssemblyName System.Speech;          $s = New-Object System.Speech.Synthesis.SpeechSynthesizer;          $s.Rate = {rate};          $s.SetOutputToWaveFile('{}');          $s.Speak([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{}')));          $s.Dispose()",
        wav.display(),
        base64::engine::general_purpose::STANDARD.encode(text),
    );
    let wav_path = wav.clone();
    let synth = tokio::task::spawn_blocking(move || {
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .creation_flags(0x0800_0000)
            .output()
            .map(|o| o.status.success() && wav_path.exists())
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    if !synth {
        let _ = std::fs::remove_file(&wav);
        return denied(
            StatusCode::INTERNAL_SERVER_ERROR,
            "TTS 合成失败（System.Speech 不可用？）",
        );
    }
    let file_ref = format!(
        "file:///{}",
        wav.display().to_string().trim_start_matches('/')
    );
    let segments = vec![queqiao_onebot::segment::record(&file_ref)];
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
            // NapCat 发送时已读入文件；即时清理临时 wav。
            let _ = std::fs::remove_file(&wav);
            mark_reply_action(&state, &key);
            ok_json(json!({ "verified": true, "messageId": message_id }))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&wav);
            denied(StatusCode::BAD_GATEWAY, &format!("语音发送失败：{e}"))
        }
    }
}

/// qq_send_voice：发语音库（state/voices/*.wav）里的音频。
async fn send_voice(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key = match state.authorize(&headers, key, state.cfg.social_v2.tools.send_voice) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let name = body
        .get("voiceName")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if name.trim().is_empty() {
        return denied(StatusCode::BAD_REQUEST, "voiceName 不能为空");
    }
    // 防路径穿越：名字仅字母数字/下划线/连字符，实际文件固定 .wav 后缀。
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return denied(
            StatusCode::BAD_REQUEST,
            "voiceName 仅允许字母数字/下划线/连字符",
        );
    }
    let wav = crate::paths::queqiao_home()
        .join("state")
        .join("voices")
        .join(format!("{name}.wav"));
    if !wav.exists() {
        return denied(
            StatusCode::NOT_FOUND,
            &format!("语音 {name} 不存在（qq_list_voices 查库）"),
        );
    }
    if let Err(e) = check_send_rate(&state, &key) {
        return e;
    }
    let file_ref = format!(
        "file:///{}",
        wav.display().to_string().trim_start_matches('/')
    );
    let segments = vec![queqiao_onebot::segment::record(&file_ref)];
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
            mark_reply_action(&state, &key);
            ok_json(json!({ "verified": true, "messageId": message_id }))
        }
        Err(e) => denied(StatusCode::BAD_GATEWAY, &format!("语音发送失败：{e}")),
    }
}

/// qq_list_voices：语音库清单。
async fn list_voices(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let key = q
        .get("conversationKey")
        .map(String::as_str)
        .unwrap_or_default();
    if let Err(e) = state.authorize(&headers, key, state.cfg.social_v2.tools.send_voice) {
        return e;
    }
    let dir = crate::paths::queqiao_home().join("state").join("voices");
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if let Some(stem) = n.strip_suffix(".wav") {
                names.push(stem.to_owned());
            }
        }
    }
    names.sort();
    ok_json(json!({ "voices": names, "count": names.len() }))
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

/// 诊断包：一键收集排障所需（控制台「导出诊断包」按钮）——运行日志双份、
/// 活动流水、版本、脱敏配置、会话概要。全部内嵌单 JSON，前端直接下载。
async fn diagnostics(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Query(_q): Query<BTreeMap<String, String>>,
) -> ApiResult {
    // 沿用控制台令牌（比 agent token 宽：诊断含配置，只该给操作者）。
    if !state.cfg.console_token.is_empty() {
        let auth = headers
            .get("x-console-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth != state.cfg.console_token {
            return denied(StatusCode::UNAUTHORIZED, "console token required");
        }
    }
    let home = crate::paths::queqiao_home();
    let read = |name: &str| {
        std::fs::read_to_string(home.join("state").join(name))
            .unwrap_or_else(|_| "(不可读或不存在)".to_owned())
    };
    // 配置脱敏：令牌类键打码后内嵌。
    let mut cfg_json = serde_json::to_value(&state.cfg).unwrap_or(Value::Null);
    if let Some(obj) = cfg_json.as_object_mut() {
        for section in ["snowluma", "dsh"] {
            if let Some(s) = obj.get_mut(section).and_then(Value::as_object_mut) {
                for key in ["accessToken", "authToken"] {
                    if let Some(v) = s.get(key)
                        && v.as_str().is_some_and(|x| !x.is_empty())
                    {
                        s.insert(key.to_string(), json!("***"));
                    }
                }
            }
        }
        if let Some(t) = obj.get_mut("consoleToken")
            && t.as_str().is_some_and(|x| !x.is_empty())
        {
            *t = json!("***");
        }
    }
    let store = state.social.lock().unwrap();
    let sessions: Vec<String> = store.conversation_keys();
    let summary: Vec<Value> = sessions
        .iter()
        .map(|k| {
            store
                .conversation(k)
                .map(|st| {
                    json!({
                        "key": k,
                        "unread": st.unread.len(),
                        "recent": st.recent_messages.len(),
                        "lastIncomingAt": st.last_incoming_at,
                        "wakeMode": st.wake_config.mode,
                    })
                })
                .unwrap_or(Value::Null)
        })
        .collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    ok_json(json!({
        "generatedAt": now,
        "version": env!("CARGO_PKG_VERSION"),
        "bridgeLog": read("bridge.log"),
        "bridgeLogPrev": read("bridge.log.1"),
        "activityLog": read("activity.log"),
        "activityLogPrev": read("activity.log.1"),
        "config": cfg_json,
        "sessions": summary,
    }))
}

/// 工具调用审计查询（Node readToolLog）：尾 N 条 call/result 流水。
/// 仅控制台——agent token 一律拒绝（防 AI 审计自己越权）。
async fn tool_log(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> ApiResult {
    if !state.cfg.console_token.is_empty() {
        let auth = headers
            .get("x-console-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth != state.cfg.console_token {
            return denied(StatusCode::UNAUTHORIZED, "该接口仅控制台可用");
        }
    }
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(1, 500);
    let file = crate::paths::queqiao_home()
        .join("state")
        .join("tool-calls.jsonl");
    let raw = std::fs::read_to_string(&file).unwrap_or_default();
    let entries: Vec<Value> = raw
        .lines()
        .rev()
        .take(limit)
        .filter_map(|l| serde_json::from_str(l.trim()).ok())
        .collect();
    ok_json(json!({ "entries": entries }))
}

/// 清空审计流水（控制台）。
async fn tool_log_clear(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
) -> ApiResult {
    if !state.cfg.console_token.is_empty() {
        let auth = headers
            .get("x-console-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth != state.cfg.console_token {
            return denied(StatusCode::UNAUTHORIZED, "该接口仅控制台可用");
        }
    }
    let file = crate::paths::queqiao_home()
        .join("state")
        .join("tool-calls.jsonl");
    let _ = std::fs::write(&file, "");
    ok_json(json!({ "cleared": true }))
}

/// qq_get_active_members：会话活跃成员（recentMessages 按 sender 聚合）。
async fn active_members(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> ApiResult {
    let key = match state.authorize(
        &headers,
        q.get("conversationKey").map(String::as_str).unwrap_or(""),
        state.cfg.social_v2.tools.get_active_members,
    ) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(10);
    let store = state.social.lock().unwrap();
    let recent = store
        .conversation(&key)
        .map(|st| st.recent_messages.clone())
        .unwrap_or_default();
    drop(store);
    ok_json(json!({ "key": key, "members": crate::social_store::active_members(&recent, limit) }))
}

/// 花费时间序列（控制台趋势图用；仅控制台）。
async fn tokens_series(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> ApiResult {
    if !state.cfg.console_token.is_empty() {
        let auth = headers
            .get("x-console-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth != state.cfg.console_token {
            return denied(StatusCode::UNAUTHORIZED, "该接口仅控制台可用");
        }
    }
    let hours = q
        .get("hours")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(24)
        .clamp(1, 24 * 90);
    let buckets = q
        .get("buckets")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(48);
    let file = crate::paths::queqiao_home()
        .join("state")
        .join("token-usage.jsonl");
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    ok_json(
        serde_json::to_value(queqiao_core::token_ledger::series(&text, hours, buckets))
            .unwrap_or_default(),
    )
}

/// 按会话回合明细（控制台；key 必填）。
async fn tokens_turns(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> ApiResult {
    if !state.cfg.console_token.is_empty() {
        let auth = headers
            .get("x-console-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth != state.cfg.console_token {
            return denied(StatusCode::UNAUTHORIZED, "该接口仅控制台可用");
        }
    }
    let key = q
        .get("key")
        .map(String::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if key.is_empty() {
        return denied(StatusCode::BAD_REQUEST, "key 不能为空");
    }
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(200);
    let file = crate::paths::queqiao_home()
        .join("state")
        .join("token-usage.jsonl");
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let turns = queqiao_core::token_ledger::conversation(&text, &key, limit);
    if turns.is_empty() {
        return denied(StatusCode::NOT_FOUND, "未找到该会话的用量记录");
    }
    ok_json(json!({ "key": key, "turns": turns }))
}

/// 人格卡列表（控制台切换用）：roles/*.md 名单 + 当前角色与 perConv 覆盖。
async fn roles_list(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
) -> ApiResult {
    if !state.cfg.console_token.is_empty() {
        let auth = headers
            .get("x-console-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth != state.cfg.console_token {
            return denied(StatusCode::UNAUTHORIZED, "console token required");
        }
    }
    let home = crate::paths::queqiao_home();
    let mut cards: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(home.join("roles")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(stem) = name.strip_suffix(".md")
                && crate::role_card_store::sanitize_output_name(stem)
            {
                cards.push(stem.to_owned());
            }
        }
    }
    cards.sort();
    let current: Value = std::fs::read_to_string(home.join("state").join("current-role.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok())
        .unwrap_or(Value::Null);
    ok_json(json!({ "roles": cards, "current": current }))
}

/// 切换当前人格（控制台）：写 current-role.json 的 role 字段（保留 perConv
/// 等其余字段；原子写）。注入无缓存，下一条消息即刻生效。
async fn role_switch(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    if !state.cfg.console_token.is_empty() {
        let auth = headers
            .get("x-console-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth != state.cfg.console_token {
            return denied(StatusCode::UNAUTHORIZED, "console token required");
        }
    }
    let role = body
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if role.is_empty() {
        return denied(StatusCode::BAD_REQUEST, "role 不能为空");
    }
    if !crate::role_card_store::sanitize_output_name(&role) {
        return denied(StatusCode::BAD_REQUEST, "角色名只允许中文/字母/数字/横线");
    }
    let home = crate::paths::queqiao_home();
    if !home.join("roles").join(format!("{role}.md")).exists() {
        return denied(StatusCode::NOT_FOUND, &format!("roles/{role}.md 不存在"));
    }
    let file = home.join("state").join("current-role.json");
    let mut current: Value = std::fs::read_to_string(&file)
        .ok()
        .and_then(|raw| serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_else(|| json!({}));
    if !current.is_object() {
        current = json!({});
    }
    current["role"] = json!(role);
    let body_text = serde_json::to_string_pretty(&current).unwrap_or_else(|_| "{}".into());
    if let Err(e) = crate::state::atomic_write_json(&file, &body_text) {
        return denied(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("写 current-role.json 失败：{e}"),
        );
    }
    ok_json(json!({ "switched": true, "role": role }))
}

/// DSH 模型目录（控制台换模型下拉用；session.modelCatalog 回落 session.models）。
async fn dsh_catalog(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
) -> ApiResult {
    if !state.cfg.console_token.is_empty() {
        let auth = headers
            .get("x-console-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth != state.cfg.console_token {
            return denied(StatusCode::UNAUTHORIZED, "console token required");
        }
    }
    let Ok(mut dsh) = state.dsh.lock() else {
        return denied(StatusCode::INTERNAL_SERVER_ERROR, "dsh 锁中毒");
    };
    let now = crate::session_manager::SessionManager::now_ms();
    let catalog = match dsh
        .call_unary("session/modelCatalog", Some(json!({})), now)
        .and_then(|o| o.unwrap("session/modelCatalog"))
    {
        Ok(v) => v,
        Err(_) => match dsh
            .call_unary("session/models", Some(json!({})), now)
            .and_then(|o| o.unwrap("session/models"))
        {
            Ok(v) => v,
            Err(e) => return denied(StatusCode::BAD_GATEWAY, &format!("模型目录查询失败：{e}")),
        },
    };
    match serde_json::to_value(&catalog) {
        Ok(v) => ok_json(v),
        Err(e) => denied(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("目录序列化失败：{e}"),
        ),
    }
}

/// 会话重置（控制台）：退役本地映射，下次投递按 fail-closed 重建空历史会话。
async fn session_reset(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    if !state.cfg.console_token.is_empty() {
        let auth = headers
            .get("x-console-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth != state.cfg.console_token {
            return denied(StatusCode::UNAUTHORIZED, "console token required");
        }
    }
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let Ok(mut m) = state.sessions.lock() else {
        return denied(StatusCode::INTERNAL_SERVER_ERROR, "sessions 锁中毒");
    };
    m.retire(&key);
    ok_json(json!({ "retired": true, "conversationKey": key }))
}

/// qq_slang_query：查已确认黑话表（按出现次数排序；含 candidate 供 AI 了解学习态）。
async fn slang_query(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> ApiResult {
    let key = match state.authorize(
        &headers,
        q.get("conversationKey").map(String::as_str).unwrap_or(""),
        state.cfg.social_v2.tools.slang_query,
    ) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let _ = key;
    let file = crate::slang_store::slang_file(&crate::paths::queqiao_home());
    let entries = match crate::slang_store::load(&file) {
        Ok(v) => v,
        Err(e) => return denied(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let query = q
        .get("query")
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();
    let mut matched: Vec<&queqiao_core::slang::SlangEntry> = if query.is_empty() {
        entries.iter().collect()
    } else {
        entries
            .iter()
            .filter(|e| {
                format!("{} {} {} {}", e.content, e.meaning, e.usage, e.example)
                    .to_lowercase()
                    .contains(&query)
            })
            .collect()
    };
    matched.sort_by_key(|e| std::cmp::Reverse(e.count));
    let items: Vec<serde_json::Value> = matched
        .iter()
        .take(30)
        .map(|e| serde_json::to_value(*e).unwrap_or_default())
        .collect();
    ok_json(json!({ "total": entries.len(), "matched": matched.len(), "slang": items }))
}

/// qq_slang_submit：AI 提交/更新词条（candidate 待确认；同 content 覆盖释义）。
async fn slang_submit(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let key = match state.authorize(&headers, &key, state.cfg.social_v2.tools.slang_submit) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let content = body
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if content.is_empty() {
        return denied(StatusCode::BAD_REQUEST, "content 不能为空");
    }
    let meaning = body
        .get("meaning")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let usage = body
        .get("usage")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let example = body
        .get("example")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let file = crate::slang_store::slang_file(&crate::paths::queqiao_home());
    let mut entries = match crate::slang_store::load(&file) {
        Ok(v) => v,
        Err(e) => return denied(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let now_iso = crate::logging::local_timestamp().unwrap_or_default();
    match entries.iter_mut().find(|e| e.content == content) {
        Some(e) => {
            // 同词更新：AI 侧补充释义，保留人工确认态。
            if !meaning.is_empty() {
                e.meaning = meaning;
            }
            if !usage.is_empty() {
                e.usage = usage;
            }
            if !example.is_empty() {
                e.example = example;
            }
            e.count += 1;
            e.updated_at = now_iso;
            ok_json(
                json!({ "updated": true, "entry": serde_json::to_value(&*e).unwrap_or_default() }),
            )
        }
        None => {
            let entry = queqiao_core::slang::SlangEntry {
                content,
                meaning,
                usage,
                example,
                status: "candidate".into(),
                source: "ai".into(),
                count: 1,
                evidence: vec![queqiao_core::slang::SlangEvidence {
                    key,
                    sender: "ai".into(),
                    text: "AI 提交".into(),
                    time: crate::session_manager::SessionManager::now_ms(),
                }],
                created_at: now_iso.clone(),
                updated_at: now_iso,
                ..Default::default()
            };
            let v = serde_json::to_value(&entry).unwrap_or_default();
            entries.push(entry);
            let _ = crate::slang_store::save(&file, entries);
            ok_json(json!({ "updated": false, "entry": v }))
        }
    }
}

/// 花费汇总（Node /api/tokens/summary 子集）：仅控制台可用——记账是给操作者
/// 看的，agent token 一律拒绝（Node 同语义）。
async fn tokens_summary(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
) -> ApiResult {
    if !state.cfg.console_token.is_empty() {
        let auth = headers
            .get("x-console-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth != state.cfg.console_token {
            return denied(StatusCode::UNAUTHORIZED, "该接口仅控制台可用");
        }
    }
    let file = crate::paths::queqiao_home()
        .join("state")
        .join("token-usage.jsonl");
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let summary = queqiao_core::token_ledger::summarize(&text);
    ok_json(serde_json::to_value(&summary).unwrap_or_default())
}

/// 手动检查更新（控制台「检查更新」按钮）：立即查 GitHub latest release。
/// 发现新版则经 update_tx 转发主循环，与启动自动检测共用下载/应用链路；
/// 应用时机仍是全体空闲自动，不提供手动强制应用（不打断会话）。
async fn update_check(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
) -> ApiResult {
    // 沿用控制台令牌（检查含 release 信息，只该给操作者；空 token 本机放行）。
    if !state.cfg.console_token.is_empty() {
        let auth = headers
            .get("x-console-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth != state.cfg.console_token {
            return denied(StatusCode::UNAUTHORIZED, "console token required");
        }
    }
    // reqwest blocking 不得嵌 tokio——检测放 spawn_blocking。
    let current = env!("CARGO_PKG_VERSION").to_string();
    let cur = current.clone();
    let checked = tokio::task::spawn_blocking(move || update::check_latest(&cur))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
    match checked {
        Ok(None) => ok_json(json!({ "hasUpdate": false, "current": current })),
        Ok(Some(rel)) => {
            let tag = rel.tag.clone();
            let notes = rel.notes.clone();
            // 通道容量 1：启动检测已投递过则丢弃（下载/应用本就在进行）。
            let _ = state.update_tx.try_send(rel);
            ok_json(json!({
                "hasUpdate": true,
                "current": current,
                "tag": tag,
                "notes": notes,
            }))
        }
        Err(e) => denied(StatusCode::BAD_GATEWAY, &format!("检查更新失败：{e}")),
    }
}

/// QQ 收藏同步（Node syncStickerLibrary）：fetch_custom_face_detail → merge
/// （AI 认知保留、QQ 侧字段更新、complete 清理已删）→ 存库。TTL 缓存
/// （syncTtlMs 默认 60s）；data 非法**放弃同步**（绝不能拿空数组清库）。
fn sync_stickers(state: &ToolApiState, force: bool) -> Vec<queqiao_core::sticker::StickerEntry> {
    use std::sync::Mutex;
    static SYNCED_AT: Mutex<u64> = Mutex::new(0);
    let home = crate::paths::queqiao_home();
    let file = crate::sticker_store::sticker_file(&home);
    let local = match crate::sticker_store::load(&file) {
        Ok(v) => v,
        Err(_) => return Vec::new(), // 只读不写降级
    };
    if !state.cfg.social_v2.sticker.enabled {
        return local;
    }
    let now = crate::session_manager::SessionManager::now_ms();
    let ttl = state.cfg.social_v2.sticker.sync_ttl_ms;
    let synced = *SYNCED_AT.lock().unwrap_or_else(|e| e.into_inner());
    if !force && synced > 0 && now.saturating_sub(synced) < ttl {
        return local;
    }
    let count = state.cfg.social_v2.sticker.max_list_count.clamp(1, 500);
    let resp = state
        .onebot
        .call("fetch_custom_face_detail", json!({ "count": count }));
    let Ok(resp) = resp else {
        return local; // 失败降级用本地
    };
    // 校验三关：status ok / retcode 0 / data 是数组——任何一关不过都放弃同步。
    let ok = resp.get("status").and_then(Value::as_str) == Some("ok")
        && resp.get("retcode").and_then(Value::as_i64) == Some(0);
    let Some(data) = resp.get("data").and_then(Value::as_array) else {
        return local;
    };
    if !ok {
        return local;
    }
    let fetched: Vec<queqiao_core::sticker::ValueLoose> = data
        .iter()
        .filter_map(|v| serde_json::from_value::<queqiao_core::sticker::ValueLoose>(v.clone()).ok())
        .collect();
    let complete = fetched.len() < count;
    let now_iso = crate::logging::local_timestamp().unwrap_or_default();
    let merged = queqiao_core::sticker::merge_sticker_library(local, &fetched, complete, &now_iso);
    let n = merged.len();
    if crate::sticker_store::save(&file, &merged).is_ok() {
        *SYNCED_AT.lock().unwrap_or_else(|e| e.into_inner()) = now;
        crate::logging::log(&format!(
            "[sticker] 已同步 QQ 收藏表情 {} 个（本地库 {n} 条，{}）",
            fetched.len(),
            if complete { "全量" } else { "部分" }
        ));
    }
    merged
}

/// 强制同步（控制台按钮/AI 明确要求时）。
async fn sticker_sync(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
) -> ApiResult {
    if !state.cfg.console_token.is_empty() {
        let auth = headers
            .get("x-console-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth != state.cfg.console_token {
            return denied(StatusCode::UNAUTHORIZED, "console token required");
        }
    }
    let entries = sync_stickers(&state, true);
    ok_json(json!({ "total": entries.len() }))
}

/// qq_list_stickers：表情库查询（query 匹配 desc/localNote/usage/tags/id/md5）。
async fn sticker_list(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> ApiResult {
    let key = match state.authorize(
        &headers,
        q.get("conversationKey").map(String::as_str).unwrap_or(""),
        state.cfg.social_v2.tools.list_stickers,
    ) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let _ = key;
    let home = crate::paths::queqiao_home();
    let entries = match crate::sticker_store::load(&crate::sticker_store::sticker_file(&home)) {
        Ok(v) => v,
        Err(e) => return denied(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let query = q
        .get("query")
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(48)
        .clamp(1, 500);
    let filtered: Vec<&queqiao_core::sticker::StickerEntry> = if query.is_empty() {
        entries.iter().collect()
    } else {
        entries
            .iter()
            .filter(|e| {
                let hay = format!(
                    "{} {} {} {} {} {} {} {}",
                    e.desc,
                    e.local_note,
                    e.usage,
                    e.id,
                    e.res_id,
                    e.md5,
                    e.tags.join(" "),
                    e.url
                )
                .to_lowercase();
                hay.contains(&query)
            })
            .collect()
    };
    let matched = filtered.len();
    let truncated = matched > limit;
    let items: Vec<serde_json::Value> = filtered
        .into_iter()
        .take(limit)
        .map(|e| serde_json::to_value(e).unwrap_or_default())
        .collect();
    ok_json(json!({
        "total": entries.len(),
        "matched": matched,
        "truncated": truncated,
        "stickers": items,
    }))
}

/// qq_get_sticker_image：看图（返回 URL 与备注——AI 凭 URL 看，确认含义再发）。
async fn sticker_image(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> ApiResult {
    let key = match state.authorize(
        &headers,
        q.get("conversationKey").map(String::as_str).unwrap_or(""),
        state.cfg.social_v2.tools.get_sticker_image,
    ) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let _ = key;
    // 看图也懒同步（收藏可能在会话过程中新增）。
    let entries = sync_stickers(&state, false);
    let id = q.get("stickerId").map(String::as_str).unwrap_or_default();
    match queqiao_core::sticker::find_sticker(&entries, id) {
        Some(e) => ok_json(json!({
            "id": e.id, "url": e.url, "desc": e.desc,
            "localNote": e.local_note, "tags": e.tags, "usage": e.usage,
        })),
        None => denied(StatusCode::NOT_FOUND, "表情未找到（id/md5/URL 任一匹配）"),
    }
}

/// qq_sticker_note：AI 认知更新（note/tags/usage——单行化后入库）。
async fn sticker_note(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let flag =
        state.cfg.social_v2.tools.sticker_note || state.cfg.social_v2.tools.set_sticker_remark;
    let key = match state.authorize(&headers, &key, flag) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let _ = key;
    let id = body
        .get("stickerId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let note = body.get("note").and_then(Value::as_str).map(str::to_string);
    let tags: Option<Vec<String>> = body.get("tags").and_then(|t| t.as_array()).map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect()
    });
    let usage = body
        .get("usage")
        .and_then(Value::as_str)
        .map(str::to_string);
    if note.is_none() && tags.is_none() && usage.is_none() {
        return denied(StatusCode::BAD_REQUEST, "至少提供 note / tags / usage 之一");
    }
    let home = crate::paths::queqiao_home();
    let file = crate::sticker_store::sticker_file(&home);
    let mut entries = match crate::sticker_store::load(&file) {
        Ok(v) => v,
        Err(e) => return denied(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    match queqiao_core::sticker::apply_sticker_note(&mut entries, id, note, tags, usage) {
        Some(updated) => {
            if let Err(e) = crate::sticker_store::save(&file, &entries) {
                return denied(StatusCode::INTERNAL_SERVER_ERROR, &e);
            }
            ok_json(serde_json::to_value(&updated).unwrap_or_default())
        }
        None => denied(StatusCode::NOT_FOUND, "表情未找到"),
    }
}

/// qq_send_sticker：发一张收藏表情（image 段 = 表情 URL；一次一张不混文字）。
async fn send_sticker(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let key = match state.authorize(&headers, &key, state.cfg.social_v2.tools.send_sticker) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let id = body
        .get("stickerId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let home = crate::paths::queqiao_home();
    let file = crate::sticker_store::sticker_file(&home);
    let mut entries = match crate::sticker_store::load(&file) {
        Ok(v) => v,
        Err(e) => return denied(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let Some(entry) = queqiao_core::sticker::find_sticker(&entries, id).cloned() else {
        return denied(
            StatusCode::NOT_FOUND,
            "表情未找到（先 qq_list_stickers 查）",
        );
    };
    if entry.url.is_empty() {
        return denied(
            StatusCode::CONFLICT,
            "该表情无 URL（QQ 端未提供），无法发送",
        );
    }
    if let Err(e) = check_send_rate(&state, &key) {
        return e;
    }
    let mut segments = vec![queqiao_onebot::segment::image(&entry.url)];
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
            // 使用计数 + 记账（与 send_message 同款）。
            let _ = queqiao_core::sticker::mark_sticker_used(&mut entries, id, &key);
            let _ = crate::sticker_store::save(&file, &entries);
            {
                let mut store = state.social.lock().unwrap();
                let now = crate::session_manager::SessionManager::now_ms();
                let st = store.conversation_mut(&key);
                st.last_action_at = now;
                st.last_ai_reply_at = now;
                store.save();
            }
            ok_json(json!({ "messageId": message_id, "sticker": entry.desc }))
        }
        Err(e) => denied(StatusCode::BAD_GATEWAY, &format!("OneBot 发送失败：{e}")),
    }
}

/// qq_memory_query：记忆文本块（话题/想法/印象——与注入 prompt 的形态一致）。
async fn memory_list(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> ApiResult {
    let key = match state.authorize(
        &headers,
        q.get("conversationKey").map(String::as_str).unwrap_or(""),
        state.cfg.social_v2.tools.memory,
    ) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let store = state.social.lock().unwrap();
    let now = crate::session_manager::SessionManager::now_ms();
    let block = store
        .conversation(&key)
        .map(|st| crate::memory::format_memory(st, now))
        .unwrap_or_default();
    ok_json(json!({ "memory": block }))
}

/// qq_memory_append：追加记忆（activeTopic / pendingThought / memberImpression）。
async fn memory_append(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let key = match state.authorize(&headers, &key, state.cfg.social_v2.tools.memory) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let category = body
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let content = body
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if content.trim().is_empty() {
        return denied(StatusCode::BAD_REQUEST, "content 不能为空");
    }
    let extra = crate::memory::MemoryExtra {
        participants: body
            .get("participants")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        pending_question: body
            .get("pendingQuestion")
            .and_then(Value::as_str)
            .map(str::to_owned),
        motivation: body
            .get("motivation")
            .and_then(Value::as_str)
            .map(str::to_owned),
        expires_at_ms: body.get("expiresAtMs").and_then(Value::as_u64),
        target: body
            .get("target")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    let now = crate::session_manager::SessionManager::now_ms();
    let mut store = state.social.lock().unwrap();
    let st = store.conversation_mut(&key);
    let written = crate::memory::append(st, category, content, &extra, now);
    st.last_action_at = now;
    store.save();
    if written {
        ok_json(json!({ "appended": true, "category": category }))
    } else {
        denied(
            StatusCode::BAD_REQUEST,
            "未知 category（activeTopic/pendingThought/memberImpression）或缺少 target",
        )
    }
}

/// qq_memory_remove：按类别 + 内容精确匹配删除。
async fn memory_remove(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let key = match state.authorize(&headers, &key, state.cfg.social_v2.tools.memory) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let category = body
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let content = body
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut store = state.social.lock().unwrap();
    let st = store.conversation_mut(&key);
    let removed = crate::memory::remove(st, category, content);
    store.save();
    if removed {
        ok_json(json!({ "removed": true }))
    } else {
        denied(StatusCode::NOT_FOUND, "未找到匹配的记忆条目")
    }
}

/// qq_memory_clear：清空类别（或 all）。
async fn memory_clear(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let key = body
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let key = match state.authorize(&headers, &key, state.cfg.social_v2.tools.memory) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let category = body
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or("all");
    let mut store = state.social.lock().unwrap();
    let ok = crate::memory::clear(store.conversation_mut(&key), category);
    store.save();
    if ok {
        ok_json(json!({ "cleared": category }))
    } else {
        denied(StatusCode::BAD_REQUEST, "未知 category")
    }
}

/// qq_get_recent_messages（含 my-recent 别名）：最近消息（倒序，默认 20 条）。
async fn recent_messages(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> ApiResult {
    let key = match state.authorize(
        &headers,
        q.get("conversationKey").map(String::as_str).unwrap_or(""),
        state.cfg.social_v2.tools.get_recent,
    ) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let limit: usize = q
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(20)
        .min(100);
    let only_self = q.contains_key("onlySelf");
    let store = state.social.lock().unwrap();
    let messages: Vec<Value> = store
        .conversation(&key)
        .map(|st| {
            st.recent_messages
                .iter()
                .rev()
                .filter(|m| !only_self || m.is_self)
                .take(limit)
                .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
                .collect()
        })
        .unwrap_or_default();
    ok_json(json!({ "messages": messages, "count": messages.len() }))
}

/// qq_get_message_detail：按 messageId 查单条消息（含媒体信息）。
async fn message_detail(
    axum::extract::State(state): axum::extract::State<Arc<ToolApiState>>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> ApiResult {
    let key = match state.authorize(
        &headers,
        q.get("conversationKey").map(String::as_str).unwrap_or(""),
        state.cfg.social_v2.tools.get_message_detail,
    ) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let Some(message_id) = q.get("messageId") else {
        return denied(StatusCode::BAD_REQUEST, "缺少 messageId");
    };
    let store = state.social.lock().unwrap();
    let found = store.conversation(&key).and_then(|st| {
        st.recent_messages
            .iter()
            .chain(st.unread.iter())
            .find(|m| &m.message_id == message_id)
            .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
    });
    match found {
        Some(m) => ok_json(m),
        None => denied(
            StatusCode::NOT_FOUND,
            "近期记录中找不到该 messageId（只能查本会话缓冲内的消息）",
        ),
    }
}
