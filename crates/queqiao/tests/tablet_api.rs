//! 平板控制端点进程内集成测试：build_router + oneshot 直打，不占端口。
//! status 用真实 hdc（本机 DevEco 自带，设备可离线——探测语义本就允许）；
//! 控制类端点验证门禁链（未启用→403；未授权令牌→403）。
//!
//! 状态构造须在同步上下文（reqwest blocking client 在 async 上下文构造/
//! 析构会 panic——slang_admin_api 同款坑，同款处理）。

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::util::ServiceExt;

use queqiao::config::Config;
use queqiao::social_store::SocialStore;
use queqiao::tools_api::{ToolApiState, build_router};

fn temp_home() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("queqiao-tablet-api-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("state")).unwrap();
    dir
}

fn make_router(home: &std::path::Path, tablet_enabled: bool) -> axum::Router {
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
    let mut cfg = Config::default();
    cfg.tablet.enabled = tablet_enabled;
    let state = Arc::new(ToolApiState {
        diary_pending: std::sync::Arc::new(std::sync::Mutex::new(None)),
        social: Arc::new(Mutex::new(social)),
        onebot: queqiao_onebot::client::OneBotHttpClient::new("http://127.0.0.1:1", "", 50),
        cfg,
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

async fn get(router: &mut axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    let value = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, value)
}

async fn post(router: &mut axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    let value = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, value)
}

#[test]
fn tablet_gate_chain_and_status_probe() {
    let home = temp_home();
    unsafe { std::env::set_var("QUEQIAO_HOME", &home) };

    // 同步上下文构造（reqwest blocking client 限制）。
    let mut r_off = make_router(&home, false);
    let mut r_on = make_router(&home, true);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // ① 未启用：控制端点一律 403（明确提示）。
        let (code, v) = post(
            &mut r_off,
            "/api/tablet/tap",
            json!({"conversationKey": "private:1", "x": 1, "y": 2}),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN, "未启用拒绝：{}", v);
        assert!(
            v["error"].as_str().unwrap_or("").contains("未启用"),
            "错误信息指向配置项"
        );

        // ② 启用但无有效令牌：403（agent token 与会话不匹配）。
        let (code, v) = post(
            &mut r_on,
            "/api/tablet/tap",
            json!({"conversationKey": "private:1", "x": 1, "y": 2}),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert!(
            v["error"].as_str().unwrap_or("").contains("令牌"),
            "错误指向令牌：{}",
            v
        );

        // ③ status 不需要令牌：探测真实 hdc（设备可离线——online:false + 提示）。
        let (code, v) = get(&mut r_on, "/api/tablet/status").await;
        assert_eq!(code, StatusCode::OK, "{}", v);
        let value = &v["value"];
        assert_eq!(value["enabled"], json!(true));
        if value["online"].as_bool() != Some(true) {
            // 离线也要给出可操作提示（设备连接指引）。
            assert!(
                value["hint"].as_str().is_some_and(|h| !h.is_empty()),
                "离线提示：{value}"
            );
        } else {
            // 真机在线（测试机恰有设备连接时）：型号字段存在。
            assert!(value["device"]["model"].as_str().is_some());
        }
    }); // block_on 结束：状态在同步上下文析构。

    let _ = std::fs::remove_dir_all(&home);
}
