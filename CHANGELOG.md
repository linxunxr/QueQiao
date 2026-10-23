# 更新日志（CHANGELOG）

本仓库的版本记录。格式参照 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本遵循语义化版本（SemVer）。

## 版本策略

- **0.x 阶段**：开发中，API 与行为可能随版本变化（SemVer 对 0.x 不要求兼容承诺）；
- **minor（0.2.0、0.3.0 …）**：对齐功能域完成（工具面补全、控制台与看门狗、记忆/表情包注入…）；
- **1.0.0 门槛**：与 Node 版行为对拍一致（M5）+ 生产切换完成——"可用于生产"的承诺点；
- patch（0.1.x）：缺陷修复与文档，不含行为变更。

## [0.3.0] - 2026-10-23

### 新增

- **轻量记忆域（迁移数据的完整消费链）**：三类记忆——进行中的话题（activeTopic，24h 过期、追加刷新）、想说还没说的（pendingThought，2h TTL）、对群友的印象（memberImpression，按人 trait 去重、50 人上限淘汰）。桥工具 API 新增 6 端点（memory / memory-append / memory-remove / memory-clear / recent-messages / message-detail）；MCP 工具面 6→11（新增 `qq_memory_append` / `qq_memory_query` / `qq_memory_remove` / `qq_get_recent_messages` / `qq_get_message_detail`）。唤醒 prompt 自动注入记忆块（【进行中的话题】【你想说但还没说的】【对群友的印象】），DSH 侧可随时回看近消息与消息详情。
- **Node 版桥迁移清单补全**：`migrate-from-node.ps1` 补 `state/tool-calls.jsonl`（工具调用审计流水）与 `state/qq-activity.log`（活动历史）——现 15 项，覆盖 config、DSH 会话映射、唤醒状态、社交消息流水、日记、记忆、表情包、黑话、花费记账与全部人格卡。配合记忆域，迁移后数据即刻可消费：实测迁移起桥后记忆查询可读出 Node 版积累的话题与群友印象，并随唤醒 prompt 注入。

## [0.2.1] - 2026-10-23

### 新增

- **日志与观测**：桥日志双写 stdout + `state/bridge.log`（2000 行轮转、单行截断、敏感值脱敏、本地时间）；崩溃捕获（panic 落日志）；会话活动流水 `state/activity.log`（消息/唤醒/发言事件，回溯 AI 行为用）；**一键诊断包**（控制台「导出诊断包」按钮——日志双份+版本+脱敏配置+会话概要单 JSON）。
- **安装体验**：SnowLuma 下载进度条与百分比；SnowLuma 三段式发现（参数/config homeDir/常见位置扫描/交互询问，不再单一固定目录）；安装目录交互式选择。
- **Node 版桥迁移**：`migrate-from-node.ps1` 一次性带走 config、DSH 会话映射、唤醒状态、日记、表情包、黑话、花费记账与全部人格卡。
- **安装器体验**：目录选择前置到第 0 步并使用系统文件夹选择对话框（取消回退手输）；安装器与配套文件（install/upgrade/migrate/preset）收尾迁入安装目录；桌面快捷方式「鹊桥 QueQiao.lnk」；安装完成询问「是否立即启动」（回车即启动桌面客户端）；**发布包（zip）布局直接部署自带 exe——全程无需 Rust 工具链**（源码布局才本地构建，install/upgrade 双脚本自动识别）。

### 修复

- 桥运行时 panic（自动更新通道在事件多路复用中被重复消费）；首次向导与升级部署的 preset 源路径错误；Windows PowerShell 对无 BOM UTF-8 脚本的 ANSI 误读（安装器/迁移器均补 BOM）；安装器三段式发现的空串崩溃（InstallDir 未定时的 Join-Path 空基目录、非交互场景 Read-Host 返回 null）与第 0 步目录选择的函数先使用后定义报错（三处均为 v0.2.1 用户实测报告，均已修）。

## [0.2.0] - 2026-10-22

### 新增

- **Tauri 2 桌面客户端（queqiao-app）**：窗口设置 UI（复用控制台页）、桥子进程编排（启动/停止/日志流/崩溃可见）、系统托盘（关窗隐藏到托盘、桥继续跑、退出杀桥）。桥作为独立子进程——桥崩溃不拖垮 UI。授权边界：客户端不加载 SnowLuma 专有组件，仅编排官方发行包进程。
- **设置 UI**：首次运行浏览器向导（无 config 自动弹浏览器，保存即启动）+ 日常控制台热更（`http://127.0.0.1:3100`）——常用项表单化、全量 JSON 高级视图、保存经 serde 校验。配置不再是用户门槛。
- **自动更新**：启动异步检测 GitHub Releases 新版 → 打印更新内容 → 自动下载 staging → 会话空闲时自动应用并重启（rename 化解 Windows 文件锁）。
- **升级部署更名**：setup → upgrade（名实相符）；安装目录交互式选择。

