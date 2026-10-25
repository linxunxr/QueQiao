//! 鹊桥（QueQiao）—— QQ ↔ DeepSeek Harness 桥接主程序。
//!
//! 当前形态（M4 编排批次）：OneBot 事件流 → core 决策（Orchestrator）→ 动作
//! 执行。SendPrompt 的 DSH 投递全链（会话管理/prompt 构造）与控制台、看门狗
//! 在后续批次接入——当前动作以结构化日志输出（决策闭环已可观测）。
//!
//! Node 版（qq-bridge）是权威参照实现：行为契约以其 43 个 audit 脚本为准。

use std::collections::HashSet;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::time::{SystemTime, UNIX_EPOCH};

use queqiao_core::action::{BridgeAction, BridgeEvent};
use queqiao_core::diary::CivilTime;
use queqiao_core::runtime::{DecideRules, DiaryRules};
use queqiao_core::triggers::TriggerRules;
use queqiao_dsh::client::{AuthSources, DshClient, ReqwestTransport};
use queqiao_dsh::port::{discover_dsh_base_url, encoded_port_scan_command};
use queqiao_onebot::client::OneBotHttpClient;
use queqiao_onebot::conn::{
    AccountEndpoints, LoginProbe, discover_connection, parse_account_config,
};
use queqiao_onebot::event::{MessageBody, OneBotEvent, extract_plain_text};
use queqiao_onebot::ws::EventStream;

use queqiao::config::ConfigFile;
use queqiao::orchestrator::Orchestrator;
use queqiao::update;

/// 消息是否 @ 了 bot（at 段 qq 匹配——self_id 本批从 login_info 取，简化为
/// 任意 at 段都视为 @ 自己之外的目标：at_me 需要精确 self_id，接线批次补全）。
fn now_local_str(_now_ms: u64) -> String {
    // kernel32 GetLocalTime（非 Windows 回落毫秒数）。
    queqiao::logging::local_timestamp().unwrap_or_else(|| _now_ms.to_string())
}

fn at_self(body: &MessageBody) -> bool {
    match body {
        MessageBody::Array(segments) => segments.iter().any(|s| {
            s.get("type").and_then(|t| t.as_str()) == Some("at")
                && s.get("data").and_then(|d| d.get("qq")).is_some()
        }),
        MessageBody::Text(_) => false,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// 本地民用时间（时区换算——本机当前时区取系统本地时间的小时/分钟）。
fn now_local() -> CivilTime {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // 简化：用 UTC+8（部署环境为中国时区）。完整时区库（chrono-tz）在控制台
    // 批次按需引入。
    let secs_per_day = 86_400;
    let days = now / secs_per_day;
    let secs_today = (now % secs_per_day + 8 * 3600) % secs_per_day;
    let hour = (secs_today / 3600) as u8;
    let minute = ((secs_today % 3600) / 60) as u8;
    // 自 1970-01-01 起的天数转 civil（无时区库的平年近似——2026 年窗口内准确）。
    let mut year = 1970i32;
    let mut remaining = days;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if remaining >= len {
            remaining -= len;
            year += 1;
        } else {
            break;
        }
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_lengths = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u8;
    let mut day = remaining as u32;
    for len in month_lengths {
        if day < len {
            break;
        }
        day -= len;
        month += 1;
    }
    CivilTime {
        date: queqiao_core::diary::CivilDate {
            year,
            month,
            day: day as u8 + 1,
        },
        hour,
        minute,
    }
}

/// 读 SnowLuma per-account 配置（token 自发现的数据源）。
fn read_accounts(home: &std::path::Path) -> Vec<AccountEndpoints> {
    let mut accounts = Vec::new();
    let Ok(entries) = std::fs::read_dir(home.join("config")) else {
        return accounts;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(uin) = queqiao_onebot::conn::account_file_uin(&name) else {
            continue;
        };
        if let Ok(text) = std::fs::read_to_string(entry.path())
            && let Some(account) = parse_account_config(uin, &text)
        {
            accounts.push(account);
        }
    }
    accounts.sort_by(|a, b| a.uin.cmp(&b.uin));
    accounts
}

struct HttpProbe;

impl LoginProbe for HttpProbe {
    fn probe(&mut self, base_url: &str, token: &str) -> Option<serde_json::Value> {
        OneBotHttpClient::new(base_url, token, 5000)
            .get_login_info()
            .ok()
    }
}

/// PowerShell 扫描 DSH 端口（复用 M2 的脚本常量）。
fn scan_dsh_ports() -> Vec<u16> {
    let Ok(out) = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-EncodedCommand",
            &encoded_port_scan_command(),
        ])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|t| t.parse::<u16>().ok())
        .filter(|p| *p > 0)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

/// 运行中重探测 DSH 基址：DSH 重启端口常态漂移（3080→6411→11831→10690，
/// 2026-10 真机连漂三次），mux 重连失败时调用——同启动期链（config 基址 →
/// 扫进程端口逐个 verify）。阻塞（powershell 扫描秒级），须 spawn_blocking。
fn rediscover_dsh_base(cfg_base: &str) -> Option<String> {
    let yaml = std::env::var("USERPROFILE")
        .ok()
        .map(|h| {
            std::path::PathBuf::from(h)
                .join(".dsh")
                .join(".credentials.yaml")
        })
        .and_then(|p| std::fs::read_to_string(p).ok())?;
    let secret = queqiao_dsh::auth::parse_browser_session_secret(&yaml)?;
    let dsh_ok = |base: &str| {
        let sources = AuthSources {
            launch_token: String::new(),
            token_explicit: false,
            secret: Box::new(move || Some(secret)),
            rediscover_token: None,
        };
        let mut client = DshClient::new(base, ReqwestTransport::new(5000), sources, false);
        client
            .call_unary("settings/describe", None, now_ms())
            .is_ok()
    };
    let ports = scan_dsh_ports();
    discover_dsh_base_url(&ports, cfg_base, dsh_ok)
}

