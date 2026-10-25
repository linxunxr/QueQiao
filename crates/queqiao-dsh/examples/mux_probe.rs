//! mux 探针：按桥同款协议连 DSH remote.mux，打印所有入站原始帧。
//! 排查「mux 已连接但零事件」——区分零帧到达（订阅未生效/协议变更）与
//! 帧到了但桥解析不出（格式变化）。
//! 用法：cargo run -p queqiao-dsh --features reqwest --example mux_probe --
//!       <base> <sessionId> [观察秒数=60]

use queqiao_dsh::client::{AuthSources, DshClient, ReqwestTransport};
use queqiao_dsh::mux::{events_open_frame, follow_open_frame};
use queqiao_dsh::ws::WsStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("用法：mux_probe <base> <sessionId> [秒数]");
        std::process::exit(2);
    }
    let base = &args[0];
    let session_id = args[1].clone();
    let watch_secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(60);

    // 鉴权在 runtime 外同步做（reqwest blocking client 不得在 async 上下文
    // 创建/drop——runtime-in-runtime panic，本项目坑清单第一条）。
    let yaml = std::fs::read_to_string(
        std::env::var("USERPROFILE")
            .map(std::path::PathBuf::from)
            .unwrap()
            .join(".dsh\\.credentials.yaml"),
    )
    .expect("读不到 ~/.dsh/.credentials.yaml");
    let secret = queqiao_dsh::auth::parse_browser_session_secret(&yaml).expect("无 secret");
    let cookie = {
        let mut client = DshClient::new(
            base,
            ReqwestTransport::new(10_000),
            AuthSources {
                launch_token: String::new(),
                token_explicit: false,
                secret: Box::new(move || Some(secret)),
                rediscover_token: None,
            },
            false,
        );
        client.ensure_auth(now_ms()).expect("鉴权失败")
    };
    let base = base.clone();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async move {
        let mut ws = queqiao_dsh::ws::tungstenite::TungsteniteWs::connect(&base, &cookie)
            .await
            .expect("mux WS 连接失败");
        println!("已连接 {base}/api/remote.mux，开 events + follow({session_id}) …");
        ws.send_text(&events_open_frame("probe-events"))
            .await
            .unwrap();
        ws.send_text(&follow_open_frame("probe-f1", &session_id, 20))
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(watch_secs);
        let mut frames = 0usize;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                r = ws.recv_text() => {
                    match r {
                        Ok(Some(text)) => {
                            frames += 1;
                            let cap = if text.contains("\"snapshot\"") { 20000 } else { 400 };
                            let head: String = text.chars().take(cap).collect();
                            println!("[帧 {frames}] {head}");
                        }
                        Ok(None) => { println!("[连接被对端关闭]"); break; }
                        Err(e) => { println!("[错误] {e}"); break; }
                    }
                }
            }
        }
        println!("观察结束：共收到 {frames} 帧");
    });
}
