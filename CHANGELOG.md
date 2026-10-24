# 更新日志（CHANGELOG）

本仓库的版本记录。格式参照 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本遵循语义化版本（SemVer）。

## 版本策略

- **0.x 阶段**：开发中，API 与行为可能随版本变化（SemVer 对 0.x 不要求兼容承诺）；
- **minor（0.2.0、0.3.0 …）**：功能域对齐（如记忆域、工具面补全、控制台与看门狗）；
- **1.0.0 门槛**：与 Node 版行为对拍一致（M5）+ 生产切换完成——"可用于生产"的承诺点；
- patch（0.1.x…0.3.x）：缺陷修复、文档与小步体验改进（用户反馈驱动的局部补齐，不构成功能域对齐）。

## [0.4.4] - 2026-10-24

### 修复

- **一键启动恒等"OneBot 服务就绪"直至 5 分钟超时**（v0.3.1 引入，v0.4.3 一键启动链放大成全程阻塞）：就绪探测只认桥 config.json 的 `snowluma.httpUrl`，而该键从未被写入（安装脚本只写 homeDir、桥 discover 也不回写）——OneBot 明明监听 127.0.0.1:3000（WebUI 显示正常、TCP 实测可达），卡片恒显"未就绪"、桥始终不被拉起。桥本体不受影响的原因是 discover 走 SnowLuma per-account 文件，与客户端探测源两套数据。探测目标改为两级：显式 `httpUrl` 优先（尊重用户手填），缺位回落读 `config/onebot_<uin>.json` 的 httpServers[0]（与桥 discover 同源）；顺带修 `httpUrl` 写 `localhost` 恒假阴——SocketAddr 只认 IP 字面量，原实现直接 parse 必败，现走系统解析。
- **点"从 Node 版迁移"报"未找到迁移脚本"**（v0.3.1 引入）：客户端按钮硬依赖安装目录随包的 `migrate-from-node.ps1`——新装链与发布包都带它，但两条升级链（upgrade.ps1 手动升级、自动更新 apply-update.bat）都不补，早期安装后一路升级上来的目录永远缺。两条链都已补上（存在即拷，ps1 无运行锁）。

## [0.4.3] - 2026-10-24

### 新增

- **SnowLuma 一键代为启动**：A 档授权原文明确含"在用户本地安装、启动和运行"（此前按授权下达前的旧裁决自我设限）——一键启动检测未运行时代拉官方 launcher.bat（官方形态运行、不注入不修改）；QQ 登录与 OneBot 开启仍需在其窗口/WebUI 完成；停止/重启不做（授权未提及，从紧）。
- **SnowLuma 迁入安装目录**（A 档："与 Node.js 一同部署到客户端目录"）：upgrade 检测 homeDir 在安装目录外时询问迁移（默认迁：停锁目录实例 → 移动 → config 更新；已有副本仅指过去；可保持原位）——安装/启动/数据归拢一目录。

### 修复

- **客户端"检查更新"必报 `res is not defined`**（v0.4.1 引入）：直查改造把 `const res` 挪进浏览器回落分支的块级作用域，漏删旧检查行 `if (!res.ok)`——Tauri 客户端走直查分支时该变量未定义必抛 ReferenceError（GitHub 请求本身成功，死在结果处理上）；浏览器回落路径 res 有定义而正常，掩盖了三版。已删残留行并用 Node 提取函数真跑五分支场景验证。
- **SnowLuma 状态假阳**：探测命令行含 homeDir 明文，双开客户端时互相命中对方探测进程致 UI 假显示"运行中"（真机抓到）——匹配排除 powershell 自身。
- **浏览按钮闪 PowerShell 窗**：CREATE_NO_WINDOW 只隐藏控制台宿主，WinForms 对话框不受影响（原"对话框须可见故不加"为错误推理）。

## [0.4.2] - 2026-10-24

### 新增

- **迁移目录改系统对话框选择**：总览页迁移区块加「浏览…」按钮（WinForms FolderBrowserDialog，与安装器同款、零新依赖），迁移主按钮升为实心。
- **模型切换改级联下拉**：设置页 DSH 三项从手填输入框改为 select——提供方（目录列出）→ 模型（按提供方过滤、无思考档位模型标注）→ 思考强度（按所选模型目录声明收敛，路由别名只剩 none——从根源杜绝选不支持档位致 selectModel 连锁失败）；保存即热生效；桥离线时占位提示手填 config.json 兜底。

## [0.4.1] - 2026-10-24

### 修复

- **桥因 SnowLuma 缺位启动即崩**（真机日志曾积 5 条同款 panic）：discover 从失败即 panic 改为 30 秒等待重试（无限循环、每轮重扫账号配置）——升级重启/开机自启常早于 SnowLuma，此前每次启动死在同一步；账号未开 WS（OneBot HTTP 可用但 wsServers 缺失）纳入同一路径，补开 WS 后自动继续。
- **检查更新依赖桥在线的死锁**：客户端新增 `check_update_direct` 直查 GitHub（复用桥的版本比较逻辑），UI 优先客户端直查、浏览器直开控制台页回落桥端点；桥挂了升级入口恰恰必须可用。发现新版但桥未运行时明确提示"先启动桥再点一次检查更新即可开始下载"。

