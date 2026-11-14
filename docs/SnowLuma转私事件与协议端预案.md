# SnowLuma 上游转私事件与协议端预案

> 2026-11-10 记录。事件：SnowLuma（鹊桥的 OneBot 协议端）作者因腾讯对第三方
> QQ 机器人框架的法律压力将仓库转为 private（对未登录访客 404）。这是 2020
> 年"晨风机器人被起诉 → 酷Q/QQLight/乾坤等十几个框架集体停服"剧本的重演。
>
> **2026-11-13 续记：备胎清单也塌了。** NapCat 官宣终结（主仓 11-12 最后提交
> `docs: ending` + `Keep README only`，README 只剩"到这里告一段落"，12.8k
> star、1915 fork 的头牌协议端落幕）；Lagrange.OneBot 仓库同日确认 404
> （删除或转私，Lagrange.Core 本体 11-07 仍在 push 但 OneBot 适配层已失）。
> 非官方协议端生态进入全面收缩期。**本机 SnowLuma 2.2.1 从"备胎旁有活路"
> 变为"实质孤品"**——第三节预案的可换端前提已不成立，策略随之调整（见 3.5）。
>
> **2026-11-14 三续：官方 Bot API 出现「并行第二会话源」新解。** §四与
> §三对比表中「官方 Bot API 形态不兼容」的判断针对**全量替换**路线，维持
> 不变；但把官方机器人作为**并行第二会话源**接入（不替换 SnowLuma，bot
> 身份独立成 `og:`/`op:` 会话，复用鹊桥全部资产）可以同时拿到合规对冲与
> 资产保全——EOL 决策树相应插入「降级官方通道保底」一步。完整评估与
> 里程碑见 `官方机器人通道接入预案.md`。

## 一、事件核验（2026-11-10 实测）

| 验证项 | 结果 |
|---|---|
| `github.com/SnowLuma/SnowLuma` | 404（private 或删除） |
| Release 资产直链（v2.4.0 / latest） | 404，**更新链断供** |
| 本地 SnowLuma 2.2.1（D:\200software\237QueQiao\SnowLuma） | 正常运行，与 NTQQ 9.9.36-53644 的 hook 兼容 |
| 昨日日志还提示的 "newer release v2.4.0" | 已不可下载 |

## 二、已实施的保命措施（本机）

**冻结 NTQQ 于 9.9.36-53644**——SnowLuma 的 hook 钉在特定 NTQQ 构建上，
QQ 一旦自动升级且新构建不兼容 hook，本地 2.2.1 即永久失效（无新版可追）。

