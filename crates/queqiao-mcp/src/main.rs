//! queqiao-mcp —— MCP 工具面（独立 stdio 进程，DSH spawn）。
//!
//! 三层权限体系（规格 §3.6）：
//! 1. 注册期过滤：socialV2.tools 开关为 false 的工具不注册（描述/schema 不进模型上下文）；
//! 2. 调用期鉴权：agent token + 会话准入；
//! 3. 桥端点级校验：模式门禁/工具开关/静默/白名单/频率/业务合法性。
//!
//! 工具回调 = 对桥 HTTP（127.0.0.1:3100 /api/socialV2/*）的薄转发；崩溃不拖垮主桥。

fn main() {
    println!("queqiao-mcp — MCP 工具面（骨架期，规格见 docs/规格设计.md §3.6）");
}
