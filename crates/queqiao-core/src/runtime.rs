//! 运行时会话状态与主决策函数（busy 四条件 / 回合租约 / 暂存兜底 / 退配额 /
//! noAction 重置 / 日记与主动检查——迁移红线的运行时组装，规格 §五全链）。
//!
//! 分层：`SessionRuntime` 持有单会话的运行时态（**不落盘**）与持久字段镜像
//! （wake_times 等，bin 与 ConversationState 双向同步）；[`decide`] 把
//! [`BridgeEvent`] 变换为 [`BridgeAction`] 序列，IO 全在 bin 解释执行。

use crate::action::{BridgeAction, BridgeEvent, IncomingMessage, TimerKind};
use crate::diary::{
    CivilTime, DiaryTickDecision, DiaryTickInput, DiaryTickMode, diary_tick_decision,
};
use crate::rate::{rate_limited, refund_time};
use crate::schedule::{WakeSchedule, defer_wake, schedule_wake};
use crate::triggers::{TriggerRules, evaluate_trigger};
use crate::wake::{PendingWakeAction, PendingWakeReason, pending_wake_fallback_decision};

/// 回合状态（busy 条件之一；任意帧刷新 `last_beat_at`，静默超限由
/// TimerFired(TurnLease) 强制清理——迁移红线之二）。
#[derive(Debug, Clone, PartialEq)]
pub struct TurnState {
    pub session_id: String,
    pub started_at: u64,
    pub last_beat_at: u64,
    /// 回本累计文本（TurnCollector 集成）。
    pub text: String,
}

/// 待发送合批窗口（busy 条件之一）。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingTimer {
    pub reason: String,
}

/// 单会话运行时状态。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRuntime {
    pub key: String,
    // ── 持久镜像（bin 与 ConversationState 同步；落盘截断归 bin）──
    pub wake_times: Vec<u64>,
    pub last_activity_at: u64,
    pub last_action_at: u64,
    pub no_action_count: u32,
    pub written_today: bool,
    pub last_diary_try_at: u64,
    pub bootstrap_sent: bool,
    pub wake: TriggerRules,
    // ── 运行时（绝不落盘，重启即清）──
    pub seq: u64,
    pub pending_queue: Vec<PendingWakeReason>,
    pub pending_timer: Option<PendingTimer>,
    pub turn: Option<TurnState>,
    /// 最近一次真实唤醒的时刻（error 回合退配额的回滚锚点）。
    /// 历史超限上次退役时间戳（0=无；冷却防误判循环——Node historyTrimAt 内存语义一致）。
    pub history_trim_at: u64,
    /// 瞬时错误已重投次数（成功回合清零；用尽后错误亮给群友）。
    pub agent_retry_attempts: u32,
    pub last_wake_at: Option<u64>,
    /// 唤醒在途（SendPrompt 已发、回合未开始——Node pendingWakeKeys）。
    pub prompt_in_flight: bool,
}

impl SessionRuntime {
    pub fn new(key: &str, wake: TriggerRules) -> Self {
        Self {
            key: key.to_owned(),
            wake_times: Vec::new(),
            last_activity_at: 0,
            last_action_at: 0,
            no_action_count: 0,
            written_today: false,
            last_diary_try_at: 0,
            bootstrap_sent: false,
            wake,
            seq: 0,
            pending_queue: Vec::new(),
            pending_timer: None,
            turn: None,
            history_trim_at: 0,
            agent_retry_attempts: 0,
            last_wake_at: None,
            prompt_in_flight: false,
        }
    }

    /// busy 四条件（迁移红线之一，对齐 Node isConversationBusyV2）：
    /// ①待发送合批定时器；②唤醒在途（SendPrompt 已出、回合未开始——Node
    /// pendingWakeKeys）；③DSH prompt 投递队列运行中（bin 层串行化队列，
    /// 投递期间映射为 prompt_in_flight）；④回合状态（含租约心跳）。
    /// **暂存唤醒队列不是 busy 条件**——它是 busy 的结果，不是原因。
    pub fn is_busy(&self) -> bool {
        self.pending_timer.is_some() || self.prompt_in_flight || self.turn.is_some()
    }
}

/// 决策参数（配置切片，bin 从 Config 转换）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecideRules {
    pub batch_window_ms: u64,
    pub max_wake_per_minute: u32,
    pub max_wake_per_hour: u32,
    pub no_action_limit: u32,
    /// 回合租约时长（默认 30min）。
    pub turn_lease_ms: u64,
    /// 暂存兜底时长（默认 35min，必须 > 租约）。
    pub pending_fallback_ms: u64,
    /// 历史超限退役冷却（默认 10min：冷却内重复 413 不再触发，防误判循环）。
    pub history_trim_cooldown_ms: u64,
    /// 历史超限退役后的重投延迟（默认 3s）。
    pub history_trim_resend_ms: u64,
    /// 主动冒泡（Node `socialV2.proactive`——scheduleProactiveCheckV2 语义）。
    pub proactive: ProactiveRules,
}

/// 主动冒泡参数：每会话一条常驻检查环（区间随机间隔，命中与否都自重排），
/// 触发时按「群安静 ≥ 阈值 × 概率 × 不繁忙」判定是否唤醒（reason=
/// `proactiveCheck`）。默认值对齐 Node v2 配置段。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProactiveRules {
    pub enabled: bool,
    pub check_min_ms: u64,
    pub check_max_ms: u64,
    pub idle_threshold_ms: u64,
    pub probability: f64,
}

impl Default for ProactiveRules {
    fn default() -> Self {
        Self {
            enabled: true,
            check_min_ms: 30 * 60_000,
            check_max_ms: 90 * 60_000,
            idle_threshold_ms: 15 * 60_000,
            probability: 0.3,
        }
    }
}

/// 下一次主动检查的随机间隔（Node 语义：min 下限 60s、max 不低于 min）。
pub fn proactive_check_delay(p: &ProactiveRules, rng: &mut dyn FnMut() -> f64) -> u64 {
    let min = p.check_min_ms.max(60_000);
    let max = p.check_max_ms.max(min);
    min + (rng() * (max - min) as f64) as u64
}