## [0.4.0] - 2026-10-24

Node 版功能对齐大版本：12 个功能域补齐（两轮按序 + 长尾），桥从"大脑完整"推进到"功能面基本对齐"。MCP 工具面 11→18。

### 新增（功能域）

- **人格卡完整消费链**：唤醒 prompt 注入角色名（current-role.json，perConv 按会话覆盖）、`qq_get_prompt` 返回全文卡（小节按〔一代〕/〔二代〕标记筛选、一代指令兜底过滤、回复示例空格转逗号、名字净化防路径穿越）；设置页人格卡下拉一键切换即生效。
- **表情包完整链**：唤醒 prompt 注入策略与常用摘要（跨会话"资料非指令"安全框定）；查/看图/记认知/发送 4 工具；**QQ 收藏同步**（fetch_custom_face_detail 三关校验防空数组清库、AI 认知保留、complete 才清理已删）——本地库由迁移静态快照变为活性数据。
- **黑话注入全链**：每条投递带当前时间行 + 已确认黑话表（HTML 转义 + CQ 码中文化防提示词伪造）；库损坏原文另存 `.corrupt-*` 拒绝写入（词条不可复现）；查询/提交工具。
- **413 历史超限退役重投**：识别 → 退役 DSH 会话（桥侧记忆保留）→ 3s 重投 → 10min 冷却防循环；重投绕频率判定（失败回合已退配额，再吃频率窗会把修复死锁在冷却里）。
- **瞬时错误退避重投**：503/限流类 8s/20s 静默重投最多 2 次（绕频率同上），用尽才亮群友；成功回合清零。
- **会话管理 selectModel 生效链**（此前完全缺失）：投递前按签名缓存钉选 provider/model/effort；effort 为 none 不带字段（路由别名模型）；不支持档位自动降级去档重试（Node 旧日志 112 条同错根因）；配置热更后下一条消息重选；模型目录与会话重置端点；设置页 provider/effort 输入。
- **花费记账面**：DeepSeek 官方口径计价（三桶分计、峰谷 2 倍、2026-2027 节假日表）；总览花费卡片（累计金额/近 24h/会话榜点击下钻回合明细）。
- **看门狗**：客户端 30s 探测三件套（桥用控制台口 TCP 探活专抓"进程活着但 HTTP 卡死"），防抖判挂、告警直发管理员私聊**不经桥本身**；重启不接管（SnowLuma 授权边界）。
- **活跃成员统计**：`qq_get_active_members`（近期谁在说话/条数/管理员标注）。
- **工具调用审计**：mcp 转发层 call/result 流水（token 打码），桥查询/清空端点（仅控制台），与 Node 版流水同文件同形状可续读。
- **迁移入口 UI**：总览页「从 Node 版迁移」区块（随包 migrate 脚本、输出回显）。

### 修复

- 客户端 UI 周期性冻结（WMI 组件查询占主线程 → async + spawn_blocking；轮询 10s 且窗口隐藏暂停）；轮询闪终端窗（CREATE_NO_WINDOW）；浏览器直开控制台页 JS 判空。
- upgrade.ps1 MCP 注册幂等（pattern 不含 queqiao 致每次升级追加重复块，真机曾积 4 块；现全局替换去重）。

### 定界说明（无需迁移项）

- web 搜索 / PC / 平板远控：preset 生态独立 stdio server（DSH 直接 spawn），与桥零耦合，Rust 桥切换后照常工作。
- 语音（qq_speak 系统 TTS）：桥侧功能、默认关闭，延后到 0.4.x。

## [0.3.4] - 2026-10-24

### 新增

- **升级收尾自动重启**：检测到鹊桥在跑则询问（回车默认执行）退出全部旧进程并启动新版——**严格恢复升级前状态**：客户端+桥都在跑则一并恢复；桥被手动停了则重启客户端但桥保持停止（一次性标记 `state/bridge-autostart` 传递，客户端读后即删，平时正常启动默认拉桥不变）；只有桥在跑则只重启桥；都没跑则不启动。启动失败有兜底提示（进程已退不能让用户面前全停）。

### 修复

- 发布包内说明文件改英文名 `INSTALL-NOTES.txt`（PS5.1 Compress-Archive 不打 UTF-8 文件名标志，原中文名"安装说明.txt"在非中文系统解压乱码；内容仍为中文）。

## [0.3.3] - 2026-10-23

### 修复

