//! 轻量记忆（Node 版 appendMemoryV2 / formatMemoryV2 语义移植）。
//!
//! 三类记忆，各有上限与过期（防无限膨胀）：
//! - **activeTopic** 进行中的话题（lastMentionAt 超 24h 清除；同文本刷新而非重复入队）
//! - **pendingThought** 想说未说（expiresAt 过期清除，默认 2h）
//! - **memberImpression** 对群友的印象（traits 去重追加，超 50 人按 lastSeenAt 淘汰）
//!
//! `format_memory` 生成注入唤醒 prompt 的文本块（话题含"已搁置"标记与分钟前、
//! 印象含互动次数）——迁移来的 social-v2.json 里的记忆直接被消费。

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

/// 记忆文本块（注入唤醒 prompt 的形态，Node 版 formatMemoryV2 逐段对应）。
pub fn format_memory(st: &ConversationState, now_ms: u64) -> String {
    let mut lines: Vec<String> = Vec::new();
    let topics: Vec<&ActiveTopic> = st
        .active_topics
        .iter()
        .filter(|t| !t.text.is_empty())
        .collect();
    if !topics.is_empty() {
        lines.push("【进行中的话题】".to_owned());
        for t in topics.iter().rev().take(10) {
            let ago_min = now_ms.saturating_sub(t.last_mention_at) / 60_000;
            let stale = if now_ms.saturating_sub(t.last_mention_at) > 2 * 60 * 60 * 1000 {
                "（已搁置）"
            } else {
                ""
            };
            let ago = if ago_min > 0 {
                format!("{ago_min}分钟前")
            } else {
                "刚刚".to_owned()
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
            lines.push(format!("- {}{}（{}）{}", t.text, stale, ago, q));
        }
    }
    let thoughts: Vec<&Value> = st
        .pending_thoughts
        .iter()
        .filter(|t| {
            t.get("text")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
                && t.get("expiresAt")
                    .and_then(Value::as_u64)
                    .is_none_or(|e| now_ms < e)
        })
        .collect();
    if !thoughts.is_empty() {
        lines.push("【你想说但还没说的】".to_owned());
        for t in thoughts.iter().rev().take(10) {
            let text = t["text"].as_str().unwrap_or_default();
            let motivation = t["motivation"].as_str().filter(|m| !m.is_empty());
            lines.push(match motivation {
                Some(m) => format!("- {text}（{m}）"),
                None => format!("- {text}"),
            });
        }
    }
    if !st.member_impressions.is_empty() {
        lines.push("【对群友的印象】".to_owned());
        for (i, (name, im)) in st.member_impressions.iter().enumerate().rev().take(10) {
            let traits: Vec<&str> = im
                .get("traits")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
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
            lines.push(format!("- {name}：{trait_text}（互动 {count} 次）"));
            if i == 0 {
                break;
            }
        }
    }
    lines.join("\n")
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

    /// pendingThought：过期不进 format、上限 20、motivation 注入。
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
        assert!(block.contains("想问他作业写完没（curiosity）"));
        // 过期后不再出现在记忆块。
        let block2 = format_memory(&s, 1000 + THOUGHT_DEFAULT_TTL_MS + 1);
        assert!(!block2.contains("作业写完没"));
    }

    /// memberImpression：trait 去重追加、互动计数、淘汰。
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
        let im = &s.member_impressions["管理员"];
        assert_eq!(im["traits"].as_array().unwrap().len(), 2);
        assert_eq!(im["interactionCount"], 3);
        let block = format_memory(&s, 3000);
        assert!(block.contains("管理员：爱聊硬件、出手大方（互动 3 次）"));
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
        assert!(block.contains("爱私聊摸能力边界"));
    }
}
