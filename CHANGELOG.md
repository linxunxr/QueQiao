# 更新日志（CHANGELOG）

本仓库的版本记录。格式参照 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本遵循语义化版本（SemVer）。

## 版本策略

- **0.x 阶段**：开发中，API 与行为可能随版本变化（SemVer 对 0.x 不要求兼容承诺）；
- **minor（0.2.0、0.3.0 …）**：功能域对齐（如记忆域、工具面补全、控制台与看门狗）；
- **1.0.0 门槛**：与 Node 版行为对拍一致（M5）+ 生产切换完成——"可用于生产"的承诺点；
- patch（0.1.x…0.3.x）：缺陷修复、文档与小步体验改进（用户反馈驱动的局部补齐，不构成功能域对齐）。

## [未发布]

### 修复

- **主循环卡死风险四连修（2026-10-31 卡死事故的超时审计）**：探索证实主循环存在多条无上限阻塞路径，任一触发即全桥停摆（配合本批心跳观测，复发时可定位到段）。①**投递总帽 180s**——主循环 5 处 `executor.execute` 调用统一走 `execute_capped`：稳态单次投递 30s 封顶，但新会话/重建链串联多次 HTTP（401 重试翻倍）无总帽即卡死病灶；超时放弃本轮剩余动作（后台投递自然完成，回复可能迟到不丢），主循环恢复推进，`[stall]` 日志定位。②**更新下载出主循环**——`update_rx` 分支原来内联 await 下载（单次 300s 超时 × 主备源 3+3 重试最坏半小时，期间 diary/热更/消息全停），改独立任务 + 回报通道（slang_research 同模式），下载段 in-flight 打点（health 可见）。③**dsh_catalog 解放 worker**——async handler 里直接跑 reqwest blocking（最坏 60s）占死 runtime worker 线程且持 dsh 锁令 executor 投递干等，改 `spawn_blocking`。④**WS 双侧超时**——OneBot 事件流新增 `recv_event_keepalive`（空闲 120s 主动 ping 探活、30s 无回音断定半开走既有重连退避——消灭 NAT 超时/断网无 close 帧时 recv 永久 pending 的「桥活着但收不到任何消息且零日志」）；DSH mux `send_text` 包 30s 写超时（对端停滞/缓冲满时 sink 发送可无限挂起，MuxRunner 静默）。

### 新增

- **主循环心跳与 in-flight 观测（2026-10-31 卡死事故的观测闭环）**：主循环 6 处内联 `executor.execute().await` 无总帽，任一环节挂起即全桥停摆（diary/热更/消息全停、零日志），而看门狗 TCP 探活只看端口（serve 是独立任务）必然误报健康。新增 `watch::WatchState`（500ms tick 心跳 + 投递段 begin/end 打点）：`/api/health` 从恒 "ok" 升级为 `{ok, mainLoopAlive, heartbeatAgeMs, inFlight}`；`/api/diagnostics` 增 watch 段。客户端看门狗探活同步升级——手写极简 HTTP GET /api/health 解析 mainLoopAlive（零新依赖；200 但非 JSON 的升级期旧桥回落 TCP 语义防误告警），健康判定区分「忙/死」：心跳在=活；心跳停但有 in-flight 段且 <5min=忙不告警（慢投递是新会话链正常上限）；心跳停且无在忙段=死（真卡死才告警）。复发时抓线程栈的运维手段：`procdump -accepteula queqiao.exe`（Rust 无便携跨线程栈转储，观测定位段落后手动抓）。

- **控制台 API 鉴权补全——/api/config 与 /api/logs/tail 此前零守卫（v0.4.8 起记录的缺口）**：GET /api/config 可读全部配置、PUT 可改 ownerQQ/白名单，且 CORS 全路由放行意味着用户浏览器里的任意网页可跨域读——与 v0.4.16 防注入想堵的「接管控制台」同款后果，攻击者从 AI 浏览器换成本机任意进程/恶意网页。三层修复：①`consoleToken` 缺位时启动自动生成密码学随机令牌（24 字节 → 48 hex）并写回 config.json——写 config 而非独立文件，保住 queqiao-mcp 从 config.json 读令牌的既有链（Node 版存 state/console-token 的做法在这里会断 MCP）；②守卫判定抽公共函数 `console_token_check`（agent 令牌头一律 403——MCP 双令牌都带，仅拒 at 才挡得住 AI 经工具读写配置；令牌非空失配 401；读活值热更即时生效），/api/config GET/PUT 与 /api/logs/tail 三端点接入，tools_api `console_only` 同源复用；③UI 全部桥 fetch（29 处）统一经 `apiFetch` 注入令牌——Tauri 模式新增 `console_token_read` 命令直读 config.json（与 MCP 同源），浏览器直开模式 401 时 prompt 输入，localStorage 缓存；401 自愈单飞（并发 401 只弹一次 prompt、在途旧令牌响应直接带新令牌重试——playwright 实测暴露并修复的 prompt 风暴）。向导模式令牌为空天然放行（首次配置的唯一写入路径不被锁死）；页面壳 GET / 不鉴权（无敏感数据）。**升级后需重启 DSH 拉起新 MCP**——存量 MCP 进程启动时读到的令牌为空，不重启则工具调用全部 401（v0.4.15 同款发版须知）。

## [0.4.17] - 2026-10-31

### 修复

