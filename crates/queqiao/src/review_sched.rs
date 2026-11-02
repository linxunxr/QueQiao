//! 周期复盘素材与状态（M7 第一批次，规格 §十二 P3——判定纯函数在
//! core::review，本模块管素材组装与 state/review.json 读写）。
//!
//! 复盘素材是**全局**视角（区别于唤醒 prompt 的单会话素材）：各会话记忆
//! 概览 + 近期日记锚点 + 人格卡状态（版本/待审提议）+ 黑话统计——给 agent
//! 一个"整体盘点"的输入面。产出仅为提议走既有门控，素材只读。

use std::path::{Path, PathBuf};

use crate::social_store::SocialStore;

/// 复盘状态文件（state/review.json：仅 lastReviewAt——低频，独立小文件
/// 不动 social-v2.json 形状）。
pub fn review_state_path() -> PathBuf {
    crate::paths::queqiao_home()
        .join("state")
        .join("review.json")
}

/// 上次复盘时刻（0=从未）。
pub fn load_last_review_at() -> u64 {
    std::fs::read_to_string(review_state_path())
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("lastReviewAt").and_then(|x| x.as_u64()))
        .unwrap_or(0)
}

/// 落盘上次复盘时刻（原子写；失败仅日志——复盘非关键路径）。
pub fn save_last_review_at(now_ms: u64) {
    let text = serde_json::json!({ "lastReviewAt": now_ms }).to_string();
    let p = review_state_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap_or(Path::new(".")));
    if let Err(e) = crate::state::atomic_write_json(&p, &text) {
        crate::logging::log(&format!("[review] 状态落盘失败：{e}"));
    }
}

/// 组装【复盘素材】块（全局盘点视角；`host` 宿主会话标注"你在这里被唤醒"）。
pub fn build_review_material(home: &Path, social: &SocialStore, host: &str) -> String {
    let mut out =
        String::from("【复盘素材】以下是你现在的整体状态，只读盘点用。\n\n· 各会话记忆：\n");
    for key in social.conversation_keys() {
        let Some(st) = social.conversation(&key) else {
            continue;
        };
        let topics = &st.active_topics;
        let pending = &st.pending_thoughts;
        let impressions = st.member_impressions.len();
        let latest = topics.last().map(|t| t.text.as_str()).unwrap_or("（无）");
        let mark = if key == host {
            "（本次复盘在此会话唤醒）"
        } else {
            ""
        };
        out.push_str(&format!(
            "  - {key}{mark}：话题 {} 条（最新：{}）、待办想法 {} 条、群友印象 {} 人\n",
            topics.len(),
            latest.chars().take(40).collect::<String>(),
            pending.len(),
            impressions
        ));
    }
    // 近 3 篇日记锚点。
    let entries = queqiao_core::diary::parse_diary(
        &std::fs::read_to_string(crate::paths::diary_path()).unwrap_or_default(),
    );
    out.push_str("\n· 近 3 篇日记：\n");
    if entries.is_empty() {
        out.push_str("  （还没有日记）\n");
    } else {
        for e in entries.iter().rev().take(3) {
            out.push_str(&format!(
                "  - {}：{}\n",
                e.date,
                e.content.chars().take(50).collect::<String>()
            ));
        }
    }
    // 长期记忆库统计（M7-2：复盘是蒸馏主时机——素材给全局量，指引在
    // prompt 尾段「蒸馏值得长期保留的」）。
    let ltm = crate::longterm_store::open(&crate::longterm_store::db_path())
        .map(|c| crate::longterm_store::count(&c, None))
        .unwrap_or((0, 0, 0));
    out.push_str(&format!(
        "\n· 长期记忆（跨会话永久，qq_memory_query category=longterm 可查全文）：话题 {} 条、事实 {} 条、印象 {} 条\n",
        ltm.0, ltm.1, ltm.2
    ));
    // 人格卡状态：当前角色/版本/待审提议。
    let role = crate::role_card_store::current_role_name(home, host)
        .unwrap_or_else(|| "（未设置）".into());
    let revision = crate::role_card_store::current_revision(home, host)
        .map(|r| format!("版本 {r}"))
        .unwrap_or_else(|| "无卡".into());
    let pending_proposals = crate::role_card_store::load_proposals(home)
        .iter()
        .filter(|p| p.status == "pending")
        .count();
    out.push_str(&format!(
        "\n· 人格卡：{role}（{revision}，待审修改提议 {pending_proposals} 条）\n"
    ));
    // 黑话统计。
    let slang_counts = crate::slang_store::load(&crate::slang_store::slang_file(home))
        .map(|list| {
            let confirmed = list.iter().filter(|e| e.status == "confirmed").count();
            let candidate = list.iter().filter(|e| e.status == "candidate").count();
            (confirmed, candidate)
        })
        .unwrap_or((0, 0));
    out.push_str(&format!(
        "\n· 黑话：已确认 {} 条、待审候选 {} 条\n",
        slang_counts.0, slang_counts.1
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 状态读写往返 + 缺省 0。
    #[test]
    fn review_state_roundtrip() {
        let _env = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("qq-review-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        unsafe { std::env::set_var("QUEQIAO_HOME", &root) };
        assert_eq!(load_last_review_at(), 0, "缺省从未");
        save_last_review_at(12345);
        assert_eq!(load_last_review_at(), 12345);
    }

    /// 素材块：会话记忆概览 + 宿主标注 + 缺席日记/人格兜底文案。
    #[test]
    fn review_material_shape() {
        let _env = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("qq-review-mat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::create_dir_all(root.join("roles")).unwrap();
        unsafe { std::env::set_var("QUEQIAO_HOME", &root) };
        let mut social =
            SocialStore::load(&root.join("state").join("social-v2.json"), None).unwrap();
        social
            .conversation_mut("group:4242")
            .active_topics
            .push(crate::state::ActiveTopic {
                text: "最近在聊钓鱼".into(),
                ..Default::default()
            });
        social.save();
        let m = build_review_material(&root, &social, "group:4242");
        assert!(m.contains("【复盘素材】"));
        assert!(m.contains("group:4242（本次复盘在此会话唤醒）"));
        assert!(m.contains("话题 1 条（最新：最近在聊钓鱼）"), "{m}");
        assert!(m.contains("（还没有日记）"));
        assert!(m.contains("人格卡：（未设置）"), "{m}");
        assert!(m.contains("黑话：已确认 0 条、待审候选 0 条"));
    }
}
