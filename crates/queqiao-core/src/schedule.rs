//! 唤醒优先级、合批窗口与暂存去重（从 Node 版 bridge.js `scheduleWakeV2` /
//! `wakePriorityV2` 提取为纯函数——M1 遗留项，规格 §3.1 schedule 模块）。
//!
//! 语义来源（Node 实战契约）：
//! - **优先级合并**：合批窗口内已有待发送唤醒时，高优先级**升级**最终原因
//!   ——「先概率后 @ 却仍按概率唤醒」的错误不许再犯；
//! - **繁忙暂存去重**：busy 时暂存 (reason, seq)，重复的 (reason, seq) 不重复
//!   入队、有界 20 条（消息洪峰下不无限增长）；
//! - **合批窗口 clamp**：≥1000ms（配置面 min 1h 上限）。

use crate::wake::PendingWakeReason;

/// 唤醒原因优先级（数值越大越先）。带子类型的原因（`keyword:小鲸鱼`、
/// `speaker:昵称`）取冒号前的基名计算；未知原因 0（最低，仍可唤醒）。
pub fn wake_priority(reason: &str) -> u32 {
    let base = reason.split(':').next().unwrap_or_default();
    match base {
        "private" => 100,
        "atMention" => 90,
        "question" => 80,
        "speaker" => 75,
        "nameMention" => 70,
        "keyword" => 60,
        "anyMessage" => 50,
        "replyCheck" => 40,
        "timeout" => 30,
        "proactiveCheck" => 20,
        "diary" => 15,
        _ => 0,
    }
}

/// 合批窗口内的合并结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// 窗口内没有更高优先级：保持既有原因。
    Keep,
    /// 新原因优先级更高：升级（from → to）。
    Upgraded { from: String, to: String },
}

/// 合批窗口内已有待发送唤醒（`existing`）时，决定新原因（`incoming`）是否升级。
/// 优先级相等不升级（先到先得——Node `>` 严格大于语义）。
pub fn merge_pending(existing: &str, incoming: &str) -> MergeOutcome {
    if wake_priority(incoming) > wake_priority(existing) {
        MergeOutcome::Upgraded {
            from: existing.to_owned(),
            to: incoming.to_owned(),
        }
    } else {
        MergeOutcome::Keep
    }
}

/// 合批窗口下限（防配置成 0 让合批失效）。
pub const MIN_BATCH_WINDOW_MS: u64 = 1000;
/// 合批窗口上限（配置面 clamp）。
pub const MAX_BATCH_WINDOW_MS: u64 = 3_600_000;

/// 合批窗口 clamp：配置值收进 [1000, 3600_000]，非法（0）回默认 8000。
pub fn clamp_batch_window_ms(configured: u64) -> u64 {
    if configured == 0 {
        8000
    } else {
        configured.clamp(MIN_BATCH_WINDOW_MS, MAX_BATCH_WINDOW_MS)
    }
}

/// 暂存队列上限（消息洪峰下有界）。
pub const PENDING_QUEUE_LIMIT: usize = 20;

/// 繁忙时暂存一条唤醒原因：(reason, seq) 重复不重复入队；有界 20
///（超界丢最旧——Node `splice(0, len-20)` 语义）。
pub fn defer_wake(queue: &mut Vec<PendingWakeReason>, reason: &str, seq: u64) {
    if queue.iter().any(|r| r.reason == reason && r.seq == seq) {
        return;
    }
    queue.push(PendingWakeReason {
        reason: reason.to_owned(),
        seq,
    });
    let len = queue.len();
    if len > PENDING_QUEUE_LIMIT {
        queue.drain(0..len - PENDING_QUEUE_LIMIT);
    }
}

/// 调度一条唤醒的总决策（scheduleWakeV2 的纯函数形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeSchedule {
    /// 空闲：开合批窗口，`batch_ms` 后以 reason 发送。
    StartBatch { reason: String, batch_ms: u64 },
    /// 窗口内合并：升级或保持（调用方更新 pending 原因）。
    MergeInWindow(MergeOutcome),
    /// 会话繁忙：暂存队列（调用方据 [`defer_wake`] 维护队列）。
    DeferToQueue,
}