impl Default for DecideRules {
    fn default() -> Self {
        Self {
            batch_window_ms: 8000,
            max_wake_per_minute: 1,
            max_wake_per_hour: 12,
            no_action_limit: 3,
            turn_lease_ms: 30 * 60_000,
            pending_fallback_ms: 35 * 60_000,
            history_trim_cooldown_ms: 10 * 60_000,
            history_trim_resend_ms: 3_000,
            proactive: ProactiveRules::default(),
        }
    }
}

/// 日记决策参数（全局一份；日记是跨会话统一一本，用虚拟会话键）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiaryRules {
    pub enabled: bool,
    pub hour: u8,
    pub not_before_hour: u8,
    pub quiet_ms: u64,
    pub retry_cooldown_ms: u64,
}

impl Default for DiaryRules {
    fn default() -> Self {
        Self {
            enabled: true,
            hour: 23,
            not_before_hour: 18,
            quiet_ms: 3_600_000,
            retry_cooldown_ms: 3_600_000,
        }
    }
}

pub const DIARY_KEY: &str = "diary";
pub const DIARY_TICK_MS: u64 = 60_000;

/// 内部：真正发送唤醒（频率判定 + 记账 + 动作）。
fn fire_wake(
    rt: &mut SessionRuntime,
    reason: &str,
    now_ms: u64,
    rules: &DecideRules,
) -> Vec<BridgeAction> {
    if rate_limited(
        &rt.wake_times,
        now_ms,
        rules.max_wake_per_minute,
        rules.max_wake_per_hour,
    ) {
        return vec![BridgeAction::Log {
            message: format!("[{}] 唤醒频率超限，跳过（{}）", rt.key, reason),
        }];
    }
    rt.wake_times.push(now_ms);
    let len = rt.wake_times.len();
    if len > 200 {
        rt.wake_times.drain(0..len - 200);
    }
    rt.last_wake_at = Some(now_ms);
    rt.prompt_in_flight = true;
    rt.pending_timer = None;
    vec![
        BridgeAction::SendPrompt {
            key: rt.key.clone(),
            reason: reason.to_owned(),
        },
        BridgeAction::ScheduleTimer {
            key: rt.key.clone(),
            timer: TimerKind::TurnLease,
            delay_ms: rules.turn_lease_ms,
        },
        BridgeAction::ScheduleTimer {
            key: rt.key.clone(),
            timer: TimerKind::PendingWakeFallback,
            delay_ms: rules.pending_fallback_ms,
        },
        BridgeAction::Log {
            message: format!("[{}] 唤醒（{}）", rt.key, reason),
        },
    ]
}

/// 内部：繁忙时的暂存路径（去重入队 + 兜底定时器）。
fn defer_wake_with_fallback(
    rt: &mut SessionRuntime,
    reason: &str,
    rules: &DecideRules,
) -> Vec<BridgeAction> {
    defer_wake(&mut rt.pending_queue, reason, rt.seq);
    vec![BridgeAction::ScheduleTimer {
        key: rt.key.clone(),
        timer: TimerKind::PendingWakeFallback,
        delay_ms: rules.pending_fallback_ms,
    }]
}

/// 主决策函数：事件 + 状态 → 动作序列（bin 解释执行）。
///
/// `rng` 提供 `[0,1)` 随机数（probability 档与主动检查用）；`now_local` 是
/// 本地民用时间（日记判定用，bin 换算）。
pub fn decide(
    rt: &mut SessionRuntime,
    event: &BridgeEvent,
    now_ms: u64,
    now_local: CivilTime,
    rules: &DecideRules,
    diary: &DiaryRules,
    rng: &mut dyn FnMut() -> f64,
) -> Vec<BridgeAction> {
    match event {
        BridgeEvent::MessageArrived(msg) => on_message(rt, msg, now_ms, rules, rng),
        BridgeEvent::PokeArrived { .. } => on_poke(rt, now_ms, rules),
        BridgeEvent::TimerFired { timer, .. } => {
            on_timer(rt, *timer, now_ms, now_local, rules, diary, rng)
        }
        BridgeEvent::TurnStarted { session_id, .. } => {
            rt.prompt_in_flight = false;
            if rt.turn.is_none() {
                rt.turn = Some(TurnState {
                    session_id: session_id.clone(),
                    started_at: now_ms,
                    last_beat_at: now_ms,
                    text: String::new(),
                });
            }
            Vec::new()
        }
        BridgeEvent::TurnBeat { .. } => {
            if let Some(turn) = rt.turn.as_mut() {
                turn.last_beat_at = now_ms;
            }
            Vec::new()
        }
        BridgeEvent::TurnEnded { ok, error_text, .. } => {
            on_turn_ended(rt, *ok, error_text.as_deref(), now_ms, rules)
        }
        BridgeEvent::UpstreamHeartbeat { .. } => Vec::new(),
    }
}

fn on_message(
    rt: &mut SessionRuntime,
    msg: &IncomingMessage,
    now_ms: u64,
    rules: &DecideRules,
    rng: &mut dyn FnMut() -> f64,
) -> Vec<BridgeAction> {
    // 消息先入状态（水位推进与活跃度更新——bin 负责持久镜像同步，这里只推运行时）。
    if msg.seq > rt.seq {
        rt.seq = msg.seq;
    }
    rt.last_activity_at = now_ms;
    if msg.is_self {
        // 自己的回显不触发（防自激励）。
        return Vec::new();
    }
    if !rt.bootstrap_sent {
        // 首条消息触发 bootstrap 唤醒（建立 agent 上下文）。
        rt.bootstrap_sent = true;
        return enter_schedule(rt, "bootstrap", rules);
    }
    let Some(reason) = evaluate_trigger(&rt.wake, msg, rng) else {
        return Vec::new();
    };
    enter_schedule(rt, &reason, rules)
}

