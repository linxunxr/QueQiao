//! 唤醒 prompt 文本构造（Node 版 buildWakePromptV2 的 Rust 对应物）。
//!
//! 结构 = 基础块（角色/会话令牌/反AI味/积极性/首回复节奏/此刻状态/当前唤醒/
//! 沉睡前观察）+ reason 专属尾段（逐字转译）。记忆块/表情包摘要/黑话前缀的
//! 注入在投递层组装（见 bin main 的 wake prompt 组装段），本文件只管骨架。
//! 投递层 withSlangContext，后续批次）——省略项不改变已有行的语义。

/// 构造上下文（bin 从持久态与配置组装）。
#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    pub key: String,
    /// 当前角色名（current-role.json；空省略该行——完整角色卡由 qq_get_prompt 工具查）。
    pub role_name: String,
    /// 会话令牌（工具调用必须携带；bootstrap 时生成）。
    pub agent_token: String,
    pub unread_count: usize,
    /// 最近一条非自发消息（发送者, 文本前 30 字）。
    pub last_message: Option<(String, String)>,
    /// 最近一条消息疑似没说完（v2_wait 判定）。
    pub last_unfinished: bool,
    /// 上次 AI 发言距现在的分钟数。
    pub last_ai_reply_min_ago: Option<u64>,
    /// 唤醒配置摘要（如「潜水，无限；触发：@/名字/关键词」）。
    pub wake_summary: String,
    pub pre_sleep_ms: u64,
    /// 首回复短静默参数。
    pub reply_quiet_ms: u64,
    /// 距离短静默满足还差的毫秒数。
    pub reply_remaining_ms: u64,
}

