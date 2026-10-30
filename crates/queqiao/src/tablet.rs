//! 鸿蒙平板 hdc 执行器（Node `mcp-tablet.js` 的 hdc 面移植）。
//!
//! 分层：本模块只负责与 hdc.exe 打交道（定位/带超时执行/目标解析/文件
//! 传输/串行闸）；鉴权（allowConvKeys 双层门禁）与 HTTP 端点在
//! `tools_api`；UI 树压缩等纯判定在 `queqiao_core::tablet`。
//!
//! 能力面刻意收敛（与 Node 同款，不做 tablet_shell/任意 hdc 命令）：
//! 截图 / 读屏 / 点击 / 滑动 / 快滑 / 按键 / 输入 / 开网页。

#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::config::TabletConfig;

/// hdc 串行闸：hdc server 对同一目标的并发调用偶发拒绝，排队比加锁重试
/// 简单可靠（Node hdcChain 同款）。
static HDC_CHAIN: Mutex<()> = Mutex::new(());

pub struct HdcOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// hdc.exe 定位（Node findHdc 顺序）：config 显式路径 → PATH → DevEco
/// Studio 默认安装位置。返回字符串（可能只是 "hdc" 交给 PATH 兜底）。
fn find_hdc(cfg: &TabletConfig) -> Result<String, String> {
    let configured = cfg.hdc_path.trim();
    if !configured.is_empty() {
        if Path::new(configured).exists() {
            return Ok(configured.to_owned());
        }
        return Err(format!(
            "tablet.hdcPath 指向的文件不存在：{configured}（请改填 DevEco Studio 的 sdk/<版本>/openharmony/toolchains/hdc.exe）"
        ));
    }
    let mut roots: Vec<String> = Vec::new();
    if let Ok(pf) = std::env::var("ProgramFiles") {
        roots.push(pf);
    }
    if let Ok(pf86) = std::env::var("ProgramFiles(x86)") {
        roots.push(pf86);
    }
    roots.push("C:\\Program Files".into());
    for root in roots {
        let sdk_dir = Path::new(&root)
            .join("Huawei")
            .join("DevEco Studio")
            .join("sdk");
        let mut versions: Vec<_> = match std::fs::read_dir(&sdk_dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect(),
            Err(_) => continue,
        };
        versions.sort();
        for v in versions.iter().rev() {
            let candidate = sdk_dir
                .join(v)
                .join("openharmony")
                .join("toolchains")
                .join("hdc.exe");
            if candidate.exists() {
                return Ok(candidate.display().to_string());
            }
        }
    }
    Ok("hdc".into())
}

