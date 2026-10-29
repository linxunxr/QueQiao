//! 轻量记忆（Node 版 appendMemoryV2 / formatMemoryV2 语义移植）。
//!
//! 三类记忆，各有上限与过期（防无限膨胀）：
//! - **activeTopic** 进行中的话题（lastMentionAt 超 24h 清除；同文本刷新而非重复入队）
//! - **pendingThought** 想说未说（expiresAt 过期清除，默认 2h）
//! - **memberImpression** 对群友的印象（traits 去重追加，超 50 人按 lastSeenAt 淘汰）
//!
//! 召回分层（2026-10 改造，对齐业界 working 注入 / semantic 检索的分法）：
//! 唤醒 prompt 只注入索引——话题/想法各取最新 5 条正文，印象只给人数与最近
//! 互动者一行；全量详情走 `format_memory_detail`（qq_memory_query 按类别拉取，
//! 印象返回全部不受注入截断限制）。注入不带相对时间与 motivation：模型用不上
//! 分钟级新鲜度（prompt 自带【当前时间】行），而每次变化的字符串只白白撑大
//! 注入体量。

use crate::state::{ActiveTopic, ConversationState};
use serde_json::Value;

/// 上限：话题 20 条、想法 20 条、印象 50 人、单条文本 200 字、trait 50 字。
const MAX_TOPICS: usize = 20;
const MAX_THOUGHTS: usize = 20;
const MAX_IMPRESSIONS: usize = 50;
const MAX_TEXT: usize = 200;
const MAX_TRAIT: usize = 50;
const TOPIC_STALE_MS: u64 = 24 * 60 * 60 * 1000;
const THOUGHT_DEFAULT_TTL_MS: u64 = 2 * 60 * 60 * 1000;

#[derive(Debug, Clone, Default)]
pub struct MemoryExtra {
    pub participants: Vec<String>,
    pub pending_question: Option<String>,
    pub motivation: Option<String>,
    pub expires_at_ms: Option<u64>,
    /// memberImpression 的目标（群友名/QQ）。
    pub target: Option<String>,
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 追加一条记忆。返回是否实际写入（未知类别/空文本/非法 target 返回 false）。
pub fn append(
    st: &mut ConversationState,
    category: &str,
    content: &str,
    extra: &MemoryExtra,
    now_ms: u64,
) -> bool {
    let text = content.trim().to_owned();
    // 轻量清理：过期想法移除；超过 24h 未提起的话题移除。
    st.pending_thoughts.retain(|t| {
        t.get("expiresAt")
            .and_then(Value::as_u64)
            .is_none_or(|e| now_ms < e)
    });
    st.active_topics
        .retain(|t| now_ms.saturating_sub(t.last_mention_at) < TOPIC_STALE_MS);

    match category {
        "activeTopic" => {
            if text.is_empty() {
                return false;
            }
            if let Some(existing) = st.active_topics.iter_mut().find(|t| t.text == text) {
                existing.last_mention_at = now_ms;
                for p in &extra.participants {
                    if !existing.participants.contains(p) {
                        existing.participants.push(p.clone());
                    }
                }
                existing.participants.truncate(10);
                if let Some(q) = &extra.pending_question {
                    existing.pending_question = Value::String(truncate_chars(q, 200));
                }
            } else {
                st.active_topics.push(ActiveTopic {
                    text: truncate_chars(&text, MAX_TEXT),
                    last_mention_at: now_ms,
                    participants: extra.participants.iter().take(10).cloned().collect(),
                    pending_question: extra
                        .pending_question
                        .as_deref()
                        .map(|q| Value::String(truncate_chars(q, 200)))
                        .unwrap_or(Value::Null),
                });
            }
            if st.active_topics.len() > MAX_TOPICS {
                let excess = st.active_topics.len() - MAX_TOPICS;
                st.active_topics.drain(0..excess);
            }
            true
        }
        "pendingThought" => {
            if text.is_empty() {
                return false;
            }
            let ttl = extra.expires_at_ms.unwrap_or(THOUGHT_DEFAULT_TTL_MS);
            if let Some(existing) = st
                .pending_thoughts
                .iter_mut()
                .find(|t| t.get("text").and_then(Value::as_str) == Some(text.as_str()))
            {
                existing["createdAt"] = Value::from(now_ms);
                existing["expiresAt"] = Value::from(now_ms + ttl);
                if let Some(m) = &extra.motivation {
                    existing["motivation"] = Value::String(truncate_chars(m, 50));
                }
            } else {
                st.pending_thoughts.push(serde_json::json!({
                    "text": truncate_chars(&text, MAX_TEXT),
                    "createdAt": now_ms,
                    "expiresAt": now_ms + ttl,
                    "motivation": truncate_chars(extra.motivation.as_deref().unwrap_or("curiosity"), 50),
                }));
            }
            if st.pending_thoughts.len() > MAX_THOUGHTS {
                let excess = st.pending_thoughts.len() - MAX_THOUGHTS;
                st.pending_thoughts.drain(0..excess);
            }
            true
        }
        "memberImpression" => {
            let Some(target) = extra
                .target
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
            else {
                return false;
            };
            let old = st
                .member_impressions
                .get(target)
                .cloned()
                .unwrap_or(Value::Null);
            let mut traits: Vec<String> = old
                .get("traits")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .take(10)
                        .collect()
                })
                .unwrap_or_default();
            if !text.is_empty() {
                let t = truncate_chars(&text, MAX_TRAIT);
                if !traits.contains(&t) {
                    traits.push(t);
                }
            }
            let count = old
                .get("interactionCount")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                + 1;
            st.member_impressions.insert(
                target.to_owned(),
                serde_json::json!({ "traits": traits, "interactionCount": count, "lastSeenAt": now_ms }),
            );
            // 超 50 人按 lastSeenAt 最旧淘汰。
            if st.member_impressions.len() > MAX_IMPRESSIONS {
                let mut entries: Vec<(String, u64)> = st
                    .member_impressions
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            v.get("lastSeenAt").and_then(Value::as_u64).unwrap_or(0),
                        )
                    })
                    .collect();
                entries.sort_by_key(|(_, seen)| *seen);
                let excess = entries.len() - MAX_IMPRESSIONS;
                for (k, _) in entries.into_iter().take(excess) {
                    st.member_impressions.remove(&k);
                }
            }
            true
        }
        _ => false,
    }
}