/// mux 重连失败路径共用：探测 DSH 新基址，漂移则切换共享 client
/// （后续 HTTP 投递/mux 连接自动跟随；未漂移或探测不到则不动）。
async fn switch_if_drifted(
    dsh: &std::sync::Arc<std::sync::Mutex<DshClient<ReqwestTransport>>>,
    cfg_base: &str,
) {
    let cfg_base = cfg_base.to_owned();
    let found = tokio::task::spawn_blocking(move || rediscover_dsh_base(&cfg_base))
        .await
        .ok()
        .flatten();
    if let Some(nb) = found
        && let Ok(mut c) = dsh.lock()
        && c.base_url() != nb
    {
        queqiao::logging::log(&format!("DSH 已漂移：{} -> {nb}", c.base_url()));
        c.set_base_url(&nb);
    }
}

fn main() {
    let cfg_path = queqiao::paths::config_path();
    let first_run = !cfg_path.exists();
    let cfg = if first_run {
        // 首次运行：起设置向导（控制台 + 自动开浏览器），保存后继续启动。
        let console_state = std::sync::Arc::new(queqiao::console::ConsoleState {
            config_path: cfg_path.clone(),
            current: std::sync::Mutex::new(queqiao::config::Config::default()),
            first_run: std::sync::atomic::AtomicBool::new(true),
        });
        let port = std::env::var("QUEQIAO_CONSOLE_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(3100);
        {
            let cs = console_state.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().expect("setup runtime");
                rt.block_on(async move {
                    // 向导模式：仅控制台路由（ToolApiState 依赖的连接发现还没跑）。
                    let app = axum::Router::new()
                        .merge(queqiao::console::router(cs))
                        .fallback(queqiao::console::router_fallback);
                    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                        .await
                        .expect("绑定控制台端口失败（3100 被占用？设 QUEQIAO_CONSOLE_PORT 换口）");
                    println!("首次使用：请在浏览器完成初始设置 → http://127.0.0.1:{port}");
                    queqiao::console::open_browser(&format!("http://127.0.0.1:{port}"));
                    let _ = axum::serve(listener, app).await;
                });
            });
        }
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if !console_state
                .first_run
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        ConfigFile::load(&cfg_path).expect("首次保存后的 config 加载失败")
    } else {
        match ConfigFile::load(&cfg_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("配置加载失败：{e}（{}）", cfg_path.display());
                eprintln!("修正后重试，或删除该文件重新进入首次设置向导。");
                std::process::exit(1);
            }
        }
    };
    let cfg = cfg.config().clone();
    queqiao::logging::init(&queqiao::paths::queqiao_home());
    queqiao::logging::install_panic_hook();
    println!(
        "{} {} — 鹊桥主程序（M4 编排：OneBot 事件 → core 决策 → 动作日志）",
        queqiao_core::APP_NAME,
        env!("CARGO_PKG_VERSION")
    );

    // 发现段全部同步（reqwest blocking 不嵌 tokio——M2/M3 同款约束）。
    // SnowLuma 缺位等待重试（30s 一轮，无限——升级重启/开机自启常早于它；
    // 期间日志可见，就绪后自动进入决策循环）。
    let setup = loop {
        match discover(cfg.clone()) {
            Some(s) => break s,
            None => {
                queqiao::logging::log(
                    "SnowLuma 未就绪（未运行或 OneBot 未开启），30 秒后重试——就绪后自动继续",
                );
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        }
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run(setup));
}

/// 启动期同步发现：OneBot 连接 + DSH 基址（含端口自适应）。
struct Setup {
    ws_url: String,
    ws_token: String,
    /// 同步 discover 阶段构建（reqwest blocking client 不得在 async
    /// 上下文创建——runtime-in-runtime panic）。
    onebot_http: OneBotHttpClient,
    cfg: queqiao::config::Config,
    dsh: std::sync::Arc<std::sync::Mutex<DshClient<ReqwestTransport>>>,
    sessions: std::sync::Arc<std::sync::Mutex<queqiao::session_manager::SessionManager>>,
    social: std::sync::Arc<std::sync::Mutex<queqiao::social_store::SocialStore>>,
}

