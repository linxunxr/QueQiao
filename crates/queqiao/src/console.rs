//! 控制台：设置中心（浏览器 UI——降低配置门槛，规格 §3.5 控制台的首块）。
//!
//! 形态：axum 复用 3100 端口；页面为内嵌单页（无前端构建链）；`PUT /api/config`
//! 做深度合并 + serde 校验（坏配置拒绝写入），保存后主循环按 mtime 热更生效。
//! 首次运行（config.json 不存在）进入**设置向导模式**：起页面 + 自动开浏览器，
//! 保存完成即继续正常启动。
//!
//! 鉴权：本服务只绑 127.0.0.1（本地单用户）；`consoleToken` 非空时
//! [`console_token_check`] 校验 `x-console-token`（tools_api 既有语义，启动时
//! 缺位自动生成——见 `Config::ensure_console_token`）。向导模式令牌为空 =
//! 免鉴权（首次配置的唯一写入路径不能被锁死）。

use std::path::PathBuf;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Html;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::Value;

use crate::config::Config;
use crate::state::atomic_write_json;

// 单源：与 Tauri 客户端共用同一份 UI（queqiao-app/ui/index.html）——
// 浏览器模式由页面内 T 判空自动降级（无组件编排，仅设置功能）。
const CONSOLE_HTML: &str = include_str!("../../queqiao-app/ui/index.html");

pub struct ConsoleState {
    /// config.json 路径（home 解析）。
    pub config_path: PathBuf,
    /// 当前生效配置（主循环热更共享的副本；UI 读它而非每次读盘以保一致）。
    pub current: Mutex<Config>,
    /// 首次设置模式（config.json 尚不存在；保存后置 false）。
    pub first_run: std::sync::atomic::AtomicBool,
}

impl ConsoleState {
    /// 守卫用令牌（读活值——热更改 consoleToken 即时生效，与 socialV2
    /// 前缀中间件同语义；向导模式 config 全默认 = 空 = 放行）。
    fn token_now(&self) -> String {
        self.current.lock().unwrap().console_token.clone()
    }
}

/// 控制台令牌校验（tools_api `console_only` 与本模块三端点共用的公共判定）：
/// 带 agent 令牌头一律 403（AI 经 MCP 也带 x-console-token，仅拒 at 才挡得住
/// AI 读写配置）；consoleToken 非空且不匹配 401；留空放行。
pub fn console_token_check(
    token: &str,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<Value>)> {
    if headers.contains_key("x-agent-token") {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "ok": false, "error": "该接口仅控制台可用" })),
        ));
    }
    if !token.is_empty() {
        let auth = headers
            .get("x-console-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth != token {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "ok": false, "error": "该接口仅控制台可用" })),
            ));
        }
    }
    Ok(())
}

/// 深度合并：对象递归、标量/数组覆盖（null 视为「未提供」跳过，便于 UI 局部提交）。
fn merge_into(base: &mut Value, patch: &Value) {
    if let (Value::Object(b), Value::Object(p)) = (base, patch) {
        for (k, v) in p {
            if v.is_null() {
                continue;
            }
            match (b.get_mut(k), v) {
                (Some(Value::Object(_)), Value::Object(_)) => merge_into(b.get_mut(k).unwrap(), v),
                _ => {
                    b.insert(k.clone(), v.clone());
                }
            }
        }
    }
}

async fn page() -> Html<&'static str> {
    Html(CONSOLE_HTML)
}

/// 桥日志尾部回放：UI 的 bridge-log 事件是增量流，桥先于 UI 启动，
/// 启动期日志（探活/OneBot/WS 连接）必错过——总览日志区需要历史回放。
async fn logs_tail(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::BTreeMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = console_token_check(&state.token_now(), &headers) {
        return e;
    }
    let n: usize = q
        .get("n")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
        .min(1000);
    let lines: Vec<String> = state
        .config_path
        .parent()
        .map(|home| home.join("state/bridge.log"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| {
            let mut tail: Vec<&str> = t.lines().filter(|l| !l.is_empty()).collect();
            let take = tail.len().saturating_sub(n);
            tail.drain(..take);
            tail.into_iter().map(str::to_owned).collect()
        })
        .unwrap_or_default();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true, "value": { "lines": lines } })),
    )
}

