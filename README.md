# 鹊桥（QueQiao）

QQ ↔ DeepSeek Harness（DSH）桥接的 Rust 实现：QQ 群/私聊消息经 [SnowLuma](https://github.com/SnowLuma/SnowLuma)（OneBot v11 网关）接入 DSH agent，AI 以仿真群友形态自主参与对话。

**权威参照**：Node 版 [qq-bridge](../qq-bridge) 是生产运行中的实现，其 43 个 audit 脚本钉住的行为契约是本仓库迁移的唯一语义标准；每个模块迁移时**测试先行、契约同步**。Rust 迁移的完整可行性分析见 qq-bridge 的 `docs/guides/RUST_MIGRATION_ANALYSIS.md`。

## 架构

```
crates/
├── queqiao-core/     纯逻辑内核：仿真状态机决策函数（零 IO，时间/随机性注入）
├── queqiao-onebot/   OneBot v11 客户端：WS 事件流 + HTTP 调用（连 SnowLuma 官方发行包）
├── queqiao-dsh/      DSH 私有协议：Cookie 离线铸造、斜杠 RPC、remote.mux 事件流
├── queqiao-media/    媒体处理：图片压缩 / 语音 ASR / 视频抽帧（后期阶段）
└── queqiao/          二进制主程序：tokio 编排（定时器/网络/文件 IO 皆在此层）
```

分层铁律（Node 版踩坑结晶）：**core 不碰 IO**——时钟与随机性一律参数注入，定时器/网络全在 `queqiao` bin 层，保证决策函数可离线全量单测。

## 路线图

1. **core 纯函数迁移**：wake（唤醒防卡死，已入库）→ retry（瞬时错误重投）→ diary（日记三路径调度）→ rate（频率与失败退配额）→ media-cache（LRU+TTL）；
2. **dsh 协议层**：Cookie 铸造（HMAC）→ RPC 信封 → mux 事件流，逐形状钉单测；
3. **onebot 客户端**：事件反序列化 → WS 重连 + token 自愈 → 媒体获取；
4. **bin 组装**：状态机编排 → 控制台 HTTP → 看门狗；
5. **对齐验收**：与 Node 版并行跑同一群消息流，行为对拍后切换。

## 许可说明

- 本仓库自身代码以 **MIT** 发布；
- 本仓库**不包含、不镜像、不逆向** SnowLuma 的任何专有组件（`snowluma-*.node` / `*.dll`）；运行时依赖官方发行包（一键脚本从官方发布地址原样下载），相关授权状态见仓库 Wiki / `AUTHORIZATION.md`（公开前补齐）；
- 使用者各自须遵守 [SnowLuma EULA](https://github.com/SnowLuma/SnowLuma/blob/main/EULA.md)，商业使用需向其权利人另行取得授权。

## 开发

```bash
cargo test        # 全工作区测试（core 契约测试是迁移进度标尺）
cargo run -p queqiao
```

## 更新记录

| 日期       | 变更内容                                     |
| ---------- | -------------------------------------------- |
| 2026-10-20 | 骨架：workspace 五 crate 分层 + wake 防卡死决策首个迁移（含契约测试） |
