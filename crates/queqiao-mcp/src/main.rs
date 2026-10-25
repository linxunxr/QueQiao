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
struct ToolDef {
    name: &'static str,
    description: &'static str,
    /// 桥端点（相对 /api/socialV2/）。
    endpoint: &'static str,
    method: &'static str,
    /// config.socialV2.tools 的开关键名（None = 无开关恒注册）。
    flag: Option<&'static str>,
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
        params: &[],
    },
    ToolDef {
        name: "qq_get_unread_messages",
        description: "获取未读消息列表（可选 afterSeq 游标增量读取）",
        endpoint: "unread",
        method: "GET",
        flag: Some("getUnread"),
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
        description: "查看当前记忆（进行中的话题/想说未说/对群友的印象）",
        endpoint: "memory",
        method: "GET",
        flag: Some("memory"),
        params: &[],
    },
    ToolDef {
        name: "qq_memory_remove",
        description: "删除一条记忆（category + content 精确匹配）",
        endpoint: "memory-remove",
        method: "POST",
        flag: Some("memory"),
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
        params: &[("limit", "number", false, "人数上限（默认 10，最大 20）")],
    },
    ToolDef {
        name: "qq_slang_query",
        description: "查群聊黑话表（梗/网络用语的含义与用法，按出现次数排序）",
        endpoint: "slang/query",
        method: "GET",
        flag: Some("slangQuery"),
        params: &[("query", "string", false, "关键词（匹配词条/含义/用法）")],
    },
    ToolDef {
        name: "qq_slang_submit",
        description: "提交/更新一条黑话释义（同词条已存在则补充释义；新词条为 candidate 待确认）",
        endpoint: "slang/submit",
        method: "POST",
        flag: Some("slangSubmit"),
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
        description: "看表情图（返回 URL 供看图确认含义，再决定发不发）",
        endpoint: "sticker-image",
        method: "GET",
        flag: Some("getStickerImage"),
        params: &[("stickerId", "string", true, "表情 id/md5/URL 任一")],
    },
    ToolDef {
        name: "qq_send_sticker",
        description: "发一张收藏表情（一条消息只一张、不混文字；先 qq_list_stickers 选 id）",
        endpoint: "send-sticker",
        method: "POST",
        flag: Some("sendSticker"),
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
        params: &[],
    },
    ToolDef {
        name: "qq_reply",
        description: "在群里引用/回复某条消息并发文本（回更早的消息/多句指向不同人时用；上下文唯一时不必引用）。replyToMessageId 从近期消息工具获取，可为负数。需带会话令牌。",
        endpoint: "reply",
        method: "POST",
        flag: Some("reply"),
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
];

fn tool_flag_enabled(flag: Option<&str>, tools: &Value) -> bool {
    match flag {
        Some(key) => tools.get(key).and_then(Value::as_bool).unwrap_or(true),
        None => true,
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
    let url = format!("{bridge}/api/socialV2/{}", def.endpoint);
    let resp = if def.method == "GET" {
        let mut extra = String::new();
        if let Some(seq) = args.get("afterSeq").and_then(Value::as_u64) {
            extra = format!("&afterSeq={seq}");
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
/// 字符串），逐条调桥 send-message，间隔 600-1500ms 随机（Node 版真人化
/// 节奏）。上限 8 条防刷屏。
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
    if messages.len() > 8 {
        messages.truncate(8);
    }
    let key = format!("group:{group}");
    let send_def = TOOLS
        .iter()
        .find(|d| d.endpoint == "send-message")
        .expect("send-message 定义存在");
    let mut ids = Vec::new();
    for (i, text) in messages.iter().enumerate() {
        if i > 0 {
            let gap = 600 + rand_delay_ms(900);
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
                .filter(|d| tool_flag_enabled(d.flag, tools_cfg))
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
            let key = args
                .get("conversationKey")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
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
    let console_token = load_section("consoleToken")
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let registered = TOOLS
        .iter()
        .filter(|d| tool_flag_enabled(d.flag, &tools_cfg))
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
        let resp = handle_request(&id, &method, &params, &tools_cfg, &console_token);
        if let Ok(text) = serde_json::to_string(&resp)
            && writeln!(out, "{text}").is_ok()
        {
            let _ = out.flush();
        } else {
            break;
        }
    }
}