/// 删除记忆：category + content 精确匹配（thoughts 按 text，topics 按 text）。
pub fn remove(st: &mut ConversationState, category: &str, content: &str) -> bool {
    let text = content.trim();
    match category {
        "activeTopic" => {
            let before = st.active_topics.len();
            st.active_topics.retain(|t| t.text != text);
            st.active_topics.len() != before
        }
        "pendingThought" => {
            let before = st.pending_thoughts.len();
            st.pending_thoughts
                .retain(|t| t.get("text").and_then(Value::as_str) != Some(text));
            st.pending_thoughts.len() != before
        }
        _ => false,
    }
}

/// 清空指定类别（或全部）记忆。返回是否有效请求。
pub fn clear(st: &mut ConversationState, category: &str) -> bool {
    match category {
        "activeTopic" => {
            st.active_topics.clear();
            true
        }
        "pendingThought" => {
            st.pending_thoughts.clear();
            true
        }
        "memberImpression" => {
            st.member_impressions.clear();
            true
        }
        "all" => {
            st.active_topics.clear();
            st.pending_thoughts.clear();
            st.member_impressions.clear();
            true
        }
        _ => false,
    }
}

/// 印象按最近互动（lastSeenAt）降序——注入索引取前几名、detail 全量都按此序，
/// 取代旧版 HashMap 迭代序的非确定选取。
fn impressions_by_recency(st: &ConversationState) -> Vec<(&String, &Value)> {
    let mut entries: Vec<(&String, &Value)> = st.member_impressions.iter().collect();
    entries.sort_by_key(|(_, v)| {
        std::cmp::Reverse(v.get("lastSeenAt").and_then(Value::as_u64).unwrap_or(0))
    });
    entries
}

fn unfinished(thought: &Value, now_ms: u64) -> bool {
    thought
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
        && thought
            .get("expiresAt")
            .and_then(Value::as_u64)
            .is_none_or(|e| now_ms < e)
}