fn on_poke(rt: &mut SessionRuntime, _now_ms: u64, rules: &DecideRules) -> Vec<BridgeAction> {
    // 拍一拍不区分目标（拍别人也是群事件；自己拍的不进这里——bin 过滤）。
    if !rt.wake.poke {
        return Vec::new();
    }
    if !rt.bootstrap_sent {
        rt.bootstrap_sent = true;
        return enter_schedule(rt, "bootstrap", rules);
    }
    enter_schedule(rt, "poke", rules)
}

fn enter_schedule(rt: &mut SessionRuntime, reason: &str, rules: &DecideRules) -> Vec<BridgeAction> {
    let busy = rt.is_busy();
    let schedule = schedule_wake(
        rt.pending_timer.is_some(),
        rt.pending_timer.as_ref().map(|t| t.reason.as_str()),
        busy,
        reason,
        rules.batch_window_ms,
    );
    match schedule {
        WakeSchedule::StartBatch { reason, batch_ms } => {
            rt.pending_timer = Some(PendingTimer {
                reason: reason.clone(),
            });
            vec![
                BridgeAction::ScheduleTimer {
                    key: rt.key.clone(),
                    timer: TimerKind::BatchWindow,
                    delay_ms: batch_ms,
                },
                BridgeAction::Log {
                    message: format!(
                        "[{}] 计划唤醒（{}），{}s 后发送",
                        rt.key,
                        reason,
                        batch_ms / 1000
                    ),
                },
            ]
        }
        WakeSchedule::MergeInWindow(outcome) => {
            if let crate::schedule::MergeOutcome::Upgraded { to, .. } = outcome {
                if let Some(timer) = rt.pending_timer.as_mut() {
                    timer.reason = to.clone();
                }
                return vec![BridgeAction::Log {
                    message: format!("[{}] 合并窗口内升级唤醒原因 → {to}", rt.key),
                }];
            }
            Vec::new()
        }
        WakeSchedule::DeferToQueue => defer_wake_with_fallback(rt, reason, rules),
    }
}

fn on_timer(
    rt: &mut SessionRuntime,
    timer: TimerKind,
    now_ms: u64,
    now_local: CivilTime,
    rules: &DecideRules,
    diary: &DiaryRules,
    rng: &mut dyn FnMut() -> f64,
) -> Vec<BridgeAction> {
    match timer {
        TimerKind::BatchWindow => {
            let Some(pending) = rt.pending_timer.take() else {
                return Vec::new();
            };
            // 到点重查 busy：窗口期间回合可能已经开始。
            if rt.is_busy() {
                return defer_wake_with_fallback(rt, &pending.reason, rules);
            }
            fire_wake(rt, &pending.reason, now_ms, rules)
        }
        TimerKind::TurnLease => {
            // 回合静默超限：强制解除 busy（迁移红线之二——租约先于兜底）。
            if rt.turn.take().is_some() {
                let log = BridgeAction::Log {
                    message: format!("[{}] 回合租约到期，强制解除 busy", rt.key),
                };
                // 解除后若暂存非空，立即走兜底路径补发。
                if !rt.pending_queue.is_empty() {
                    let mut actions = vec![log];
                    actions.extend(on_fallback(rt, now_ms, rules));
                    return actions;
                }
                return vec![log];
            }
            Vec::new()
        }
        TimerKind::PendingWakeFallback => on_fallback(rt, now_ms, rules),
        TimerKind::SleepUntil => fire_wake(rt, "timeout", now_ms, rules),
        // 413 退役重投：绕频率判定（失败回合已退配额），直接重发唤醒。
        TimerKind::HistoryTrim => force_wake(rt, "history-trim", now_ms, rules),
        TimerKind::AgentRetry => force_wake(rt, "agent-retry", now_ms, rules),
        TimerKind::DiaryTick => {
            // 日记三路径判定（定点 / 安静提前 / 冷却）。
            let input = DiaryTickInput {
                now_ms,
                now_local,
                last_activity_at: rt.last_activity_at,
                written_today: rt.written_today,
                last_tried_at: rt.last_diary_try_at,
                retry_cooldown_ms: diary.retry_cooldown_ms,
                hour: diary.hour,
                not_before_hour: diary.not_before_hour,
                quiet_ms: diary.quiet_ms,
            };
            match diary_tick_decision(&input) {
                DiaryTickDecision::Write(mode) => {
                    rt.last_diary_try_at = now_ms;
                    let label = match mode {
                        DiaryTickMode::Scheduled => "scheduled",
                        DiaryTickMode::Quiet => "quiet",
                    };
                    vec![
                        BridgeAction::SendPrompt {
                            key: DIARY_KEY.to_owned(),
                            reason: format!("diary:{label}"),
                        },
                        BridgeAction::Log {
                            message: format!("日记触发（{label}）"),
                        },
                    ]
                }
                DiaryTickDecision::Skip => Vec::new(),
            }
        }
        TimerKind::ProactiveCheck => {
            // 主动检查（Node scheduleProactiveCheckV2 回调语义）：群安静达阈值、
            // 随机命中且不繁忙才唤醒；无论命中与否都按区间随机重排下一次，
            // 检查环常驻（enabled=false 时熄环——不再重排）。
            let mut actions = Vec::new();
            if rules.proactive.enabled {
                let idle_ms = now_ms.saturating_sub(rt.last_activity_at);
                if idle_ms >= rules.proactive.idle_threshold_ms
                    && rng() < rules.proactive.probability
                    && !rt.is_busy()
                {
                    actions.extend(fire_wake(rt, "proactiveCheck", now_ms, rules));
                }
                actions.push(BridgeAction::ScheduleTimer {
                    key: rt.key.clone(),
                    timer: TimerKind::ProactiveCheck,
                    delay_ms: proactive_check_delay(&rules.proactive, rng),
                });
            }
            actions
        }
    }
}

fn on_fallback(rt: &mut SessionRuntime, now_ms: u64, rules: &DecideRules) -> Vec<BridgeAction> {
    let busy = rt.is_busy();
    match pending_wake_fallback_decision(&rt.pending_queue, busy) {
        PendingWakeAction::Noop => Vec::new(),
        PendingWakeAction::Retry => vec![BridgeAction::ScheduleTimer {
            key: rt.key.clone(),
            timer: TimerKind::PendingWakeFallback,
            delay_ms: rules.turn_lease_ms,
        }],
        PendingWakeAction::Refire { .. } => {
            let entry = rt.pending_queue.remove(0);
            fire_wake(rt, &entry.reason, now_ms, rules)
        }
    }
}

