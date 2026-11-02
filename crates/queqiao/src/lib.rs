//! queqiao —— 鹊桥主程序的库面（bin 编排层的可测部分）。
//!
//! 分层铁律：IO（定时器/网络/文件）在 main.rs 的 tokio 编排里；本 lib 承载
//! 可离线单测的组装件（配置加载、状态持久化、控制台路由等）。

pub mod browser;
pub mod config;
pub mod console;
pub mod diary_sched;
pub mod logging;
pub mod longterm_store;
pub mod memory;
pub mod orchestrator;
pub mod paths;
pub mod prompt;
pub mod review_sched;
pub mod role_card_store;
pub mod session_manager;
pub mod slang_store;
pub mod social_store;
pub mod state;
pub mod sticker_store;
pub mod tablet;
pub mod tools_api;
pub mod update;
pub mod upgrade_reconcile;
pub mod watch;

#[cfg(test)]
/// QUEQIAO_HOME 环境变量测试互斥锁：进程级 env 的并行 set_var 存在竞态
///（upgrade_reconcile 与 review_sched 的文件状态测试共用——同三 store
/// FS_LOCK 的教训，env 污染面跨模块时锁也必须跨模块）。
pub(crate) static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
