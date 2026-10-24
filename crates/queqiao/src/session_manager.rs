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
    /// 模型钉选配置快照（热更时由 bin 同步；sig 变化触发重选）。
    dsh_model: (String, String, String), // (provider, model, reasoningEffort)
    /// 会话 → 已应用模型 sig（Node modelAppliedSessions+epoch 的等价物：
    /// sig 含全部三参，配置变化天然失效重选）。
    model_applied: std::collections::HashMap<String, String>,
}

/// agent 目录绝对化：DSH 按自身 CWD（Electron 主进程，任意）解析相对路径，
/// 相对 path 一律 realpath ENOENT——Node 版发绝对路径，Rust 版曾相对直传
/// （2026-10-24 真机：目录存在仍 create 失败，example 绝对路径同刻成功，
/// 两次"缺 workspaceId"表象相同真因不同：首犯目录缺失，次犯相对路径）。
fn normalize_agent_dir(dir: &str) -> String {
    let p = Path::new(dir);
    if p.is_absolute() {
        return dir.to_owned();
    }
    std::path::absolute(p)
        .map(|a| a.display().to_string())
        .unwrap_or_else(|_| dir.to_owned())
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
            agent_dir: normalize_agent_dir(agent_dir),
            preset_verified: false,
            dsh_model: Default::default(),
            model_applied: std::collections::HashMap::new(),
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
        // DSH 对不存在路径直接 workspace/invalid-path（realpath ENOENT）——先
        // 建目录再 create（Node 版 mkdirSync recursive 同款）。旧会话有映射
        // 不走此处，新会话首次落此：安装目录 state/agents 从未被创建过
        // （迁移清单不迁 agents/），2026-10-24 真机新会话全卡在 create。
        std::fs::create_dir_all(&self.agent_dir)
            .map_err(|e| format!("建 agent 目录失败（{}）：{e}", self.agent_dir))?;
        let mut dsh = self.dsh.lock().map_err(|e| e.to_string())?;
        // workspace/create 对相同 path 幂等；统一 workspace 让 GUI 不散落。
        let ws = dsh
            .call_unary(
                "workspace/create",
                Some(json!({ "path": self.agent_dir })),
                Self::now_ms(),
            )
            .map_err(|e| format!("workspace/create 失败：{e}"))?
            // 业务错误原样透出（DSH 的 code/message）：曾直接挖 result.value，
            // ok=false 时错误被吞、统一报"缺 workspaceId"，真因（invalid-path）
            // 藏在 value=null 后面——日志只见现象不见原因。
            .unwrap("workspace/create")
            .map_err(|e| format!("workspace/create 失败：{e}"))?;
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

    /// 热更同步模型钉选配置（bin 在 config 热更时调用；sig 变化后重选）。
    pub fn set_dsh_model(&mut self, provider: &str, model: &str, effort: &str) {
        self.dsh_model = (provider.to_owned(), model.to_owned(), effort.to_lowercase());
    }

    /// 模型钉选（Node ensureChatModel）：sig 缓存命中跳过；effort 为 none/空
    /// 不带字段（路由别名模型必须不带档位）；「不支持思考档位」降级去档重试
    /// （钉模型优先于档位——旧日志 112 条同错的慢性根因）；not-found 上抛
    /// 由 deliver_prompt 的退役重建路径处理。
    fn ensure_chat_model(&mut self, session_id: &str) -> Result<(), String> {
        let (provider, model, effort) = self.dsh_model.clone();
        if provider.is_empty() && model.is_empty() {
            return Ok(()); // 未配置 = 用 DSH 默认
        }
        let sig = format!("{provider}/{model}/{effort}");
        if self.model_applied.get(session_id) == Some(&sig) {
            return Ok(());
        }
        let with_effort = effort != "none" && !effort.is_empty();
        let mut dsh = self.dsh.lock().map_err(|e| e.to_string())?;
        let build = |eff: bool| {
            let mut payload = json!({
                "sessionId": session_id,
                "provider": provider,
                "model": model,
            });
            if eff {
                payload["reasoningEffort"] = json!(effort);
            }
            payload
        };
        let now = Self::now_ms();
        match dsh.call_unary("session/selectModel", Some(build(with_effort)), now) {
            Ok(_) => {
                drop(dsh);
                self.model_applied.insert(session_id.to_owned(), sig);
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                if with_effort && msg.contains("does not support reasoning effort") {
                    // 降级：去档位重试——思考强度交给 DSH/路由默认。
                    let r = dsh.call_unary("session/selectModel", Some(build(false)), now);
                    drop(dsh);
                    match r {
                        Ok(_) => {
                            self.model_applied
                                .insert(session_id.to_owned(), format!("{provider}/{model}/none"));
                            Ok(())
                        }
                        Err(e2) => Err(format!("selectModel 失败（去档重试）：{e2}")),
                    }
                } else if msg.to_lowercase().contains("not") && msg.to_lowercase().contains("found")
                {
                    Err(format!("not-found: {msg}"))
                } else {
                    Err(format!("selectModel 失败：{msg}"))
                }
            }
        }
    }

    /// 投递 prompt（mode=queue，文本段）。会话在 DSH 侧不存在时退役重建一次。
    pub fn deliver_prompt(&mut self, key: &str, text: &str) -> Result<(), String> {
        let session_id = self.ensure_session(key)?;
        if let Err(e) = self.ensure_chat_model(&session_id) {
            if e.starts_with("not-found") {
                // 与 DSH 侧会话消失同路：退役重建一次再投。
                self.retire(key);
                let session_id = self.ensure_session(key)?;
                self.ensure_chat_model(&session_id)?;
            } else {
                // 钉模型失败不阻塞投递（会话跑 DSH 默认模型）——记日志继续。
                // 连续失败每条都会重试（sig 未缓存）。
            }
        }
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

    /// agent_dir 必须绝对化：DSH 按自身 CWD 解析相对路径，相对即 ENOENT
    /// （2026-10-24 真机第二次 create 失败的真因——目录在，路径相对）。
    #[test]
    fn agent_dir_normalized_to_absolute() {
        let (_, m) = manager_with(Path::new("nonexistent-agent-dir-test.json"));
        assert!(
            Path::new(&m.agent_dir).is_absolute(),
            "相对 agent_dir 须被绝对化：{}",
            m.agent_dir
        );
        let abs = r"D:\somewhere\state\agents";
        assert_eq!(normalize_agent_dir(abs), abs);
    }
}
