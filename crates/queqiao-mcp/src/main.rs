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
];

fn tool_flag_enabled(flag: Option<&str>, tools: &Value) -> bool {
    match flag {
        Some(key) => tools.get(key).and_then(Value::as_bool).unwrap_or(true),
        None => true,
    }
}

/// 读 config.json 的 socialV2.tools（注册期过滤数据源；文件缺失按全默认开）。
fn load_section(section: &str) -> Value {
    std::fs::read_to_string("config.json")
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
            match call_tool(def, &args, console_token) {
                Ok(value) => json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "content": [ { "type": "text", "text": value.to_string() } ] }
                }),
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
