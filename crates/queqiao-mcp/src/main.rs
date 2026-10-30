//! queqiao-mcp —— MCP 工具面（独立 stdio 进程，DSH spawn；规格 §3.6）。
//!
//! 三层权限体系的第 1 层在本进程：**注册期过滤**——config.socialV2.tools 为
//! false 的工具不进 tools/list（描述与 schema 都不进模型上下文，省 token 且
//! 防误调）。调用期鉴权（agent token）与端点级校验（工具开关/白名单/业务）
//! 在桥端（tools_api）单点收口。
//!
//! 协议：MCP stdio（换行分隔 JSON-RPC 2.0）。工具回调 = 对桥 HTTP
//! （默认 127.0.0.1:3100 /api/socialV2/*）的薄转发——本进程崩溃不拖垮主桥。

use std::io::{BufRead, Write};

use serde_json::{Value, json};

/// 工具定义（name → 桥端点 + 参数转发 + 注册开关键）。
/// 浏览器门禁（与 TabletGate 同构，独立枚举避免语义混用）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum BrowserGate {
    #[default]
    None,
    Always,
    OnEnabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum TabletGate {
    #[default]
    None,
    Always,
    OnEnabled,
}

struct ToolDef {
    name: &'static str,
    description: &'static str,
    /// 桥端点（相对 /api/socialV2/；tablet 类为绝对路径）。
    endpoint: &'static str,
    method: &'static str,
    /// config.socialV2.tools 的开关键名（None = 无开关恒注册）。
    flag: Option<&'static str>,
    /// 平板门禁：Always=恒注册（仅 tablet_status 探测用）；OnEnabled=仅
    /// tablet.enabled=true 时注册（八个控制类工具——关着不占 system
    /// prompt、不给模型误调机会，Node 注册期过滤同义）。
    tablet_gate: TabletGate,
    /// 浏览器门禁（同构）：Always=仅 browser_status；OnEnabled=仅
    /// browser.enabled=true 时注册。
    browser_gate: BrowserGate,
    /// 参数 schema（属性名 + 类型 + 是否必填 + 描述）。
    params: &'static [(&'static str, &'static str, bool, &'static str)],
}

/// 每个工具的公共参数：conversationKey + agentToken（唤醒 prompt 注入）。
const KEY_PARAM: (&str, &str, bool, &str) = (
    "conversationKey",
    "string",
    true,
    "会话键（group:群号 或 private:QQ号，即唤醒 prompt 里的会话标识）",
);
const TOKEN_PARAM: (&str, &str, bool, &str) = (
    "agentToken",
    "string",
    true,
    "会话令牌（唤醒 prompt 里【会话令牌】给出）",
);

