//! 人格卡修改提议（M6 P1-3 受治理自修订）进程内集成测试：agent 提议入队、
//! 影响提示（当前修订号+绑定会话数）、批准生效为新版（旧版归档）、拒绝留痕、
//! pending 上限、console/agent 鉴权分层。

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
    let dir = std::env::temp_dir().join(format!("queqiao-role-proposal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("state")).unwrap();
    std::fs::create_dir_all(dir.join("roles")).unwrap();
    dir
}

fn make_router(home: &std::path::Path) -> (Router, String) {
    // 预置：当前角色小鲸鱼（v1 卡），会话 group:4242 绑定 v1 修订号。
    std::fs::write(
        home.join("state").join("current-role.json"),
        r#"{"role":"小鲸鱼"}"#,
    )
    .unwrap();
    std::fs::write(home.join("roles").join("小鲸鱼.md"), "# 你是谁\n第一版").unwrap();
    let v1_rev = queqiao::role_card_store::card_revision("# 你是谁\n第一版");
    let mut social = SocialStore::load(&home.join("state").join("social-v2.json"), None).unwrap();
    let token = social.ensure_agent_token("group:4242");
    social.conversation_mut("group:4242").role_revision = Some(v1_rev);
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
        longterm: std::sync::Arc::new(std::sync::Mutex::new(
            rusqlite::Connection::open_in_memory().unwrap(),
        )),
    });
    (build_router(state, "", None), token)
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

fn post(uri: &str, token: Option<&str>, body: Value) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = token {
        b = b.header("x-agent-token", t);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn propose(token: &str, content: &str) -> Request<Body> {
    post(
        "/api/socialV2/role/proposal",
        Some(token),
        json!({
            "conversationKey": "group:4242",
            "role": "小鲸鱼",
            "content": content,
            "reason": "想把说话风格调得更活泼一点"
        }),
    )
}

#[test]
fn role_proposal_lifecycle_and_guards() {
    let home = temp_home();
    unsafe { std::env::set_var("QUEQIAO_HOME", &home) };
    let (r, token) = make_router(&home);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // ① agent 提议：入队 pending，不生效（卡内容不变）。
    let id1 = rt.block_on(async {
        let (code, v) = call(&r, propose(&token, "# 你是谁\n第二版（更活泼）")).await;
        assert_eq!(code, StatusCode::OK, "提议入队：{v}");
        assert_eq!(v["value"]["pending"], json!(1));
        v["value"]["id"].as_str().unwrap().to_owned()
    });
    assert!(
        std::fs::read_to_string(home.join("roles").join("小鲸鱼.md"))
            .unwrap()
            .contains("第一版"),
        "提议不直接生效"
    );

    // ② 控制台列表：pending 带影响提示（v1 修订号 + 1 个会话绑定）。
    rt.block_on(async {
        let req = Request::builder()
            .method("GET")
            .uri("/api/role/proposals")
            .body(Body::empty())
            .unwrap();
        let (code, v) = call(&r, req).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        let p = &v["value"]["proposals"][0];
        assert_eq!(p["status"], json!("pending"));
        let v1_rev = queqiao::role_card_store::card_revision("# 你是谁\n第一版");
        assert_eq!(p["impact"]["currentRevision"], json!(v1_rev));
        assert_eq!(p["impact"]["sessionsOnCurrent"], json!(1));
    });

    // ③ 批准：卡更新为提议内容、旧版进 history、提议转 approved。
    rt.block_on(async {
        let (code, v) = call(
            &r,
            post("/api/role/proposal/approve", None, json!({ "id": id1 })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "批准：{v}");
        assert_eq!(
            v["value"]["newRevision"],
            json!(queqiao::role_card_store::card_revision(
                "# 你是谁\n第二版（更活泼）"
            ))
        );
    });
    assert!(
        std::fs::read_to_string(home.join("roles").join("小鲸鱼.md"))
            .unwrap()
            .contains("第二版"),
        "批准后生效为新版"
    );
    assert_eq!(
        queqiao::role_card_store::history_revisions(&home, "小鲸鱼").len(),
        1,
        "旧版已归档"
    );

    // ④ 再提一条并拒绝：留痕 rejected、卡不动。
    let id2 = rt.block_on(async {
        let (code, v) = call(&r, propose(&token, "# 你是谁\n第三版")).await;
        assert_eq!(code, StatusCode::OK);
        v["value"]["id"].as_str().unwrap().to_owned()
    });
    rt.block_on(async {
        let (code, v) = call(
            &r,
            post("/api/role/proposal/reject", None, json!({ "id": id2 })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "拒绝：{v}");
        // 重复处理 → 409。
        let (code, _) = call(
            &r,
            post("/api/role/proposal/reject", None, json!({ "id": id2 })),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT);
    });
    assert!(
        std::fs::read_to_string(home.join("roles").join("小鲸鱼.md"))
            .unwrap()
            .contains("第二版"),
        "拒绝不改卡"
    );

    // ⑤ pending 上限：再提 5 条全 pending（此前 2 条已处理）→ 第 6 条 429。
    rt.block_on(async {
        for i in 0..5 {
            let (code, v) = call(&r, propose(&token, &format!("# 你是谁\n候选{i}"))).await;
            assert_eq!(code, StatusCode::OK, "第 {i} 条应入队：{v}");
        }
        let (code, v) = call(&r, propose(&token, "# 你是谁\n超限候选")).await;
        assert_eq!(code, StatusCode::TOO_MANY_REQUESTS, "满员拒新：{v}");
    });

    // ⑥ 鉴权分层：agent token 打控制台三端点一律 403。
    rt.block_on(async {
        for uri in ["/api/role/proposal/approve", "/api/role/proposal/reject"] {
            let (code, _) = call(&r, post(uri, Some(&token), json!({ "id": "whatever" }))).await;
            assert_eq!(code, StatusCode::FORBIDDEN, "{uri} 应拒 agent token");
        }
        let req = Request::builder()
            .method("GET")
            .uri("/api/role/proposals")
            .header("x-agent-token", &token)
            .body(Body::empty())
            .unwrap();
        let (code, _) = call(&r, req).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
    });
}
