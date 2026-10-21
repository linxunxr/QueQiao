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

    // DSH 探活（铸 Cookie + 端口自适应；仅打印，客户端全链下批接入）。
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

    let ws = found.ws.as_ref().expect("该账号没有 wsServers 配置");
    Setup {
        ws_url: format!("ws://{}:{}", ws.host, ws.port),
        ws_token: ws.token.clone(),
        cfg,
    }
}

async fn run(setup: Setup) {
    let cfg = setup.cfg;
    let ws_url = setup.ws_url;
    let stream = EventStream::connect(&ws_url, &setup.ws_token)
        .await
        .expect("OneBot WS 连接失败（token 失效？重跑或检查 per-account 配置）");
    println!("OneBot WS 已连接 {ws_url}");
    let (tx, mut rx) = tokio::sync::mpsc::channel::<BridgeEvent>(256);
    tokio::spawn(async move {
        let mut stream = stream;
        while let Some(event) = stream.recv_event().await {
            let bridged = match event {
                Ok(OneBotEvent::GroupMessage(m)) if !m.sent_by_self => {
                    Some(BridgeEvent::MessageArrived(Box::new(
                        queqiao_core::action::IncomingMessage {
                            key: format!("group:{}", m.group_id),
                            seq: 0, // seq 由水位推进（onebot 无内建序号——bin 接线批次补）
                            message_id: m.message_id.to_string(),
                            sender: m.sender.card.clone().unwrap_or(m.sender.nickname.clone()),
                            user_id: m.user_id.to_string(),
                            text: extract_plain_text(&m.message),
                            is_self: m.sent_by_self,
                            at_me: at_self(&m.message),
                            reply_to_me: false,
                            time_ms: now_ms(),
                        },
                    )))
                }
                Ok(OneBotEvent::PrivateMessage(m)) if !m.sent_by_self => Some(
                    BridgeEvent::MessageArrived(Box::new(queqiao_core::action::IncomingMessage {
                        key: format!("private:{}", m.user_id),
                        seq: 0,
                        message_id: m.message_id.to_string(),
                        sender: m.sender.nickname.clone(),
                        user_id: m.user_id.to_string(),
                        text: extract_plain_text(&m.message),
                        is_self: m.sent_by_self,
                        at_me: false,
                        reply_to_me: false,
                        time_ms: now_ms(),
                    })),
                ),
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
    let mut last_tick = now_ms();
    loop {
        let timeout = tokio::time::sleep(std::time::Duration::from_millis(500));
        tokio::select! {
            maybe_event = rx.recv() => {
                let Some(event) = maybe_event else { break };
                let now = now_ms();
                let actions = orchestrator.on_event(event, now, now_local(), &mut rng);
                execute(actions, now);
                let tick_actions = orchestrator.advance(now_ms(), now_local(), &mut rng);
                execute(tick_actions, now_ms());
            }
            _ = timeout => {
                let now = now_ms();
                // 半秒一拍：推进定时器轮 + 配置热更检查。
                let actions = orchestrator.advance(now, now_local(), &mut rng);
                execute(actions, now);
                if now.saturating_sub(last_tick) > 5_000 {
                    last_tick = now;
                    // 配置热更（mtime 检测；此处只打印——热应用在控制台批次）。
                }
            }
        }
    }
}

/// 动作执行（当前形态）：SendPrompt 打结构化日志（DSH 投递全链下批），
/// Log 直通 stdout，其余类型预告后续批次。
fn execute(actions: Vec<BridgeAction>, now: u64) {
    for action in actions {
        match action {
            BridgeAction::SendPrompt { key, reason } => {
                println!("[{now}] SendPrompt {key}（{reason}）— DSH 投递全链待接入");
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
