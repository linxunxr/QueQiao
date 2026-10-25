//! 编排器：事件归一、decide 驱动与定时器轮（bin 主循环的可测核心）。
//!
//! 分层：本模块是纯逻辑——`on_event` 喂事件返回动作序列、`due` 轮询到期定时器；
//! 真实 IO（WS/HTTP/异步定时器）在 main.rs 的薄层接线里。定时器用**代际**实现
//! 同 (key, kind) 重排覆盖：新排一次代数 +1，旧任务到期发现代数不匹配即丢弃——
//! 等价 Node 侧 `clearTimeout + setTimeout`，但无需持有真实定时器句柄。

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use crate::state::ConversationState;
use queqiao_core::action::{BridgeAction, BridgeEvent, TimerKind};
use queqiao_core::diary::CivilTime;
use queqiao_core::runtime::{
    DecideRules, DiaryRules, SessionRuntime, decide, proactive_check_delay,
};
use queqiao_core::triggers::TriggerRules;

/// 伪定时器轮：按到期时刻排序的最小堆 + 代际去废。
#[derive(Debug, Default)]
pub struct TimerWheel {
    gens: HashMap<(String, TimerKind), u64>,
    heap: BinaryHeap<Reverse<TimerEntry>>,
    next_gen: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TimerEntry {
    /// 到期时刻（先到期先出堆）。
    fire_at: u64,
    /// 代数（越大越新；取消/重排后旧代作废）。
    generation: u64,
    key: String,
    kind: TimerKind,
}

impl TimerWheel {
    /// 排一个定时器；同 (key, kind) 已有则覆盖（旧代作废）。
    pub fn schedule(&mut self, key: &str, kind: TimerKind, delay_ms: u64, now_ms: u64) {
        let id = (key.to_owned(), kind);
        let generation = self.next_gen;
        self.next_gen += 1;
        self.gens.insert(id, generation);
        self.heap.push(Reverse(TimerEntry {
            fire_at: now_ms.saturating_add(delay_ms),
            generation,
            key: key.to_owned(),
            kind,
        }));
    }

    /// 取消（作废对应代的定时器；堆里的陈旧项在 `due` 时惰性清理）。
    pub fn cancel(&mut self, key: &str, kind: TimerKind) {
        self.gens.remove(&(key.to_owned(), kind));
    }

    /// 取出所有到期且未作废的定时器事件（陈旧项顺带丢弃）。
    pub fn due(&mut self, now_ms: u64) -> Vec<BridgeEvent> {
        let mut out = Vec::new();
        while let Some(Reverse(top)) = self.heap.peek() {
            if top.fire_at > now_ms {
                break;
            }
            let entry = self.heap.pop().unwrap().0;
            let live = self
                .gens
                .get(&(entry.key.clone(), entry.kind))
                .is_some_and(|g| *g == entry.generation);
            if live {
                // 单发：触发后即作废（重排由下一次 schedule 显式做）。
                self.gens.remove(&(entry.key.clone(), entry.kind));
                out.push(BridgeEvent::TimerFired {
                    key: entry.key,
                    timer: entry.kind,
                });
            }
        }
        out
    }

    /// 在册定时器数（测试观测口）。
    pub fn active(&self) -> usize {
        self.gens.len()
    }
}

/// 编排器：会话运行时表 + decide 驱动。
///
/// `SessionRuntime` 与持久 `ConversationState` 的同步策略：decide 修改运行时里的
/// 持久镜像，`dirty` 标记的会话在 [`Orchestrator::sync_dirty`] 时写回
/// （bin 决定落盘时机；白名单式序列化见 state 模块）。
pub struct Orchestrator {
    sessions: HashMap<String, SessionRuntime>,
    timers: TimerWheel,
    rules: DecideRules,
    diary: DiaryRules,
    dirty: std::collections::HashSet<String>,
}

impl Orchestrator {
    pub fn new(rules: DecideRules, diary: DiaryRules) -> Self {
        Self {
            sessions: HashMap::new(),
            timers: TimerWheel::default(),
            rules,
            diary,
            dirty: std::collections::HashSet::new(),
        }
    }

