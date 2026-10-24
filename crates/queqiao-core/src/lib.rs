//! queqiao-core —— 纯逻辑内核：仿真状态机的全部**决策函数**，零 IO、零时钟依赖。
//!
//! 架构约定（沿用 Node 版踩坑总结出的分层）：
//! - 时间与随机性一律由参数注入（`now_ms` / 随机源），本 crate 永不直接读系统时钟——
//!   这是 Node 版 audit harness（vm 沙箱）验证过的可测性关键；
//! - IO（定时器、网络、文件）全部留在二进制 crate 编排，本 crate 只回答
//!   「给定状态与输入，下一步该做什么动作」；
//! - 从 Node 版迁移的每个模块都必须带着行为契约测试一起搬（Node 版 43 个
//!   audit 脚本钉住的语义是唯一权威）。
//!
//! 迁移进度（纯函数先行；「已入库」= 源码+契约测试均已迁移）：
//! 1. `wake`：唤醒防卡死（回合租约判定 / 暂存唤醒兜底决策）——已入库
//! 2. `retry`：agent 回合瞬时错误重投判定（容量类可重试 / 历史超限走压缩）——已入库
//! 3. `diary`：日记触发三路径判定（定点 / 安静提前 / 启动补写）与按日存储模型——已入库
//! 4. `media_cache`：取图短缓存（LRU+TTL）——已入库
//! 5. `recall`：自发消息撤回资格判定（规格外补充，qq_recall_message 端点校验）——已入库
//! 6. `search`：多引擎搜索解析与聚合排序（Bing/DDG/360 三源五路 + 熔断器）——已入库
//! 7. `v2_wait`：「话说一半」判定（规格外补充，qq_wait_for_messages 观察语义）——已入库
//! 8. `state`：会话状态模型——实体化为 Node 版 `src/core/session-router.mjs`
//!    （每会话策略路由，`session_router` 模块）——已入库
//! 9. `rate` / `schedule`：频率窗口与失败退配额 / 唤醒优先级合并与合批——
//!    已在 M4 编排批次从 bridge.js 提取为纯函数（rate.rs / schedule.rs）
//! 10. `action`：决策接口形态——BridgeEvent/BridgeAction/TimerKind（主循环契约）

pub mod action;
pub mod diary;
pub mod media_cache;
pub mod rate;
pub mod recall;
pub mod retry;
pub mod role_card;
pub mod runtime;
pub mod schedule;
pub mod search;
pub mod session_router;
pub mod slang;
pub mod sticker;
pub mod token_ledger;
pub mod triggers;
pub mod v2_wait;
pub mod wake;
pub mod watchdog;

pub const APP_NAME: &str = "queqiao";
