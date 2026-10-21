//! 唤醒触发判定（从 Node 版 bridge.js `evaluateWakeTriggerV2` / `isDirectedAtAi`
//! 提取为纯函数；随机源注入——分层铁律）。
//!
//! 判定顺序即优先级（先命中先返回）：
//! private（私聊恒触发）→ anyMessage → atMention（@ 或引用自己）→ nameMention →
//! keyword（短英文/数字词用 ASCII 词边界，防 ADS 误中 DS）→ question
//! （针对 AI 的提问/挑战/催促）→ speaker（指定成员）→ probability（随机）。

use crate::action::IncomingMessage;

/// 触发规则（持久 wakeConfig.triggers 的决策视图，bin 侧转换）。
#[derive(Debug, Clone, PartialEq)]
pub struct TriggerRules {
    pub at_mention: bool,
    pub name_mention: bool,
    pub any_message: bool,
    pub question: bool,
    pub poke: bool,
    pub keywords: Vec<String>,
    pub speaker_ids: Vec<String>,
    /// 普通消息的随机触发概率（0 = 关）。
    pub probability: f64,
    /// bot 昵称（nameMention 用，空串关闭）。
    pub self_nickname: String,
}

impl Default for TriggerRules {
    fn default() -> Self {
        Self {
            at_mention: true,
            name_mention: true,
            any_message: false,
            question: true,
            poke: true,
            keywords: Vec::new(),
            speaker_ids: Vec::new(),
            probability: 0.0,
            self_nickname: String::new(),
        }
    }
}

/// ASCII 词边界包含（JS `\b` 语义：词字符 = [A-Za-z0-9_]）。
/// 短英文/数字关键词（如 DS/R1）用边界匹配避免 ADS/BDSM/DSL 误触发。
fn contains_word(haystack_lower: &str, needle_lower: &str) -> bool {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut start = 0;
    while let Some(pos) = haystack_lower[start..].find(needle_lower) {
        let abs = start + pos;
        let before_ok = haystack_lower[..abs]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word(c));
        let after_idx = abs + needle_lower.len();
        let after_ok = haystack_lower[after_idx..]
            .chars()
            .next()
            .is_none_or(|c| !is_word(c));
        if before_ok && after_ok {
            return true;
        }
        start = abs + needle_lower.len().max(1);
    }
    false
}

fn re_matches(pattern: &str, lower_text: &str) -> bool {
    use std::sync::LazyLock;
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<String, regex::Regex>> =
            std::cell::RefCell::new(std::collections::HashMap::new());
    }
    let _ = LazyLock::<regex::Regex>::new;
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        let re = cache
            .entry(pattern.to_owned())
            .or_insert_with(|| regex::Regex::new(pattern).expect("静态问句正则必然合法"));
        re.is_match(lower_text)
    })
}

/// 「针对 AI 的发言」判定（Node isDirectedAtAi 全量正则转译；输入须已小写）。
pub fn is_directed_at_ai(lower_text: &str) -> bool {
    let ai_mention = r"deepseek|claude|chatgpt|gpt|大肥鱼|小鲸鱼|鲸鱼|d指导|d老师|d师傅|深度求索|\bds\b|ai|人工智障|机器人|模型";
    let challenge = r"强|弱|行不行|能不能|会不会|是不是|一半|水平|垃圾|废物|白嫖|菜|不如|厉害|赢|输|比.*强|比.*弱";
    let question = r"[?？吗呢吧]|怎么|为什么|哪|谁";
    if re_matches(ai_mention, lower_text)
        && (re_matches(question, lower_text) || re_matches(challenge, lower_text))
    {
        return true;
    }
    // 「你/您 + 疑问/比较/挑战」（放宽，避免漏掉「你有claude一半强吗」这类直接问）。
    if re_matches(
        r"(你|您).{0,10}(吗|呢|？|\?|怎么|是不是|能不能|行不行|有没有|有|没有|比|不如|强|弱|一半|厉害|垃圾|菜|赢|输)",
        lower_text,
    ) {
        return true;
    }
    if re_matches(
        r"^(你|您)(是不是|行不行|能不能|会不会|觉得|有|没有)",
        lower_text,
    ) {
        return true;
    }
    // 「你是…还是… / 你是…吗 / 你是…？」（如：你是人类还是ai、你是真人吗）。
    if re_matches(
        r"(你|您)是[^？?。！!]{0,14}(还是|或者|吗|么|？|\?)",
        lower_text,
    ) {
        return true;
    }
    // 追问/催促：AI 没回时的真人式催促。
    re_matches(
        r"怎么不说话|人呢|回我|说话啊|理我|别装死|在不在|装死|说话",
        lower_text,
    )
}