- **DSH 重启后 AI 对一切消息静默无反应——mux 断连残留回合态令会话永久 busy（272246e）**：真机事故链：DSH 重启 → mux 断连（进行中回合的 turn/end 事件随连接丢失）→ 会话 turn/prompt_in_flight 残留 → is_busy 恒真 → 之后所有唤醒进 busy 分支被静默吞掉（无「计划唤醒」日志、无「频率超限」日志、wakeTimes 不记账）——消息照常入库但 AI 永不响应，诊断特征为 lastIncomingAt 持续更新而 unread 堆积、唤醒账本零新增。修法：mux 任务断连分支经 unbounded channel 通知主循环 → `orchestrator.on_mux_disconnect` 清全部会话进行中回合态（abort_inflight），持久态不动、清理量打日志；守护测试钉住 busy→清→不 busy。
- **升级/安装脚本把 MCP 注册写成空 command——AI 会话挂不到 qq 工具、唤醒空转无回复（31cd433）**：`Write-McpPatch` 调用传的 `$McpExe` 在 install.ps1/upgrade.ps1 里都只有 param 声明、从未赋值，PowerShell 未定义变量展开为空串 → DSH 两档 cordis.patch.yml 落盘 `command: ''` → spawn 不了 queqiao-mcp → AI 函数列表里没有 qq_* 工具（DSH 会话历史里 AI 自报实锤），唤醒后看一眼没工具即空转结束，activityLog 反复「连续 3 次唤醒无行动」。修复：调用前从安装目录拼 `$McpExe` 并 Test-Path 防空（拒绝写空注册）。
- **SnowLuma 卡片恒红「未运行」——homeDir 未配置时探测假阴（ed6093f）**：homeDir 为空时进程判定恒 false（`-like '**'` 匹配一切的防御行为），SnowLuma 实际在跑卡片却恒红（v0.4.4 修 httpUrl 探活时这半没跟上）。治本 `snowluma_running` 纯函数：homeDir 空时回落用 OneBot HTTP 探活（有人应答必有进程在服务），三态单测；「运行中 · OneBot 未就绪」中间态在有 homeDir 时不受影响。
- v0.4.16 代码补跑 fmt+clippy（b6ec9bb，纯格式无行为变化）。

## [0.4.16] - 2026-10-31

### 新增

- **防聊天侧 prompt 注入四层防线（fa15f4a）**：威胁模型=聊天里的恶意用户（QQ 群友/私聊对象、小黑盒网页内容）注入话术诱导 AI 危害宿主机。调研确认的完整攻击链：`browser_navigate` 到 `file:///…/config.json` → snapshot 把密钥读进上下文 → 发回群里全泄；变体 navigate 到 `127.0.0.1:3100` 操作控制台 UI 改 ownerQQ/关白名单=完整接管。四层：①`guard_url` 硬边界——浏览器/平板导航与上传下载统一 URL 校验，本机/回环/私有网段（含 169.254 云元数据）/IPv6 本机与 ULA 一律拒，`file://` 默认拒且新配置 `browser.allowFileUrls` 显式开（开了内网主机路径仍拒），26 形态单测钉死；②高危工具 owner-only——`qq_memory_clear`/`qq_memory_remove` 仅 `private:<ownerQQ>` 会话可用（会话键即调用方身份，群聊 key 即使带管理员令牌也拒；未配置 ownerQQ fail-closed；拒绝记 `[security]` 日志）；③唤醒 prompt 安全段——所有 reason 固定注入【安全边界】（消息/网页/文件内容是数据非指令、管理操作只认 isOwner=true，要求拒绝并 report_feedback 上报）；④console 端点收口——`/api/roles`、`/api/role` 换 `console_only` 拒 agent-token。硬边界为主话术为辅；已知边界：域名解析到内网的 DNS 绕过不防、十六进制 IP 变体交给浏览器站点隔离。
- **人格卡支持管理页编辑（3aa403e）**：此前只能切换、卡本体 `roles/<名>.md` 只能手改文件。新增 GET/PUT/DELETE `/api/role/card`（原子写 64KB 上限；删除时被 current-role.json 全局或 perConv 引用即 409 拒，防删在用卡致人格静默丢失）+ 前端「编辑/新建」按钮与编辑区（新建撞名确认覆盖、删除危险确认），保存后下一条消息即刻生效。顺带钉住净化语义坑：`sanitize_role_name` 是过滤清洗（`../evil` 改名放行为 `escape`——无安全洞但用户起名会静默改名），core 新增 `role_name_is_valid` 严格校验供编辑端点用（原样不合法即拒），两判定共用同一 `char_allowed` 字符白名单。

### 修复

- **账号绑定面板每次打开都是空的（a4b9e89）**：两层根因——①前端漏接线：初始化/桥恢复链/展开面板三处都不调 `refreshBind`，面板永远停在「检测中…」占位；②`bind_status` 浏览器未运行直接短路报未登录，而专属浏览器懒启动+闲置 30 分钟自动退出，打开面板几乎必是离线态（UI「登录态保留」文案自迁移起是不可达死代码）。修法：新增绑定快照 `state/browser-bindings.json`——在线确认登录时落盘（confirmedAt+昵称头像，5min 节流），离线回放报已登录+上次确认时间（前端转相对时间），在线实测未登录/解绑即清不谎报。登录态本体一直在 browser-profile 没丢，纯「看不见」。未走离线读 profile Cookies 路线：加密 SQLite 一堆坑且准确性不优于快照。
- **设置页「桥未连接」假死三连修（36bbe93）**：真机病征=开机自启后设置页人格/provider/model 全挂占位，而桥进程一直健康——UI 卡死在启动瞬间的失败占位永不自愈。三层：①`probe_components` 的 PowerShell/WMI 调用无超时，开机 WMI 不响应把 UI tick 轮询链整个挂死——改 5s 超时+kill；②恢复链只信 WMI 进程探测——`refreshComponents` 补 HTTP 探活兜底（`/api/health` 通即视在线、恢复链秒级触发，顺带修通浏览器直开模式恢复链从不触发的旧问题）；③设置页数据获取不再耦合桥进程——`load`/`loadRoles` fetch 失败降级 Tauri 直读文件（`config_read_direct`/`roles_read_direct`，形状与 API 一致），桥没跑照常显示；「保存」仍走桥（热更即时生效+单写者防并发写坏）。

