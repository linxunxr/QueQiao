//! 自动更新：检测 GitHub Releases → 下载 staging → 生成应用脚本。
//!
//! 应用时机由主循环决定（空闲即应用，不打断进行中回合）。Windows 关键技巧：
//! 运行中的 exe 不能删除但**可以改名**——apply 脚本把旧 exe 改名 `.old`、新 exe
//! 拷入原路径、重启进程；`queqiao-mcp.exe` 即便正被 DSH 运行，rename 同样成功，
//! DSH 下次 spawn 自然用新版。`.old` 在下次更新时清理。
//!
//! 失败语义：检测/下载任一环节失败都静默降级（更新是增益不是依赖）。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

const REPO: &str = "linxunxr/QueQiao";

/// 解析 "0.1.2" → (0, 1, 2)；非法输入返回 None。
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut it = s.split('.');
    let maj = it.next()?.parse().ok()?;
    let min = it.next()?.parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    Some((maj, min, patch))
}

/// 一次 GitHub release 的信息。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReleaseInfo {
    pub tag: String,
    pub notes: String,
    /// win64 zip 的下载直链。
    pub zip_url: String,
    /// 发布时间（GitHub published_at 原文；更新面板展示用）。
    #[serde(default)]
    pub published_at: String,
}

/// 查询 GitHub latest release；比 `current_version` 新则返回 Some。
/// 任何失败（网络/解析/无资产）返回 Err（调用方静默降级）。
pub fn check_latest(current_version: &str) -> Result<Option<ReleaseInfo>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("queqiao-update-check")
        .build()
        .map_err(|e| e.to_string())?;
    let rel: Value = client
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .header("accept", "application/vnd.github+json")
        .send()
        .map_err(|e| format!("GitHub API 请求失败：{e}"))?
        .error_for_status()
        .map_err(|e| format!("GitHub API 响应异常：{e}"))?
        .json()
        .map_err(|e| format!("GitHub API 解析失败：{e}"))?;
    let tag = rel
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or("release 缺 tag_name")?
        .to_owned();
    let (cur, new) = match (
        parse_version(current_version),
        parse_version(tag.trim_start_matches('v')),
    ) {
        (Some(c), Some(n)) => (c, n),
        _ => {
            return Err(format!(
                "版本号解析失败：current={current_version} tag={tag}"
            ));
        }
    };
    if new <= cur {
        return Ok(None);
    }
    // win64 zip 资产（release workflow 命名 queqiao-<tag>-win64.zip）。
    // 下载走 **API asset 端点**而非 browser_download_url：本机网络常见
    // api.github.com 通而 github.com 下载域断（gh CLI 同款通道），且 API
    // 端点对 Range/重试更友好。
    let zip_url = rel
        .get("assets")
        .and_then(Value::as_array)
        .and_then(|assets| {
            assets
                .iter()
                .find(|a| {
                    a.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| n.ends_with("-win64.zip"))
                })
                .and_then(|a| a.get("id"))
                .and_then(Value::as_u64)
                .map(|id| format!("https://api.github.com/repos/{REPO}/releases/assets/{id}"))
        })
        .ok_or("release 无 win64 zip 资产")?;
    let notes = rel
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("（本次发布未附更新说明）")
        .to_owned();
    let published_at = rel
        .get("published_at")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(Some(ReleaseInfo {
        tag,
        notes,
        zip_url,
        published_at,
    }))
}

/// 下载并解压到 `<home>/update-staging/`（幂等：先清旧 staging）。
/// 解压用 PowerShell Expand-Archive（与安装器同路径，不引 zip 依赖）。
pub fn stage_update(rel: &ReleaseInfo, home: &Path) -> Result<PathBuf, String> {
    stage_update_with_progress(rel, home, |_, _| {})
}

