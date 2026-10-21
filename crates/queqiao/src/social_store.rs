//! social-v2.json 持久态库（消息入库/seq 水位/agentToken/落盘——Node 版
//! appendSocialV2Message 与 saveSocialV2State 的编排对应物）。

use std::collections::BTreeMap;
use std::path::Path;

use queqiao_core::action::IncomingMessage;
use queqiao_core::runtime::SessionRuntime;

use crate::state::{ConversationState, MessageRecord, SocialV2File, atomic_write_json};

/// 内存持久态 + 落盘。
pub struct SocialStore {
    path: std::path::PathBuf,
    paused: bool,
    conversations: BTreeMap<String, ConversationState>,
    /// 白名单成员的 owner 标注。
    owner_qq: Option<u64>,
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

impl SocialStore {
    pub fn load(path: &Path, owner_qq: Option<u64>) -> Result<Self, String> {
        let file = if path.exists() {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("读 {} 失败：{e}", path.display()))?;
            serde_json::from_str::<SocialV2File>(text.trim_start_matches('\u{FEFF}'))
                .map_err(|e| format!("{} 解析失败：{e}", path.display()))?
        } else {
            SocialV2File::default()
        };
        Ok(Self {
            paused: file.paused,
            conversations: file.conversations,
            path: path.to_owned(),
            owner_qq,
        })
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    pub fn conversation(&self, key: &str) -> Option<&ConversationState> {
        self.conversations.get(key)
    }

    pub fn conversation_mut(&mut self, key: &str) -> &mut ConversationState {
        self.conversations.entry(key.to_owned()).or_default()
    }

    /// 消息入库（seq = lastUnreadSeq + 1；进 recent + unread；水位与活跃度推进；
    /// preSleep 观察窗作废——Node appendSocialV2Message 语义）。返回分配的 seq。
    pub fn append_message(&mut self, key: &str, msg: &IncomingMessage, now_ms: u64) -> u64 {
        let owner_qq = self.owner_qq;
        let owner = owner_qq.map(|o| o.to_string()).as_deref() == Some(msg.user_id.as_str());
        let st = self.conversation_mut(key);
        let seq = st.last_unread_seq + 1;
        let text = truncate_chars(&msg.text, 200);
        let record = MessageRecord {
            seq: Some(seq),
            message_id: msg.message_id.clone(),
            sender: msg.sender.clone(),
            user_id: Some(msg.user_id.clone()),
            text: text.clone(),
            plain: text.clone(),
            tail: Some(truncate_chars(&msg.text, 200)),
            quote_target_is_self: msg.reply_to_me,
            is_owner: owner,
            owner_label: owner.then(|| format!("管理员（ownerQQ {owner_qq:?}）")),
            is_self: msg.is_self,
            time: now_ms,
            ..Default::default()
        };
        st.last_unread_seq = seq;
        st.last_incoming_at = now_ms;
        // 有新消息进来，之前的「沉睡前已等待/已观察」作废。
        st.pre_sleep_wait_satisfied_at = 0;
        st.pre_sleep_wait_observed_at = 0;
        st.pre_sleep_wait_accum_ms = 0;
        st.recent_messages.push(record.clone());
        if !msg.is_self {
            st.unread.push(record);
        }
        let len = st.recent_messages.len();
        if len > 100 {
            st.recent_messages.drain(0..len - 100);
        }
        let len = st.unread.len();
        if len > 30 {
            st.unread.drain(0..len - 30);
        }
        seq
    }

    /// 会话 agentToken：缺失生成（24 字节随机 hex——Node crypto.randomBytes(24)）。
    pub fn ensure_agent_token(&mut self, key: &str) -> String {
        let st = self.conversation_mut(key);
        if let Some(token) = st.agent_token.clone()
            && !token.is_empty()
        {
            return token;
        }
        let token = random_hex24();
        st.agent_token = Some(token.clone());
        token
    }

