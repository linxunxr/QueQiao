//! 日记调度组装件（Node 版 bridge.js `scheduleDiaryV2`/`pickDiaryHostKey`/
//! diary 唤醒 prompt 素材段的 Rust 对应物——bin 侧可测部分）。
//!
//! 分工：core 的 [`queqiao_core::diary`] 纯函数回答「该不该写」（定点/安静/
//! 补写三路径）；本模块负责跨会话数据组装——全局活跃度、宿主挑选、补写
//! 计数与唤醒素材块。60s 轮询节奏、配置门禁与 SendPrompt 投递在 main.rs
//! 编排（IO 不进本模块）。
//!
//! Node 版关键语义（10-23 生产切换后日记断更的直接根因——这些接线在
//! Rust 版缺失，本模块即补齐）：
//! - 日记状态是**跨会话全局**的：lastActivity 取所有会话非自发消息的最大
//!   时刻，不挂在单个会话的运行时上；
//! - 唤醒发往「宿主会话」（48h 内非自发消息最多的在册会话），宿主繁忙则
//!   本次跳过（60min 冷却兜住重试节奏）；
//! - 写入日期由桥锁定（`diary_pending`），模型无法指定——补写回合锁昨天、
//!   常规回合锁当前日记日（凌晨 0~4 点归昨天）。

use queqiao_core::diary::{CivilTime, DiaryEntry, current_diary_day, diary_date_key, recent_diary};

use crate::social_store::SocialStore;

/// 宿主挑选与补写计数的素材窗口（Node pickDiaryHostKey/启动补写检查的
/// `48 * 3600 * 1000`）。
const HOST_WINDOW_MS: u64 = 48 * 3_600_000;

/// 常规日记的素材窗口：最近 24h（Node diary prompt 的 since）。
const MATERIAL_WINDOW_MS: u64 = 24 * 3_600_000;

/// 单会话素材摘要最多条数（Node `.slice(-15)`）。
const DIGEST_MAX_MSGS: usize = 15;

/// 素材摘要单条文本截断（Node `.slice(0, 60)`）。
const DIGEST_TEXT_CHARS: usize = 60;

/// 锚点摘录长度（Node recentDiary excerptChars: 100）。
const ANCHOR_EXCERPT_CHARS: usize = 100;

/// 当前日记日的日期键（凌晨 0~4 点归昨天——与触发判定同基准）。
pub fn today_diary_key(now_local: CivilTime) -> String {
    diary_date_key(current_diary_day(now_local))
}

/// 全局最近活跃时刻：所有会话 recent_messages 中非自发消息 time 的最大值
///（0 = 无活动记录）。Node diaryTick 的 lastActivityAt 聚合语义。
pub fn global_last_activity(social: &SocialStore) -> u64 {
    social
        .conversation_keys()
        .iter()
        .filter_map(|key| social.conversation(key))
        .flat_map(|st| st.recent_messages.iter())
        .filter(|m| !m.is_self)
        .map(|m| m.time)
        .max()
        .unwrap_or(0)
}

/// 窗口内非自发消息总数（启动补写判定的 yesterdayActivityCount——Node 用
/// 48h 宽窗口兜「桥白天关机、凌晨才开机」的作息）。
pub fn activity_count_since(social: &SocialStore, since_ms: u64) -> u32 {
    social
        .conversation_keys()
        .iter()
        .filter_map(|key| social.conversation(key))
        .flat_map(|st| st.recent_messages.iter())
        .filter(|m| !m.is_self && m.time >= since_ms)
        .count() as u32
}

/// 挑日记宿主：`keys`（模式允许的在册会话）中 48h 内非自发消息最多者；
/// 全部无活动返回 None（Node pickDiaryHostKey——没有宿主就不触发）。
pub fn pick_diary_host(social: &SocialStore, keys: &[String], now_ms: u64) -> Option<String> {
    let since = now_ms.saturating_sub(HOST_WINDOW_MS);
    let mut best: Option<(String, u32)> = None;
    for key in keys {
        let count = social
            .conversation(key)
            .map(|st| {
                st.recent_messages
                    .iter()
                    .filter(|m| !m.is_self && m.time >= since)
                    .count() as u32
            })
            .unwrap_or(0);
        if count > 0 && best.as_ref().is_none_or(|(_, c)| count > *c) {
            best = Some((key.clone(), count));
        }
    }
    best.map(|(key, _)| key)
}