/// 记忆索引块（注入唤醒 prompt 的形态）。话题/想法各取最新 5 条正文，印象只
/// 给一行索引——详情由模型按需 `qq_memory_query` 拉取（见 format_memory_detail）。
pub fn format_memory(st: &ConversationState, now_ms: u64) -> String {
    let mut lines: Vec<String> = Vec::new();
    let topics: Vec<&ActiveTopic> = st
        .active_topics
        .iter()
        .filter(|t| !t.text.is_empty())
        .collect();
    if !topics.is_empty() {
        lines.push("【进行中的话题】".to_owned());
        for t in topics.iter().rev().take(5) {
            let stale = if now_ms.saturating_sub(t.last_mention_at) > 2 * 60 * 60 * 1000 {
                "（已搁置）"
            } else {
                ""
            };
            let q = if t.pending_question.is_null()
                || t.pending_question.to_string().trim_matches('"').is_empty()
            {
                String::new()
            } else {
                format!(
                    "；待追问：{}",
                    t.pending_question.as_str().unwrap_or_default()
                )
            };
            lines.push(format!("- {}{}{}", t.text, stale, q));
        }
    }
    let thoughts: Vec<&Value> = st
        .pending_thoughts
        .iter()
        .filter(|t| unfinished(t, now_ms))
        .collect();
    if !thoughts.is_empty() {
        lines.push("【你想说但还没说的】".to_owned());
        for t in thoughts.iter().rev().take(5) {
            lines.push(format!("- {}", t["text"].as_str().unwrap_or_default()));
        }
    }
    if !st.member_impressions.is_empty() {
        let sorted = impressions_by_recency(st);
        let recent: Vec<&str> = sorted.iter().take(3).map(|(n, _)| n.as_str()).collect();
        lines.push(format!(
            "【对群友的印象】共 {} 人，最近互动：{}；详情 qq_memory_query(category=\"memberImpression\")",
            sorted.len(),
            recent.join("、")
        ));
    }
    lines.join("\n")
}

fn ago_str(now_ms: u64, at_ms: u64) -> String {
    let min = now_ms.saturating_sub(at_ms) / 60_000;
    if min >= 60 {
        format!("{} 小时前", min / 60)
    } else if min > 0 {
        format!("{min} 分钟前")
    } else {
        "刚刚".to_owned()
    }
}

