//! queqiao —— 鹊桥主程序的库面（bin 编排层的可测部分）。
//!
//! 分层铁律：IO（定时器/网络/文件）在 main.rs 的 tokio 编排里；本 lib 承载
//! 可离线单测的组装件（配置加载、状态持久化、控制台路由等）。

pub mod config;
pub mod console;
pub mod logging;
pub mod memory;
pub mod orchestrator;
pub mod paths;
pub mod prompt;
pub mod session_manager;
pub mod social_store;
pub mod state;
pub mod tools_api;
pub mod update;