## [0.4.15] - 2026-10-30

### 修复

- **日记断更 8 天——Rust 版迁移漏接调度环，补齐 Node 版三路径全链（8d9c2be）**：真机定位 10-23 晚生产由 Node 切 Rust 后再无日记——core 有判定纯函数但 bin 从未接线：无 60s 轮询、无启动补写、无宿主挑选，`diary:*` 唤醒永不发生。对齐 Node scheduleDiaryV2 全链：主循环 60s 一拍（定点/安静提前；跨会话全局 lastActivity、60min 冷却）+ 启动 10min 后一次性补写昨天 + 宿主挑选（48h 最活跃在册会话）+ 唤醒素材块（各会话当天动静摘要 + 近 3 篇日记锚点 + 补写/常规文案区分）+ 写入日期桥锁定（补写=昨天、凌晨 0~4 点归昨天，模型无法指定日期——补写回合不再写歪成今天）。顺带清理 core 不可达的 DiaryTick 死分支与 written_today 等字段；write_diary 端点改走 core build_diary_entry 统一截断/trim/同日拒契约。
- **三处同款「core 有、触发源无」迁移漏接线——回复检查唤醒/有限潜水闹钟/错误亮群友（5fdeec6）**：日记断更排查的系统性复查（对照 Node 全部定时器 vs Rust 接线）又抓三个：①replyCheck 零触发——AI 发完话就潜水，自己挑起的话题没人 @ 永远接不上；②set_wake_config 的有限潜水闹钟（SleepUntil）只落盘从不排定时器（core 消费分支自迁移起是死代码）——「潜水 N 小时后叫我」永不响；③agent 出错重试耗尽时的错误提示动作只有 println 占位——出错时 AI 静默消失、群友无感知。三处全接线：发送端点统一回执点报 ReplyCheck（问句 ×1.5、15s 内连发 ≥3 条 ×1.3 自适应延迟，unbounded mpsc 转交主循环排程）；set_wake_config 报 WakeConfig 主循环重排/取消闹钟，且比 Node 更稳一步——启动时恢复未过期的持久闹钟（Node 重启即丢）；唤醒接管时取消在排的 ReplyCheck/SleepUntil 防旧闹钟补刀；Executor 补 OneBot 通道真实投递错误提示。已知未动：paused 时消息触发唤醒不挡（Node 统一挡，Rust 仅 proactive 挡）——影响面另议。
- **工具 schema 两类非法结构致智谱 400 [1210]，GLM-5.3-Flash 自 v0.4.13 起真机全量降级 MiniMax（13c223d）**：`input_schema()` 无条件把公共参数（conversationKey/agentToken）chain 进每个工具，与 browser/tablet 等工具手写的同名参数叠加——33 个工具的 required 数组出现重复元素；另有 `qq_sticker_note.tags` 自造 `type:"json"`（JSON Schema 无此类型；且模型按 json 传字符串时桥端 as_array() 解析不出、tags 静默丢失——本条顺带修复）。智谱是唯一严格校验 tools schema 的上游（required 唯一性 + type 合法性），违规直接 1210；MiniMax/DeepSeek 宽松放行，故 omniroute 高可用组合第一位的 GLM 全量挂、顺位降级 MiniMax（10-25 起累计 2300+ 次降级，单日峰值 1063；DSH 会话创建时快照工具 schema，旧会话全部中招、此后新会话才带毒——这也是故障按会话 100% 聚类的原因）。修复：chain 前过滤已手写的公共参数（一处覆盖 33 工具）+ tags 改 array（桥端本按字符串数组解析，零兼容成本）；新增守护测试 `tool_schemas_are_valid_json_schema`（required 唯一 + type 合法，防再犯）。真机验证：生产 config 56 工具 3/3 过、门禁全开 60 工具过。**升级后需退役重建 DSH 会话**——旧会话快照的毒 schema 不随 MCP 更新自愈。

## [0.4.14] - 2026-10-30

### 新增

- **国内双源更新（Gitee 镜像优先 + GitHub 回落，主包经 gitee-sync 云函数中转）**：更新器查询改 Gitee 镜像优先——源选择语义上"仅 Gitee 明确报有新版才采用"（镜像可能落后或未配置，Gitee 说没新版不可信，一律再问 GitHub），Gitee 命中后顺手查 GitHub 同 tag 资产作备源。下载重试链升级为主源 3 次失败自动换备源再 3 次（下载失败不回退是 tauri-plugin-updater 的硬伤，同类项目真机事故教训——Gitee 附件同步滞后时更新不再失败）。发版链路同步自动化：release.yml 末尾新增云函数中转步（GitHub Release 主包 → 香港 Region 云函数下载 → Gitee 发行版附件 + CHANGELOG 正文透传，secrets 未配置则跳过不阻塞发版）；只镜像主包——Gitee 单附件上限 100MB，browser 352MB 资产不镜像。另附 `resync-gitee.yml` 补传 workflow：发版后同步失败或镜像 tag 滞后时单独重触发（17 秒级，不重跑 8 分钟构建；云函数幂等，附件已存在跳过）。
- **`resync-gitee` 触发口令不对齐时的显式报错**：与 release.yml 的静默跳过不同，手动补传是显式意图，secrets 缺配置直接 error 让人看见。

