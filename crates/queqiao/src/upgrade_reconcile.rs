//! 升级自检（v0.5.1 起：升级链自动生效——用户「检查更新」一键直达）。
//!
//! 背景断链（v0.5.0 发现）：apply bat 曾把随包 preset 拷到安装目录
//! `dist-preset-update` 后无人消费（upgrade.ps1 无 preset 部署步）——本版
//! apply 已改为直接落 `dsh\agent-presets`。桥启动时对比
//! `state/last-bridge-version.json` 与自身版本，**不同**则：
//! ①部署 preset：安装目录 `dsh\agent-presets\**` → `~/.dsh/agent-presets\**`
//!   （内容差异才写，幂等）；
//! ②kill 本机 queqiao-mcp.exe（按安装目录 exe 全路径匹配 PID）——DSH 对
//!   stdio MCP 子进程有 respawn（2026-11-01 真机实证：kill 后 8s 内自动
//!   拉起新进程），**无需重启 DSH** 即可换新 MCP（新工具列表）；
//! ③返回 `retire_needed=true` → 编排层退役全部 QQ 会话：DSH 会话在创建
//!   时快照工具列表与 preset，重建后的新会话才用新版（顺带自愈 v0.4.15
//!   类「会话快照毒 schema 不自愈」的升级残留）。
//! 版本相同 → 零动作（重启桥不重复部署/退役）。

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
/// `dsh\agent-presets`）。IO 全部失败容忍（升级残留最坏是维持手动流程）。
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
    if same {
        return ReconcileOutcome {
            retire_needed: false,
            notes: Vec::new(),
        };
    }

    let mut notes = vec![format!("检测到桥版本升级 → {version}，执行升级自检")];

    // ①部署 preset（差异才写）。
    let shipped = install_dir.join("dsh").join("agent-presets");
    match dsh_home() {
        Some(dsh) if shipped.is_dir() => {
            let (copied, skipped) = deploy_tree(&shipped, &dsh.join("agent-presets"));
            notes.push(format!(
                "preset 部署：{copied} 个文件更新、{skipped} 个已是新版"
            ));
        }
        _ => notes.push("preset 部署跳过（随包 dsh/agent-presets 缺席或 DSH home 不可解）".into()),
    }

    // ②kill 本机旧 MCP（DSH respawn 拉新进程；按安装目录 exe 全路径匹配，
    //   绝不误杀别处同名进程）。
    let mcp_exe = install_dir.join("queqiao-mcp.exe");
    if mcp_exe.exists() {
        kill_mcp_at(&mcp_exe);
        notes.push("旧 MCP 进程已请求退出（DSH 将自动拉起新版，无需重启 DSH）".into());
    }

    // ③版本标记落盘（退役标记返回给编排层）。
    let _ = std::fs::create_dir_all(marker.parent().unwrap_or(Path::new(".")));
    let _ = std::fs::write(
        &marker,
        serde_json::json!({ "version": version }).to_string(),
    );

    ReconcileOutcome {
        retire_needed: true,
        notes,
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
    ///（DSH_HOME 指向隔离目录防污染真机；kill 分支用不存在的安装目录绕开。）
    #[test]
    fn marker_idempotent() {
        let root = temp_root("marker");
        unsafe {
            std::env::set_var("QUEQIAO_HOME", &root);
            std::env::set_var("DSH_HOME", root.join("no-dsh"));
        }
        let install = root.join("install");
        std::fs::create_dir_all(&install).unwrap();
        // 安装目录无 queqiao-mcp.exe → kill 分支跳过；无 dsh/ → 部署跳过。
        let out = run(&install);
        assert!(out.retire_needed, "首跑（版本标记缺失）应触发退役");
        assert!(root.join("state").join("last-bridge-version.json").exists());
        let out2 = run(&install);
        assert!(!out2.retire_needed, "同版本重跑零动作");
        assert!(out2.notes.is_empty());
    }
}
