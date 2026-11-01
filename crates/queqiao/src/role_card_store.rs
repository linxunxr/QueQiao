//! 人格卡 IO 层：state/current-role.json 与 roles/<名称>.md 的读取。
//!
//! current-role.json 形状（Node 版同源）：`{"role": "小鲸鱼", "perConv":
//! {"group:123": "另一个人格"}}`——perConv 按会话覆盖全局角色。文件用户可
//! 直接编辑，人格名经 sanitize 白名单净化后才拼路径（core::role_card），
//! '//' 与路径穿越在净化层即被消除。唤醒是低频路径，不做 mtime 缓存。

use std::collections::BTreeMap;
use std::path::Path;

use queqiao_core::role_card::{role_hint_v2, sanitize_role_name};
use sha2::{Digest, Sha256};

#[derive(Debug, Default, serde::Deserialize)]
struct RoleStateFile {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    #[serde(rename = "perConv")]
    per_conv: BTreeMap<String, String>,
}

/// 角色名合法性（列卡/写入共用 core 判定）。
pub fn sanitize_output_name(name: &str) -> bool {
    sanitize_role_name(name).is_some()
}

/// 当前会话生效的角色名（perConv 覆盖全局；净化后拼 roles/<名>.md）。
pub fn current_role_name(home: &Path, conv_key: &str) -> Option<String> {
    let raw = std::fs::read_to_string(home.join("state").join("current-role.json")).ok()?;
    let state: RoleStateFile = serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok()?;
    let name = state
        .per_conv
        .get(conv_key)
        .cloned()
        .or_else(|| state.role.clone())?;
    sanitize_role_name(&name)
}

/// V2 注入用全文人格卡（模式筛小节 + 一代指令兜底过滤 + 截断）。
/// 角色未设置 / 卡片不存在 / 名非法 → 空串（调用方省略注入）。
pub fn hint_v2(home: &Path, conv_key: &str, max_chars: usize) -> String {
    let Some(name) = current_role_name(home, conv_key) else {
        return String::new();
    };
    let roles_dir = home.join("roles");
    let card = roles_dir.join(format!("{name}.md"));
    // 净化已消除穿越；canonical 校验兜底（ROLES_DIR 之外拒读）。
    let Ok(resolved) = card.canonicalize() else {
        return String::new();
    };
    let Ok(base) = roles_dir.canonicalize() else {
        return String::new();
    };
    if !resolved.starts_with(base.join("")) {
        return String::new();
    }
    match std::fs::read_to_string(&resolved) {
        Ok(text) => role_hint_v2(&text, max_chars),
        Err(_) => String::new(),
    }
}

// ── 人格卡版本化（M6 P0 第三项）───────────────────────────────────────────
// 修订号 = 内容 sha256 前 8 hex（跨 Rust 版本稳定，可落盘做标识）；编辑时旧
// 版归档 state/roles-history/<名>-<rev>.md（每卡保留最近 8 份）；会话在
// qq_get_prompt 首读时绑定修订号，卡被编辑后老会话仍读绑定版（防漂移），
// 绑定决策见 core::role_card::resolve_role_view。

/// 内容修订号：sha256 前 8 hex。
pub fn card_revision(content: &str) -> String {
    let d = Sha256::digest(content.as_bytes());
    d.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// 修订号合法性（拼归档路径前的防御——绑定值来自状态文件，可被手改）。
fn rev_ok(r: &str) -> bool {
    r.len() == 8 && r.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 指定修订号的卡全文：当前文件匹配读当前；否则查 roles-history 归档；
/// 归档缺失回落当前内容（版本找不到时保可用性优先于保版本）。
fn card_content_at(home: &Path, name: &str, rev: Option<&str>) -> Option<String> {
    let cur = std::fs::read_to_string(home.join("roles").join(format!("{name}.md"))).ok();
    let rev = rev.filter(|r| rev_ok(r));
    match rev {
        None => cur,
        Some(r) => {
            if cur.as_ref().is_some_and(|c| card_revision(c) == r) {
                cur
            } else {
                std::fs::read_to_string(
                    home.join("state")
                        .join("roles-history")
                        .join(format!("{name}-{r}.md")),
                )
                .ok()
                .or(cur)
            }
        }
    }
}

/// 按修订号取 V2 注入卡（版本化核心：热更后老会话仍读绑定版）。
pub fn hint_v2_at(home: &Path, conv_key: &str, max_chars: usize, rev: Option<&str>) -> String {
    let Some(name) = current_role_name(home, conv_key) else {
        return String::new();
    };
    match card_content_at(home, &name, rev) {
        Some(text) => role_hint_v2(&text, max_chars),
        None => String::new(),
    }
}

/// 某卡的历史归档路径（name 已在调用方净化）。
fn history_dir(home: &Path) -> std::path::PathBuf {
    home.join("state").join("roles-history")
}

/// 编辑前归档旧版（内容变化才归档；新建卡无旧版）；每卡保留最近 8 份。
pub fn archive_if_changed(home: &Path, name: &str, new_content: &str) {
    let Ok(old) = std::fs::read_to_string(home.join("roles").join(format!("{name}.md"))) else {
        return;
    };
    if card_revision(&old) == card_revision(new_content) {
        return;
    }
    let dir = history_dir(home);
    let _ = std::fs::create_dir_all(&dir);
    let rev = card_revision(&old);
    let _ = std::fs::write(dir.join(format!("{name}-{rev}.md")), &old);
    prune_history(&dir, name, 8);
}

/// 裁掉该卡最老的历史归档，保留最近 keep 份。
fn prune_history(dir: &Path, name: &str, keep: usize) {
    let prefix = format!("{name}-");
    let mut olds: Vec<(std::time::SystemTime, std::path::PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.starts_with(&prefix) && n.ends_with(".md")
        })
        .filter_map(|e| {
            e.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| (t, e.path()))
        })
        .collect();
    olds.sort();
    while olds.len() > keep {
        let (_, p) = olds.remove(0);
        let _ = std::fs::remove_file(p);
    }
}

