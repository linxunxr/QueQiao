//! 唤醒防卡死决策（移植自 Node 版 `src/wake-guards.js`，行为契约同步迁移）。
//!
//! 背景（2026-10-19 Node 版实战诊断）：busy 判定被回合状态永久占用——回合状态
//! 只在收到回合结束事件时清理，结束事件一旦丢失（上游挂起/事件流断连错帧），
//! busy 永真，消息只进暂存队列，而暂存队列唯一的消费时机又是回合结束事件——死锁。

use std::time::Duration;

/// 回合状态租约：一个回合连续静默超过该时长即视为已死
///（单次 wait 工具上限 10 分钟必有结果帧，静默 30 分钟必死）。
pub const TURN_LEASE: Duration = Duration::from_secs(30 * 60);

/// 暂存唤醒兜底：应长于回合租约——确保租约先把 busy 解除，兜底再补发。
pub const PENDING_WAKE_FALLBACK: Duration = Duration::from_secs(35 * 60);

/// 暂存唤醒兜底决策。
///
/// busy 已解除但暂存队列无人消费（没有回合结束事件来触发补发）时的行动：
/// - [`PendingWakeAction::Noop`]：队列为空，无事可做（调用方清掉兜底定时器）；
/// - [`PendingWakeAction::Retry`]：仍繁忙，等下一个租约窗口后再试；
/// - [`PendingWakeAction::Refire`]：busy 已解除，补发最早一条
///   （`index` 为应 shift 的队列下标，恒为 0，显式给出以便调用方校验）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingWakeAction {
    Noop,
    Retry,
    Refire { index: usize },
}

/// 队列条目：与 Node 版兼容的形状（reason + seq）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWakeReason {
    pub reason: String,
    pub seq: u64,
}

/// 判定暂存唤醒兜底该做什么。
///
/// `busy` 由调用方按完整 busy 判定（待发送定时器/租约/投递队列/回合状态）算出。
pub fn pending_wake_fallback_decision(
    queue: &[PendingWakeReason],
    busy: bool,
) -> PendingWakeAction {
    if queue.is_empty() {
        return PendingWakeAction::Noop;
    }
    if busy {
        return PendingWakeAction::Retry;
    }
    PendingWakeAction::Refire { index: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(reasons: &[(&str, u64)]) -> Vec<PendingWakeReason> {
        reasons
            .iter()
            .map(|(r, s)| PendingWakeReason { reason: r.to_string(), seq: *s })
            .collect()
    }

    /// 行为契约与 Node 版 test-wake-guards 一致：空队列 → noop。
    #[test]
    fn empty_queue_is_noop() {
        assert_eq!(pending_wake_fallback_decision(&[], false), PendingWakeAction::Noop);
        assert_eq!(pending_wake_fallback_decision(&[], true), PendingWakeAction::Noop);
    }

    /// 仍繁忙 → retry，不带补发下标。
    #[test]
    fn still_busy_retries() {
        let queue = q(&[("private", 24), ("anyMessage", 25)]);
        assert_eq!(pending_wake_fallback_decision(&queue, true), PendingWakeAction::Retry);
    }

    /// busy 已解除 → 补发最早一条（队列顺序即时间顺序）。
    #[test]
    fn freed_busy_refires_oldest() {
        let queue = q(&[("private", 24), ("anyMessage", 25)]);
        assert_eq!(
            pending_wake_fallback_decision(&queue, false),
            PendingWakeAction::Refire { index: 0 }
        );
    }

    /// 兜底窗口必须长于回合租约（常量关系钉死，改一个忘了另一个时编译期之外的第一道闸）。
    #[test]
    fn fallback_outlives_turn_lease() {
        assert!(PENDING_WAKE_FALLBACK > TURN_LEASE);
    }
}
