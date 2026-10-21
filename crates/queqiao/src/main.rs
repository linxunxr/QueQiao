//! 鹊桥（QueQiao）—— QQ ↔ DeepSeek Harness 桥接主程序。
//!
//! 组装顺序（成熟后的形态）：
//! OneBot 事件流（queqiao-onebot）→ 仿真状态机（queqiao-core 决策 + 本 crate 的
//! tokio 编排）→ DSH 投递（queqiao-dsh）→ 回合事件回流驱动状态推进。
//!
//! Node 版（qq-bridge）是权威参照实现：行为契约以其 43 个 audit 脚本为准。

fn main() {
    println!(
        "{} {} — 鹊桥：QQ ↔ DeepSeek Harness 桥接（Rust 版，骨架期）",
        queqiao_core::APP_NAME,
        env!("CARGO_PKG_VERSION")
    );
}