/// 某卡的历史修订号列表（旧→新按 mtime；控制台展示用）。
pub fn history_revisions(home: &Path, name: &str) -> Vec<String> {
    let prefix = format!("{name}-");
    let mut out: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(history_dir(home))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.starts_with(&prefix) && n.ends_with(".md")
        })
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            e.metadata().ok().and_then(|m| m.modified().ok()).map(|t| {
                (
                    t,
                    n.trim_start_matches(&prefix)
                        .trim_end_matches(".md")
                        .to_owned(),
                )
            })
        })
        .collect();
    out.sort();
    out.into_iter().map(|(_, r)| r).collect()
}

// ── 人格卡修改提议（M6 P1-3 受治理自修订：agent 提议 → owner 批准）─────

/// 提议文件形状（state/role-proposals.json）。
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ProposalFile {
    proposals: Vec<queqiao_core::role_card::RoleProposal>,
}

fn proposals_file(home: &Path) -> std::path::PathBuf {
    home.join("state").join("role-proposals.json")
}

/// 读全部提议（文件缺失 → 空）。
pub fn load_proposals(home: &Path) -> Vec<queqiao_core::role_card::RoleProposal> {
    std::fs::read_to_string(proposals_file(home))
        .ok()
        .and_then(|raw| {
            serde_json::from_str::<ProposalFile>(raw.trim_start_matches('\u{feff}')).ok()
        })
        .map(|f| f.proposals)
        .unwrap_or_default()
}

