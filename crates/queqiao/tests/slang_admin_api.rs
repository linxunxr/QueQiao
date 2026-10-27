//! 黑话词库管理面（/api/slang*）进程内集成测试：build_router + oneshot
//! 直打端点，不占端口、不依赖 SnowLuma/DSH。全链语义对拍 Node
//! bridge.js /api/slang* 八端点（列表/新增/编辑/批量确认/批量拒绝/
//! 单条捷径/批量删除/清空 + agent token 拒绝）。

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
    let dir = std::env::temp_dir().join(format!("queqiao-slang-api-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("state")).unwrap();
    dir
}

fn make_router(home: &std::path::Path) -> Router {
    let social = SocialStore::load(&home.join("state").join("social-v2.json"), None).unwrap();
    let (update_tx, _rx) = tokio::sync::mpsc::channel(1);
    let dsh = queqiao_dsh::client::DshClient::new(
        "http://127.0.0.1:1",
        queqiao_dsh::client::ReqwestTransport::new(50),
        queqiao_dsh::client::AuthSources {
            launch_token: String::new(),
            token_explicit: false,
            secret: Box::new(|| None),
            rediscover_token: None,
        },
        false,
    );
    let state = Arc::new(ToolApiState {
        social: Arc::new(Mutex::new(social)),
        onebot: queqiao_onebot::client::OneBotHttpClient::new("http://127.0.0.1:1", "", 50),
        cfg: Config::default(),
        dsh: Arc::new(Mutex::new(dsh)),
        sessions: Arc::new(Mutex::new(
            queqiao::session_manager::SessionManager::new(
                Arc::new(Mutex::new(queqiao_dsh::client::DshClient::new(
                    "http://127.0.0.1:1",
                    queqiao_dsh::client::ReqwestTransport::new(50),
                    queqiao_dsh::client::AuthSources {
                        launch_token: String::new(),
                        token_explicit: false,
                        secret: Box::new(|| None),
                        rediscover_token: None,
                    },
                    false,
                ))),
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

async fn call(
    router: &mut Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let req = match body {
        Some(b) => req.body(Body::from(b.to_string())).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    (status, value)
}

#[test]
fn slang_admin_full_lifecycle() {
    let home = temp_home();
    // edition 2024：set_var unsafe；本测试二进制仅此一个测试，无并发竞争。
    unsafe { std::env::set_var("QUEQIAO_HOME", &home) };
    // 状态构造必须在同步上下文：reqwest blocking client 的 build 在 async
    // 上下文里会建临时 runtime 并 panic（"reqwest blocking 不嵌 tokio"同坑）。
    let mut r = make_router(&home);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // ① 新增：默认 confirmed（手动添加入库即注入）；重复词条拒绝。
        let (code, v) = call(
            &mut r,
            "POST",
            "/api/slang",
            Some(json!({"content": "绝绝子", "meaning": "很好、很棒"})),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let confirmed_id = v["value"]["entry"]["id"].as_str().unwrap().to_owned();
        assert_eq!(v["value"]["entry"]["status"], "confirmed");
        let (code, v) = call(
            &mut r,
            "POST",
            "/api/slang",
            Some(json!({"content": "yyds", "meaning": "", "status": "candidate"})),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let cand_id = v["value"]["entry"]["id"].as_str().unwrap().to_owned();

        let (code, v) = call(
            &mut r,
            "POST",
            "/api/slang",
            Some(json!({"content": "绝绝子", "meaning": "重复"})),
        )
        .await;
        assert_eq!(
            code,
            StatusCode::BAD_REQUEST,
            "重复词条拒绝：{}",
            v["error"]
        );

        // ② agent token 一律 403（控制台专用面）。
        let req = Request::builder()
            .method("GET")
            .uri("/api/slang")
            .header("x-agent-token", "whatever")
            .body(Body::empty())
            .unwrap();
        let resp = r.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "agent token 拒绝");

        // ③ 列表：全量 + 按 status 过滤；配置随行。
        let (code, v) = call(&mut r, "GET", "/api/slang", None).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["value"]["entries"].as_array().unwrap().len(), 2);
        assert_eq!(v["value"]["config"]["injectMax"], 8);
        let (code, v) = call(&mut r, "GET", "/api/slang?status=candidate", None).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["value"]["entries"].as_array().unwrap().len(), 1);

        // ④ 批量确认：缺含义跳过并给原因。
        let (code, v) = call(
            &mut r,
            "POST",
            "/api/slang/batch-confirm",
            Some(json!({"ids": [cand_id]})),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["value"]["confirmedCount"], 0);
        assert_eq!(v["value"]["skipped"][0]["reason"], "缺少含义");

        // ⑤ 编辑补含义（PATCH）后再确认 → 转正。
        let (code, v) = call(
            &mut r,
            "PATCH",
            &format!("/api/slang/{cand_id}"),
            Some(json!({"meaning": "永远的神", "usage": "夸人"})),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["value"]["entry"]["meaning"], "永远的神");
        let (code, v) = call(
            &mut r,
            "POST",
            "/api/slang/batch-confirm",
            Some(json!({"ids": [cand_id]})),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["value"]["confirmedCount"], 1);

        // ⑥ 单条拒绝 → 恢复候选（PATCH status）→ 单条确认捷径。
        let (code, _) = call(
            &mut r,
            "POST",
            &format!("/api/slang/{cand_id}/reject"),
            None,
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let (code, v) = call(
            &mut r,
            "PATCH",
            &format!("/api/slang/{cand_id}"),
            Some(json!({"status": "candidate"})),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["value"]["entry"]["status"], "candidate");
        let (code, _) = call(
            &mut r,
            "POST",
            &format!("/api/slang/{cand_id}/confirm"),
            None,
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        // 非 candidate 不能走确认捷径（单条捷径复用批量语义）。
        let (code, _) = call(
            &mut r,
            "POST",
            &format!("/api/slang/{cand_id}/confirm"),
            None,
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);

        // ⑦ 编辑与他人 content 冲突拒绝；不存在的词条 404。
        let (code, _) = call(
            &mut r,
            "PATCH",
            &format!("/api/slang/{cand_id}"),
            Some(json!({"content": "绝绝子"})),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        let (code, _) = call(
            &mut r,
            "PATCH",
            "/api/slang/no-such",
            Some(json!({"meaning": "x"})),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);

        // ⑧ 批量删除 + 按 status 清空 + 全清。
        let (code, v) = call(
            &mut r,
            "POST",
            "/api/slang/batch-delete",
            Some(json!({"ids": [confirmed_id]})),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["value"]["removedCount"], 1);
        let (code, v) = call(
            &mut r,
            "POST",
            "/api/slang/clear",
            Some(json!({"status": "confirmed"})),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["value"]["removedCount"], 1, "清掉剩余 confirmed（yyds）");
        let (_, v) = call(&mut r, "GET", "/api/slang", None).await;
        assert_eq!(v["value"]["entries"].as_array().unwrap().len(), 0);

        // ⑨ 落盘与内存一致：全链操作后 state/slang.json 应为空库（删除/清空已生效）。
        let text = std::fs::read_to_string(home.join("state").join("slang.json")).unwrap();
        let entries_now: Vec<Value> = serde_json::from_str(&text).unwrap_or_default();
        assert!(entries_now.is_empty(), "最终库应为空，实际 {text}");
    }); // block_on 结束：状态在同步上下文析构，无 runtime 嵌套 panic。
    let _ = std::fs::remove_dir_all(&home);
}
