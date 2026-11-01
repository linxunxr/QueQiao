//! 主动发言审计（M6 P1）进程内集成测试：observe-only 拦截不耗配额、
//! 正常模式放行也落审计、非主动回合不审计、控制台查询与 agent token 拒绝。

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::util::ServiceExt;

use queqiao::config::Config;
use queqiao::social_store::SocialStore;
use queqiao::tools_api::{ToolApiState, build_router};

fn temp_home(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "queqiao-proactive-audit-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("state")).unwrap();
    dir
}

/// 造路由：预置 group:4242 会话（指定唤醒原因）+ agent token，
/// observe_only 按场景可配。返回 (router, token)。
fn make_router(home: &std::path::Path, observe_only: bool, wake_reason: &str) -> (Router, String) {
    let mut social = SocialStore::load(&home.join("state").join("social-v2.json"), None).unwrap();
    let token = social.ensure_agent_token("group:4242");
    social.conversation_mut("group:4242").last_wake_reason = Some(wake_reason.to_owned());
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
    cfg.social_v2.proactive.observe_only = observe_only;
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

fn send(token: &str, text: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/socialV2/send-message")
        .header("content-type", "application/json")
        .header("x-agent-token", token)
        .body(Body::from(
            json!({ "conversationKey": "group:4242", "text": text }).to_string(),
        ))
        .unwrap()
}

fn audit_lines(home: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(home.join("state").join("proactive-audit.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_owned())
        .collect()
}

#[test]
fn proactive_audit_gate_and_listing() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // ① observe-only + 主动回合：403 拦截、审计 blocked、频率配额未消耗。
    let home_a = temp_home("block");
    unsafe { std::env::set_var("QUEQIAO_HOME", &home_a) };
    let (r, token) = make_router(&home_a, true, "proactiveCheck");
    rt.block_on(async {
        let (code, v) = call(&r, send(&token, "我主动来冒个泡")).await;
        assert_eq!(code, StatusCode::FORBIDDEN, "观察模式拦截：{v}");
        assert!(
            v.to_string().contains("观察模式"),
            "拒绝文案应引导安静收尾：{v}"
        );
    });
    let lines = audit_lines(&home_a);
    assert_eq!(lines.len(), 1, "拦截也落审计");
    assert!(lines[0].contains("\"blocked\""));
    assert!(lines[0].contains("我主动来冒个泡"));
    {
        // 配额未耗：send_times 仍空（门禁先于频率护栏）。
        let social = SocialStore::load(&home_a.join("state").join("social-v2.json"), None).unwrap();
        assert!(
            social
                .conversation("group:4242")
                .unwrap()
                .send_times
                .is_empty(),
            "被拦的发送不消耗频率配额"
        );
    }

    // ② 正常模式 + 主动回合：不拦、审计 allowed。空文本在空校验处 400
    //（审计先于空校验），借它绕开 OneBot 网络调用（blocking client 在
    // 测试 runtime 内 drop 会 panic）。
    let home_b = temp_home("allow");
    unsafe { std::env::set_var("QUEQIAO_HOME", &home_b) };
    let (r_b, token_b) = make_router(&home_b, false, "proactiveCheck");
    rt.block_on(async {
        let (code, v) = call(&r_b, send(&token_b, "")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "放行后走到空校验：{v}");
    });
    let lines = audit_lines(&home_b);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("\"allowed\""));

    // ③ 非主动回合（atMention 唤醒）：观察模式也不拦、不审计（同样空文本
    // 收在 400，不触网络）。
    let home_c = temp_home("passive");
    unsafe { std::env::set_var("QUEQIAO_HOME", &home_c) };
    let (r_c, token_c) = make_router(&home_c, true, "atMention");
    rt.block_on(async {
        let (code, _) = call(&r_c, send(&token_c, "")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "被动回复不受门禁");
    });
    assert!(audit_lines(&home_c).is_empty(), "passive 不落审计");

    // ④ 控制台查询：无令牌（向导模式）放行带计数；agent token 一律 403。
    unsafe { std::env::set_var("QUEQIAO_HOME", &home_b) };
    rt.block_on(async {
        let req = Request::builder()
            .method("GET")
            .uri("/api/proactive/audit?limit=10")
            .body(Body::empty())
            .unwrap();
        let (code, v) = call(&r_b, req).await;
        assert_eq!(code, StatusCode::OK, "console 查询：{v}");
        assert_eq!(v["value"]["allowed"], json!(1));
        assert_eq!(v["value"]["blocked"], json!(0));
        assert_eq!(v["value"]["observeOnly"], json!(false));

        let req = Request::builder()
            .method("GET")
            .uri("/api/proactive/audit")
            .header("x-agent-token", &token_b)
            .body(Body::empty())
            .unwrap();
        let (code, _) = call(&r_b, req).await;
        assert_eq!(code, StatusCode::FORBIDDEN, "agent token 拒绝");
    });
}
