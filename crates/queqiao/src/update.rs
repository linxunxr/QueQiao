//! 自动更新：检测 GitHub Releases → 下载 staging → 生成应用脚本。
//!
//! 应用时机由主循环决定（空闲即应用，不打断进行中回合）。Windows 关键技巧：
//! 运行中的 exe 不能删除但**可以改名**——apply 脚本把旧 exe 改名 `.old`、新 exe
//! 拷入原路径、重启进程；`queqiao-mcp.exe` 即便正被 DSH 运行，rename 同样成功，
//! DSH 下次 spawn 自然用新版。`.old` 在下次更新时清理。
//!
//! 失败语义：检测/下载任一环节失败都静默降级（更新是增益不是依赖）。

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
#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub tag: String,
    pub notes: String,
    /// win64 zip 的下载直链。
    pub zip_url: String,
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
                .and_then(|a| a.get("browser_download_url"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .ok_or("release 无 win64 zip 资产")?;
    let notes = rel
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("（本次发布未附更新说明）")
        .to_owned();
    Ok(Some(ReleaseInfo {
        tag,
        notes,
        zip_url,
    }))
}

/// 下载并解压到 `<home>/update-staging/`（幂等：先清旧 staging）。
/// 解压用 PowerShell Expand-Archive（与安装器同路径，不引 zip 依赖）。
pub fn stage_update(rel: &ReleaseInfo, home: &Path) -> Result<PathBuf, String> {
    let staging = home.join("update-staging");
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| format!("清旧 staging 失败：{e}"))?;
    }
    std::fs::create_dir_all(&staging).map_err(|e| format!("建 staging 失败：{e}"))?;
    let zip = home.join("update.zip");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = client
        .get(&rel.zip_url)
        .send()
        .map_err(|e| format!("下载失败：{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载响应异常：HTTP {}", resp.status()));
    }
    let mut file = std::fs::File::create(&zip).map_err(|e| format!("写临时文件失败：{e}"))?;
    std::io::copy(&mut resp, &mut file).map_err(|e| format!("下载落盘失败：{e}"))?;
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
/// 清 staging → 重启主程序。文件锁靠 rename 化解（运行中 exe 可改名不可删除）。
pub fn write_apply_bat(staging: &Path, home: &Path) -> Result<PathBuf, String> {
    let bat = home.join("apply-update.bat");
    let swap = |name: &str| {
        format!(
            "if exist \"{name}\" ren \"{name}\" \"{name}.old\"\r\n\
             if exist \"{staging}\\{name}\" copy /y \"{staging}\\{name}\" \"{name}\" >nul\r\n",
            staging = staging.display()
        )
    };
    let content = format!(
        "@echo off\r\n\
         rem QueQiao auto-update applier - generated by queqiao {}\r\n\
         timeout /t 3 /nobreak >nul\r\n\
         cd /d \"{}\"\r\n\
         del queqiao.exe.old >nul 2>&1\r\n\
         del queqiao-mcp.exe.old >nul 2>&1\r\n\
         {}\
         {}\
         if exist \"{staging}\\install.bat\" copy /y \"{staging}\\install.bat\" install.bat >nul\r\n\
         if exist \"{staging}\\setup.bat\" copy /y \"{staging}\\setup.bat\" setup.bat >nul\r\n\
         if exist \"{staging}\\THIRD_PARTY_NOTICES.md\" copy /y \"{staging}\\THIRD_PARTY_NOTICES.md\" THIRD_PARTY_NOTICES.md >nul\r\n\
         if exist \"{staging}\\dsh\" xcopy /e /y /i \"{staging}\\dsh\" dist-preset-update >nul 2>&1\r\n\
         rd /s /q \"{staging}\" >nul 2>&1\r\n\
         if exist queqiao.exe start \"\" queqiao.exe\r\n",
        env!("CARGO_PKG_VERSION"),
        home.display(),
        swap("queqiao.exe"),
        swap("queqiao-mcp.exe"),
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
        let bat = write_apply_bat(&dir.join("update-staging"), &dir).unwrap();
        let content = std::fs::read_to_string(&bat).unwrap();
        assert!(content.contains("ren \"queqiao.exe\" \"queqiao.exe.old\""));
        assert!(content.contains("ren \"queqiao-mcp.exe\" \"queqiao-mcp.exe.old\""));
        assert!(content.contains("start \"\" queqiao.exe"));
        assert!(content.contains(&format!("cd /d \"{}\"", dir.display())));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