/// 判定一条消息触发哪个唤醒原因；无触发返回 `None`。
/// `rng` 提供 `[0,1)` 随机数（probability 档用）。
pub fn evaluate_trigger(
    rules: &TriggerRules,
    msg: &IncomingMessage,
    rng: impl FnOnce() -> f64,
) -> Option<String> {
    if msg.key.starts_with("private:") {
        return Some("private".to_owned());
    }
    if rules.any_message {
        return Some("anyMessage".to_owned());
    }
    if rules.at_mention && (msg.at_me || msg.reply_to_me) {
        return Some("atMention".to_owned());
    }
    if rules.name_mention && !rules.self_nickname.is_empty() {
        let lower = msg.text.to_lowercase();
        let nick = rules.self_nickname.to_lowercase();
        if lower.contains(&nick) {
            return Some("nameMention".to_owned());
        }
    }
    if !rules.keywords.is_empty() {
        let lower = msg.text.to_lowercase();
        for kw in &rules.keywords {
            let kw_lower = kw.to_lowercase();
            if kw_lower.is_empty() {
                continue;
            }
            let hit = if kw_lower.len() <= 4 && kw_lower.bytes().all(|b| b.is_ascii_alphanumeric())
            {
                contains_word(&lower, &kw_lower)
            } else {
                lower.contains(&kw_lower)
            };
            if hit {
                return Some(format!("keyword:{kw}"));
            }
        }
    }
    if rules.question && is_directed_at_ai(&msg.text.to_lowercase()) {
        return Some("question".to_owned());
    }
    if !rules.speaker_ids.is_empty() && rules.speaker_ids.contains(&msg.user_id) {
        return Some(format!("speaker:{}", msg.sender));
    }
    if rules.probability > 0.0 && rng() < rules.probability {
        return Some("probability".to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str, key: &str) -> IncomingMessage {
        IncomingMessage {
            key: key.into(),
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

    fn rules() -> TriggerRules {
        TriggerRules {
            keywords: vec!["小鲸鱼".into(), "DS".into(), "R1".into()],
            speaker_ids: vec!["10086".into()],
            self_nickname: "DeepSeek".into(),
            ..Default::default()
        }
    }

    /// 契约与 Node evaluateWakeTriggerV2 一致：判定顺序即优先级。
    #[test]
    fn trigger_order_matches_node() {
        // 私聊恒触发。
        assert_eq!(
            evaluate_trigger(&rules(), &msg("随便", "private:10001"), || 1.0),
            Some("private".into())
        );
        // @ 自己 > 昵称 > 关键词。
        let mut m = msg("小鲸鱼你好", "group:1");
        m.at_me = true;
        assert_eq!(
            evaluate_trigger(&rules(), &m, || 1.0),
            Some("atMention".into())
        );
        assert_eq!(
            evaluate_trigger(&rules(), &msg("DeepSeek 在吗", "group:1"), || 1.0),
            Some("nameMention".into())
        );
        // 短英文词边界：ADS 不触发 DS。
        assert_eq!(
            evaluate_trigger(&rules(), &msg("这个 ADS 广告", "group:1"), || 1.0),
            None
        );
        assert_eq!(
            evaluate_trigger(&rules(), &msg("新的 DS 模型", "group:1"), || 1.0),
            Some("keyword:DS".into())
        );
        // 中文关键词子串。
        assert_eq!(
            evaluate_trigger(&rules(), &msg("小鲸鱼出来", "group:1"), || 1.0),
            Some("keyword:小鲸鱼".into())
        );
    }

    /// question：针对 AI 的提问/挑战/催促（Node isDirectedAtAi 全量语义）。
    #[test]
    fn question_detection() {
        let plain = TriggerRules {
            keywords: vec![],
            ..rules()
        };
        for text in [
            "deepseek 你觉得呢",
            "你有claude一半强吗",
            "你是不是机器人",
            "你是人类还是ai",
            "怎么不说话",
        ] {
            assert!(
                is_directed_at_ai(&text.to_lowercase()),
                "应判定针对 AI：{text}"
            );
        }
        for text in ["今天天气不错", "ads 是什么", "普通聊天"] {
            assert!(!is_directed_at_ai(&text.to_lowercase()), "不应误判：{text}");
        }
        assert_eq!(
            evaluate_trigger(&plain, &msg("你有claude一半强吗", "group:1"), || 1.0),
            Some("question".into())
        );
    }

    /// speaker 与 probability：指定成员直通、随机档注入。
    #[test]
    fn speaker_and_probability() {
        let r = rules();
        let mut m = msg("普通消息", "group:1");
        m.user_id = "10086".to_owned();
        assert_eq!(
            evaluate_trigger(&r, &m, || 1.0),
            Some("speaker:群友".into()),
            "指定成员任何消息触发"
        );
        assert_eq!(
            evaluate_trigger(&r, &msg("普通", "group:1"), || 0.99),
            None,
            "随机未命中"
        );
        let mut p = r.clone();
        p.probability = 0.05;
        assert_eq!(
            evaluate_trigger(&p, &msg("普通", "group:1"), || 0.01),
            Some("probability".into())
        );
    }
}
