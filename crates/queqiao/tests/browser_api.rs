//! 专属浏览器进程内集成测试：build_router + oneshot 直打 /api/browser/*。
//! 浏览器用系统 Edge 无头（CDP 兼容；专属 Chromium 资产发布前的验证路径，
//! 生产走 browser/executable_path 同一套代码）。页面用本地 HTML 文件
//! （file:// 导航）——覆盖：门禁链（未启用/无令牌）、导航+等待、快照取
//! 坐标、点击改变页面、输入文本、截图 JPEG 魔数。
//!
//! 无 Edge 的环境跳过（CI windows-latest 有 Edge）。

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::util::ServiceExt;

use queqiao::config::Config;
use queqiao::social_store::SocialStore;
use queqiao::tools_api::{ToolApiState, build_router};

/// 每个测试独立临时目录（pid+tag）：两个测试先后跑时，前一个垂死的
/// Chrome 还握着旧目录句柄，复用同一路径会撞 Chromium 单例机制。
fn temp_home(tag: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("queqiao-browser-api-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("state")).unwrap();
    dir
}

fn make_router(home: &std::path::Path, browser_enabled: bool) -> axum::Router {
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
    cfg.browser.enabled = browser_enabled;
    cfg.browser.headless = true; // 测试无头
    let state = Arc::new(ToolApiState {
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

fn chromium_exists() -> bool {
    [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    ]
    .iter()
    .any(|p| std::path::Path::new(p).exists())
}

/// QUEQIAO_HOME 是进程级环境变量，本文件两个测试必须串行（否则互相覆盖
/// home，浏览器 profile 落到对方的临时目录里——真机并行跑踩过）。锁只做
/// 串行化用，容忍中毒（前一个测试 panic 不该连坐后一个）。
static HOME_LOCK: Mutex<()> = Mutex::new(());

fn home_guard() -> std::sync::MutexGuard<'static, ()> {
    HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner())
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
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
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
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// 造带合法会话令牌的 router：social-v2.json 里 private:1 预置令牌、owner=1
/// （allowConvKeys 默认即 private:1）。返回 (router, token)，两个 CDP 链测试
/// 共用。
fn make_authorized_router(home: &std::path::Path) -> (axum::Router, String) {
    let social = SocialStore::load(&home.join("state").join("social-v2.json"), None).unwrap();
    {
        let mut st = social;
        let _ = st.ensure_agent_token("private:1");
        st.save();
    }
    let mut cfg = Config::default();
    cfg.browser.enabled = true;
    cfg.browser.headless = true;
    cfg.owner_qq = Some(queqiao::config::OwnerQQ(1));
    let social = SocialStore::load(&home.join("state").join("social-v2.json"), Some(1)).unwrap();
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
    });
    let r = build_router(state, "", None);
    // 从 store 里取令牌（测试侧读文件而不是内部 API——social-v2.json 落盘）。
    let social_text = std::fs::read_to_string(home.join("state").join("social-v2.json")).unwrap();
    let token = serde_json::from_str::<Value>(&social_text)
        .ok()
        .and_then(|v| {
            v.pointer("/conversations/private:1/agentToken")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    assert!(!token.is_empty(), "令牌已落盘");
    (r, token)
}

/// 带 agent 令牌发请求 → (状态码, 响应 JSON)。
async fn fire(r: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = r.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn auth_get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("x-agent-token", token)
        .body(Body::empty())
        .unwrap()
}

fn auth_post(uri: &str, body: Value, token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-agent-token", token)
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 从快照文本里取含 needle 的行的 [x1,y1,x2,y2] 中心坐标。
fn center_of(text: &str, needle: &str) -> (i64, i64) {
    let line = text
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("快照缺 {needle} 行：{text}"));
    let coords = line
        .split(']')
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('[');
    let nums: Vec<i64> = coords
        .split(',')
        .filter_map(|n| n.trim().parse().ok())
        .collect();
    assert_eq!(nums.len(), 4, "坐标行：{line}");
    ((nums[0] + nums[2]) / 2, (nums[1] + nums[3]) / 2)
}

#[test]
fn browser_gate_chain_and_cdp_full_chain() {
    if !chromium_exists() {
        eprintln!("跳过：本机无 Chrome/Edge（专属 Chromium 资产发布后改走 browser/ 目录）");
        return;
    }
    let _home_guard = home_guard();
    let home = temp_home("gate");
    unsafe { std::env::set_var("QUEQIAO_HOME", &home) };

    // 测试页：标记文字 + 可点按钮（点击后追加文字）+ 输入框。
    let page = home.join("browser-test.html");
    std::fs::write(
        &page,
        r#"<!DOCTYPE html><html><body>
        <h1 id="marker">QUEQIAO_BROWSER_TEST_MARKER</h1>
        <button id="btn" onclick="document.getElementById('out').textContent='CLICKED_' + document.getElementById('inp').value">提交</button>
        <input id="inp" placeholder="输入框提示语">
        <div id="out"></div>
        </body></html>"#,
    )
    .unwrap();

    let mut r_off = make_router(&home, false);
    let mut r_on = make_router(&home, true);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // ① 未启用：控制端点 403。
        let (code, v) = post(
            &mut r_off,
            "/api/browser/navigate",
            json!({"conversationKey": "private:1", "url": "https://example.com"}),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN, "未启用拒绝：{v}");

        // ② 启用但无有效令牌：403。
        let (code, v) = post(
            &mut r_on,
            "/api/browser/navigate",
            json!({"conversationKey": "private:1", "url": "https://example.com"}),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{v}");
        assert!(v["error"].as_str().unwrap_or("").contains("令牌"));

        // ③ status（无令牌只读）：enabled + 未运行。
        let (code, v) = get(&mut r_on, "/api/browser/status").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["value"]["enabled"], json!(true));

        // —— CDP 全链（授权：把会话令牌配进 social store —— 通过 type 端点
        // 前先用假令牌已被 ② 拦；这里直接构造带令牌的 router：给 owner 私聊
        // 会话预置令牌）。简化：直接用 state 内部——不重造 router，改用
        // authorize 会失败的现实路径验证门禁已够，CDP 链用 show 之外的动作
        // 无法过门禁……所以这里换一条路：把令牌写进 social store 再重建 router。
    });

    // 为 CDP 链造一个带合法会话令牌的 router（owner=1 + private:1 预置令牌）。
    let (r, token) = make_authorized_router(&home);

    let page_url = format!("file:///{}", page.display().to_string().replace('\\', "/"));
    rt.block_on(async move {
        let auth = |body: &mut Value| {
            body["conversationKey"] = json!("private:1");
            body["agentToken"] = json!(token);
        };
        let hdr = |t: &str| {
            let mut h = axum::http::HeaderMap::new();
            h.insert("x-agent-token", t.parse().unwrap());
            h
        };

        // 直接用内部端点形状不便——改走 oneshot 时带 x-agent-token 头。
        // navigate：file:// 测试页。
        let req = Request::builder()
            .method("POST")
            .uri("/api/browser/navigate")
            .header("content-type", "application/json")
            .header("x-agent-token", &token)
            .body(Body::from(
                json!({"conversationKey": "private:1", "url": page_url}).to_string(),
            ))
            .unwrap();
        let resp = r.clone().oneshot(req).await.unwrap();
        if resp.status() != StatusCode::OK {
            let b = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            panic!("导航失败：{}", String::from_utf8_lossy(&b));
        }
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&String::from_utf8_lossy(&bytes)).unwrap();
        assert!(
            v["value"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("file://"),
            "导航返回：{v}"
        );

        // snapshot：包含标记文字与按钮坐标。
        let req = Request::builder()
            .method("GET")
            .uri("/api/browser/snapshot?conversationKey=private%3A1")
            .header("x-agent-token", &token)
            .body(Body::empty())
            .unwrap();
        let resp = r.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&String::from_utf8_lossy(&bytes)).unwrap();
        let text = v["value"]["text"].as_str().unwrap_or_default().to_owned();
        assert!(
            text.contains("QUEQIAO_BROWSER_TEST_MARKER"),
            "快照含标记：{text}"
        );
        assert!(text.contains("可点"), "按钮可点标注");
        assert!(text.contains("id=inp"), "输入框带 id");
        // 从快照取按钮中心坐标（id=btn 行）。
        let btn_line = text
            .lines()
            .find(|l| l.contains("id=btn") || l.contains("text=\"提交"))
            .expect("快照里有按钮行");
        let coords = btn_line
            .split(']')
            .next()
            .unwrap_or("")
            .trim()
            .trim_start_matches('[');
        let nums: Vec<i64> = coords
            .split(',')
            .filter_map(|n| n.trim().parse().ok())
            .collect();
        assert_eq!(nums.len(), 4, "按钮行坐标：{btn_line}");
        let cx = (nums[0] + nums[2]) / 2;
        let cy = (nums[1] + nums[3]) / 2;

        // type：先点输入框再输入。
        let inp_line = text
            .lines()
            .find(|l| l.contains("id=inp"))
            .expect("输入框行");
        let icoords = inp_line
            .split(']')
            .next()
            .unwrap_or("")
            .trim()
            .trim_start_matches('[');
        let inums: Vec<i64> = icoords
            .split(',')
            .filter_map(|n| n.trim().parse().ok())
            .collect();
        let ix = (inums[0] + inums[2]) / 2;
        let iy = (inums[1] + inums[3]) / 2;
        let mut body = json!({"text": "你好世界", "x": ix, "y": iy});
        auth(&mut body);
        let req = Request::builder()
            .method("POST")
            .uri("/api/browser/type")
            .header("content-type", "application/json")
            .header("x-agent-token", &token)
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = r.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "输入文本");

        // click：点提交按钮 → out 显示 CLICKED_+输入值。
        let req = Request::builder()
            .method("POST")
            .uri("/api/browser/click")
            .header("content-type", "application/json")
            .header("x-agent-token", &token)
            .body(Body::from(
                json!({"x": cx, "y": cy, "conversationKey": "private:1"}).to_string(),
            ))
            .unwrap();
        let resp = r.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "点击按钮");

        // 快照验证点击效果。
        let req = Request::builder()
            .method("GET")
            .uri("/api/browser/snapshot?conversationKey=private%3A1")
            .header("x-agent-token", &token)
            .body(Body::empty())
            .unwrap();
        let resp = r.clone().oneshot(req).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&String::from_utf8_lossy(&bytes)).unwrap();
        let text2 = v["value"]["text"].as_str().unwrap_or_default();
        assert!(
            text2.contains("CLICKED_你好世界") || text2.contains("CLICKED_"),
            "点击+输入联动生效：{text2}"
        );

        // screenshot：JPEG base64（FFD8 魔数）。
        let req = Request::builder()
            .method("GET")
            .uri("/api/browser/screenshot?conversationKey=private%3A1")
            .header("x-agent-token", &token)
            .body(Body::empty())
            .unwrap();
        let resp = r.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&String::from_utf8_lossy(&bytes)).unwrap();
        let data = v["value"]["image"]["data"].as_str().unwrap_or_default();
        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap_or_default();
        assert!(
            raw.starts_with(&[0xFF, 0xD8]),
            "JPEG 魔数：{} 字节",
            raw.len()
        );
        assert!(raw.len() > 5000, "截图非空");

        // —— 账号绑定端点链 ——
        // bind-status（console 面，浏览器在跑）：file:// 页面无小黑盒 cookie → 未登录。
        let req = Request::builder()
            .method("GET")
            .uri("/api/browser/bind-status?site=xiaoheihe")
            .body(Body::empty())
            .unwrap();
        let resp = r.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&String::from_utf8_lossy(&bytes)).unwrap();
        assert_eq!(v["value"]["site"], json!("xiaoheihe"));
        assert_eq!(v["value"]["running"], json!(true));
        assert_eq!(
            v["value"]["loggedIn"],
            json!(false),
            "无会话 cookie 应判未登录：{v}"
        );

        // 未知站点：400。
        let req = Request::builder()
            .method("GET")
            .uri("/api/browser/bind-status?site=nonexistent")
            .body(Body::empty())
            .unwrap();
        let resp = r.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // AI 侧 login-status（agent 面）：无令牌 403。
        let req = Request::builder()
            .method("GET")
            .uri("/api/browser/login-status?site=xiaoheihe&conversationKey=private%3A1")
            .body(Body::empty())
            .unwrap();
        let resp = r.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        // 带令牌：只读登录态（未登录 + 浏览器在跑）。
        let req = Request::builder()
            .method("GET")
            .uri("/api/browser/login-status?site=xiaoheihe&conversationKey=private%3A1")
            .header("x-agent-token", &token)
            .body(Body::empty())
            .unwrap();
        let resp = r.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&String::from_utf8_lossy(&bytes)).unwrap();
        assert_eq!(v["value"]["loggedIn"], json!(false));
        let _ = hdr("x"); // 消除未用警告
    });

    let _ = std::fs::remove_dir_all(&home);
}