    /// 注册/更新一个会话（启动时从持久状态恢复运行时镜像）。
    pub fn ensure_session(&mut self, key: &str, wake: TriggerRules) {
        self.sessions
            .entry(key.to_owned())
            .or_insert_with(|| SessionRuntime::new(key, wake));
    }

    /// 为全部在册会话排主动检查定时器（Node 模式进入时 scheduleProactiveCheckV2
    /// 全量排程语义；启动后由 ProactiveCheck 触发自重排维持检查环，无需再调）。
    /// enabled=false 不排；同 (key, ProactiveCheck) 覆盖重排。
    pub fn schedule_proactive_checks(&mut self, now_ms: u64, rng: &mut dyn FnMut() -> f64) {
        if !self.rules.proactive.enabled {
            return;
        }
        let keys: Vec<String> = self.sessions.keys().cloned().collect();
        for key in keys {
            self.schedule_proactive_check(&key, now_ms, rng);
        }
    }

    /// 单会话排一次主动检查（运行期动态注册的新会话用；覆盖重排同款）。
    pub fn schedule_proactive_check(
        &mut self,
        key: &str,
        now_ms: u64,
        rng: &mut dyn FnMut() -> f64,
    ) {
        if !self.rules.proactive.enabled {
            return;
        }
        let delay = proactive_check_delay(&self.rules.proactive, rng);
        self.timers
            .schedule(key, TimerKind::ProactiveCheck, delay, now_ms);
    }

    /// 喂一条事件，返回决策动作序列（并驱动内部定时器轮询）。
    pub fn on_event(
        &mut self,
        event: BridgeEvent,
        now_ms: u64,
        now_local: CivilTime,
        rng: &mut dyn FnMut() -> f64,
    ) -> Vec<BridgeAction> {
        // 事件路由到会话（心跳类无会话归属的直接吞掉）。
        let key = match &event {
            BridgeEvent::MessageArrived(m) => Some(m.key.clone()),
            BridgeEvent::PokeArrived { key, .. } => key.clone(),
            BridgeEvent::TimerFired { key, .. } => Some(key.clone()),
            BridgeEvent::TurnStarted { key, .. }
            | BridgeEvent::TurnBeat { key, .. }
            | BridgeEvent::TurnEnded { key, .. } => Some(key.clone()),
            BridgeEvent::UpstreamHeartbeat { .. } => None,
        };
        let Some(key) = key else {
            return Vec::new();
        };
        let Some(rt) = self.sessions.get_mut(&key) else {
            return Vec::new();
        };
        let actions = decide(rt, &event, now_ms, now_local, &self.rules, &self.diary, rng);
        // 动作里的定时器指令进轮；其余透传给调用方执行。
        let mut out = Vec::with_capacity(actions.len());
        for action in actions {
            match &action {
                BridgeAction::ScheduleTimer {
                    key,
                    timer,
                    delay_ms,
                } => {
                    self.timers.schedule(key, *timer, *delay_ms, now_ms);
                }
                BridgeAction::CancelTimer { key, timer } => {
                    self.timers.cancel(key, *timer);
                }
                _ => out.push(action),
            }
            // 有真实动作（非定时器指令）的会话视为脏，待同步。
            self.dirty.insert(key.clone());
        }
        out
    }

    /// 推进到 `now_ms`：触发所有到期定时器（每轮触发的定时器事件再进决策）。
    pub fn advance(
        &mut self,
        now_ms: u64,
        now_local: CivilTime,
        rng: &mut dyn FnMut() -> f64,
    ) -> Vec<BridgeAction> {
        let mut out = Vec::new();
        // 定时器触发可能再排新定时器（如退避重试），循环直到本轮无到期项。
        loop {
            let due = self.timers.due(now_ms);
            if due.is_empty() {
                break;
            }
            for event in due {
                out.extend(self.on_event(event, now_ms, now_local, rng));
            }
        }
        out
    }

    /// 自上一会话动作以来变脏的会话键（bin 据此回写持久层）。
    pub fn take_dirty(&mut self) -> Vec<String> {
        std::mem::take(&mut self.dirty).into_iter().collect()
    }

