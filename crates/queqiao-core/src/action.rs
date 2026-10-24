//! 决策接口形态：入站事件与出站动作（规格 §3.1 决策范式
//! `fn decide(state, event, now_ms) -> Vec<Action>` 的类型面）。
//!
//! 这是 core 与 bin 编排层的**接口契约**：bin 把 OneBot/DSH/定时器事件归一为
//! [`BridgeEvent`] 喂给决策层，决策层返回 [`BridgeAction`] 由 bin 解释执行
//! （定时器/网络/文件 IO 全在 bin——分层铁律）。运行时会话状态
//! （busy 四条件、回合收集、租约心跳）由决策层自持，形状随后续编排批次长全。

/// 一条进入决策层的消息（bin 从 OneBotEvent 构造；core 不依赖协议层类型）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingMessage {
    /// 会话键：`group:<id>` / `private:<id>`。
    pub key: String,
    /// 会话内单调序号（水位推进与暂存去重用）。
    pub seq: u64,
    pub message_id: String,
    /// 显示名（群名片/昵称）。
    pub sender: String,
    pub user_id: String,
    pub text: String,
    /// 自己发出的回显（message_sent）。
    pub is_self: bool,
    /// @ 了本 bot。
    pub at_me: bool,
    /// 引用了本 bot 的消息。
    pub reply_to_me: bool,
    pub time_ms: u64,
}

/// 定时器种类（bin 侧的定时器句柄按 (key, kind) 唯一）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TimerKind {
    /// 合批窗口到期：以合并后的最终原因发送唤醒。
    BatchWindow,
    /// 回合租约到期：回合连续静默超限，强制解除 busy（迁移红线之二）。
    TurnLease,
    /// 暂存兜底到期：busy 已解除则补发最早一条暂存唤醒（35min，> 租约）。
    PendingWakeFallback,
    /// 有限潜水到期（sleepUntil）。
    SleepUntil,
    /// 日记轮询（60s 一拍，三路径判定见 [`crate::diary`]）。
    DiaryTick,
    /// 主动检查（30~90min 随机间隔）。
    ProactiveCheck,
    /// 历史超限退役后的重投（默认 3s；到点绕频率判定直接重发唤醒——
    /// 失败回合已退配额，重投不该再吃频率窗）。
    HistoryTrim,
}

/// 入站事件（bin 归一后的决策输入）。
#[derive(Debug, Clone, PartialEq)]
pub enum BridgeEvent {
    /// 群/私聊消息（含自发回显，`is_self` 区分）。
    MessageArrived(Box<IncomingMessage>),
    /// 拍一拍（`key` 为 None 表示好友拍一拍之外的无会话上下文形态）。
    PokeArrived {
        key: Option<String>,
        user_id: u64,
        target_is_self: bool,
    },
    /// 定时器到期（bin 侧定时器触发）。
    TimerFired { key: String, timer: TimerKind },
    /// DSH 回合开始（mux turn/start——回合租约的心跳源之一）。
    TurnStarted { key: String, session_id: String },
    /// DSH 回合内任意帧（tool/call、assistant/message…——刷新租约心跳；
    /// 静默超过租约时长由 TurnLease 定时器强制解除）。
    TurnBeat { key: String, session_id: String },
    /// DSH 回合结束（ok=false 表示 error 回合——触发退配额）。
    TurnEnded {
        key: String,
        session_id: String,
        ok: bool,
        /// 失败回合的错误文本（reason 原文；413 判定用）。
        error_text: Option<String>,
    },
    /// SnowLuma 心跳（`status.good` 权威健康信号；None 为旧运行时缺字段）。
    UpstreamHeartbeat { good: Option<bool> },
}

/// 决策层输出的动作（bin 解释执行）。
#[derive(Debug, Clone, PartialEq)]
pub enum BridgeAction {
    /// 向 DSH 投递唤醒 prompt（reason 用于日志与审计）。
    SendPrompt { key: String, reason: String },
    /// 经 OneBot 发消息（reply_to 为引用目标）。
    SendMessage {
        key: String,
        text: String,
        reply_to: Option<i64>,
    },
    /// 排定时器（同 (key, kind) 重排覆盖）。
    ScheduleTimer {
        key: String,
        timer: TimerKind,
        delay_ms: u64,
    },
    /// 取定时器。
    CancelTimer { key: String, timer: TimerKind },
    /// 失败回合退配额：回滚指定时刻记的唤醒（迁移红线之三）。
    RefundWakeTime { key: String, at: u64 },
    /// 退役 DSH 会话映射（413 历史超限 / not-found：桥侧记忆保留，
    /// 下次投递按 fail-closed 重建新会话）。
    RetireSession { key: String },
    /// 状态落盘（节流由 bin 决定）。
    SaveState,
    /// 运行日志（桥日志与对拍轨迹都吃这个）。
    Log { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 接口形态冒烟：事件与动作可构造/可比较（对拍轨迹将按动作序列比对）。
    #[test]
    fn event_action_shapes() {
        let event = BridgeEvent::MessageArrived(Box::new(IncomingMessage {
            key: "group:4242".into(),
            seq: 7,
            message_id: "-123".into(),
            sender: "群友".into(),
            user_id: "10001".into(),
            text: "在吗".into(),
            is_self: false,
            at_me: false,
            reply_to_me: false,
            time_ms: 1_792_570_000_000,
        }));
        let BridgeEvent::MessageArrived(msg) = &event else {
            unreachable!()
        };
        assert_eq!(msg.key, "group:4242");

        let actions = [
            BridgeAction::ScheduleTimer {
                key: msg.key.clone(),
                timer: TimerKind::BatchWindow,
                delay_ms: 8000,
            },
            BridgeAction::Log {
                message: "计划唤醒".into(),
            },
        ];
        // 动作序列可比较（对拍维度——规格 §八.3）。
        assert_eq!(actions.len(), 2);
        assert_ne!(actions[0], actions[1]);
        assert_eq!(
            actions[0],
            BridgeAction::ScheduleTimer {
                key: "group:4242".into(),
                timer: TimerKind::BatchWindow,
                delay_ms: 8000
            }
        );
    }
}
