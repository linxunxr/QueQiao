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
/// Gitee 镜像（云函数 gitee-sync 中转主包；仅主包——Gitee 单附件上限
/// 100MB，browser 资产 352MB 上不去）。查询走列表端点取第一条：Gitee v5
/// 的 releases/latest 语义不如列表稳（灵鉴项目同款用法）。
const GITEE_REPO: &str = "mwcxlinxun/QueQiao";

/// 解析 "0.1.2" → (0, 1, 2)；非法输入返回 None。
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut it = s.split('.');
    let maj = it.next()?.parse().ok()?;
    let min = it.next()?.parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    Some((maj, min, patch))
}

/// 一次 release 的信息。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReleaseInfo {
    pub tag: String,
    pub notes: String,
    /// win64 zip 的下载直链。
    pub zip_url: String,
    /// 备源直链（主源下载失败时换源重试）：Gitee 命中时填 GitHub API
    /// asset 端点。Gitee 附件可能滞后缺失（同步是发版后的异步增强步），
    /// 下载失败不能像 tauri-plugin-updater 那样硬失败（灵鉴事故教训）。
    #[serde(default)]
    pub fallback_zip_url: Option<String>,
    /// 发布时间（GitHub published_at 原文；更新面板展示用）。
    #[serde(default)]
    pub published_at: String,
}

/// 查询最新 release：Gitee 镜像优先（国内快），未命中/查询失败回落 GitHub。
/// 任何失败（网络/解析/无资产）返回 Err（调用方静默降级）。
///
/// 源选择语义：只有 Gitee 明确报「有新版」才采用 Gitee——镜像可能落后
/// （同步失败/未配置），Gitee 说「没新版」不可信，一律再问 GitHub。
pub fn check_latest(current_version: &str) -> Result<Option<ReleaseInfo>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("queqiao-update-check")
        .build()
        .map_err(|e| e.to_string())?;
    // Gitee 匿名查询（公开仓库）；失败/无更新都回落 GitHub。
    if let Ok(Some(mut info)) = check_gitee(&client, current_version) {
        // Gitee 命中后顺手查 GitHub 拿备源（查询失败不阻塞 Gitee 更新）。
        if let Ok(Some(gh)) = check_github(&client, current_version)
            && gh.tag == info.tag
        {
            info.fallback_zip_url = Some(gh.zip_url);
        }
        return Ok(Some(info));
    }
    check_github(&client, current_version)
}

/// 当前版本不落后于 release tag 时返回 Ok(None)。
fn not_newer(current_version: &str, tag: &str) -> Result<bool, String> {
    match (
        parse_version(current_version),
        parse_version(tag.trim_start_matches('v')),
    ) {
        (Some(c), Some(n)) => Ok(n <= c),
        _ => Err(format!(
            "版本号解析失败：current={current_version} tag={tag}"
        )),
    }
}

fn check_github(
    client: &reqwest::blocking::Client,
    current_version: &str,
) -> Result<Option<ReleaseInfo>, String> {
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
    github_release_info(&rel, current_version)
}

/// GitHub release JSON → ReleaseInfo（纯函数，供单测）。
fn github_release_info(rel: &Value, current_version: &str) -> Result<Option<ReleaseInfo>, String> {
    let tag = rel
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or("release 缺 tag_name")?
        .to_owned();
    if not_newer(current_version, &tag)? {
        return Ok(None);
    }
    // win64 zip 资产（release workflow 命名 queqiao-<tag>-win64.zip）。
    // 下载走 **API asset 端点**而非 browser_download_url：本机网络常见
    // api.github.com 通而 github.com 下载域断（gh CLI 同款通道），且 API
    // 端点对 Range/重试更友好。
    // 资产精确匹配 queqiao-<tag>-win64.zip：v0.4.10 起 Release 还携带
    // queqiao-browser-<pos>-win64.zip（352MB Chromium 专属资产），此前的
    // ends_with("-win64.zip") 会命中它——更新器把 352MB 当更新包下载
    // （真机事故：19KB/s 下 5 小时，下完解压出的还不是三件套，apply 必
    // 报废安装）。精确名 + 双重前缀排除。
    let want = format!("queqiao-{tag}-win64.zip");
    let zip_url = rel
        .get("assets")
        .and_then(Value::as_array)
        .and_then(|assets| {
            assets
                .iter()
                .find(|a| {
                    a.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| n == want)
                })
                .and_then(|a| a.get("id"))
                .and_then(Value::as_u64)
                .map(|id| format!("https://api.github.com/repos/{REPO}/releases/assets/{id}"))
        })
        .ok_or("release 无主包资产（queqiao-<tag>-win64.zip）")?;
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
        fallback_zip_url: None,
        published_at,
    }))
}

