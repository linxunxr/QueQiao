//! queqiao-app —— 鹊桥桌面客户端（Tauri 2 壳）。
//!
//! 架构：**桥作为独立子进程编排**（与 MCP 进程隔离同哲学）——桥崩溃不拖垮 UI，
//! 桥的自动更新/编排逻辑零改动；壳负责窗口 UI（设置复用桥的 3100 控制台）、
//! 桥进程管理（启动/停止/日志流/崩溃检测）、系统托盘。
//!
//! 桥定位顺序：`QUEQIAO_BRIDGE_EXE` 环境变量（开发态指向 target/release）→
//! 安装包 resource 目录（binaries/）→ exe 同目录。
//!
//! 授权边界：客户端不加载 SnowLuma 专有组件（获批排除项），仅编排官方发行包
//! 的 node 进程（"与 Node.js 一同部署到客户端目录"的获批形态）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::sync::Mutex;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager};

#[derive(Default)]
struct BridgeState {
    child: Mutex<Option<Child>>,
    /// 桥 exe 路径（None = 未找到，UI 显示引导）。
    path: Mutex<Option<std::path::PathBuf>>,
    /// 安装目录（桥的工作目录 = 它的 home：config.json/state 都在这里）。
    home: std::path::PathBuf,
}

/// 读升级脚本留下的一次性桥自启标记（state/bridge-autostart）。
/// Some(true/false) = 按升级前状态决定是否拉桥（读后即删）；None = 正常
/// 启动无标记（默认拉桥）。
fn take_bridge_autostart_flag(home: &std::path::Path) -> Option<bool> {
    let marker = home.join("state").join("bridge-autostart");
    let flag = std::fs::read_to_string(&marker)
        .ok()
        .map(|s| s.trim() == "true");
    if flag.is_some() {
        let _ = std::fs::remove_file(&marker);
    }
    flag
}

/// 看门狗驱动（Node watchdog.mjs 的客户端形态）：30s 一轮探测三件套——
/// bridge=控制台口 TCP 探活（进程活着但 HTTP 卡死也测得出），dsh/snowluma
/// 复用进程探测。告警**直发 OneBot HTTP 私聊管理员，不经桥本身**——
/// 桥挂了告警才发得出。重启动作不接管（SnowLuma 授权边界、DSH 形态多样，
/// 告警先行的最小可用）。
fn start_watchdog(home: std::path::PathBuf) {
    std::thread::spawn(move || {
        let mut state = queqiao_core::watchdog::WatchdogState::default();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            // config 读取（ownerQQ/snowluma httpUrl+token/consolePort）。
            let (mut owner, mut http, mut token, mut port) =
                (None, String::new(), String::new(), 3100u16);
            if let Ok(raw) = std::fs::read_to_string(home.join("config.json"))
                && let Ok(v) =
                    serde_json::from_str::<serde_json::Value>(raw.trim_start_matches('\u{feff}'))
            {
                owner = v.get("ownerQQ").and_then(|x| x.as_i64());
                if let Some(sl) = v.get("snowluma").and_then(|x| x.as_object()) {
                    http = sl
                        .get("httpUrl")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .into();
                    token = sl
                        .get("accessToken")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .into();
                }
                if let Some(p) = v.get("consolePort").and_then(|x| x.as_u64()) {
                    port = p as u16;
                }
            }
            let bridge_alive = std::net::TcpStream::connect(("127.0.0.1", port)).is_ok();
            let (dsh, snowluma, _) = probe_components("");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let actions = queqiao_core::watchdog::evaluate(
                &mut state,
                [bridge_alive, dsh, snowluma],
                now,
                false,
            );
            for a in actions {
                let text = match &a {
                    queqiao_core::watchdog::WatchdogAction::Alert { target, detail } => {
                        format!("[看门狗] {target} 故障：{detail}")
                    }
                    queqiao_core::watchdog::WatchdogAction::Recovered { target } => {
                        format!("[看门狗] {target} 已恢复")
                    }
                    queqiao_core::watchdog::WatchdogAction::Restart { .. } => continue,
                };
                println!("[watchdog] {text}");
                // 直发管理员私聊（不经桥）。无 ownerQQ/token 配置则只打日志。
                if let Some(qq) = owner
                    && !http.is_empty()
                    && !token.is_empty()
                {
                    let client = queqiao_onebot::client::OneBotHttpClient::new(&http, &token, 5000);
                    let msg =
                        queqiao_onebot::segment::message(&[queqiao_onebot::segment::text(&text)]);
                    if let Err(e) = client.send_private_msg(qq as u64, &msg) {
                        println!("[watchdog] 告警发送失败：{e}");
                    }
                }
            }
        }
    });
}