/// 带超时执行一条 hdc 命令（串行闸内）。超时 kill 子进程并报错——hdc 卡死
/// 不拖垮桥（std 无 wait_timeout，用 try_wait 轮询）。
pub fn run_hdc(cfg: &TabletConfig, args: &[&str], timeout_ms: u64) -> Result<HdcOutput, String> {
    let _guard = HDC_CHAIN.lock().map_err(|e| e.to_string())?;
    let hdc = find_hdc(cfg)?;
    let mut cmd = std::process::Command::new(&hdc);
    cmd.args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = cmd.spawn().map_err(|e| {
        format!("无法启动 hdc（{hdc}）：{e}；可在 config.json 的 tablet.hdcPath 填绝对路径")
    })?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = child.stdout.take().map(read_all).unwrap_or_default();
                let stderr = child.stderr.take().map(read_all).unwrap_or_default();
                return Ok(HdcOutput {
                    code: status.code().unwrap_or(-1),
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    return Err(format!(
                        "hdc 命令超时（{timeout_ms}ms）：hdc {}",
                        args.join(" ")
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(format!("hdc 执行失败：{e}")),
        }
    }
}

fn read_all<R: std::io::Read>(mut r: R) -> String {
    let mut buf = Vec::new();
    let _ = r.read_to_end(&mut buf);
    // hdc 输出混有 GBK/UTF-8，容错解码（UTF-8 优先，失败按本地编码字节有损）。
    String::from_utf8(buf.clone()).unwrap_or_else(|_| String::from_utf8_lossy(&buf).into_owned())
}

fn fail_sniff(out: &HdcOutput) -> Result<&HdcOutput, String> {
    match queqiao_core::tablet::hdc_output_failed(&out.stdout, &out.stderr) {
        Some(summary) => Err(summary),
        None => Ok(out),
    }
}

fn host_port(s: &str) -> bool {
    let (host, port) = match s.split_once(':') {
        Some(x) => x,
        None => return false,
    };
    !host.is_empty()
        && host.split('.').count() == 4
        && host.split('.').all(|p| {
            !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u8>().is_ok()
        })
        && !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.len() <= 5
}

/// 解析目标设备（Node resolveTarget）：配置了 `tablet.target`（IP:端口）先
/// tconn 自愈掉线；未配置则取唯一在线设备（多台报错要求指定）。
fn resolve_target(cfg: &TabletConfig) -> Result<String, String> {
    let target_cfg = cfg.target.trim().to_owned();
    if !target_cfg.is_empty() && host_port(&target_cfg) {
        // 幂等 tconn；失败不拦截，用在线列表给出准确报错。
        let _ = run_hdc(cfg, &["tconn", &target_cfg], 8000);
    }
    let out = run_hdc(cfg, &["list", "targets"], 8000)?;
    let lines: Vec<String> = out
        .stdout
        .lines()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty() && s != "[Empty]" && !s.starts_with("IO "))
        .collect();
    if !target_cfg.is_empty() {
        if !lines.contains(&target_cfg) {
            return Err(format!(
                "目标设备 {target_cfg} 不在线。无线调试的端口在平板重启/重开后会变：到 设置→系统→开发者选项→无线调试 查看当前端口，更新 config.json 的 tablet.target"
            ));
        }
        return Ok(target_cfg);
    }
    match lines.len() {
        0 => Err("没有在线的鸿蒙设备（hdc list targets 为空）。平板需开启开发者选项，并完成无线调试配对/USB 连接".into()),
        1 => Ok(lines[0].clone()),
        _ => Err(format!(
            "检测到多台设备（{}）：请在 config.json 的 tablet.target 指定其中一台",
            lines.join("、")
        )),
    }
}

/// 设备会话：解析一次目标，后续命令带 `-t`。
pub struct Device<'a> {
    cfg: &'a TabletConfig,
    target: String,
}

pub fn with_device<F>(cfg: &TabletConfig, f: F) -> Result<String, String>
where
    F: FnOnce(&Device) -> Result<String, String>,
{
    let target = resolve_target(cfg)?;
    f(&Device {
        cfg,
        target: target.clone(),
    })
}

impl Device<'_> {
    fn hdc(&self, args: &[&str], timeout_ms: u64) -> Result<HdcOutput, String> {
        let mut full: Vec<&str> = vec!["-t", &self.target];
        full.extend_from_slice(args);
        run_hdc(self.cfg, &full, timeout_ms)
    }

    pub fn shell(&self, args: &[&str], timeout_ms: u64) -> Result<String, String> {
        let mut full: Vec<&str> = vec!["shell"];
        full.extend_from_slice(args);
        let out = self.hdc(&full, timeout_ms)?;
        fail_sniff(&out).map(|o| o.stdout.trim().to_owned())
    }
}

fn shots_dir() -> PathBuf {
    crate::paths::queqiao_home()
        .join("state")
        .join("tablet-shots")
}

fn prune_shots(keep: usize) {
    let dir = shots_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<_> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("shot-"))
        .collect();
    files.sort();
    if files.len() > keep {
        for name in &files[..files.len() - keep] {
            let _ = std::fs::remove_file(dir.join(name));
        }
    }
}