/// 日记唤醒素材块（Node diary prompt 的 head + digests + anchors + tail，
/// 拼在唤醒骨架之后；补写与常规文案区分）。
///
/// - `pending`：桥锁定的写入日期（补写=昨天键；None 视作常规——但 main
///   触发时总会先锁定，None 只是兜底）；
/// - `today_key`：当前日记日键（与 pending 不同即补写回合）；
/// - `now_local`/`now_ms`：素材窗口换算（常规=最近 24h；补写=昨天 0 点起）。
pub fn build_diary_material(
    social: &SocialStore,
    keys: &[String],
    entries: &[DiaryEntry],
    pending: Option<&str>,
    today_key: &str,
    now_ms: u64,
    now_local: CivilTime,
) -> String {
    let pending_key = pending.unwrap_or(today_key);
    let is_catchup = pending_key != today_key;
    let elapsed_today_ms = (now_local.hour as u64 * 3600 + now_local.minute as u64 * 60) * 1000;
    // 补写素材从昨天 0 点起（覆盖昨天全天；今天凌晨的动静一并给 AI 参考）。
    let since = if is_catchup {
        now_ms.saturating_sub(elapsed_today_ms + 86_400_000)
    } else {
        now_ms.saturating_sub(MATERIAL_WINDOW_MS)
    };

    let mut digests = Vec::new();
    for key in keys {
        let Some(st) = social.conversation(key) else {
            continue;
        };
        let msgs: Vec<_> = st
            .recent_messages
            .iter()
            .filter(|m| !m.is_self && m.time >= since)
            .collect();
        if msgs.is_empty() {
            continue;
        }
        let tail: Vec<String> = msgs
            .iter()
            .rev()
            .take(DIGEST_MAX_MSGS)
            .rev()
            .map(|m| {
                format!(
                    "{}：{}",
                    m.sender,
                    m.text.chars().take(DIGEST_TEXT_CHARS).collect::<String>()
                )
            })
            .collect();
        digests.push(format!("【{key}】\n{}", tail.join("\n")));
    }

    let anchors = recent_diary(entries, 3, ANCHOR_EXCERPT_CHARS);
    let anchor_text = if anchors.is_empty() {
        "（还没有历史日记，这是第一篇，开个头）".to_owned()
    } else {
        anchors
            .iter()
            .map(|e| format!("{}：{}", e.date, e.content))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let digest_text = if digests.is_empty() {
        "（那段时间很安静，什么都没发生）".to_owned()
    } else {
        digests.join("\n\n")
    };

    let (head, tail) = if is_catchup {
        (
            format!(
                "【日记时间·补写】{pending_key} 关机太早没来得及写日记，现在把那天的补上。各会话那天的动静："
            ),
            "按【日记】规则补写那天的日记（日期桥接已锁定，你只管写内容），写好调用 qq_write_diary 提交。",
        )
    } else {
        (
            "【日记时间】今天过完了。各会话今天的动静：".to_owned(),
            "按【日记】规则用你自己的口吻写今天的日记，写好调用 qq_write_diary 提交——今天只能提交一次。实在不想写也可以安静收尾，但连续不写会让你的记忆断线。",
        )
    };
    format!(
        "{head}\n\n{digest_text}\n\n你最近几天的日记（衔接风格用，别重复内容）：\n{anchor_text}\n\n{tail}日记不会发给任何群友，只属于你自己。\n\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use queqiao_core::action::IncomingMessage;

    fn msg(key: &str, text: &str, is_self: bool) -> IncomingMessage {
        IncomingMessage {
            key: key.into(),
            seq: 0,
            message_id: format!("m-{key}-{text}"),
            sender: "群友".into(),
            user_id: "10001".into(),
            text: text.into(),
            is_self,
            at_me: false,
            reply_to_me: false,
            time_ms: 0,
        }
    }

    fn store() -> SocialStore {
        let dir = std::env::temp_dir().join(format!("queqiao-ds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        SocialStore::load(&dir.join("social-v2.json"), None).unwrap()
    }

    fn civil(h: u8, min: u8) -> CivilTime {
        CivilTime {
            date: queqiao_core::diary::CivilDate {
                year: 2026,
                month: 10,
                day: 30,
            },
            hour: h,
            minute: min,
        }
    }

    /// 全局活跃度：跨会话取最大、自发消息不算、无活动为 0。
    #[test]
    fn global_last_activity_takes_max_non_self() {
        let mut s = store();
        s.append_message("group:a", &msg("group:a", "hi", false), 1000);
        s.append_message("group:b", &msg("group:b", "yo", false), 5000);
        s.append_message("group:b", &msg("group:b", "回显", true), 9000);
        assert_eq!(global_last_activity(&s), 5000, "自发回显不算活跃");
        let empty = store();
        assert_eq!(global_last_activity(&empty), 0);
    }

    /// 窗口计数：48h 窗口外与自发消息不计。
    #[test]
    fn activity_count_respects_window_and_self() {
        let mut s = store();
        let now = 100 * 3_600_000;
        s.append_message("group:a", &msg("group:a", "新", false), now - 1000);
        s.append_message(
            "group:a",
            &msg("group:a", "旧", false),
            now - 50 * 3_600_000,
        );
        s.append_message("group:a", &msg("group:a", "回显", true), now - 2000);
        assert_eq!(activity_count_since(&s, now - 48 * 3_600_000), 1);
    }

    /// 宿主挑选：在册 keys 中 48h 活动最多者；无活动 None；只看 keys 内的。
    #[test]
    fn pick_host_by_activity_within_keys() {
        let mut s = store();
        let now = 1_000_000;
        s.append_message("group:a", &msg("group:a", "1", false), now);
        s.append_message("group:b", &msg("group:b", "1", false), now);
        s.append_message("group:b", &msg("group:b", "2", false), now);
        s.append_message("group:c", &msg("group:c", "不在册", false), now);
        let keys = vec!["group:a".to_owned(), "group:b".to_owned()];
        assert_eq!(pick_diary_host(&s, &keys, now).as_deref(), Some("group:b"));
        // 窗口外无活动 → None。
        assert_eq!(pick_diary_host(&s, &keys, now + 49 * 3_600_000), None);
    }

    /// 素材块：常规文案带摘要与锚点；空摘要有静默占位。
    #[test]
    fn material_regular_and_quiet() {
        let mut s = store();
        s.append_message("group:4242", &msg("group:4242", "聊了猫", false), 1_000_000);
        let entries = vec![DiaryEntry {
            date: "2026-10-29".into(),
            content: "昨天很闲。".into(),
            key: String::new(),
            created_at: 0,
        }];
        let now_ms = 1_100_000u64;
        let text = build_diary_material(
            &s,
            &["group:4242".to_owned()],
            &entries,
            Some("2026-10-30"),
            "2026-10-30",
            now_ms,
            civil(23, 5),
        );
        assert!(text.contains("【日记时间】今天过完了"));
        assert!(text.contains("【group:4242】"));
        assert!(text.contains("群友：聊了猫"));
        assert!(text.contains("2026-10-29：昨天很闲。"));
        assert!(text.contains("日记不会发给任何群友"));
    }

    /// 素材块：补写文案区分、素材窗口从昨天 0 点起、无锚点有开篇占位。
    #[test]
    fn material_catchup_and_first_entry() {
        let mut s = store();
        // 昨天 0 点（今天 23:05 → elapsed=82_500_000ms）之后的消息才进摘要。
        let now_ms = 1_000_000_000u64;
        let elapsed = (23 * 3600 + 5 * 60) * 1000;
        let yesterday_zero = now_ms - elapsed - 86_400_000;
        s.append_message(
            "group:4242",
            &msg("group:4242", "昨天的事", false),
            yesterday_zero + 1000,
        );
        s.append_message(
            "group:4242",
            &msg("group:4242", "前天的事", false),
            yesterday_zero - 1000,
        );
        let text = build_diary_material(
            &s,
            &["group:4242".to_owned()],
            &[],
            Some("2026-10-29"),
            "2026-10-30",
            now_ms,
            civil(23, 5),
        );
        assert!(text.contains("【日记时间·补写】2026-10-29"));
        assert!(text.contains("昨天的事"));
        assert!(!text.contains("前天的事"), "补写窗口不含前天");
        assert!(text.contains("（还没有历史日记，这是第一篇，开个头）"));
    }

    /// today_diary_key：白天归今天、凌晨 0~4 点归昨天。
    #[test]
    fn today_key_dawn_window() {
        assert_eq!(today_diary_key(civil(23, 0)), "2026-10-30");
        assert_eq!(today_diary_key(civil(2, 30)), "2026-10-29", "凌晨归昨天");
    }
}
