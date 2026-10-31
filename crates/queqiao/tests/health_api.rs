//! /api/health 升级形状（心跳观测）进程内集成测试：build_router + oneshot，
//! 不占端口。health 无鉴权（探活端点，看门狗与 UI 恢复链用）。

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::util::ServiceExt;

use queqiao::config::Config;
use queqiao::social_store::SocialStore;
use queqiao::tools_api::{ToolApiState, build_router};

fn make_router(dir: &std::path::Path) -> Router {
    let social = SocialStore::load(&dir.join("state").join("social-v2.json"), None).unwrap();
    let (update_tx, _rx) = tokio::sync::mpsc::channel(1);
    let mk_dsh = || {
        queqiao_dsh::client::DshClient::new(
            "http://127.0.0.1:1",
            queqiao_dsh::client::ReqwestTransport::new(50),
            queqiao_dsh::client::AuthSources {
                launch_token: String::new(),
                token_explicit: false,
                secret: Box::new(|| None),
                rediscover_token: None,
            },
            false,
        )
    };
    let state = Arc::new(ToolApiState {
        sched_tx: tokio::sync::mpsc::unbounded_channel::<queqiao::orchestrator::SchedReq>().0,
        diary_pending: Arc::new(Mutex::new(None)),
        social: Arc::new(Mutex::new(social)),
        onebot: queqiao_onebot::client::OneBotHttpClient::new("http://127.0.0.1:1", "", 50),
        cfg: Config::default(),
        dsh: Arc::new(Mutex::new(mk_dsh())),
        sessions: Arc::new(Mutex::new(
            queqiao::session_manager::SessionManager::new(
                Arc::new(Mutex::new(mk_dsh())),
                &dir.join("state").join("sessions.json"),
                "qq-chat-v2",
                "QQ 聊天",
                "agents",
            )
            .unwrap(),
        )),
        update_tx,
        feedback_times: Mutex::new(std::collections::HashMap::new()),
        slang_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        slang_research_tx: tokio::sync::mpsc::unbounded_channel().0,
        browser: queqiao::browser::new_browser(),
        watch: queqiao::watch::WatchState::shared(),
    });
    build_router(state, "", None)
}

/// health 形状：ok + mainLoopAlive + heartbeatAgeMs + inFlight；新建心跳即活。
/// （同步构造 + 手动 runtime：reqwest blocking client 不得在 async 上下文
/// 创建/drop——runtime-in-runtime panic，坑清单定式。）
#[test]
fn health_exposes_watch_snapshot() {
    let dir = std::env::temp_dir().join(format!("queqiao-health-api-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("state")).unwrap();
    unsafe { std::env::set_var("QUEQIAO_HOME", &dir) };
    let r = make_router(&dir);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value =
            serde_json::from_str(&String::from_utf8_lossy(&bytes)).expect("health 是 JSON");
        assert_eq!(v["ok"], true);
        assert_eq!(v["mainLoopAlive"], true, "新建 WatchState 即有心跳");
        assert!(v["heartbeatAgeMs"].is_u64());
        assert!(v["inFlight"].is_null(), "无打点时 inFlight 为 null");
    });
    let _ = std::fs::remove_dir_all(&dir);
}
