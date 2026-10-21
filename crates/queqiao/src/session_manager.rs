//! DSH 会话管理与 prompt 投递（Node 版 ensureSession / deliverPromptNow 的
//! Rust 编排对应物；映射持久化在 state/sessions.json——形状见 state 模块）。
//!
//! 安全红线（Node 实战教训，保留）：**preset fail-closed**——目标 preset
//!（qq-chat-v2 等）不在 DSH 可用清单时拒绝建会话，绝不让 DSH 用默认 preset
//! （standard，带 bash/文件工具）顶上——那会把本地工具暴露给 QQ 群里的任何人。
//!
//! 本批简化（后续批次补）：selectModel 应用、投递队列、413 退役重投全链、
//! 媒体段。prompt 文本为简化模板（完整唤醒文本构造含人格卡/消息快照，下批）。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use queqiao_dsh::client::{DshClient, ReqwestTransport};
use serde_json::{Value, json};

use crate::state::{SessionsFile, encode_session_policy};

/// 会话管理器：key（group:/private: id）→ DSH sessionId 映射 + 建会话 + 投递。
pub struct SessionManager {
    dsh: Arc<Mutex<DshClient<ReqwestTransport>>>,
    sessions: SessionsFile,
    path: std::path::PathBuf,
    preset: String,
    workspace_title: String,
    agent_dir: String,
    /// preset 可用性缓存（启动时校验一次；fail-closed 红线的执行点）。
    preset_verified: bool,
}

impl SessionManager {
    pub fn new(
        dsh: Arc<Mutex<DshClient<ReqwestTransport>>>,
        state_path: &Path,
        preset: &str,
        workspace_title: &str,
        agent_dir: &str,
    ) -> Result<Self, String> {
        let sessions = if state_path.exists() {
            let text = std::fs::read_to_string(state_path)
                .map_err(|e| format!("读 {} 失败：{e}", state_path.display()))?;
            serde_json::from_str(text.trim_start_matches('\u{FEFF}'))
                .map_err(|e| format!("{} 解析失败：{e}", state_path.display()))?
        } else {
            SessionsFile::default()
        };
        Ok(Self {
            dsh,
            sessions,
            path: state_path.to_owned(),
            preset: preset.to_owned(),
            workspace_title: workspace_title.to_owned(),
            agent_dir: agent_dir.to_owned(),
            preset_verified: false,
        })
    }

    pub fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    fn save(&self) {
        if let Ok(text) = serde_json::to_string_pretty(&self.sessions) {
            let mut out = text;
            out.push('\n');
            let _ = crate::state::atomic_write_json(&self.path, &out);
        }
    }

    /// 确保会话存在：复用映射或创建（workspace 幂等 + session/create 带 preset）。
    pub fn ensure_session(&mut self, key: &str) -> Result<String, String> {
        if let Some(existing) = self.sessions.sessions.get(key) {
            return Ok(existing.clone());
        }
        self.verify_preset()?;
        let mut dsh = self.dsh.lock().map_err(|e| e.to_string())?;
        // workspace/create 对相同 path 幂等；统一 workspace 让 GUI 不散落。
        let ws = dsh
            .call_unary(
                "workspace/create",
                Some(json!({ "path": self.agent_dir })),
                Self::now_ms(),
            )
            .map_err(|e| format!("workspace/create 失败：{e}"))?
            .result
            .value
            .unwrap_or(Value::Null);
        let workspace_id = ws
            .get("workspace")
            .and_then(|w| w.get("workspaceId"))
            .and_then(Value::as_str)
            .ok_or("workspace/create 返回缺 workspaceId")?
            .to_owned();
        let created = ws.get("created").and_then(Value::as_bool).unwrap_or(false);
        if created && !self.workspace_title.is_empty() {
            let _ = dsh.call_unary(
                "workspace/rename",
                Some(json!({ "workspaceId": workspace_id, "title": self.workspace_title })),
                Self::now_ms(),
            );
        }
        let value = dsh
            .call_unary(
                "session/create",
                Some(json!({ "workspaceId": workspace_id, "agentPreset": self.preset })),
                Self::now_ms(),
            )
            .map_err(|e| format!("session/create 失败（preset {}）：{e}", self.preset))?
            .result
            .value
            .unwrap_or(Value::Null);
        let session_id = value
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or("session/create 返回缺 sessionId")?
            .to_owned();
        drop(dsh);
        self.sessions
            .sessions
            .insert(key.to_owned(), session_id.clone());
        self.sessions.session_policies.insert(
            key.to_owned(),
            encode_session_policy("reserved2", &self.preset, None),
        );
        self.save();
        Ok(session_id)
    }