fn discover(cfg: queqiao::config::Config) -> Option<Setup> {
    let home = std::path::PathBuf::from(&cfg.snowluma.home_dir);
    let accounts = read_accounts(&home);
    println!("SnowLuma 账号配置：{} 份", accounts.len());
    let mut probe = HttpProbe;
    let Ok(found) = discover_connection(
        &mut probe,
        Some(&cfg.snowluma.access_token),
        &cfg.snowluma.http_url,
        &accounts,
    ) else {
        // SnowLuma 未就绪不是致命错（升级重启/开机自启常早于它）：调用方
        // 带间隔重试，就绪后自动进入决策循环——检查更新等控制台能力不受影响。
        return None;
    };
    println!(
        "OneBot 已发现：账号 {}（{}）via {}",
        found
            .self_info
            .get("user_id")
            .and_then(|v| v.as_u64())
            .unwrap_or_default(),
        found
            .self_info
            .get("nickname")
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
        found.source
    );

    // DSH 客户端（铸 Cookie + 端口自适应）与 DSH 会话管理器。
    let secret = std::env::var("USERPROFILE")
        .ok()
        .map(|p| std::path::PathBuf::from(p).join(".dsh"))
        .and_then(|home| std::fs::read_to_string(home.join(".credentials.yaml")).ok())
        .and_then(|text| queqiao_dsh::auth::parse_browser_session_secret(&text));
    let dsh_ok = |base: &str| {
        let Some(secret) = secret else { return false };
        let sources = AuthSources {
            launch_token: String::new(),
            token_explicit: false,
            secret: Box::new(move || Some(secret)),
            rediscover_token: None,
        };
        let mut client = DshClient::new(base, ReqwestTransport::new(5000), sources, false);
        client
            .call_unary("settings/describe", None, now_ms())
            .is_ok()
    };
    let dsh_base = if dsh_ok(&cfg.dsh.base_url) {
        cfg.dsh.base_url.clone()
    } else {
        let ports = scan_dsh_ports();
        discover_dsh_base_url(&ports, &cfg.dsh.base_url, dsh_ok)
            .unwrap_or_else(|| cfg.dsh.base_url.clone())
    };
    println!("DSH 基址：{dsh_base}");

    let secret_for_client = secret;
    let sources = AuthSources {
        launch_token: cfg.dsh.auth_token.clone(),
        token_explicit: !cfg.dsh.auth_token.is_empty(),
        secret: Box::new(move || secret_for_client),
        rediscover_token: None,
    };
    let dsh = std::sync::Arc::new(std::sync::Mutex::new(DshClient::new(
        &dsh_base,
        ReqwestTransport::new(30_000),
        sources,
        cfg.dsh.allow_remote,
    )));
    let agent_dir = if cfg.session_cwd.is_empty() {
        "state/agents".to_owned()
    } else {
        cfg.session_cwd.clone()
    };
    let preset = cfg.social_v2.agent_preset.clone();
    let sessions = std::sync::Arc::new(std::sync::Mutex::new(
        queqiao::session_manager::SessionManager::new(
            dsh.clone(),
            &queqiao::paths::sessions_path(),
            &preset,
            &cfg.workspace_title,
            &agent_dir,
        )
        .expect("sessions.json 加载失败"),
    ));
    {
        let d = &cfg.dsh;
        if let Ok(mut m) = sessions.lock() {
            m.set_dsh_model(&d.provider, &d.model, &d.reasoning_effort);
        }
    }

    let social = std::sync::Arc::new(std::sync::Mutex::new(
        queqiao::social_store::SocialStore::load(
            &queqiao::paths::social_v2_path(),
            cfg.owner_qq.map(|o| o.0),
        )
        .expect("social-v2.json 加载失败"),
    ));

    // 账号开了 OneBot HTTP 但没开 WS（或配置缺 wsServers）：与 SnowLuma 未就绪
    // 同路等待重试——用户在 SnowLuma 里补开 WS 后自动继续，不 panic。
    let ws = found.ws.as_ref()?;
    Some(Setup {
        ws_url: format!("ws://{}:{}", ws.host, ws.port),
        ws_token: ws.token.clone(),
        onebot_http: OneBotHttpClient::new(&found.base_url, &found.token, 8000),
        cfg,
        dsh,
        sessions,
        social,
    })
}