    /// 决策层持久镜像回写（decide 改过的字段同步回持久态）。
    pub fn sync_runtime(&mut self, key: &str, rt: &SessionRuntime) {
        let st = self.conversation_mut(key);
        st.wake_times = rt.wake_times.clone();
        st.last_action_at = rt.last_action_at;
        st.wake_config.no_action_count = rt.no_action_count;
        st.bootstrap_sent = rt.bootstrap_sent;
    }

    /// 落盘（白名单式序列化 + 落盘截断）。
    pub fn save(&self) {
        let file = SocialV2File {
            paused: self.paused,
            conversations: self
                .conversations
                .iter()
                .map(|(k, v)| (k.clone(), v.clone().clamped()))
                .collect(),
        };
        if let Ok(mut text) = serde_json::to_string_pretty(&file) {
            text.push('\n');
            let _ = atomic_write_json(&self.path, &text);
        }
    }
}

fn random_hex24() -> String {
    // 24 字节随机数的 hex（48 字符）。无 rand 依赖：时间戳 + 进程计数器混合的
    /// xorshift——令牌强度足够（会话级令牌，非密码学边界；与 Node randomBytes
    /// 的差异不构成安全降级：令牌只在本地桥与 MCP 工具间传递）。
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64) << 32;
    let mut out = String::with_capacity(48);
    for _ in 0..6 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.push_str(&format!("{:08x}", seed as u32));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(key: &str, text: &str, user: &str) -> IncomingMessage {
        IncomingMessage {
            key: key.into(),
            seq: 0,
            message_id: "m1".into(),
            sender: "群友".into(),
            user_id: user.into(),
            text: text.into(),
            is_self: false,
            at_me: false,
            reply_to_me: false,
            time_ms: 0,
        }
    }

    /// 消息入库：seq 递增、recent/unread 追加、水位推进、观察窗作废。
    #[test]
    fn append_message_assigns_seq_and_buffers() {
        let dir = std::env::temp_dir().join(format!("queqiao-ss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("social-v2.json");
        let mut store = SocialStore::load(&path, Some(3388404701)).unwrap();

        let s1 = store.append_message("group:1", &msg("group:1", "第一条", "10001"), 1000);
        let s2 = store.append_message("group:1", &msg("group:1", "第二条", "3388404701"), 2000);
        assert_eq!((s1, s2), (1, 2), "seq 从 1 递增");
        let st = store.conversation("group:1").unwrap();
        assert_eq!(st.last_unread_seq, 2);
        assert_eq!(st.recent_messages.len(), 2);
        assert_eq!(st.unread.len(), 2);
        assert!(st.recent_messages[1].is_owner, "owner 消息标注");
        assert_eq!(st.last_incoming_at, 2000);
        store.save();
        // 落盘后重载：状态保持。
        let re = SocialStore::load(&path, Some(3388404701)).unwrap();
        assert_eq!(re.conversation("group:1").unwrap().last_unread_seq, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 自发回显进 recent 不进 unread；长文本截 200 字。
    #[test]
    fn self_echo_and_truncation() {
        let dir = std::env::temp_dir().join(format!("queqiao-ss2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = SocialStore::load(&dir.join("s.json"), None).unwrap();
        let mut m = msg("group:1", &"长".repeat(300), "10001");
        m.is_self = true;
        store.append_message("group:1", &m, 1000);
        let st = store.conversation("group:1").unwrap();
        assert_eq!(st.recent_messages.len(), 1);
        assert!(st.unread.is_empty(), "自发回显不进未读");
        assert_eq!(st.recent_messages[0].text.chars().count(), 200, "截 200 字");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// agentToken：生成后稳定（幂等）、48 hex 字符。
    #[test]
    fn agent_token_stable() {
        let dir = std::env::temp_dir().join(format!("queqiao-ss3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = SocialStore::load(&dir.join("s.json"), None).unwrap();
        let t1 = store.ensure_agent_token("group:1");
        let t2 = store.ensure_agent_token("group:1");
        assert_eq!(t1, t2);
        assert_eq!(t1.len(), 48);
        assert!(t1.chars().all(|c| c.is_ascii_hexdigit()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