/// 历史超限重投（绕频率判定）：与 fire_wake 同样记账与动作，但不吃频率窗——
/// 失败回合已退配额，重投再被限流会把 413 修复死锁在冷却里。
fn force_wake(
    rt: &mut SessionRuntime,
    reason: &str,
    now_ms: u64,
    rules: &DecideRules,
) -> Vec<BridgeAction> {
    rt.wake_times.push(now_ms);
    let len = rt.wake_times.len();
    if len > 200 {
        rt.wake_times.drain(0..len - 200);
    }
    rt.last_wake_at = Some(now_ms);
    rt.prompt_in_flight = true;
    vec![
        BridgeAction::SendPrompt {
            key: rt.key.clone(),
            reason: reason.to_owned(),
        },
        BridgeAction::ScheduleTimer {
            key: rt.key.clone(),
            timer: TimerKind::TurnLease,
            delay_ms: rules.turn_lease_ms,
        },
        BridgeAction::ScheduleTimer {
            key: rt.key.clone(),
            timer: TimerKind::PendingWakeFallback,
            delay_ms: rules.pending_fallback_ms,
        },
    ]
}

fn on_turn_ended(
    rt: &mut SessionRuntime,
    ok: bool,
    error_text: Option<&str>,
    now_ms: u64,
    rules: &DecideRules,
) -> Vec<BridgeAction> {
    // 孤儿 end（turn/start 丢失但 end 仍到达）也走退配额——error 就是 error。
    rt.prompt_in_flight = false;
    let turn = rt.turn.take();
    if turn.is_none() && rt.last_wake_at.is_none() {
        return Vec::new();
    }
    let mut actions = vec![
        BridgeAction::CancelTimer {
            key: rt.key.clone(),
            timer: TimerKind::TurnLease,
        },
        BridgeAction::CancelTimer {
            key: rt.key.clone(),
            timer: TimerKind::PendingWakeFallback,
        },
    ];
    if !ok {
        // 失败回合退配额（迁移红线之三）：回滚本回合记的唤醒时间戳。
        if let Some(at) = rt.last_wake_at.take()
            && refund_time(&mut rt.wake_times, at)
        {
            actions.push(BridgeAction::RefundWakeTime {
                key: rt.key.clone(),
                at,
            });
            actions.push(BridgeAction::Log {
                message: format!("[{}] error 回合退配额（回滚 @{at}）", rt.key),
            });
        }
        // 历史超限（413 chat_history_too_large）：重试无效——退役 DSH 会话
        // 重建（桥侧记忆保留）+ 3s 后重投；10min 冷却防误判循环
        //（Node [history-trim] 同源；连续 413 不该把会话锁进频率限制）。
        if let Some(text) = error_text
            && crate::retry::is_history_limit_error(text)
            && now_ms.saturating_sub(rt.history_trim_at) > rules.history_trim_cooldown_ms
        {
            rt.history_trim_at = now_ms;
            actions.push(BridgeAction::Log {
                message: format!(
                    "[{}] 会话历史超上游硬限，退役重建 DSH 会话（桥侧记忆保留），{}s 后重投",
                    rt.key,
                    rules.history_trim_resend_ms / 1000
                ),
            });
            actions.push(BridgeAction::RetireSession {
                key: rt.key.clone(),
            });
            actions.push(BridgeAction::ScheduleTimer {
                key: rt.key.clone(),
                timer: TimerKind::HistoryTrim,
                delay_ms: rules.history_trim_resend_ms,
            });
        }
        // 瞬时错误（容量/限流/502/503）：退避后静默重投（8s/20s，最多 2 次）；
        // 用尽或不可重试才把错误亮给群友（Node agent-retry 同源——重投也绕频率）。
        if let Some(text) = error_text
            && !crate::retry::is_history_limit_error(text)
        {
            let attempt = rt.agent_retry_attempts;
            match crate::retry::retry_backoff(text, attempt) {
                Some(backoff) => {
                    rt.agent_retry_attempts = attempt + 1;
                    let secs = backoff.as_millis() / 1000;
                    actions.push(BridgeAction::Log {
                        message: format!(
                            "[{}] 回合瞬时错误，{}s 后重投（第 {}/{} 次）：{}",
                            rt.key,
                            secs,
                            attempt + 1,
                            crate::retry::AGENT_RETRY_MAX,
                            text.chars().take(100).collect::<String>()
                        ),
                    });
                    actions.push(BridgeAction::ScheduleTimer {
                        key: rt.key.clone(),
                        timer: TimerKind::AgentRetry,
                        delay_ms: backoff.as_millis() as u64,
                    });
                }
                None => {
                    let note = if attempt > 0 {
                        format!("（已自动重试 {attempt} 次）")
                    } else {
                        String::new()
                    };
                    let brief: String = text.chars().take(500).collect();
                    actions.push(BridgeAction::SendMessage {
                        key: rt.key.clone(),
                        text: format!("⚠️ agent 处理出错{note}：{brief}"),
                        reply_to: None,
                    });
                    rt.agent_retry_attempts = 0;
                }
            }
        }
        rt.last_wake_at = None;
    } else {
        rt.last_wake_at = None;
        // 成功回合清零瞬时重投计数。
        rt.agent_retry_attempts = 0;
        // noAction 判定：回合期间既没发消息也没动作 → 累计；达限重置唤醒配置
        //（防 AI 卡死——连续无动作说明配置不再适配当前会话节奏）。
        // 孤儿 end 没有回合起点，跳过 noAction 统计（无从判定动作窗口）。
        let action_taken = turn
            .as_ref()
            .is_none_or(|t| rt.last_action_at >= t.started_at || !t.text.is_empty());
        if turn.is_none() {
            // 孤儿 end：不参与 noAction 统计。
        } else if action_taken {
            rt.no_action_count = 0;
        } else {
            rt.no_action_count += 1;
            if rt.no_action_count >= rules.no_action_limit {
                rt.wake = TriggerRules::default();
                rt.bootstrap_sent = true;
                rt.no_action_count = 0;
                actions.push(BridgeAction::Log {
                    message: format!(
                        "[{}] 连续 {} 次唤醒无行动，重置唤醒配置",
                        rt.key, rules.no_action_limit
                    ),
                });
            }
        }
    }
    // 回合结束消费暂存队列（暂存的常规消费时机——Node 语义：turn end 处理器
    // 直接消费，不查 busy 四条件；频率判定在 fire_wake 内）。
    if !rt.pending_queue.is_empty() {
        let entry = rt.pending_queue.remove(0);
        actions.extend(fire_wake(rt, &entry.reason, now_ms, rules));
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::IncomingMessage;

    fn msg(text: &str) -> IncomingMessage {
        IncomingMessage {
            key: "group:4242".into(),
            seq: 1,
            message_id: "1".into(),
            sender: "群友".into(),
            user_id: "10001".into(),
            text: text.into(),
            is_self: false,
            at_me: false,
            reply_to_me: false,
            time_ms: 0,
        }
    }

    fn runtime() -> SessionRuntime {
        let mut rt = SessionRuntime::new(
            "group:4242",
            TriggerRules {
                keywords: vec!["小鲸鱼".into()],
                ..Default::default()
            },
        );
        rt.bootstrap_sent = true;
        rt
    }

    fn decide1(rt: &mut SessionRuntime, event: &BridgeEvent, now_ms: u64) -> Vec<BridgeAction> {
        let rules = DecideRules::default();
        let diary = DiaryRules::default();
        let mut rng = || 0.99;
        decide(
            rt,
            event,
            now_ms,
            crate::diary::CivilTime {
                date: crate::diary::CivilDate {
                    year: 2026,
                    month: 10,
                    day: 21,
                },
                hour: 12,
                minute: 0,
            },
            &rules,
            &diary,
            &mut rng,
        )
    }

    /// 消息 → 合批窗口 → 到点发送：完整链（含频率记账与租约/兜底定时器）。
    #[test]
    fn message_to_batch_to_wake() {
        let mut rt = runtime();
        let actions = decide1(
            &mut rt,
            &BridgeEvent::MessageArrived(Box::new(msg("小鲸鱼在吗"))),
            1000,
        );
        // 触发 keyword → 开合批窗口。
        assert!(matches!(
            &actions[..],
            [
                BridgeAction::ScheduleTimer {
                    timer: TimerKind::BatchWindow,
                    delay_ms: 8000,
                    ..
                },
                _
            ]
        ));
        assert!(rt.pending_timer.is_some());

        // 窗口内来一条更高优先级（@）→ 升级。
        let mut m = msg("看这个");
        m.at_me = true;
        m.seq = 2;
        decide1(&mut rt, &BridgeEvent::MessageArrived(Box::new(m)), 2000);
        assert_eq!(rt.pending_timer.as_ref().unwrap().reason, "atMention");

        // 到点：发送 + 租约/兜底 + wakeTimes 记账。
        let actions = decide1(
            &mut rt,
            &BridgeEvent::TimerFired {
                key: "group:4242".into(),
                timer: TimerKind::BatchWindow,
            },
            9000,
        );
        assert!(actions.iter().any(
            |a| matches!(a, BridgeAction::SendPrompt { reason, .. } if reason == "atMention")
        ));
        assert!(actions.iter().any(|a| matches!(a, BridgeAction::ScheduleTimer { timer: TimerKind::TurnLease, delay_ms, .. } if *delay_ms == 1_800_000)));
        assert_eq!(rt.wake_times, [9000]);
        assert_eq!(rt.last_wake_at, Some(9000));
        assert!(rt.is_busy(), "唤醒在途即 busy");
    }

    /// busy 期间的消息进暂存；回合结束消费暂存（红线一：不打断回合）。
    #[test]
    fn busy_defers_and_consumes_after_turn() {
        let mut rt = runtime();
        decide1(
            &mut rt,
            &BridgeEvent::MessageArrived(Box::new(msg("小鲸鱼"))),
            1000,
        );
        decide1(
            &mut rt,
            &BridgeEvent::TimerFired {
                key: "group:4242".to_owned(),
                timer: TimerKind::BatchWindow,
            },
            9000,
        );
        // 回合开始后新消息 → 暂存。
        decide1(
            &mut rt,
            &BridgeEvent::TurnStarted {
                key: "group:4242".to_owned(),
                session_id: "s-1".into(),
            },
            10000,
        );
        let mut m = msg("小鲸鱼还在吗");
        m.seq = 2;
        let actions = decide1(&mut rt, &BridgeEvent::MessageArrived(Box::new(m)), 11000);
        assert!(actions.iter().any(|a| matches!(
            a,
            BridgeAction::ScheduleTimer {
                timer: TimerKind::PendingWakeFallback,
                ..
            }
        )));
        assert_eq!(rt.pending_queue.len(), 1);
        // 回合正常结束：消费暂存 → 新唤醒（时间推到分钟窗外，让频率判定放行）。
        let actions = decide1(
            &mut rt,
            &BridgeEvent::TurnEnded {
                key: "group:4242".to_owned(),
                session_id: "s-1".into(),
                ok: true,
                error_text: None,
            },
            9000 + 61_000,
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, BridgeAction::SendPrompt { .. })),
            "暂存被消费"
        );
        assert!(rt.pending_queue.is_empty());
        assert_eq!(rt.wake_times.len(), 2);
    }

    /// error 回合退配额（红线三）：wakeTimes 回滚 + RefundWakeTime 动作。
    #[test]
    fn error_turn_refunds_wake_time() {
        let mut rt = runtime();
        decide1(
            &mut rt,
            &BridgeEvent::MessageArrived(Box::new(msg("小鲸鱼"))),
            1000,
        );
        decide1(
            &mut rt,
            &BridgeEvent::TimerFired {
                key: "group:4242".to_owned(),
                timer: TimerKind::BatchWindow,
            },
            9000,
        );
        assert_eq!(rt.wake_times, [9000]);
        // error 回合（无 TurnStarted 的孤儿 end）→ 仍回滚。
        let actions = decide1(
            &mut rt,
            &BridgeEvent::TurnEnded {
                key: "group:4242".to_owned(),
                session_id: "s-1".into(),
                ok: false,
                error_text: None,
            },
            12000,
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, BridgeAction::RefundWakeTime { at: 9000, .. }))
        );
        assert!(rt.wake_times.is_empty(), "回滚后可再唤醒");
    }

    /// 回合租约到期强制解 busy（红线二），随后兜底补发暂存。
    #[test]
    fn turn_lease_frees_busy_and_refires_pending() {
        let mut rt = runtime();
        decide1(
            &mut rt,
            &BridgeEvent::MessageArrived(Box::new(msg("小鲸鱼"))),
            1000,
        );
        decide1(
            &mut rt,
            &BridgeEvent::TimerFired {
                key: "group:4242".to_owned(),
                timer: TimerKind::BatchWindow,
            },
            9000,
        );
        decide1(
            &mut rt,
            &BridgeEvent::TurnStarted {
                key: "group:4242".to_owned(),
                session_id: "s-1".into(),
            },
            10000,
        );
        let mut m = msg("小鲸鱼人呢");
        m.seq = 5;
        decide1(&mut rt, &BridgeEvent::MessageArrived(Box::new(m)), 11000);
        assert_eq!(rt.pending_queue.len(), 1);
        // 租约到期（turn/end 丢失的场景）。
        let actions = decide1(
            &mut rt,
            &BridgeEvent::TimerFired {
                key: "group:4242".to_owned(),
                timer: TimerKind::TurnLease,
            },
            11000 + 30 * 60_000,
        );
        assert!(rt.turn.is_none(), "租约清回合");
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, BridgeAction::SendPrompt { .. })),
            "暂存立即补发"
        );
    }

    /// noAction：连续无动作回合达限 → 重置唤醒配置（防 AI 卡死）。
    #[test]
    fn no_action_resets_wake_config() {
        let mut rt = runtime();
        rt.wake.probability = 0.9;
        for round in 0..3u64 {
            decide1(
                &mut rt,
                &BridgeEvent::TurnStarted {
                    key: "group:4242".to_owned(),
                    session_id: "s".into(),
                },
                (round + 1) * 1000,
            );
            let actions = decide1(
                &mut rt,
                &BridgeEvent::TurnEnded {
                    key: "group:4242".to_owned(),
                    session_id: "s".into(),
                    ok: true,
                    error_text: None,
                },
                (round + 1) * 1000 + 100,
            );
            if round < 2 {
                assert!(rt.no_action_count == round as u32 + 1);
            } else {
                assert!(rt.wake == TriggerRules::default(), "达限重置为默认配置");
                assert!(
                    actions
                        .iter()
                        .any(|a| matches!(a, BridgeAction::Log { .. }))
                );
            }
        }
        // 有动作（last_action_at 在回合内）→ 清零。
        rt.wake.probability = 0.9;
        rt.last_action_at = 9000;
        decide1(
            &mut rt,
            &BridgeEvent::TurnStarted {
                key: "group:4242".to_owned(),
                session_id: "s".into(),
            },
            8000,
        );
        decide1(
            &mut rt,
            &BridgeEvent::TurnEnded {
                key: "group:4242".to_owned(),
                session_id: "s".into(),
                ok: true,
                error_text: None,
            },
            8500,
        );
        assert_eq!(rt.no_action_count, 0);
    }

    /// 频率超限跳过（rate 红线：错误循环之外的成本失控防线）。
    #[test]
    fn rate_limit_skips_wake() {
        let mut rt = runtime();
        rt.wake_times = vec![1000];
        decide1(
            &mut rt,
            &BridgeEvent::MessageArrived(Box::new(msg("小鲸鱼"))),
            2000,
        );
        let actions = decide1(
            &mut rt,
            &BridgeEvent::TimerFired {
                key: "group:4242".to_owned(),
                timer: TimerKind::BatchWindow,
            },
            3000,
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, BridgeAction::Log { .. })),
            "超限只记日志"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, BridgeAction::SendPrompt { .. }))
        );
        assert_eq!(rt.wake_times, [1000], "不记账");
    }

    /// 日记定点触发（红线六：定点 23 点路径）。
    #[test]
    fn diary_scheduled_path() {
        let mut rt = runtime();
        let rules = DecideRules::default();
        let diary = DiaryRules::default();
        let mut rng = || 0.99;
        let at_23 = crate::diary::CivilTime {
            date: crate::diary::CivilDate {
                year: 2026,
                month: 10,
                day: 21,
            },
            hour: 23,
            minute: 1,
        };
        let actions = decide(
            &mut rt,
            &BridgeEvent::TimerFired {
                key: "group:4242".to_owned(),
                timer: TimerKind::DiaryTick,
            },
            1_792_570_860_000,
            at_23,
            &rules,
            &diary,
            &mut rng,
        );
        assert!(actions.iter().any(|a| matches!(a, BridgeAction::SendPrompt { key, reason } if key == "diary" && reason.starts_with("diary:"))));
        assert_eq!(rt.last_diary_try_at, 1_792_570_860_000);
        // 冷却期内再触发 → Skip。
        let actions = decide(
            &mut rt,
            &BridgeEvent::TimerFired {
                key: "group:4242".to_owned(),
                timer: TimerKind::DiaryTick,
            },
            1_792_570_860_000 + 60_000,
            at_23,
            &rules,
            &diary,
            &mut rng,
        );
        assert!(actions.is_empty());
    }

    /// 自己的回显不触发（防自激励）。
    #[test]
    fn self_echo_does_not_trigger() {
        let mut rt = runtime();
        let mut m = msg("小鲸鱼");
        m.is_self = true;
        let actions = decide1(&mut rt, &BridgeEvent::MessageArrived(Box::new(m)), 1000);
        assert!(actions.is_empty());
        assert!(rt.pending_timer.is_none());
    }
}

