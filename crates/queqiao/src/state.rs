//! 会话状态持久化（规格 §六；形状兼容 Node 版 state/ 文件）。
//!
//! **白名单式序列化**：`ConversationState` 等 struct 本身就是持久白名单——
//! 定时器句柄、回合状态、pendingWakeReasons 等运行时字段**根本不进类型**，
//! 重启即清（防僵尸状态复活，Node 版实战教训；Rust 版由类型系统强制）。
//!
//! 落盘截断（Node 版 saveSocialV2State 同款）：recent 200 / unread 100 /
//! wakeTimes 200 / sendTimes 500 / stickerCollectTimes 500 / activeTopics 50 /
//! pendingThoughts 50 / seenForwardIds 1000——防状态文件随时间无限膨胀。
//!
//! 历史宽容：生产文件存在同文件内字段漂移（新老条目字段集不同）与**双层编码**
//! 遗留（个别字段值是 JSON 字符串）——一律 `#[serde(default)]` + 非预期类型
//! 归默认（Node 当前 load 行为：`Array.isArray(...) ? ... : []`）。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── sessions.json：key → DSH sessionId 与策略元数据 ─────────────────────────

/// sessions.json 形状。`session_policies` 的值是**JSON 字符串**（Node 版双层
/// 编码的历史形态，三元组 `[mode, preset, model|null]`——与 core::session_router
/// 的策略记录呼应），Rust 版保持同形态写盘以兼容双向切换。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionsFile {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub sessions: BTreeMap<String, String>,
    #[serde(rename = "sessionPolicies", skip_serializing_if = "BTreeMap::is_empty")]
    pub session_policies: BTreeMap<String, String>,
}

/// 解析一条 sessionPolicy 三元组：`[mode, preset, model|null]`。
/// 坏值（空/非 JSON/形状不符）返回 None（fail-closed，调用方按无策略处理）。
pub fn parse_session_policy(raw: &str) -> Option<(String, String, Option<String>)> {
    let v: Vec<Value> = serde_json::from_str(raw).ok()?;
    let mode = v.first()?.as_str()?.to_owned();
    let preset = v.get(1)?.as_str()?.to_owned();
    let model = v.get(2).and_then(Value::as_str).map(str::to_owned);
    if mode.is_empty() || preset.is_empty() {
        return None;
    }
    Some((mode, preset, model))
}

/// 把策略三元组编码回 JSON 字符串（写盘形态）。
pub fn encode_session_policy(mode: &str, preset: &str, model: Option<&str>) -> String {
    serde_json::to_string(&serde_json::json!([mode, preset, model])).expect("三元组序列化必然成功")
}

// ── social-v2.json：每会话状态 ────────────────────────────────────────────────

/// speakerIds：非零正整数字符串、去重；兼容「逗号/中文逗号/空白分隔的字符串」
/// 整体拆分（Node normalizeSpeakerIdsV2 语义）。
fn parse_speaker_ids(value: Option<&str>) -> Vec<String> {
    let Some(raw) = value else { return Vec::new() };
    let mut seen = std::collections::HashSet::new();
    raw.split([',', '，', ' ', '\t'])
        .map(|s| s.trim().to_owned())
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 20
                && s.as_bytes()[0] != b'0'
                && s.bytes().all(|b| b.is_ascii_digit())
                && seen.insert(s.to_owned())
        })
        .collect()
}

fn deserialize_speaker_ids<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // 数组逐项转字符串再统一过滤；非数组（含分隔符整串）走整体拆分。
    let raw = Value::deserialize(deserializer)?;
    let text = match &raw {
        Value::Array(items) => items
            .iter()
            .map(|v| match v {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    Ok(parse_speaker_ids(Some(&text)))
}

/// 宽容列表：数组逐项解析（坏项跳过）；非数组（含双层编码遗留的 JSON 字符串
/// 形态）归空——Node 当前 load 行为（`Array.isArray(...) ? ... : []`）。
fn lenient_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let raw = Value::deserialize(deserializer)?;
    Ok(match raw {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|item| serde_json::from_value::<T>(item).ok())
            .collect(),
        _ => Vec::new(),
    })
}

/// 宽容数值：数字或数字字符串（历史双层编码遗留），其余归 0。
fn lenient_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Value::deserialize(deserializer)?;
    Ok(match raw {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    })
}

/// 唤醒触发器配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WakeTriggers {
    pub at_mention: bool,
    pub name_mention: bool,
    #[serde(deserialize_with = "deserialize_speaker_ids")]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub speaker_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    pub question: bool,
    pub poke: bool,
    pub any_message: bool,
    pub probability: f64,
}

