//! 鹊桥（QueQiao）—— QQ ↔ DeepSeek Harness 桥接主程序。
//!
//! 当前形态（M4 编排批次）：OneBot 事件流 → core 决策（Orchestrator）→ 动作
//! 执行。SendPrompt 的 DSH 投递全链（会话管理/prompt 构造）与控制台、看门狗
//! 在后续批次接入——当前动作以结构化日志输出（决策闭环已可观测）。
//!
//! Node 版（qq-bridge）是权威参照实现：行为契约以其 43 个 audit 脚本为准。

use std::collections::HashSet;
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

/// 消息是否 @ 了 bot（at 段 qq 匹配——self_id 本批从 login_info 取，简化为
/// 任意 at 段都视为 @ 自己之外的目标：at_me 需要精确 self_id，接线批次补全）。
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

fn main() {
    let cfg = match ConfigFile::load(std::path::Path::new("config.json")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "配置加载失败：{e}（请在工作目录放 config.json，参照 Node 版 config.example.json）"
            );
            std::process::exit(1);
        }
    };
    let cfg = cfg.config().clone();
    println!(
        "{} {} — 鹊桥主程序（M4 编排：OneBot 事件 → core 决策 → 动作日志）",
        queqiao_core::APP_NAME,
        env!("CARGO_PKG_VERSION")
    );

    // 发现段全部同步（reqwest blocking 不嵌 tokio——M2/M3 同款约束）。
    let setup = discover(cfg);
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run(setup));
}

/// 启动期同步发现：OneBot 连接 + DSH 基址（含端口自适应）。
struct Setup {
    ws_url: String,
    ws_token: String,
    cfg: queqiao::config::Config,
    dsh: std::sync::Arc<std::sync::Mutex<DshClient<ReqwestTransport>>>,
    sessions: std::sync::Arc<std::sync::Mutex<queqiao::session_manager::SessionManager>>,
    social: std::sync::Arc<std::sync::Mutex<queqiao::social_store::SocialStore>>,
}

fn discover(cfg: queqiao::config::Config) -> Setup {
    let home = std::path::PathBuf::from(&cfg.snowluma.home_dir);
    let accounts = read_accounts(&home);
    println!("SnowLuma 账号配置：{} 份", accounts.len());
    let mut probe = HttpProbe;
    let found = discover_connection(
        &mut probe,
        Some(&cfg.snowluma.access_token),
        &cfg.snowluma.http_url,
        &accounts,
    )
    .expect("SnowLuma 连接自发现失败（确认 SnowLuma 已登录且 OneBot 已开启）");
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
            std::path::Path::new("state/sessions.json"),
            &preset,
            &cfg.workspace_title,
            &agent_dir,
        )
        .expect("sessions.json 加载失败"),
    ));

    let social = std::sync::Arc::new(std::sync::Mutex::new(
        queqiao::social_store::SocialStore::load(
            std::path::Path::new("state/social-v2.json"),
            cfg.owner_qq.map(|o| o.0),
        )
        .expect("social-v2.json 加载失败"),
    ));

    let ws = found.ws.as_ref().expect("该账号没有 wsServers 配置");
    Setup {
        ws_url: format!("ws://{}:{}", ws.host, ws.port),
        ws_token: ws.token.clone(),
        cfg,
        dsh,
        sessions,
        social,
    }
}