### 修复

- **专属浏览器死会话致账号绑定失败且重试无效（4f6cba4）**：绑定拉起有头 Chrome 后手动关窗，Session 缓存残留（闲置看门狗未到点、感知不到进程死），再点「绑定/登录」对已关闭 WS 发 Page.navigate 报 "CDP 发送失败：Trying to work with closed connection"，且 bind 不走 op() 的失败丢弃路径、重试也无效。修复：ensure() 复用会话前补 child.try_wait() 存活检查（死即丢弃重拉，一处覆盖全部非 op 路径）；bind/show/unbind 改走 op_with（参数化自 op()），获得 reconcile 页面接管与失败丢弃的自愈语义。集成测试新增死会话自愈链（json/close 关唯一页 → 等进程退出 → bind 自动重拉）。

## [0.4.13] - 2026-10-29

### 新增

- **专属浏览器发帖/评论能力补全（browser_key 按键 + browser_upload 传图 + 输入 focus 自愈 + 快照操作栏前置）**：真机评估坐实 AI 在小黑盒上"能发帖但评论打不进字、带图帖发不了"——评论框是自定义受控组件（CDP 注入鼠标事件的原生 focus 不生效，insertText 落空）、缺键盘键（回车发送/退格纠错/全选替换做不了）、缺文件上传。四项补齐：①browser_key 走 Input.dispatchKeyEvent（enter/backspace/delete/escape/方向键/home/end/pageup/pagedown，modifiers 组合 ctrl+a 全选后输入即替换，也用于清空；文本键带 text、命令键 rawKeyDown，Puppeteer 同款语义）；②browser_upload 收 http(s) 图片/视频直链自动下载到桥缓存（魔数白名单 PNG/JPEG/GIF/WebP/WebM/MP4 + 25MB 上限，防错误页混入）或鹊桥目录内本地路径（canonicalize 前缀校验，防任意磁盘文件被诱导外传）→ DOM.setFileInputFiles 填进页面第 index 个文件框；③browser_type 带坐标时点击后先跑内置 focus 探测（elementFromPoint 向上找最近可编辑元素显式 focus，返回聚焦详情——仍是"固定脚本只插坐标"边界，未开放任意 JS）；④快照两修：可点判定放宽到最近交互祖先（组件库把点击挂父层，事件冒泡保证点子元素坐标行为正确），fixed/sticky 操作栏（顶栏/底部评论栏）改两轮遍历前置输出且先于文档流消耗预算（首版按文档序排队预算，DOM 尾部的底栏在预算耗尽后根本访问不到——集成测试当场抓住）。预设规则 23 补 key/upload 用法与发帖/评论操作流。

### 修复

- **看图工具（qq_get_message_images）上线起 404 全灭（13b71f6 引入）**：MCP 相对 endpoint 统一拼 /api/socialV2/ 前缀，桥却把看图路由挂在 /api/images/message——axum 对未匹配路由回 404 空 body，MCP 的 resp.json() 瞬时报 "error decoding response body" 毫无线索（从上线起全部调用 0 秒失败，AI 只能说"图又挂了"，五天后才定位）。修复：桥补 /api/socialV2/images/message 双路由（旧路径保留，存量 MCP 进程配新桥立即恢复）；新增守护测试遍历全部 MCP 工具按真实拼法打桥路由断言非 404（防两端路径约定再漂移）；顺带给图片下载补 error_for_status（QQ CDN rkey 过期回 403 错误页不再被 base64 成"图片"喂视觉模型，失败原因透传给 AI）。

### 变更

- **控制台 UI 简约现代化（信息架构 + 排版节奏 + 一致性 + 轻量对话框）**：总览页此前一根滚动轴挂 9 个区块，运行监控（状态/会话/日志）与低频回顾（花费/日记/反馈）混在同一层级——"显得不简约"的主因是信息架构而非视觉风格。四项收敛：①花费/AI 日记/AI 反馈三个低频面板独立成「回顾」页签（默认展开，切页补刷一次，不进 tick 轮询的既有约定不变），总览只留运行监控与账号绑定/迁移两类事务；②排版节奏拉层级（h1 17→19px、h2 13.5→14.5px、区块间距 18→20px，此前全站字号挤在 11~14px 一档无呼吸感）；③圆角归一为成文三档（`--r-box` 10 / `--r-mid` 8 / `--r-ctl` 6，此前 5/6/8/10 四值无规则，tag 的 5px 已并入 6），折叠面板标题去掉与 section 标题重复的蓝色竖条（层级信号由 h2 独占，summary 只留箭头）；④原生 `confirm`/`alert` 换应用内轻量对话框与 toast（桌面 Tauri 里原生弹窗样式出戏且阻塞渲染）——危险操作（清空反馈/解绑站点）确认键用与错误色一脉的深红底变体，默认聚焦「取消」，支持 Escape/遮罩关闭；`stopBridge` 的错误改走 toast。附窄窗口兜底：会话/黑话/花费三个长表容器横向滚动，列不再挤压换行。

## [0.4.12] - 2026-10-29

### 变更

