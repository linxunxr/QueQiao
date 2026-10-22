# 鹊桥（QueQiao）

QQ ↔ DeepSeek Harness（DSH）桥接的 Rust 实现：QQ 群/私聊消息经 [SnowLuma](https://github.com/SnowLuma/SnowLuma)（OneBot v11 网关）接入 DSH agent，AI 以仿真群友形态自主参与对话——看消息、发言、等待、唤醒调度、潜水、写日记，全部通过工具自主决定。

> **非官方、无隶属**：本项目是独立的第三方互操作性项目，与腾讯公司（Tencent）及 QQ 无任何隶属、合作、授权或背书关系；与 DeepSeek（杭州深度求索）同样无隶属关系。"QQ""腾讯"等名称及标识为其各自所有者的商标。使用非官方协议工具接入第三方平台可能违反该平台条款并导致账号风险，使用者自行承担。

## 当前状态（WIP）

核心链路已通、实机验证过，工具面与配套设施在按域补全：

| 层 | 状态 |
| --- | --- |
| core 决策内核（触发/调度/频率/防卡死/退配额/日记/回合生命周期） | ✅ 完成，契约测试全绿 |
| dsh 协议层（Cookie 铸造/RPC/mux/端口自适应） | ✅ 真实 DSH 实例冒烟通过 |
| onebot 客户端（事件流/调用/token 自愈） | ✅ 真实 SnowLuma 收发冒烟通过 |
| bin 编排（配置/持久化/决策循环/DSH 投递/mux 回流/prompt 构造） | ✅ 主程序实机三线并行运行 |
| MCP 工具面 | 🔶 核心闭环（桥侧 6 端点 + stdio server 6 工具）；45 个工具的其余部分按域补全 |
| 控制台面板 / 看门狗 / 表情包 / 语音 / 记忆注入 | ⬜ 未开始 |
| 与 Node 版行为对拍（最终验收） | ⬜ 未开始 |

## 架构

```
crates/
├── queqiao-core/     纯逻辑内核：仿真状态机决策函数（零 IO，时间/随机性注入）
├── queqiao-onebot/   OneBot v11 客户端：WS 事件流 + HTTP 调用（连 SnowLuma 官方发行包）
├── queqiao-dsh/      DSH 私有协议：Cookie 离线铸造、斜杠 RPC、remote.mux 事件流
├── queqiao-media/    媒体处理：图片压缩 / 语音 ASR / 视频抽帧（未开始）
├── queqiao-mcp/      MCP 工具面：DSH spawn 的 stdio server（qq_* 工具 → 桥 HTTP）
└── queqiao/          二进制主程序：tokio 编排（定时器/网络/文件 IO 皆在此层）
```

分层铁律：**core 不碰 IO**——时钟与随机性一律参数注入，定时器/网络全在 bin 层，保证决策函数可离线全量单测。

## 快速上手

**前提**（本项目不含也不分发 SnowLuma，需自备）：

1. Windows + Rust 工具链（edition 2024）；
2. [SnowLuma 官方发行包](https://github.com/SnowLuma/SnowLuma)已安装、已登录目标 QQ、OneBot 服务已开启（`<SnowLuma>/config/onebot_<QQ号>.json` 存在）；
3. DSH（DeepSeek Harness）已安装并初始化（`~/.dsh` 存在）；
4. 按需安装 DSH preset（agent 会话需要 `qq-chat-v2` 预设，安装方式见 DSH 文档）。

**一键安装（推荐）**：

```powershell
git clone https://github.com/linxunxr/QueQiao.git
cd QueQiao
powershell -NoProfile -ExecutionPolicy Bypass -File install.ps1
```

安装器总揽全流程（每步先检测、已装即跳过、可反复运行）：①DSH 检测与官方渠道引导；②SnowLuma 从官方 Releases 自动下载原样安装；③部署 `qq-chat-v2` preset 到 DSH；④构建 release；⑤交互式生成最小 `config.json`（管理员 QQ、群/私聊白名单）；⑥注册 MCP 条目。结束后按提示启动即可。

安装目录可选（`-InstallDir`，缺省 `%LOCALAPPDATA%\QueQiao`，与源码仓库无关）：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File install.ps1 -InstallDir D:\Tools\QueQiao
```

**手动路径**（偏好自己控制每一步时）：

```bash
git clone <本仓库> && cd QueQiao && cargo build --release
```

配置（exe 同目录布局；`QUEQIAO_HOME` 环境变量可覆盖 home）：参照 `crates/queqiao/tests/fixtures/config.example.json` 的形状编写 `config.json`，关键字段：`snowluma.homeDir`（SnowLuma 主目录，token 自发现数据源）、`allow.groups` / `allow.private`（会话白名单）、`ownerQQ`；preset 需手工部署 `dsh/agent-presets/` 到 `~/.dsh/agent-presets/`；MCP 注册见 `scripts/upgrade.ps1`。

**运行**：

```bash
cargo run --release -p queqiao
```

启动时依次：发现 SnowLuma 连接（per-account 双 token 自动发现）→ 探测 DSH 端口（3080 不通时自动扫描进程端口）→ 连接 WS 事件流与 DSH mux → 启动决策循环与工具 API（127.0.0.1:3100）。

**升级部署**（已装过全部依赖，一键脚本）：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/upgrade.ps1
```

构建 release、布置 `dist/`、把 `~/.dsh/profiles/web/cordis.patch.yml` 中的旧桥托管块替换为指向 `queqiao-mcp.exe` 的 MCP 条目（可用 `-NodeBridge <路径>` 从已有部署迁移 config 与 state）。之后停旧桥、在 `dist/` 运行 `queqiao.exe`、重启 DSH 即可。

**验证安装**（只读冒烟，不干扰生产）：

```bash
cargo run -p queqiao --example dsh_smoke      # DSH：铸 Cookie / 端口自适应 / mux
cargo run -p queqiao --example onebot_smoke   # SnowLuma：token 发现 / 收发 / 心跳
```

## 文件布局

运行期（exe 同目录）：

```
config.json          主配置（深度合并默认，缺键不丢默认）
state/sessions.json  会话 → DSH sessionId 映射
state/social-v2.json 每会话状态（白名单式序列化：运行时字段绝不落盘）
state/diary.jsonl    AI 日记（按日去重）
```

## 开发

```bash
cargo test                                    # 全工作区测试
cargo clippy --all-targets \
  --features queqiao-onebot/io,queqiao-dsh/reqwest,queqiao-dsh/ws \
  -- -D warnings                              # 特性组合下的完整检查
```

规格基线见 [docs/规格设计.md](docs/规格设计.md)（分层规格、七条迁移红线、里程碑与验收口径）。

## 许可

- 本仓库自身代码与文档以 **MIT** 发布（见 [LICENSE](LICENSE)，第三方依赖许可见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)）；
- 本仓库**不包含、不镜像、不逆向** SnowLuma 的任何专有组件（`snowluma-*.node` / `*.dll`）；运行时依赖官方发行包；
- 使用者各自须遵守 [SnowLuma EULA](https://github.com/SnowLuma/SnowLuma/blob/main/EULA.md) 与其 LICENSE（源码可见非商业许可），商业使用需向其权利人另行取得授权；
- 本项目公开发布的相关授权存证见 [`AUTHORIZATION.md`](AUTHORIZATION.md)。

## 更新记录

| 日期       | 变更内容                                                         |
| ---------- | ---------------------------------------------------------------- |
| 2026-10-20 | 骨架：workspace 分层 + wake 防卡死首个迁移（含契约测试）          |
| 2026-10-21 | M1~M4 主体：core 全量、dsh/onebot 双冒烟、bin 编排闭环、MCP 核心闭环、出包与部署脚本 |
| 2026-10-22 | 开源准备：LICENSE、路径参数化（CWD 无关）、README 面向外部受众重写、依赖许可审计 |
| 2026-10-22 | preset 随仓库分发 + install.ps1 一键安装总入口（SnowLuma 官方渠道自动下载） |
| 2026-10-22 | install.bat / upgrade.bat 双击启动器——绕开「双击 .ps1 记事本打开」与执行策略墙 |
| 2026-10-22 | v0.1.1：自动更新（检测/显示更新内容/空闲时自动应用重启）、preset 分发、一键安装 |
