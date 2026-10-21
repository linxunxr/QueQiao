//! DSH 0.2.0 会话队列（inbox 投影）解析（移植自 Node 版 `src/dsh-client.js` 投影段
//! 与 `scripts/test-dsh-inbox-projection.mjs`，契约同步迁移）。
//!
//! 桥接退役/重置 QQ 会话时要先清掉 DSH 侧还没跑的**待处理队列**，再取消当前 turn。
//! 读队列这一步过去只认 `session/control` 开场 baseline 里的
//! `value.queues[<sessionId>]`；DSH 0.2.0 把队列并进了**投影表**——
//! `value.projections[<sessionId>].values.inbox`（形状
//! `{ 'next-turn': UserMessage[], 'next-step': UserMessage[] }`）——`queues` 键
//! 不复存在。后果不是「少清几条」，而是**每一次退役都抛错**：队列清不掉、旧任务
//! 继续在 DSH 里跑。本模块用真实采样下来的帧骨架把解析钉死：结构再变就是测试红，
//! 而不是线上静默漏水。

use serde_json::Value;

/// 投影解析失败（契约变更必须显式失败——静默当成空队列正是缺陷的形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxError {
    /// `invalid session inbox projection`
    InvalidProjection,
    /// `invalid session inbox item`
    InvalidItem,
    /// `invalid session/control baseline`
    InvalidBaseline,
}

impl std::fmt::Display for InboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InboxError::InvalidProjection => write!(f, "invalid session inbox projection"),
            InboxError::InvalidItem => write!(f, "invalid session inbox item"),
            InboxError::InvalidBaseline => write!(f, "invalid session/control baseline"),
        }
    }
}

impl std::error::Error for InboxError {}

/// 从一份 Session 投影值里取出待处理的 inbox 项（`next-turn` + `next-step`，
/// 顺序固定 next-turn → next-step）。
///
/// 每条是一个序列化后的 `UserMessage`，带 `id`（MessageId）——这正是
/// `session/updateQueue` 的 `itemId` 需要的值。
///
/// - 能力缺失（无 inbox 投影 / values 为 null / 非对象）⇒ 空数组，不是错误；
/// - 单个边界缺失是可选能力；
/// - inbox 存在但结构不是两个数组字段 ⇒ [`InboxError::InvalidProjection`]。
pub fn inbox_items_of_projections(values: Option<&Value>) -> Result<Vec<Value>, InboxError> {
    let Some(values) = values else {
        return Ok(Vec::new());
    };
    // 非对象（null/字符串/数字）视为能力缺失（与 Node 版 typeof 守卫一致）。
    let Some(obj) = values.as_object() else {
        return Ok(Vec::new());
    };
    let Some(inbox) = obj.get("inbox") else {
        return Ok(Vec::new());
    };
    let Some(inbox_obj) = inbox.as_object() else {
        return Err(InboxError::InvalidProjection);
    };
    let mut items = Vec::new();
    for boundary in ["next-turn", "next-step"] {
        match inbox_obj.get(boundary) {
            None => continue,
            Some(Value::Array(list)) => items.extend(list.iter().cloned()),
            Some(_) => return Err(InboxError::InvalidProjection),
        }
    }
    Ok(items)
}

/// 校验并抽出待处理项的 `itemId` 列表——`session/updateQueue` 需要 MessageId，
/// 拿不到就等于清不掉队列，必须显式失败而不是静默跳过。
pub fn inbox_item_ids(items: &[Value]) -> Result<Vec<String>, InboxError> {
    items
        .iter()
        .map(|item| {
            item.get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .ok_or(InboxError::InvalidItem)
        })
        .collect()
}