- **记忆召回分层（唤醒注入瘦身，qq_memory_query 升级为全量详情）**：此前每次唤醒 prompt 全量注入记忆（话题/想法各 10 条 + 印象 10 人全文，`qq_memory_query` 只是注入内容的复读，查询无增量——记忆可见性 100% 押在注入上，这是"每次都得注入还费 token"的结构性原因）。现对齐业界分层记忆的召回分法（working 层注入、semantic 层检索）：注入只带索引——话题/想法各取最新 5 条正文、印象压缩成一行（共 N 人 + 最近互动 3 人名 + 查询指引），预期记忆块从典型 ~500 字压到 ~120 字；`qq_memory_query` 新增 `category` 参数（activeTopic/pendingThought/memberImpression，缺省全部）返回按类别全量详情（印象全部 50 人、话题/想法全部 20 条，带最后提及时间与过期剩余），"注入=索引、查询=全文"后减注入不丢信息。注入同时去掉相对时间（"X分钟前"——prompt 已有【当前时间】行，分钟级新鲜度模型用不上，只让每次注入字节必变）与想法 motivation 括注。顺修 `format_memory` 印象段按 HashMap 迭代序取人的非确定缺陷（改为按 lastSeenAt 降序）。附带唤醒 prompt 体量记账日志（`[prompt-size]`：total/memory/sticker/slang 各段字节数），跑一周攒真实配比后再裁决下一刀（骨架固定文案与记忆块孰为大头）。预设规则 8 与主动开话题段同步改写（印象全文查询指引）。发版前 preset 需随安装器重新部署到 `~/.dsh/agent-presets/`。

## [0.4.11] - 2026-10-28

### 新增

- **专属浏览器标签页治理（单页纪律 + 预算回收 + browser_tabs 工具）**：AI 刷资讯站时文章链接几乎全是 target=_blank，点一次漏一个标签页且无人回收，慢慢积到几十个拖垮浏览器。三层治理对标 ego-browser 的标签页设计（不换引擎、保隔离）：①注入脚本把带 target 的链接点击改写为当前页打开（同源 window.open 就地导航；跨源弹窗如 OAuth 登录刻意放行不误伤）；②每次操作前自检——活跃页失联自动接管现存页（一页不剩则新开空白页，替代原来丢会话整个重启浏览器）、页面超 3 个自动回收最老的非活跃页；③新增 browser_tabs 工具（列出/关闭标签页，close 异步确认真关完再报数，最后一页保底不关）。预设规则 23 同步补 browser_* 用法说明（操作流/登录态查询/管理员授权限定）。
- **账号绑定卡片展示账号详情**：绑定成功后刮取昵称之外新增头像与等级（localStorage 优先、DOM 兜底，结构化 JSON 返回兼容旧纯字符串），客户端卡片升级为 40px 圆头像 + 昵称 + Lv.等级，并加「个人主页」按钮直达站点个人页。

### 修复

- **更新器把 352MB Chromium 资产当更新包下载（v0.4.10 引入，严重）**：v0.4.10 起 Release 携带双资产，资产选择的 `ends_with("-win64.zip")` 会先命中 `queqiao-browser-<tag>-win64.zip`——更新器把 352MB 当更新包下载（慢网下数小时），下完解压出的还是 Chromium 目录而非三件套，apply 时三件套被改名后拷不进新文件（安装报废，重演竞态事故形态）。修复：精确匹配 `queqiao-<tag>-win64.zip`；带选择谓词回归测试（同时反证旧谓词确实先命中 browser 资产）。**v0.4.10 的在用安装须手动升级到本版**（其自带的更新器已损坏——鸡生蛋问题）。
- **Chromium 单例重启竞态（v0.4.10 浏览器功能引入）**：同一 profile 目录重启浏览器（无头↔有头切换、闲置关闭后再拉起都走这条路）时，垂死旧实例令新进程被 Chromium 单例机制委派后立即退出，残留的 DevToolsActivePort 指向已死端口——表现为"页面目标发现超时"卡 95 秒以上且无明确报错。现重启前显式 kill+wait 旧进程、启动前清残留端口文件、子进程提前退出立即报可操作的原因。

## [0.4.10] - 2026-10-27

### 新增

