//! 升级自检（v0.5.1 起：升级链自动生效——用户「检查更新」一键直达）。
//!
//! 两层职责：
//! ①**版本对账（每次启动）**：`queqiao-mcp/app.exe --version` 与桥自身版本
//!   比对——半更新检测（apply 三件套独立换装，可能只换了部分：桥版本没变
//!   但组件停旧版，2026-11-01 真机 0.5.0 那轮即桥件静默未换）。不一致时从
//!   `update-staging*/` 补装包自愈（apply 失败保留 staging 的配套设计；
//!   运行中 exe 可 ren：MCP 换后 kill 靠 DSH respawn 拉新，app 换后旧进程
//!   继续到用户重启）；无补装包则活动日志告警。
//! ②**版本升级自检（版本变化时）**：对比 `state/last-bridge-version.json`
//!   与自身版本，不同则：部署 preset（安装目录 `dsh\agent-presets\**` →
//!   `~/.dsh/agent-presets\**`，差异才写）+ kill 本机旧 MCP（DSH 对 stdio
//!   MCP 子进程有 respawn，2026-11-01 真机实证 kill 后 8s 自动拉新，**无需
//!   重启 DSH**）+ 退役全部 QQ 会话（DSH 会话创建时快照工具列表与 preset，
//!   重建后的新会话才用新版，顺带自愈会话快照残留）。
//! 同版本且组件一致 → 仅对账探测、零变更（幂等）。

use std::path::{Path, PathBuf};

/// 自检结果（notes 进活动日志；retire_needed 由编排层消费）。
pub struct ReconcileOutcome {
    pub retire_needed: bool,
    pub notes: Vec<String>,
}

/// DSH home（与 main 的凭据发现同源：DSH_HOME 覆盖 → %USERPROFILE%\.dsh）。
fn dsh_home() -> Option<PathBuf> {
    std::env::var("DSH_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("USERPROFILE")
                .ok()
                .map(|p| PathBuf::from(p).join(".dsh"))
        })
}

/// 升级自检主入口。`install_dir` = 桥 exe 所在目录（随包 preset 在其
/// `dsh\agent-presets`，补装包在 `update-staging*`）。IO 全部失败容忍
///（升级残留最坏是维持手动流程）。
pub fn run(install_dir: &Path) -> ReconcileOutcome {
    let version = env!("CARGO_PKG_VERSION");
    let marker = crate::paths::queqiao_home()
        .join("state")
        .join("last-bridge-version.json");
    let same = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("version").and_then(|x| x.as_str().map(String::from)))
        .is_some_and(|v| v == version);

    // ①版本对账（每次启动）：半更新检测与自愈。
    let mut notes = Vec::new();
    let mcp_patched = reconcile_component_versions(install_dir, &mut notes);
    if !same {
        notes.push(format!("检测到桥版本升级 → {version}，执行升级自检"));
        // ②部署 preset（差异才写）。
        let shipped = install_dir.join("dsh").join("agent-presets");
        match dsh_home() {
            Some(dsh) if shipped.is_dir() => {
                let (copied, skipped) = deploy_tree(&shipped, &dsh.join("agent-presets"));
                notes.push(format!(
                    "preset 部署：{copied} 个文件更新、{skipped} 个已是新版"
                ));
            }
            _ => notes
                .push("preset 部署跳过（随包 dsh/agent-presets 缺席或 DSH home 不可解）".into()),
        }
        // ③kill 本机旧 MCP（版本变化时即使对账未补拷也强杀一次——DSH
        //   respawn 拉起新映像，保障工具列表与桥一致）。
        let mcp_exe = install_dir.join("queqiao-mcp.exe");
        if mcp_exe.exists() {
            kill_mcp_at(&mcp_exe);
            notes.push("旧 MCP 进程已请求退出（DSH 将自动拉起新版，无需重启 DSH）".into());
        }
        // ④版本标记落盘。
        let _ = std::fs::create_dir_all(marker.parent().unwrap_or(Path::new(".")));
        let _ = std::fs::write(
            &marker,
            serde_json::json!({ "version": version }).to_string(),
        );
        ReconcileOutcome {
            retire_needed: true,
            notes,
        }
    } else {
        // 同版本但 MCP 自愈过 → 工具面变了，会话同样需要重建。
        ReconcileOutcome {
            retire_needed: mcp_patched,
            notes,
        }
    }
}