1. **`%APPDATA%\QQ\packages` 目录锁死**（2026-11-10 已执行）：创建目录后
   `icacls /inheritance:r` 移除全部继承权限——更新包无处下载，整包升级被硬阻断。
   参考方法：[一劳永逸的禁用 NTQQ 的更新（B站）](https://www.bilibili.com/opus/901489587941015763)
   （原文是对 packages/full 子目录操作；本机 preemptive 锁整个 packages 等效且更早拦截）
2. **QQ 设置内"当有新版本时自动帮我下载并安装"建议手动关闭**（软开关，作第二道）
3. `D:\200software\203QQ\versions` 保持现有 ACL 未动——若日后需要第三道防线，
   可对该目录去写权限（会同时阻断手动升级，代价是重装才能换版本）
4. NTQQ 模块热更新（dynamic_package 目录）**未阻断**——它是加载期模块替换，
   不动主程序构建号，hook 不受影响；若观察到 hook 异常可再评估

**锁定的代价**：这台机器上的 QQ（两个账号实例）将停留在 9.9.36-53644，
不再获得安全更新与新功能。解锁方式：`icacls "$env:APPDATA\QQ\packages" /reset`。

## 三、协议端适配层设计（备胎预案，未动工）

**触发条件**：本地 SnowLuma 2.2.1 真失效（NTQQ 被外部因素升级 / hook 崩坏 /
重装）之日才启动。在那之前不写代码。

### 现状耦合面（2026-11-10 盘点）

桥对协议端的全部 12 个 OneBot action 调用：

| action | 标准 OneBot v11 | SnowLuma 私有 | 桥侧降级路径 |
|---|---|---|---|
| send_private_msg / send_group_msg / send_msg（含 reply） | ✅ | — | — |
| get_msg / get_forward_msg / delete_msg | ✅ | — | — |
| get_group_info / get_group_list / get_login_info / get_status | ✅ | — | — |
| get_record / get_image | ✅ | — | 失败回落文本标注 |
| **fetch_custom_face_detail** | ❌ | ✅（QQ 收藏表情同步） | 已有：同步失败回落本地库 |
| **_get_group_notice** | ❌ | ✅（群公告） | 已有：拉取失败不阻塞 |

SnowLuma WebUI/更新 API（`/api/update/*`、`x-snowluma-token`、5099）只在
配置发现与探活链路使用，不在消息主链上。

**结论：90% 调用面是标准协议，换协议端理论上零改动；私有 2 项已各自带降级。**

### 候选协议端对比

| | NapCatQQ | Lagrange.OneBot | 官方 Bot API |
|---|---|---|---|
| 技术路线 | hook NTQQ（与 SnowLuma 同路线） | 纯协议实现 | 官方开放接口 |
| 换端成本 | 极低（OneBot v11 兼容） | 低（同左） | **高**（消息模型完全不同） |
| 账号风险 | 中（与 SnowLuma 同级） | **高**（非官方客户端指纹，易风控冻结） | 无 |
| 形态兼容 | 群友人设可保留 | 同左 | **不兼容**（频道/管理形态，不能当群友聊天） |
| 法律风险 | 与 SnowLuma 同侧（腾讯打击半径内） | 同左 | 无 |
| 维护活跃度 | 社区大、更新频繁 | 活跃 | 官方 |

### 适配层设计要点（动工时执行）

1. `crates/queqiao` 新增 `onebot_backend` trait：包住现有 12 个 action 调用
   与事件流入口（EventStream 已是独立结构，天然适配点）
2. 两个实现：`snowluma`（现状代码平移）与 `napcat`（只实现标准 11 个，
   私有 2 个返回 Err 走既有降级）
3. config `snowluma.backend: "snowluma" | "napcat"` + napcat 的 ws/http 地址段
4. 事件流差异抹平：NapCat 的事件字段与 SnowLuma 有细微出入（message_id 正负、
   face 数组格式），在适配层归一化——以现有 core::action::BridgeEvent 为契约
5. 迁移验证脚本：双端同群对照收发（人肉 diff 事件流）

工程量预估：trait 抽取+平移 ~2 天，NapCat 实现+字段归一 ~3 天，验证 ~2 天。
**不做的理由**：本地 2.2.1 稳定运行中，适配层引入的是抽象成本+双实现维护成本，
收益只在"失效日"兑现。备胎放在文档里比放在代码里便宜。

### 3.5 2026-11-13 修订：备胎前提已塌，策略转"孤品保全"

NapCat 官宣 ending、Lagrange.OneBot 消失后，上表候选序列实际状态：

| 候选 | 2026-11-13 状态 |
|---|---|
| NapCatQQ | **官宣终结**（README："到这里告一段落"；社区 1915 fork 理论可继，但继任者未成势，追 fork = 追一个同样会被打击的目标） |
| Lagrange.OneBot | **仓库 404**（删除/转私；Lagrange.Core 本体还在，OneBot 适配层没了，等价不可用） |
| 官方 Bot API | 形态不兼容（维持原判断） |

**修订后的失效日预案**（本地 2.2.1 失效时按序尝试，均失败则项目归档）：

1. NapCat 存量版本（4.4.x）仍可安装——hook NTQQ 的机制不因停更立即失效，
   但只能配冻结版 NTQQ ≤ 其支持上限（若上限低于 9.9.36 则需要降级 QQ）
2. 活跃 fork 盘点（失效日再查——过早盘点无意义，fork 半衰期同样以月计）
3. 两者皆不可行 → 鹊桥 QQ 链路 EOL，桥代码与 DSH/工具面资产保留，
   等 QQ 开放平台是否长出"群友人设"能力的政策窗口

**当下的正确动作反而变简单了**：孤品的保全价值更高——
NTQQ 冻结（packages 锁死）从"保命措施"升级为"核心资产保全"；
SnowLuma 安装目录建议整目录备份一份离线介质（2.2.1 发行包 + config + data），
防止误删/磁盘故障这类本地风险。适配层代码**不再排期**——没有可换的目标，
抽象失去了对象。

## 四、风险共担声明

只要鹊桥继续通过非官方协议端接入 QQ，就在腾讯打击半径内。SnowLuma 转私（11-10）
与 NapCat 终结、Lagrange.OneBot 消失（11-13）是该风险在一周内的三次显形——
**非官方协议端生态已全面收缩，"换一个端"不再是可靠预案**。鹊桥自身的合规
退出路径（官方 Bot API）与"群友 AI"产品形态根本冲突，故维持现状并接受风险：

- bot 账号（2120097258）与你的主号（3388404701）隔离，风控损失可控
- 群内行为有频率护栏（maxSendPerMinute/Hour），不构成骚扰形态
- 本地 SnowLuma 2.2.1 + 冻结 NTQQ 9.9.36 构成自包含闭环，不依赖任何上游存活
- 本文档 3.5 节预案保证失效日有明确的决策树（存量版本 → fork → EOL）

## 已验证版本矩阵（发版时顺带更新）

| 组件 | 版本 | 验证日期 |
|---|---|---|
| NTQQ | 9.9.36-53644（冻结） | 2026-11-10 |
| SnowLuma runtime | 2.2.1（上游断供，本地终点版本） | 2026-11-10 |
| OneBot 实现 | SnowLuma snapshot 模式，HTTP 3000 + WS 3001 | 2026-11-10 |
| DSH | DeepSeek Harness 内嵌（expose-internals 子进程） | 2026-11-08 |
| 鹊桥 | 0.5.8 | 2026-11-14 |