- **账号绑定（客户端总览「账号绑定」区块 + 站点注册表）**：把"绑个小黑盒账号"做成客户端一等公民——绑定 = 点按钮弹有头专属浏览器直达登录弹窗（内置脚本点开 `.login-btn`，不给模型任意 JS 的边界不破），扫码/验证码后客户端每 3 秒轮询登录态、成功自动变绿并刮取昵称（localStorage 用户缓存 + DOM 兜底）；解绑按 origin 清 cookie+localStorage（`Storage.clearDataForOrigin`，不碰 profile 里其他站点）。桥侧站点注册表（域名/登录页/会话 cookie 键/登录点击脚本/昵称脚本）——加站点=加配置不写代码，小黑盒是首条（会话判定 `pkey`/`heybox_uid`，与逆向社区抓包口径同源）。鉴权分层：bind/bind-status/unbind 走 console 令牌（管理操作），AI 侧另有只读 `browser_login_status` 工具（agent 令牌 + allowConvKeys）——AI 刷网页遇到未登录页可主动报告"登录态失效，去客户端重扫"。
- **专属浏览器（browser 十工具，AI 的独立 Chromium）**：给 AI 一只完全隔离的浏览器——独立进程、独立 profile（state/browser-profile，删目录=登录态清空）、CDP 只绑本机随机端口，用户日常浏览器/注册表/扩展零接触。能力面：导航/快照（DOM 压缩树，坐标+文字+可点标注，坐标直接喂点击）/截图（JPEG 进上下文）/点击/输入（中文 base64 经 CDP insertText）/滚动/后退/等待/登录显示（强制有头窗口，人在窗口里扫码，cookie 自动留 profile）。两层门禁同 tablet：注册期 browser.enabled=false 时九个控制工具不注册（仅 browser_status 探测），调用期会话令牌须命中 browser.allowConvKeys（默认仅管理员私聊）。无头/有头设置页可控（默认有头可监督），切换在下一次浏览器生命周期生效；懒启动 + 闲置自动关闭（默认 30 分钟）。进程内集成测试全链绿：本机 Chrome 无头跑通 导航→快照→中文输入→点击联动→截图 JPEG 魔数。**真机排查两坑入册**：①程序化 spawn Chromium 系浏览器 --user-data-dir 必须等号连写（两参数分离在本机静默卡死：进程活着但 profile 不初始化、DevToolsActivePort 永不出现，bash 终端启动却正常）；②DevTools /json/list 对 Connection: close 不总关连接，HTTP 客户端须按 Content-Length 精确读体（read_to_end 挂到超时）。专属 Chromium 以独立 Release 资产分发（主包不携带，升级不重下），资产发布前开发兜底顺序 browser/ 目录 → 系统 Chrome → 系统 Edge（Edge 无头对位置参数报 Multiple targets 拒启，仅最后兜底）。
- **鸿蒙平板控制（tablet 九工具，规格配套 MCP 落地）**：AI 可以看到并操作管理员的鸿蒙平板——截图（图像直接进上下文，本地留档最近 12 张）/ 读屏（dumpLayout 压缩成坐标+文字+可点可滚的紧凑树）/ 点击 / 滑动 / 按方向快滑（刷视频翻列表）/ 按键 / 输入文本（中文走 base64 解码防 shell 拆参）/ 开网页（仅 http/https）。能力面刻意收敛：不做 tablet_shell 或任意 hdc 命令（平板是主人物理设备，任意 shell 等于整台设备交给模型）。两层门禁与 Node 版同款：注册期 `tablet.enabled=false` 时八个控制工具不注册（不占 system prompt、不给误调机会，仅 `tablet_status` 探测可用）；调用期会话令牌反查后须命中 `tablet.allowConvKeys`（默认仅管理员私聊）。hdc 定位顺序：config 显式路径 → PATH → DevEco Studio 默认安装位置；每次调用重新解析（改 config 即生效）；串行闸防 hdc server 并发拒绝；全部带超时（hdc 卡死不拖垮桥）。真机动作待设备连接后验证（本机 hdc 3.2.0f 就位、设备离线时给出可操作指引）。

### 修复

- **双更新器竞态致换新扑空、客户端消失（2026-10-26 真机事故，根因修复）**：桥自动更新与客户端自主更新是两个独立执行者，却共用同一 staging 目录与同一 apply-update.bat——双触发时一方的"清旧 staging"掏空另一方刚下好的包，apply bat 拷贝扑空：三件套被改名后新件缺失、客户端退出不再回来。现按执行者隔离（客户端 update-staging-app/apply-update-app.bat，桥 update-staging/apply-update.bat）+ update.lock 跨执行者互斥（bat 执行完删除，20 分钟过期防死锁）；apply bat 全程写 update-apply.log（此前每步吞错，失败零痕迹）并加固 cmd 语法（括号块 && 串联与 timeout 在部分环境翻车，改三行独立 if + ping 等待）。
- **更新进度条"不动"观感（表层修复）**：下载失败只弹 toast，进度条区永久停在"准备下载… 0%"像卡死；连接期（首块 64KB 前）零反馈且连接重试最长可等 15 分钟无提示。现失败就地显示原因可重试、连接建立即发 0 进度帧、20 秒看门狗明示"在等首字节"、重试时复位速度基线；浏览器直开的控制台页点"立即更新"给出明确指引（此前抛 TypeError 留下冻住的进度条）。
- **更新面板的更新日志不解析 Markdown（v0.4.9 起）**：Release 正文恢复后，面板此前用 <pre> 纯文本渲染——标题/加粗/列表全部原样显示为井号和星号。改为极简 Markdown 渲染（标题分级/列表/加粗/行内代码/链接，先整体 HTML 转义再变换防注入），面板保持深色样式；以 v0.4.9 真实 Release 正文渲染验证（3 标题 + 10 条列表 + 加粗，无裸标记残留）。

## [0.4.8] - 2026-10-25

### 新增

- **AI 日记查看（总览页折叠区）**：此前日记只有 AI 工具面查询端点，用户无法自看；现展示近 90 天日记（最新在上，折叠头显示篇数与最新日期）。内容按纯文本渲染并统一转义。

### 修复

- **客户端退出后桥无痕死亡（v0.4.3 起）**：桥的 stdout 管道连着客户端，客户端退出/被杀后管道断裂，桥下一次打日志 `println!` 写失败直接 panic，且 panic 记录本身也要打印（二次 panic）——文件日志零痕迹、进程无声消失。日志改为文件先行 + stdout 容错；serve 任务退出原因落文件日志（原只写 stderr 无人捕获，控制台端口静默消失无从排查）。
- **总览"会话概要"恒显"暂无会话"（v0.3.1 起）**：UI 调错端点（想要会话列表却调单会话 status，其要求 conversationKey 无参必拒）；新增会话列表端点，真实会话的未读/近期/唤醒模式正常显示。
- **总览"运行日志"打开即空**：日志区纯靠实时事件流，桥先于界面启动、启动期日志必错过；新增日志尾部回放端点，界面打开与桥重启后自动补齐最近 150 行。
- **设置页人格卡恒"桥未连接"**：加载函数只在页面初始化调一次，桥未就绪即错过且无重试；桥上线自动补调。
- **selectModel 档位被拒即整轮失败**：降级重试条件补上 1210（Invalid API parameter——路由不认档位参数的另一形态），自动去档重试。
- 设置页档位下拉补 off（DSH 实际档位 off/low/high/max，此前 off 被过滤选不到）。

