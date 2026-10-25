//! 诊断工具：真调一次 workspace/create 并打印完整 result（ok/error/value）。
//! 排查「SendPrompt 投递失败：workspace/create 返回缺 workspaceId」——该文案
//! 吞掉了 DSH 的业务错误（session_manager 未看 result.ok 挖 value 所致）。
//! 用法：cargo run -p queqiao-dsh --example dump_workspace [候选基址...]

use queqiao_dsh::client::{AuthSources, DshClient, ReqwestTransport};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn make_client(base: &str) -> DshClient<ReqwestTransport> {
    let yaml = std::fs::read_to_string(
        std::env::var("USERPROFILE")
            .map(std::path::PathBuf::from)
            .unwrap()
            .join(".dsh\\.credentials.yaml"),
    )
    .expect("读不到 ~/.dsh/.credentials.yaml");
    let secret = queqiao_dsh::auth::parse_browser_session_secret(&yaml)
        .expect("credentials 里无 browser session secret");
    DshClient::new(
        base,
        ReqwestTransport::new(10_000),
        AuthSources {
            launch_token: String::new(),
            token_explicit: false,
            secret: Box::new(move || Some(secret)),
            rediscover_token: None,
        },
        false,
    )
}

fn main() {
    // 可选 --delete <workspaceId>：删 DSH workspace（清理验证时误建的）。
    // 可选 --prompt <sessionId> <text>：直投一条 prompt（DSH 侧回合触发排查）。
    let (mut delete_id, mut prompt): (Option<String>, Option<(String, String)>) = (None, None);
    let mut rest = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--delete" {
            delete_id = it.next();
        } else if a == "--prompt" {
            let id = it.next().unwrap_or_default();
            let text = it.next().unwrap_or_default();
            prompt = Some((id, text));
        } else {
            rest.push(a);
        }
    }
    let candidates = if rest.is_empty() {
        vec![
            "http://127.0.0.1:3080".to_owned(),
            "http://127.0.0.1:6411".to_owned(),
        ]
    } else {
        rest
    };
    for base in &candidates {
        let mut client = make_client(base);
        if let Some((id, text)) = &prompt {
            match client.call_unary(
                "session/prompt",
                Some(json!({
                    "sessionId": id,
                    "mode": "queue",
                    "content": [ { "type": "text", "text": text } ],
                })),
                now_ms(),
            ) {
                Ok(o) => println!(
                    "[{base}] session/prompt ok={} error={:?} value={}",
                    o.result.ok,
                    o.result.error,
                    o.result.value.unwrap_or(serde_json::Value::Null)
                ),
                Err(e) => println!("[{base}] session/prompt 调用失败：{e}"),
            }
            return;
        }
        if let Some(id) = &delete_id {
            match client.call_unary(
                "workspace/delete",
                Some(json!({ "workspaceId": id })),
                now_ms(),
            ) {
                Ok(o) => println!(
                    "[{base}] workspace/delete ok={} error={:?}",
                    o.result.ok, o.result.error
                ),
                Err(e) => println!("[{base}] workspace/delete 调用失败：{e}"),
            }
            return;
        }
        match client.call_unary("settings/describe", None, now_ms()) {
            Err(e) => {
                println!("[{base}] settings/describe 失败：{e}");
                continue;
            }
            Ok(o) if !o.result.ok => {
                println!("[{base}] settings/describe 业务错误：{:?}", o.result.error);
                continue;
            }
            Ok(_) => {}
        }
        println!("[{base}] DSH 在线，调 workspace/create …");
        let agent_dir = std::env::current_dir()
            .unwrap()
            .join("state")
            .join("agents");
        let payload = json!({ "path": agent_dir });
        match client.call_unary("workspace/create", Some(payload), now_ms()) {
            Ok(o) => {
                println!("  ok      = {}", o.result.ok);
                println!(
                    "  error   = {}",
                    serde_json::to_string_pretty(&o.result.error).unwrap_or_default()
                );
                println!(
                    "  value   = {}",
                    serde_json::to_string_pretty(
                        &o.result.value.unwrap_or(serde_json::Value::Null)
                    )
                    .unwrap_or_default()
                );
            }
            Err(e) => println!("  调用失败：{e}"),
        }
        return;
    }
    eprintln!("候选基址都不在线：{candidates:?}");
}