fn find_bridge(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("QUEQIAO_BRIDGE_EXE")
        && std::path::Path::new(&p).exists()
    {
        return Some(p.into());
    }
    // 安装态：resource 目录的 binaries/。
    if let Ok(res) = app.path().resource_dir() {
        let candidate = res.join("binaries").join("queqiao.exe");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    // 便携态：exe 同目录。
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("queqiao.exe");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

impl BridgeState {
    fn running(&self) -> bool {
        self.child
            .lock()
            .ok()
            .and_then(|mut c| {
                c.as_mut()
                    .map(|p| p.try_wait().map(|w| w.is_none()).unwrap_or(false))
            })
            .unwrap_or(false)
    }

    fn spawn(&self, app: &tauri::AppHandle) -> Result<(), String> {
        if self.running() {
            return Ok(());
        }
        let path = self
            .path
            .lock()
            .ok()
            .and_then(|p| p.clone())
            .ok_or_else(|| {
                "未找到 queqiao.exe（QUEQIAO_BRIDGE_EXE / 安装目录 / exe 同目录）".to_owned()
            })?;
        let mut child = Command::new(&path)
            .current_dir(&self.home)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(0x0800_0000)
            .spawn()
            .map_err(|e| format!("启动桥失败：{e}"))?;
        // stdout/stderr 逐行读 → 前端日志流。
        if let Some(out) = child.stdout.take() {
            let app = app.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let _ = app.emit("bridge-log", line);
                }
            });
        }
        if let Some(err) = child.stderr.take() {
            let app = app.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    let _ = app.emit("bridge-log", format!("[stderr] {line}"));
                }
            });
        }
        *self.child.lock().map_err(|e| e.to_string())? = Some(child);
        let _ = app.emit("bridge-state", serde_json::json!({ "running": true }));
        Ok(())
    }

    fn stop(&self) -> Result<bool, String> {
        let mut guard = self.child.lock().map_err(|e| e.to_string())?;
        match guard.as_mut() {
            Some(p) => {
                let killed = p.kill().is_ok();
                let _ = p.wait();
                *guard = None;
                Ok(killed)
            }
            None => Ok(false),
        }
    }
}

#[tauri::command]
fn bridge_status(state: tauri::State<'_, BridgeState>) -> serde_json::Value {
    serde_json::json!({
        "running": state.running(),
        "path": state.path.lock().ok().and_then(|p| p.clone()).map(|p| p.display().to_string()),
    })
}

/// 客户端自身版本（顶栏常显用——不依赖桥在线；桥与客户端同版本发布）。
#[tauri::command]
fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

// ── 三件套编排：DSH → SnowLuma → 桥 ─────────────────────────────────────────
//
// SnowLuma 只探测、不启停（Node 版 mcp-host-server.js 2026-09-20 的裁决：
// EULA §5.4 把"通过自动化脚本部署"专有组件列入须书面授权范围，程序化拉起
// 落在射程内——不变量 L6"只探测、不部署"）。未运行时给可操作的提示
// （打开目录请用户双击 launcher.bat）。DSH 与桥是自有/无限制软件，可代劳。

