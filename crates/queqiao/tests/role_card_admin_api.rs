//! 人格卡编辑面（/api/role/card）进程内集成测试：build_router + oneshot
//! 直打端点，不占端口。覆盖：读写删往返、非法名（路径穿越）拒绝、
//! 内容上限、在用卡删除保护（全局/perConv 引用 409）、agent token 拒绝。

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
    let dir = std::env::temp_dir().join(format!("queqiao-role-api-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("state")).unwrap();
    std::fs::create_dir_all(dir.join("roles")).unwrap();
    dir
}

fn make_router(home: &std::path::Path) -> Router {
    let social = SocialStore::load(&home.join("state").join("social-v2.json"), None).unwrap();
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
        diary_pending: std::sync::Arc::new(std::sync::Mutex::new(None)),
        social: Arc::new(Mutex::new(social)),
        onebot: queqiao_onebot::client::OneBotHttpClient::new("http://127.0.0.1:1", "", 50),
        cfg: Config::default(),
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
    });
    build_router(state, "", None)
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

fn get(uri: &str) -> Request<Body> {
    Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()
}
fn del(uri: &str) -> Request<Body> {
    Request::builder().method("DELETE").uri(uri).body(Body::empty()).unwrap()
}
fn put(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[test]
fn role_card_crud_and_guards() {
    let home = temp_home();
    unsafe { std::env::set_var("QUEQIAO_HOME", &home) };
    let r = make_router(&home);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        // ① 新建（PUT）：落盘 roles/小鲸鱼.md。
        let (code, v) = call(
            &r,
            put(
                "/api/role/card",
                json!({ "name": "小鲸鱼", "content": "# 人格\n你是小鲸鱼。" }),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "新建卡：{v}");
        assert!(home.join("roles").join("小鲸鱼.md").exists());

        // ② 读回（GET）：内容一致。
        let (code, v) = call(&r, get("/api/role/card?name=小鲸鱼")).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["value"]["content"], json!("# 人格\n你是小鲸鱼。"));

        // ③ 覆盖保存 → 再读回新内容。
        let (code, _) = call(
            &r,
            put("/api/role/card", json!({ "name": "小鲸鱼", "content": "v2" })),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let (_, v) = call(&r, get("/api/role/card?name=小鲸鱼")).await;
        assert_eq!(v["value"]["content"], json!("v2"));

        // ④ 非法名（路径穿越/空白）拒绝：不落盘、不留逃逸文件。
        for bad in ["../evil", "a/b", "  "] {
            let (code, _) = call(&r, put("/api/role/card", json!({ "name": bad, "content": "x" }))).await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "非法名 {bad:?} 应 400");
        }
        assert!(!home.join("evil.md").exists());
        assert!(!home.parent().unwrap().join("evil.md").exists());

        // ⑤ 内容超 64KB 拒绝。
        let big = "x".repeat(64 * 1024 + 1);
        let (code, _) = call(&r, put("/api/role/card", json!({ "name": "big", "content": big }))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);

        // ⑥ 在用卡删除保护：全局引用 → 409。
        std::fs::write(
            home.join("state").join("current-role.json"),
            r#"{"role": "小鲸鱼", "perConv": {}}"#,
        )
        .unwrap();
        let (code, v) = call(&r, del("/api/role/card?name=小鲸鱼")).await;
        assert_eq!(code, StatusCode::CONFLICT, "全局在用应 409：{v}");
        assert!(home.join("roles").join("小鲸鱼.md").exists(), "在用卡不得删除");

        // ⑦ perConv 引用同样保护：切走全局后仍 409。
        std::fs::write(
            home.join("state").join("current-role.json"),
            r#"{"role": "别的", "perConv": {"private:1": "小鲸鱼"}}"#,
        )
        .unwrap();
        let (code, _) = call(&r, del("/api/role/card?name=小鲸鱼")).await;
        assert_eq!(code, StatusCode::CONFLICT, "perConv 在用应 409");

        // ⑧ 解除引用后可删：删完 GET 404。
        std::fs::write(home.join("state").join("current-role.json"), r#"{"role": "别的"}"#).unwrap();
        let (code, _) = call(&r, del("/api/role/card?name=小鲸鱼")).await;
        assert_eq!(code, StatusCode::OK);
        assert!(!home.join("roles").join("小鲸鱼.md").exists());
        let (code, _) = call(&r, get("/api/role/card?name=小鲸鱼")).await;
        assert_eq!(code, StatusCode::NOT_FOUND);

        // ⑨ 不存在的卡：GET/DELETE 404；agent token 面 403。
        let (code, _) = call(&r, get("/api/role/card?name=不存在")).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        let (code, _) = call(&r, del("/api/role/card?name=不存在")).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        let req = Request::builder()
            .method("GET")
            .uri("/api/role/card?name=x")
            .header("x-agent-token", "anything")
            .body(Body::empty())
            .unwrap();
        let (code, _) = call(&r, req).await;
        assert_eq!(code, StatusCode::FORBIDDEN, "agent token 不得走 console 面");

        // ⑩ /api/roles 与 /api/role（切换）同属 console 面：agent-token 拒。
        let req = Request::builder()
            .method("GET")
            .uri("/api/roles")
            .header("x-agent-token", "anything")
            .body(Body::empty())
            .unwrap();
        let (code, _) = call(&r, req).await;
        assert_eq!(code, StatusCode::FORBIDDEN, "roles 列表拒 agent token");
        let req = Request::builder()
            .method("POST")
            .uri("/api/role")
            .header("content-type", "application/json")
            .header("x-agent-token", "anything")
            .body(Body::from(json!({ "role": "某人格" }).to_string()))
            .unwrap();
        let (code, _) = call(&r, req).await;
        assert_eq!(code, StatusCode::FORBIDDEN, "人格切换拒 agent token");
    });

    let _ = std::fs::remove_dir_all(&home);
}