    /// 决策层的持久镜像快照（供回写 ConversationState / 控制台展示）。
    pub fn session(&self, key: &str) -> Option<&SessionRuntime> {
        self.sessions.get(key)
    }

    /// 会话键清单。
    pub fn session_keys(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }

    /// 从持久状态恢复运行时镜像（启动加载）。
    pub fn hydrate(&mut self, key: &str, persisted: &ConversationState, wake: TriggerRules) {
        let mut rt = SessionRuntime::new(key, wake);
        rt.wake_times = persisted.wake_times.clone();
        rt.last_activity_at = persisted.last_incoming_at;
        rt.last_action_at = persisted.last_action_at;
        rt.no_action_count = persisted.wake_config.no_action_count;
        rt.written_today = false; // 日记状态按日重算（bin 侧决定）
        rt.bootstrap_sent = persisted.bootstrap_sent;
        self.sessions.insert(key.to_owned(), rt);
    }

    pub fn timers(&self) -> &TimerWheel {
        &self.timers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use queqiao_core::action::IncomingMessage;

    fn msg(key: &str, text: &str, seq: u64) -> IncomingMessage {
        IncomingMessage {
            key: key.into(),
            seq,
            message_id: format!("m{seq}"),
            sender: "群友".into(),
            user_id: "10001".into(),
            text: text.into(),
            is_self: false,
            at_me: false,
            reply_to_me: false,
            time_ms: 0,
        }
    }

    fn now_local(hour: u8) -> CivilTime {
        CivilTime {
            date: queqiao_core::diary::CivilDate {
                year: 2026,
                month: 10,
                day: 21,
            },
            hour,
            minute: 0,
        }
    }

    /// 定时器轮：到期触发、重排覆盖旧代、取消作废。
    #[test]
    fn timer_wheel_gens_and_cancel() {
        let mut t = TimerWheel::default();
        t.schedule("k", TimerKind::BatchWindow, 1000, 0);
        assert!(t.due(999).is_empty());
        // 到点触发一次。
        let fired = t.due(1000);
        assert_eq!(fired.len(), 1);
        // 重排覆盖：旧代到期不再触发。
        t.schedule("k", TimerKind::TurnLease, 1000, 0);
        t.schedule("k", TimerKind::TurnLease, 5000, 0); // 覆盖
        assert!(t.due(1000).is_empty(), "旧代作废");
        assert_eq!(t.due(5000).len(), 1, "新代触发");
        // 取消。
        t.schedule("k", TimerKind::DiaryTick, 100, 0);
        t.cancel("k", TimerKind::DiaryTick);
        assert!(t.due(1000).is_empty());
        assert_eq!(t.active(), 0);
    }

    /// 端到端决策序列：消息 → 合批 → 推进到窗口 → 发送唤醒（动作序列可断言）。
    #[test]
    fn orchestrator_end_to_end_action_sequence() {
        let mut o = Orchestrator::new(DecideRules::default(), DiaryRules::default());
        o.ensure_session(
            "group:4242",
            TriggerRules {
                keywords: vec!["小鲸鱼".into()],
                ..Default::default()
            },
        );
        let mut rng = || 0.99;

        // 首条消息触发 bootstrap 唤醒（建立 agent 上下文）并完成回合。
        let _ = o.on_event(
            BridgeEvent::MessageArrived(Box::new(msg("group:4242", "第一句", 1))),
            1000,
            now_local(12),
            &mut rng,
        );
        let _ = o.advance(9000, now_local(12), &mut rng);
        let _ = o.on_event(
            BridgeEvent::TurnStarted {
                key: "group:4242".into(),
                session_id: "s-0".into(),
            },
            9500,
            now_local(12),
            &mut rng,
        );
        let _ = o.on_event(
            BridgeEvent::TurnEnded {
                key: "group:4242".into(),
                session_id: "s-0".into(),
                ok: true,
                error_text: None,
            },
            9600,
            now_local(12),
            &mut rng,
        );

        // 正式链：关键词消息 → 合批（动作只有日志，定时器进轮）。
        let actions = o.on_event(
            BridgeEvent::MessageArrived(Box::new(msg("group:4242", "小鲸鱼在吗", 2))),
            70_000,
            now_local(12),
            &mut rng,
        );
        assert!(matches!(&actions[..], [BridgeAction::Log { .. }]));
        assert_eq!(o.timers().active(), 1, "合批定时器在册");

        // 推进到窗口外：唤醒发送 + 租约/兜底两个新定时器。
        let actions = o.advance(78_000, now_local(12), &mut rng);
        assert!(actions.iter().any(
            |a| matches!(a, BridgeAction::SendPrompt { reason, .. } if reason == "keyword:小鲸鱼")
        ));
        assert_eq!(o.timers().active(), 2, "租约 + 兜底");
        assert!(o.session("group:4242").unwrap().prompt_in_flight);

        // 回程：turn 开始 → 结束（无动作）→ 租约/兜底定时器被取消。
        o.on_event(
            BridgeEvent::TurnStarted {
                key: "group:4242".into(),
                session_id: "s-1".into(),
            },
            10000,
            now_local(12),
            &mut rng,
        );
        let actions = o.on_event(
            BridgeEvent::TurnEnded {
                key: "group:4242".into(),
                session_id: "s-1".into(),
                ok: true,
                error_text: None,
            },
            15000,
            now_local(12),
            &mut rng,
        );
        assert!(
            actions
                .iter()
                .all(|a| matches!(a, BridgeAction::CancelTimer { .. }))
        );
        assert_eq!(o.timers().active(), 0, "回合结束清租约/兜底");
        assert!(
            o.take_dirty().contains(&"group:4242".to_owned()),
            "脏会话待持久同步"
        );
    }

    /// 未注册会话的事件被忽略（fail-safe，不 panic）。
    #[test]
    fn unknown_session_events_are_ignored() {
        let mut o = Orchestrator::new(DecideRules::default(), DiaryRules::default());
        let mut rng = || 0.99;
        let actions = o.on_event(
            BridgeEvent::MessageArrived(Box::new(msg("group:999", "小鲸鱼", 1))),
            1000,
            now_local(12),
            &mut rng,
        );
        assert!(actions.is_empty());
    }

    /// 主动检查环：全量排程 → 到期触发（不命中也自重排）→ 环常驻。
    #[test]
    fn proactive_check_loop_sustains() {
        let mut o = Orchestrator::new(DecideRules::default(), DiaryRules::default());
        o.ensure_session("group:4242", TriggerRules::default());
        let mut rng = || 0.99; // 概率永不命中，纯验环
        o.schedule_proactive_checks(0, &mut rng);
        assert_eq!(o.timers().active(), 1);
        // 到期：不唤醒（rng 0.99 ≥ 0.3）但重排，环仍在册。
        let actions = o.advance(90 * 60_000, now_local(12), &mut rng);
        assert!(actions.is_empty(), "未命中无真实动作");
        assert_eq!(o.timers().active(), 1, "触发后自重排，检查环常驻");
    }

    /// proactive 关闭时不排程。
    #[test]
    fn proactive_disabled_no_schedule() {
        let rules = DecideRules {
            proactive: queqiao_core::runtime::ProactiveRules {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut o = Orchestrator::new(rules, DiaryRules::default());
        o.ensure_session("group:4242", TriggerRules::default());
        let mut rng = || 0.99;
        o.schedule_proactive_checks(0, &mut rng);
        assert_eq!(o.timers().active(), 0);
    }

    /// 从持久状态恢复：wake_times/bootstrap 画像正确。
    #[test]
    fn hydrate_from_persisted() {
        let mut o = Orchestrator::new(DecideRules::default(), DiaryRules::default());
        let persisted = ConversationState {
            wake_times: vec![111, 222],
            bootstrap_sent: true,
            last_action_at: 333,
            wake_config: Default::default(),
            ..Default::default()
        };
        o.hydrate("group:1", &persisted, TriggerRules::default());
        let rt = o.session("group:1").unwrap();
        assert_eq!(rt.wake_times, [111, 222]);
        assert!(rt.bootstrap_sent);
        assert_eq!(rt.last_action_at, 333);
    }
}