async fn get_config(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = console_token_check(&state.token_now(), &headers) {
        return e;
    }
    let cur = state.current.lock().unwrap().clone();
    let mut v = serde_json::to_value(&cur).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut v {
        map.insert("_version".into(), env!("CARGO_PKG_VERSION").into());
        map.insert(
            "_firstRun".into(),
            state
                .first_run
                .load(std::sync::atomic::Ordering::SeqCst)
                .into(),
        );
    }
    (StatusCode::OK, Json(v))
}

async fn put_config(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = console_token_check(&state.token_now(), &headers) {
        return e;
    }
    let reply = |ok: bool, error: Option<String>, restart_hint: bool| {
        (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": ok, "error": error, "restartHint": restart_hint })),
        )
    };
    // 基底：当前生效配置（首次模式为全默认）。
    let mut merged = serde_json::to_value(&*state.current.lock().unwrap()).unwrap_or(Value::Null);
    merge_into(&mut merged, &body);
    // 校验：serde 反序列化会做深度合并默认 + normalize（坏形状在此暴露）。
    let cfg = match serde_json::from_value::<Config>(merged) {
        Ok(c) => c,
        Err(e) => return reply(false, Some(format!("配置校验失败：{e}")), false),
    };
    let text = cfg.to_json_text();
    if let Err(e) = atomic_write_json(&state.config_path, &text) {
        return reply(false, Some(format!("写盘失败：{e}")), false);
    }
    *state.current.lock().unwrap() = cfg;
    let was_first = state.first_run.load(std::sync::atomic::Ordering::SeqCst);
    if was_first {
        state
            .first_run
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
    reply(true, None, was_first)
}

/// 控制台路由（定型 Router，可 merge 进任意已定型应用）。
pub fn router(state: Arc<ConsoleState>) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/logs/tail", get(logs_tail))
        .with_state(state)
}

/// 向导模式的兜底路由：非控制台路径一律回设置页（此模式只有设置可用）。
pub async fn router_fallback() -> Html<&'static str> {
    Html(CONSOLE_HTML)
}

/// 打开系统浏览器（Windows start；失败仅提示手动访问）。
pub fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .creation_flags(0x0800_0000)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    /// 深度合并语义：对象递归、null 跳过、标量覆盖。
    #[test]
    fn merge_semantics() {
        let mut base = serde_json::json!({ "a": 1, "b": { "x": 1, "y": 2 }, "keep": true });
        merge_into(
            &mut base,
            &serde_json::json!({ "a": 9, "b": { "x": 10, "z": 3 }, "drop": null }),
        );
        assert_eq!(
            base,
            serde_json::json!({ "a": 9, "b": { "x": 10, "y": 2, "z": 3 }, "keep": true })
        );
        // null 不清字段、未知键不覆盖已存在结构。
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    /// 分支：非空令牌无头/失配 401、匹配放行、agent 令牌头一律 403
    /// （MCP 双令牌都带，仅拒 at 才挡得住 AI）；空令牌放行（向导模式，
    /// agent 头仍拒——与 tools_api console_only 原语义一致）。
    #[test]
    fn console_token_check_branches() {
        assert!(console_token_check("", &headers(&[])).is_ok());
        let (code, _) = console_token_check("tok", &headers(&[])).unwrap_err();
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, _) =
            console_token_check("tok", &headers(&[("x-console-token", "wrong")])).unwrap_err();
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert!(console_token_check("tok", &headers(&[("x-console-token", "tok")])).is_ok());
        let (code, _) = console_token_check(
            "tok",
            &headers(&[("x-agent-token", "a"), ("x-console-token", "tok")]),
        )
        .unwrap_err();
        assert_eq!(code, StatusCode::FORBIDDEN);
        let (code, _) = console_token_check("", &headers(&[("x-agent-token", "a")])).unwrap_err();
        assert_eq!(code, StatusCode::FORBIDDEN);
    }
}