const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "qq_get_prompt",
        description: "获取角色卡、推荐参数、可用工具与当前状态（引导时先调这个）",
        endpoint: "prompt",
        method: "GET",
        flag: Some("getPrompt"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[],
    },
    ToolDef {
        name: "qq_get_unread_messages",
        description: "获取未读消息列表（可选 afterSeq 游标增量读取）",
        endpoint: "unread",
        method: "GET",
        flag: Some("getUnread"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[(
            "afterSeq",
            "number",
            false,
            "只返回 seq 大于此值的消息（增量读取游标）",
        )],
    },
    ToolDef {
        name: "qq_send_message",
        description: "向当前会话发送文本消息（群或私聊按 conversationKey 自动路由）",
        endpoint: "send-message",
        method: "POST",
        flag: Some("sendMessage"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("text", "string", true, "消息文本"),
            ("replyToMessageId", "number", false, "引用回复的消息 id"),
        ],
    },
    ToolDef {
        name: "qq_memory_append",
        description: "记一条轻量记忆：进行中的话题(activeTopic)/想说未说(pendingThought)/对群友的印象(memberImpression 需 target)",
        endpoint: "memory-append",
        method: "POST",
        flag: Some("memory"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            (
                "category",
                "string",
                true,
                "activeTopic / pendingThought / memberImpression",
            ),
            ("content", "string", true, "记忆内容（200 字内）"),
            (
                "target",
                "string",
                false,
                "memberImpression 必填：群友名或 QQ",
            ),
            (
                "participants",
                "array",
                false,
                "话题参与者名（activeTopic）",
            ),
            (
                "pendingQuestion",
                "string",
                false,
                "话题待追问的问题（activeTopic）",
            ),
            (
                "motivation",
                "string",
                false,
                "想法的动机注记（pendingThought）",
            ),
            (
                "expiresAtMs",
                "number",
                false,
                "想法有效期毫秒（pendingThought，默认 2 小时）",
            ),
        ],
    },
    ToolDef {
        name: "qq_memory_query",
        description: "查看记忆详情（全量）：进行中的话题/想说未说/对群友的印象。唤醒提示里只有摘要与印象索引，查某类全文时传 category（印象必传）",
        endpoint: "memory",
        method: "GET",
        flag: Some("memory"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[(
            "category",
            "string",
            false,
            "activeTopic / pendingThought / memberImpression（缺省全部）",
        )],
    },
    ToolDef {
        name: "qq_memory_remove",
        description: "删除一条记忆（category + content 精确匹配）",
        endpoint: "memory-remove",
        method: "POST",
        flag: Some("memory"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("category", "string", true, "activeTopic / pendingThought"),
            ("content", "string", true, "要删除的记忆原文"),
        ],
    },
    ToolDef {
        name: "qq_get_recent_messages",
        description: "获取最近消息（默认最新 20 条，可 onlySelf 只看自己发的）",
        endpoint: "recent-messages",
        method: "GET",
        flag: Some("getRecent"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("limit", "number", false, "条数（默认 20，上限 100）"),
            ("onlySelf", "boolean", false, "只看自己发的消息（传 true）"),
        ],
    },
    ToolDef {
        name: "qq_get_active_members",
        description: "会话活跃成员（近期谁在说话、说多少条、管理员标注）",
        endpoint: "active-members",
        method: "GET",
        flag: Some("getActiveMembers"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[("limit", "number", false, "人数上限（默认 10，最大 20）")],
    },
    ToolDef {
        name: "qq_slang_query",
        description: "查群聊黑话表（梗/网络用语的含义与用法，按出现次数排序）",
        endpoint: "slang/query",
        method: "GET",
        flag: Some("slangQuery"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[("query", "string", false, "关键词（匹配词条/含义/用法）")],
    },
    ToolDef {
        name: "qq_slang_submit",
        description: "提交/更新一条黑话释义（同词条已存在则补充释义；新词条为 candidate 待确认）",
        endpoint: "slang/submit",
        method: "POST",
        flag: Some("slangSubmit"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("content", "string", true, "黑话词条本身（50 字内）"),
            ("meaning", "string", true, "含义（300 字内）"),
            ("usage", "string", false, "用法说明（可选）"),
            ("example", "string", false, "使用示例（可选）"),
        ],
    },
    ToolDef {
        name: "qq_list_stickers",
        description: "表情库查询（query 匹配备注/标签/用途/id/md5；返回 id+备注供选择）",
        endpoint: "sticker-list",
        method: "GET",
        flag: Some("listStickers"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            (
                "query",
                "string",
                false,
                "关键词（匹配 desc/localNote/usage/tags）",
            ),
            ("limit", "number", false, "条数（默认 48，上限 500）"),
        ],
    },
    ToolDef {
        name: "qq_get_sticker_image",
        description: "查看指定收藏表情的图片本体（视觉确认含义后再决定发不发）。需带会话令牌。",
        endpoint: "sticker-image",
        method: "GET",
        flag: Some("getStickerImage"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[("stickerId", "string", true, "表情 id/md5/URL 任一")],
    },
    ToolDef {
        name: "qq_send_sticker",
        description: "发一张收藏表情（一条消息只一张、不混文字；先 qq_list_stickers 选 id）",
        endpoint: "send-sticker",
        method: "POST",
        flag: Some("sendSticker"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("stickerId", "string", true, "表情 id/md5/URL 任一"),
            (
                "replyToMessageId",
                "number",
                false,
                "引用回复的消息 id（可选）",
            ),
        ],
    },
    ToolDef {
        name: "qq_sticker_note",
        description: "记下对一个表情的理解（note/tags/usage——下次选择更准）",
        endpoint: "sticker-note",
        method: "POST",
        flag: Some("stickerNote"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("stickerId", "string", true, "表情 id/md5/URL 任一"),
            ("note", "string", false, "你的理解（如：被戳中时用）"),
            ("tags", "json", false, "标签数组（如 [\"开心\", \"接梗\"]）"),
        ],
    },
    ToolDef {
        name: "qq_get_message_detail",
        description: "按 messageId 查看单条消息详情（含媒体信息）",
        endpoint: "message-detail",
        method: "GET",
        flag: Some("getMessageDetail"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[(
            "messageId",
            "string",
            true,
            "消息 id（近期消息列表里的 messageId）",
        )],
    },
    ToolDef {
        name: "qq_mark_read",
        description: "确认已处理到 throughSeq（水位推进；新消息留待后续处理）",
        endpoint: "mark-read",
        method: "POST",
        flag: Some("markRead"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[(
            "throughSeq",
            "number",
            true,
            "已处理到的本地 seq（qq_get_unread_messages 返回的 seq，非 QQ messageId）",
        )],
    },
    ToolDef {
        name: "qq_set_wake_config",
        description: "设置下次唤醒条件（模式/有限时长/触发器；沉睡前先完成观察窗口）",
        endpoint: "set-wake-config",
        method: "POST",
        flag: Some("setWakeConfig"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("mode", "string", false, "diving（潜水）或 active（活跃）"),
            ("infinite", "boolean", false, "true=无限期潜水等触发"),
            (
                "sleepUntil",
                "number",
                false,
                "有限潜水的到期时刻（毫秒时间戳）；null 清除",
            ),
            (
                "triggers",
                "object",
                false,
                "触发器：atMention/nameMention/question/poke/anyMessage(布尔) 与 keywords(字符串数组)",
            ),
        ],
    },
    ToolDef {
        name: "qq_status",
        description: "会话状态摘要（未读数/水位/唤醒配置）",
        endpoint: "status",
        method: "GET",
        flag: Some("getUnread"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[],
    },
    ToolDef {
        name: "qq_reply",
        description: "在群里引用/回复某条消息并发文本（回更早的消息/多句指向不同人时用；上下文唯一时不必引用）。replyToMessageId 从近期消息工具获取，可为负数。需带会话令牌。",
        endpoint: "reply",
        method: "POST",
        flag: Some("reply"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("groupId", "string", true, "群号（须在白名单）"),
            (
                "replyToMessageId",
                "number",
                true,
                "被引用消息 id（非零整数，可为负）",
            ),
            (
                "message",
                "string",
                true,
                "文本（纯文本，勿 Markdown/CQ 码）",
            ),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_send_burst",
        description: "在群里分多条连发消息（真人化随机间隔，每条一个气泡）。数组每元素=一条消息；字符串内别用空格拆句。暂不支持引用（用 qq_reply）。需带会话令牌。",
        endpoint: "send-message",
        method: "POST",
        flag: Some("sendBurst"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("groupId", "string", true, "群号（须在白名单）"),
            (
                "messages",
                "array",
                true,
                "消息数组（每条纯文本；兼容 JSON 数组字符串）",
            ),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_send_poke",
        description: "拍一拍（群聊须传 targetUserId；私聊默认拍对方）。代替废话式回应/提醒/回拍，别频繁。需带会话令牌。",
        endpoint: "send-poke",
        method: "POST",
        flag: Some("sendPoke"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            (
                "conversationKey",
                "string",
                true,
                "会话 key（group:群号 / private:QQ号）",
            ),
            (
                "targetUserId",
                "string",
                false,
                "要拍的群友 QQ 号（群聊必填）",
            ),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_react_message",
        description: "给近期消息点 QQ 表情回应（轻度围观：好笑/赞同点一个，别每条都点）。emojiId 常用：76 笑哭、4 得意、5 流泪、54 喝彩、66 爱心。messageId 用真实 QQ 消息 id。需带会话令牌。",
        endpoint: "react-message",
        method: "POST",
        flag: Some("reactMessage"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            (
                "messageId",
                "number",
                true,
                "要点回应的消息 id（QQ messageId 非 seq）",
            ),
            ("emojiId", "string", true, "QQ 表情编号（数字字符串）"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_recall_message",
        description: "撤回自己刚发的消息（QQ 仅允许 2 分钟内撤自己的；发错/说漏嘴时用，撤后可补发或说「打错了」）。messageId 用发送返回/近期消息里的真实 id。需带会话令牌。",
        endpoint: "recall-message",
        method: "POST",
        flag: Some("recallMessage"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            ("messageId", "number", true, "要撤回的消息 id"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_get_message_images",
        description: "获取消息中的图片/表情并直接返回图像内容（看到 [图片]/[表情] 时调用，别假装看不到；视频暂不支持）。需带会话令牌。",
        endpoint: "images/message",
        method: "GET",
        flag: Some("getImages"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            (
                "messageId",
                "string",
                true,
                "消息 id（QQ messageId 可为负）",
            ),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_get_forward_msg",
        description: "查看合并转发消息内容（文本树；嵌套转发给出 id 可再查）。文本含 [转发消息 id=...] 时调用。需带会话令牌。",
        endpoint: "forward-msg",
        method: "GET",
        flag: Some("getForwardMsg"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            (
                "id",
                "string",
                true,
                "转发的 resid（消息里 [转发消息 id=...] 的 id）",
            ),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_collect_sticker",
        description: "收藏别人消息里有趣的图片/表情入库（先看图确认，别频繁收）。messageId 用近期消息的真实 id。需带会话令牌。",
        endpoint: "collect-sticker",
        method: "POST",
        flag: Some("collectSticker"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            ("messageId", "number", true, "带图消息的 id"),
            ("note", "string", false, "简短备注（含义/标签/用法）"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_fetch_sticker",
        description: "从图片 URL 入库（兜底手段——优先用现有收藏和偷群友的图；先用搜索拿直链）。需带会话令牌。",
        endpoint: "fetch-sticker",
        method: "POST",
        flag: Some("fetchSticker"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            ("url", "string", true, "图片直链（http/https）"),
            ("desc", "string", false, "备注"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_set_sticker_remark",
        description: "修改表情库内某表情的备注（localNote；QQ 官方收藏备注默认禁用）。需带会话令牌。",
        endpoint: "set-sticker-remark",
        method: "POST",
        flag: Some("setStickerRemark"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            (
                "stickerId",
                "string",
                true,
                "表情 id（qq_list_stickers 的 id）",
            ),
            ("localNote", "string", true, "新备注"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_write_diary",
        description: "提交今天的日记（第一人称私房话，200-500 字，每天仅一篇；写给自己的，不发给群友）。深夜【日记时间】唤醒时提交。需带会话令牌。",
        endpoint: "write-diary",
        method: "POST",
        flag: Some("writeDiary"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            ("content", "string", true, "日记内容"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_get_diary",
        description: "翻看最近几天的日记（默认 7 天；管理员问\"看看你的日记\"时也用）。需带会话令牌。",
        endpoint: "get-diary",
        method: "GET",
        flag: Some("getDiary"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            ("days", "number", false, "天数（默认 7，上限 90）"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_get_group_members",
        description: "群成员列表（QQ 号/昵称/群名片）。需要 userId 时从这里或活跃成员工具获取，别凭记忆猜。需带会话令牌。",
        endpoint: "group-members",
        method: "GET",
        flag: Some("groupMembers"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key（group:群号）"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_get_group_profile",
        description: "群档案（群名称/人数/公告）——记不清群是干嘛的、公告有没有更新时用。低频工具。需带会话令牌。",
        endpoint: "group-profile",
        method: "GET",
        flag: Some("groupProfile"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key（group:群号）"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_get_group_history",
        description: "群最近 20 条消息（补历史上下文用；唤醒快照通常已够，别重复拉）。需带会话令牌。",
        endpoint: "group-history",
        method: "GET",
        flag: Some("groupHistory"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key（group:群号）"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_get_self_image",
        description: "看自己的 Q 版形象（头像图直供视觉）。需带会话令牌。",
        endpoint: "self-image",
        method: "GET",
        flag: Some("getSelfImage"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_wait_for_messages",
        description: "等待新消息/静默。purpose=reply：回复前短静默（对方安静够即返）；默认 messages：等新消息或超时（沉睡前观察用 timeoutMs=300000）。返回 newMessages 直接用，无需再查。需带会话令牌。",
        endpoint: "wait-messages",
        method: "GET",
        flag: Some("waitMessages"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            (
                "purpose",
                "string",
                false,
                "messages（默认，等消息/观察）或 reply（回复前短静默）",
            ),
            (
                "timeoutMs",
                "number",
                false,
                "等待预算毫秒（默认 30000，上限 600000）",
            ),
            (
                "quietMs",
                "number",
                false,
                "静默门槛毫秒（默认 10000，仅 reply 用）",
            ),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_social_state",
        description: "查看会话的二代仿真状态：唤醒配置、未读数、上次唤醒/发言时间、频率用量、记忆条数（只读）。需带会话令牌。",
        endpoint: "social-state",
        method: "GET",
        flag: Some("socialState"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_memory_clear",
        description: "清空本会话的全部轻量记忆（话题/想说未说/成员印象）。时过境迁或记忆过时时用。需带会话令牌。",
        endpoint: "memory-clear",
        method: "POST",
        flag: Some("memory"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_get_my_recent_messages",
        description: "看自己最近发过的消息（回顾自己说过什么/纠正重复）。需带会话令牌。",
        endpoint: "my-recent",
        method: "GET",
        flag: Some("getMyRecent"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            ("limit", "number", false, "条数（默认 10，上限 50）"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_list_groups",
        description: "列出我所在的群（群号/名称/人数）。需带会话令牌。",
        endpoint: "list-groups",
        method: "GET",
        flag: Some("socialState"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key（鉴权用）"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_report_feedback",
        description: "把用户的反馈/抱怨/建议原样上报管理员（别自行总结转述）。需带会话令牌。",
        endpoint: "report-feedback",
        method: "POST",
        flag: Some("feedback"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            ("text", "string", true, "反馈原文"),
            (
                "level",
                "string",
                false,
                "反馈级别 info/warning/error，默认 info",
            ),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_speak",
        description: "把一段文字直接合成语音（系统 TTS）作为 QQ 语音发出——不依赖语音库。短句更像真人（1~2 句），标点影响停顿，rate 微调语速（-10~10）。语音默认关闭：报「语音功能已关闭」是正常状态，不要重试、不要假装发了。返回 verified=true 才代表落地。需带会话令牌。",
        endpoint: "speak-voice",
        method: "POST",
        flag: Some("sendVoice"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            ("text", "string", true, "要合成的文本"),
            ("rate", "number", false, "语速（-10~10，默认 0）"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_send_voice",
        description: "发语音库（qq_list_voices）里准备好的音频。语音默认关闭：报「语音功能已关闭」是正常状态。返回 verified=true 才代表落地。需带会话令牌。",
        endpoint: "send-voice",
        method: "POST",
        flag: Some("sendVoice"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            ("voiceName", "string", true, "语音名（qq_list_voices 返回）"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_list_voices",
        description: "语音库清单（可用的音频名）。需带会话令牌。",
        endpoint: "list-voices",
        method: "GET",
        flag: Some("sendVoice"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("conversationKey", "string", true, "会话 key（鉴权用）"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_send_group_message",
        description: "向指定群发一条文本（与 qq_send_message 同链；带 replyToMessageId 可引用）。优先用 qq_send_message。需带会话令牌。",
        endpoint: "send-message",
        method: "POST",
        flag: Some("sendGroup"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("groupId", "string", true, "群号（须在白名单）"),
            ("message", "string", true, "文本（纯文本）"),
            ("replyToMessageId", "number", false, "引用消息 id（可选）"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "qq_send_private_message",
        description: "向指定好友发一条私聊文本（与 qq_send_message 同链；带 replyToMessageId 可引用）。优先用 qq_send_message。需带会话令牌。",
        endpoint: "send-message",
        method: "POST",
        flag: Some("sendPrivate"),
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::None,
        params: &[
            ("userId", "string", true, "好友 QQ 号（须在白名单）"),
            ("message", "string", true, "文本（纯文本）"),
            ("replyToMessageId", "number", false, "引用消息 id（可选）"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "tablet_status",
        description: "探测鸿蒙平板是否可用（只读）：功能开关、目标设备在线状态、设备型号。不需要会话令牌。其余 tablet_* 工具需要会话令牌，且仅管理员授权的会话（默认管理员私聊）可通过。",
        endpoint: "/api/tablet/status",
        method: "GET",
        flag: None,
        tablet_gate: TabletGate::Always,
        browser_gate: BrowserGate::None,
        params: &[],
    },
    ToolDef {
        name: "tablet_screenshot",
        description: "截取鸿蒙平板当前屏幕，图像直接进入上下文（配合 tablet_read_screen 可同时拿到文字）。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/tablet/screenshot",
        method: "GET",
        flag: None,
        tablet_gate: TabletGate::OnEnabled,
        browser_gate: BrowserGate::None,
        params: &[("agentToken", "string", true, "会话令牌")],
    },
    ToolDef {
        name: "tablet_read_screen",
        description: "读取鸿蒙平板屏幕上当前可见的界面结构：每行一个元素的坐标/文字/是否可点可滚，适合获取屏幕上的文字信息（视频标题、评论、设置项等）。返回较大时会被截断。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/tablet/read-screen",
        method: "GET",
        flag: None,
        tablet_gate: TabletGate::OnEnabled,
        browser_gate: BrowserGate::None,
        params: &[
            (
                "maxChars",
                "number",
                false,
                "返回文本上限（字符），默认 18000",
            ),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "tablet_tap",
        description: "点击鸿蒙平板屏幕上的坐标。坐标可从 tablet_read_screen 的 [x1,y1,x2,y2] 或 tablet_screenshot 中取（点元素中心）。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/tablet/tap",
        method: "POST",
        flag: None,
        tablet_gate: TabletGate::OnEnabled,
        browser_gate: BrowserGate::None,
        params: &[
            ("x", "number", true, "横坐标（像素）"),
            ("y", "number", true, "纵坐标（像素）"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "tablet_swipe",
        description: "在鸿蒙平板上从 (x1,y1) 滑动到 (x2,y2)（慢速精确滑动，如拖动进度条、移动图标）。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/tablet/swipe",
        method: "POST",
        flag: None,
        tablet_gate: TabletGate::OnEnabled,
        browser_gate: BrowserGate::None,
        params: &[
            ("x1", "number", true, "起点横坐标"),
            ("y1", "number", true, "起点纵坐标"),
            ("x2", "number", true, "终点横坐标"),
            ("y2", "number", true, "终点纵坐标"),
            (
                "speedPxPerS",
                "number",
                false,
                "滑动速度（像素/秒，200~40000，默认 600）",
            ),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "tablet_fling",
        description: "在鸿蒙平板上按方向快滑（系统默认速度，适合翻页/刷视频/滚列表）：up=手指向上滑（看下一条），down=向下滑（回看上一条），left/right=横向翻页。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/tablet/fling",
        method: "POST",
        flag: None,
        tablet_gate: TabletGate::OnEnabled,
        browser_gate: BrowserGate::None,
        params: &[
            (
                "direction",
                "string",
                true,
                "手指滑动方向：up/down/left/right",
            ),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "tablet_key",
        description: "按鸿蒙平板的按键：Back（返回）、Home（回桌面）、Power（电源）或数字键码。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/tablet/key",
        method: "POST",
        flag: None,
        tablet_gate: TabletGate::OnEnabled,
        browser_gate: BrowserGate::None,
        params: &[
            ("key", "string", true, "Back / Home / Power 或数字键码"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "tablet_text",
        description: "向鸿蒙平板输入文本（最多 500 字符）。给 x/y 时先点击该坐标处的输入框再输入；不给则输入到当前已聚焦的输入框。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/tablet/text",
        method: "POST",
        flag: None,
        tablet_gate: TabletGate::OnEnabled,
        browser_gate: BrowserGate::None,
        params: &[
            ("text", "string", true, "要输入的文本"),
            (
                "x",
                "number",
                false,
                "输入框横坐标（可选，不给则输入到已聚焦输入框）",
            ),
            ("y", "number", false, "输入框纵坐标（与 x 一起给）"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "tablet_open_url",
        description: "在鸿蒙平板的默认浏览器里打开一个 http(s) 网页（能用平板上已登录的账号看到渲染后的页面）。看内容用 tablet_screenshot 截图 / tablet_read_screen 读文字，tablet_fling 滚动页面，tablet_key Back 返回。仅允许 http/https。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/tablet/open-url",
        method: "POST",
        flag: None,
        tablet_gate: TabletGate::OnEnabled,
        browser_gate: BrowserGate::None,
        params: &[
            ("url", "string", true, "要打开的 http(s) 网址"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "browser_status",
        description: "探测专属浏览器状态（只读）：功能开关、运行态、当前页面。不需要会话令牌。其余 browser_* 工具需要会话令牌，且仅管理员授权的会话（默认管理员私聊）可通过。",
        endpoint: "/api/browser/status",
        method: "GET",
        flag: None,
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::Always,
        params: &[],
    },
    ToolDef {
        name: "browser_navigate",
        description: "在专属浏览器中打开网页（http/https/file），等到加载完成。已登录站点的 cookie 保存在专属 profile 里。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/browser/navigate",
        method: "POST",
        flag: None,
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::OnEnabled,
        params: &[
            ("url", "string", true, "要打开的网址"),
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "browser_snapshot",
        description: "读取专属浏览器当前页面的可见元素树：每行 [x1,y1,x2,y2]坐标 + 文字/输入框值/是否可点。坐标可直接喂给 browser_click。返回较大时截断。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/browser/snapshot",
        method: "GET",
        flag: None,
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::OnEnabled,
        params: &[
            (
                "maxChars",
                "number",
                false,
                "返回文本上限（字符），默认 18000",
            ),
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "browser_screenshot",
        description: "截取专属浏览器当前页面（JPEG 图像直接进上下文）。canvas/视频等快照读不到的画面用这个看。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/browser/screenshot",
        method: "GET",
        flag: None,
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::OnEnabled,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "browser_click",
        description: "点击专属浏览器视口坐标（坐标来自 browser_snapshot 的 [x1,y1,x2,y2] 取中心）。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/browser/click",
        method: "POST",
        flag: None,
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::OnEnabled,
        params: &[
            ("x", "number", true, "横坐标（像素）"),
            ("y", "number", true, "纵坐标（像素）"),
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "browser_type",
        description: "向专属浏览器输入文本（最多 2000 字符）。给 x/y 时先点击该坐标处的输入框再输入；不给则输入到已聚焦输入框。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/browser/type",
        method: "POST",
        flag: None,
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::OnEnabled,
        params: &[
            ("text", "string", true, "要输入的文本"),
            ("x", "number", false, "输入框横坐标（可选）"),
            ("y", "number", false, "输入框纵坐标（与 x 一起给）"),
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "browser_scroll",
        description: "滚动专属浏览器页面：up=向下看更多内容，down=向上回滚，left/right=横向。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/browser/scroll",
        method: "POST",
        flag: None,
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::OnEnabled,
        params: &[
            ("direction", "string", true, "滚动方向：up/down/left/right"),
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "browser_back",
        description: "专属浏览器后退一页。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/browser/back",
        method: "POST",
        flag: None,
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::OnEnabled,
        params: &[
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "browser_wait_load",
        description: "等待专属浏览器页面加载稳定（慢页面/登录跳转后用；timeoutS 默认 5 秒）。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/browser/wait",
        method: "GET",
        flag: None,
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::OnEnabled,
        params: &[
            ("timeoutS", "number", false, "等待上限秒数（1~60，默认 5）"),
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "browser_login_status",
        description: "查询指定网站在专属浏览器里的登录态（只读：已登录/未登录/浏览器未运行）。刷网页遇到未登录页面时可先查这个再向管理员报告。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/browser/login-status",
        method: "GET",
        flag: None,
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::OnEnabled,
        params: &[
            ("site", "string", true, "站点 id（当前支持：xiaoheihe）"),
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "browser_show",
        description: "把专属浏览器以可见窗口打开（登录流程）：无视无头配置强制有头，可选直接导航到登录页。人在窗口里扫码/输验证码，cookie 自动留在专属 profile。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/browser/show",
        method: "POST",
        flag: None,
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::OnEnabled,
        params: &[
            ("url", "string", false, "要打开的登录页网址（可选）"),
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "browser_tabs",
        description: "查看/收拾专属浏览器的标签页：action=list 列出全部标签页（序号/标题/URL/是否当前页），action=close 关标签页（不给 index=只留当前页，给 index=关那一页；最后一个标签页不会关）。系统平时自动单页打开并按预算回收，标签页明显变多时才需要手动收拾。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/browser/tabs",
        method: "GET",
        flag: None,
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::OnEnabled,
        params: &[
            ("action", "string", false, "list=列出（默认）；close=关闭"),
            (
                "index",
                "number",
                false,
                "要关的标签页序号（action=close 可选；不给=只留当前页）",
            ),
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "browser_key",
        description: "按键盘键（配合输入框用）：enter 回车（很多发送框回车即发）、backspace 删光标前字符（打错了用）、delete、escape 关弹窗、方向键/home/end 移动光标、ctrl+a 全选（全选后再输入即替换内容，也用于清空）。modifiers 可选，如 ctrl、shift。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/browser/key",
        method: "POST",
        flag: None,
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::OnEnabled,
        params: &[
            (
                "key",
                "string",
                true,
                "键名：enter/backspace/delete/escape/tab/space/arrowup/arrowdown/arrowleft/arrowright/home/end/pageup/pagedown/a/c/v/x",
            ),
            (
                "modifiers",
                "string",
                false,
                "修饰键，逗号分隔：ctrl/shift/alt/meta（如 key=a + modifiers=ctrl 全选）",
            ),
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
    ToolDef {
        name: "browser_upload",
        description: "上传图片/视频到网页的文件选择框（发帖带图用）：source 传 http(s) 图片/视频直链（自动下载到桥缓存再填入）或鹊桥目录内的本地路径。index 选第几个文件输入框（默认 0）。先 snapshot 确认发帖窗口已打开再调。需要会话令牌；仅管理员授权的会话可用。",
        endpoint: "/api/browser/upload",
        method: "POST",
        flag: None,
        tablet_gate: TabletGate::None,
        browser_gate: BrowserGate::OnEnabled,
        params: &[
            (
                "source",
                "string",
                true,
                "http(s) 文件直链或鹊桥目录内本地路径",
            ),
            (
                "index",
                "number",
                false,
                "第几个 input[type=file]（默认 0）",
            ),
            ("conversationKey", "string", true, "会话 key"),
            ("agentToken", "string", true, "会话令牌"),
        ],
    },
];

fn tool_flag_enabled(flag: Option<&str>, tools: &Value) -> bool {
    match flag {
        Some(key) => tools.get(key).and_then(Value::as_bool).unwrap_or(true),
        None => true,
    }
}

/// 平板门禁判定（注册期与调用期共用）：OnEnabled 仅 tablet.enabled=true 时
/// 放行（八个控制类工具——关着不占 system prompt、不给模型误调机会，Node
/// 注册期过滤同义）；Always 恒放行（仅 tablet_status 探测）。
fn tablet_gate_ok(gate: TabletGate, tablet_cfg: &Value) -> bool {
    match gate {
        TabletGate::None | TabletGate::Always => true,
        TabletGate::OnEnabled => tablet_cfg.get("enabled") == Some(&Value::Bool(true)),
    }
}

fn browser_gate_ok(gate: BrowserGate, browser_cfg: &Value) -> bool {
    match gate {
        BrowserGate::None | BrowserGate::Always => true,
        BrowserGate::OnEnabled => browser_cfg.get("enabled") == Some(&Value::Bool(true)),
    }
}

/// 读 config.json 的 socialV2.tools（注册期过滤数据源；文件缺失按全默认开）。
/// CWD 无关的 config.json 定位（DSH spawn 的进程 CWD 不确定）。
fn config_path() -> std::path::PathBuf {
    if let Ok(home) = std::env::var("QUEQIAO_HOME") {
        return std::path::PathBuf::from(home).join("config.json");
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("config.json");
        if candidate.exists() {
            return candidate;
        }
    }
    std::path::PathBuf::from("config.json")
}

fn load_section(section: &str) -> Value {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|text| {
            serde_json::from_str::<Value>(text.trim_start_matches('\u{FEFF}'))
                .ok()
                .and_then(|cfg| cfg.get(section).cloned())
        })
        .unwrap_or_else(|| json!({}))
}

fn input_schema(def: &ToolDef) -> Value {
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, ty, req, desc) in def.params.iter().chain([&KEY_PARAM, &TOKEN_PARAM]) {
        props.insert(name.to_string(), json!({ "type": ty, "description": desc }));
        if *req {
            required.push(name.to_string());
        }
    }
    json!({
        "type": "object",
        "properties": props,
        "required": required,
    })
}

/// 转发工具调用到桥 HTTP。GET 走查询串，POST 走 JSON body。
/// 工具调用审计（Node tool-calls.jsonl 同源）：call/result 两行流水，
/// args 里 token 类参数打码。失败不影响工具调用本身。
fn audit_tool_call(def: &ToolDef, args: &Value, result: &Result<Value, String>, key: &str) {
    use std::io::Write;
    let home = std::env::var("QUEQIAO_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let _ = std::fs::create_dir_all(home.join("state"));
    let file = home.join("state").join("tool-calls.jsonl");
    let now = chrono_like_now();
    // token 类参数打码（审计流水会随诊断包导出）。
    let mut sanitized = args.clone();
    if let Some(obj) = sanitized.as_object_mut() {
        for k in ["agentToken", "token", "consoleToken"] {
            if let Some(v) = obj.get_mut(k)
                && v.as_str().is_some_and(|x| !x.is_empty())
            {
                *v = json!("***");
            }
        }
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
    else {
        return;
    };
    let tool = format!("mcp__queqiao__{}", def.name);
    let call = json!({
        "type": "call", "time": now, "key": key, "tool": tool,
        "args": serde_json::to_string(&sanitized).unwrap_or_default(),
    });
    let _ = writeln!(f, "{call}");
    let result_line = match result {
        Ok(_) => {
            json!({"type": "result", "time": now, "key": key, "tool": tool, "ok": true, "error": null})
        }
        Err(e) => {
            json!({"type": "result", "time": now, "key": key, "tool": tool, "ok": false, "error": e})
        }
    };
    let _ = writeln!(f, "{result_line}");
}

fn chrono_like_now() -> String {
    // ISO 时间戳（免引 chrono：systemtime→UTC 手拼，精度秒足够审计用途）。
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = now / 86400;
    let (y, m, d) = civil_from_days(days as i64);
    let secs = now % 86400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let mut y = 1970i64;
    let mut rest = days;
    loop {
        let len = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
            366
        } else {
            365
        };
        if rest < len {
            break;
        }
        rest -= len;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let ml = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    for (m, &l) in (1u32..).zip(ml.iter()) {
        if rest < l {
            return (y, m, rest as u32 + 1);
        }
        rest -= l;
    }
    (y, 12, 31)
}

fn call_tool(def: &ToolDef, args: &Value, console_token: &str) -> Result<Value, String> {
    let bridge =
        std::env::var("QUEQIAO_BRIDGE").unwrap_or_else(|_| "http://127.0.0.1:3100".to_owned());
    let token = args
        .get("agentToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let key = args
        .get("conversationKey")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    // tablet 端点是绝对路径（/api/tablet/*），QQ 工具是相对（/api/socialV2/）。
    let url = if def.endpoint.starts_with('/') {
        format!("{bridge}{}", def.endpoint)
    } else {
        format!("{bridge}/api/socialV2/{}", def.endpoint)
    };
    let resp = if def.method == "GET" {
        let mut extra = String::new();
        if let Some(seq) = args.get("afterSeq").and_then(Value::as_u64) {
            extra = format!("&afterSeq={seq}");
        }
        // tablet GET 工具的查询参数按参数表转发（如 read-screen 的 maxChars）。
        for (name, _, _, _) in def.params {
            if *name == "agentToken" || *name == "conversationKey" {
                continue;
            }
            if let Some(v) = args.get(*name) {
                let val = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                extra.push_str(&format!("&{name}={val}"));
            }
        }
        client
            .get(format!("{url}?conversationKey={key}{extra}"))
            .header("x-agent-token", &token)
            .header("x-console-token", console_token)
            .send()
            .map_err(|e| format!("桥连接失败（{bridge}）：{e}"))?
    } else {
        // 只转发桥关心的参数（去掉本层公共参数名后按工具参数表重组）。
        let mut body = serde_json::Map::new();
        body.insert("conversationKey".into(), json!(key));
        for (name, _, _, _) in def.params {
            if let Some(v) = args.get(*name) {
                body.insert(name.to_string(), v.clone());
            }
        }
        client
            .post(&url)
            .header("x-agent-token", &token)
            .header("x-console-token", console_token)
            .json(&Value::Object(body))
            .send()
            .map_err(|e| format!("桥连接失败（{bridge}）：{e}"))?
    };
    resp.json::<Value>()
        .map_err(|e| format!("桥响应解析失败：{e}"))
}

/// qq_send_burst 的连发实现：messages 归一（数组 / JSON 数组字符串 / 单条
/// 字符串），逐条调桥 send-message。间隔走 config send 节（Node computeGapsV2
/// auto 模式）：普通区间 burstIntervalMin/Max，longGapProbability 概率落
/// longGapMin/Max 长间隔，上限 maxGapMs；上限条数 burstMaxMessages。
fn send_burst(args: &Value, console_token: &str) -> Result<Value, String> {
    let group = args
        .get("groupId")
        .map(|g| match g {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => String::new(),
        })
        .unwrap_or_default();
    let token = args
        .get("agentToken")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let raw_messages = args.get("messages").cloned().unwrap_or(Value::Null);
    let mut messages: Vec<String> = Vec::new();
    match raw_messages {
        Value::Array(arr) => {
            for m in arr {
                if let Some(s) = m.as_str() {
                    messages.push(s.to_owned());
                }
            }
        }
        Value::String(s) => {
            let t = s.trim();
            if t.starts_with('[')
                && let Ok(parsed) = serde_json::from_str::<Value>(t)
                && let Some(arr) = parsed.as_array()
            {
                for m in arr {
                    if let Some(s) = m.as_str() {
                        messages.push(s.to_owned());
                    }
                }
            }
            if messages.is_empty() && !t.is_empty() {
                messages.push(s.clone());
            }
        }
        _ => {}
    }
    if messages.is_empty() {
        return Err("messages 为空（数组或非空字符串）".to_owned());
    }
    let send_cfg = load_section("socialV2")
        .get("send")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let num = |k: &str, d: u64| send_cfg.get(k).and_then(Value::as_u64).unwrap_or(d);
    let burst_max_messages = num("burstMaxMessages", 8).clamp(1, 8) as usize;
    if messages.len() > burst_max_messages {
        messages.truncate(burst_max_messages);
    }
    let burst_min = num("burstIntervalMinMs", 1_000).max(100);
    let burst_max = num("burstIntervalMaxMs", 3_000).max(burst_min);
    let long_prob = send_cfg
        .get("longGapProbability")
        .and_then(Value::as_f64)
        .unwrap_or(0.2)
        .clamp(0.0, 1.0);
    let long_min = num("longGapMinMs", 5_000).max(100);
    let long_max = num("longGapMaxMs", 10_000).max(long_min);
    let gap_cap = num("maxGapMs", 10_000).max(100);
    let key = format!("group:{group}");
    let send_def = TOOLS
        .iter()
        .find(|d| d.endpoint == "send-message")
        .expect("send-message 定义存在");
    let mut ids = Vec::new();
    for (i, text) in messages.iter().enumerate() {
        if i > 0 {
            // 长间隔判定 + 区间随机，clamp 到 maxGapMs（Node auto 模式）。
            let use_long = long_prob > 0.0 && (rand_delay_ms(1000) as f64) < long_prob * 1000.0;
            let (min, max) = if use_long {
                (long_min, long_max)
            } else {
                (burst_min, burst_max)
            };
            let gap = (min + rand_delay_ms(max - min)).min(gap_cap);
            std::thread::sleep(std::time::Duration::from_millis(gap));
        }
        let one = json!({
            "conversationKey": key,
            "text": text,
            "agentToken": token,
        });
        let out = call_tool(send_def, &one, console_token)?;
        ids.push(out.get("messageId").cloned().unwrap_or(Value::Null));
    }
    Ok(json!({ "ok": true, "messageIds": ids, "count": ids.len() }))
}

/// 0..bound 毫秒的伪随机（mcp 无随机依赖——时间戳低位抖动够用）。
fn rand_delay_ms(bound: u64) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    (nanos.wrapping_mul(6364136223846793005).wrapping_add(1) >> 33) % bound.max(1)
}

fn handle_request(
    id: &Value,
    method: &str,
    params: &Value,
    tools_cfg: &Value,
    tablet_cfg: &Value,
    browser_cfg: &Value,
    console_token: &str,
) -> Value {
    match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2024-11-05"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "queqiao-mcp", "version": env!("CARGO_PKG_VERSION") }
            }
        }),
        "tools/list" => {
            let tools: Vec<Value> = TOOLS
                .iter()
                .filter(|d| {
                    tool_flag_enabled(d.flag, tools_cfg)
                        && tablet_gate_ok(d.tablet_gate, tablet_cfg)
                })
                .map(|d| {
                    json!({
                        "name": d.name,
                        "description": d.description,
                        "inputSchema": input_schema(d),
                    })
                })
                .collect();
            json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } })
        }
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let Some(def) = TOOLS.iter().find(|d| d.name == name) else {
                return json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "content": [ { "type": "text", "text": format!("未知工具：{name}") } ], "isError": true }
                });
            };
            // 注册期被过滤的工具不可调（模型拿到了过期的工具清单也拦住）。
            if !tool_flag_enabled(def.flag, tools_cfg) {
                return json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "content": [ { "type": "text", "text": format!("工具 {name} 已被 config.socialV2.tools 关闭") } ], "isError": true }
                });
            }
            if !tablet_gate_ok(def.tablet_gate, tablet_cfg) {
                return json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "content": [ { "type": "text", "text": format!("工具 {name} 已被 config.tablet.enabled=false 关闭（仅 tablet_status 探测可用）") } ], "isError": true }
                });
            }
            if !browser_gate_ok(def.browser_gate, browser_cfg) {
                return json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "content": [ { "type": "text", "text": format!("工具 {name} 已被 config.browser.enabled=false 关闭（仅 browser_status 探测可用）") } ], "isError": true }
                });
            }
            let key = args
                .get("conversationKey")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            // qq_send_group/private_message：参数名变体（groupId/userId + message）
            // → conversationKey + text 统一走 send-message 端点。
            if name == "qq_send_group_message" || name == "qq_send_private_message" {
                let prefix = if name == "qq_send_group_message" {
                    "group:"
                } else {
                    "private:"
                };
                let raw_id = args
                    .get(if prefix == "group:" {
                        "groupId"
                    } else {
                        "userId"
                    })
                    .map(|v| match v {
                        Value::Number(n) => n.to_string(),
                        Value::String(s) => s.clone(),
                        _ => String::new(),
                    })
                    .unwrap_or_default();
                let mut one = json!({
                    "conversationKey": format!("{prefix}{raw_id}"),
                    "text": args.get("message").and_then(Value::as_str).unwrap_or_default(),
                    "agentToken": args.get("agentToken").and_then(Value::as_str).unwrap_or_default(),
                });
                if let Some(r) = args.get("replyToMessageId") {
                    one["replyToMessageId"] = r.clone();
                }
                let result = call_tool(def, &one, console_token);
                audit_tool_call(
                    def,
                    &args,
                    &result,
                    one.get("conversationKey")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
                return match result {
                    Ok(value) => json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "content": [ { "type": "text", "text": value.to_string() } ] }
                    }),
                    Err(e) => json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "content": [ { "type": "text", "text": e } ], "isError": true }
                    }),
                };
            }
            // qq_send_burst：多条连发，每条间真人化随机间隔（600-1500ms），
            // 逐条走 send-message 端点（白名单/频率/记账逐条生效）。
            if name == "qq_send_burst" {
                let result = send_burst(&args, console_token);
                audit_tool_call(def, &args, &result, &key);
                return match result {
                    Ok(value) => json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "content": [ { "type": "text", "text": value.to_string() } ] }
                    }),
                    Err(e) => json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "content": [ { "type": "text", "text": e } ], "isError": true }
                    }),
                };
            }
            let result = call_tool(def, &args, console_token);
            audit_tool_call(def, &args, &result, &key);
            match result {
                Ok(value) => {
                    // qq_get_self_image / qq_get_sticker_image / tablet_screenshot：
                    // 单图（value.image）转 MCP image 块（模型看不了裸 URL）。
                    if (name == "qq_get_self_image"
                        || name == "qq_get_sticker_image"
                        || name == "tablet_screenshot"
                        || name == "browser_screenshot")
                        && let Some(img) = value.get("image")
                        && let (Some(data), Some(mime)) = (
                            img.get("data").and_then(Value::as_str),
                            img.get("mimeType").and_then(Value::as_str),
                        )
                    {
                        let label = if name == "qq_get_self_image" {
                            let nickname =
                                value.get("nickname").and_then(Value::as_str).unwrap_or("?");
                            format!("当前形象（{nickname}）：")
                        } else if name == "tablet_screenshot" {
                            let file = value.get("file").and_then(Value::as_str).unwrap_or("");
                            format!("平板截图（{file}）：")
                        } else {
                            let desc = value.get("desc").and_then(Value::as_str).unwrap_or("");
                            let note = value.get("note").and_then(Value::as_str).unwrap_or("");
                            if note.is_empty() {
                                format!("收藏表情（{desc}）：")
                            } else {
                                format!("收藏表情（{desc}）——{note}")
                            }
                        };
                        return json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "content": [
                                { "type": "text", "text": label },
                                { "type": "image", "mimeType": mime, "data": data }
                            ] }
                        });
                    }
                    // qq_get_message_images：images 数组里带 data 的项转 MCP
                    // image 内容块（视觉模型可直接看图），其余以文本标注。
                    if name == "qq_get_message_images" {
                        let imgs = value.get("images").and_then(Value::as_array);
                        if let Some(imgs) = imgs
                            && !imgs.is_empty()
                        {
                            let mut content: Vec<Value> = Vec::new();
                            let mut labels = Vec::new();
                            for (i, img) in imgs.iter().enumerate() {
                                let kind =
                                    if img.get("kind").and_then(Value::as_str) == Some("face") {
                                        "表情"
                                    } else {
                                        "图片"
                                    };
                                match (
                                    img.get("data").and_then(Value::as_str),
                                    img.get("mimeType").and_then(Value::as_str),
                                ) {
                                    (Some(data), Some(mime)) => {
                                        labels.push(format!("[{}{}]", kind, i + 1));
                                        content.push(json!({ "type": "image", "mimeType": mime, "data": data }));
                                    }
                                    _ => labels.push(format!("[{}{}（获取失败）]", kind, i + 1)),
                                }
                            }
                            content.insert(
                                    0,
                                    json!({ "type": "text", "text": format!("消息媒体（{} 项）：\n{}", imgs.len(), labels.join("\n")) }),
                                );
                            return json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": { "content": content }
                            });
                        }
                    }
                    json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "content": [ { "type": "text", "text": value.to_string() } ] }
                    })
                }
                Err(e) => json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "content": [ { "type": "text", "text": e } ], "isError": true }
                }),
            }
        }
        "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        other => json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": -32601, "message": format!("method not found: {other}") }
        }),
    }
}

fn main() {
    let tools_cfg = load_section("socialV2")
        .get("tools")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let tablet_cfg = load_section("tablet");
    let browser_cfg = load_section("browser");
    let console_token = load_section("consoleToken")
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let registered = TOOLS
        .iter()
        .filter(|d| {
            tool_flag_enabled(d.flag, &tools_cfg) && tablet_gate_ok(d.tablet_gate, &tablet_cfg)
        })
        .count();
    eprintln!(
        "queqiao-mcp {} 启动：{} 个工具注册（{} 个被注册期过滤）",
        env!("CARGO_PKG_VERSION"),
        registered,
        TOOLS.len() - registered
    );
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // 通知（无 id）不回应。
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let resp = handle_request(
            &id,
            &method,
            &params,
            &tools_cfg,
            &tablet_cfg,
            &browser_cfg,
            &console_token,
        );
        if let Ok(text) = serde_json::to_string(&resp)
            && writeln!(out, "{text}").is_ok()
        {
            let _ = out.flush();
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::util::ServiceExt;

    /// 构造桥 router（同 queqiao/tests/tablet_api.rs 的最小装配；reqwest
    /// blocking client 不能在 async 上下文构造/析构，须在同步测试线程里建）。
    fn make_dsh() -> queqiao_dsh::client::DshClient<queqiao_dsh::client::ReqwestTransport> {
        queqiao_dsh::client::DshClient::new(
            "http://127.0.0.1:1",
            queqiao_dsh::client::ReqwestTransport::new(50),
            queqiao_dsh::client::AuthSources {
                launch_token: String::new(),
                token_explicit: false,
                secret: Box::new(|| None),
                rediscover_token: None,
            },
            false,
        )
    }

    fn bridge_router() -> axum::Router {
        let home = std::env::temp_dir().join(format!("queqiao-mcp-routes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("state")).unwrap();
        let social = queqiao::social_store::SocialStore::load(
            &home.join("state").join("social-v2.json"),
            None,
        )
        .unwrap();
        let (update_tx, _rx) = tokio::sync::mpsc::channel(1);
        let state = Arc::new(queqiao::tools_api::ToolApiState {
            sched_tx: tokio::sync::mpsc::unbounded_channel::<queqiao::orchestrator::SchedReq>().0,
            diary_pending: std::sync::Arc::new(std::sync::Mutex::new(None)),
            social: Arc::new(std::sync::Mutex::new(social)),
            onebot: queqiao_onebot::client::OneBotHttpClient::new("http://127.0.0.1:1", "", 50),
            cfg: queqiao::config::Config::default(),
            dsh: Arc::new(std::sync::Mutex::new(make_dsh())),
            sessions: std::sync::Arc::new(std::sync::Mutex::new(
                queqiao::session_manager::SessionManager::new(
                    Arc::new(std::sync::Mutex::new(make_dsh())),
                    &home.join("state").join("sessions.json"),
                    "qq-chat-v2",
                    "QQ 聊天",
                    "agents",
                )
                .unwrap(),
            )),
            update_tx,
            feedback_times: std::sync::Mutex::new(std::collections::HashMap::new()),
            slang_busy: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            slang_research_tx: tokio::sync::mpsc::unbounded_channel().0,
            browser: queqiao::browser::new_browser(),
        });
        queqiao::tools_api::build_router(state, "", None)
    }

    /// 守护：TOOLS 里每个 endpoint 按 call_tool 同款规则拼 URL，桥上必须有
    /// 路由。路径漂移时 axum 回 404 空 body，MCP 的 resp.json() 瞬时报
    /// 「error decoding response body」且无任何线索（qq_get_message_images
    /// 上线起全灭 5 天才发现，本测试即防复发）。缺参 400 / 门禁 403 都证明
    /// 路由存在，只断言 ≠404。
    #[test]
    fn all_tool_endpoints_exist_on_bridge() {
        let router = bridge_router();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            for def in TOOLS {
                let uri = if def.endpoint.starts_with('/') {
                    def.endpoint.to_owned()
                } else {
                    format!("/api/socialV2/{}", def.endpoint)
                };
                let req = if def.method == "GET" {
                    Request::builder()
                        .method("GET")
                        .uri(format!("{uri}?conversationKey=group:1"))
                        .body(Body::empty())
                        .unwrap()
                } else {
                    Request::builder()
                        .method("POST")
                        .uri(&uri)
                        .header("content-type", "application/json")
                        .body(Body::from("{\"conversationKey\":\"group:1\"}"))
                        .unwrap()
                };
                let resp = router.clone().oneshot(req).await.unwrap();
                assert_ne!(
                    resp.status(),
                    StatusCode::NOT_FOUND,
                    "{} → {uri} 在桥上无路由（404 空 body 会让 MCP 报解码失败）",
                    def.name
                );
            }
        });
    }
}
