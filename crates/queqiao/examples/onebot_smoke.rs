//! SnowLuma（OneBot v11）冒烟驱动（M3 完成标志；规格 §九：接真实 SnowLuma）。
//!
//! 运行：`cargo run -p queqiao --example onebot_smoke`
//! SnowLuma home 的取值顺序：环境变量 `SNOWLUMA_HOME` → 相邻 Node 版仓库的
//! config.json（`snowluma.homeDir`）——冒烟专用临时代码。
//!
//! 冒烟项：
//! 1. per-account 双 token 解析 + 按「谁能用」自动发现（get_login_info 真探）；
//! 2. HTTP 调用（get_login_info / get_status）；
//! 3. WS 事件流连接，收一条事件并验证心跳载荷的 `status.good`
//!    （SnowLuma 每 30s 一拍；最多等 ~40s）。
//!
//! 只做只读调用，不发消息、不干扰在跑的桥。

use std::path::PathBuf;
use std::time::Duration;

use queqiao_onebot::client::OneBotHttpClient;
use queqiao_onebot::conn::{
    AccountEndpoints, LoginProbe, discover_connection, parse_account_config,
};
use queqiao_onebot::event::OneBotEvent;
use queqiao_onebot::ws::EventStream;

fn snowluma_home() -> Result<PathBuf, String> {
    if let Ok(home) = std::env::var("SNOWLUMA_HOME") {
        return Ok(PathBuf::from(home));
    }
    // 冒烟便利：相邻 Node 版仓库的配置里就有 homeDir。
    let bridge_cfg =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../qq-bridge/config.json");
    let text = std::fs::read_to_string(&bridge_cfg)
        .map_err(|e| format!("未设 SNOWLUMA_HOME 且读不到 {bridge_cfg:?}：{e}"))?;
    let cfg: serde_json::Value =
        serde_json::from_str(text.trim_start_matches('\u{FEFF}')).map_err(|e| e.to_string())?;
    cfg.get("snowluma")
        .and_then(|s| s.get("homeDir"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .ok_or_else(|| "Node 版 config.json 里没有 snowluma.homeDir".to_owned())
}

/// 读 per-account 配置：onebot_<数字>.json 逐个解析（坏文件跳过、按 uin 排序）。
fn read_accounts(home: &std::path::Path) -> Vec<AccountEndpoints> {
    let dir = home.join("config");
    let mut accounts = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
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

/// 真实探测：get_login_info 谁通用谁。
struct HttpProbe;

impl LoginProbe for HttpProbe {
    fn probe(&mut self, base_url: &str, token: &str) -> Option<serde_json::Value> {
        OneBotHttpClient::new(base_url, token, 5000)
            .get_login_info()
            .ok()
    }
}

fn main() {
    // HTTP 段同步跑（reqwest blocking 不能嵌 tokio 上下文 drop——dsh_smoke 同款教训）。
    let ws_target = match http_smoke() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("\n=== SnowLuma 冒烟失败：{e} ===");
            std::process::exit(1);
        }
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(ws_smoke(ws_target.0, ws_target.1)) {
        eprintln!("\n=== SnowLuma 冒烟失败（WS 段）：{e} ===");
        std::process::exit(1);
    }
    println!("\n=== SnowLuma 冒烟全部通过 ===");
}

/// HTTP 段：双 token 解析 → 自动发现 → get_status。返回 (ws_url, ws_token)。
fn http_smoke() -> Result<(String, String), String> {
    let home = snowluma_home()?;
    let accounts = read_accounts(&home);
    if accounts.is_empty() {
        return Err(format!(
            "{}\\config 下没有可解析的 onebot_<QQ号>.json（SnowLuma 未登录或 OneBot 未开启？）",
            home.display()
        ));
    }
    for account in &accounts {
        println!(
            "      账号 {}：http={}（token {}…）ws={}（token {}…）",
            account.uin,
            account
                .http
                .as_ref()
                .map(|e| format!("{}:{}", e.host, e.port))
                .unwrap_or_default(),
            account
                .http
                .as_ref()
                .map(|e| &e.token[..e.token.len().min(4)])
                .unwrap_or(""),
            account
                .ws
                .as_ref()
                .map(|e| format!("{}:{}", e.host, e.port))
                .unwrap_or_default(),
            account
                .ws
                .as_ref()
                .map(|e| &e.token[..e.token.len().min(4)])
                .unwrap_or(""),
        );
    }
    let mut probe = HttpProbe;
    let found = discover_connection(&mut probe, None, "http://127.0.0.1:3000", &accounts)
        .map_err(|e| format!("自动发现失败：{e}"))?;
    println!(
        "[1/3] 发现连接：账号 {}（{}），HTTP {}，WS token {}",
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
        found.base_url,
        found
            .ws
            .as_ref()
            .map(|e| format!("{}…", &e.token[..e.token.len().min(4)]))
            .unwrap_or("(无)".into()),
    );

    let client = OneBotHttpClient::new(&found.base_url, &found.token, 8000);
    let status = client
        .get_status()
        .map_err(|e| format!("get_status 失败：{e}"))?;
    let good = status.get("good").and_then(|v| v.as_bool());
    let online = status.get("online").and_then(|v| v.as_bool());
    println!("[2/3] get_status：online={online:?} good={good:?}");

    // 发送侧验证：发私聊给**自己**（同步到自己的其他设备，无群副作用）。
    let self_id = found
        .self_info
        .get("user_id")
        .and_then(|v| v.as_u64())
        .unwrap_or_default();
    let message = queqiao_onebot::segment::message(&[queqiao_onebot::segment::text(
        "queqiao（鹊桥）M3 冒烟：send_private_msg 自检消息，请忽略。",
    )]);
    let sent_id = client
        .send_private_msg(self_id, &message)
        .map_err(|e| format!("send_private_msg 失败：{e}"))?;
    println!("      send_private_msg → message_id {sent_id}（发送侧 OK）");

    let ws = found.ws.as_ref().ok_or("该账号没有 wsServers 配置")?;
    Ok((format!("ws://{}:{}", ws.host, ws.port), ws.token.clone()))
}

/// WS 段：事件流连接，验证心跳载荷的 status.good。
async fn ws_smoke(ws_url: String, ws_token: String) -> Result<(), String> {
    let mut stream = EventStream::connect(&ws_url, &ws_token)
        .await
        .map_err(|e| format!("{e}（WS token 与 HTTP 不同，换账号后 1006 多为此）"))?;
    println!("      WS 已连接 {ws_url}，等待心跳（SnowLuma 每 30s 一拍，最多等 40s）…");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    let mut saw_heartbeat_good: Option<bool> = None;
    let mut events = 0usize;
    while tokio::time::Instant::now() < deadline {
        let recv = tokio::time::timeout_at(deadline, stream.recv_event()).await;
        match recv {
            Ok(Some(Ok(OneBotEvent::Heartbeat { online, good }))) => {
                events += 1;
                println!(
                    "      心跳：online={online:?} good={good:?}（status.good 是 hook 健康权威信号）"
                );
                saw_heartbeat_good = good;
                if saw_heartbeat_good.is_some() {
                    break;
                }
            }
            Ok(Some(Ok(other))) => {
                events += 1;
                if events <= 3 {
                    println!("      事件：{other:?}");
                }
            }
            Ok(Some(Err(e))) => return Err(format!("事件流错误：{e}")),
            Ok(None) => return Err("事件流在心跳前被关闭（token 失效？）".into()),
            Err(_) => break, // 超时
        }
    }
    match saw_heartbeat_good {
        Some(good) => println!("[3/3] 收到心跳，status.good = {good}（共 {events} 个事件）"),
        None => {
            return Err(format!(
                "40 秒内未收到心跳（共 {events} 个事件）——WS 链路或 SnowLuma 异常"
            ));
        }
    }
    Ok(())
}
