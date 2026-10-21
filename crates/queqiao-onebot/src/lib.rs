//! queqiao-onebot —— OneBot v11 客户端：连接 SnowLuma 官方发行包。
//!
//! 职责边界（Node 版 `@snowluma/sdk` 的替代）：
//! - WS 事件流客户端（3001）：群/私聊消息、拍一拍、心跳载荷（hook 健康信号）；
//! - HTTP 调用（3000）：发送、get_image / get_record / get_video、表情库、get_status；
//! - 事件模型：`serde` 强类型反序列化，拒绝 `serde_json::Value` 裸奔。
//!
//! ⚠️ 许可红线（见仓库 README）：本 crate 是 SnowLuma 的**客户端**，不含、不逆向、
//! 不复刻其任何专有组件；私有扩展字段（心跳 `status.good` 等）按官方文档与
//! 源码可见部分对照实现。

/// OneBot 事件骨架。字段按需补全——只建模桥实际消费的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OneBotEvent {
    GroupMessage {
        group_id: u64,
        user_id: u64,
        message_id: i64,
    },
    PrivateMessage {
        user_id: u64,
        message_id: i64,
    },
    GroupPoke {
        group_id: u64,
        target_id: u64,
    },
    FriendPoke {
        user_id: u64,
    },
    Heartbeat {
        online: bool,
        receive_healthy: bool,
    },
}
