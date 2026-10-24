//! 人格卡 IO 层：state/current-role.json 与 roles/<名称>.md 的读取。
//!
//! current-role.json 形状（Node 版同源）：`{"role": "小鲸鱼", "perConv":
//! {"group:123": "另一个人格"}}`——perConv 按会话覆盖全局角色。文件用户可
//! 直接编辑，人格名经 sanitize 白名单净化后才拼路径（core::role_card），
//! '//' 与路径穿越在净化层即被消除。唤醒是低频路径，不做 mtime 缓存。

use std::collections::BTreeMap;
use std::path::Path;

use queqiao_core::role_card::{role_hint_v2, sanitize_role_name};

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

#[cfg(test)]
mod tests {
    use super::*;

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
        let home = setup("a", &[("state/current-role.json", r#"{"role":"不存在"}"#)]);
        assert_eq!(hint_v2(&home, "group:1", 6000), "");
        let empty = setup("b0", &[]);
        assert_eq!(current_role_name(&empty, "group:1"), None);
        assert_eq!(hint_v2(&empty, "group:1", 6000), "");
    }

    #[test]
    fn traversal_name_neutralized() {
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