fn check_gitee(
    client: &reqwest::blocking::Client,
    current_version: &str,
) -> Result<Option<ReleaseInfo>, String> {
    let list: Value = client
        .get(format!(
            "https://gitee.com/api/v5/repos/{GITEE_REPO}/releases?direction=desc&page=1&per_page=1"
        ))
        .send()
        .map_err(|e| format!("Gitee API 请求失败：{e}"))?
        .error_for_status()
        .map_err(|e| format!("Gitee API 响应异常：{e}"))?
        .json()
        .map_err(|e| format!("Gitee API 解析失败：{e}"))?;
    let rel = list
        .as_array()
        .and_then(|a| a.first())
        .ok_or("Gitee 无发行版（镜像未同步）")?;
    gitee_release_info(rel, current_version)
}

/// Gitee release JSON → ReleaseInfo（纯函数，供单测）。
/// Gitee 结构差异：附件直链在 assets[].browser_download_url（attach_files
/// 恒空——云函数同步侧同款双字段兜底的教训）；时间字段是 created_at。
fn gitee_release_info(rel: &Value, current_version: &str) -> Result<Option<ReleaseInfo>, String> {
    let tag = rel
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or("release 缺 tag_name")?
        .to_owned();
    if not_newer(current_version, &tag)? {
        return Ok(None);
    }
    let want = format!("queqiao-{tag}-win64.zip");
    let zip_url = rel
        .get("assets")
        .and_then(Value::as_array)
        .and_then(|assets| {
            assets.iter().find(|a| {
                a.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n == want)
            })
        })
        .and_then(|a| a.get("browser_download_url"))
        .and_then(Value::as_str)
        .ok_or("Gitee 发行版无主包附件（镜像同步未跑或未完成）")?
        .to_owned();
    let notes = rel
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("（本次发布未附更新说明）")
        .to_owned();
    let published_at = rel
        .get("created_at")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(Some(ReleaseInfo {
        tag,
        notes,
        zip_url,
        fallback_zip_url: None,
        published_at,
    }))
}

/// 更新互斥锁（2026-10-26 真机事故根治）：桥自动更新与客户端自主更新是
/// 两个独立执行者，曾共用同一 staging 目录与同一 apply-update.bat——双触发
/// 时一方的"清旧 staging"会掏空另一方刚下好的 staging，apply bat 拷贝扑空、
/// 三件套被改名后新件缺失（客户端消失）。现按 tag 隔离目录与 bat 名，文件锁
/// 兜底跨执行者互斥；锁超时 20 分钟（覆盖 3×300s 连接重试 + 下载最坏情形），
/// bat 执行完删除，过期视为残留可抢占。
const UPDATE_LOCK_STALE_MS: u64 = 20 * 60_000;

fn acquire_update_lock(home: &Path, tag: &str) -> Result<(), String> {
    let lock = home.join("update.lock");
    if let Ok(meta) = std::fs::metadata(&lock) {
        let age = meta
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if age < UPDATE_LOCK_STALE_MS {
            return Err(format!(
                "另一更新器正在执行（update.lock {age}ms 前写入）——本次跳过，稍后会自动重试"
            ));
        }
    }
    std::fs::write(
        &lock,
        format!(
            "tag={}\ntime={}\n",
            tag,
            crate::logging::local_timestamp().unwrap_or_default()
        ),
    )
    .map_err(|e| format!("写 update.lock 失败：{e}"))?;
    Ok(())
}

/// staging 目录按执行者隔离：客户端 `update-staging-app`，桥 `update-staging`。
fn staging_dir(home: &Path, tag: &str) -> PathBuf {
    if tag == "app" {
        home.join("update-staging-app")
    } else {
        home.join("update-staging")
    }
}

/// 下载并解压（幂等：先清旧 staging）。`tag`: "bridge"（桥自动更新）/
/// "app"（客户端自主更新）——决定隔离目录与 bat 名。
/// 解压用 PowerShell Expand-Archive（与安装器同路径，不引 zip 依赖）。
pub fn stage_update(rel: &ReleaseInfo, home: &Path, tag: &str) -> Result<PathBuf, String> {
    stage_update_with_progress(rel, home, tag, |_, _| {})
}