async fn run(setup: Setup) {
    let cfg = setup.cfg;
    let ws_url = setup.ws_url;
    let dsh_arc = setup.dsh;
    let sessions_arc = setup.sessions;
    let social_arc = setup.social;
    let stream = EventStream::connect(&ws_url, &setup.ws_token)
        .await
        .expect("OneBot WS 连接失败（token 失效？重跑或检查 per-account 配置）");
    println!("OneBot WS 已连接 {ws_url}");
    let social_for_stream = social_arc.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<BridgeEvent>(256);
    tokio::spawn(async move {
        let mut stream = stream;
        let social = social_for_stream;
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
                break;
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
        },
        DiaryRules {
            enabled: cfg.social_v2.diary.enabled,
            hour: cfg.social_v2.diary.hour,
            not_before_hour: cfg.social_v2.diary.not_before_hour,
            quiet_ms: cfg.social_v2.diary.quiet_ms,
            retry_cooldown_ms: 3_600_000,
        },
    );
    // 白名单里的会话预注册（生产形状：allow 列表）。
    for group in &cfg.allow.groups {
        let key = format!("group:{group}");
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
                    Err(_) => {
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                        continue;
                    }
                };
                let desired: HashSet<String> = sessions
                    .lock()
                    .map(|m| m.mappings().values().cloned().collect())
                    .unwrap_or_default();
                println!("DSH mux 已连接 {base}（follow {} 个会话）", desired.len());
                let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
                let runner = queqiao_dsh::ws::MuxRunner::new(ws, desired, env_tx.clone(), cmd_rx);
                let mut handle = tokio::spawn(runner.run());
                tokio::select! {
                    _ = &mut handle => {
                        // 断连：退避后重连（鉴权由 outcome 无效时 ensure_auth 重铸）。
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
    let executor = Executor {
        sessions: sessions_arc.clone(),
        reconnect: reconnect.clone(),
        social: social_arc.clone(),
        wake_cfg: WakePromptConfig {
            role_name: String::new(), // current-role.json 读取在控制台批次接入
            pre_sleep_ms: cfg.social_v2.wake.pre_sleep_wait_ms,
            reply_quiet_ms: cfg.social_v2.wait.default_quiet_ms,
            reply_remaining_ms: cfg.social_v2.wait.default_quiet_ms,
        },
    };
    loop {
        let timeout = tokio::time::sleep(std::time::Duration::from_millis(500));
        tokio::select! {
            maybe_event = rx.recv() => {
                let Some(event) = maybe_event else { break };
                let now = now_ms();
                let actions = orchestrator.on_event(event, now, now_local(), &mut rng);
                executor.execute(actions, now).await;
                let tick_actions = orchestrator.advance(now_ms(), now_local(), &mut rng);
                executor.execute(tick_actions, now_ms()).await;
                sync_dirty(&orchestrator, &social_arc);
            }
            maybe_envelope = mux_env_rx.recv() => {
                // mux 信封 → 回合生命周期事件（sessionId 反查会话键）。
                if let Some(event) = maybe_envelope
                    .and_then(|envelope| bridge_mux_envelope(&executor.sessions, envelope))
                {
                    let now = now_ms();
                    let actions = orchestrator.on_event(event, now, now_local(), &mut rng);
                    executor.execute(actions, now).await;
                }
            }
            _ = timeout => {
                let now = now_ms();
                let actions = orchestrator.advance(now, now_local(), &mut rng);
                executor.execute(actions, now).await;
                sync_dirty(&orchestrator, &social_arc);
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
}

/// 唤醒文本构造参数（配置切片）。
struct WakePromptConfig {
    role_name: String,
    pre_sleep_ms: u64,
    reply_quiet_ms: u64,
    reply_remaining_ms: u64,
}

impl Executor {
    async fn execute(&self, actions: Vec<BridgeAction>, now: u64) {
        for action in actions {
            match action {
                BridgeAction::SendPrompt { key, reason } => {
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
                            role_name: self.wake_cfg.role_name.clone(),
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
                    let text = queqiao::prompt::build_wake_prompt(&ctx, &reason);
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
                        Ok(()) => println!("[{now}] SendPrompt {key}（{reason_for_log}）已投递"),
                        Err(e) => println!("[{now}] SendPrompt {key} 投递失败：{e}"),
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
                BridgeAction::SaveState => {}
                BridgeAction::Log { message } => println!("[{now}] {message}"),
                BridgeAction::ScheduleTimer { .. } | BridgeAction::CancelTimer { .. } => {
                    // 定时器已在 Orchestrator 内部处理。
                }
            }
        }
    }
}
