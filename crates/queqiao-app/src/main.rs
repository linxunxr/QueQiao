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
            bridge_stop
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            {
                let state = app.state::<BridgeState>();
                let found = find_bridge(&handle);
                *state.path.lock().unwrap() = found.clone();
                if let Some(p) = found {
                    println!("[app] 桥程序：{}", p.display());
                    // 自动启动桥（崩溃由退出事件通知，UI 可手动重启）。
                    if let Err(e) = state.spawn(&handle) {
                        println!("[app] {e}");
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