- **发布包 upgrade.bat 报 `could not find Cargo.toml`**：`scripts/upgrade.ps1` 的发布包布局检测残缺——`$shipped` 消费端在、定义端缺失（v0.2.1 加布局检测时未完整同步到升级脚本），且第 1 步仍无条件 `cargo build`。现与 install.ps1 同款：zip 布局（脚本根 ≥2 个 exe）直接复制、零 Rust 依赖。连带修同文件两处：`Read-Host` 无 null 防护（PS5.1 非交互 `.Trim()` 崩溃，v0.2.1 同类坑未同步）；`Test-Path (PatchFile)` 缺 `$` 的命令调用错误（此前一直被构建失败掩盖、从未执行到）。模拟 zip 布局真机全链验证通过（跳过构建/复制/MCP 注册/启动指引）。
- **升级须手输安装目录**：upgrade 自动探测已有安装——四级链（HKCU 注册表登记 → 运行中进程路径 → 桌面快捷方式 → 常见位置），候选须含 `queqiao.exe` 才认，都无才交互询问；install 收尾写 `HKCU\Software\QueQiao\InstallDir` 登记（存量安装无登记，靠进程/快捷方式覆盖）。真机验证命中运行中客户端所在真实目录。
- **升级覆盖运行中 exe 撞锁**：探测靠运行中进程定位安装目录、复制却撞同一进程的写入锁（Windows 运行中 exe 不可覆盖只可改名）——直拷 `IOException` 时旧文件转 `.old` 再落新版（与桥自动更新 apply 同技巧），运行中进程不受影响、重开即新版；`.old` 静默清理，仍被占用则下次升级再清。真实锁场景（运行中副本占文件）验证交换三态全过。

## [0.3.2] - 2026-10-23

### 修复

- **自动更新不覆盖客户端**：apply 清单只交换 `queqiao.exe` / `queqiao-mcp.exe`，`queqiao-app.exe` 不在其中——桥自动升级后界面停在旧版，总览页/一键启动/检查更新等 UI 永远不更新。现客户端一并交换（运行中 exe 可 rename，Windows 锁删除不锁改名；客户端退出后重开即新版），`.old` 清理同步覆盖三个 exe。
- **更新检测只查一次**：仅在桥启动时检测，长跑不重启的桥永远发现不了新版——改为启动查一次 + 之后每小时轮询（tag 去重防重复下载，失败静默）。

## [0.3.1] - 2026-10-23

### 新增

- **客户端总览页 + 一键启动编排**：控制台分「总览 / 设置」两页签。总览页含三件套状态卡片（DSH / SnowLuma / 桥）、一键启动（**DSH** 未跑自动拉起——定位 `LOCALAPPDATA\Programs\DeepSeek*` 主程序独立运行；**SnowLuma** 按 EULA §5.4 授权边界只探测不代劳——未就绪时打开所在目录提示双击 launcher.bat，轮询 OneBot HTTP 口就绪；**桥**等依赖就绪后自动起，避开 discover 阶段 SnowLuma 缺位即退出的行为）、会话概要表（未读 / 近期消息 / 唤醒模式实时读控制台 status）、常显运行日志。配置表单收进设置页签。
- **检查更新入口**：总览页按钮 + `POST /api/update/check`——立即查 GitHub latest release，发现新版弹出版本号与更新内容，并汇入既有自动下载 / 空闲应用链路（应用时机仍全自动，不做手动强制）。
- **版本号常显**：客户端经 `app_version` 命令自报版本（桥与客户端同版本发布），不再依赖桥在线。
- **安装器 DSH 基址探测**：试默认 3080 不通则扫 DeepSeek/dsh 进程监听口逐个验证（与桥 port.rs 同源逻辑）；生成 config 直接写真实基址（原硬编码 3080）；已有 config 的 baseUrl 探活失败且探得可用口时自愈替换；DSH 未运行时安全降级（默认 3080 + 桥启动自动扫提示）。
- **SnowLuma 安装位置与回填**：全新下载装进用户所选安装目录（原固定 `%USERPROFILE%\SnowLuma`）；已有 config 空白 homeDir 自动补填本次检测到的路径（文本级插入/替换，防 200 键 JSON 往返重排，非空不覆盖）；已有 SnowLuma 部署不自动移动（可能正被运行或被 Node 版桥引用）。

### 修复

- **桥启动 panic**（第 4 次 runtime-in-runtime）：`OneBotHttpClient` 构建（reqwest blocking client）一直在 `run()` 异步函数体内，debug 构建实测必崩、release 侥幸存活（tokio 1.54.1 安全补丁后 drop 语义趋严）——挪入同步 discover 阶段构建、经 `Setup` 传入。
- 浏览器直开控制台页 JS 静默中断（`window.__TAURI__` 未判空）；桥内嵌控制台页与 Tauri 客户端页原为两份 HTML 存在漂移隐患——单源化（共用 `queqiao-app/ui/index.html`），浏览器模式自动降级提示。
- 控制台页加载容错：桥未连接时状态栏明确提示，不再整页空白。

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