/// 桥 config 的 snowluma 段（homeDir/httpUrl——探测与提示用）。
fn snowluma_cfg(home: &std::path::Path) -> (String, String) {
    let mut dir = String::new();
    let mut http = String::new();
    if let Ok(raw) = std::fs::read_to_string(home.join("config.json"))
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim_start_matches('\u{feff}'))
        && let Some(sl) = v.get("snowluma").and_then(|s| s.as_object())
    {
        dir = sl
            .get("homeDir")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .into();
        http = sl
            .get("httpUrl")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .into();
    }
    (dir, http)
}

/// 一条 PowerShell 查三态：DSH 进程 / SnowLuma 进程 / queqiao.exe 进程。
/// 输出 "dsh|snowluma|bridge"（True/False）。
fn probe_components(snowluma_dir: &str) -> (bool, bool, bool) {
    let esc = snowluma_dir.replace('\'', "''");
    let script = format!(
        "$procs = Get-CimInstance Win32_Process -ErrorAction SilentlyContinue; \
         $d = [bool]($procs | Where-Object {{ $_.Name -match '^DeepSeek' -or ($_.CommandLine -and $_.CommandLine -match 'dsh.*bin\\.js +web') }}); \
         $s = [bool]($procs | Where-Object {{ $_.CommandLine -and $_.CommandLine -like '*{esc}*' }}); \
         $b = [bool]($procs | Where-Object {{ $_.Name -eq 'queqiao.exe' }}); \
         \"$d|$s|$b\""
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW——GUI 进程 spawn 控制台程序不闪终端窗
        .output();
    let text = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return (false, false, false),
    };
    let mut it = text.split('|').map(|t| t.trim() == "True");
    let dsh = it.next().unwrap_or(false);
    // homeDir 未配置时 `-like '**'` 会匹配一切进程——恒判未运行。
    let snowluma = !snowluma_dir.trim().is_empty() && it.next().unwrap_or(false);
    (dsh, snowluma, it.next().unwrap_or(false))
}

/// SnowLuma 的 OneBot HTTP 口是否可达（桥 discover panic 的前置条件——
/// 一键启动必须等它就绪才起桥）。
fn http_url_alive(http_url: &str) -> bool {
    let url = http_url
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let Some((host, port)) = url.rsplit_once(':') else {
        return false;
    };
    let Ok(addr) = format!("{host}:{port}").parse::<std::net::SocketAddr>() else {
        return false;
    };
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(1200)).is_ok()
}

/// 找 DSH 桌面版主程序（Electron 惯例装在 LOCALAPPDATA\Programs）。
fn find_dsh_exe() -> Option<std::path::PathBuf> {
    let programs = std::path::PathBuf::from(std::env::var("LOCALAPPDATA").ok()?).join("Programs");
    let base = std::fs::read_dir(&programs).ok()?.flatten().find_map(|e| {
        let name = e.file_name().to_string_lossy().to_string();
        (name.starts_with("DeepSeek") && e.path().is_dir()).then_some(e.path())
    })?;
    // 主 exe：目录下最大的 DeepSeek*.exe（Electron 应用同目录常带卸载器等小 exe）。
    std::fs::read_dir(&base)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_string_lossy().to_string();
            let size = p.metadata().ok()?.len();
            (name.starts_with("DeepSeek") && name.ends_with(".exe")).then_some((p, size))
        })
        .max_by_key(|(_, size)| *size)
        .map(|(p, _)| p)
}

/// 组件三态查询。async + spawn_blocking：WMI 进程枚举在系统忙时可达秒级，
/// 同步命令占主线程会让 UI 周期性冻结（实测体感"时不时卡住"）。
#[tauri::command]
async fn components_status(
    state: tauri::State<'_, BridgeState>,
) -> Result<serde_json::Value, String> {
    let home = state.home.clone();
    let child_running = state.running();
    let st = tauri::async_runtime::spawn_blocking(move || {
        let (dir, http) = snowluma_cfg(&home);
        let (dsh, snowluma, bridge_proc) = probe_components(&dir);
        serde_json::json!({
            "dsh": dsh,
            "snowluma": snowluma,
            "snowlumaHttp": http_url_alive(&http),
            "snowlumaDir": dir,
            "dshPath": find_dsh_exe().map(|p| p.display().to_string()),
            "bridgeProc": bridge_proc,
        })
    })
    .await
    .map_err(|e| e.to_string())?;
    let mut st = st;
    // 桥：本壳 child 或系统进程任一在跑即运行（手动直跑 queqiao.exe 也算）。
    st["bridge"] = serde_json::json!(child_running || st["bridgeProc"].as_bool().unwrap_or(false));
    Ok(st)
}