## [0.4.9] - 2026-10-26

### 新增

- **AI 反馈查看与闭环（总览页折叠区）**：此前 AI 用 qq_report_feedback 上报的问题只落文件、无处可看（闭环断裂）；提交侧补齐 Node 全语义（level 三档/限频 5 分 20 时/maxLength 截断/入库前敏感打码），控制台新增读/清端点与「AI 反馈」折叠区。
- **主动冒泡（proactive）接线**：core 有定时器与处理器、config 有整套参数，但从未有人调度——AI 永不主动开话题。现启动全量排程 + 触发后自重排（检查环常驻），判定改配置驱动（空闲阈值/概率/间隔），暂停期间不自主冒泡。
- **收藏表情看图**：qq_get_sticker_image 此前只回 URL（QQ preset 的模型看不了），现返回图片本体转视觉 image 块（魔数判格式、失败降级元数据）。
- **黑话自动提取管线（第二期）**：候选从此有自动来源——每会话滚动窗口攒群友普通消息（上限 80 条，/命令与自己发言不入窗），攒够 extractMinMessages（默认 10）且过冷却（默认 5 分钟）即派发独立 DSH 学习会话（learner preset、独立映射文件/工作区，preset fail-closed 与 404 重建由会话管理器承担）提取候选；回合全文经 mux 回流收集（观察者先于投递注册、mux follow 并入学习会话映射、投递前通知重连等跟随建立），解析用 Node 同款括号配平抠数组（防解释文字+[1] 干扰静默丢结果），同词条 count+1 合并去重证据不覆盖已有释义；busy 串行防并发写库，失败保留窗口稍后再试。extractMinMessages/cooldown/learnerPreset/workspaceTitle 四个配置从死字段转正。
- **黑话联网研究管线（第三期，功能域收官）**：候选释义不再靠人工——出现次数命中阈值（默认 2/4/8，可配）且超上次研究水位时，提取任务链式送独立学习会话联网考究（web_search/web_fetch 由 learner preset 侧工具承担，无工具时按语境降级 confirmed=false）；也可在词库页候选面板手动「送研究（选中）/研究全部候选」。研究结果仅 confirmed=true 的写回释义/用法/示例/风险/来源并记水位（**status 不动——转正仍须管理员确认**）；inferenceThresholds/autoResearch 两配置转正。
- **黑话词库审核面（控制台第三页「黑话」）**：此前 AI 用 qq_slang_submit 提交的候选词条永远停在 candidate、无任何转正途径（闭环断裂的核心）；新增 /api/slang 管理端点八件（列表/手动添加/编辑/批量确认/批量拒绝/单条捷径/批量删除/清空，agent token 一律 403），core 补批量状态流转纯函数（确认须 candidate 且有含义，拒绝可下架已确认）；UI 三态页签（候选/已确认/已拒绝）+ 批量操作 + 搜索 + 手动添加 + 编辑面板 + 注入参数（enabled/注入上限走 config 深度合并）。进程内集成测试（build_router 抽取 + tower oneshot）九步全链 + Playwright 浏览器全流程验证；顺手修掉黑话页初始化的 TDZ 隐患（showTab 先于 bridgeOnline 声明执行）。

### 修复

- **AI 日记/反馈/黑话页恒显"桥未运行"（7369cb3 引入，确定性必现）**：tick 循环里 bridgeOnline 置位在"桥转在线"翻转分支**之后**——翻转分支调用 refreshDiary 等低频面板时全局标志仍为 false，面板内部按它门禁直接渲染"桥未运行"返回；而日记/反馈/黑话不在每 10 秒常规刷新清单（低频回顾面，防重渲染打断阅读），错误状态永久卡住，桥重启再翻转又踩同坑。修复：标志置位先于翻转分支（wasOffline 局部快照判断），tick 与 visibilitychange 两处同改；mock 桥 + Playwright 模拟"桥转在线"时序验证日记渲染真实内容。
- **更新面板恒无更新日志（v0.1.0 起全部版本）**：release 工作流上传产物时从未给 Release 写 body，更新检查（桥/客户端）读到的发布说明恒为空串——面板只剩版本号和日期。组包步骤现从 CHANGELOG.md 提取本版本段落（新增/修复明细）写入 release-body.md 并随上传带上；下次发版起生效，历史 Release 的 body 需另行补写。
- **发送链敏感审计缺失（安全）**：v2 工具发送此前只有白名单+频率两道闸，模型被诱导可把本机路径/凭据特征发进群；现移植 Node SENSITIVE_RE（命中即 403 整条拦截）+ 已知 agent token 出站打码，正则与 Node 原版 21 用例对拍一致。
- **死配置接线与误导默认值**：deny 黑名单/allowAllWhenEmpty 此前全库无消费（现统一准入判定 + allowAllWhenEmpty 动态注册会话）；pricing.models 自定义价目表现在生效（换中转模型费用折算不再走兜底价）；连发间隔改吃 send 节配置（长间隔概率/区间/maxGapMs，此前硬编码 600-1500ms）；voice.asrEnabled/autoReadback 未实现改默认关；slang 学习参数等未消费字段补注释标明。

## [0.4.7] - 2026-10-25

### 修复

- **设置页保存必报"JSON 解析失败"（v0.4.3 起 UI Tauri 化即带）**：桥的 CORS 中间件只挂在工具 API 路由上，`/api/config` 所在的控制台路由 merge 后没有覆盖——客户端 webview 跨域拉配置被浏览器拦截，设置页恒空表单、保存解析空串报错；同页其他面板正常，长期被掩盖。现在 merge 后统一再盖一层 CORS 全路由覆盖。
- **设置页三层兜底**：保存时 JSON 基底为空/损坏则先从桥现拉配置重建再续存（表单已填值不丢）；轮询检测桥从离线转在线自动补调配置加载（原只在页面初始化时调，启动竞态错过即永久空表单）；加载回填加判空防御。拉不到配置时报错文案改为指向真因（"桥未连接——请先启动桥再保存"）。