/// 截图（Node captureScreenshot）：uitest screenCap → file recv → 本地留档
/// （保留 12 张）→ base64。超 maxScreenshotBytes 拒绝并提示。
pub fn screenshot(cfg: &TabletConfig) -> Result<String, String> {
    use base64::Engine;
    let stamp = format!(
        "{}-{}",
        crate::session_manager::SessionManager::now_ms(),
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
            % 0xffff
    );
    let remote = format!("/data/local/tmp/queqiao-shot-{stamp}.jpeg");
    let max_bytes = cfg.max_screenshot_bytes.max(1);
    with_device(cfg, |dev| {
        dev.shell(&["uitest", "screenCap", "-p", &remote], 25_000)?;
        std::fs::create_dir_all(shots_dir()).map_err(|e| format!("建截图目录失败：{e}"))?;
        let local = shots_dir().join(format!("shot-{stamp}.jpeg"));
        let out = dev.hdc(&["file", "recv", &remote, &local.to_string_lossy()], 30_000)?;
        fail_sniff(&out)?;
        let _ = dev.hdc(&["shell", "rm", "-f", &remote], 8_000);
        let bytes = std::fs::read(&local).map_err(|e| format!("读截图失败：{e}"))?;
        if bytes.len() as u64 > max_bytes {
            return Err(format!(
                "截图 {:.1}MB 超过上限（tablet.maxScreenshotBytes={max_bytes}）：换个画面（如先 Home 回桌面）再截，或调大上限",
                bytes.len() as f64 / 1048576.0
            ));
        }
        prune_shots(12);
        Ok(serde_json::json!({
            "image": { "mimeType": "image/jpeg", "data": base64::engine::general_purpose::STANDARD.encode(&bytes) },
            "file": local.file_name().unwrap_or_default().to_string_lossy(),
            "bytes": bytes.len(),
        }).to_string())
    })
}

/// 读屏（Node readScreen）：dumpLayout → 压缩树文本（max_chars 截断）。
pub fn read_screen(cfg: &TabletConfig, max_chars: usize) -> Result<String, String> {
    let stamp = format!(
        "{}-{}",
        crate::session_manager::SessionManager::now_ms(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
            % 0xffff
    );
    let remote = format!("/data/local/tmp/queqiao-layout-{stamp}.json");
    let local = shots_dir().join(format!("layout-{stamp}.json"));
    let result = with_device(cfg, |dev| {
        std::fs::create_dir_all(shots_dir()).ok();
        dev.shell(&["uitest", "dumpLayout", "-p", &remote], 25_000)?;
        let out = dev.hdc(&["file", "recv", &remote, &local.to_string_lossy()], 20_000)?;
        fail_sniff(&out)?;
        let text = std::fs::read_to_string(&local).map_err(|e| format!("读布局失败：{e}"))?;
        let root: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("布局 JSON 解析失败：{e}"))?;
        Ok(queqiao_core::tablet::condense_layout(&root, max_chars))
    });
    let _ = std::fs::remove_file(&local);
    let _ = with_device(cfg, |dev| {
        let _ = dev.hdc(&["shell", "rm", "-f", &remote], 8_000);
        Ok(String::new())
    });
    result
}

/// 中文/特殊字符走设备 shell 内的 base64 解码（Node shellSafeText）：
/// 文本里的空格/引号/换行不会把 hdc shell 参数拆坏。
fn shell_safe_text(text: &str) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("$(printf '%s' '{b64}' | base64 -d)")
}

/// hdc 把 argv 空格拼接交给设备 shell 再解析——URL 等含元字符的参数须自包
/// 单引号（Node shellQuote）。
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn tap(cfg: &TabletConfig, x: i64, y: i64) -> Result<String, String> {
    with_device(cfg, |dev| {
        dev.shell(
            &["uitest", "uiInput", "click", &x.to_string(), &y.to_string()],
            20_000,
        )?;
        Ok("ok".to_owned())
    })
}