/// 决定一条唤醒原因如何进入调度。
pub fn schedule_wake(
    pending_timer_active: bool,
    pending_reason: Option<&str>,
    busy: bool,
    reason: &str,
    batch_window_ms: u64,
) -> WakeSchedule {
    if pending_timer_active {
        // 合并窗口内已有待发送唤醒：按优先级升级最终原因。
        let existing = pending_reason.unwrap_or(reason);
        return WakeSchedule::MergeInWindow(merge_pending(existing, reason));
    }
    if busy {
        return WakeSchedule::DeferToQueue;
    }
    WakeSchedule::StartBatch {
        reason: reason.to_owned(),
        batch_ms: clamp_batch_window_ms(batch_window_ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 优先级表与基名解析（子类型取冒号前）。
    #[test]
    fn priority_table_and_base_name() {
        assert!(wake_priority("private") > wake_priority("atMention"));
        assert!(wake_priority("atMention") > wake_priority("question"));
        assert!(wake_priority("question") > wake_priority("keyword"));
        assert_eq!(wake_priority("keyword:小鲸鱼"), wake_priority("keyword"));
        assert_eq!(wake_priority("speaker:昵称"), 75);
        assert_eq!(wake_priority("proactiveCheck"), 20);
        assert!(wake_priority("proactiveCheck") > wake_priority("diary"));
        assert_eq!(wake_priority("mystery"), 0, "未知原因最低但仍可唤醒");
    }

    /// 合并语义：严格大于才升级；先概率后 @ 应升级为 @。
    #[test]
    fn merge_upgrades_on_strictly_higher() {
        assert_eq!(
            merge_pending("keyword:小鲸鱼", "atMention"),
            MergeOutcome::Upgraded {
                from: "keyword:小鲸鱼".into(),
                to: "atMention".into()
            }
        );
        assert_eq!(
            merge_pending("atMention", "keyword:小鲸鱼"),
            MergeOutcome::Keep
        );
        assert_eq!(
            merge_pending("atMention", "atMention"),
            MergeOutcome::Keep,
            "同级不升级"
        );
    }

    /// 窗口 clamp：0 回默认 8000；下限 1000；上限 1h。
    #[test]
    fn batch_window_clamp() {
        assert_eq!(clamp_batch_window_ms(0), 8000);
        assert_eq!(clamp_batch_window_ms(100), MIN_BATCH_WINDOW_MS);
        assert_eq!(clamp_batch_window_ms(8000), 8000);
        assert_eq!(clamp_batch_window_ms(9_000_000), MAX_BATCH_WINDOW_MS);
    }

    /// 暂存去重与有界：同 (reason, seq) 不重复；超 20 丢最旧。
    #[test]
    fn defer_queue_dedup_and_bound() {
        let mut queue = Vec::new();
        defer_wake(&mut queue, "keyword:小鲸鱼", 1);
        defer_wake(&mut queue, "keyword:小鲸鱼", 1);
        assert_eq!(queue.len(), 1, "重复 (reason,seq) 不入队");
        defer_wake(&mut queue, "keyword:小鲸鱼", 2);
        assert_eq!(queue.len(), 2, "同 reason 不同 seq 视为新唤醒");
        for seq in 3..=25 {
            defer_wake(&mut queue, "anyMessage", seq);
        }
        assert_eq!(queue.len(), PENDING_QUEUE_LIMIT);
        assert_eq!(queue[0].seq, 6, "丢最旧");
        // 首条被挤掉的 reason/seq 组合可重新入队（不在队列中即可）。
        defer_wake(&mut queue, "keyword:小鲸鱼", 1);
        assert!(
            queue
                .iter()
                .any(|r| r.seq == 1 && r.reason == "keyword:小鲸鱼")
        );
    }

    /// 调度总决策三分支：空闲开窗 / 窗口内合并 / 繁忙暂存。
    #[test]
    fn schedule_wake_branches() {
        // 空闲：开窗（窗口 clamp 生效）。
        assert_eq!(
            schedule_wake(false, None, false, "atMention", 100),
            WakeSchedule::StartBatch {
                reason: "atMention".into(),
                batch_ms: MIN_BATCH_WINDOW_MS
            }
        );
        // 窗口内：升级。
        assert_eq!(
            schedule_wake(true, Some("keyword:x"), false, "private", 8000),
            WakeSchedule::MergeInWindow(MergeOutcome::Upgraded {
                from: "keyword:x".into(),
                to: "private".into()
            })
        );
        // 繁忙（无窗口）：暂存。
        assert_eq!(
            schedule_wake(false, None, true, "anyMessage", 8000),
            WakeSchedule::DeferToQueue
        );
    }
}