## [0.1.1] - 2026-10-22

### 新增

- **自动更新**：启动后异步检测 GitHub Releases 新版本——发现即打印版本号与更新内容，自动下载 staging；会话**全体空闲时**（无回合/无在途 prompt/无待发窗口，不打断会话）自动应用并重启。Windows 文件锁以 rename 化解（运行中 exe 可改名不可删除，旧文件转 `.old`，下次更新清理；`queqiao-mcp.exe` 即便正被 DSH 运行同样处理，DSH 下次 spawn 自然用新版）。检测/下载失败静默降级，不影响运行。
- **一键安装（双击入口）**：`install.bat`（全新安装）/ `upgrade.bat`（升级部署）——绕开"双击 .ps1 记事本打开"与执行策略墙。
- **preset 随仓库分发**：`dsh/agent-presets/`（qq-chat / qq-chat-v2）入库，安装器部署到 `~/.dsh/agent-presets/`——修复全新安装闭环缺口（此前 preset 只在 Node 版桥，外部用户会被 fail-closed 挡在会话创建外）。
- **install.ps1 总入口安装器**：DSH 官方渠道引导 → SnowLuma 从官方 Releases 自动下载原样安装（A 档授权形态）→ preset 部署 → 构建 → 交互式生成最小 config.json → MCP 注册；每步先检测、已装即跳过、可反复运行。

## [0.1.0] - 2026-10-22

首次公开发布。QQ ↔ DeepSeek Harness（DSH）桥接的 Rust 实现：核心链路全通、实机验证过，工具面与配套设施在按域补全。

### 核心链路（M1~M4 主体）

- **queqiao-core**（决策内核，零 IO）：唤醒防卡死（回合租约/暂存兜底）、瞬时错误重投与 413 历史超限分流、失败退配额、频率窗口、唤醒优先级与合批、触发判定八级链、日记三路径、回合生命周期与 noAction 重置、每会话策略路由（session_router）、多引擎搜索解析与聚合、「话说一半」判定；`decide` 主决策函数把规格的七条迁移红线全部在决策层组装。
- **queqiao-dsh**（DSH 协议）：浏览器会话 Cookie 离线铸造（HMAC）、斜杠 RPC 信封（args 按方法包装、requestId 自动补）、inbox 投影解析（0.2.0 形状、旧 queues 显式拒绝）、回合收集状态机、端口自适应、鉴权状态机（铸造优先、401 策略切换、回环闸门）、remote.mux 事件流状态机（follow 跨重连重放、快照隔离、流错误分级）。**真实 DSH 实例冒烟通过**（端口 3080→扫描发现、铸 Cookie 鉴权、mux 连接）。
- **queqiao-onebot**（OneBot v11 客户端）：事件强类型反序列化（含心跳 `status.good` 权威信号）、消息段构造、`file` 字段白名单、per-account 双 token 自发现与端点写回、HTTP 调用与 WS 事件流。**真实 SnowLuma 收发冒烟通过**（账号发现、心跳 good、自发消息）。
- **queqiao**（bin 编排）：config.json 强类型加载与深度合并默认（兼容 Node 版形状）、状态持久化（白名单式序列化：运行时字段绝不落盘）、编排器（代际定时器轮 + decide 驱动）、DSH 会话管理（preset fail-closed）、mux 回流决策、唤醒 prompt 文本构造（Node 版逐字转译）、seq 水位与持久镜像回写。**主程序实机三线并行运行**。
- **queqiao-mcp**（MCP 工具面）：stdio JSON-RPC（initialize/tools/list/tools/call）、注册期过滤、桥 HTTP 薄转发；桥侧工具 API 宿主（axum，六层校验链前三层 + status/prompt/unread/send-message/mark-read/set-wake-config 六端点）。

### 工程与合规

- 209 项契约测试全绿；clippy `-D warnings` 零告警（含特性组合）；
- 一键部署脚本（release 构建 → dist 布局 → DSH preset 托管块替换）；
- SnowLuma A 档书面授权取得并存证（`AUTHORIZATION.md`）：独立客户端公开、脚本部署、与 Node.js 同目录部署；不镜像、不打包分发、不修改原生组件为永久红线；
- MIT LICENSE 与第三方依赖许可声明（181 项依赖全部为宽松许可）。

### 已知限制（WIP）

- MCP 工具面为核心子集（6/45），其余按域补全；
- 控制台面板、看门狗、表情包、语音、记忆/角色卡注入未开始；
- 与 Node 版行为对拍（M5）未开始——切换生产前须完成。
