//! 自发消息撤回的资格判定（移植自 Node 版 `src/qq-recall.js`，契约测试同步迁移；
//! 规格外补充——MCP `qq_recall_message` 的端点校验核心）。
//!
//! QQ 服务端只允许撤回自己 2 分钟内的消息，桥端必须硬校验「消息在近期记录里 +
//! 确实是自己发的 + 未超窗」，不能信模型。

/// 撤回窗口：QQ 官方 2 分钟 + 10s 缓冲（时钟误差/网络耗时）。
pub const RECALL_WINDOW_MS: u64 = 130 * 1000;

/// 近期消息记录的最小判定形状（桥侧 recentMessages 的相关字段投影）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecallRecord<'a> {
    /// 桥侧记录的消息 id（字符串形式；无 id 的记录无法撤回）。
    pub message_id: Option<&'a str>,
    pub is_self: bool,
    pub recalled: bool,
    /// 消息时间戳（毫秒）；`None` 表示无时间戳，跳过窗口判定（与 Node 版
    /// `typeof m.time === 'number'` 守卫一致）。
    pub time: Option<i64>,
}

/// 撤回被拒的原因。`Display` 文案与 Node 版一致——error 面向模型，可直传。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecallError {
    InvalidId,
    AlreadyRecalled,
    NotSelf,
    Timeout { window_ms: u64 },
    NotFound,
}

impl std::fmt::Display for RecallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecallError::InvalidId => {
                write!(f, "messageId 必须是非零整数（消息 id 可能为负数）")
            }
            RecallError::AlreadyRecalled => write!(f, "这条消息已经撤回过了"),
            RecallError::NotSelf => write!(f, "只能撤回自己发送的消息"),
            RecallError::Timeout { window_ms } => write!(
                f,
                "撤回超时：QQ 只允许撤回 {} 秒内自己发的消息",
                ((*window_ms as f64) / 1000.0).round() as u64
            ),
            RecallError::NotFound => write!(
                f,
                "在近期消息记录里找不到这条自己发的消息（只能撤回本会话刚发出的消息）"
            ),
        }
    }
}

impl std::error::Error for RecallError {}

/// messageId 合法性：可选负号 + 非零开头的纯数字（等价 Node 版 `/^-?[1-9]\d*$/`）。
fn is_nonzero_integer(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    !body.is_empty() && !body.starts_with('0') && body.bytes().all(|b| b.is_ascii_digit())
}

/// 在近期消息记录里定位可撤回的自发消息（倒序取最新一条匹配）。
///
/// 返回命中记录的下标，或面向模型的 [`RecallError`]。
pub fn find_recallable_message(
    recent: &[RecallRecord<'_>],
    message_id: &str,
    now_ms: u64,
) -> Result<usize, RecallError> {
    let r = message_id.trim();
    if !is_nonzero_integer(r) {
        return Err(RecallError::InvalidId);
    }
    for (i, m) in recent.iter().enumerate().rev() {
        if m.message_id != Some(r) {
            continue;
        }
        if m.recalled {
            return Err(RecallError::AlreadyRecalled);
        }
        if !m.is_self {
            return Err(RecallError::NotSelf);
        }
        if let Some(t) = m.time
            && now_ms as i64 - t > RECALL_WINDOW_MS as i64
        {
            return Err(RecallError::Timeout {
                window_ms: RECALL_WINDOW_MS,
            });
        }
        return Ok(i);
    }
    Err(RecallError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;

    fn rec(
        id: Option<&'static str>,
        is_self: bool,
        ago_ms: i64,
        recalled: bool,
    ) -> RecallRecord<'static> {
        RecallRecord {
            message_id: id,
            is_self,
            recalled,
            time: Some(NOW as i64 - ago_ms),
        }
    }

    fn base() -> Vec<RecallRecord<'static>> {
        vec![
            rec(Some("100"), true, 10_000, false),
            rec(Some("101"), false, 10_000, false),
            rec(Some("102"), true, 10 * 60_000, false),
            rec(None, true, 5_000, false),
        ]
    }

    /// 契约与 Node 版 test-media-cache（qq-recall 段）一致：可撤回自己的新消息。
    #[test]
    fn recalls_own_fresh_message() {
        assert_eq!(find_recallable_message(&base(), "100", NOW), Ok(0));
    }

    /// 别人的消息拒绝。
    #[test]
    fn rejects_others_message() {
        let err = find_recallable_message(&base(), "101", NOW).unwrap_err();
        assert!(matches!(err, RecallError::NotSelf));
        assert!(err.to_string().contains("只能撤回自己"));
    }

    /// 超时拒绝（窗口语义：10 分钟前 ≫ 130 秒）。
    #[test]
    fn rejects_stale_message() {
        let err = find_recallable_message(&base(), "102", NOW).unwrap_err();
        assert!(matches!(err, RecallError::Timeout { .. }));
        assert!(err.to_string().contains("超时"));
    }

    /// 找不到拒绝。
    #[test]
    fn rejects_missing_message() {
        let err = find_recallable_message(&base(), "999", NOW).unwrap_err();
        assert!(matches!(err, RecallError::NotFound));
        assert!(err.to_string().contains("找不到"));
    }

    /// 非法 id 拒绝（非数字/零/小数）。
    #[test]
    fn rejects_invalid_id() {
        for bad in ["abc", "0", "-0", "1.5", "01", ""] {
            let err = find_recallable_message(&base(), bad, NOW).unwrap_err();
            assert!(matches!(err, RecallError::InvalidId), "id={bad:?} 应判非法");
        }
        assert!(RecallError::InvalidId.to_string().contains("非零整数"));
    }

    /// 负数 id 合法（QQ 消息 id 可能为负）。
    #[test]
    fn accepts_negative_id() {
        let recent = [rec(Some("-100"), true, 1_000, false)];
        assert_eq!(find_recallable_message(&recent, "-100", NOW), Ok(0));
    }

    /// 重复撤回拒绝。
    #[test]
    fn rejects_already_recalled() {
        let recent = [rec(Some("100"), true, 0, true)];
        let err = find_recallable_message(&recent, "100", NOW).unwrap_err();
        assert!(matches!(err, RecallError::AlreadyRecalled));
        assert!(err.to_string().contains("已经撤回"));
    }

    /// 窗口边界：恰好 windowMs - 1 内可撤。
    #[test]
    fn recalls_at_window_edge() {
        let recent = [rec(Some("200"), true, RECALL_WINDOW_MS as i64 - 1, false)];
        assert_eq!(find_recallable_message(&recent, "200", NOW), Ok(0));
    }
}
