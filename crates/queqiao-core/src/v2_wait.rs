//! 二代仿真模式：等待/结束回合相关的轻量判断工具（移植自 Node 版 `src/v2-wait.js`，
//! 契约测试同步迁移；供 qq_wait_for_messages 的沉睡前观察语义使用）。
//!
//! 这些是「提示/软信号」，不是硬规则；最终是否等待、是否发催话仍由 AI 自主决定。

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

/// 常见「话说一半/没说完」的结尾特征。
static UNFINISHED_TAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:你知道|等一下|我跟你讲|其实吧|但是|所以说|然后|那个|就是|我想说|对了|等我|等等|我看看|还有|再说|主要是|毕竟|因为|所以|但是吧|回头|回头说|待会|晚点|等会|再说吧)$")
        .unwrap()
});

/// 以中文逗号/顿号/分号/冒号等「非终止标点」结尾，也常表示还没说完。
static UNFINISHED_PUNCT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[，、；：,;:]$").unwrap());

/// 明确以终止标点/省略号结尾的消息视为说完。
static FINISHED_TAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[。！？!?…～~]+$").unwrap());

/// 判定一条消息是否「看起来没说完」（判定顺序：终止标点优先 → 口头禅结尾 →
/// 非终止标点结尾；空消息不算未说完）。
pub fn looks_like_unfinished(text: &str) -> bool {
    let s = text.trim();
    if s.is_empty() {
        return false;
    }
    if FINISHED_TAIL.is_match(s) {
        return false;
    }
    UNFINISHED_TAIL.is_match(s) || UNFINISHED_PUNCT.is_match(s)
}

/// AI 等不到下文时可以用的「催话」短句。
pub const UNFINISHED_PROMPT_REPLIES: &[&str] = &["什么", "啥", "你说啊", "然后呢", "？"];

/// 建议的「结束前再等一轮」的等待时长下限。
pub const END_ROUND_WAIT_MIN: Duration = Duration::from_secs(5 * 60);
/// 建议的「结束前再等一轮」的等待时长上限。
pub const END_ROUND_WAIT_MAX: Duration = Duration::from_secs(10 * 60);

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约与 Node 版 test-v2-wait 一致：常见「话说一半」的结尾判 true。
    #[test]
    fn detects_unfinished_tails() {
        for text in [
            "你知道",
            "等一下",
            "我跟你讲",
            "但是",
            "所以说",
            "然后",
            "那个",
            "就是",
            "我想说",
            "对了",
            "等我",
            "等等",
            "我看看",
            "还有",
            "再说",
            "主要是",
            "回头说",
            "待会",
            "再说吧",
            "你听我说，",
        ] {
            assert!(looks_like_unfinished(text), "应判未说完: {text:?}");
        }
    }

    /// 已说完/普通消息不误判。
    #[test]
    fn does_not_flag_finished_messages() {
        for text in [
            "",
            "好的",
            "今天好累",
            "B站搜猫踩奶视频 解压一绝",
            "那去看跳伞第一视角视频",
            "你再说一遍试试",
            "？",
            "哦牛批",
            "这么刺激",
            "你说完了。",
        ] {
            assert!(!looks_like_unfinished(text), "应判已说完: {text:?}");
        }
    }

    /// 催话短句与等待常量。
    #[test]
    fn provides_prompt_replies_and_wait_bounds() {
        assert!(!UNFINISHED_PROMPT_REPLIES.is_empty());
        assert!(END_ROUND_WAIT_MIN >= Duration::from_secs(5 * 60));
        assert!(END_ROUND_WAIT_MAX >= END_ROUND_WAIT_MIN);
    }

    // Node 版测试第 3 条（读 config.json 校验 wait.maxMs >= 600000）是部署配置
    // 检查而非纯函数契约，归 Rust 版 bin 层配置加载校验，不在 core 单测范围。
}