## [0.4.6] - 2026-10-25

### 新增

- **二代仿真工具面全量对齐（18 → 45，与 Node 版逐一对拍补齐）**：P0 互动七件（引用回复/连发/拍一拍/表情回应/撤回/看图/合并转发）；P1 九件（偷图收藏/URL 入库/备注、日记读写（每日一次，格式兼容既有数据）、群成员/群档案/群历史、Q 版形象）；收尾十一件（等待新消息（静默判定两态）、会话状态摘要、群列表、反馈上报、语音三件（TTS 合成/语音库/清单，默认关）、发送变体、自己最近消息）。全部带频率护栏（每分钟 8 条/每小时 60 条，超限 429）。
- **客户端自主更新（桥挂了也能升级）**：更新面板（新版本号+发布日期+更新日志全文）、实时下载进度（速度+百分比+已收/总量）、下载完成自动换新重启。下载走 GitHub API asset 端点（国内网络下载域常断，API 域通）带三次重试。
- **自动更新开关**：设置页可关闭桥的定时自动检查/升级（热更即时生效），手动升级始终可用。
- **DSH 端口漂移自动跟随**：DSH 每次重启监听端口必变，此前需重启桥跟随；现在 mux 重连失败自动重探测（扫进程端口逐个验证）并切换，真机验证 DSH 重启后桥 6 秒内自动重连。
- **排障工具对**：`dump_workspace`（真调 DSH 打印 ok/error/value）、`mux_probe`（旁观事件流原始帧）、`update_drill`（更新链全流程演练）。

### 修复

- **升级/安装的 MCP 注册只写 web 档**（2026-10-25"AI 不回消息"总根因治本）：DSH 桌面版读 desktop 档，存量用户该档残留 Node 版旧注册（脚本已停用）导致 AI 全工具失败静默待命。现在两档都写、幂等清理 Node 旧块、保留用户自配条目。
- **新会话投递必失败（两层）**：agent 目录未预建被 DSH 拒绝；相对路径直传被 DSH 按自身工作目录解析成不存在路径。另修 RPC 业务错误被吞成"缺 workspaceId"。
- **OneBot WS 断连后桥变植物人**：读循环静默退出，进程/控制台全正常但消息无人收且零日志——改为退避重连全程落日志。
- **mux 重连三处静默失败无日志**（鉴权/连接/断连），观测黑洞全补。
- **升级脚本不自更新、迁移输出全乱码（GBK/UTF-8）、表情回应参数名（emoji_id）** 等。

### 变更

- **客户端 UI 系统化重做**：视觉系统（accent 收权、状态反馈三态、空态结构化、对比度上调）；信息架构按使用频率分层（迁移/花费折叠沉底、日志上移、检查更新归位标题栏）；功能语义修正六项（保存条只在设置页、sessionCwd 补表单入口、日志清屏、未保存提醒等）。
- **移除 serde_yaml + unsafe-libyaml 依赖链**（手写固定路径提取替代，语义测试等价）。

## [0.4.5] - 2026-10-25

### 新增

- **DSH 基址运行中自动重探测**：DSH 每次重启监听端口必漂（真机连漂四次），此前仅启动期有端口自适应、运行中只能重启桥跟随。现在 mux 重连的失败路径自动触发"扫进程端口逐个验证"并切换共享基址，事件流与 HTTP 投递一并跟随；真机验证 DSH 重启后桥 6 秒内自动重连、全程不重启。
- **排障工具**：`queqiao-dsh` 新增两个 example——`dump_workspace`（真调 workspace/create 打印 ok/error/value 全貌，`--prompt` 直投、`--delete` 清理误建）与 `mux_probe`（旁观 remote.mux 帧流，打印入站原始帧与快照）。本版多项修复即靠它们拿到决定性证据。

### 修复

- **OneBot WS 断连后桥变"植物人"**：读循环在断连时静默退出任务——进程、控制台、看门狗探测全部正常，但消息从此无人收且日志零痕迹。改为断连退避重连（5s 起翻倍封顶 60s），断开/重连/失败均落日志。
- **mux 重连三处静默失败无日志**（鉴权失败/WS 连接失败/断连）：观测黑洞，掉线后 UI 一切"正常"而事件永远不来，真机只能靠 netstat 反推——全部补日志并接入上述重探测。
- **新会话投递必失败（两层）**：workspace/create 前不建 agent 目录被 DSH realpath ENOENT 拒绝（迁移清单不迁 agents/，旧会话靠映射复用掩盖）；且 agent 目录相对路径直传 DSH——DSH 按自身 CWD 解析，目录建好仍失败（Node 版恒绝对路径，Rust 翻译丢失）。另修 RPC 业务错误被吞成"缺 workspaceId"（真因藏在 value=null 后面，日志只见现象）。
- **升级脚本不自更新**：安装目录 `scripts\upgrade.ps1` 永远停留旧版，新版升级逻辑永不生效（清单不含自己）；补自拷。
- **迁移输出回显全乱码**：PS5.1 默认 GBK 控制台编码被客户端按 UTF-8 解码（文件 BOM 与进程 stdout 编码是两回事）；脚本开头切 UTF-8 输出，`pick_folder` 中文路径同款隐患一并修。

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
