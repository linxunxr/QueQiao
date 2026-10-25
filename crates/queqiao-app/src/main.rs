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

/// httpUrl 未配置时的 OneBot 探测回落：与桥 discover 同源——SnowLuma
/// per-account 配置（config/onebot_<uin>.json）里的 http 端点。安装脚本只写
/// homeDir、桥也不回写 httpUrl，缺位时按显式键探测恒假阴（2026-10-24 真机：
/// OneBot 明明监听 3000，卡片恒显"未就绪"、一键启动空等 5 分钟超时）。
fn fallback_http_urls(snowluma_dir: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let Ok(entries) = std::fs::read_dir(std::path::Path::new(snowluma_dir).join("config")) else {
        return urls;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(uin) = queqiao_onebot::conn::account_file_uin(&name) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Some(http) =
            queqiao_onebot::conn::parse_account_config(uin, &text).and_then(|a| a.http)
        {
            urls.push(format!("http://{}:{}", http.host, http.port));
        }
    }
    urls.sort();
    urls
}

/// 一条 PowerShell 查三态：DSH 进程 / SnowLuma 进程 / queqiao.exe 进程。
/// 输出 "dsh|snowluma|bridge"（True/False）。
fn probe_components(snowluma_dir: &str) -> (bool, bool, bool) {
    let esc = snowluma_dir.replace('\'', "''");
    let script = format!(
        "$procs = Get-CimInstance Win32_Process -ErrorAction SilentlyContinue; \
         $d = [bool]($procs | Where-Object {{ $_.Name -match '^DeepSeek' -or ($_.CommandLine -and $_.CommandLine -match 'dsh.*bin\\.js +web') }}); \
         $s = [bool]($procs | Where-Object {{ $_.CommandLine -and $_.Name -ne 'powershell.exe' -and $_.CommandLine -like '*{esc}*' }}); \
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

/// httpUrl → 探测地址列表。IP 字面量直用；主机名（如 localhost）交给系统
/// 解析——SocketAddr 只认 IP 字面量，`http://localhost:3000` 直接 parse 恒失败。
fn probe_addrs(http_url: &str) -> Vec<std::net::SocketAddr> {
    let url = http_url.trim_start_matches("http://").trim_end_matches('/');
    let Some((host, port)) = url.rsplit_once(':') else {
        return Vec::new();
    };
    let Ok(port) = port.parse::<u16>() else {
        return Vec::new();
    };
    if let Ok(addr) = format!("{host}:{port}").parse::<std::net::SocketAddr>() {
        return vec![addr];
    }
    use std::net::ToSocketAddrs;
    (host, port)
        .to_socket_addrs()
        .map(|it| it.collect())
        .unwrap_or_default()
}

/// SnowLuma 的 OneBot HTTP 口是否可达（桥 discover panic 的前置条件——
/// 一键启动必须等它就绪才起桥）。
fn http_url_alive(http_url: &str) -> bool {
    probe_addrs(http_url).iter().any(|addr| {
        std::net::TcpStream::connect_timeout(addr, std::time::Duration::from_millis(1200)).is_ok()
    })
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
        // OneBot 探测目标：显式 httpUrl 优先（尊重用户选择）；未配置时回落
        // per-account 端点（桥 discover 同源）——不依赖安装脚本从不写入的键。
        let probe_urls: Vec<String> = if http.trim().is_empty() {
            fallback_http_urls(&dir)
        } else {
            vec![http]
        };
        serde_json::json!({
            "dsh": dsh,
            "snowluma": snowluma,
            "snowlumaHttp": probe_urls.iter().any(|u| http_url_alive(u)),
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

/// 拉起已安装的 SnowLuma（A 档授权原文："通过脚本……在用户本地安装、启动和
/// 运行"——2026-09-20 L6 裁决的"不程序化启停"在'启动'这一点上已被书面授权
/// 覆盖；'停止'仍未授权，不做）。按官方随附方式：launcher.bat、带自身控制台
/// 窗口、不注入不修改。
#[tauri::command]
fn start_snowluma(state: tauri::State<'_, BridgeState>) -> Result<String, String> {
    let (dir, _) = snowluma_cfg(&state.home);
    if dir.is_empty() {
        return Err("config 未配置 snowluma.homeDir（设置页填写后重试）".to_owned());
    }
    let launcher = std::path::Path::new(&dir).join("launcher.bat");
    if !launcher.exists() {
        return Err(format!(
            "{} 下没有 launcher.bat——请先安装 SnowLuma（安装器可从官方渠道下载）",
            dir
        ));
    }
    // 直接 spawn .bat：std 自动经 cmd 执行；无控制台的 GUI 父进程会为新进程
    // 分配控制台窗口——正是手动双击 launcher.bat 的官方形态（登录/输出都在
    // 这个窗口里）。不用 `cmd /c start`——无控制台父进程本就开新窗口，且
    // start 的窗口标题引号语义在参数数组里易歧义（实测踩过）。
    Command::new(&launcher)
        .current_dir(&dir)
        .spawn()
        .map_err(|e| format!("启动 SnowLuma 失败：{e}"))?;
    Ok(dir)
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

/// 系统目录选择对话框（WinForms FolderBrowserDialog，STA；install.ps1 同款）。
/// 返回 None = 用户取消。脚本首行把控制台输出切 UTF-8——回传的路径可能含
/// 中文（用户目录名），PS5.1 默认 GBK 会被 UTF-8 解码成乱码路径。
#[tauri::command]
async fn pick_folder(title: String) -> Result<Option<String>, String> {
    let script = format!(
        r#"[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
Add-Type -AssemblyName System.Windows.Forms
$d = New-Object System.Windows.Forms.FolderBrowserDialog
$d.Description = '{}'
if ($d.ShowDialog() -eq 'OK') {{ Write-Output $d.SelectedPath }} else {{ Write-Output '' }}"#,
        title.replace('\'', "''")
    );
    // CREATE_NO_WINDOW 只隐藏控制台宿主（不闪 PowerShell 窗）；WinForms
    // 对话框是 GUI 窗口、由 STA 消息循环创建，不受其影响照常弹出。
    // spawn_blocking 里等，命令本身 async（不冻 UI）。
    let out = tauri::async_runtime::spawn_blocking(move || {
        std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-STA",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &script,
            ])
            .creation_flags(0x0800_0000)
            .output()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("打开目录选择失败：{e}"))?;
    // trim BOM：子进程输出万一带头 BOM（脚本侧已用无 BOM encoder，双保险）。
    let path = String::from_utf8_lossy(&out.stdout)
        .trim_start_matches('\u{feff}')
        .trim()
        .to_string();
    Ok(if path.is_empty() { None } else { Some(path) })
}

/// 直查 GitHub latest（不经桥——桥挂了升级入口恰恰必须可用）。
/// 返回完整 rel（tag/notes/zip_url/published_at）：前端更新面板展示日志，
/// 确认后原样传回 update_self 由客户端自主完成下载与应用。
#[tauri::command]
async fn check_update_direct() -> Result<serde_json::Value, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let cur = current.clone();
    let checked = tauri::async_runtime::spawn_blocking(move || queqiao::update::check_latest(&cur))
        .await
        .map_err(|e| e.to_string())??;
    Ok(match checked {
        None => serde_json::json!({ "hasUpdate": false, "current": current }),
        Some(rel) => serde_json::json!({
            "hasUpdate": true, "current": current,
            "rel": serde_json::to_value(&rel).unwrap_or_default(),
        }),
    })
}

/// 客户端自主更新（桥挂了也必须能升级——2026-10-24 真机死结的根治）：
/// 下载（进度经 `update-progress` 事件推送，前端渲染进度条）→ 解包 staging
/// → 写客户端版 apply bat（重启目标=客户端）→ spawn 后返回，前端提示
/// "即将重启"再调 quit_app 交棒：bat 等客户端退出后换三件套并重启新版。
#[tauri::command]
async fn update_self(
    app: tauri::AppHandle,
    state: tauri::State<'_, BridgeState>,
    rel: serde_json::Value,
) -> Result<(), String> {
    use tauri::Emitter;
    let rel: queqiao::update::ReleaseInfo =
        serde_json::from_value(rel).map_err(|e| format!("更新信息无效：{e}"))?;
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let staging = queqiao::update::stage_update_with_progress(&rel, &home, |recv, total| {
            let _ = app.emit(
                "update-progress",
                serde_json::json!({ "received": recv, "total": total }),
            );
        })?;
        let bat = queqiao::update::write_apply_bat(&staging, &home, true)?;
        std::process::Command::new(&bat)
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW——bat 不闪控制台
            .spawn()
            .map_err(|e| format!("启动更新脚本失败：{e}"))?;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 配合 update_self：前端渲染完"即将重启"后调用，退出交棒 apply bat。
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
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
    // 脚本侧已把输出切 UTF-8（无 BOM）；万一仍带头 BOM，去之（双保险）。
    let stdout = String::from_utf8_lossy(&out.stdout)
        .trim_start_matches('\u{feff}')
        .trim()
        .to_string();
    let stderr = String::from_utf8_lossy(&out.stderr)
        .trim_start_matches('\u{feff}')
        .trim()
        .to_string();
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
            migrate_from_node,
            check_update_direct,
            update_self,
            quit_app,
            pick_folder,
            start_snowluma
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

    /// httpUrl 缺位回落（2026-10-24 真机：config 从未有 httpUrl，OneBot 探测
    /// 恒假阴）——per-account 文件的 http 端点被拾取，坏 JSON 文件跳过。
    #[test]
    fn fallback_http_urls_reads_per_account_endpoints() {
        let dir = std::env::temp_dir().join(format!("qq-fb-{}", std::process::id()));
        let cfg = dir.join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("onebot_123.json"),
            r#"{"networks":{"httpServers":[{"accessToken":"t","host":"127.0.0.1","port":3456}]}}"#,
        )
        .unwrap();
        std::fs::write(cfg.join("onebot_456.json"), "{ not json").unwrap();
        assert_eq!(
            fallback_http_urls(&dir.display().to_string()),
            vec!["http://127.0.0.1:3456".to_owned()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// URL → 探测地址解析：IP 直用；localhost 走系统解析（原实现直接 parse
    /// SocketAddr 恒失败——主机名不是 IP 字面量）。
    #[test]
    fn probe_addrs_accepts_hostnames() {
        assert_eq!(
            probe_addrs("http://127.0.0.1:3000/"),
            vec!["127.0.0.1:3000".parse().unwrap()]
        );
        assert!(
            !probe_addrs("http://localhost:3000").is_empty(),
            "localhost 须能解析出探测地址"
        );
        assert!(probe_addrs("").is_empty());
    }
}

#[cfg(test)]
mod probe_fp_tests {
    /// 探测脚本必须排除 powershell 自身：探测命令行含 homeDir 明文，
    /// 双开客户端时 A 的探测命中 B 的探测 → SnowLuma 假阳"运行中"
    ///（2026-10-24 用户真机抓到：UI 显示运行中而实际未开）。
    #[test]
    fn probe_script_excludes_powershell_self() {
        let src = include_str!("main.rs");
        assert!(
            src.contains("$_.Name -ne 'powershell.exe' -and $_.CommandLine -like"),
            "snowluma 进程匹配必须排除 powershell（防探测自匹配假阳）"
        );
    }
}
