//! 启动/重连补拉（离线消息回填）。
//!
//! 病灶（2026-11-14 用户报障「早上启动时大量历史消息没有回复」）：桥的消息
//! 唯一来源是 OneBot WS 事件流，机器关机/SnowLuma 下线窗口里群里发的消息
//! 桥物理上收不到；启动连上 WS 后也没有对账——早上的 AI 只回「桥见过的最后
//! 几条」，隔夜消息要么迟 12 小时被顺带回一句，要么永远不回。
//!
//! 修复：WS **首次**连接成功后，对每个白名单群会话调 `get_group_msg_history`
//! 拉最近历史，把「晚于水位且桥没见过」的消息回填进 recent+unread，并推
//! MessageArrived 进决策循环（触发词命中照常唤醒；补拉量本身不硬造唤醒，
//! 全部不命中时由既有 proactiveCheck 兜底）。只在启动首连做——运行期 WS
//! 断连重连（秒级窗口）SnowLuma 会补推错帧期间的事件，再拉会与去重逻辑
//! 打架，收益趋零。

use std::collections::HashSet;

use serde_json::Value;

/// get_group_msg_history 的拉取条数（SnowLuma 上限 20/页；补拉窗口是
/// 「停机到启动」，20 条覆盖典型过夜闲群；更长的缺口由 AI 的
/// qq_get_group_history 工具自查——补拉只保证「不丢」不保证「全」）。
pub const BACKFILL_COUNT: u64 = 20;

/// 补拉窗口上限（毫秒）：水位过老（>12h）时按水位+12h 截断——把上周的
/// 陈年消息灌进 unread 只会污染上下文，那个量级该走主动检查里的工具自查。
pub const BACKFILL_MAX_WINDOW_MS: u64 = 12 * 3_600_000;

/// 补拉单元测试与 IO 层共用的解析产物。
#[derive(Debug, Clone, PartialEq)]
pub struct BackfillCandidate {
    pub group_id: u64,
    pub message_id: i64,
    /// 群内消息全局序（SnowLuma 的 message_seq；缺字段为 None——去重退
    /// message_id 集合判定）。
    pub message_seq: Option<u64>,
    pub sender: String,
    pub user_id: String,
    pub text: String,
    /// OneBot 事件 time 字段（秒）——补拉判定用真实消息时刻而非当前时刻。
    pub time_ms: u64,
    pub sent_by_self: bool,
    /// 段数组里的 at 段是否指向自己（与主流程 at_self 同判定）。
    pub at_me: bool,
}

/// 从 get_group_msg_history 响应里取 `message_seq`（消息在群内的全局序，
/// SnowLuma 返回；缺字段返回 None）。
fn message_seq(m: &Value) -> Option<u64> {
    m.get("message_seq").and_then(Value::as_u64)
}

/// 解析 get_group_msg_history 的 `messages` 数组为候选列表（时间升序；
/// 坏条目跳过不拦整体）。
pub fn parse_history(raw: &Value) -> Vec<BackfillCandidate> {
    let Some(arr) = raw.get("messages").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<BackfillCandidate> = arr
        .iter()
        .filter_map(|m| {
            let message_id = m.get("message_id").and_then(Value::as_i64)?;
            let sender_obj = m.get("sender")?;
            let card = sender_obj
                .get("card")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let sender = card
                .or_else(|| sender_obj.get("nickname").and_then(Value::as_str))
                .unwrap_or("?")
                .to_owned();
            let user_id = sender_obj
                .get("user_id")
                .and_then(Value::as_u64)
                .unwrap_or_default()
                .to_string();
            let segments = m.get("message").and_then(Value::as_array)?;
            let mut text = String::new();
            let mut at_me = false;
            for seg in segments {
                match seg.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let t = seg
                            .get("data")
                            .and_then(|d| d.get("text"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if !text.is_empty() && !t.is_empty() {
                            text.push(' ');
                        }
                        text.push_str(t);
                    }
                    Some("record") => {
                        if !text.is_empty() {
                            text.push(' ');
                        }
                        text.push_str("[语音]");
                    }
                    _ => {}
                }
                if seg.get("type").and_then(Value::as_str) == Some("at")
                    && seg.get("data").and_then(|d| d.get("qq")).is_some()
                {
                    at_me = true;
                }
            }
            let time_ms = m
                .get("time")
                .and_then(Value::as_u64)
                .unwrap_or_default()
                .saturating_mul(1000);
            // post_type == "message_sent" 为自发回显；历史接口一般不带，
            // user_id 匹配自身时按回显处理（调用方注入 self_uin）。
            let sent_by_self = m.get("post_type").and_then(Value::as_str) == Some("message_sent");
            let group_id = m
                .get("group_id")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            Some(BackfillCandidate {
                group_id,
                message_id,
                message_seq: message_seq(m),
                sender,
                user_id,
                text,
                time_ms,
                sent_by_self,
                at_me,
            })
        })
        .collect();
    out.sort_by_key(|c| c.time_ms);
    out
}