/// 构造唤醒 prompt（reason 支撑全部 Node 版专属尾段）。
pub fn build_wake_prompt(ctx: &PromptContext, reason: &str) -> String {
    let mut base = String::new();
    if !ctx.role_name.is_empty() {
        base.push_str(&format!(
            "【当前角色】{}（完整角色卡请调用 qq_get_prompt 查看）\n\n",
            ctx.role_name
        ));
    }
    base.push_str(&format!(
        "【会话令牌】{}（调用二代状态工具时请在参数中带上此令牌）\n\n",
        ctx.agent_token
    ));
    base.push_str(
        "【反AI味】你不是客服：不用有求必应，可以装傻、已读乱回、拒绝推荐；少用「一绝/真管用/保证/绝对」这类词。\n\n",
    );
    base.push_str(
        "【积极性】不要习惯性潜水：群里有你能接的话题就主动参与，偶尔插一句别人的话题也很正常；只有确实没话可说、对方已明确结束、或长时间没人说话时才潜水。\n\n",
    );
    base.push_str(&format!(
        "【首回复节奏】先处理已有消息，完整的问题及时回答。需要确认对方是否继续补充时，用 qq_wait_for_messages(purpose=\"reply\", quietMs={})；从最后来信计时，合批和读提示词的耗时也算，目前还差约 {} 秒静默。已安静够久可直接回，工具也会立即返回并补上思考期间的新消息。只有明确没说完、你确实要等下文时，才用普通 messages 长等待。\n\n",
        ctx.reply_quiet_ms,
        ctx.reply_remaining_ms / 1000
    ));
    let mut status = vec![format!("未读 {} 条", ctx.unread_count)];
    if let Some((sender, text)) = &ctx.last_message {
        status.push(format!("最近一条来自 {sender}：{text}"));
        if ctx.last_unfinished {
            status.push("对方可能没说完".to_owned());
        }
    }
    if let Some(min) = ctx.last_ai_reply_min_ago {
        status.push(format!("你上次发言 {min} 分钟前"));
    }
    base.push_str(&format!("【此刻状态】{}\n\n", status.join("；")));
    base.push_str(&format!("【当前唤醒】{}\n\n", ctx.wake_summary));
    base.push_str(&format!(
        "【沉睡前强制等待】这是决定潜水收尾时的步骤，不是首回复的前置条件。除非对方明确说「不聊了/晚安/下了/拜拜」等结束语，否则每次设置潜水/下一次唤醒前，必须先调用 qq_wait_for_messages(timeoutMs={}) 完成一次沉睡前观察；回复前 purpose=\"reply\" 的短静默不能代替这次完整观察。若 {} 分钟内没人说话，返回 preSleepWaitSatisfied=true，可以设置下一次唤醒并沉睡；若期间有人发新消息，先查看返回的 newMessages——判断不需要你参与就可以直接沉睡，若你选择参与回复，下次想睡时需要重新等待观察窗口。如果返回里带 preSleepWaitRemainingMs，就按剩余时间继续等待。\n\n",
        ctx.pre_sleep_ms,
        ctx.pre_sleep_ms / 60000
    ));

    match reason {
        "bootstrap" => format!(
            "{base}【引导唤醒】你已接入 QQ 会话 {key}。\n当前是二代仿真模式：你的文本输出不会自动发送到 QQ，所有发言必须通过工具完成。\n请先调用 qq_get_prompt 查看角色、推荐值、可用工具和状态；有待回应的消息先按首回复节奏处理，最后才用 qq_set_wake_config 设置你希望如何被唤醒。",
            key = ctx.key
        ),
        "timeout" => format!(
            "{base}【唤醒】{key}\n原因：你设置的有限潜水时间已到；在你规定的时间内没有任何一项条件被触发，只是因为时间到了所以你被唤醒。\n你可以查看消息，或继续设置新的唤醒条件。",
            key = ctx.key
        ),
        "replyCheck" => format!(
            "{base}【回复检查】{key}\n原因：你刚刚发送过消息，现在回来检查是否有人回复。\n已有回复时先看消息，需要短静默就用 qq_wait_for_messages(purpose=\"reply\") 判断对方是否说完；如果没人回你，不用硬补一句，但也不要立刻潜水——先调用 qq_wait_for_messages(timeoutMs={pre}) 完成沉睡前观察：没人说话可收尾；有人说话则查看 newMessages，不需要你参与也可直接收尾（qq_mark_read 或 qq_set_wake_config）。",
            key = ctx.key,
            pre = ctx.pre_sleep_ms
        ),
        "proactiveCheck" => format!(
            "{base}【主动机会】{key}\n原因：群里已经安静了一段时间，这是一次你可以主动冒泡的机会。\n优先主动开个话题、追问上次没聊完的事、分享一个刚想到的想法；如果一时想不到，可以用 mcp__web-search-safe__web_search 搜一下当前热点/时事/网络热梗，再结合记忆里的群友兴趣挑一个自然角度。只要内容自然，就大胆开口；如果实在没话想说，再安静收尾（qq_mark_read 或 qq_set_wake_config）。",
            key = ctx.key
        ),
        "poke" => format!(
            "{base}【拍一拍】{key}\n有人在群里拍了拍你（或你拍的人引发关注）。查看最近消息判断语境：如果是叫你，自然回应；如果是别人的互动，看情况要不要凑热闹。用 qq_set_wake_config 维持你的唤醒条件。",
            key = ctx.key
        ),
        other => {
            // keyword:xxx / atMention / private / question / speaker / anyMessage /
            // probability / diary 等：通用唤醒尾段。
            if other.starts_with("diary") {
                // 素材与指引在投递层拼接的【日记素材】段（Node diary prompt 的
                // digests/anchors/补写文案——diary_sched::build_diary_material）。
                format!(
                    "{base}【日记时间】按消息末尾【日记素材】段的动静与指引写日记并提交（qq_write_diary；写入日期由桥锁定，不用你指定）。写完照常设置唤醒条件。"
                )
            } else {
                format!(
                    "{base}【唤醒】{key}\n触发原因：{other}。\n请查看未读消息（qq_get_unread_messages），按你的人格与当前语境决定是否以及如何回应；处理完用 qq_set_wake_config 设置下次唤醒，需要沉睡时先完成沉睡前观察。",
                    key = ctx.key,
                    other = other
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PromptContext {
        PromptContext {
            key: "group:4242".into(),
            role_name: "小鲸鱼".into(),
            agent_token: "tok-123".into(),
            unread_count: 3,
            last_message: Some(("群友".into(), "小鲸鱼在吗".into())),
            last_unfinished: true,
            last_ai_reply_min_ago: Some(12),
            wake_summary: "潜水，无限；触发：@/名字/关键词".into(),
            pre_sleep_ms: 300_000,
            reply_quiet_ms: 8_000,
            reply_remaining_ms: 6_000,
        }
    }

    /// 基础块与尾段的关键锚点逐项在（语义转译校验，不做全文比对）。
    #[test]
    fn prompt_contains_key_anchors() {
        let text = build_wake_prompt(&ctx(), "keyword:小鲸鱼");
        for anchor in [
            "【当前角色】小鲸鱼",
            "【会话令牌】tok-123",
            "【反AI味】",
            "【首回复节奏】",
            "【此刻状态】未读 3 条；最近一条来自 群友：小鲸鱼在吗；对方可能没说完；你上次发言 12 分钟前",
            "【当前唤醒】潜水，无限；触发：@/名字/关键词",
            "【沉睡前强制等待】",
            "触发原因：keyword:小鲸鱼",
        ] {
            assert!(text.contains(anchor), "缺锚点：{anchor}");
        }
    }

    /// reason 专属尾段分支。
    #[test]
    fn reason_specific_tails() {
        assert!(
            build_wake_prompt(&ctx(), "bootstrap")
                .contains("【引导唤醒】你已接入 QQ 会话 group:4242")
        );
        assert!(build_wake_prompt(&ctx(), "bootstrap").contains("文本输出不会自动发送到 QQ"));
        assert!(build_wake_prompt(&ctx(), "timeout").contains("有限潜水时间已到"));
        assert!(build_wake_prompt(&ctx(), "replyCheck").contains("【回复检查】"));
        assert!(build_wake_prompt(&ctx(), "proactiveCheck").contains("【主动机会】"));
        assert!(build_wake_prompt(&ctx(), "poke").contains("【拍一拍】"));
        assert!(build_wake_prompt(&ctx(), "diary:scheduled").contains("【日记时间】"));
        // 角色名为空省略角色行。
        let mut bare = ctx();
        bare.role_name = String::new();
        assert!(!build_wake_prompt(&bare, "poke").contains("【当前角色】"));
    }
}