#[cfg(test)]
mod history_trim_tests {
    use super::*;
    use crate::action::{BridgeAction, BridgeEvent, TimerKind};

    fn rt() -> SessionRuntime {
        SessionRuntime::new("group:1", TriggerRules::default())
    }
    fn decide1(rt: &mut SessionRuntime, event: &BridgeEvent, now_ms: u64) -> Vec<BridgeAction> {
        let rules = DecideRules::default();
        let diary = DiaryRules::default();
        let now_local = crate::diary::CivilTime {
            date: crate::diary::CivilDate {
                year: 2026,
                month: 10,
                day: 21,
            },
            hour: 12,
            minute: 0,
        };
        let mut rng = || 0.99;
        decide(rt, event, now_ms, now_local, &rules, &diary, &mut rng)
    }
    fn err_turn(text: &str) -> BridgeEvent {
        BridgeEvent::TurnEnded {
            key: "group:1".into(),
            session_id: "s-1".into(),
            ok: false,
            error_text: Some(text.into()),
        }
    }

    /// 413 → 退役 + 3s 重投定时器；重投 fire 绕频率直接 SendPrompt。
    #[test]
    fn history_limit_retires_and_reschedules() {
        let mut r = rt();
        // 先有在途回合（退配额路径成立）。
        let _ = decide1(&mut r, &err_turn("413 chat_history_too_large"), 10_000_000);
        let mut r2 = rt();
        r2.last_wake_at = Some(10_000_000);
        r2.wake_times = vec![10_000_000];
        let actions = decide1(&mut r2, &err_turn("413 chat_history_too_large"), 10_001_000);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, BridgeAction::RetireSession { key } if key == "group:1"))
        );
        let sched = actions.iter().find_map(|a| match a {
            BridgeAction::ScheduleTimer {
                timer: TimerKind::HistoryTrim,
                delay_ms,
                ..
            } => Some(*delay_ms),
            _ => None,
        });
        assert_eq!(sched, Some(3000), "重投延迟取 rules.history_trim_resend_ms");
        assert_eq!(r2.history_trim_at, 10_001_000, "冷却戳记录");

        // 冷却内第二次 413：退配额照常，但不再退役。
        let mut r3 = rt();
        r3.last_wake_at = Some(10_000_000);
        r3.history_trim_at = 10_001_000;
        let actions = decide1(&mut r3, &err_turn("413 chat_history_too_large"), 10_002_000);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, BridgeAction::RetireSession { .. })),
            "冷却内不重复退役"
        );

        // 冷却过后再 413：重新退役（前一步已 take 掉 last_wake_at，重设防早退守卫）。
        r3.last_wake_at = Some(10_003_000);
        let actions = decide1(&mut r3, &err_turn("413 chat_history_too_large"), 10_700_000);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, BridgeAction::RetireSession { .. }))
        );
    }

    /// 重投 fire：绕频率判定（窗口塞满也必须发——退配额已回滚，不能再被限流死锁）。
    #[test]
    fn history_trim_fires_despite_rate_limit() {
        let mut r = rt();
        // 塞满分钟窗（正常唤醒会被拒）。
        r.wake_times = vec![10_000_000, 10_000_001];
        let actions = decide1(
            &mut r,
            &BridgeEvent::TimerFired {
                key: "group:1".into(),
                timer: TimerKind::HistoryTrim,
            },
            2000,
        );
        assert!(actions.iter().any(
            |a| matches!(a, BridgeAction::SendPrompt { reason, .. } if reason == "history-trim")
        ));
    }

    /// 非 413 错误不触发退役。
    #[test]
    fn other_errors_do_not_retire() {
        let mut r = rt();
        r.last_wake_at = Some(10_000_000);
        let actions = decide1(&mut r, &err_turn("503 chat_admission_busy"), 10_001_000);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, BridgeAction::RetireSession { .. }))
        );
    }
}