/// 带进度的下载（`progress(received, total)`，total=Content-Length，chunked
/// 时为 0——前端按有无可换双态显示）。桥侧自动更新无 UI 走空回调包装。
pub fn stage_update_with_progress<F: Fn(u64, u64)>(
    rel: &ReleaseInfo,
    home: &Path,
    progress: F,
) -> Result<PathBuf, String> {
    let staging = home.join("update-staging");
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| format!("清旧 staging 失败：{e}"))?;
    }
    std::fs::create_dir_all(&staging).map_err(|e| format!("建 staging 失败：{e}"))?;
    let zip = home.join("update.zip");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        // API asset 端点匿名请求必须带 UA（无 UA 一律 403）。
        .user_agent("queqiao-update-check")
        .build()
        .map_err(|e| e.to_string())?;
    // 直连 GitHub 常见抖动（连接层失败非超时）——三次重试，间隔 2s。
    let mut resp = None;
    let mut last_err = String::new();
    for attempt in 0..3 {
        match client
            .get(&rel.zip_url)
            // API asset 端点必须带此 Accept 才返回文件流（否则回 asset 元数据 JSON）。
            .header("accept", "application/octet-stream")
            .send()
        {
            Ok(r) if r.status().is_success() => {
                resp = Some(r);
                break;
            }
            Ok(r) => last_err = format!("HTTP {}", r.status()),
            Err(e) => last_err = e.to_string(),
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }
    let mut resp = resp.ok_or_else(|| format!("下载失败（重试 3 次）：{last_err}"))?;
    let total = resp.content_length().unwrap_or(0);
    let mut file = std::fs::File::create(&zip).map_err(|e| format!("写临时文件失败：{e}"))?;
    let mut received = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = resp.read(&mut buf).map_err(|e| format!("下载中断：{e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("下载落盘失败：{e}"))?;
        received += n as u64;
        progress(received, total);
    }
    drop(file);
    let status = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
                zip.display(),
                staging.display()
            ),
        ])
        .status()
        .map_err(|e| format!("解压进程启动失败：{e}"))?;
    let _ = std::fs::remove_file(&zip);
    if !status.success() {
        return Err("解压失败（Expand-Archive 非零退出）".to_owned());
    }
    Ok(staging)
}

