//! queqiao-onebot —— OneBot v11 客户端：连接 SnowLuma 官方发行包。
//!
//! 职责边界（Node 版 `@snowluma/sdk` 的替代）：
//! - [`event`]：WS 事件流载荷（3001）的强类型反序列化——群/私聊消息、拍一拍、
//!   心跳载荷（`status.good` 是 hook 健康的权威信号）；
//! - [`segment`]：出站消息段构造（text/at/reply/image…）；
//! - [`file_guard`]：`file` 字段白名单与 home 限定（防路径穿越/SSRF）；
//! - [`conn`]：连接自发现——per-account 双 token 解析、按「谁能用」选择、
//!   端点写回（HTTP 与 WS 是两个不同 token，换账号后旧 token 全部 401）；
//! - `client` / `ws`（feature `io`）：HTTP 调用与 WS 事件流客户端。
//!
//! ⚠️ 许可红线（见仓库 README）：本 crate 是 SnowLuma 的**客户端**，不含、不逆向、
//! 不复刻其任何专有组件；私有扩展字段（心跳 `status.good` 等）按官方文档与
//! 源码可见部分对照实现。

#[cfg(feature = "io")]
pub mod client;
pub mod conn;
pub mod event;
pub mod file_guard;
pub mod segment;
#[cfg(feature = "io")]
pub mod ws;