#[cfg(test)]
mod agent_retry_tests {
    use super::*;
    use crate::action::{BridgeAction, TimerKind};

    fn rt() -> SessionRuntime {
        SessionRuntime::new("group:1", TriggerRules::default())
    }
    fn err_turn(text: &str) -> BridgeEvent {
        BridgeEvent::TurnEnded {
            key: "group:1".into(),
            session_id: "s-1".into(),
            ok: false,
            error_text: Some(text.into()),
        }
    }
    fn decide1(rt: &mut SessionRuntime, event: &BridgeEvent, now_ms: u64) -> Vec<BridgeAction> {
        let rules = DecideRules::default();
        let diary = DiaryRules::default();
        let now_local = crate::diary::CivilTime {
            date: crate::diary::CivilDate {
                year: 2026,
                month: 10,
                day: 21,
            },
            hour: 12,
            minute: 0,
        };
        let mut rng = || 0.99;
        decide(rt, event, now_ms, now_local, &rules, &diary, &mut rng)
    }

    /// 503 → 8s 重投定时器；第二次 20s；用尽后错误亮群友并清零。
    #[test]
    fn transient_error_backoff_ladder() {
        let mut r = rt();
        r.last_wake_at = Some(10_000_000);
        let a1 = decide1(&mut r, &err_turn("503 chat_admission_busy"), 10_001_000);
        let d1 = a1.iter().find_map(|a| match a {
            BridgeAction::ScheduleTimer {
                timer: TimerKind::AgentRetry,
                delay_ms,
                ..
            } => Some(*delay_ms),
            _ => None,
        });
        assert_eq!(d1, Some(8_000), "第 1 次退避 8s");
        assert_eq!(r.agent_retry_attempts, 1);

        r.last_wake_at = Some(10_010_000);
        let a2 = decide1(&mut r, &err_turn("503 service overloaded"), 10_020_000);
        let d2 = a2.iter().find_map(|a| match a {
            BridgeAction::ScheduleTimer {
                timer: TimerKind::AgentRetry,
                delay_ms,
                ..
            } => Some(*delay_ms),
            _ => None,
        });
        assert_eq!(d2, Some(20_000), "第 2 次退避 20s");
        assert_eq!(r.agent_retry_attempts, 2);

        r.last_wake_at = Some(10_040_000);
        let a3 = decide1(&mut r, &err_turn("503 again"), 10_050_000);
        assert!(
            !a3.iter()
                .any(|a| matches!(a, BridgeAction::ScheduleTimer { .. })),
            "用尽不再排重投"
        );
        let shout = a3.iter().find_map(|a| match a {
            BridgeAction::SendMessage { text, .. } => Some(text.clone()),
            _ => None,
        });
        let shout = shout.expect("用尽后错误亮给群友");
        assert!(shout.contains("已自动重试 2 次"), "{shout}");
        assert_eq!(r.agent_retry_attempts, 0, "亮错后清零");
    }