    /// preset fail-closed：目标 preset 必须在 DSH 可用清单，否则拒绝建会话
    ///（绝不让默认 preset 的本地工具面暴露给 QQ 群）。
    fn verify_preset(&mut self) -> Result<(), String> {
        if self.preset_verified {
            return Ok(());
        }
        let mut dsh = self.dsh.lock().map_err(|e| e.to_string())?;
        let value = dsh
            .call_unary("agentPresets/list", None, Self::now_ms())
            .map_err(|e| format!("agentPresets/list 失败：{e}"))?
            .result
            .value
            .unwrap_or(Value::Null);
        let available: Vec<&str> = value
            .get("presets")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.get("id").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        if !available.contains(&self.preset.as_str()) {
            return Err(format!(
                "拒绝建会话：模式 reserved2 需要 preset \"{}\"，但它不在 DSH 可用清单（{}）中。\
回退到 DSH 默认 preset 会把本地工具暴露给 QQ 群；请先安装 preset 并重启 DSH。",
                self.preset,
                available.join(", ")
            ));
        }
        self.preset_verified = true;
        Ok(())
    }

    /// 投递 prompt（mode=queue，文本段）。会话在 DSH 侧不存在时退役重建一次。
    pub fn deliver_prompt(&mut self, key: &str, text: &str) -> Result<(), String> {
        let session_id = self.ensure_session(key)?;
        let payload = json!({
            "sessionId": session_id,
            "mode": "queue",
            "content": [ { "type": "text", "text": text } ],
        });
        let mut dsh = self.dsh.lock().map_err(|e| e.to_string())?;
        match dsh.call_unary("session/prompt", Some(payload), Self::now_ms()) {
            Ok(_) => Ok(()),
            Err(e) => {
                let code = match &e {
                    queqiao_dsh::client::ClientError::Rpc { code, .. } => code.clone(),
                    _ => String::new(),
                };
                if code.contains("not-found") {
                    // DSH 侧会话已不存在（重启/归档竞态）：退役本地映射重建一次。
                    drop(dsh);
                    self.retire(key);
                    let session_id = self.ensure_session(key)?;
                    let mut dsh = self.dsh.lock().map_err(|e| e.to_string())?;
                    dsh.call_unary(
                        "session/prompt",
                        Some(json!({
                            "sessionId": session_id,
                            "mode": "queue",
                            "content": [ { "type": "text", "text": text } ],
                        })),
                        Self::now_ms(),
                    )
                    .map_err(|e| format!("重投失败：{e}"))?;
                    Ok(())
                } else {
                    Err(format!("session/prompt 失败：{e}"))
                }
            }
        }
    }

    /// 退役会话映射（413 历史超限 / DSH 侧消失 / 模式切换 reconcile 用）。
    pub fn retire(&mut self, key: &str) {
        self.sessions.sessions.remove(key);
        self.sessions.session_policies.remove(key);
        self.save();
    }

    /// sessionId → 会话键 反查（mux 回流事件需要）。
    pub fn key_of_session(&self, session_id: &str) -> Option<String> {
        self.sessions
            .sessions
            .iter()
            .find(|(_, sid)| sid.as_str() == session_id)
            .map(|(key, _)| key.clone())
    }

    pub fn mappings(&self) -> &BTreeMap<String, String> {
        &self.sessions.sessions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_sessions(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    /// 映射持久化往返：已有映射的会话文件加载 + 退役写盘 + 反查。
    #[test]
    fn sessions_roundtrip_and_retire() {
        let dir = std::env::temp_dir().join(format!("queqiao-sm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        write_sessions(
            &path,
            r#"{"sessions":{"group:1":"s-a"},"sessionPolicies":{"group:1":"[\"reserved2\",\"qq-chat-v2\",null]"}}"#,
        );
        let (_transport_arc, manager) = manager_with(&path);
        assert_eq!(manager.mappings().len(), 1);
        assert_eq!(manager.key_of_session("s-a").as_deref(), Some("group:1"));
        let mut manager = manager;
        manager.retire("group:1");
        assert!(manager.mappings().is_empty(), "退役写盘并清内存");
        let reloaded: SessionsFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(reloaded.sessions.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn manager_with(path: &Path) -> (Arc<Mutex<DshClient<ReqwestTransport>>>, SessionManager) {
        // 会话文件 IO 与映射逻辑可离线测；DSH 调用路径靠实机验证（冒烟同源）。
        let sources = queqiao_dsh::client::AuthSources::token_only("", false);
        let client = DshClient::new(
            "http://127.0.0.1:9",
            ReqwestTransport::new(500),
            sources,
            false,
        );
        let arc = Arc::new(Mutex::new(client));
        let manager =
            SessionManager::new(arc.clone(), path, "qq-chat-v2", "QQ 聊天", "agents").unwrap();
        (arc, manager)
    }
}
