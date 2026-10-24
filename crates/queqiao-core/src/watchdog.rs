//! 看门狗判定核心（Node 版 watchdog-core.js 逐字转译，纯函数）。
//!
//! 职责边界：桥进程崩溃由客户端/脚本守护自愈，看门狗管三种形态——桥进程
//! 活着但 HTTP 无响应（卡死）、DSH 挂了/换端口、SnowLuma 掉线。告警必须
//! 直发 OneBot HTTP（不经桥本身——桥挂了告警才发得出），由驱动方执行。

pub const WATCHDOG_TARGETS: [&str; 3] = ["bridge", "dsh", "snowluma"];

#[derive(Debug, Clone, PartialEq)]
pub struct TargetState {
    pub fail_streak: u32,
    /// 当前是否已判定挂了（去重告警用）。
    pub down: bool,
    pub last_alert_at: u64,
    /// 最近自动重启时间戳（风暴控制）。
    pub restarts: Vec<u64>,
}

#[derive(Debug, Clone)]
pub struct WatchdogState {
    pub fail_threshold: u32,
    pub max_restarts_per_hour: u32,
    pub alert_resend_ms: u64,
    pub targets: [(&'static str, TargetState); 3],
}

impl Default for WatchdogState {
    fn default() -> Self {
        Self::new(2, 2, 30 * 60_000)
    }
}

impl WatchdogState {
    pub fn new(fail_threshold: u32, max_restarts_per_hour: u32, alert_resend_ms: u64) -> Self {
        Self {
            fail_threshold,
            max_restarts_per_hour,
            alert_resend_ms,
            targets: WATCHDOG_TARGETS.map(|name| {
                (
                    name,
                    TargetState {
                        fail_streak: 0,
                        down: false,
                        last_alert_at: 0,
                        restarts: Vec::new(),
                    },
                )
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WatchdogAction {
    /// 判挂（或长挂重复提醒）——驱动方直发 OneBot 告警。
    Alert { target: &'static str, detail: String },
    /// 从故障恢复。
    Recovered { target: &'static str },
    /// 自动重启（autoRestart 开启且未超每小时上限；bridge 不接管）。
    Restart { target: &'static str },
}

/// 一轮探测 → 动作。`probes` 三键 true=健康；防抖：连续 fail_threshold 次
/// 失败才判挂；down 翻转去重；alert_resend_ms 后重复提醒一次。
pub fn evaluate(
    state: &mut WatchdogState,
    probes: [bool; 3],
    now_ms: u64,
    auto_restart: bool,
) -> Vec<WatchdogAction> {
    let mut actions = Vec::new();
    for (i, (name, t)) in state.targets.iter_mut().enumerate() {
        let healthy = probes[i];
        if healthy {
            t.fail_streak = 0;
            if t.down {
                t.down = false;
                actions.push(WatchdogAction::Recovered { target: name });
            }
            continue;
        }
        t.fail_streak += 1;
        if t.fail_streak < state.fail_threshold {
            continue;
        }
        if !t.down {
            t.down = true;
            t.last_alert_at = now_ms;
            actions.push(WatchdogAction::Alert {
                target: name,
                detail: format!("连续 {} 次探测失败", t.fail_streak),
            });
        } else if now_ms.saturating_sub(t.last_alert_at) >= state.alert_resend_ms {
            t.last_alert_at = now_ms;
            actions.push(WatchdogAction::Alert {
                target: name,
                detail: "仍在故障中（重复提醒）".into(),
            });
        }
        if auto_restart && *name != "bridge" {
            t.restarts.retain(|ts| now_ms.saturating_sub(*ts) < 3_600_000);
            if (t.restarts.len() as u32) < state.max_restarts_per_hour {
                t.restarts.push(now_ms);
                actions.push(WatchdogAction::Restart { target: name });
            }
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_and_dedupe() {
        let mut st = WatchdogState::default();
        // 单次失败不告警（防抖）。
        assert!(evaluate(&mut st, [true, false, true], 1000, false).is_empty());
        // 第二次连续失败 → 判挂告警。
        let a = evaluate(&mut st, [true, false, true], 2000, false);
        assert!(matches!(a[0], WatchdogAction::Alert { target: "dsh", .. }));
        // 持续失败不重复（去重）；30min 后重复提醒一次。
        assert!(evaluate(&mut st, [true, false, true], 3000, false).is_empty());
        let a = evaluate(&mut st, [true, false, true], 3_000 + 30 * 60_000, false);
        assert!(matches!(a[0], WatchdogAction::Alert { target: "dsh", detail: ref d } if d.contains("重复")));
        // 恢复 → Recovered 一次。
        let a = evaluate(&mut st, [true, true, true], 999_999, false);
        assert!(matches!(a[0], WatchdogAction::Recovered { target: "dsh" }));
        assert!(evaluate(&mut st, [true, true, true], 1_000_000, false).is_empty());
    }

    #[test]
    fn restart_storm_control() {
        let mut st = WatchdogState::default();
        for _ in 0..2 {
            evaluate(&mut st, [false, false, false], 10_000, true);
        }
        // 三目标都判挂；dsh/snowluma 各重启 1 次（bridge 不接管），本小时内
        // 第二轮不再重启（上限 2 已被两目标占满——每目标独立计数）。
        let a = evaluate(&mut st, [false, false, false], 11_000, true);
        assert_eq!(
            a.iter().filter(|x| matches!(x, WatchdogAction::Restart { .. })).count(),
            2,
            "每目标独立计数：dsh+snowluma 各一"
        );
        // bridge 永不 restart。
        assert!(!a.iter().any(|x| matches!(x, WatchdogAction::Restart { target: "bridge" })));
    }

    #[test]
    fn alert_recovery_cycle_on_bridge() {
        let mut st = WatchdogState::default();
        evaluate(&mut st, [false, true, true], 100, false);
        let a = evaluate(&mut st, [false, true, true], 200, false);
        assert!(matches!(a[0], WatchdogAction::Alert { target: "bridge", .. }));
    }
}
