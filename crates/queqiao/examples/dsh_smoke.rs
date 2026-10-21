//! DSH 冒烟驱动（M2 完成标志；规格 §九：冒烟驱动为 bin 内临时代码）。
//!
//! 运行：`cargo run -p queqiao --example dsh_smoke`
//! 环境变量 `DSH_BASE_URL` 可指定基址；缺省先试 127.0.0.1:3080，不通则扫
//! DSH 相关进程的监听端口逐个验证（端口自适应契约的实机演练）。
//!
//! 冒烟项（对应 Node 版 test-dsh-auth 的端到端段）：
//! 1. 端口发现与业务口验证；
//! 2. 铸造 Cookie 过鉴权（settings/describe）；
//! 3. agentPresets/list 可达；
//! 4. mux WS 连接并收帧（只收全局事件，不 follow 生产会话——不干扰在跑的桥）。
//!
//! 两段式：HTTP 段同步跑（reqwest blocking 自带 runtime，不能嵌在 tokio
//! 上下文里 drop），WS 段才建 tokio runtime。

use std::collections::HashSet;
use std::process::Command;
use std::time::Duration;

use queqiao_dsh::auth::parse_browser_session_secret;
use queqiao_dsh::client::{AuthSources, DshClient, ReqwestTransport};
use queqiao_dsh::port::{discover_dsh_base_url, encoded_port_scan_command};
use queqiao_dsh::ws::tungstenite::TungsteniteWs;
use queqiao_dsh::ws::{MuxCommand, MuxRunner};

fn dsh_home() -> std::path::PathBuf {
    if let Ok(home) = std::env::var("DSH_HOME") {
        return std::path::PathBuf::from(home);
    }
    let mut p = std::env::var_os("USERPROFILE")
        .map(std::path::PathBuf::from)
        .expect("USERPROFILE 必然存在");
    p.push(".dsh");
    p
}

/// PowerShell 扫描 DSH 相关进程的监听端口（找不到返回空）。
fn scan_ports() -> Vec<u16> {
    let Ok(out) = Command::new("powershell")
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// 冒烟探测：settings/describe 是否走通（端口验证用）。
fn probe_ok(client: &mut DshClient<ReqwestTransport>) -> bool {
    client
        .call_unary("settings/describe", None, now_ms())
        .is_ok()
}

fn build_client(base: &str) -> Result<DshClient<ReqwestTransport>, String> {
    let credentials = std::fs::read_to_string(dsh_home().join(".credentials.yaml"))
        .map_err(|e| format!("读 credentials 失败（{}）：{e}", dsh_home().display()))?;
    let secret = parse_browser_session_secret(&credentials)
        .ok_or("credentials 里没有可用的 browser-session 密钥（32 字节）")?;
    let secret_provider = move || Some(secret);
    Ok(DshClient::new(
        base,
        ReqwestTransport::new(15000),
        AuthSources {
            launch_token: String::new(),
            token_explicit: false,
            secret: Box::new(secret_provider),
            rediscover_token: None,
        },
        false,
    ))
}

fn main() {
    let (base, cookie) = match http_smoke() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("\n=== DSH 冒烟失败：{e} ===");
            std::process::exit(1);
        }
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(ws_smoke(&base, &cookie)) {
        eprintln!("\n=== DSH 冒烟失败（WS 段）：{e} ===");
        std::process::exit(1);
    }
    println!("\n=== DSH 冒烟全部通过 ===");
}

/// HTTP 段：端口发现 → 铸 Cookie 鉴权 → agentPresets。返回 (基址, mux 用 Cookie)。
fn http_smoke() -> Result<(String, String), String> {
    // ── 1. 端口发现 ────────────────────────────────────────────────────────
    let preferred =
        std::env::var("DSH_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:3080".into());
    let probe = |base: &str| build_client(base).is_ok_and(|mut c| probe_ok(&mut c));
    let base = if probe(&preferred) {
        preferred.clone()
    } else {
        let ports = scan_ports();
        println!("端口扫描候选：{ports:?}");
        discover_dsh_base_url(&ports, &preferred, |base| probe(base))
            .ok_or_else(|| format!("未找到可用的 DSH 业务口（先试 {preferred}，再扫 {ports:?}）"))?
    };
    println!("[1/4] DSH 基址：{base}");

    // ── 2/3. 铸 Cookie 鉴权 + agentPresets ─────────────────────────────────
    let mut client = build_client(&base)?;
    let describe = client
        .call_unary("settings/describe", None, now_ms())
        .map_err(|e| format!("settings/describe 失败（铸 Cookie 鉴权）：{e}"))?;
    println!(
        "[2/4] settings/describe ok={}（策略 {:?}）",
        describe.result.ok,
        client.strategy()
    );

    let presets = client
        .call_unary("agentPresets/list", None, now_ms())
        .map_err(|e| format!("agentPresets/list 失败：{e}"))?;
    let preset_ids: Vec<String> = presets
        .result
        .value
        .as_ref()
        .and_then(|v| v.get("presets"))
        .and_then(|p| p.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| p.get("id").and_then(|i| i.as_str()))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if preset_ids.is_empty() {
        return Err("agentPresets/list 返回空 preset 列表".into());
    }
    println!("[3/4] presets：{}", preset_ids.join(", "));

    let cookie = client
        .ensure_auth(now_ms())
        .map_err(|e| format!("ensure_auth 失败：{e}"))?;
    Ok((base, cookie))
}

/// WS 段：mux 连接收帧（不 follow 生产会话）。
async fn ws_smoke(base: &str, cookie: &str) -> Result<(), String> {
    let ws = TungsteniteWs::connect(base, cookie)
        .await
        .map_err(|e| format!("mux 连接失败（{base}）：{e}"))?;
    let (env_tx, mut env_rx) = tokio::sync::mpsc::channel(64);
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
    let runner = MuxRunner::new(ws, HashSet::new(), env_tx, cmd_rx);
    let handle = tokio::spawn(runner.run());

    let mut received = 0usize;
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(envelope) = env_rx.recv().await {
            received += 1;
            if received <= 3 {
                println!("      mux 信封：{envelope:?}");
            }
        }
    })
    .await;
    let _ = cmd_tx.send(MuxCommand::Abort).await;
    let outcome = handle.await.map_err(|e| e.to_string())?;
    // 不 follow 任何会话时 3 秒内可能没有信封——连接保持即算通过。
    println!(
        "[4/4] mux 连接存活（信封 {received} 条，invalidated={}）",
        outcome.invalidated_auth
    );
    Ok(())
}