/// 记忆详情（qq_memory_query 返回形态）：按类别全量输出，不受注入截断限制。
/// category：activeTopic / pendingThought / memberImpression / all（缺省 all）；
/// 无效类别返回 None（桥端回参数错误）。
pub fn format_memory_detail(st: &ConversationState, category: &str, now_ms: u64) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    if category == "activeTopic" || category == "all" {
        let topics: Vec<&ActiveTopic> = st
            .active_topics
            .iter()
            .filter(|t| !t.text.is_empty())
            .collect();
        if !topics.is_empty() {
            lines.push("【进行中的话题】".to_owned());
            for t in topics.iter().rev() {
                let stale = if now_ms.saturating_sub(t.last_mention_at) > 2 * 60 * 60 * 1000 {
                    "（已搁置）"
                } else {
                    ""
                };
                let who = if t.participants.is_empty() {
                    String::new()
                } else {
                    format!("；{}", t.participants.join("、"))
                };
                let q = if t.pending_question.is_null()
                    || t.pending_question.to_string().trim_matches('"').is_empty()
                {
                    String::new()
                } else {
                    format!(
                        "；待追问：{}",
                        t.pending_question.as_str().unwrap_or_default()
                    )
                };
                lines.push(format!(
                    "- {}{}（最后提及 {}{}）{}",
                    t.text,
                    stale,
                    ago_str(now_ms, t.last_mention_at),
                    who,
                    q
                ));
            }
        }
    }
    if category == "pendingThought" || category == "all" {
        let thoughts: Vec<&Value> = st
            .pending_thoughts
            .iter()
            .filter(|t| unfinished(t, now_ms))
            .collect();
        if !thoughts.is_empty() {
            lines.push("【你想说但还没说的】".to_owned());
            for t in thoughts.iter().rev() {
                let text = t["text"].as_str().unwrap_or_default();
                let motivation = t
                    .get("motivation")
                    .and_then(Value::as_str)
                    .filter(|m| !m.is_empty());
                let left_min = t
                    .get("expiresAt")
                    .and_then(Value::as_u64)
                    .map(|e| e.saturating_sub(now_ms) / 60_000)
                    .unwrap_or(0);
                let expiry = if left_min >= 60 {
                    format!("，{} 小时后过期", left_min / 60)
                } else {
                    format!("，{left_min} 分钟后过期")
                };
                lines.push(match motivation {
                    Some(m) => format!("- {text}（{m}{expiry}）"),
                    None => format!("- {text}（{expiry}）"),
                });
            }
        }
    }
    if category == "memberImpression" || category == "all" {
        let sorted = impressions_by_recency(st);
        if !sorted.is_empty() {
            lines.push("【对群友的印象】".to_owned());
            for (name, im) in sorted {
                let traits: Vec<&str> = im
                    .get("traits")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let trait_text: String = if traits.is_empty() {
                    "暂无记录".to_owned()
                } else {
                    traits.join("、")
                };
                let count = im
                    .get("interactionCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let seen = im
                    .get("lastSeenAt")
                    .and_then(Value::as_u64)
                    .unwrap_or(now_ms);
                lines.push(format!(
                    "- {name}：{trait_text}（互动 {count} 次，{}）",
                    ago_str(now_ms, seen)
                ));
            }
        }
    }
    if !matches!(
        category,
        "activeTopic" | "pendingThought" | "memberImpression" | "all"
    ) {
        return None;
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> ConversationState {
        ConversationState::default()
    }

    /// activeTopic：去重刷新、24h 过期清除、上限 20。
    #[test]
    fn topic_append_dedup_and_expiry() {
        let mut s = st();
        assert!(append(
            &mut s,
            "activeTopic",
            "聊游戏",
            &MemoryExtra::default(),
            1000
        ));
        assert!(append(
            &mut s,
            "activeTopic",
            "聊游戏",
            &MemoryExtra::default(),
            2000
        ));
        assert_eq!(s.active_topics.len(), 1, "同文本去重");
        assert_eq!(s.active_topics[0].last_mention_at, 2000, "刷新时刻");
        // 过期清除：25h 前的话题在新 append 时被清。
        let mut s2 = st();
        append(&mut s2, "activeTopic", "旧话题", &MemoryExtra::default(), 0);
        append(
            &mut s2,
            "activeTopic",
            "新话题",
            &MemoryExtra::default(),
            TOPIC_STALE_MS + 1,
        );
        assert_eq!(s2.active_topics.len(), 1);
        assert_eq!(s2.active_topics[0].text, "新话题");
    }

    /// pendingThought：过期不进注入/detail、注入不带 motivation（detail 带）。
    #[test]
    fn thought_ttl_and_format() {
        let mut s = st();
        append(
            &mut s,
            "pendingThought",
            "想问他作业写完没",
            &MemoryExtra {
                motivation: Some("curiosity".into()),
                ..Default::default()
            },
            1000,
        );
        let block = format_memory(&s, 1500);
        assert!(block.contains("【你想说但还没说的】"));
        assert!(block.contains("- 想问他作业写完没"));
        assert!(!block.contains("curiosity"), "注入不带 motivation");
        let detail = format_memory_detail(&s, "all", 1500).unwrap();
        assert!(
            detail.contains("想问他作业写完没（curiosity，"),
            "detail 保留 motivation 与剩余时长"
        );
        // 过期后不再出现在记忆块。
        let block2 = format_memory(&s, 1000 + THOUGHT_DEFAULT_TTL_MS + 1);
        assert!(!block2.contains("作业写完没"));
    }

    /// memberImpression：trait 去重追加、互动计数、注入只给索引行、detail 给全文。
    #[test]
    fn impression_traits_and_count() {
        let mut s = st();
        let extra = |t: &str| MemoryExtra {
            target: Some(t.into()),
            ..Default::default()
        };
        append(
            &mut s,
            "memberImpression",
            "爱聊硬件",
            &extra("管理员"),
            1000,
        );
        append(
            &mut s,
            "memberImpression",
            "爱聊硬件",
            &extra("管理员"),
            2000,
        ); // 重复 trait 不叠加
        append(
            &mut s,
            "memberImpression",
            "出手大方",
            &extra("管理员"),
            3000,
        );
        // 注入只给索引行（人数 + 最近互动者 + 查询指引）。
        let block = format_memory(&s, 3000);
        assert!(block.contains("【对群友的印象】共 1 人，最近互动：管理员；"));
        assert!(!block.contains("爱聊硬件"), "注入不给印象正文");
        // detail 给全文。
        let detail = format_memory_detail(&s, "memberImpression", 3000).unwrap();
        assert!(detail.contains("管理员：爱聊硬件、出手大方（互动 3 次"));
    }

    /// 注入截断（各 5 条），detail 全量；注入不带相对时间。
    #[test]
    fn inject_truncates_detail_full_and_no_time() {
        let mut s = st();
        for i in 0..11 {
            append(
                &mut s,
                "activeTopic",
                &format!("话题{i}"),
                &MemoryExtra::default(),
                1000 + i,
            );
        }
        let block = format_memory(&s, 20_000);
        assert!(block.contains("话题10"), "最新在前");
        assert!(!block.contains("话题5"), "注入只取最新 5 条");
        assert!(!block.contains("分钟前"), "注入不带相对时间");
        let detail = format_memory_detail(&s, "activeTopic", 20_000).unwrap();
        assert!(detail.contains("话题0"), "detail 全量");
        assert!(detail.contains("最后提及"), "detail 带最后提及时间");
    }

    /// 印象索引行按最近互动排序（取代旧 HashMap 迭代序的非确定选取）。
    #[test]
    fn impression_index_recency_order() {
        let mut s = st();
        let extra = |t: &str| MemoryExtra {
            target: Some(t.into()),
            ..Default::default()
        };
        append(&mut s, "memberImpression", "a", &extra("最早"), 1000);
        append(&mut s, "memberImpression", "b", &extra("最新"), 3000);
        append(&mut s, "memberImpression", "c", &extra("居中"), 2000);
        let block = format_memory(&s, 3000);
        assert!(block.contains("最近互动：最新、居中、最早"));
        let detail = format_memory_detail(&s, "memberImpression", 3000).unwrap();
        assert!(
            detail.find("最新：").unwrap() < detail.find("居中：").unwrap()
                && detail.find("居中：").unwrap() < detail.find("最早：").unwrap(),
            "detail 按 lastSeenAt 降序"
        );
    }

    /// 无效类别 None；有效但空类别给空串。
    #[test]
    fn detail_category_validation() {
        let mut s = st();
        append(&mut s, "activeTopic", "话题", &MemoryExtra::default(), 1000);
        assert!(format_memory_detail(&s, "badCategory", 1000).is_none());
        assert_eq!(
            format_memory_detail(&s, "pendingThought", 1000),
            Some(String::new())
        );
    }

    /// remove / clear 语义。
    #[test]
    fn remove_and_clear() {
        let mut s = st();
        append(
            &mut s,
            "activeTopic",
            "话题A",
            &MemoryExtra::default(),
            1000,
        );
        assert!(remove(&mut s, "activeTopic", "话题A"));
        assert!(s.active_topics.is_empty());
        assert!(!remove(&mut s, "activeTopic", "不存在"));
        append(
            &mut s,
            "pendingThought",
            "想法",
            &MemoryExtra::default(),
            1000,
        );
        assert!(clear(&mut s, "pendingThought"));
        assert!(s.pending_thoughts.is_empty());
        assert!(!clear(&mut s, "badCategory"));
    }

    /// 迁移数据兼容：Node 版 social-v2.json 形状的 activeTopics 直接被 format 消费。
    #[test]
    fn format_consumes_migrated_shape() {
        let mut s = st();
        s.active_topics.push(ActiveTopic {
            text: "群权限这条路我已经用尽自己的尝试".into(),
            last_mention_at: 1_792_000_000_000u64,
            participants: vec!["管理员".into()],
            pending_question: Value::Null,
        });
        s.member_impressions.insert(
            "管理员3388404701".into(),
            serde_json::json!({ "traits": ["爱私聊摸能力边界"], "interactionCount": 1, "lastSeenAt": 1_792_465_000_000u64 }),
        );
        let block = format_memory(&s, 1_792_500_000_000);
        assert!(block.contains("群权限这条路"));
        assert!(block.contains("共 1 人，最近互动：管理员3388404701"));
        let detail = format_memory_detail(&s, "memberImpression", 1_792_500_000_000).unwrap();
        assert!(detail.contains("爱私聊摸能力边界"));
    }
}