/// 三件套版本对账：spawn `<exe> --version`（输出 "<名称> <版本>"）与桥
/// 版本比对。MCP 旧版 → 补拷+kill（respawn 拉新），返回 true（工具面变化
/// 需要退役会话）；app 旧版 → 补拷（旧进程续跑到用户重启）。探测失败或
/// 版本一致不动。
fn reconcile_component_versions(install_dir: &Path, notes: &mut Vec<String>) -> bool {
    let me = env!("CARGO_PKG_VERSION");
    let mut mcp_patched = false;
    // (exe 名, 是否 kill respawn)。
    for (exe, kill_after) in [("queqiao-mcp.exe", true), ("queqiao-app.exe", false)] {
        let path = install_dir.join(exe);
        if !path.exists() {
            continue; // 组件缺席（最小部署形态）不算异常
        }
        let got = std::process::Command::new(&path)
            .arg("--version")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.split_whitespace().nth(1).map(String::from));
        match got {
            Some(v) if v == me => {}
            Some(v) => mcp_patched |= heal_component(install_dir, exe, &v, kill_after, notes),
            None => notes.push(format!("{exe} 版本探测失败（--version 无输出）")),
        }
    }
    mcp_patched
}

/// 旧组件自愈：找 apply 失败保留的补装包（两个执行者的 staging）→ 旧件
/// 转 .old → 补拷新件；MCP 补拷后 kill（DSH respawn 拉新）。返回是否
/// 动了 MCP（工具面变化 → 调用方触发会话退役）。
fn heal_component(
    install_dir: &Path,
    exe: &str,
    old_ver: &str,
    kill_after: bool,
    notes: &mut Vec<String>,
) -> bool {
    let me = env!("CARGO_PKG_VERSION");
    let path = install_dir.join(exe);
    let src = ["update-staging", "update-staging-app"]
        .iter()
        .map(|d| install_dir.join(d).join(exe))
        .find(|p| p.exists());
    match src {
        Some(src) => {
            let _ = std::fs::rename(&path, install_dir.join(format!("{exe}.old")));
            if std::fs::copy(&src, &path).is_ok() {
                if kill_after {
                    kill_mcp_at(&path);
                    notes.push(format!(
                        "{exe} 版本对账：{old_ver} → {me}（自补装包自愈，DSH 将拉起新进程）"
                    ));
                    true
                } else {
                    notes.push(format!(
                        "{exe} 版本对账：{old_ver} → {me}（自补装包自愈，重启客户端后生效）"
                    ));
                    false
                }
            } else {
                notes.push(format!("{exe} 补拷失败（{src:?} → {path:?}）"));
                false
            }
        }
        None => {
            notes.push(format!(
                "{exe} 版本 {old_ver} ≠ 桥 {me} 且无补装包——请手动「检查更新」重新下载"
            ));
            false
        }
    }
}

/// 递归部署：内容不同才写（返回 (更新数, 未变数)）。
fn deploy_tree(src: &Path, dst: &Path) -> (usize, usize) {
    let mut copied = 0;
    let mut skipped = 0;
    let Ok(rd) = std::fs::read_dir(src) else {
        return (0, 0);
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            let (c, s) = deploy_tree(&path, &target);
            copied += c;
            skipped += s;
        } else {
            let same = std::fs::read(&path)
                .ok()
                .zip(std::fs::read(&target).ok())
                .is_some_and(|(a, b)| a == b);
            if same {
                skipped += 1;
            } else {
                let _ = std::fs::create_dir_all(target.parent().unwrap_or(Path::new(".")));
                if std::fs::copy(&path, &target).is_ok() {
                    copied += 1;
                }
            }
        }
    }
    (copied, skipped)
}