impl Default for WakeTriggers {
    fn default() -> Self {
        Self {
            at_mention: true,
            name_mention: true,
            speaker_ids: Vec::new(),
            keywords: Vec::new(),
            question: true,
            poke: true,
            any_message: false,
            probability: 0.5,
        }
    }
}

/// 每会话唤醒配置（持久字段；lastWakeAt/wakeCount/noActionCount 随状态落盘，
/// 但定时器类字段绝不落盘）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WakeConfigState {
    pub mode: String,
    pub infinite: bool,
    pub sleep_until: Option<u64>,
    pub triggers: WakeTriggers,
    pub batch_window_ms: u64,
    pub last_wake_at: u64,
    pub wake_count: u64,
    pub no_action_count: u32,
    pub confirmed_at: Option<u64>,
    pub confirmed_by: Option<String>,
}

impl Default for WakeConfigState {
    fn default() -> Self {
        Self {
            mode: "diving".into(),
            infinite: true,
            sleep_until: None,
            triggers: WakeTriggers::default(),
            batch_window_ms: 8000,
            last_wake_at: 0,
            wake_count: 0,
            no_action_count: 0,
            confirmed_at: None,
            confirmed_by: None,
        }
    }
}

/// 一条消息记录（同文件内字段漂移：老条目带 seq/userId/media，新条目更精简
/// ——全字段宽容）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MessageRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub message_id: String,
    pub sender: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub text: String,
    pub plain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail: Option<String>,
    pub quote_target_is_self: bool,
    pub is_owner: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_label: Option<String>,
    pub is_self: bool,
    pub time: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<Value>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub has_media: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub forward_ids: Vec<String>,
}

/// 活跃话题（记忆条目）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ActiveTopic {
    pub text: String,
    #[serde(default)]
    pub last_mention_at: u64,
    #[serde(default)]
    pub participants: Vec<String>,
    #[serde(default)]
    pub pending_question: Value,
}

/// 每会话持久状态——**本身就是持久白名单**：运行时字段（回合状态、
/// pendingWakeReasons、定时器句柄、proactiveNextCheckAt、modelSeenSeqs）
/// 不进本类型，重启即清。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ConversationState {
    pub wake_config: WakeConfigState,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_vec")]
    pub recent_messages: Vec<MessageRecord>,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_vec")]
    pub unread: Vec<MessageRecord>,
    #[serde(default)]
    pub last_wake_reason: Option<String>,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_u64")]
    pub last_ai_reply_at: u64,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_u64")]
    pub last_action_at: u64,
    #[serde(default)]
    pub agent_token: Option<String>,
    #[serde(default)]
    pub bootstrap_sent: bool,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_vec")]
    pub wake_times: Vec<u64>,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_vec")]
    pub send_times: Vec<u64>,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_vec")]
    pub sticker_collect_times: Vec<u64>,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_u64")]
    pub last_incoming_at: u64,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_u64")]
    pub pre_sleep_wait_satisfied_at: u64,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_u64")]
    pub pre_sleep_wait_observed_at: u64,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_u64")]
    pub pre_sleep_wait_accum_ms: u64,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_u64")]
    pub last_unread_seq: u64,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_u64")]
    pub last_read_through_seq: u64,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_vec")]
    pub active_topics: Vec<ActiveTopic>,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_vec")]
    pub pending_thoughts: Vec<Value>,
    #[serde(default)]
    pub member_impressions: BTreeMap<String, Value>,
    #[serde(default)]
    #[serde(deserialize_with = "lenient_vec")]
    pub seen_forward_ids: Vec<String>,
}

impl ConversationState {
    /// 落盘截断（Node saveSocialV2State 的 slice 上限，防止状态文件无限膨胀）。
    /// `now_ms` 用于 sleep_until 已过期值的清零。
    pub fn clamped(mut self) -> Self {
        self.recent_messages = self
            .recent_messages
            .split_off(self.recent_messages.len().saturating_sub(200));
        self.unread = self.unread.split_off(self.unread.len().saturating_sub(100));
        self.wake_times = self
            .wake_times
            .split_off(self.wake_times.len().saturating_sub(200));
        self.send_times = self
            .send_times
            .split_off(self.send_times.len().saturating_sub(500));
        self.sticker_collect_times = self
            .sticker_collect_times
            .split_off(self.sticker_collect_times.len().saturating_sub(500));
        self.active_topics = self
            .active_topics
            .split_off(self.active_topics.len().saturating_sub(50));
        self.pending_thoughts = self
            .pending_thoughts
            .split_off(self.pending_thoughts.len().saturating_sub(50));
        self.seen_forward_ids = self
            .seen_forward_ids
            .split_off(self.seen_forward_ids.len().saturating_sub(1000));
        self
    }
}