/// 生成 apply-update.bat：等待旧进程退出 → 旧 exe 改名 .old → 新 exe 拷入 →
/// 清 staging → 重启。文件锁靠 rename 化解（运行中 exe 可改名不可删除）。
/// `restart_app=false` 重启桥（桥侧自动更新）；`true` 重启客户端（客户端
/// 自主更新——客户端启动默认拉桥，桥随之恢复）。
pub fn write_apply_bat(staging: &Path, home: &Path, restart_app: bool) -> Result<PathBuf, String> {
    let bat = home.join("apply-update.bat");
    let swap = |name: &str| {
        format!(
            "if exist \"{name}\" ren \"{name}\" \"{name}.old\"\r\n\
             if exist \"{staging}\\{name}\" copy /y \"{staging}\\{name}\" \"{name}\" >nul\r\n",
            staging = staging.display()
        )
    };
    let restart_target = if restart_app {
        "queqiao-app.exe"
    } else {
        "queqiao.exe"
    };
    let content = format!(
        "@echo off\r\n\
         rem QueQiao auto-update applier - generated by queqiao {}\r\n\
         timeout /t 3 /nobreak >nul\r\n\
         cd /d \"{}\"\r\n\
         del queqiao.exe.old >nul 2>&1\r\n\
         del queqiao-mcp.exe.old >nul 2>&1\r\n\
         del queqiao-app.exe.old >nul 2>&1\r\n\
         {}\
         {}\
         {}\
         if exist \"{staging}\\install.bat\" copy /y \"{staging}\\install.bat\" install.bat >nul\r\n\
         if exist \"{staging}\\upgrade.bat\" copy /y \"{staging}\\upgrade.bat\" upgrade.bat >nul\r\n\
         if exist \"{staging}\\migrate-from-node.ps1\" copy /y \"{staging}\\migrate-from-node.ps1\" migrate-from-node.ps1 >nul\r\n\
         if exist \"{staging}\\THIRD_PARTY_NOTICES.md\" copy /y \"{staging}\\THIRD_PARTY_NOTICES.md\" THIRD_PARTY_NOTICES.md >nul\r\n\
         if exist \"{staging}\\dsh\" xcopy /e /y /i \"{staging}\\dsh\" dist-preset-update >nul 2>&1\r\n\
         rd /s /q \"{staging}\" >nul 2>&1\r\n\
         if exist {restart_target} start \"\" {restart_target}\r\n",
        env!("CARGO_PKG_VERSION"),
        home.display(),
        swap("queqiao.exe"),
        swap("queqiao-mcp.exe"),
        // 客户端一并换：正在运行亦可 rename（Windows 锁删除不锁改名），退出后
        // 重开即新版——否则客户端停在旧版，总览/一键启动等 UI 永远不更新。
        swap("queqiao-app.exe"),
        staging = staging.display()
    );
    std::fs::write(&bat, content).map_err(|e| format!("写 apply 脚本失败：{e}"))?;
    Ok(bat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(parse_version("0.1.1") > parse_version("0.1.0"));
        assert!(parse_version("0.2.0") > parse_version("0.1.9"));
        assert!(parse_version("1.0.0") > parse_version("0.9.9"));
        assert_eq!(parse_version("0.1.0"), parse_version("0.1.0"));
        assert_eq!(
            parse_version("0.1"),
            parse_version("0.1.0"),
            "缺 patch 位按 0"
        );
        assert!(parse_version("abc").is_none());
    }

    #[test]
    fn apply_bat_contains_swap_logic() {
        let dir = std::env::temp_dir().join(format!("queqiao-upd-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("update-staging")).unwrap();
        let bat = write_apply_bat(&dir.join("update-staging"), &dir, false).unwrap();
        let content = std::fs::read_to_string(&bat).unwrap();
        assert!(content.contains("ren \"queqiao.exe\" \"queqiao.exe.old\""));
        assert!(content.contains("ren \"queqiao-mcp.exe\" \"queqiao-mcp.exe.old\""));
        assert!(content.contains("ren \"queqiao-app.exe\" \"queqiao-app.exe.old\""));
        assert!(content.contains("del queqiao-app.exe.old"));
        // 迁移脚本必须随更新补齐（客户端"从 Node 版迁移"按钮运行时硬依赖；
        // 与 upgrade.ps1 同款缺口——2026-10-24 真机存量目录缺它，点按钮即报错）。
        assert!(content.contains(&format!(
            "copy /y \"{}\\migrate-from-node.ps1\" migrate-from-node.ps1",
            dir.join("update-staging").display()
        )));
        assert!(content.contains("start \"\" queqiao.exe"));
        assert!(content.contains(&format!("cd /d \"{}\"", dir.display())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 客户端自主更新变体：重启目标必须是客户端（桥侧变体重启桥）。
    /// 两个变体各用独立目录——同一 home 下 bat 路径相同，后写会覆盖先写。
    #[test]
    fn apply_bat_restart_target_variants() {
        let base = std::env::temp_dir().join(format!("queqiao-upd-app-{}", std::process::id()));
        let dir_b = base.join("bridge-home");
        let dir_a = base.join("app-home");
        std::fs::create_dir_all(dir_b.join("update-staging")).unwrap();
        std::fs::create_dir_all(dir_a.join("update-staging")).unwrap();
        let bridge = write_apply_bat(&dir_b.join("update-staging"), &dir_b, false).unwrap();
        let app = write_apply_bat(&dir_a.join("update-staging"), &dir_a, true).unwrap();
        let b = std::fs::read_to_string(&bridge).unwrap();
        let a = std::fs::read_to_string(&app).unwrap();
        assert!(b.contains("if exist queqiao.exe start \"\" queqiao.exe"));
        assert!(a.contains("if exist queqiao-app.exe start \"\" queqiao-app.exe"));
        assert!(!a.contains("if exist queqiao.exe start"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
