//! agent 回合瞬时错误的自动重投判定（移植自 Node 版 `src/agent-retry.js`，契约测试同步迁移）。
//!
//! 背景：omniroute 路由上游（v4-pro 等重模型）在 DeepSeek 服务端会撞准入容量限制
//!（503 chat_admission_busy，"Retry shortly"）——这类错误上游什么都没执行、秒级
//! 自愈，把错误直接甩到群里体验很差；退避几秒重投一次通常就过。
//! 注意**不重试**超时类错误：超时时回合可能已在 DSH 侧部分执行，重投有重复动作风险。

use std::sync::LazyLock;
use std::time::Duration;

use regex::RegexSet;

/// 重投上限：最多 2 次（第 1 次退避 8s、第 2 次 20s）。
pub const AGENT_RETRY_MAX: u32 = 2;

/// 可重试的瞬时错误模式（与 Node 版逐条对应；`\b` 对 ASCII 输入语义一致）。
static RETRYABLE: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"(?i)chat_admission_busy",
        r"(?i)temporarily unavailable",
        r"(?i)\bcapacity\b",
        r"(?i)\b50[23]\b",
        r"(?i)overloaded",
        r"(?i)rate[ _-]?limit",
    ])
    .expect("静态正则必然合法")
});

/// 会话历史超限模式（413 类，走退役重建路径而非重试路径）。
static HISTORY_LIMIT: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([r"(?i)chat_history_too_large", r"(?i)message_limit"]).expect("静态正则必然合法")
});

/// 容量/限流/502/503 类瞬时错误判定——上游什么都没执行，退避重投通常就过。
pub fn is_retryable_agent_error(message: &str) -> bool {
    RETRYABLE.is_match(message)
}

/// 退避时长；`attempt` 为已重投次数。返回 [`None`] 表示不可重试或重试用尽。
/// 第 1 次 8s、第 2 次 20s（Node 版以 0 毫秒表示不可重试，Rust 版用 Option 显式区分）。
pub fn retry_backoff(message: &str, attempt: u32) -> Option<Duration> {
    if !is_retryable_agent_error(message) {
        return None;
    }
    if attempt >= AGENT_RETRY_MAX {
        return None;
    }
    Some(if attempt == 0 {
        Duration::from_secs(8)
    } else {
        Duration::from_secs(20)
    })
}

/// 会话历史超上游硬限（omniroute 上游 800 条消息，413 chat_history_too_large）。
/// 这类错误重试无效——历史不会自己变短；正确处理是退役 DSH 会话重建（桥侧记忆
/// 保留、新会话空历史重新 bootstrap），再重投该回合。
pub fn is_history_limit_error(message: &str) -> bool {
    HISTORY_LIMIT.is_match(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约与 Node 版 test-agent-retry 一致：实战 503 报文（chat_admission_busy +
    /// temporarily unavailable 双命中）可重试。
    #[test]
    fn admission_busy_is_retryable() {
        assert!(is_retryable_agent_error(
            "503: {\"message\":\"Chat admission capacity is temporarily unavailable. Retry shortly.\",\"type\":\"server_error\",\"code\":\"chat_admission_busy\"}"
        ));
    }

    /// capacity 独立成词可重试。
    #[test]
    fn capacity_is_retryable() {
        assert!(is_retryable_agent_error(
            "gateway/capacity-exceeded: no room"
        ));
    }

    /// 502/503 独立成数可重试。
    #[test]
    fn gateway_5xx_is_retryable() {
        assert!(is_retryable_agent_error("HTTP 503 bad gateway"));
        assert!(is_retryable_agent_error("HTTP 502"));
    }

    /// provider overloaded 可重试。
    #[test]
    fn overloaded_is_retryable() {
        assert!(is_retryable_agent_error("provider overloaded"));
    }

    /// rate limit 三种分隔写法（下划线/空格/连字符）均可重试。
    #[test]
    fn rate_limit_is_retryable() {
        assert!(is_retryable_agent_error("rate_limit: too many requests"));
        assert!(is_retryable_agent_error("rate limit: too many requests"));
        assert!(is_retryable_agent_error("rate-limit: too many requests"));
    }

    /// 参数错误不可重试（重投同样会失败）。
    #[test]
    fn invalid_arguments_not_retryable() {
        assert!(!is_retryable_agent_error(
            "arguments-invalid: unexpected \"sessionId\""
        ));
    }

    /// 超时不可重试（可能已部分执行）——迁移红线之一。
    #[test]
    fn timeout_not_retryable() {
        assert!(!is_retryable_agent_error("request timed out after 60000ms"));
    }

    /// 未知错误与空消息不可重试。
    #[test]
    fn unknown_and_empty_not_retryable() {
        assert!(!is_retryable_agent_error("unknown failure"));
        assert!(!is_retryable_agent_error(""));
    }

    /// 退避阶梯：首次 8s、第二次 20s。
    #[test]
    fn backoff_ladder() {
        assert_eq!(
            retry_backoff("503 chat_admission_busy", 0),
            Some(Duration::from_secs(8))
        );
        assert_eq!(
            retry_backoff("503 chat_admission_busy", 1),
            Some(Duration::from_secs(20))
        );
    }

    /// 重试用尽（attempt 达上限）与超上限均返回不可重试。
    /// （Node 版的负数/小数 attempt 防御由 u32 类型在编译期排除。）
    #[test]
    fn backoff_exhaustion() {
        assert_eq!(
            retry_backoff("503 chat_admission_busy", AGENT_RETRY_MAX),
            None
        );
        assert_eq!(retry_backoff("503", 5), None);
    }

    /// 不可重试错误无论第几次都无退避。
    #[test]
    fn backoff_requires_retryable_error() {
        assert_eq!(retry_backoff("arguments-invalid", 0), None);
    }

    /// 实战 413 报文判定为历史超限。
    #[test]
    fn live_413_is_history_limit() {
        let live = "413: {\"message\":\"Chat history exceeds the 800-message limit; compact the conversation and retry.\",\"type\":\"payload_too_large\",\"code\":\"chat_history_too_large\",\"reason\":\"message_limit\"}";
        assert!(is_history_limit_error(live));
    }

    /// message_limit 独立命中也判定为超限。
    #[test]
    fn message_limit_is_history_limit() {
        assert!(is_history_limit_error("some reason: message_limit"));
    }

    /// 两条路径互斥：超限错误不进重试路径。
    #[test]
    fn history_limit_not_retryable() {
        let live = "413 chat_history_too_large";
        assert!(!is_retryable_agent_error(live));
    }

    /// 容量错误不进超限路径。
    #[test]
    fn capacity_not_history_limit() {
        assert!(!is_history_limit_error("503 chat_admission_busy"));
    }
}