    /// 不可重试错误直接亮群友（无重试次数注记）；成功回合清零计数。
    #[test]
    fn non_retryable_shouts_and_success_resets() {
        let mut r = rt();
        r.last_wake_at = Some(10_000_000);
        let a = decide1(&mut r, &err_turn("some fatal error"), 10_001_000);
        let shout = a.iter().find_map(|x| match x {
            BridgeAction::SendMessage { text, .. } => Some(text.clone()),
            _ => None,
        });
        assert!(
            shout.unwrap().starts_with("⚠️ agent 处理出错"),
            "首错即亮无重试注记"
        );

        r.last_wake_at = Some(10_002_000);
        r.agent_retry_attempts = 1;
        decide1(
            &mut r,
            &BridgeEvent::TurnEnded {
                key: "group:1".into(),
                session_id: "s".into(),
                ok: true,
                error_text: None,
            },
            10_003_000,
        );
        assert_eq!(r.agent_retry_attempts, 0, "成功回合清零");
    }
}

#[cfg(test)]
mod proactive_tests {
    use super::*;
    use crate::action::{BridgeAction, BridgeEvent, TimerKind};

    fn fire(
        rt: &mut SessionRuntime,
        rules: &DecideRules,
        now_ms: u64,
        rng_val: f64,
    ) -> Vec<BridgeAction> {
        let now_local = crate::diary::CivilTime {
            date: crate::diary::CivilDate {
                year: 2026,
                month: 10,
                day: 21,
            },
            hour: 12,
            minute: 0,
        };
        let mut rng = move || rng_val;
        decide(
            rt,
            &BridgeEvent::TimerFired {
                key: "group:1".into(),
                timer: TimerKind::ProactiveCheck,
            },
            now_ms,
            now_local,
            rules,
            &DiaryRules::default(),
            &mut rng,
        )
    }