/// 补拉判定（纯函数）：解析产物 + 水位/已见集合 + 自身 uin → 需要入库的
/// 消息（时间升序）。
///
/// 过滤维度：
/// - **message_seq 去重（权威）**：SnowLuma 每条消息带全局递增 message_seq，
///   ≤ 已见最大 seq 的都是桥已经收过的（WS 在线期间收到的正常消息也有
///   message_seq——与历史接口同源）；
/// - **message_id 兜底去重**：接口不带 message_seq 时退已见集合判定；
/// - **自身消息过滤**：回显不入 unread（与主流程一致，AI 不需要被自己的
///   发言唤醒；仍进 recent 供上下文——见入库层）；
/// - **窗口截断**：time 早于「水位时刻 − 上限」的陈年消息丢弃；无水位
///   （新装/新群）不回填——bootstrap 链路自会建立上下文，灌陈年历史只会
///   污染上下文。
pub fn select_backfill(
    group_id: u64,
    candidates: &[BackfillCandidate],
    seen_message_ids: &HashSet<String>,
    max_seen_message_seq: Option<u64>,
    watermark_ms: u64,
    _now_ms: u64,
    self_uin: u64,
) -> Vec<BackfillCandidate> {
    if watermark_ms == 0 {
        return Vec::new();
    }
    let window_floor_ms = watermark_ms.saturating_sub(BACKFILL_MAX_WINDOW_MS);
    candidates
        .iter()
        .filter(|c| c.group_id == group_id)
        .filter(|c| {
            // 权威去重：message_seq 存在且 ≤ 已见最大值 → 已收过。
            match (c.message_seq, max_seen_message_seq) {
                (Some(seq), Some(max)) => seq > max,
                _ => true,
            }
        })
        .filter(|c| !seen_message_ids.contains(&c.message_id.to_string()))
        .filter(|c| c.user_id.parse::<u64>().unwrap_or(0) != self_uin)
        .filter(|c| c.time_ms >= window_floor_ms)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist_msg(id: i64, seq_in: i64, user: u64, text: &str, time: u64) -> Value {
        serde_json::json!({
            "message_id": id,
            "message_seq": seq_in,
            "group_id": 771440459u64,
            "time": time,
            "post_type": "message",
            "sender": { "user_id": user, "nickname": format!("群友{user}"), "card": "" },
            "message": [ { "type": "text", "data": { "text": text } } ],
        })
    }

    fn voice_msg(id: i64, user: u64, time: u64) -> Value {
        serde_json::json!({
            "message_id": id,
            "group_id": 771440459u64,
            "time": time,
            "post_type": "message",
            "sender": { "user_id": user, "nickname": "语音哥" },
            "message": [ { "type": "record", "data": { "file": "x.silk" } } ],
        })
    }

    /// 解析形状：text 拼接、语音占位、at 检出、时间升序。
    #[test]
    fn parses_and_orders() {
        let raw = serde_json::json!({
            "messages": [
                hist_msg(-3, 120, 10001, "早上好", 1794648000),
                hist_msg(-2, 121, 10002, "昨晚的事怎么说", 1794642000),
                voice_msg(-1, 10003, 1794645000),
            ]
        });
        let out = parse_history(&raw);
        assert_eq!(out.len(), 3);
        // 时间升序。
        assert!(out[0].time_ms <= out[1].time_ms && out[1].time_ms <= out[2].time_ms);
        assert!(out.iter().any(|c| c.text.contains("昨晚的事")));
        assert!(out.iter().any(|c| c.text == "[语音]"));
        // at 段检出。
        let at_raw = serde_json::json!({
            "messages": [serde_json::json!({
                "message_id": -7, "group_id": 771440459u64, "time": 1794640000,
                "sender": { "user_id": 10001, "nickname": "a" },
                "message": [
                    { "type": "at", "data": { "qq": 99999 } },
                    { "type": "text", "data": { "text": "在吗" } },
                ],
            })]
        });
        assert!(parse_history(&at_raw)[0].at_me);
    }

    /// 选择语义：seq 权威去重、message_id 兜底、自发回显剔除、窗口截断。
    #[test]
    fn selects_by_watermark_and_dedup() {
        let raw = serde_json::json!({
            "messages": [
                hist_msg(-5, 100, 10001, "旧的已收", 1794600000),
                hist_msg(-4, 101, 10002, "断档期间的新消息", 1794640000),
                hist_msg(-3, 102, 99999, "自己的回显", 1794641000),
                hist_msg(-2, 103, 10003, "也新", 1794642000),
            ]
        });
        let candidates = parse_history(&raw);
        let mut seen = HashSet::new();
        seen.insert("-2".to_owned()); // message_id 已见（WS 已收过的兜底去重）
        let out = select_backfill(
            771440459,
            &candidates,
            &seen,
            Some(100),
            1_794_600_000_000,
            1_794_648_000_000,
            99999,
        );
        assert_eq!(out.len(), 1, "只留断档期间的新消息：{out:?}");
        assert!(out[0].text.contains("断档期间"));
    }

    /// message_seq 缺失时退 message_id 去重（旧实现兼容）。
    #[test]
    fn falls_back_to_message_id_dedup() {
        let mut no_seq = hist_msg(-6, 1, 10001, "没带seq的新消息", 1794640000);
        let obj = no_seq.as_object_mut().unwrap();
        obj.remove("message_seq");
        let candidates = parse_history(&serde_json::json!({ "messages": [no_seq] }));
        // max_seen_message_seq=None：不能因 seq 缺失误杀，靠 message_id 判新。
        let out = select_backfill(
            771440459,
            &candidates,
            &HashSet::new(),
            None,
            1_794_600_000_000,
            1_794_648_000_000,
            99999,
        );
        assert_eq!(out.len(), 1);
        // 同 message_id 已见 → 去重。
        let mut seen = HashSet::new();
        seen.insert("-6".to_owned());
        let out2 = select_backfill(
            771440459,
            &candidates,
            &seen,
            None,
            1_794_600_000_000,
            1_794_648_000_000,
            99999,
        );
        assert!(out2.is_empty());
    }

    /// 无水位（新会话）不回填陈年历史。
    #[test]
    fn no_watermark_no_backfill() {
        let raw = serde_json::json!({ "messages": [hist_msg(-1, 5, 10001, "老消息", 1794640000)] });
        let candidates = parse_history(&raw);
        let out = select_backfill(
            771440459,
            &candidates,
            &HashSet::new(),
            None,
            0,
            1_794_648_000_000,
            99999,
        );
        assert!(out.is_empty(), "无水位时不灌历史");
    }

    /// 窗口截断：水位太老时丢弃陈年消息（不该把上周的灌进未读）。
    #[test]
    fn truncates_stale_window() {
        // 水位 3 天前，消息在水位前 20h → 超出「水位−12h」窗口，丢弃。
        let candidates = parse_history(&serde_json::json!({
            "messages": [hist_msg(-8, 300, 10001, "更早的", 1794328000)]
        }));
        let out = select_backfill(
            771440459,
            &candidates,
            &HashSet::new(),
            Some(100),
            1_794_400_000_000,
            1_794_648_000_000,
            99999,
        );
        assert!(out.is_empty(), "超窗丢弃");
        // 窗口内的保留（水位后 1h 的消息）。
        let candidates = parse_history(&serde_json::json!({
            "messages": [hist_msg(-7, 301, 10001, "窗口内的", 1794436400)]
        }));
        let out = select_backfill(
            771440459,
            &candidates,
            &HashSet::new(),
            Some(100),
            1_794_436_000_000,
            1_794_648_000_000,
            99999,
        );
        assert_eq!(out.len(), 1, "窗口内保留");
    }

    /// 群号不匹配的候选丢弃（多群共享响应的防御）。
    #[test]
    fn filters_foreign_group() {
        let mut foreign = hist_msg(-9, 1, 10001, "别的群", 1794640000);
        foreign["group_id"] = serde_json::json!(12345);
        let candidates = parse_history(&serde_json::json!({ "messages": [foreign] }));
        let out = select_backfill(
            771440459,
            &candidates,
            &HashSet::new(),
            None,
            1_794_600_000_000,
            1_794_648_000_000,
            99999,
        );
        assert!(out.is_empty());
    }
}
