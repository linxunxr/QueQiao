//! 回合文本收集状态机与内容块提取（移植自 Node 版 `src/dsh-client.js` 的
//! `createTurnCollector` / `blocksToText`）。
//!
//! Node 版无独立单测文件，本模块按源码注释钉住的实战契约转译：
//! - **忽略 `assistant/chunk` 流式分块**：`assistant/message` 携带同一内容的完整
//!   组装文本，两者都累加会导致回复文本翻倍（曾因此把「收到」发成「收到收到」）；
//! - turn 按 `data.turn` 分组，`turn/end` 终结并吐出最终文本。

use std::collections::HashMap;

use serde_json::Value;

/// 一次 turn 的终结产物（`push` 返回 `Some` 时可取最终文本）。
#[derive(Debug, Clone, PartialEq)]
pub struct TurnEnd {
    pub turn: String,
    /// `data.reason` 原样透传（JSON 值，形态由 DSH 决定）。
    pub reason: Value,
    pub text: String,
}

/// 在会话事件流里收集一次 turn 的 assistant 文本（按 turn 分组）。
///
/// 输入是 mux 上的 session 事件帧（`{ type, data }` 形状的 `serde_json::Value`）；
/// collector 只消费四种事件，其余原样忽略。
#[derive(Debug, Default)]
pub struct TurnCollector {
    turns: HashMap<String, String>,
}

impl TurnCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// 处理一条 session/event；该事件终结了一个 turn 时返回 [`TurnEnd`]。
    pub fn push(&mut self, event: &Value) -> Option<TurnEnd> {
        let event_type = event.get("type").and_then(Value::as_str)?;
        let data = event.get("data");
        let turn_id = || data.and_then(|d| d.get("turn")).and_then(Value::as_str);
        match event_type {
            "turn/start" => {
                if let Some(turn) = turn_id() {
                    self.turns.insert(turn.to_owned(), String::new());
                }
                None
            }
            // 忽略流式分块（防文本翻倍——见模块文档）。
            "assistant/chunk" => None,
            "assistant/message" => {
                let turn = turn_id()?;
                let text = self.turns.get_mut(turn)?;
                for block in data
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if block.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(chunk) = block.get("text").and_then(Value::as_str)
                    {
                        text.push_str(chunk);
                    }
                }
                None
            }
            "turn/end" => {
                let turn = turn_id()?;
                let text = self.turns.remove(turn)?;
                Some(TurnEnd {
                    turn: turn.to_owned(),
                    reason: data
                        .and_then(|d| d.get("reason"))
                        .cloned()
                        .unwrap_or(Value::Null),
                    text,
                })
            }
            _ => None,
        }
    }

    /// 指定 turn 是否正在进行（未终结）。
    pub fn has(&self, turn: &str) -> bool {
        self.turns.contains_key(turn)
    }

    /// 进行中的回合数（断连清理场景的观测口：mux 断连应清空回合状态防重复累加）。
    pub fn active_turns(&self) -> usize {
        self.turns.len()
    }
}

/// 从 assistant 消息的 ContentBlock[] 中提取纯文本。
pub fn blocks_to_text(content: Option<&Value>) -> String {
    content
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn start(turn: &str) -> Value {
        json!({ "type": "turn/start", "data": { "turn": turn } })
    }

    fn chunk(turn: &str, text: &str) -> Value {
        json!({ "type": "assistant/chunk", "data": { "turn": turn, "text": text } })
    }

    fn message(turn: &str, blocks: Value) -> Value {
        json!({ "type": "assistant/message", "data": { "turn": turn, "message": { "content": blocks } } })
    }

    fn end(turn: &str, reason: Value) -> Value {
        json!({ "type": "turn/end", "data": { "turn": turn, "reason": reason } })
    }

    /// turn 生命周期：start → message 累加 → end 吐出完整文本。
    #[test]
    fn collects_text_across_turn() {
        let mut c = TurnCollector::new();
        assert!(c.push(&start("t-1")).is_none());
        assert!(c.has("t-1"));
        c.push(&message("t-1", json!([{ "type": "text", "text": "你好" }])));
        c.push(&message(
            "t-1",
            json!([{ "type": "text", "text": "，世界" }]),
        ));
        let done = c.push(&end("t-1", json!("stop"))).unwrap();
        assert_eq!(done.turn, "t-1");
        assert_eq!(done.reason, json!("stop"));
        assert_eq!(done.text, "你好，世界");
        assert!(!c.has("t-1"));
        // end 后再无该 turn：重复 end 不再产出。
        assert!(c.push(&end("t-1", Value::Null)).is_none());
    }

    /// assistant/chunk 必须被忽略（同内容双通道累加会把回复翻倍——实战契约）。
    #[test]
    fn ignores_stream_chunks() {
        let mut c = TurnCollector::new();
        c.push(&start("t-1"));
        c.push(&chunk("t-1", "收到"));
        c.push(&message("t-1", json!([{ "type": "text", "text": "收到" }])));
        let done = c.push(&end("t-1", Value::Null)).unwrap();
        assert_eq!(done.text, "收到");
    }

    /// 多 turn 并行互不串扰；未知事件类型与无 turn 字段的帧被忽略。
    #[test]
    fn parallel_turns_and_noise() {
        let mut c = TurnCollector::new();
        c.push(&start("t-1"));
        c.push(&start("t-2"));
        assert_eq!(c.active_turns(), 2);
        c.push(&message("t-1", json!([{ "type": "text", "text": "A" }])));
        c.push(&message("t-2", json!([{ "type": "text", "text": "B" }])));
        assert_eq!(c.push(&end("t-2", Value::Null)).unwrap().text, "B");
        assert_eq!(c.push(&end("t-1", Value::Null)).unwrap().text, "A");
        // 未知类型 / 缺 data.turn。
        assert!(
            c.push(&json!({ "type": "tool/call", "data": { "x": 1 } }))
                .is_none()
        );
        assert!(c.push(&json!({ "type": "turn/end", "data": {} })).is_none());
        assert!(c.push(&json!({})).is_none());
    }

    /// message 只认 text 块的字符串 text；其他块类型跳过。
    #[test]
    fn skips_non_text_blocks() {
        let blocks = json!([
            { "type": "text", "text": "前" },
            { "type": "tool_use", "name": "qq_send" },
            { "type": "text", "text": 42 },
            { "type": "text", "text": "后" },
        ]);
        assert_eq!(blocks_to_text(Some(&blocks)), "前后");
        assert_eq!(blocks_to_text(None), "");
        let mut c = TurnCollector::new();
        c.push(&start("t"));
        c.push(&message("t", blocks));
        assert_eq!(c.push(&end("t", Value::Null)).unwrap().text, "前后");
    }
}