    fn has_wake(actions: &[BridgeAction]) -> bool {
        actions.iter().any(
            |a| matches!(a, BridgeAction::SendPrompt { reason, .. } if reason == "proactiveCheck"),
        )
    }
    fn has_reschedule(actions: &[BridgeAction]) -> bool {
        actions.iter().any(|a| {
            matches!(
                a,
                BridgeAction::ScheduleTimer {
                    timer: TimerKind::ProactiveCheck,
                    ..
                }
            )
        })
    }

    /// 空闲达阈值 + 随机命中 + 不繁忙 → 唤醒 + 自重排（检查环常驻）。
    #[test]
    fn fires_when_idle_and_lucky() {
        let mut r = SessionRuntime::new("group:1", TriggerRules::default());
        r.last_activity_at = 1_000_000;
        let rules = DecideRules::default();
        let actions = fire(&mut r, &rules, 1_000_000 + 20 * 60_000, 0.01);
        assert!(has_wake(&actions));
        assert!(has_reschedule(&actions));
    }

    /// 群活跃（空闲不足）/ 随机未命中 / 繁忙 → 不唤醒但照常重排。
    #[test]
    fn skips_but_reschedules() {
        let rules = DecideRules::default();
        // 空闲 5min < 阈值 15min。
        let mut r = SessionRuntime::new("group:1", TriggerRules::default());
        r.last_activity_at = 1_000_000;
        let actions = fire(&mut r, &rules, 1_000_000 + 5 * 60_000, 0.01);
        assert!(!has_wake(&actions));
        assert!(has_reschedule(&actions));
        // 随机未命中（rng 0.9 ≥ 0.3）。
        let mut r2 = SessionRuntime::new("group:1", TriggerRules::default());
        r2.last_activity_at = 1_000_000;
        let actions = fire(&mut r2, &rules, 1_000_000 + 20 * 60_000, 0.9);
        assert!(!has_wake(&actions));
        assert!(has_reschedule(&actions));
        // 繁忙（合批窗口在途）。
        let mut r3 = SessionRuntime::new("group:1", TriggerRules::default());
        r3.last_activity_at = 1_000_000;
        r3.pending_timer = Some(PendingTimer {
            reason: "keyword".into(),
        });
        let actions = fire(&mut r3, &rules, 1_000_000 + 20 * 60_000, 0.01);
        assert!(!has_wake(&actions));
        assert!(has_reschedule(&actions));
    }

    /// enabled=false → 熄环：不唤醒也不再重排（环自然终结）。
    #[test]
    fn disabled_stops_the_loop() {
        let mut r = SessionRuntime::new("group:1", TriggerRules::default());
        r.last_activity_at = 1_000_000;
        let rules = DecideRules {
            proactive: ProactiveRules {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let actions = fire(&mut r, &rules, 1_000_000 + 20 * 60_000, 0.01);
        assert!(actions.is_empty());
    }

    /// 间隔区间：min 下限 60s、max 不低于 min、端点取值正确。
    #[test]
    fn delay_bounds_and_floors() {
        let p = ProactiveRules {
            check_min_ms: 10_000,
            check_max_ms: 5 * 60_000,
            ..Default::default()
        };
        assert_eq!(
            proactive_check_delay(&p, &mut || 0.0),
            60_000,
            "min 下限 60s"
        );
        assert_eq!(
            proactive_check_delay(&p, &mut || 1.0),
            300_000,
            "max 不低于 min"
        );
        assert_eq!(
            proactive_check_delay(&p, &mut || 0.5),
            60_000 + 120_000,
            "区间中值"
        );
    }
}
