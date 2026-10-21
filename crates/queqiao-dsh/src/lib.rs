//! queqiao-dsh —— DeepSeek Harness 私有协议客户端。
//!
//! 从 Node 版 `src/dsh-client.js`（逆向适配）移植，四个子系统（纯逻辑部分已入库，
//! HTTP/WS IO 客户端后续批次接入 reqwest + tokio-tungstenite）：
//! - [`auth`]：Cookie 离线铸造——读 `~/.dsh/.credentials.yaml` 的浏览器会话签名
//!   密钥，HMAC-SHA256 自签 Cookie（绕开「启动 token 只在内存」的获取难题）；
//! - [`envelope`]：斜杠 RPC 信封（`/api/<ns>/<method>`，client-request /
//!   server-response，args 按方法包装、session/prompt 自动补 requestId）；
//! - [`inbox`] / [`turn`]：remote.mux 事件流驱动——投影/回合帧解析，回合收集
//!   状态机（回合状态租约的心跳源；断连清理防重复累加）；
//! - [`port`]：端口自适应（候选口逐个 verify 业务口，Electron 杂端口不误判）。
//!
//! ⚠️ 版本跟进：DSH 0.1 → 0.2 已大改过一次（队列读法从 control baseline 挪到
//! projections），本 crate 的每个信封形状都要钉住单测。

pub mod auth;
pub mod envelope;
pub mod inbox;
pub mod port;
pub mod turn;
