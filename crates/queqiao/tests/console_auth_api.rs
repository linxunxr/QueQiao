//! 控制台 API 鉴权（/api/config、/api/logs/tail）进程内集成测试：
//! console::router + oneshot 直打，不占端口。覆盖：令牌非空时无头/失配 401、
//! agent 令牌头 403、正确令牌放行；空令牌（向导模式）全放行——首次配置的
//! 唯一写入路径不能被锁死。

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::util::ServiceExt;

use queqiao::config::Config;
use queqiao::console::{ConsoleState, router};

fn make_router(token: &str) -> Router {
    let cfg = Config {
        console_token: token.to_string(),
        ..Default::default()
    };
    let state = Arc::new(ConsoleState {
        config_path: std::env::temp_dir().join("queqiao-console-auth-unused.json"),
        current: Mutex::new(cfg),
        first_run: std::sync::atomic::AtomicBool::new(false),
    });
    router(state)
}

async fn call(r: &Router, req: Request<Body>) -> (StatusCode, Option<Value>) {
    let resp = r.clone().oneshot(req).await.unwrap();
    let code = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_str(&String::from_utf8_lossy(&bytes)).ok();
    (code, v)
}

fn get(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::empty()).unwrap()
}

fn put(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::from(json!({}).to_string())).unwrap()
}

/// 用例元组：(路径, 请求头, 期望码)。
type Case = (&'static str, &'static [(&'static str, &'static str)], StatusCode);

/// 令牌非空：无头 401、失配 401、agent 头 403、正确令牌 200——三端点同语义。
#[tokio::test]
async fn token_enforced_on_all_three_endpoints() {
    let r = make_router("sekrit");
    const CASES: &[Case] = &[
        // (路径, 头, 期望码)
        ("/api/config", &[], StatusCode::UNAUTHORIZED),
        ("/api/logs/tail", &[], StatusCode::UNAUTHORIZED),
        (
            "/api/config",
            &[("x-console-token", "wrong")],
            StatusCode::UNAUTHORIZED,
        ),
        (
            "/api/logs/tail",
            &[("x-console-token", "wrong")],
            StatusCode::UNAUTHORIZED,
        ),
        (
            "/api/config",
            &[("x-agent-token", "a"), ("x-console-token", "sekrit")],
            StatusCode::FORBIDDEN,
        ),
        (
            "/api/config",
            &[("x-console-token", "sekrit")],
            StatusCode::OK,
        ),
        (
            "/api/logs/tail",
            &[("x-console-token", "sekrit")],
            StatusCode::OK,
        ),
    ];
    for (path, headers, want) in CASES {
        let (code, body) = call(&r, get(path, headers)).await;
        assert_eq!(code, *want, "GET {path} 带 {headers:?}");
        if *want != StatusCode::OK {
            assert_eq!(
                body.as_ref()
                    .and_then(|v| v.get("ok"))
                    .and_then(Value::as_bool),
                Some(false),
                "拒绝响应带 ok:false"
            );
        }
    }
    // PUT 同样守卫。
    let (code, _) = call(&r, put("/api/config", &[])).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    let (code, _) = call(
        &r,
        put(
            "/api/config",
            &[("x-agent-token", "a"), ("x-console-token", "sekrit")],
        ),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
}

/// 空令牌（向导模式）：三端点无头放行——首次配置不被锁死；页面壳 GET / 恒开。
#[tokio::test]
async fn empty_token_wizard_mode_allows_all() {
    let r = make_router("");
    let (code, _) = call(&r, get("/api/config", &[])).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = call(&r, get("/api/logs/tail", &[])).await;
    assert_eq!(code, StatusCode::OK);
    // 页面壳不鉴权（API 全锁后壳无敏感数据；Tauri 向导/直开模式零依赖）。
    let resp = r.clone().oneshot(get("/", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// 热更语义：current 里的令牌即活值（守卫读 token_now——用户改 consoleToken
/// 即时生效，与 socialV2 前缀中间件一致）。
#[tokio::test]
async fn guard_follows_live_config_token() {
    let cfg = Config::default();
    let state = Arc::new(ConsoleState {
        config_path: std::env::temp_dir().join("queqiao-console-auth-unused.json"),
        current: Mutex::new(cfg),
        first_run: std::sync::atomic::AtomicBool::new(false),
    });
    let r = router(state.clone());
    // 初始空令牌：放行。
    let (code, _) = call(&r, get("/api/config", &[])).await;
    assert_eq!(code, StatusCode::OK);
    // 令牌热更生效：同一 router 实例，改 current 即 401。
    state.current.lock().unwrap().console_token = "rotated".into();
    let (code, _) = call(&r, get("/api/config", &[("x-console-token", "rotated")])).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = call(&r, get("/api/config", &[])).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
}