/// 带进度的下载（`progress(received, total)`，total=Content-Length，chunked
/// 时为 0——前端按有无可换双态显示）。桥侧自动更新无 UI 走空回调包装。
pub fn stage_update_with_progress<F: Fn(u64, u64)>(
    rel: &ReleaseInfo,
    home: &Path,
    tag: &str,
    progress: F,
) -> Result<PathBuf, String> {
    acquire_update_lock(home, tag)?;
    let staging = staging_dir(home, tag);
    let zip = if tag == "app" {
        home.join("update-app.zip")
    } else {
        home.join("update.zip")
    };
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| format!("清旧 staging 失败：{e}"))?;
    }
    std::fs::create_dir_all(&staging).map_err(|e| format!("建 staging 失败：{e}"))?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        // API asset 端点匿名请求必须带 UA（无 UA 一律 403）。
        .user_agent("queqiao-update-check")
        .build()
        .map_err(|e| e.to_string())?;
    // 直连抖动（连接层失败非超时）常见——主源 3 次重试；仍失败且有备源
    // （Gitee 命中时填了 GitHub asset 端点）换源再来 3 次。下载失败不回退
    // 是 tauri-plugin-updater 的硬伤（灵鉴事故：Gitee 附件缺失即更新失败），
    // 自研更新器把换源做进重试链。
    let mut resp = None;
    let mut last_err = String::new();
    let mut tried_fallback = false;
    // 显式构造尝试序列：主源 →（可选）备源。
    let mut attempt_urls = vec![rel.zip_url.clone()];
    if let Some(fb) = &rel.fallback_zip_url {
        attempt_urls.push(fb.clone());
    }
    for url in &attempt_urls {
        for attempt in 0..3 {
            match client
                .get(url)
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
        if resp.is_some() {
            break;
        }
        if attempt_urls.len() > 1 {
            tried_fallback = true;
        }
    }
    let mut resp = resp.ok_or_else(|| {
        format!(
            "下载失败（主备源各重试 3 次）：{last_err}{}",
            if tried_fallback {
                "（已含备源）"
            } else {
                ""
            }
        )
    })?;
    let total = resp.content_length().unwrap_or(0);
    // 连接建立即发一帧 0 进度——慢网下首块数据前界面不再死寂（准备下载→已连接）。
    progress(0, total);
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

/// 生成 apply bat：等待旧进程退出 → 旧 exe 改名 .old → 新 exe 拷入 →
/// 清 staging → 删更新锁 → 重启。文件锁靠 rename 化解（运行中 exe 可改名
/// 不可删除）。`restart_app=false` 桥侧自动更新（apply-update.bat，staging
/// update-staging）；`true` 客户端自主更新（**apply-update-app.bat**，staging
/// update-staging-app——bat 名与目录按执行者隔离，双更新器并发触发不再互
/// 踩）。全程写 update-apply.log（此前每步 >nul 吞错，换新失败零痕迹可查）。
pub fn write_apply_bat(staging: &Path, home: &Path, restart_app: bool) -> Result<PathBuf, String> {
    let bat = if restart_app {
        home.join("apply-update-app.bat")
    } else {
        home.join("apply-update.bat")
    };
    let swap = |name: &str| {
        // 三行独立 if（不用括号块串联 &&：cmd 在部分环境报"此时不应有
        // &&"，2026-10-26 drill 实测换新链中断）；copy 结果落 update-apply.log。
        format!(
            "if exist \"{name}\" ren \"{name}\" \"{name}.old\"\r\n\
             if exist \"{staging}\\{name}\" copy /y \"{staging}\\{name}\" \"{name}\" >nul\r\n\
             if exist \"{name}\" (echo [ok] {name}) else echo [FAIL] missing {name} >>\"%LOG%\"\r\n",
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
         rem QueQiao auto-update applier - generated by queqiao {} ({})\r\n\
         set \"LOG=%~dp0update-apply.log\"\r\n\
         echo [%date% %time%] apply begin (target={restart_target}) >\"%LOG%\"\r\n\
         ping -n 4 127.0.0.1 >nul\r\n\
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
         del update.lock >nul 2>&1\r\n\
         echo [%date% %time%] apply done >>\"%LOG%\"\r\n\
         if exist {restart_target} start \"\" {restart_target}\r\n",
        env!("CARGO_PKG_VERSION"),
        if restart_app { "app" } else { "bridge" },
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

    /// Gitee release JSON 解析：附件直链取 assets[].browser_download_url
    /// （attach_files 恒空）；无主包附件报错（镜像同步未跑）。
    #[test]
    fn gitee_release_info_parsing() {
        let rel = serde_json::json!({
            "tag_name": "v9.9.9",
            "body": "更新说明",
            "created_at": "2026-10-29T12:00:00+08:00",
            "attach_files": [],
            "assets": [
                { "name": "queqiao-browser-1710741-win64.zip", "browser_download_url": "https://gitee.com/x/y/releases/download/v9.9.9/browser.zip" },
                { "name": "queqiao-v9.9.9-win64.zip", "browser_download_url": "https://gitee.com/mwcxlinxun/QueQiao/releases/download/v9.9.9/queqiao-v9.9.9-win64.zip" }
            ]
        });
        let info = gitee_release_info(&rel, "0.4.13").unwrap().expect("有新版");
        assert_eq!(info.tag, "v9.9.9");
        assert!(
            info.zip_url.ends_with("queqiao-v9.9.9-win64.zip"),
            "取主包直链非 browser 资产：{}",
            info.zip_url
        );
        assert_eq!(info.published_at, "2026-10-29T12:00:00+08:00");
        assert!(info.fallback_zip_url.is_none());

        // 无主包附件（同步未跑）：Err 而非 Ok(None)——调用方据此回落 GitHub。
        let empty = serde_json::json!({ "tag_name": "v9.9.9", "assets": [] });
        assert!(gitee_release_info(&empty, "0.4.13").is_err());

        // 版本不新：Ok(None)。
        let same = serde_json::json!({ "tag_name": "v0.4.13", "assets": [
            { "name": "queqiao-v0.4.13-win64.zip", "browser_download_url": "u" }
        ]});
        assert!(gitee_release_info(&same, "0.4.13").unwrap().is_none());
    }

    /// GitHub release JSON 解析回归：仍走 API asset 端点；新字段不破坏旧语义。
    #[test]
    fn github_release_info_parsing() {
        let rel = serde_json::json!({
            "tag_name": "v9.9.9",
            "body": "notes",
            "published_at": "2026-10-29T05:00:00Z",
            "assets": [
                { "name": "queqiao-v9.9.9-win64.zip", "id": 12345 }
            ]
        });
        let info = github_release_info(&rel, "0.4.13")
            .unwrap()
            .expect("有新版");
        assert_eq!(
            info.zip_url,
            "https://api.github.com/repos/linxunxr/QueQiao/releases/assets/12345"
        );
        assert!(info.fallback_zip_url.is_none());
    }

    /// ReleaseInfo 新增 fallback 字段向后兼容：旧 JSON（无该字段）可反序列化。
    #[test]
    fn release_info_deser_compat() {
        let old = r#"{"tag":"v0.4.13","notes":"n","zip_url":"u","published_at":"t"}"#;
        let info: ReleaseInfo = serde_json::from_str(old).unwrap();
        assert!(info.fallback_zip_url.is_none());
    }

    /// 更新互斥锁：锁存在即拒绝（防双更新器并发踩踏）；无锁可获取并落盘。
    #[test]
    fn update_lock_mutual_exclusion() {
        let dir = std::env::temp_dir().join(format!("qq-upd-lock-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(acquire_update_lock(&dir, "app").is_ok(), "无锁可获取");
        assert!(
            acquire_update_lock(&dir, "bridge").is_err(),
            "锁存在时另一执行者被拒"
        );
        assert!(dir.join("update.lock").exists(), "锁已落盘");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// staging/bat 按执行者隔离：客户端与桥互不共用目录与脚本名。
    #[test]
    fn staging_isolated_by_tag() {
        let home = std::path::Path::new("D:/nonexistent-home");
        assert_eq!(
            staging_dir(home, "app").file_name().unwrap(),
            "update-staging-app"
        );
        assert_eq!(
            staging_dir(home, "bridge").file_name().unwrap(),
            "update-staging"
        );
    }

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

#[cfg(test)]
mod asset_pick_tests {

    /// 资产选择必须精确命中主包：双资产时代 ends_with 会先命中 352MB 的
    /// browser 资产（v0.4.10 真机事故：更新器把 Chromium 包当更新包下了
    /// 5 小时）。此测试钉住选择函数的输入输出形状——check_latest 的完整
    /// 网络链不测，只测选择谓词与构造。
    #[test]
    fn main_asset_exact_name_wins_over_browser_asset() {
        let tag = "v9.9.9";
        let want = format!("queqiao-{tag}-win64.zip");
        let names = [
            ("queqiao-browser-1710741-win64.zip", 111u64),
            ("queqiao-v9.9.9-win64.zip", 222u64),
            ("queqiao-v9.9.9-win64.zip.sha256", 333u64),
        ];
        // 与 check_latest 同款谓词：精确名匹配（而非 ends_with）。
        let picked = names.iter().find(|(n, _)| *n == want).map(|(_, id)| *id);
        assert_eq!(picked, Some(222), "精确命中主包 id，不被 browser 资产抢先");
        // 反证：旧的 ends_with 谓词会命中 browser 资产——这就是事故本身。
        let old_predicate = names
            .iter()
            .find(|(n, _)| n.ends_with("-win64.zip"))
            .map(|(_, id)| *id);
        assert_eq!(
            old_predicate,
            Some(111),
            "旧谓词确实先命中 browser 资产（事故复现）"
        );
    }
}