pub fn swipe(
    cfg: &TabletConfig,
    x1: i64,
    y1: i64,
    x2: i64,
    y2: i64,
    speed: Option<i64>,
) -> Result<String, String> {
    with_device(cfg, |dev| {
        let mut args: Vec<String> = vec![
            "uitest".into(),
            "uiInput".into(),
            "swipe".into(),
            x1.to_string(),
            y1.to_string(),
            x2.to_string(),
            y2.to_string(),
        ];
        if let Some(s) = speed {
            args.push(s.to_string());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        dev.shell(&refs, 20_000)?;
        Ok("ok".to_owned())
    })
}

pub fn fling(cfg: &TabletConfig, dir: &str) -> Result<String, String> {
    let num = queqiao_core::tablet::fling_direction(dir)
        .ok_or_else(|| format!("无效方向：{dir}（up/down/left/right）"))?;
    with_device(cfg, |dev| {
        dev.shell(
            &["uitest", "uiInput", "dircFling", &num.to_string()],
            20_000,
        )?;
        Ok("ok".to_owned())
    })
}

pub fn key(cfg: &TabletConfig, key: &str) -> Result<String, String> {
    if !queqiao_core::tablet::valid_key(key) {
        return Err(format!("无效按键：{key}（Back / Home / Power 或数字键码）"));
    }
    with_device(cfg, |dev| {
        dev.shell(&["uitest", "uiInput", "keyEvent", key], 20_000)?;
        Ok("ok".to_owned())
    })
}

pub fn input_text(
    cfg: &TabletConfig,
    text: &str,
    x: Option<i64>,
    y: Option<i64>,
) -> Result<String, String> {
    let enc = shell_safe_text(text);
    with_device(cfg, |dev| match (x, y) {
        (Some(x), Some(y)) => {
            dev.shell(
                &[
                    "uitest",
                    "uiInput",
                    "inputText",
                    &x.to_string(),
                    &y.to_string(),
                    &enc,
                ],
                20_000,
            )?;
            Ok("ok".to_owned())
        }
        _ => {
            dev.shell(&["uitest", "uiInput", "text", &enc], 20_000)?;
            Ok("ok".to_owned())
        }
    })
}

pub fn open_url(cfg: &TabletConfig, url: &str) -> Result<String, String> {
    let url = url.trim();
    // 同浏览器导航硬边界（browser::guard_url）：平板 WebView 的 file://
    // 无正常用途（本机路径读取面），本机/内网一律拒——file 开关不给
    // 平板留（浏览器侧的 allowFileUrls 不适用于此）。
    crate::browser::guard_url(url, false)?;
    let quoted = shell_quote(url);
    with_device(cfg, |dev| {
        let out = dev.shell(&["aa", "start", "-U", &quoted], 20_000)?;
        // aa start 成功输出 "start ability successfully."；失败只打 "error: ..."。
        if !out.to_lowercase().contains("successfully") {
            let first = out.lines().next().unwrap_or("无输出");
            return Err(format!("打开失败：{first}"));
        }
        Ok("ok".to_owned())
    })
}

/// 只读探测（Node tablet_status）：功能开关 + 设备在线状态 + 型号。
pub fn status(cfg: &TabletConfig) -> serde_json::Value {
    if !cfg.enabled {
        return serde_json::json!({
            "enabled": false,
            "hint": "平板工具未启用：在 config.json 设置 tablet.enabled=true 并填 tablet.target 后重启 DSH"
        });
    }
    let mut info = serde_json::json!({
        "enabled": true,
        "target": if cfg.target.trim().is_empty() { "(自动选择唯一在线设备)".to_owned() } else { cfg.target.trim().to_owned() }
    });
    match resolve_target(cfg) {
        Ok(target) => {
            info["online"] = serde_json::json!(true);
            info["target"] = serde_json::json!(target);
            let dev = Device { cfg, target };
            info["device"] = serde_json::json!({
                "model": dev.shell(&["param", "get", "const.product.model"], 10_000).unwrap_or_default(),
                "name": dev.shell(&["param", "get", "const.product.name"], 10_000).unwrap_or_default(),
            });
        }
        Err(e) => {
            info["online"] = serde_json::json!(false);
            info["hint"] = serde_json::json!(e);
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_format() {
        assert!(host_port("192.168.3.27:36271"));
        assert!(host_port("127.0.0.1:8080"));
        assert!(!host_port("abc:123"));
        assert!(!host_port("192.168.3:8080"));
        assert!(!host_port("192.168.3.999:8080"));
        assert!(!host_port("nopercent"));
    }

    /// shell 防注入包装：base64 解码与单引号转义。
    #[test]
    fn shell_wrapping() {
        assert!(shell_safe_text("你好 world").starts_with("$(printf '%s' '"));
        let q = shell_quote("https://a.b/c?x=1&y='z'");
        assert!(q.starts_with('\'') && q.ends_with('\''));
        assert!(q.contains("'\\''"), "内部单引号转义");
    }
}