/// 按 exe 全路径匹配并结束 queqiao-mcp 进程（PowerShell 单条命令；输出
/// 不关心——查不到进程属正常（MCP 未被拉起））。
fn kill_mcp_at(exe: &Path) {
    let exe = exe.to_string_lossy().replace('\'', "''");
    let script = format!(
        "Get-CimInstance Win32_Process -Filter \"Name='queqiao-mcp.exe'\" | \
         Where-Object {{ $_.ExecutablePath -eq '{exe}' }} | \
         ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}"
    );
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .output();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qq-upgrec-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 部署树：差异文件写入、相同跳过、目录递归。
    #[test]
    fn deploy_tree_copies_only_changed() {
        let root = temp_root("deploy");
        let src = root.join("src").join("agent-presets").join("qq-chat-v2");
        let dst = root.join("dst").join("agent-presets").join("qq-chat-v2");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("agent.cordis.yml"), "v2").unwrap();
        std::fs::write(src.join("preset.yml"), "p1").unwrap();

        let (c, s) = deploy_tree(&root.join("src"), &root.join("dst"));
        assert_eq!((c, s), (2, 0), "首次全量部署");
        assert_eq!(
            std::fs::read_to_string(dst.join("agent.cordis.yml")).unwrap(),
            "v2"
        );

        // 目标已同步 → 全跳过；源更新一个 → 只写一个。
        let (c, s) = deploy_tree(&root.join("src"), &root.join("dst"));
        assert_eq!((c, s), (0, 2));
        std::fs::write(src.join("agent.cordis.yml"), "v3").unwrap();
        let (c, s) = deploy_tree(&root.join("src"), &root.join("dst"));
        assert_eq!((c, s), (1, 1));
        assert_eq!(
            std::fs::read_to_string(dst.join("agent.cordis.yml")).unwrap(),
            "v3"
        );
    }

    /// 版本标记：首跑 retire=true 并落盘；同版本重跑零动作。
    ///（DSH_HOME 指向隔离目录防污染真机；组件对账用不存在的 exe 绕开。）
    #[test]
    fn marker_idempotent() {
        let _env = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = temp_root("marker");
        unsafe {
            std::env::set_var("QUEQIAO_HOME", &root);
            std::env::set_var("DSH_HOME", root.join("no-dsh"));
        }
        let install = root.join("install");
        std::fs::create_dir_all(&install).unwrap();
        // 安装目录无 queqiao-mcp/app.exe → 对账跳过；无 dsh/ → 部署跳过。
        let out = run(&install);
        assert!(out.retire_needed, "首跑（版本标记缺失）应触发退役");
        assert!(root.join("state").join("last-bridge-version.json").exists());
        let out2 = run(&install);
        assert!(!out2.retire_needed, "同版本重跑零动作");
        assert!(out2.notes.is_empty(), "组件缺席不算异常，无 note");
    }

    /// 组件自愈（heal_component 直测——spawn `--version` 探测需真 PE，留
    /// 真机验证）：旧版 + staging 补装包 → 覆盖+转 .old；无补装包 → 告警
    /// 不动文件。app 形态（kill_after=false）即验证补拷主链。
    #[test]
    fn component_heal_from_staging() {
        let root = temp_root("comp");
        let install = root.join("install");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(install.join("queqiao-app.exe"), "old-bytes").unwrap();
        let staging = install.join("update-staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("queqiao-app.exe"), "healed-payload").unwrap();

        let mut notes = Vec::new();
        let patched = heal_component(&install, "queqiao-app.exe", "0.0.1", false, &mut notes);
        assert!(!patched, "app 自愈不算 mcp_patched");
        assert_eq!(
            std::fs::read(install.join("queqiao-app.exe")).unwrap(),
            b"healed-payload",
            "旧件被补装包覆盖"
        );
        assert!(install.join("queqiao-app.exe.old").exists(), "旧件转 .old");
        assert!(
            notes.iter().any(|n| n.contains("版本对账")),
            "note 应记自愈：{notes:?}"
        );

        // staging 缺席的旧组件 → 告警 note 不动文件。
        std::fs::remove_dir_all(&staging).unwrap();
        std::fs::write(install.join("queqiao-app.exe"), "old-bytes-2").unwrap();
        let _ = std::fs::remove_file(install.join("queqiao-app.exe.old"));
        let mut notes2 = Vec::new();
        heal_component(&install, "queqiao-app.exe", "0.0.1", false, &mut notes2);
        assert_eq!(
            std::fs::read(install.join("queqiao-app.exe")).unwrap(),
            b"old-bytes-2",
            "无补装包不动文件"
        );
        assert!(
            notes2.iter().any(|n| n.contains("无补装包")),
            "无补装包应告警：{notes2:?}"
        );
    }
}
