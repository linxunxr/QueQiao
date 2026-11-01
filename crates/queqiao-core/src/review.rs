//! 周期复盘判定（M7 第一批次，规格 §十二 P3——对标官方 ESP 的周期性
//! 自动复盘）。纯判定：到期 + 白天窗口 + 全局安静三条件；产出面在
//! prompt 尾段与素材块（bin review_sched），本模块不碰调度与 IO。

/// 复盘判定输入（bin 从配置与状态组装；now_local_hour 由调用方从本地
/// 时间解出——core 不碰时钟）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReviewTickInput {
    pub now_ms: u64,
    /// 上次复盘时刻（0=从未——首跑即触发一次盘点，此后按周期）。
    pub last_review_at: u64,
    /// 跨会话全局最后活动时刻（安静判定的基准）。
    pub last_activity_at: u64,
    pub now_local_hour: u8,
    pub not_before_hour: u8,
    pub not_after_hour: u8,
    /// 安静阈值（默认 60min，同日记 quiet 语义）。
    pub quiet_ms: u64,
    /// 复盘周期（默认 7 天）。
    pub interval_ms: u64,
}

/// 到期且窗口与安静都满足 → 触发复盘回合。
pub fn review_due(i: &ReviewTickInput) -> bool {
    let due = i.last_review_at == 0 || i.now_ms.saturating_sub(i.last_review_at) >= i.interval_ms;
    if !due {
        return false;
    }
    // 白天窗口（默认 9~21 点）：复盘产出提议，希望 owner 当天可见。
    if i.now_local_hour < i.not_before_hour || i.now_local_hour >= i.not_after_hour {
        return false;
    }
    // 全局安静（群没人说话）——复盘是低优先级后台任务，不与人聊抢时机。
    i.now_ms.saturating_sub(i.last_activity_at) >= i.quiet_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(now: u64, last_review: u64, last_activity: u64, hour: u8) -> ReviewTickInput {
        ReviewTickInput {
            now_ms: now,
            last_review_at: last_review,
            last_activity_at: last_activity,
            now_local_hour: hour,
            not_before_hour: 9,
            not_after_hour: 21,
            quiet_ms: 3_600_000,
            interval_ms: 7 * 86_400_000,
        }
    }

    const DAY: u64 = 86_400_000;

    /// 三条件判定：从未/到期/窗口/安静全分支。
    #[test]
    fn review_due_decisions() {
        let now = 100 * DAY;
        // 从未复盘 + 白天 + 安静 → 触发（首跑盘点）。
        assert!(review_due(&input(now, 0, now - 2 * 3_600_000, 14)));
        // 未到期 → 不触发。
        assert!(!review_due(&input(
            now,
            now - 3 * DAY,
            now - 2 * 3_600_000,
            14
        )));
        // 到期但深夜 → 不触发；窗口内 → 触发。
        assert!(!review_due(&input(
            now,
            now - 8 * DAY,
            now - 2 * 3_600_000,
            22
        )));
        assert!(review_due(&input(
            now,
            now - 8 * DAY,
            now - 2 * 3_600_000,
            20
        )));
        assert!(
            !review_due(&input(now, now - 8 * DAY, now - 2 * 3_600_000, 8)),
            "早于 9 点"
        );
        // 到期且白天但群还在聊 → 不触发。
        assert!(!review_due(&input(
            now,
            now - 8 * DAY,
            now - 10 * 60_000,
            14
        )));
        // 刚好安静够 → 触发（边界含等号）。
        assert!(review_due(&input(now, now - 8 * DAY, now - 3_600_000, 14)));
    }
}
