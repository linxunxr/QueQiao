//! queqiao-dsh —— DeepSeek Harness 私有协议客户端。
//!
//! 从 Node 版 `src/dsh-client.js`（逆向适配）移植，三个子系统：
//! - **Cookie 离线铸造**：读 `~/.dsh/.credentials.yaml` 的浏览器会话签名密钥，
//!   HMAC-SHA256 自签 Cookie（绕开「启动 token 只在内存」的获取难题）；
//! - **斜杠 RPC**：`/api/<namespace>/<method>` 信封（client-request / server-response），
//!   模型目录、会话 prompt、selectModel 等；
//! - **remote.mux 事件流**：turn/start、tool/call、tool/result、turn/end 帧驱动
//!   仿真状态机（回合状态租约的心跳源）。
//!
//! ⚠️ 版本跟进：DSH 0.1 → 0.2 已大改过一次（队列读法从 control baseline 挪到
//! projections），本 crate 的每个信封形状都要钉住单测。

/// RPC 信封类型（`type` 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeKind {
    ClientRequest,
    ServerResponse,
}

/// 铸造 Cookie 用的密钥材料（32 字节，读自 credentials）。
#[derive(Debug, Clone)]
pub struct BrowserSessionSecret(pub [u8; 32]);