/// 从 `session/control` 开场 baseline 帧里取出指定会话的待处理项。
///
/// 0.2.0 的帧形状：`{ type:'baseline', value:{ projections:{ <sessionId>:{ asOfSeq,
/// values } } } }`。baseline 只包含**当前挂在宿主注册表里**的会话；不在其中即
/// 「没有活动 agent」⇒ 无待处理项。投影表结构不符必须抛错（含 0.1.7 的旧
/// `queues` 形状——它必须**不再被接受**，那正是缺陷本身）。
pub fn control_baseline_inbox_items(
    frame: Option<&Value>,
    session_id: &str,
) -> Result<Vec<Value>, InboxError> {
    let Some(frame) = frame else {
        return Err(InboxError::InvalidBaseline);
    };
    let projections = frame
        .get("value")
        .and_then(|v| v.get("projections"))
        .and_then(Value::as_object)
        .ok_or(InboxError::InvalidBaseline)?;
    let Some(entry) = projections.get(session_id) else {
        return Ok(Vec::new());
    };
    inbox_items_of_projections(entry.get("values"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 一条真实 DSH 0.2.0 inbox 项的骨架（实测采样，内容替换为占位符）。
    fn real_inbox_item(id: &str, text: &str) -> Value {
        json!({
            "content": [{ "type": "text", "text": text }],
            "source": { "kind": "user", "rpcId": "rpc-0000-0000-0000-000000000000" },
            "role": "user",
            "id": id,
        })
    }

    /// SessionProjectionValues 骨架（inbox === None 表示能力缺失）。
    fn projection_values(inbox: Option<Value>) -> Value {
        let mut v = json!({
            "title": "QQ 聊天",
            "tokenUsage": { "uncachedInputTokens": 0, "outputTokens": 0, "cacheReadTokens": 0, "cacheWriteTokens": 0 },
        });
        if let Some(inbox) = inbox {
            v["inbox"] = inbox;
        }
        v
    }

    /// session/control 开场 baseline 帧（真实采样层级）。
    fn control_baseline(sessions: &[(&str, Value)]) -> Value {
        let mut projections = serde_json::Map::new();
        for (id, values) in sessions {
            projections.insert(
                id.to_string(),
                json!({ "asOfSeq": 7, "values": values.clone() }),
            );
        }
        json!({ "type": "baseline", "value": { "projections": Value::Object(projections) } })
    }

    /// 契约与 Node 版 test-dsh-inbox-projection 一致：两个边界一起取，
    /// 顺序 next-turn → next-step。
    #[test]
    fn takes_both_boundaries_in_order() {
        let values = projection_values(Some(json!({
            "next-turn": [real_inbox_item("m-1", "第一轮")],
            "next-step": [real_inbox_item("m-2", "插话")],
        })));
        let ids = inbox_item_ids(&inbox_items_of_projections(Some(&values)).unwrap()).unwrap();
        assert_eq!(ids, ["m-1", "m-2"]);
    }

    /// 空队列返回空数组（不是错误）。
    #[test]
    fn empty_queues_are_empty() {
        let values = projection_values(Some(json!({ "next-turn": [], "next-step": [] })));
        assert!(
            inbox_items_of_projections(Some(&values))
                .unwrap()
                .is_empty()
        );
    }

    /// 能力缺失（无 inbox 键 / values 为 null / values 缺失）视为无待处理项。
    #[test]
    fn missing_capability_is_empty() {
        assert!(
            inbox_items_of_projections(Some(&projection_values(None)))
                .unwrap()
                .is_empty()
        );
        assert!(
            inbox_items_of_projections(Some(&Value::Null))
                .unwrap()
                .is_empty()
        );
        assert!(inbox_items_of_projections(None).unwrap().is_empty());
    }

    /// 只有一个边界存在时也能解析（另一个是可选能力）。
    #[test]
    fn single_boundary_is_fine() {
        let only_turn =
            projection_values(Some(json!({ "next-turn": [real_inbox_item("m-1", "x")] })));
        let ids = inbox_item_ids(&inbox_items_of_projections(Some(&only_turn)).unwrap()).unwrap();
        assert_eq!(ids, ["m-1"]);
        let only_step = projection_values(Some(json!({ "next-step": [] })));
        assert!(
            inbox_items_of_projections(Some(&only_step))
                .unwrap()
                .is_empty()
        );
    }

    /// 结构变了必须显式失败，不能静默当成空队列。
    #[test]
    fn structural_change_fails_loudly() {
        for bad in [
            json!({ "next-turn": null }),
            json!({ "next-turn": {} }),
            json!({ "next-turn": "m-1" }),
            json!({ "next-step": 3 }),
        ] {
            let values = projection_values(Some(bad.clone()));
            assert_eq!(
                inbox_items_of_projections(Some(&values)),
                Err(InboxError::InvalidProjection),
                "结构 {bad} 应报错"
            );
        }
        // inbox 是数组（JS Array.isArray 分支）。
        let values = projection_values(Some(json!([])));
        assert_eq!(
            inbox_items_of_projections(Some(&values)),
            Err(InboxError::InvalidProjection)
        );
        // inbox 是字符串。
        let values = projection_values(Some(json!("inbox")));
        assert_eq!(
            inbox_items_of_projections(Some(&values)),
            Err(InboxError::InvalidProjection)
        );
    }

    /// itemId 抽取：缺 id / 空串 / 非字符串必须报错（updateQueue 无从下手）。
    #[test]
    fn item_ids_require_string_ids() {
        let good = [real_inbox_item("m-1", "a"), real_inbox_item("m-2", "b")];
        assert_eq!(inbox_item_ids(&good).unwrap(), ["m-1", "m-2"]);
        for bad in [
            vec![json!({ "content": [] })],
            vec![json!({ "id": "" })],
            vec![json!({ "id": 42 })],
            vec![json!({ "id": null })],
            vec![Value::Null],
        ] {
            assert_eq!(
                inbox_item_ids(&bad),
                Err(InboxError::InvalidItem),
                "bad={bad:?}"
            );
        }
    }

    /// control baseline：从 projections[<sessionId>].values.inbox 取队列。
    #[test]
    fn baseline_reads_projection_table() {
        let frame = control_baseline(&[
            (
                "session-a",
                projection_values(Some(
                    json!({ "next-turn": [real_inbox_item("m-a1", "给 A 的")], "next-step": [] }),
                )),
            ),
            (
                "session-b",
                projection_values(Some(
                    json!({ "next-turn": [real_inbox_item("m-b1", "给 B 的"), real_inbox_item("m-b2", "还有一条")], "next-step": [] }),
                )),
            ),
        ]);
        let b = control_baseline_inbox_items(Some(&frame), "session-b").unwrap();
        assert_eq!(inbox_item_ids(&b).unwrap(), ["m-b1", "m-b2"]);
        let a = control_baseline_inbox_items(Some(&frame), "session-a").unwrap();
        assert_eq!(inbox_item_ids(&a).unwrap(), ["m-a1"]);
    }

    /// 会话不在 baseline 里 ⇒ 没有活动 agent ⇒ 无待处理项。
    #[test]
    fn absent_session_is_empty() {
        let frame = control_baseline(&[(
            "session-a",
            projection_values(Some(json!({ "next-turn": [], "next-step": [] }))),
        )]);
        assert!(
            control_baseline_inbox_items(Some(&frame), "session-zzz")
                .unwrap()
                .is_empty()
        );
    }

    /// 0.1.7 的旧形状 queues 已不存在——旧字段必须不再被接受（缺陷本身）。
    #[test]
    fn legacy_queues_shape_rejected() {
        let legacy = json!({
            "type": "baseline",
            "value": { "queues": { "session-a": [{ "id": "m-a1" }] } },
        });
        assert_eq!(
            control_baseline_inbox_items(Some(&legacy), "session-a"),
            Err(InboxError::InvalidBaseline)
        );
    }

    /// 投影表结构不符必须报错。
    #[test]
    fn malformed_baseline_rejected() {
        for bad in [
            json!({ "type": "baseline", "value": {} }),
            json!({ "type": "baseline", "value": { "projections": null } }),
            json!({ "type": "baseline", "value": { "projections": [] } }),
            json!({ "type": "baseline", "value": { "projections": "x" } }),
            json!({ "type": "baseline" }),
        ] {
            assert_eq!(
                control_baseline_inbox_items(Some(&bad), "session-a"),
                Err(InboxError::InvalidBaseline),
                "bad={bad}"
            );
        }
        assert_eq!(
            control_baseline_inbox_items(None, "session-a"),
            Err(InboxError::InvalidBaseline)
        );
    }

    /// 投影项缺 values 时视为能力缺失（无待处理项）。
    #[test]
    fn entry_without_values_is_empty() {
        let frame = json!({ "type": "baseline", "value": { "projections": { "session-a": { "asOfSeq": 3 } } } });
        assert!(
            control_baseline_inbox_items(Some(&frame), "session-a")
                .unwrap()
                .is_empty()
        );
    }
}