async fn run(setup: Setup) {
    let cfg = setup.cfg;
    let ws_url = setup.ws_url;
    let dsh_arc = setup.dsh;
    let sessions_arc = setup.sessions;
    let onebot_http = setup.onebot_http;
    let social_arc = setup.social;
    let stream = EventStream::connect(&ws_url, &setup.ws_token)
        .await
        .expect("OneBot WS 连接失败（token 失效？重跑或检查 per-account 配置）");
    queqiao::logging::log(&format!("OneBot WS 已连接 {ws_url}"));
    let social_for_stream = social_arc.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<BridgeEvent>(256);
    let ws_url_rc = ws_url.clone();
    let ws_token_rc = setup.ws_token.clone();
    tokio::spawn(async move {
        let mut stream = stream;
        let social = social_for_stream;
        // 断连重连：recv_event 返回 None 即 WS 断开——曾就此静默退出任务，
        // 桥变植物人（控制台活着、消息无人收；2026-10-24 真机 SnowLuma
        // 抖动断链，此后所有消息石沉大海且无任何日志）。5s 起退避重连，
        // 翻倍封顶 60s，恢复为止。
        loop {
            while let Some(event) = stream.recv_event().await {
                let bridged = match event {
                    Ok(OneBotEvent::GroupMessage(m)) if !m.sent_by_self => {
                        let key = format!("group:{}", m.group_id);
                        let mut incoming = queqiao_core::action::IncomingMessage {
                            key: key.clone(),
                            seq: 0, // seq 由持久水位分配（见下方 append_message）
                            message_id: m.message_id.to_string(),
                            sender: m.sender.card.clone().unwrap_or(m.sender.nickname.clone()),
                            user_id: m.user_id.to_string(),
                            text: extract_plain_text(&m.message),
                            is_self: m.sent_by_self,
                            at_me: at_self(&m.message),
                            reply_to_me: false,
                            time_ms: now_ms(),
                        };
                        incoming.seq = social
                            .lock()
                            .map(|mut s| s.append_message(&key, &incoming, now_ms()))
                            .unwrap_or(0);
                        if !incoming.is_self {
                            queqiao::logging::activity(&format!(
                                "{key} 消息入库：{}（{}）",
                                incoming.sender,
                                incoming.text.chars().take(60).collect::<String>()
                            ));
                        }
                        Some(BridgeEvent::MessageArrived(Box::new(incoming)))
                    }
                    Ok(OneBotEvent::PrivateMessage(m)) if !m.sent_by_self => {
                        let key = format!("private:{}", m.user_id);
                        let mut incoming = queqiao_core::action::IncomingMessage {
                            key: key.clone(),
                            seq: 0,
                            message_id: m.message_id.to_string(),
                            sender: m.sender.nickname.clone(),
                            user_id: m.user_id.to_string(),
                            text: extract_plain_text(&m.message),
                            is_self: m.sent_by_self,
                            at_me: false,
                            reply_to_me: false,
                            time_ms: now_ms(),
                        };
                        incoming.seq = social
                            .lock()
                            .map(|mut s| s.append_message(&key, &incoming, now_ms()))
                            .unwrap_or(0);
                        Some(BridgeEvent::MessageArrived(Box::new(incoming)))
                    }
                    Ok(OneBotEvent::GroupPoke {
                        group_id, user_id, ..
                    }) => {
                        Some(BridgeEvent::PokeArrived {
                            key: Some(format!("group:{group_id}")),
                            user_id,
                            target_is_self: true, // 简化：拍一拍触发不区分目标
                        })
                    }
                    Ok(OneBotEvent::Heartbeat { good, .. }) => {
                        Some(BridgeEvent::UpstreamHeartbeat { good })
                    }
                    _ => None,
                };
                if let Some(event) = bridged
                    && tx.send(event).await.is_err()
                {
                    // 决策循环已退出（run 结束）——重连无意义，任务随之收尾。
                    return;
                }
            }
            queqiao::logging::log("OneBot WS 断开——退避重连中");
            let mut backoff_secs = 5u64;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                match EventStream::connect(&ws_url_rc, &ws_token_rc).await {
                    Ok(s) => {
                        stream = s;
                        queqiao::logging::log("OneBot WS 已重连");
                        break;
                    }
                    Err(e) => {
                        queqiao::logging::log(&format!(
                            "OneBot WS 重连失败（{e}）——{}s 后再试",
                            backoff_secs.min(60) * 2
                        ));
                        backoff_secs = (backoff_secs * 2).min(60);
                    }
                }
            }
        }
    });

    // ── 决策循环：事件 + 定时器推进 → 动作 ───────────────────────────────────
    let mut orchestrator = Orchestrator::new(
        DecideRules {
            batch_window_ms: cfg.social_v2.wake.batch_window_ms,
            max_wake_per_minute: cfg.social_v2.wake.max_wake_per_minute,
            max_wake_per_hour: cfg.social_v2.wake.max_wake_per_hour,
            no_action_limit: cfg.social_v2.wake.no_action_limit,
            turn_lease_ms: 30 * 60_000,
            pending_fallback_ms: 35 * 60_000,
            history_trim_cooldown_ms: 10 * 60_000,
            history_trim_resend_ms: 3_000,
            proactive: queqiao_core::runtime::ProactiveRules {
                enabled: cfg.social_v2.proactive.enabled,
                check_min_ms: cfg.social_v2.proactive.check_interval_min_ms,
                check_max_ms: cfg.social_v2.proactive.check_interval_max_ms,
                idle_threshold_ms: cfg.social_v2.proactive.idle_threshold_ms,
                probability: cfg.social_v2.proactive.probability,
            },
        },
        DiaryRules {
            enabled: cfg.social_v2.diary.enabled,
            hour: cfg.social_v2.diary.hour,
            not_before_hour: cfg.social_v2.diary.not_before_hour,
            quiet_ms: cfg.social_v2.diary.quiet_ms,
            retry_cooldown_ms: 3_600_000,
        },
    );
    // 白名单里的会话预注册（生产形状：allow 列表）；deny 黑名单一票否决
    // （同 allow+deny 冲突时 deny 生效——Node allowed() 语义）。
    for group in &cfg.allow.groups {
        let key = format!("group:{group}");
        if !cfg.conversation_allowed(&key) {
            continue;
        }
        orchestrator.ensure_session(
            &key,
            TriggerRules {
                keywords: cfg.social_v2.wake.recommended_keywords.clone(),
                ..Default::default()
            },
        );
    }
    for user in &cfg.allow.private {
        let key = format!("private:{user}");
        if !cfg.conversation_allowed(&key) {
            continue;
        }
        orchestrator.ensure_session(
            &key,
            TriggerRules {
                keywords: cfg.social_v2.wake.recommended_keywords.clone(),
                ..Default::default()
            },
        );
    }

    // ── DSH mux 事件流任务：信封 → BridgeEvent（回合生命周期回流决策）──────
    // 结构：连接 → follow 全部现有映射 → 信封转发；断连退避重连。新会话的
    // follow 通过 Notify 信号重连实现（重连重放全部 desired——MuxSession 契约，
    // 事件不丢）。
    let (mux_env_tx, mut mux_env_rx) =
        tokio::sync::mpsc::channel::<queqiao_dsh::mux::MuxEnvelope>(256);
    let reconnect = std::sync::Arc::new(tokio::sync::Notify::new());
    {
        let dsh = dsh_arc.clone();
        let sessions = sessions_arc.clone();
        let env_tx = mux_env_tx.clone();
        let reconnect = reconnect.clone();
        // DSH 基址重探测的起点（config 值）；DSH 重启端口漂移时 mux 失败
        // 路径触发 switch_if_drifted 切换共享 client——桥不再需要重启跟随。
        let cfg_dsh_base = cfg.dsh.base_url.clone();
        tokio::spawn(async move {
            loop {
                let dsh_clone = dsh.clone();
                let cookie = tokio::task::spawn_blocking(move || {
                    dsh_clone
                        .lock()
                        .ok()
                        .and_then(|mut c| c.ensure_auth(now_ms()).ok())
                })
                .await
                .ok()
                .flatten();
                let Some(cookie) = cookie else {
                    // 静默循环是观测黑洞：mux 掉线后 UI 一切"正常"而事件永远
                    // 不来（2026-10-25 真机排查靠 netstat 反推）。鉴权失败也
                    // 可能是 DSH 漂移（旧地址连不上）——先重探测再退避。
                    queqiao::logging::log("DSH mux 鉴权失败——重探测 DSH 基址");
                    switch_if_drifted(&dsh, &cfg_dsh_base).await;
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    continue;
                };
                let base = dsh
                    .lock()
                    .map(|c| c.base_url().to_owned())
                    .unwrap_or_default();
                let ws = match queqiao_dsh::ws::tungstenite::TungsteniteWs::connect(&base, &cookie)
                    .await
                {
                    Ok(ws) => ws,
                    Err(e) => {
                        queqiao::logging::log(&format!(
                            "DSH mux WS 连接失败（{base}，{e}）——重探测 DSH 基址"
                        ));
                        switch_if_drifted(&dsh, &cfg_dsh_base).await;
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                        continue;
                    }
                };
                let desired: HashSet<String> = sessions
                    .lock()
                    .map(|m| m.mappings().values().cloned().collect())
                    .unwrap_or_default();
                queqiao::logging::log(&format!(
                    "DSH mux 已连接 {base}（follow {} 个会话）",
                    desired.len()
                ));
                let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
                let runner = queqiao_dsh::ws::MuxRunner::new(ws, desired, env_tx.clone(), cmd_rx);
                let mut handle = tokio::spawn(runner.run());
                tokio::select! {
                    _ = &mut handle => {
                        // 断连：退避后重连（鉴权由 outcome 无效时 ensure_auth 重铸）。
                        queqiao::logging::log("DSH mux 断连——3s 后重连");
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    }
                    _ = reconnect.notified() => {
                        // 新会话映射出现：短暂等待写盘后重连重放全部 desired。
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        });
    }

    // 工具 API 宿主 + 控制台（同端口 3100；配置热更经 ConsoleState 共享）。
    let console_state = std::sync::Arc::new(queqiao::console::ConsoleState {
        config_path: queqiao::paths::config_path(),
        current: std::sync::Mutex::new(cfg.clone()),
        first_run: std::sync::atomic::AtomicBool::new(false),
    });
    // 自动更新通道：启动异步检测与控制台「检查更新」按钮共用（oneshot
    // 只能被 poll 一次——经 mpsc 转发避免 select! 多分支重 poll 的 panic）。
    let (update_tx, mut update_rx) = tokio::sync::mpsc::channel::<update::ReleaseInfo>(1);
    {
        let social = social_arc.clone();
        let cfg_clone = cfg.clone();
        let onebot = onebot_http.clone();
        let port = cfg_clone.console_port;
        let console_token = cfg_clone.console_token.clone();
        let state = std::sync::Arc::new(queqiao::tools_api::ToolApiState {
            social,
            onebot,
            cfg: cfg_clone,
            update_tx: update_tx.clone(),
            dsh: dsh_arc.clone(),
            sessions: sessions_arc.clone(),
            feedback_times: std::sync::Mutex::new(std::collections::HashMap::new()),
        });
        let console = console_state.clone();
        tokio::spawn(async move {
            // serve 永不返回才是常态；退出（Err/panic 恢复）必须落文件日志——
            // 只 eprintln 的话客户端崩溃路径读不到，3100 静默消失无从排查
            // （2026-10-25 真机：serve 死后 TCP 连接还挂着，日志零痕迹）。
            let r = queqiao::tools_api::serve(state, port, &console_token, Some(console)).await;
            queqiao::logging::log(&format!("工具 API 已退出（{r:?}）——控制台端口将不再响应"));
        });
    }
    println!(
        "控制台：http://127.0.0.1:{} （设置与状态，配置改动保存即热生效）",
        cfg.console_port
    );

    // 启动查一次 + 之后每小时轮询（长跑不重启的桥也能发现新版）；失败静默；
    // 控制台手动检查经同一通道汇入。tag 去重防轮询重复触发下载。
    // autoUpdate=false（热更即时生效——每轮重读）则只休眠不检查：升级
    // 完全交给客户端「检查更新」手动路径。
    {
        tokio::spawn(async move {
            let mut sent_tag = String::new();
            loop {
                let auto = tokio::task::spawn_blocking(|| {
                    std::fs::read_to_string(queqiao::paths::config_path())
                        .ok()
                        .and_then(|t| {
                            serde_json::from_str::<serde_json::Value>(
                                t.trim_start_matches('\u{feff}'),
                            )
                            .ok()
                        })
                        .and_then(|v| v.get("autoUpdate").and_then(|b| b.as_bool()))
                        .unwrap_or(true)
                })
                .await
                .unwrap_or(true);
                if auto {
                    let current = env!("CARGO_PKG_VERSION");
                    let checked =
                        tokio::task::spawn_blocking(move || update::check_latest(current))
                            .await
                            .ok()
                            .and_then(|r| r.ok())
                            .flatten();
                    if let Some(rel) = checked
                        && rel.tag != sent_tag
                    {
                        sent_tag = rel.tag.clone();
                        let _ = update_tx.send(rel).await;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    }

    println!(
        "决策循环启动（{} 个会话在册）— Ctrl+C 退出",
        orchestrator.session_keys().len()
    );
    let mut rng_seed = 0x2545F4914F6CDD1Du64;
    let mut rng = move || {
        // xorshift64*——决策层随机源（无需密码学质量）。
        rng_seed ^= rng_seed << 13;
        rng_seed ^= rng_seed >> 7;
        rng_seed ^= rng_seed << 17;
        ((rng_seed.wrapping_mul(0x2545F4914F6CDD1Du64)) >> 11) as f64 / (1u64 << 53) as f64
    };
    // 主动冒泡检查环：启动时全量排程一次，之后由 ProactiveCheck 触发自重排维持
    // （Node 模式进入时 scheduleProactiveCheckV2 全量排程语义）。
    orchestrator.schedule_proactive_checks(now_ms(), &mut rng);
    queqiao::logging::log(&format!(
        "主动检查环已排程（{} 个会话，间隔 {}~{}min，概率 {:.2}）",
        orchestrator.session_keys().len(),
        cfg.social_v2.proactive.check_interval_min_ms / 60_000,
        cfg.social_v2.proactive.check_interval_max_ms / 60_000,
        cfg.social_v2.proactive.probability
    ));
    let mut pending_update: Option<std::path::PathBuf> = None;
    let mut config_file = ConfigFile::load(&queqiao::paths::config_path()).unwrap_or_else(|_| {
        ConfigFile::load(&queqiao::paths::config_path()).expect("config 重新加载失败")
    });
    let mut last_tick = now_ms();
    let executor = Executor {
        sessions: sessions_arc.clone(),
        reconnect: reconnect.clone(),
        social: social_arc.clone(),
        wake_cfg: WakePromptConfig {
            pre_sleep_ms: cfg.social_v2.wake.pre_sleep_wait_ms,
            reply_quiet_ms: cfg.social_v2.wait.default_quiet_ms,
            reply_remaining_ms: cfg.social_v2.wait.default_quiet_ms,
        },
        sticker: cfg.social_v2.sticker.clone(),
        slang: cfg.slang.clone(),
    };
    loop {
        let timeout = tokio::time::sleep(std::time::Duration::from_millis(500));
        tokio::select! {
            maybe_event = rx.recv() => {
                let Some(event) = maybe_event else { break };
                let now = now_ms();
                // 动态会话注册（allowAllWhenEmpty=true 且 allow 两表皆空时，
                // 首条消息到达即注册——deny 命中仍拒；固定白名单模式下新键
                // 不在册，on_event 自然忽略）。
                {
                    let ev_key = match &event {
                        queqiao_core::action::BridgeEvent::MessageArrived(m) => Some(m.key.clone()),
                        queqiao_core::action::BridgeEvent::PokeArrived { key, .. } => key.clone(),
                        _ => None,
                    };
                    if let Some(k) = ev_key
                        && orchestrator.session(&k).is_none()
                        && cfg.conversation_allowed(&k)
                    {
                        orchestrator.ensure_session(
                            &k,
                            TriggerRules {
                                keywords: cfg.social_v2.wake.recommended_keywords.clone(),
                                ..Default::default()
                            },
                        );
                        orchestrator.schedule_proactive_check(&k, now, &mut rng);
                        queqiao::logging::log(&format!("动态注册会话 {k}（allowAllWhenEmpty）"));
                    }
                }
                let actions = orchestrator.on_event(event, now, now_local(), &mut rng);
                executor.execute(actions, now).await;
                let tick_actions = orchestrator.advance(now_ms(), now_local(), &mut rng);
                executor.execute(tick_actions, now_ms()).await;
                sync_dirty(&orchestrator, &social_arc);
            }
            maybe_envelope = mux_env_rx.recv() => {
                // mux 信封 → 回合生命周期事件（sessionId 反查会话键）。
                // 诊断日志（2026-10-25 mux 零事件排查留）：零"事件到达"即
                // mux.rs 帧解析层问题；有到达无后续即决策/回查映射问题。
                if let Some(queqiao_dsh::mux::MuxEnvelope::SessionEvent {
                    session_id, event,
                }) = &maybe_envelope
                {
                    queqiao::logging::log(&format!(
                        "mux 事件到达：{} @ {}",
                        event.get("type").and_then(|v| v.as_str()).unwrap_or("?"),
                        session_id
                    ));
                }
                if let Some(event) = maybe_envelope
                    .and_then(|envelope| bridge_mux_envelope(&executor.sessions, envelope))
                {
                    let now = now_ms();
                    let actions = orchestrator.on_event(event, now, now_local(), &mut rng);
                    executor.execute(actions, now).await;
                }
            }
            update = update_rx.recv() => {
                // 检测到新版本：显示更新内容 → 下载 staging → 全体空闲时应用并重启。
                // （循环内的输出一律走 logging::log——裸 println 在客户端管道断裂时
                // 写失败即 panic，曾致桥无痕死亡。）
                if let Some(rel) = update {
                    queqiao::logging::log(&format!("发现新版本 {}（当前 {}）", rel.tag, env!("CARGO_PKG_VERSION")));
                    for line in rel.notes.lines().take(30) {
                        queqiao::logging::log(&format!("│ {line}"));
                    }
                    let home = queqiao::paths::queqiao_home();
                    match tokio::task::spawn_blocking({
                        let rel = rel.clone();
                        move || {
                            let staging = update::stage_update(&rel, &home)?;
                            update::write_apply_bat(&staging, &home, false)
                        }
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
                    {
                        Ok(bat) => {
                            queqiao::logging::log("[update] 新版已就绪，等待会话空闲后自动应用并重启…");
                            pending_update = Some(bat);
                        }
                        Err(e) => queqiao::logging::log(&format!("[update] 下载/准备失败（本次跳过，下次启动重试）：{e}")),
                    }
                }
            }
            _ = timeout => {
                let now = now_ms();
                let actions = orchestrator.advance(now, now_local(), &mut rng);
                executor.execute(actions, now).await;
                sync_dirty(&orchestrator, &social_arc);
                // 配置热更：UI 保存 → mtime 变化 → 重载进 ConsoleState（坏配置保留旧值）。
                if now.saturating_sub(last_tick) > 5_000 {
                    last_tick = now;
                    if config_file.reload_if_changed().unwrap_or(false) {
                        let fresh = config_file.config().clone();
                        let mut cur = console_state.current.lock().unwrap();
                        if *cur != fresh {
                            *cur = fresh;
                            drop(cur);
                            queqiao::logging::log("[config] 热更生效（UI 保存的配置已应用）");
                        }
                    }
                }
                // 更新已就绪且全体空闲（无回合、无在途 prompt、无待发窗口）→ 应用并重启。
                if let Some(bat) = pending_update.take() {
                    let busy = orchestrator.session_keys().iter().any(|k| {
                        orchestrator.session(k).is_some_and(|rt| rt.is_busy())
                    });
                    if !busy {
                        println!("[update] 会话空闲，应用更新并重启…");
                        let _ = std::process::Command::new("cmd")
                            .args(["/c", "start", "", ""])
                            .arg(&bat)
                            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                            .spawn();
                        break;
                    }
                    pending_update = Some(bat);
                }
            }
        }
    }
}

/// 决策脏会话回写持久态（wake_times/noAction 等）并落盘。
fn sync_dirty(
    orchestrator: &Orchestrator,
    social: &std::sync::Arc<std::sync::Mutex<queqiao::social_store::SocialStore>>,
) {
    let mut store = social.lock().unwrap();
    for key in orchestrator.session_keys() {
        if let Some(rt) = orchestrator.session(&key) {
            store.sync_runtime(&key, rt);
        }
    }
    store.save();
}

/// mux 信封 → 回合生命周期事件（sessionId 反查会话键；未映射会话丢弃）。
fn bridge_mux_envelope(
    sessions: &std::sync::Arc<std::sync::Mutex<queqiao::session_manager::SessionManager>>,
    envelope: queqiao_dsh::mux::MuxEnvelope,
) -> Option<BridgeEvent> {
    let queqiao_dsh::mux::MuxEnvelope::SessionEvent { session_id, event } = envelope else {
        return None;
    };
    let key = sessions
        .lock()
        .ok()
        .and_then(|m| m.key_of_session(&session_id))?;
    let event_type = event
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    match event_type {
        "turn/start" => Some(BridgeEvent::TurnStarted { key, session_id }),
        "turn/end" => {
            // reason 含 error 视为失败回合（退配额路径）；其余（stop/正常收尾）为成功。
            let reason = event
                .get("data")
                .and_then(|d| d.get("reason"))
                .and_then(|r| r.as_str())
                .unwrap_or_default();
            let ok = !reason.to_ascii_lowercase().contains("error");
            Some(BridgeEvent::TurnEnded {
                key,
                session_id,
                ok,
                // 原文透传给决策层——413 历史超限判定在 core（is_history_limit_error）。
                error_text: (!ok).then(|| reason.to_string()),
            })
        }
        // 其余帧（assistant/message、tool/*…）刷新租约心跳。
        _ => Some(BridgeEvent::TurnBeat { key, session_id }),
    }
}

/// 动作执行器：SendPrompt 真实投递（spawn_blocking——reqwest 约束），
/// 新建会话后通知 mux 重连 follow；Log 直通 stdout。
struct Executor {
    sessions: std::sync::Arc<std::sync::Mutex<queqiao::session_manager::SessionManager>>,
    reconnect: std::sync::Arc<tokio::sync::Notify>,
    social: std::sync::Arc<std::sync::Mutex<queqiao::social_store::SocialStore>>,
    wake_cfg: WakePromptConfig,
    sticker: queqiao::config::StickerConfig,
    slang: queqiao::config::SlangConfig,
}

/// 唤醒文本构造参数（配置切片）。
struct WakePromptConfig {
    pre_sleep_ms: u64,
    reply_quiet_ms: u64,
    reply_remaining_ms: u64,
}

impl Executor {
    async fn execute(&self, actions: Vec<BridgeAction>, now: u64) {
        for action in actions {
            match action {
                BridgeAction::SendPrompt { key, reason } => {
                    // 暂停期间不自主冒泡（Node scheduleProactiveCheckV2 的 paused
                    // 门禁）；消息触发的唤醒照常投递——AI 借首次工具 403 安静收尾
                    // 的既有设计不变。
                    if reason == "proactiveCheck"
                        && self.social.lock().map(|s| s.paused()).unwrap_or(false)
                    {
                        queqiao::logging::log(&format!(
                            "SendPrompt {key}（{reason}）已跳过：二代 AI 处于暂停"
                        ));
                        continue;
                    }
                    // 完整唤醒文本：从持久态组装上下文（agentToken/未读/最近消息/唤醒摘要）。
                    let ctx = {
                        let mut social = self.social.lock().unwrap();
                        let token = social.ensure_agent_token(&key);
                        let st = social.conversation(&key);
                        let last_msg = st
                            .and_then(|s| s.recent_messages.iter().rev().find(|m| !m.is_self))
                            .map(|m| {
                                (
                                    m.sender.clone(),
                                    m.text.chars().take(30).collect::<String>(),
                                )
                            });
                        let last_unfinished = st
                            .and_then(|s| s.recent_messages.iter().rev().find(|m| !m.is_self))
                            .map(|m| queqiao_core::v2_wait::looks_like_unfinished(&m.text))
                            .unwrap_or(false);
                        let unread = st.map(|s| s.unread.len()).unwrap_or(0);
                        let wc = st.map(|s| s.wake_config.clone()).unwrap_or_default();
                        let mode = if wc.mode == "active" {
                            "活跃"
                        } else {
                            "潜水"
                        };
                        let timing = if wc.infinite {
                            "无限".to_owned()
                        } else {
                            "有限".to_owned()
                        };
                        let mut triggers = Vec::new();
                        if wc.triggers.at_mention {
                            triggers.push("@");
                        }
                        if wc.triggers.name_mention {
                            triggers.push("名字");
                        }
                        if !wc.triggers.keywords.is_empty() {
                            triggers.push("关键词");
                        }
                        if wc.triggers.question {
                            triggers.push("提问");
                        }
                        if wc.triggers.poke {
                            triggers.push("拍一拍");
                        }
                        if wc.triggers.any_message {
                            triggers.push("任意消息");
                        }
                        let summary = if triggers.is_empty() {
                            format!("{mode}，{timing}")
                        } else {
                            format!("{mode}，{timing}；触发：{}", triggers.join("/"))
                        };
                        queqiao::prompt::PromptContext {
                            key: key.clone(),
                            // 当前角色（current-role.json，perConv 覆盖全局）——
                            // 唤醒 prompt 只注角色名一行，全文卡由 qq_get_prompt 查。
                            role_name: queqiao::role_card_store::current_role_name(
                                &queqiao::paths::queqiao_home(),
                                &key,
                            )
                            .unwrap_or_default(),
                            agent_token: token,
                            unread_count: unread,
                            last_message: last_msg,
                            last_unfinished,
                            last_ai_reply_min_ago: None,
                            wake_summary: summary,
                            pre_sleep_ms: self.wake_cfg.pre_sleep_ms,
                            reply_quiet_ms: self.wake_cfg.reply_quiet_ms,
                            reply_remaining_ms: self.wake_cfg.reply_remaining_ms,
                        }
                    };
                    // 记忆注入（Node 版 memoryLine 语义：话题/想法/印象随唤醒出现）。
                    let memory_block = {
                        let social = self.social.lock().unwrap();
                        social
                            .conversation(&key)
                            .map(|st| {
                                let now = now_ms();
                                let block = queqiao::memory::format_memory(st, now);
                                if block.is_empty() {
                                    String::new()
                                } else {
                                    format!(
                                        "{block}

"
                                    )
                                }
                            })
                            .unwrap_or_default()
                    };
                    let text = queqiao::prompt::build_wake_prompt(&ctx, &reason);
                    // 表情包块（Node stickerLine 语义：策略提示 + 常用摘要，
                    // 位于记忆块之后；库空或开关关则省略）。
                    let sticker_block = if self.sticker.enabled && self.sticker.include_in_prompt {
                        let home = queqiao::paths::queqiao_home();
                        match queqiao::sticker_store::load(&queqiao::sticker_store::sticker_file(
                            &home,
                        )) {
                            Ok(entries) if !entries.is_empty() => format!(
                                "{}\n{}\n\n",
                                queqiao_core::sticker::build_sticker_strategy_hint(),
                                queqiao_core::sticker::build_sticker_context(
                                    &entries,
                                    self.sticker.prompt_max_stickers
                                )
                            ),
                            _ => String::new(),
                        }
                    } else {
                        String::new()
                    };
                    // 黑话表与当前时间前缀（Node withSlangContext：时间行 + 黑话块
                    // 统一在投递层注入，唤醒 prompt 内不出现第二份）。
                    let slang_block = if self.slang.enabled {
                        let file =
                            queqiao::slang_store::slang_file(&queqiao::paths::queqiao_home());
                        match queqiao::slang_store::load(&file) {
                            Ok(entries) => {
                                let block = queqiao_core::slang::build_context(
                                    &entries,
                                    self.slang.inject_max,
                                );
                                if block.is_empty() {
                                    String::new()
                                } else {
                                    format!(
                                        "{block}

"
                                    )
                                }
                            }
                            Err(e) => {
                                queqiao::logging::log(&format!("[slang] {e}"));
                                String::new()
                            }
                        }
                    } else {
                        String::new()
                    };
                    let time_line = format!(
                        "【当前时间】{}

",
                        now_local_str(now)
                    );
                    let text =
                        format!("{time_line}{slang_block}{memory_block}{sticker_block}{text}");
                    let sessions = self.sessions.clone();
                    let key_for_task = key.clone();
                    let reason_for_log = reason.clone();
                    let reconnect = self.reconnect.clone();
                    let before: HashSet<String> = sessions
                        .lock()
                        .map(|m| m.mappings().keys().cloned().collect())
                        .unwrap_or_default();
                    let result = tokio::task::spawn_blocking(move || {
                        sessions
                            .lock()
                            .map_err(|e| e.to_string())
                            .and_then(|mut m| m.deliver_prompt(&key_for_task, &text))
                    })
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                    match result {
                        Ok(()) => queqiao::logging::log(&format!(
                            "SendPrompt {key}（{reason_for_log}）已投递"
                        )),
                        Err(e) => queqiao::logging::log(&format!("SendPrompt {key} 投递失败：{e}")),
                    }
                    // 新会话创建后通知 mux 重连 follow。
                    let after: HashSet<String> = self
                        .sessions
                        .lock()
                        .map(|m| m.mappings().keys().cloned().collect())
                        .unwrap_or_default();
                    if after.len() > before.len() {
                        reconnect.notify_one();
                    }
                }
                BridgeAction::SendMessage { key, text, .. } => {
                    println!(
                        "[{now}] SendMessage {key}: {}",
                        text.chars().take(50).collect::<String>()
                    );
                }
                BridgeAction::RefundWakeTime { key, at } => {
                    println!("[{now}] RefundWakeTime {key} @{at}");
                }
                BridgeAction::RetireSession { key } => {
                    // 413 历史超限退役：桥侧记忆保留，下次投递 fail-closed 重建。
                    if let Ok(mut m) = self.sessions.lock() {
                        m.retire(&key);
                    }
                    queqiao::logging::log(&format!("[{now}] RetireSession {key}（映射已退役）"));
                    // mux follow 的 sessionId 已失效——重连按新映射重建。
                    self.reconnect.notify_one();
                }
                BridgeAction::SaveState => {}
                BridgeAction::Log { message } => {
                    queqiao::logging::log(&message);
                    queqiao::logging::activity(&message);
                }
                BridgeAction::ScheduleTimer { .. } | BridgeAction::CancelTimer { .. } => {
                    // 定时器已在 Orchestrator 内部处理。
                }
            }
        }
    }
}