/// social-v2.json 顶层形状。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SocialV2File {
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub conversations: BTreeMap<String, ConversationState>,
}

// ── 文件 IO ──────────────────────────────────────────────────────────────────

/// 原子写 JSON（临时文件 + rename；Node atomicWriteJson 语义——读到一半的
/// 读者永远看到完整旧文件或完整新文件）。失败时清理临时文件。
pub fn atomic_write_json(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension(format!("{}.{}.tmp", std::process::id(), hex_suffix()));
    let result = (|| -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败：{e}"))?;
        }
        std::fs::write(&tmp, text).map_err(|e| format!("写临时文件失败：{e}"))?;
        // Windows 上目标被占用时 rename 会失败，短暂重试（Node renameWithRetry）。
        let mut last_err = None;
        for _ in 0..5 {
            match std::fs::rename(&tmp, path) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
        Err(format!(
            "rename 失败：{}",
            last_err.map(|e| e.to_string()).unwrap_or_default()
        ))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn hex_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{nanos:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sessions.json：真实形状（双层编码 policies）解析与回写编码。
    #[test]
    fn sessions_file_roundtrip() {
        let text = r#"{
  "sessions": {
    "private:3388404701": "session-efa3663f-e332-4283-adec-4d1bcd88890c",
    "group:771440459": "session-ac555502-f46d-42eb-b639-76924b075b18"
  },
  "sessionPolicies": {
    "private:3388404701": "[\"reserved2\",\"qq-chat-v2\",null]",
    "group:771440459": "[\"reserved2\",\"qq-chat-v2\",null]"
  }
}"#;
        let file: SessionsFile = serde_json::from_str(text).unwrap();
        assert_eq!(
            file.sessions["group:771440459"],
            "session-ac555502-f46d-42eb-b639-76924b075b18"
        );
        let (mode, preset, model) =
            parse_session_policy(&file.session_policies["private:3388404701"]).unwrap();
        assert_eq!(
            (mode.as_str(), preset.as_str(), model),
            ("reserved2", "qq-chat-v2", None)
        );
        // 回写编码形态一致。
        assert_eq!(
            encode_session_policy("reserved2", "qq-chat-v2", None),
            r#"["reserved2","qq-chat-v2",null]"#
        );
        // 坏值 fail-closed。
        assert_eq!(parse_session_policy(""), None);
        assert_eq!(parse_session_policy("not json"), None);
        assert_eq!(parse_session_policy(r#"["mode"]"#), None);
    }

    /// social-v2.json：真实形状解析（含字段漂移与双层编码遗留的宽容归默认）。
    #[test]
    fn social_v2_parses_real_shapes() {
        let text = r#"{
  "paused": false,
  "conversations": {
    "group:771440459": {
      "wakeConfig": {
        "mode": "diving", "infinite": true, "sleepUntil": null,
        "triggers": {
          "atMention": true, "nameMention": true,
          "speakerIds": [3388404701, "10001"],
          "keywords": ["小鲸鱼", "DeepSeek"],
          "question": true, "poke": true, "anyMessage": false, "probability": 0.5
        },
        "batchWindowMs": 8000, "lastWakeAt": 1792569958685,
        "wakeCount": 45, "noActionCount": 0,
        "confirmedAt": 1792570455459, "confirmedBy": "mark_read"
      },
      "recentMessages": [
        { "seq": 5, "messageId": "1093529304", "sender": "群友", "userId": "10001",
          "text": "是谁？", "plain": "是谁？", "tail": "是谁？",
          "quoteTargetIsSelf": false, "isOwner": false, "ownerLabel": "管理员",
          "isSelf": false, "time": 1792569000000,
          "media": [], "hasMedia": false },
        { "sender": "我", "text": "把我拉进去说不定更靠谱", "plain": "把我拉进去说不定更靠谱",
          "quoteTargetIsSelf": false, "isOwner": true, "ownerLabel": "我",
          "isSelf": true, "messageId": "-1445328766", "time": 1792570028732 }
      ],
      "unread": "[]",
      "lastWakeReason": "\"proactiveCheck\"",
      "agentToken": "\"4bb355468bb692e9\"",
      "bootstrapSent": true,
      "wakeTimes": [1791977078485, 1791978533216],
      "sendTimes": [1791978546197],
      "lastUnreadSeq": 34,
      "lastReadThroughSeq": 34,
      "activeTopics": [
        { "text": "话题一", "lastMentionAt": 1792570470750,
          "participants": ["管理员10001"], "pendingQuestion": null }
      ],
      "memberImpressions": { "管理员10001": { "traits": ["爱私聊"], "interactionCount": 1 } },
      "seenForwardIds": ["fwd-1"]
    }
  }
}"#;
        let file: SocialV2File = serde_json::from_str(text).unwrap();
        assert!(!file.paused);
        let st = &file.conversations["group:771440459"];
        // 字段漂移：新条目无 seq/userId 也能解析。
        assert_eq!(st.recent_messages.len(), 2);
        assert_eq!(st.recent_messages[0].seq, Some(5));
        assert_eq!(st.recent_messages[1].seq, None);
        assert!(st.recent_messages[1].is_self);
        // 双层编码遗留（字符串形态）→ 宽容归默认（Node 当前 load 同语义）。
        assert!(st.unread.is_empty(), "字符串化数组归空");
        // 字符串化标量原样保留（Node 不清洗这层历史损坏）。
        assert_eq!(st.agent_token.as_deref(), Some(r#""4bb355468bb692e9""#));
        // speakerIds 数字与字符串混排统一为字符串。
        assert_eq!(st.wake_config.triggers.speaker_ids, ["3388404701", "10001"]);
        assert_eq!(st.wake_config.wake_count, 45);
        assert_eq!(st.active_topics[0].participants, ["管理员10001"]);
        assert_eq!(st.last_unread_seq, 34);
    }

    /// 落盘截断：各列表超上限时截尾保最新。
    #[test]
    fn clamping_matches_node_limits() {
        let st = ConversationState {
            recent_messages: (0..250)
                .map(|i| MessageRecord {
                    message_id: i.to_string(),
                    ..Default::default()
                })
                .collect(),
            unread: (0..120)
                .map(|i| MessageRecord {
                    message_id: i.to_string(),
                    ..Default::default()
                })
                .collect(),
            send_times: (0..600).collect(),
            seen_forward_ids: (0..1200).map(|i| format!("f{i}")).collect(),
            ..Default::default()
        };
        let clamped = st.clamped();
        assert_eq!(clamped.recent_messages.len(), 200);
        assert_eq!(
            clamped.recent_messages.last().unwrap().message_id,
            "249",
            "保最新"
        );
        assert_eq!(clamped.unread.len(), 100);
        assert_eq!(clamped.send_times.len(), 500);
        assert_eq!(clamped.seen_forward_ids.len(), 1000);
        // 未超限不动。
        let small = ConversationState {
            recent_messages: vec![MessageRecord::default()],
            ..Default::default()
        };
        assert_eq!(small.clone().clamped().recent_messages.len(), 1);
    }

    /// 白名单：序列化输出只含持久字段（运行时字段在类型上就不存在——
    /// 断言键集合钉死落盘形状）。
    #[test]
    fn persisted_shape_is_whitelist() {
        let st = ConversationState::default();
        let out = serde_json::to_value(&st).unwrap();
        let keys: Vec<&str> = out
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut expected = [
            "wakeConfig",
            "recentMessages",
            "unread",
            "lastWakeReason",
            "lastAiReplyAt",
            "lastActionAt",
            "agentToken",
            "bootstrapSent",
            "wakeTimes",
            "sendTimes",
            "stickerCollectTimes",
            "lastIncomingAt",
            "preSleepWaitSatisfiedAt",
            "preSleepWaitObservedAt",
            "preSleepWaitAccumMs",
            "lastUnreadSeq",
            "lastReadThroughSeq",
            "activeTopics",
            "pendingThoughts",
            "memberImpressions",
            "seenForwardIds",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected);
        // 运行时字段（回合状态/定时器）不在输出里——类型保证。
        for banned in [
            "turnState",
            "pendingWakeReasons",
            "timers",
            "proactiveNextCheckAt",
        ] {
            assert!(out.get(banned).is_none(), "{banned} 不得落盘");
        }
    }

    /// speakerIds 宽容：分隔符整串拆分、非零正整数过滤、去重。
    #[test]
    fn speaker_ids_normalization() {
        let cfg = serde_json::from_str::<WakeConfigState>(
            r#"{"triggers": {"speakerIds": "10001, 10002，10001 x 0 03"}}"#,
        )
        .unwrap();
        assert_eq!(
            cfg.triggers.speaker_ids,
            ["10001", "10002"],
            "去重/剔非数字/剔 0/剔前导零"
        );
        // 私聊会话 speakerIds 恒空（Node load 语义），由调用方清洗。
    }

    /// 原子写：内容与回读一致、无临时文件残留。
    #[test]
    fn atomic_write_json_roundtrip() {
        let dir = std::env::temp_dir().join(format!("queqiao-state-test-{}", std::process::id()));
        let path = dir.join("probe.json");
        let _ = std::fs::remove_dir_all(&dir);
        atomic_write_json(&path, "{\"a\":1}\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}\n");
        // 目录里只有目标文件，没有 .tmp 残留。
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