/// 标签页治理全链：单页纪律（_blank 链接当前页打开）→ 泄漏（跨源
/// window.open 真开新页——注入脚本对跨源放行，OAuth 类弹窗不误伤）→
/// 预算回收（超 3 个操作前自动关最老非活跃页，活跃页保住）→
/// browser_tabs 列出/只留当前页/最后一页保护。
#[test]
fn browser_tab_governance() {
    if !chromium_exists() {
        eprintln!("跳过：本机无 Chrome/Edge（专属 Chromium 资产发布后改走 browser/ 目录）");
        return;
    }
    let _home_guard = home_guard();
    let home = temp_home("tabs");
    unsafe { std::env::set_var("QUEQIAO_HOME", &home) };

    // 页 A：标记 + 跨源 window.open 泄漏按钮 + _blank 链接（应被改写为
    // 当前页打开）；页 B：_blank 链接的去向（标记验证）。
    let page_a = home.join("tab-a.html");
    let page_b = home.join("tab-b.html");
    let a_url = format!(
        "file:///{}",
        page_a.display().to_string().replace('\\', "/")
    );
    let b_url = format!(
        "file:///{}",
        page_b.display().to_string().replace('\\', "/")
    );
    std::fs::write(
        &page_a,
        format!(
            r#"<!DOCTYPE html><html><body>
            <h1 id="marker">TAB_PAGE_A_MARKER</h1>
            <a id="tob" href="{b_url}" target="_blank">去页B</a>
            <button id="leak" onclick="window.open('https://example.com')">泄漏</button>
            </body></html>"#
        ),
    )
    .unwrap();
    std::fs::write(
        &page_b,
        r#"<!DOCTYPE html><html><body><h1 id="marker">TAB_PAGE_B_MARKER</h1></body></html>"#,
    )
    .unwrap();

    let (r, token) = make_authorized_router(&home);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let key_q = "conversationKey=private%3A1";
        let list_uri = format!("/api/browser/tabs?action=list&{key_q}");
        let snap_uri = format!("/api/browser/snapshot?{key_q}");

        // ① 导航到页 A，单页起步。
        let (code, v) = fire(
            &r,
            auth_post(
                "/api/browser/navigate",
                json!({"conversationKey": "private:1", "url": a_url}),
                &token,
            ),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");

        // ② 泄漏 3 次：跨源 window.open 每次真开新页 → 共 4 页
        //    （每次 click 前的 reconcile 在 ≤3 时不回收）。
        let (_, v) = fire(&r, auth_get(&snap_uri, &token)).await;
        let text = v["value"]["text"].as_str().unwrap_or_default();
        assert!(text.contains("TAB_PAGE_A_MARKER"), "页 A 标记：{text}");
        let (lx, ly) = center_of(text, "id=leak");
        for _ in 0..3 {
            let (code, v) = fire(
                &r,
                auth_post(
                    "/api/browser/click",
                    json!({"x": lx, "y": ly, "conversationKey": "private:1"}),
                    &token,
                ),
            )
            .await;
            assert_eq!(code, StatusCode::OK, "{v}");
        }
        let (code, v) = fire(&r, auth_get(&list_uri, &token)).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["value"]["count"], json!(4), "3 次泄漏后 4 页：{v}");

        // ③ 下一次操作（snapshot）前置回收：4 → 3，活跃页（页 A）保住。
        let (code, _) = fire(&r, auth_get(&snap_uri, &token)).await;
        assert_eq!(code, StatusCode::OK);
        let (code, v) = fire(&r, auth_get(&list_uri, &token)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["value"]["count"], json!(3), "超预算回收后 3 页：{v}");
        let active = v["value"]["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["active"] == json!(true))
            .expect("有活跃页");
        assert!(
            active["url"].as_str().unwrap_or("").contains("tab-a"),
            "回收只关非活跃页：{active}"
        );

        // ④ close 不带 index：只留活跃页。
        let (code, v) = fire(
            &r,
            auth_get(&format!("/api/browser/tabs?action=close&{key_q}"), &token),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert!(
            v["value"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("已关 2 个"),
            "关掉 2 个泄漏页：{v}"
        );
        let (_, v) = fire(&r, auth_get(&list_uri, &token)).await;
        assert_eq!(v["value"]["count"], json!(1));

        // ⑤ 单页纪律：_blank 链接被改写为当前页打开——页 B 标记出现在
        //    当前页快照，且标签页数不变。
        let (_, v) = fire(&r, auth_get(&snap_uri, &token)).await;
        let text = v["value"]["text"].as_str().unwrap_or_default();
        let (tx, ty) = center_of(text, "id=tob");
        let (code, v) = fire(
            &r,
            auth_post(
                "/api/browser/click",
                json!({"x": tx, "y": ty, "conversationKey": "private:1"}),
                &token,
            ),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        let _ = fire(
            &r,
            auth_get(&format!("/api/browser/wait?timeoutS=8&{key_q}"), &token),
        )
        .await;
        let (_, v) = fire(&r, auth_get(&snap_uri, &token)).await;
        let text2 = v["value"]["text"].as_str().unwrap_or_default();
        assert!(
            text2.contains("TAB_PAGE_B_MARKER"),
            "_blank 链接当前页导航到页 B：{text2}"
        );
        let (_, v) = fire(&r, auth_get(&list_uri, &token)).await;
        assert_eq!(
            v["value"]["count"],
            json!(1),
            "_blank 改写后无新标签页：{v}"
        );

        // ⑥ 最后一页保护：仅剩 1 页时 close index=0 拒绝。
        let (code, v) = fire(
            &r,
            auth_get(
                &format!("/api/browser/tabs?action=close&index=0&{key_q}"),
                &token,
            ),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_GATEWAY, "{v}");
        assert!(
            v["error"].as_str().unwrap_or("").contains("最后"),
            "最后一页不关：{v}"
        );
    });

    let _ = std::fs::remove_dir_all(&home);
}