/// 落盘提议（原子写；失败仅日志——提议丢了可重提，不阻塞调用方）。
pub fn save_proposals(home: &Path, list: &[queqiao_core::role_card::RoleProposal]) {
    let text = serde_json::to_string_pretty(&ProposalFile {
        proposals: list.to_vec(),
    })
    .unwrap_or_else(|_| "{\n  \"proposals\": []\n}".into());
    if let Err(e) = crate::state::atomic_write_json(&proposals_file(home), &text) {
        crate::logging::log(&format!("[role-proposal] 提议落盘失败：{e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 文件系统测试互斥（同一 temp 根下的目录操作存在偶发竞态，实测 1/30
    /// 复现——串行化消除；锁粒度到模块即可）。
    static FS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn fs_guard() -> std::sync::MutexGuard<'static, ()> {
        FS_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn setup(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("qq-role-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).unwrap();
        std::fs::create_dir_all(dir.join("roles")).unwrap();
        for (name, content) in files {
            let p = dir.join(name);
            std::fs::write(p, content).unwrap();
        }
        dir
    }

    #[test]
    fn global_and_per_conv_role() {
        let _fs = fs_guard();
        let home = setup(
            "a",
            &[
                (
                    "state/current-role.json",
                    r#"{"role":"小鲸鱼","perConv":{"group:9":"阿蓝"}}"#,
                ),
                ("roles/小鲸鱼.md", "# 你是谁\n全局人格"),
                ("roles/阿蓝.md", "# 你是谁\n会话人格"),
            ],
        );
        assert_eq!(
            current_role_name(&home, "group:1").as_deref(),
            Some("小鲸鱼")
        );
        assert_eq!(current_role_name(&home, "group:9").as_deref(), Some("阿蓝"));
        let hint = hint_v2(&home, "group:9", 6000);
        assert!(hint.contains("会话人格"), "perConv 覆盖生效");
        assert!(hint_v2(&home, "group:1", 6000).contains("全局人格"));
    }

    #[test]
    fn missing_role_or_card_returns_empty() {
        let _fs = fs_guard();
        let home = setup("a", &[("state/current-role.json", r#"{"role":"不存在"}"#)]);
        assert_eq!(hint_v2(&home, "group:1", 6000), "");
        let empty = setup("b0", &[]);
        assert_eq!(current_role_name(&empty, "group:1"), None);
        assert_eq!(hint_v2(&empty, "group:1", 6000), "");
    }

    /// 版本化：修订号稳定；内容变化才归档；按修订号读绑定版；归档缺失与
    /// 非法修订号都回落当前内容；每卡历史至多 8 份。
    #[test]
    fn revision_archive_and_pinned_read() {
        let _fs = fs_guard();
        let home = setup(
            "rev",
            &[("state/current-role.json", r#"{"role":"小鲸鱼"}"#)],
        );
        let card = home.join("roles").join("小鲸鱼.md");
        let v1 = "# 你是谁\n第一版";
        let v2 = "# 你是谁\n第二版";
        assert_eq!(card_revision(v1), card_revision(v1), "同内容同修订号");
        assert_ne!(card_revision(v1), card_revision(v2));

        std::fs::write(&card, v1).unwrap();
        let rev1 = card_revision(v1);
        archive_if_changed(&home, "小鲸鱼", v1);
        assert!(
            history_revisions(&home, "小鲸鱼").is_empty(),
            "内容未变不归档"
        );
        archive_if_changed(&home, "小鲸鱼", v2);
        // archive 只归档旧版，新内容落卡由调用方（put 端点）写。
        std::fs::write(&card, v2).unwrap();
        assert_eq!(history_revisions(&home, "小鲸鱼"), vec![rev1.clone()]);

        // 绑定 rev1 → 读到第一版；不指定 → 当前第二版。
        assert!(hint_v2_at(&home, "group:1", 6000, Some(&rev1)).contains("第一版"));
        assert!(hint_v2_at(&home, "group:1", 6000, None).contains("第二版"));
        // 归档缺失（伪造绑定）与非法修订号（路径防御）都回落当前内容。
        assert!(hint_v2_at(&home, "group:1", 6000, Some("00000000")).contains("第二版"));
        assert!(
            hint_v2_at(&home, "group:1", 6000, Some("../../x")).contains("第二版"),
            "非法修订号不拼路径"
        );

        // 裁剪上限：连改 12 版，历史至多 8 份（mtime 同拍下留哪份不保证，数量保证）。
        for i in 0..12 {
            let next = format!("# 你是谁\n循环{i}");
            archive_if_changed(&home, "小鲸鱼", &next);
            std::fs::write(&card, &next).unwrap();
        }
        assert!(
            history_revisions(&home, "小鲸鱼").len() <= 8,
            "每卡历史至多 8 份"
        );
    }

    #[test]
    fn traversal_name_neutralized() {
        let _fs = fs_guard();
        // 用户手改 current-role.json 写恶意名：净化层剥掉路径字符后若指向
        // 不存在的卡即返回空——不存在读 ROLES_DIR 之外文件的可能。
        let home = setup(
            "a",
            &[
                ("state/current-role.json", r#"{"role":"../../etc/passwd"}"#),
                ("roles/etcpasswd.md", "# 你是谁\n伪卡"),
            ],
        );
        let name = current_role_name(&home, "group:1").unwrap();
        assert!(!name.contains('/') && !name.contains('.'));
        let hint = hint_v2(&home, "group:1", 6000);
        // 净化名 etcpasswd 的卡若真存在则读它（仍在 roles/ 内）；关键是绝不越出 roles/。
        let leaked = hint.contains("etc:") || hint.contains("root:");
        assert!(!leaked, "绝不读 roles/ 之外的文件");
    }
}

#[cfg(test)]
mod real_tests {
    use super::*;

    /// 真机回归（--ignored 手动跑）：读 Node 版桥真实角色卡与 current-role.json，
    /// 验证迁移数据即刻可消费。依赖 D:\100work\103Tools\qq-bridge 存在。
    #[test]
    #[ignore]
    fn real_node_bridge_cards() {
        let home = std::path::Path::new(r"D:\100work\103Tools\qq-bridge");
        if !home.join("state/current-role.json").exists() {
            eprintln!("跳过：Node 版桥不存在");
            return;
        }
        let name = current_role_name(home, "group:none");
        eprintln!("当前角色：{name:?}");
        let hint = hint_v2(home, "group:none", 6000);
        eprintln!(
            "V2 注入卡（{} 字）前 200：\n{}",
            hint.chars().count(),
            hint.chars().take(200).collect::<String>()
        );
        assert!(name.is_some(), "current-role.json 应有全局角色");
        assert!(!hint.is_empty(), "V2 装配应产出非空注入");
        assert!(!hint.contains("[SILENT]"), "一代指令不得漏进 V2 注入");
        eprintln!(
            "小节报告：{:?}",
            queqiao_core::role_card::parse_role_sections(
                &std::fs::read_to_string(home.join("roles").join(format!("{}.md", name.unwrap())))
                    .unwrap()
            )
            .iter()
            .map(|s| (
                s.heading.clone().unwrap_or_default(),
                matches!(s.mode, queqiao_core::role_card::SectionMode::V2),
                matches!(s.mode, queqiao_core::role_card::SectionMode::V1)
            ))
            .collect::<Vec<_>>()
        );
    }
}