#[tauri::command]
fn start_dsh() -> Result<String, String> {
    let exe = find_dsh_exe().ok_or_else(|| {
        "未找到 DeepSeek Harness（LOCALAPPDATA\\Programs\\DeepSeek*）——请手动启动 DSH".to_owned()
    })?;
    let name = exe.display().to_string();
    Command::new(&exe)
        .creation_flags(0x0800_0000) // DETACHED：DSH 独立运行，生命周期不随客户端
        .spawn()
        .map_err(|e| format!("启动 DSH 失败：{e}"))?;
    Ok(name)
}

/// 打开 SnowLuma 所在目录（资源管理器）——不是启停，只是给用户可操作的提示。
#[tauri::command]
fn open_snowluma_dir(state: tauri::State<'_, BridgeState>) -> Result<(), String> {
    let (dir, _) = snowluma_cfg(&state.home);
    if dir.is_empty() {
        return Err("config 未配置 snowluma.homeDir".to_owned());
    }
    Command::new("explorer")
        .arg(&dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开目录失败：{e}"))
}

/// 从 Node 版桥（qq-bridge）迁移数据：调安装目录随包的 migrate-from-node.ps1
/// （脚本非交互，参数直传；目标固定为本安装目录）。
#[tauri::command]
fn migrate_from_node(
    state: tauri::State<'_, BridgeState>,
    node_bridge: String,
    force: bool,
) -> Result<String, String> {
    let src = node_bridge.trim().to_string();
    if src.is_empty() {
        return Err("请填写 Node 版桥（qq-bridge）路径".to_owned());
    }
    if !std::path::Path::new(&src).join("config.json").exists() {
        return Err(format!(
            "{src} 下没有 config.json——请确认是 Node 版桥根目录"
        ));
    }
    let script = state.home.join("migrate-from-node.ps1");
    if !script.exists() {
        return Err(format!(
            "未找到迁移脚本 {}（完整安装应随包携带）",
            script.display()
        ));
    }
    let mut cmd = Command::new("powershell");
    cmd.arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(&script)
        .arg("-NodeBridge")
        .arg(&src)
        .arg("-InstallDir")
        .arg(&state.home);
    if force {
        cmd.arg("-Force");
    }
    let out = cmd
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .map_err(|e| format!("启动迁移脚本失败：{e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        return Err(format!("{stderr}\n{stdout}"));
    }
    Ok(stdout)
}

#[tauri::command]
fn bridge_start(app: tauri::AppHandle, state: tauri::State<'_, BridgeState>) -> Result<(), String> {
    state.spawn(&app)
}

#[tauri::command]
fn bridge_stop(state: tauri::State<'_, BridgeState>) -> Result<bool, String> {
    state.stop()
}

fn main() {
    tauri::Builder::default()
        .manage(BridgeState {
            home: home_dir(),
            ..Default::default()
        })
        .invoke_handler(tauri::generate_handler![
            bridge_status,
            bridge_start,
            bridge_stop,
            app_version,
            components_status,
            start_dsh,
            open_snowluma_dir,
            migrate_from_node
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            // 看门狗（客户端进程跑——桥卡死时告警仍可发）。
            {
                let state = app.state::<BridgeState>();
                start_watchdog(state.home.clone());
            }
            {
                let state = app.state::<BridgeState>();
                let found = find_bridge(&handle);
                *state.path.lock().unwrap() = found.clone();
                if let Some(p) = found {
                    println!("[app] 桥程序：{}", p.display());
                    // 自动启动桥；升级脚本重启时经一次性标记（state/bridge-autostart）
                    // 传递"升级前桥是否在跑"——桥被手动停了就不拉，保持用户意图。
                    // 读后即删；正常启动无标记，默认拉桥。
                    if take_bridge_autostart_flag(&state.home).unwrap_or(true) {
                        if let Err(e) = state.spawn(&handle) {
                            println!("[app] {e}");
                        }
                    } else {
                        println!("[app] 桥保持停止（升级前未运行）——需要时总览页一键启动");
                    }
                } else {
                    println!("[app] 未找到 queqiao.exe——UI 将显示引导");
                    let _ = handle.emit(
                        "bridge-state",
                        serde_json::json!({ "running": false, "missing": true }),
                    );
                }
            }

            // 系统托盘：显示窗口 / 启动桥 / 停止桥 / 退出（退出杀桥）。
            let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
            let start = MenuItem::with_id(app, "start", "启动桥", true, None::<&str>)?;
            let stop = MenuItem::with_id(app, "stop", "停止桥", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出（桥一并停止）", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &start, &stop, &quit])?;
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().cloned().expect("图标"))
                .tooltip("鹊桥 QueQiao")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "start" => {
                        let state = app.state::<BridgeState>();
                        if let Err(e) = state.spawn(app) {
                            println!("[app] {e}");
                        }
                    }
                    "stop" => {
                        let state = app.state::<BridgeState>();
                        let _ = state.stop();
                        let _ = app.emit("bridge-state", serde_json::json!({ "running": false }));
                    }
                    "quit" => {
                        app.state::<BridgeState>().stop().ok();
                        app.exit(0);
                    }
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // 关窗 = 隐藏到托盘（桥继续跑）。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("queqiao-app 运行失败");
}

/// 桥的 home（config.json/state 所在）：便携态 = exe 同目录；安装态 = 用户目录。
fn home_dir() -> std::path::PathBuf {
    if let Ok(home) = std::env::var("QUEQIAO_HOME") {
        return home.into();
    }
    // 安装态（resource 目录存在 binaries）用安装目录；否则 exe 同目录。
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        return dir.to_path_buf();
    }
    std::env::current_dir().unwrap_or_else(|_| ".".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_autostart_flag_three_states() {
        let dir = std::env::temp_dir().join(format!("qq-flag-{}", std::process::id()));
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let marker = state.join("bridge-autostart");
        // 无标记：None（默认拉桥）
        assert_eq!(take_bridge_autostart_flag(&dir), None);
        // true：Some(true) 且读后即删（一次性语义）
        std::fs::write(&marker, "true").unwrap();
        assert_eq!(take_bridge_autostart_flag(&dir), Some(true));
        assert!(!marker.exists(), "标记须读后即删");
        // false：Some(false)——升级前桥被手动停，不拉
        std::fs::write(&marker, "false").unwrap();
        assert_eq!(take_bridge_autostart_flag(&dir), Some(false));
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真机冒烟：三件套探测不 panic、结果可解释（具体值随环境，不断言）。
    #[test]
    fn probe_components_smoke() {
        let (dir, http) = snowluma_cfg(std::path::Path::new("."));
        println!("snowluma_cfg(本地 CWD) = ({dir}, {http})");
        let (dsh, snowluma, bridge) = probe_components(&dir);
        println!("probe = dsh:{dsh} snowluma:{snowluma} bridge:{bridge}");
        println!(
            "http_url_alive(3000) = {}",
            http_url_alive("http://127.0.0.1:3000")
        );
        println!(
            "find_dsh_exe = {:?}",
            find_dsh_exe().map(|p| p.display().to_string())
        );
        // homeDir 未配置：-like '**' 修复回归——必须判未运行。
        let (_, s2, _) = probe_components("");
        assert!(!s2, "homeDir 为空时 snowluma 必须判未运行");
    }
}
