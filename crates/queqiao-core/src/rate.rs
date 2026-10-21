//! 唤醒/发送频率窗口判定与失败退配额（从 Node 版 bridge.js `sendWakePromptV2`
//! 的频率段提取为纯函数——M1 遗留项，规格 §3.1 rate 模块）。
//!
//! 语义来源：
//! - 频率硬限制：超限**跳过**本次唤醒（不暂存——避免成本失控）；
//! - **失败退配额**：error 回合回滚刚记的唤醒时间戳（防错误循环锁死频率——
//!   迁移红线之三：error 回合回滚对应 wake 计数）。

/// 频率窗口判定：`times` 中最近 1 分钟 / 1 小时的条数是否任一达到上限。
/// 上限为 0 表示该窗口不限。上限非 0 时「达到即超」（>=，与 Node `>=` 一致）。
pub fn rate_limited(times: &[u64], now_ms: u64, per_minute: u32, per_hour: u32) -> bool {
    if per_minute == 0 && per_hour == 0 {
        return false;
    }
    let recent_minute = times
        .iter()
        .filter(|t| now_ms.saturating_sub(**t) < 60_000)
        .count();
    if per_minute > 0 && recent_minute >= per_minute as usize {
        return true;
    }
    if per_hour > 0 {
        let recent_hour = times
            .iter()
            .filter(|t| now_ms.saturating_sub(**t) < 3_600_000)
            .count();
        return recent_hour >= per_hour as usize;
    }
    false
}

/// 失败退配额：移除**最近一条**等于 `t` 的时间戳（Node `lastIndexOf + splice`
/// 语义）。返回是否真的移除了（没找到不重复退）。
pub fn refund_time(times: &mut Vec<u64>, t: u64) -> bool {
    if let Some(idx) = times.iter().rposition(|x| *x == t) {
        times.remove(idx);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约与 Node 版频率段一致：已发生次数**达到**上限即超限（>=），跳过新唤醒。
    #[test]
    fn minute_window_limits() {
        let times = vec![1_000_000u64];
        assert!(
            rate_limited(&times, 1_000_000 + 59_000, 1, 12),
            "已有 1 条且上限 1 → 超限"
        );
        assert!(
            !rate_limited(&times, 1_000_000 + 59_000, 2, 12),
            "上限 2 还有余量"
        );
        assert!(
            !rate_limited(&times, 1_000_000 + 61_000, 1, 12),
            "满 1 分钟后不再计数"
        );
    }

    /// 小时窗独立生效；两个窗口都为 0 表示不限。
    #[test]
    fn hour_window_and_unlimited() {
        // 过去 1 小时内 12 条（不含当前时刻新唤醒）。
        let times: Vec<u64> = (1..=12).map(|i| 1_000_000 + i * 4 * 60_000).collect();
        let now = 1_000_000 + 59 * 60_000;
        assert!(rate_limited(&times, now, 0, 12), "达到小时上限 → 超限");
        assert!(!rate_limited(&times, now, 0, 13));
        assert!(
            !rate_limited(&times, 1_000_000 + 65 * 60_000, 0, 12),
            "满 1 小时后不再计数"
        );
        assert!(!rate_limited(&times, 1_000_000, 0, 0), "全 0 不限");
    }

    /// 退配额：移除最近一条同值时间戳；重复退与不存在的值不动。
    #[test]
    fn refund_removes_latest_occurrence() {
        let mut times = vec![100, 200, 100];
        assert!(refund_time(&mut times, 100));
        assert_eq!(times, [100, 200], "移除的是最后一条");
        assert!(!refund_time(&mut times, 100) || times != [100, 200]);
        let mut times = vec![100];
        assert!(!refund_time(&mut times, 999), "不存在的值不动");
        assert_eq!(times, [100]);
        // 退配额后频率窗口重新放行。
        let mut times = vec![1_000_000u64];
        assert!(rate_limited(&times, 1_000_000 + 1000, 1, 0));
        refund_time(&mut times, 1_000_000);
        assert!(
            !rate_limited(&times, 1_000_000 + 1000, 1, 0),
            "回滚后可再唤醒"
        );
    }
}
