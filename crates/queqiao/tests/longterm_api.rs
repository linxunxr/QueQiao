//! 长期记忆（M7-2）进程内集成测试：save→query 联动（kind/关键词过滤、
//! 记账累计）、remove 按 id、错误形状（非法 kind/空内容/超长、remove 缺 id）。

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::util::ServiceExt;

use queqiao::config::Config;
use queqiao::social_store::SocialStore;
use queqiao::tools_api::{ToolApiState, build_router};

fn temp_home() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("queqiao-ltm-api-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("state")).unwrap();
    dir
}

fn make_router(home: &std::path::Path) -> (Router, String, String) {
    let mut social = SocialStore::load(&home.join("state").join("social-v2.json"), None).unwrap();
    let token = social.ensure_agent_token("group:4242");
    let owner_token = social.ensure_agent_token("private:3388404701");
    social.save();
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
    let mut cfg = Config::default();
    cfg.allow.groups.push(4242);
    cfg.owner_qq = Some(queqiao::config::OwnerQQ(3388404701));
    cfg.allow.private.push(3388404701);
    let state = Arc::new(ToolApiState {
        sched_tx: tokio::sync::mpsc::unbounded_channel::<queqiao::orchestrator::SchedReq>().0,
        diary_pending: std::sync::Arc::new(std::sync::Mutex::new(None)),
        social: Arc::new(Mutex::new(social)),
        onebot: queqiao_onebot::client::OneBotHttpClient::new("http://127.0.0.1:1", "", 50),
        cfg,
        dsh: Arc::new(Mutex::new(mk_dsh())),
        sessions: Arc::new(Mutex::new(
            queqiao::session_manager::SessionManager::new(
                Arc::new(Mutex::new(mk_dsh())),
                &home.join("state").join("sessions.json"),
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
        console: None,
        longterm: Arc::new(Mutex::new(
            queqiao::longterm_store::open(&home.join("state").join("ltm-test.db"))
                .expect("测试长期库"),
        )),
    });
    (build_router(state, "", None), token, owner_token)
}

async fn call(r: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = r.clone().oneshot(req).await.unwrap();
    let code = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_str(&String::from_utf8_lossy(&bytes)).unwrap_or(Value::Null);
    (code, v)
}

fn save(token: &str, kind: &str, content: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/socialV2/memory/longterm-save")
        .header("content-type", "application/json")
        .header("x-agent-token", token)
        .body(Body::from(
            json!({ "conversationKey": "group:4242", "kind": kind, "content": content })
                .to_string(),
        ))
        .unwrap()
}

#[test]
fn longterm_save_query_remove_chain() {
    let home = temp_home();
    unsafe { std::env::set_var("QUEQIAO_HOME", &home) };
    let (r, token, owner_token) = make_router(&home);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // ① 存三条不同 kind。
    rt.block_on(async {
        let (code, v) = call(&r, save(&token, "fact", "群友老刘喜欢钓鱼")).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        let (code, _) = call(&r, save(&token, "impression", "管理员爱开玩笑")).await;
        assert_eq!(code, StatusCode::OK);
        let (code, _) = call(&r, save(&token, "topic", "影之刃零通关话题")).await;
        assert_eq!(code, StatusCode::OK);
    });

    // ② query：kind 过滤 + 关键词 + 记账累计（两次查询 recall_count 涨）。
    rt.block_on(async {
        let req = Request::builder()
            .method("GET")
            .uri("/api/socialV2/memory?conversationKey=group:4242&category=longterm&kind=fact&query=%E9%92%93%E9%B1%BC")
            .header("x-agent-token", &token)
            .body(Body::empty())
            .unwrap();
        let (code, v) = call(&r, req).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["value"]["count"], json!(1));
        assert!(v["value"]["memory"][0]["content"]
            .as_str()
            .unwrap()
            .contains("钓鱼"));
        let first_recall = v["value"]["memory"][0]["recallCount"].clone();
        // 再查一次：记账累计可见。
        let req = Request::builder()
            .method("GET")
            .uri("/api/socialV2/memory?conversationKey=group:4242&category=longterm&kind=fact")
            .header("x-agent-token", &token)
            .body(Body::empty())
            .unwrap();
        let (_, v2) = call(&r, req).await;
        assert_eq!(v2["value"]["count"], json!(1));
        let second = v2["value"]["memory"][0]["recallCount"].as_i64().unwrap_or(0);
        assert!(
            second > first_recall.as_i64().unwrap_or(i64::MIN),
            "记账累计：{second} vs {first_recall}"
        );
    });

    // ③ 错误形状。
    rt.block_on(async {
        let (code, v) = call(&r, save(&token, "bogus", "x")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
        let (code, _) = call(&r, save(&token, "fact", "  ")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        let long: String = "长".repeat(501);
        let (code, _) = call(&r, save(&token, "fact", &long)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "超 500 字拒");
    });

    // ④ remove 按 id（先查 id）+ 缺 id 拒。
    rt.block_on(async {
        let req = Request::builder()
            .method("GET")
            .uri(
                "/api/socialV2/memory?conversationKey=group:4242&category=longterm&kind=impression",
            )
            .header("x-agent-token", &token)
            .body(Body::empty())
            .unwrap();
        let (_, v) = call(&r, req).await;
        let id = v["value"]["memory"][0]["id"].as_i64().unwrap();

        let req = Request::builder()
            .method("POST")
            .uri("/api/socialV2/memory-remove")
            .header("content-type", "application/json")
            .header("x-agent-token", &owner_token)
            .body(Body::from(
                json!({ "conversationKey": "private:3388404701", "category": "longterm", "longtermId": id })
                    .to_string(),
            ))
            .unwrap();
        let (code, v) = call(&r, req).await;
        assert_eq!(code, StatusCode::OK, "{v}");

        let req = Request::builder()
            .method("POST")
            .uri("/api/socialV2/memory-remove")
            .header("content-type", "application/json")
            .header("x-agent-token", &owner_token)
            .body(Body::from(
                json!({ "conversationKey": "private:3388404701", "category": "longterm" }).to_string(),
            ))
            .unwrap();
        let (code, _) = call(&r, req).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "缺 longtermId 拒");
    });
}
