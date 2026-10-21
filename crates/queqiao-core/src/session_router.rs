//! 每会话策略路由（移植自 Node 版 `src/core/session-router.mjs` + 同名测试，契约同步迁移）。
//!
//! 桥的历史形态是**一个进程级全局 mode**——「这个群跑仿真、管理员私聊跑 closed-agent」
//! 因此不可能，且 1.2 万行文件里每个模式决策都读同一个可变全局。本模块把决策变成
//! 纯查找：给定会话 key，模式 / preset / 模型 / effort 是什么。
//!
//! 设计约束（Node 版刻意为之，全部保留）：
//! - **纯且同步**：不碰 fs / 配置 / 定时器 / DSH——持久化与 preset 安装归调用方；
//! - **旧模式名是内部词汇**：`chat`/`reserved`/`reserved2` 继续可用（迁移期），
//!   但对外查询一律给 canonical 拼写（`simulation`/`closed-agent`）；
//! - **fail-closed**：key 解析失败、未知模式、未命名 preset、不属于该模式的 preset、
//!   非会话专属的 `closed-agent` 一律拒绝，绝不静默放宽；
//! - **默认永不特权**：`defaults` 必须是仿真模式，`closed-agent` 只能来自显式
//!   owner 条目；
//! - **actor 只是声明**：`updated_by` 记录「谁要求改」，本模块不验证；返回的审计
//!   事件不落盘，调用方必须先持久化再视变更为生效。
//!
//! 分层调整：Node 版 `throw TypeError` 改为 `Result`；构造器注入的 `now` 时钟改为
//! 写方法显式传 `now_ms`（与 core 其他模块统一的注入形态，u64 类型同时排除了
//! Node 版 NaN/负数时钟防御）；`model`/`reasoning_effort` 的三态
//! （字段缺失→回退 defaults / 显式 null / 值）以嵌套 `Option` 建模。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Number.MAX_SAFE_INTEGER（JS 安全整数上界，会话 key 的 id 合法域）。
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// 内部模式拼写（含全部历史名；存储与迁移层使用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Chat,
    Reserved,
    Reserved2,
    Simulation,
    ClosedAgent,
}

impl Mode {
    /// 解析任意被接受的拼写（容忍前后空白——与 Node 版 normalizeMode 一致）。
    pub fn parse(s: &str) -> Result<Self, RouterError> {
        match s.trim() {
            "simulation" => Ok(Mode::Simulation),
            "chat" => Ok(Mode::Chat),
            "reserved" => Ok(Mode::Reserved),
            "reserved2" => Ok(Mode::Reserved2),
            "closed-agent" => Ok(Mode::ClosedAgent),
            _ => Err(RouterError::UnknownMode(s.to_owned())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Chat => "chat",
            Mode::Reserved => "reserved",
            Mode::Reserved2 => "reserved2",
            Mode::Simulation => "simulation",
            Mode::ClosedAgent => "closed-agent",
        }
    }

    /// Canonical 拼写（旧词汇只存在于存储与迁移层，不从这里漏出去）。
    pub fn canonical(self) -> CanonicalMode {
        match self {
            Mode::ClosedAgent => CanonicalMode::ClosedAgent,
            _ => CanonicalMode::Simulation,
        }
    }
}

/// 对外模式拼写：只有两个。仿真（安全默认：无本地工具、白名单门禁）与
/// closed-agent（仅 owner 私聊，完整本地工具面）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanonicalMode {
    Simulation,
    ClosedAgent,
}

impl CanonicalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CanonicalMode::Simulation => "simulation",
            CanonicalMode::ClosedAgent => "closed-agent",
        }
    }
}

/// 路由层错误。`Display` 文案与 Node 版一致（fail-closed 的报错是行为契约一部分）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouterError {
    InvalidKey(String),
    InvalidOwnerQQ,
    UnknownMode(String),
    RequiresMode(String),
    RequiresPreset(String),
    UnsafePreset { label: String, mode: Mode },
    DefaultNotSimulation,
    ClosedAgentRestricted { owner_configured: bool },
    InvalidStateVersion,
    DuplicateKey(String),
    InvalidUpdatedBy,
    InvalidText(String),
    UnknownLegacyMode,
    InvalidPresets(String),
}

impl std::fmt::Display for RouterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouterError::InvalidKey(key) => write!(f, "Invalid conversation key: {key}"),
            RouterError::InvalidOwnerQQ => {
                write!(f, "ownerQQ must be a positive safe integer or omitted")
            }
            RouterError::UnknownMode(mode) => write!(f, "Unknown mode: {mode}"),
            RouterError::RequiresMode(label) => write!(f, "{label} requires mode"),
            RouterError::RequiresPreset(label) => write!(
                f,
                "{label} requires preset (preset: null was retired by ADR 0003)"
            ),
            RouterError::UnsafePreset { label, mode } => {
                write!(
                    f,
                    "{label} has an unknown or unsafe preset for {}",
                    mode.as_str()
                )
            }
            RouterError::DefaultNotSimulation => write!(
                f,
                "defaults must remain simulation (closed-agent is per-conversation only)"
            ),
            RouterError::ClosedAgentRestricted { owner_configured } => {
                if *owner_configured {
                    write!(
                        f,
                        "closed-agent is restricted to the owner private conversation"
                    )
                } else {
                    write!(
                        f,
                        "closed-agent is restricted to the owner private conversation (no ownerQQ is configured)"
                    )
                }
            }
            RouterError::InvalidStateVersion => {
                write!(f, "conversation state must have version 1")
            }
            RouterError::DuplicateKey(key) => write!(f, "Duplicate conversation key: {key}"),
            RouterError::InvalidUpdatedBy => write!(
                f,
                "updatedBy must identify console or the owner private command"
            ),
            RouterError::InvalidText(field) => {
                write!(f, "{field} must be null or a non-empty trimmed string")
            }
            RouterError::UnknownLegacyMode => write!(f, "Unknown legacy mode; refusing migration"),
            RouterError::InvalidPresets(mode) => write!(
                f,
                "presets for {mode} must be a non-empty array of trimmed strings"
            ),
        }
    }
}

impl std::error::Error for RouterError {}

/// 会话 key 的 canonical 化（`group:<id>` / `private:<id>`，仅 `\d+`、安全整数、
/// 非零；剥前后空白与前导零）。非法输入返回 `None`，调用方显式 fail closed。
pub fn canonical_conversation_key(key: &str) -> Option<String> {
    let s = key.trim();
    let (kind, id) = s.split_once(':')?;
    if !matches!(kind, "group" | "private") {
        return None;
    }
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = id.parse().ok()?;
    if n == 0 || n > MAX_SAFE_INTEGER {
        return None;
    }
    Some(format!("{kind}:{n}"))
}

fn require_key(key: &str) -> Result<String, RouterError> {
    canonical_conversation_key(key).ok_or_else(|| RouterError::InvalidKey(key.to_owned()))
}

/// ownerQQ 接受数字或数字串（QQ 号两种写法都合法）；空串/None 表示未配置。
fn normalize_owner_qq(owner_qq: Option<&str>) -> Result<Option<u64>, RouterError> {
    let Some(raw) = owner_qq else { return Ok(None) };
    if raw.is_empty() {
        return Ok(None);
    }
    let canonical = canonical_conversation_key(&format!("private:{}", raw.trim()))
        .ok_or(RouterError::InvalidOwnerQQ)?;
    Ok(Some(
        canonical
            .trim_start_matches("private:")
            .parse()
            .expect("canonical key 的 id 必是数字"),
    ))
}

fn optional_text(value: Option<&str>, field: &str) -> Result<Option<String>, RouterError> {
    match value {
        None => Ok(None),
        Some(s) if !s.is_empty() && s.trim() == s => Ok(Some(s.to_owned())),
        Some(_) => Err(RouterError::InvalidText(field.to_owned())),
    }
}

/// `updated_by` 活动者校验：仅控制台与 owner 私聊指令通道可改策略；
/// `first-run`/`migration` 只在加载路径接受（`allow_system`），活体变更 API 一律拒绝。
fn require_updated_by(
    updated_by: Option<&str>,
    owner_key: Option<&str>,
    allow_system: bool,
) -> Result<String, RouterError> {
    let Some(actor) = updated_by else {
        return Err(RouterError::InvalidUpdatedBy);
    };
    if actor == "console" {
        return Ok(actor.to_owned());
    }
    if let Some(ok) = owner_key
        && actor == format!("command:{ok}")
    {
        return Ok(actor.to_owned());
    }
    if allow_system && (actor == "first-run" || actor == "migration") {
        return Ok(actor.to_owned());
    }
    Err(RouterError::InvalidUpdatedBy)
}

/// 各模式允许的 preset 集。typo 或过期配置挂上意外的工具面会被直接拒绝。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModePresets(BTreeMap<Mode, Vec<String>>);

impl ModePresets {
    /// 出厂默认表（一代拼写保留各自实际安装的 preset；`simulation` 是 `reserved2`
    /// 的 canonical 改名，领携 canonical preset 名，同时继续接受旧名以便回读）。
    pub fn default_presets() -> Self {
        let mut map = BTreeMap::new();
        map.insert(Mode::Chat, vec!["qq-chat".to_owned()]);
        map.insert(Mode::Reserved, vec!["qq-chat".to_owned()]);
        map.insert(Mode::Reserved2, vec!["qq-chat-v2".to_owned()]);
        map.insert(
            Mode::Simulation,
            vec![
                "qsh-sim".to_owned(),
                "qsh-sim-v2".to_owned(),
                "qq-chat-v2".to_owned(),
            ],
        );
        map.insert(Mode::ClosedAgent, vec!["qsh-closed".to_owned()]);
        ModePresets(map)
    }

    /// 从调用方提供的名字构建（安装了不同 preset 体系的部署传自己的）；
    /// 非空、非空白字符串、trim 后不变，否则拒绝。
    pub fn build(overrides: &[(Mode, Vec<String>)]) -> Result<Self, RouterError> {
        let mut presets = Self::default_presets();
        for (mode, list) in overrides {
            if list.is_empty() || list.iter().any(|p| p.is_empty() || p.trim() != p) {
                return Err(RouterError::InvalidPresets(mode.as_str().to_owned()));
            }
            presets.0.insert(*mode, list.clone());
        }
        Ok(presets)
    }

    fn contains(&self, mode: Mode, preset: &str) -> bool {
        self.0
            .get(&mode)
            .is_some_and(|list| list.iter().any(|p| p == preset))
    }

    fn first(&self, mode: Mode) -> &str {
        self.0
            .get(&mode)
            .and_then(|list| list.first())
            .map(String::as_str)
            .unwrap_or_default()
    }
}

/// 待校验的策略记录（会话条目或 defaults 的原始输入形状）。
///
/// `model`/`reasoning_effort` 是三态：外层 `None` = 字段缺失（解析时回退
/// defaults）、`Some(None)` = 显式 null（解析为 null，**不**回退）、`Some(Some(v))` = 值。
/// （Node 版以 `own(record, 'model')` 与 `record.model` 的 undefined/null 区分同款三态。）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyRecordInput {
    pub mode: Option<String>,
    pub preset: Option<String>,
    pub model: Option<Option<String>>,
    pub reasoning_effort: Option<Option<String>>,
    pub updated_by: Option<String>,
}

/// 待校验的会话状态原始输入。
#[derive(Debug, Clone, Default)]
pub struct ConversationStateInput {
    pub version: u32,
    pub defaults: PolicyRecordInput,
    pub conversations: BTreeMap<String, PolicyRecordInput>,
}

/// 规范化后的策略记录（存储态；serde 字段名对齐 Node 版 camelCase 形状）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyRecord {
    pub mode: Mode,
    pub preset: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<Option<String>>,
    /// defaults 记录无 actor。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
}

impl PolicyRecord {
    /// 存储态 → 输入态（重新校验路径；Node 版对象展开的显式等价物）。
    fn to_input(&self) -> PolicyRecordInput {
        PolicyRecordInput {
            mode: Some(self.mode.as_str().to_owned()),
            preset: Some(self.preset.clone()),
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            updated_by: self.updated_by.clone(),
        }
    }
}

/// 规范化后的会话状态（`snapshot` 的返回形状）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationState {
    pub version: u32,
    pub defaults: PolicyRecord,
    pub conversations: BTreeMap<String, PolicyRecord>,
}

impl ConversationState {
    /// 存储态 → 输入态：整体重校验（set/delete 的候选构造）与 bin 层落盘前的
    /// 形状转换共用。
    pub fn to_input(&self) -> ConversationStateInput {
        ConversationStateInput {
            version: self.version,
            defaults: self.defaults.to_input(),
            conversations: self
                .conversations
                .iter()
                .map(|(k, v)| (k.clone(), v.to_input()))
                .collect(),
        }
    }
}

/// 解析后的最终策略（`policy_for` 的返回；model/effort 已做 defaults 回退）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub key: String,
    pub mode: Mode,
    pub canonical_mode: CanonicalMode,
    pub preset: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub has_override: bool,
}

/// 各 canonical 模式的能力事实（是对 preset 的**陈述**，不是执行点；
/// 执行在 preset 的工具守卫与桥自身的门禁）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    pub local_execution: bool,
    pub local_files: bool,
    pub admin_commands: bool,
}

impl Permissions {
    fn for_mode(mode: CanonicalMode) -> Self {
        match mode {
            CanonicalMode::Simulation => Permissions {
                local_execution: false,
                local_files: false,
                admin_commands: false,
            },
            CanonicalMode::ClosedAgent => Permissions {
                local_execution: true,
                local_files: true,
                admin_commands: true,
            },
        }
    }
}

/// 校验一条策略记录。**检查顺序是承重的**：先形状与 actor（认证先于载荷——
/// 一条既未认证又畸形的记录报认证失败而非泄露载荷细节），再模式，再 preset
/// （必填且必须属于该模式），最后 closed-agent 边界。
fn normalize_policy(
    record: &PolicyRecordInput,
    label: &str,
    owner_key: Option<&str>,
    presets: &ModePresets,
    is_default: bool,
) -> Result<PolicyRecord, RouterError> {
    if !is_default {
        require_updated_by(record.updated_by.as_deref(), owner_key, true)?;
    }
    let mode = record
        .mode
        .as_deref()
        .map(Mode::parse)
        .transpose()?
        .ok_or_else(|| RouterError::RequiresMode(label.to_owned()))?;

    // preset 不可省略：不点名 preset 的策略无法审计（其工具面取决于本模块看不到的
    // 设置），是配置错误（fail-closed），不是邀请本层填默认值。
    let preset = record
        .preset
        .clone()
        .ok_or_else(|| RouterError::RequiresPreset(label.to_owned()))?;
    if !presets.contains(mode, &preset) {
        return Err(RouterError::UnsafePreset {
            label: label.to_owned(),
            mode,
        });
    }

    if is_default && mode == Mode::ClosedAgent {
        return Err(RouterError::DefaultNotSimulation);
    }
    // 一条边界一个文案：owner 未配置与 key 不是 owner 的拒绝理由相同。
    if mode == Mode::ClosedAgent && label != owner_key.unwrap_or("") {
        return Err(RouterError::ClosedAgentRestricted {
            owner_configured: owner_key.is_some(),
        });
    }

    let model = record
        .model
        .as_ref()
        .map(|inner| optional_text(inner.as_deref(), &format!("{label}.model")))
        .transpose()?;
    let reasoning_effort = record
        .reasoning_effort
        .as_ref()
        .map(|inner| optional_text(inner.as_deref(), &format!("{label}.reasoningEffort")))
        .transpose()?;

    Ok(PolicyRecord {
        mode,
        preset,
        model,
        reasoning_effort,
        updated_by: if is_default {
            None
        } else {
            record.updated_by.clone()
        },
    })
}

/// 校验整份状态。键先行两遍：`private:42` 与 `private:042` 是同一会话，
/// 两者并存即是损坏——独立一遍检测保证报错的是「重复」本身、结果不依赖键序。
fn normalize_state(
    state: &ConversationStateInput,
    owner_qq: Option<u64>,
    presets: &ModePresets,
) -> Result<ConversationState, RouterError> {
    if state.version != 1 {
        return Err(RouterError::InvalidStateVersion);
    }
    let defaults = normalize_policy(&state.defaults, "defaults", None, presets, true)?;
    let owner_key = owner_qq.map(|q| format!("private:{q}"));
    let mut canonical_keys = BTreeMap::new();
    for raw_key in state.conversations.keys() {
        let key = require_key(raw_key)?;
        if canonical_keys
            .insert(key.clone(), raw_key.clone())
            .is_some()
        {
            return Err(RouterError::DuplicateKey(key));
        }
    }
    let mut conversations = BTreeMap::new();
    for (key, raw_key) in &canonical_keys {
        let record = &state.conversations[raw_key];
        let normalized = normalize_policy(record, key, owner_key.as_deref(), presets, false)?;
        conversations.insert(key.clone(), normalized);
    }
    Ok(ConversationState {
        version: 1,
        defaults,
        conversations,
    })
}

/// 构建初始会话状态。`owner_qq` 为 `None`/空串表示尚未配置；
/// `default_mode` 为 `None` 时退 canonical 默认（simulation）——closed-agent 永远
/// 不能成为默认（它是每会话的特权，不是全通道的 blanket）；owner 已知且未选择
/// 关闭时，owner 私聊首启即 closed-agent（文档化的 first-run 默认）。
pub fn create_initial_conversation_state(
    owner_qq: Option<&str>,
    default_mode: Option<&str>,
    closed_agent_enabled: bool,
) -> Result<ConversationState, RouterError> {
    let owner = normalize_owner_qq(owner_qq)?;
    build_initial_state(
        owner,
        default_mode,
        closed_agent_enabled,
        &ModePresets::default_presets(),
    )
}

fn build_initial_state(
    owner: Option<u64>,
    default_mode: Option<&str>,
    closed_agent_enabled: bool,
    presets: &ModePresets,
) -> Result<ConversationState, RouterError> {
    // 拼写仍被校验（不认识的拼写是配置错误，不是静默回退）。
    let requested = match default_mode {
        None | Some("") => None,
        Some(m) => Some(Mode::parse(m)?),
    };
    let safe_mode = match requested {
        None | Some(Mode::ClosedAgent) => Mode::Simulation,
        Some(m) => m.canonical().as_mode(),
    };
    let mut conversations = BTreeMap::new();
    if let Some(q) = owner
        && closed_agent_enabled
    {
        conversations.insert(
            format!("private:{q}"),
            PolicyRecord {
                mode: Mode::ClosedAgent,
                preset: presets.first(Mode::ClosedAgent).to_owned(),
                model: None,
                reasoning_effort: None,
                updated_by: Some("first-run".to_owned()),
            },
        );
    }
    Ok(ConversationState {
        version: 1,
        defaults: PolicyRecord {
            mode: safe_mode,
            preset: presets.first(safe_mode).to_owned(),
            model: None,
            reasoning_effort: None,
            updated_by: None,
        },
        conversations,
    })
}

impl CanonicalMode {
    fn as_mode(self) -> Mode {
        match self {
            CanonicalMode::Simulation => Mode::Simulation,
            CanonicalMode::ClosedAgent => Mode::ClosedAgent,
        }
    }
}

/// 显式迁移：从单一全局模式到每会话状态。返回调用方**必须持久化**的审计事件
/// （本模块从不写盘）；迁移没有静默分支——最常见的一次迁移同样要留审计记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationEvent {
    pub event_type: &'static str,
    pub from: String,
    pub to: String,
    pub owner_key: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationOutcome {
    pub state: ConversationState,
    pub events: Vec<MigrationEvent>,
}

pub fn migrate_legacy_mode(
    mode: &str,
    owner_qq: Option<&str>,
) -> Result<MigrationOutcome, RouterError> {
    // 与 Node 版一致：此处精确匹配（不 trim）——ALL_MODES.includes 的语义。
    if !matches!(
        mode,
        "chat" | "reserved" | "reserved2" | "simulation" | "closed-agent"
    ) {
        return Err(RouterError::UnknownLegacyMode);
    }
    let owner = normalize_owner_qq(owner_qq)?;
    let closed_agent_enabled = mode == "closed-agent" && owner.is_some();
    let mut state = build_initial_state(
        owner,
        Some(mode),
        closed_agent_enabled,
        &ModePresets::default_presets(),
    )?;
    let owner_key = owner.map(|q| format!("private:{q}"));
    let events = vec![MigrationEvent {
        event_type: "legacy-mode-migrated",
        from: mode.to_owned(),
        to: if closed_agent_enabled {
            "closed-agent for owner private; default elsewhere".to_owned()
        } else {
            format!("default {}", state.defaults.mode.as_str())
        },
        owner_key: owner_key.clone(),
        reason: if mode == "closed-agent" && owner.is_none() {
            Some("missing ownerQQ; downgraded".to_owned())
        } else {
            None
        },
    }];
    if closed_agent_enabled {
        state
            .conversations
            .get_mut(
                owner_key
                    .as_deref()
                    .expect("closed_agent_enabled 蕴含 owner_key"),
            )
            .expect("owner 条目在 build_initial_state 已插入")
            .updated_by = Some("migration".to_owned());
    }
    Ok(MigrationOutcome { state, events })
}

/// 会话策略变更结果。`audit_event` 由调用方持久化（铁律 L5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeResult {
    pub key: String,
    pub changed_keys: Vec<String>,
    pub audit_event: AuditEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    pub event_type: &'static str,
    pub key: String,
    pub updated_by: String,
    pub at: u64,
    /// 变更前后的**完整**解析策略（而不只是本模块恰好比较的字段）——审计轨迹
    /// 要能回答「这条会话变更时在哪个 preset 上」。
    pub before: Policy,
    pub after: Policy,
    pub had_override: bool,
    /// delete 专有：删的是不存在的 override。
    pub no_op: bool,
    pub policy_changed: bool,
}

/// 只读策略查找：配置变更时构建一次，从不原地突变。
#[derive(Debug, Clone)]
pub struct SessionRouter {
    owner_qq: Option<u64>,
    default_mode: CanonicalMode,
    state: ConversationState,
    mode_presets: ModePresets,
}

/// [`SessionRouter::new`] 的入参。`state` 为 `None` 时按首次运行构建；
/// `default_mode` 显式覆盖持久默认（DSH 设置对「当前全局模式」仍是权威——
/// 但它引入不了特权默认，`defaults` 校验会拒绝 closed-agent）。
#[derive(Debug, Clone, Default)]
pub struct SessionRouterOptions<'a> {
    pub owner_qq: Option<&'a str>,
    pub default_mode: Option<&'a str>,
    pub state: Option<ConversationStateInput>,
    pub mode_presets: Option<ModePresets>,
}

impl SessionRouter {
    pub fn new(opts: SessionRouterOptions<'_>) -> Result<Self, RouterError> {
        let owner_qq = normalize_owner_qq(opts.owner_qq)?;
        let mode_presets = opts
            .mode_presets
            .clone()
            .unwrap_or_else(ModePresets::default_presets);
        let default_mode_str = opts.default_mode.unwrap_or("reserved2");
        // 加载即供给路径：拿到的 state 是本模块自己写出的（first-run/migration
        // 是合法 actor——见 normalize_policy 的说明）。
        let state_input = match opts.state {
            None => build_initial_state(owner_qq, Some(default_mode_str), true, &mode_presets)?
                .to_input(),
            Some(s) => s,
        };
        let mut state = normalize_state(&state_input, owner_qq, &mode_presets)?;
        let dm = Mode::parse(default_mode_str)?;
        if dm != Mode::ClosedAgent {
            state.defaults.mode = dm.canonical().as_mode();
        }
        let default_mode = state.defaults.mode.canonical();
        Ok(Self {
            owner_qq,
            default_mode,
            state,
            mode_presets,
        })
    }

    pub fn owner_qq(&self) -> Option<u64> {
        self.owner_qq
    }

    pub fn default_mode(&self) -> CanonicalMode {
        self.default_mode
    }

    pub fn snapshot(&self) -> ConversationState {
        self.state.clone()
    }

    /// 会话的显式 override，无则是 `None`。
    pub fn override_for(&self, key: &str) -> Result<Option<&PolicyRecord>, RouterError> {
        let canonical = require_key(key)?;
        Ok(self.state.conversations.get(&canonical))
    }

    /// 会话的解析策略。默认永不返回特权模式。
    pub fn policy_for(&self, key: &str) -> Result<Policy, RouterError> {
        let canonical = require_key(key)?;
        let record = self
            .state
            .conversations
            .get(&canonical)
            .unwrap_or(&self.state.defaults);
        let has_override = self.state.conversations.contains_key(&canonical);
        let defaults = &self.state.defaults;
        // 存储态记录的 preset 都是显式点名过的（normalize_policy 拒绝 null 与
        // 不属于模式的名称），所以这里是读取而非回退链。
        let preset = if record.preset.is_empty() {
            self.mode_presets.first(record.mode).to_owned()
        } else {
            record.preset.clone()
        };
        Ok(Policy {
            key: canonical.clone(),
            mode: record.mode,
            canonical_mode: record.mode.canonical(),
            preset,
            model: record
                .model
                .as_ref()
                .or(defaults.model.as_ref())
                .cloned()
                .flatten(),
            reasoning_effort: record
                .reasoning_effort
                .as_ref()
                .or(defaults.reasoning_effort.as_ref())
                .cloned()
                .flatten(),
            has_override,
        })
    }

    /// 会话模式的 canonical 拼写（业务代码一律用这个）。
    pub fn mode_for(&self, key: &str) -> Result<CanonicalMode, RouterError> {
        Ok(self.policy_for(key)?.canonical_mode)
    }

    /// 内部/存储层拼写（`chat`/`reserved`/`reserved2` 等旧名）：只有迁移与
    /// 「当前全局设置仍是旧名」这类兼容判断需要。
    pub fn internal_mode_for(&self, key: &str) -> Result<Mode, RouterError> {
        Ok(self.policy_for(key)?.mode)
    }

    pub fn preset_for(&self, key: &str) -> Result<String, RouterError> {
        Ok(self.policy_for(key)?.preset)
    }

    pub fn model_for(&self, key: &str) -> Result<Option<String>, RouterError> {
        Ok(self.policy_for(key)?.model)
    }

    pub fn effort_for(&self, key: &str) -> Result<Option<String>, RouterError> {
        Ok(self.policy_for(key)?.reasoning_effort)
    }

    pub fn permissions_for(&self, key: &str) -> Result<Permissions, RouterError> {
        Ok(Permissions::for_mode(self.policy_for(key)?.canonical_mode))
    }

    /// 该会话是否允许以其解析出的模式运行。closed-agent 只允许 owner 私聊——
    /// 这是 L2 边界，必须留在本层让所有调用方继承。非法 key 一律 false。
    pub fn mode_allows_key(&self, key: &str) -> bool {
        let Some(canonical) = canonical_conversation_key(key) else {
            return false;
        };
        if self.mode_for(&canonical) != Ok(CanonicalMode::ClosedAgent) {
            return true;
        }
        self.owner_qq
            .is_some_and(|q| canonical == format!("private:{q}"))
    }

    /// 策略的稳定身份；变了就意味着会话必须重建。格式与 Node 版
    /// `JSON.stringify([mode, preset, model, effort, ownerQQ])` 逐字段一致
    /// （便于跨实现对拍）。
    pub fn policy_fingerprint(&self, key: &str) -> Result<String, RouterError> {
        Ok(self.fingerprint_of(&self.policy_for(key)?))
    }

    fn fingerprint_of(&self, p: &Policy) -> String {
        let json_str = |s: &str| serde_json::to_string(s).expect("字符串序列化必然成功");
        let json_opt = |v: &Option<String>| match v {
            None => "null".to_owned(),
            Some(s) => json_str(s),
        };
        format!(
            "[{},{},{},{},{}]",
            json_str(p.mode.as_str()),
            json_str(&p.preset),
            json_opt(&p.model),
            json_opt(&p.reasoning_effort),
            self.owner_qq
                .map(|q| q.to_string())
                .unwrap_or_else(|| "null".to_owned())
        )
    }

    /// 设置一条 override。`updated_by` 必须来自已完成认证的层；调用方必须持久化
    /// 返回的审计事件。校验失败时状态保持不变。
    pub fn set_conversation(
        &mut self,
        key: &str,
        record: PolicyRecordInput,
        now_ms: u64,
    ) -> Result<ChangeResult, RouterError> {
        let canonical = require_key(key)?;
        let owner_key = self.owner_qq.map(|q| format!("private:{q}"));
        // 活体变更 API 拒绝 first-run/migration：那两个 actor 只在加载路径合法。
        require_updated_by(record.updated_by.as_deref(), owner_key.as_deref(), false)?;

        let before = self.policy_for(&canonical)?;
        let before_fp = self.fingerprint_of(&before);
        let had_override = self.state.conversations.contains_key(&canonical);

        let mut candidate = self.snapshot().to_input();
        candidate.conversations.insert(canonical.clone(), record);
        let next = normalize_state(&candidate, self.owner_qq, &self.mode_presets)?;
        self.state = next;

        let after = self.policy_for(&canonical)?;
        let policy_changed = before_fp != self.fingerprint_of(&after);
        let changed_keys = if policy_changed {
            vec![canonical.clone()]
        } else {
            Vec::new()
        };
        Ok(ChangeResult {
            key: canonical.clone(),
            changed_keys,
            audit_event: AuditEvent {
                event_type: "conversation-set",
                key: canonical.clone(),
                updated_by: self
                    .state
                    .conversations
                    .get(&canonical)
                    .and_then(|r| r.updated_by.clone())
                    .expect("normalize 后会话条目必有 updated_by"),
                at: now_ms,
                before,
                after,
                had_override,
                no_op: false,
                policy_changed,
            },
        })
    }

    /// 删除一条 override，会话回落到默认。校验失败时状态保持不变。
    pub fn delete_conversation(
        &mut self,
        key: &str,
        updated_by: &str,
        now_ms: u64,
    ) -> Result<ChangeResult, RouterError> {
        let canonical = require_key(key)?;
        let owner_key = self.owner_qq.map(|q| format!("private:{q}"));
        let actor = require_updated_by(Some(updated_by), owner_key.as_deref(), false)?;

        let before = self.policy_for(&canonical)?;
        let before_fp = self.fingerprint_of(&before);
        let had_override = self.state.conversations.contains_key(&canonical);

        let mut candidate = self.snapshot().to_input();
        candidate.conversations.remove(&canonical);
        let next = normalize_state(&candidate, self.owner_qq, &self.mode_presets)?;
        self.state = next;

        let after = self.policy_for(&canonical)?;
        let policy_changed = before_fp != self.fingerprint_of(&after);
        let changed_keys = if policy_changed {
            vec![canonical.clone()]
        } else {
            Vec::new()
        };
        Ok(ChangeResult {
            key: canonical.clone(),
            changed_keys,
            audit_event: AuditEvent {
                event_type: "conversation-deleted",
                key: canonical,
                updated_by: actor,
                at: now_ms,
                before,
                after,
                had_override,
                no_op: !had_override,
                policy_changed,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER_QQ: &str = "42";
    const OWNER_KEY: &str = "private:42";

    fn policy(
        mode: Option<&str>,
        preset: Option<&str>,
        updated_by: Option<&str>,
    ) -> PolicyRecordInput {
        PolicyRecordInput {
            mode: mode.map(str::to_owned),
            preset: preset.map(str::to_owned),
            model: None,
            reasoning_effort: None,
            updated_by: updated_by.map(str::to_owned),
        }
    }

    /// Node 版测试的 stateWith：初态（owner 无 closed-agent）+ 一条 console 记录。
    fn state_with(key: &str, rec: PolicyRecordInput) -> ConversationStateInput {
        let mut state = base_input(Some(42), false);
        state.conversations.insert(key.to_owned(), rec);
        state
    }

    /// 典型合法输入状态（「初态的输出形状作为再加载的输入」）。
    fn base_input(owner: Option<u64>, closed_agent: bool) -> ConversationStateInput {
        let mut conversations = BTreeMap::new();
        if let Some(q) = owner
            && closed_agent
        {
            conversations.insert(
                format!("private:{q}"),
                policy(Some("closed-agent"), Some("qsh-closed"), Some("first-run")),
            );
        }
        ConversationStateInput {
            version: 1,
            defaults: policy(Some("simulation"), Some("qsh-sim"), None),
            conversations,
        }
    }

    fn router(owner_qq: Option<&str>) -> SessionRouter {
        SessionRouter::new(SessionRouterOptions {
            owner_qq,
            ..Default::default()
        })
        .expect("测试路由器构造必然成功")
    }

    /// 契约与 Node 版 session-router.test 一致：key 去前导零与前后空白。
    #[test]
    fn canonical_keys_strip_zeros_and_whitespace() {
        assert_eq!(
            canonical_conversation_key("  private:00042  ").as_deref(),
            Some(OWNER_KEY)
        );
        assert_eq!(
            canonical_conversation_key("group:00042").as_deref(),
            Some("group:42")
        );
    }

    /// 非法 / 零 / 超安全整数键永远成不了会话。
    #[test]
    fn invalid_keys_never_become_conversations() {
        for key in [
            "private:0",
            "group:-1",
            "group:1.5",
            "Group:1",
            "private:9007199254740992",
            "private:1:2",
            "",
        ] {
            assert_eq!(canonical_conversation_key(key), None, "key={key:?}");
        }
    }

    /// 初态：owner 私聊 closed-agent，其余通道一律仿真；快照可被本构造器重载。
    #[test]
    fn initial_owner_private_is_closed_agent() {
        let r = router(Some(OWNER_QQ));
        assert_eq!(r.mode_for(OWNER_KEY).unwrap(), CanonicalMode::ClosedAgent);
        assert_eq!(r.preset_for(OWNER_KEY).unwrap(), "qsh-closed");
        assert_eq!(r.mode_for("private:9").unwrap(), CanonicalMode::Simulation);
        assert_eq!(r.mode_for("group:9").unwrap(), CanonicalMode::Simulation);
        assert_eq!(r.preset_for("group:9").unwrap(), "qsh-sim");
        // 本构造器产出的必须能被它自己重载（持久默认在出口即 canonical 化）。
        let reloaded = SessionRouter::new(SessionRouterOptions {
            owner_qq: Some(OWNER_QQ),
            state: Some(r.snapshot().to_input()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(reloaded.default_mode(), r.default_mode());
        assert_eq!(reloaded.preset_for("group:9").unwrap(), "qsh-sim");
        assert_eq!(
            reloaded.policy_fingerprint("group:9").unwrap(),
            r.policy_fingerprint("group:9").unwrap()
        );
    }

    /// 未配置 ownerQQ：每个会话都在仿真。
    #[test]
    fn missing_owner_keeps_every_conversation_simulating() {
        let r = router(None);
        assert_eq!(r.owner_qq(), None);
        assert_eq!(r.mode_for(OWNER_KEY).unwrap(), CanonicalMode::Simulation);
        assert_eq!(r.mode_for("group:9").unwrap(), CanonicalMode::Simulation);
    }

    /// owner 可在首启时显式关闭 closed-agent。
    #[test]
    fn owner_may_disable_closed_agent_at_first_run() {
        let state = create_initial_conversation_state(Some(OWNER_QQ), None, false).unwrap();
        assert!(state.conversations.is_empty());
        let r = SessionRouter::new(SessionRouterOptions {
            owner_qq: Some(OWNER_QQ),
            state: Some(ConversationStateInput {
                version: 1,
                defaults: policy(Some("simulation"), Some("qsh-sim"), None),
                conversations: BTreeMap::new(),
            }),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r.mode_for(OWNER_KEY).unwrap(), CanonicalMode::Simulation);
    }

    /// 非法 ownerQQ 被拒绝而非静默信任。
    #[test]
    fn invalid_owner_qq_rejected() {
        for invalid in ["0", "-1", "abc", "9007199254740992"] {
            let err = SessionRouter::new(SessionRouterOptions {
                owner_qq: Some(invalid),
                ..Default::default()
            })
            .unwrap_err();
            assert!(
                matches!(err, RouterError::InvalidOwnerQQ),
                "ownerQQ={invalid:?}"
            );
            assert!(err.to_string().contains("ownerQQ"));
        }
    }

    /// closed-agent 不能成为全局默认。
    #[test]
    fn closed_agent_cannot_be_default() {
        let mut state = base_input(Some(42), true);
        state.defaults = policy(Some("closed-agent"), Some("qsh-closed"), None);
        let err = SessionRouter::new(SessionRouterOptions {
            owner_qq: Some(OWNER_QQ),
            state: Some(state),
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, RouterError::DefaultNotSimulation));
        assert!(err.to_string().contains("defaults must remain simulation"));
    }

    /// closed-agent 不能分给群、他人私聊，owner 未配置时也拒绝。
    #[test]
    fn closed_agent_restricted_to_owner() {
        for key in ["group:8", "private:8"] {
            let state = state_with(
                key,
                policy(Some("closed-agent"), Some("qsh-closed"), Some("console")),
            );
            let err = SessionRouter::new(SessionRouterOptions {
                owner_qq: Some(OWNER_QQ),
                state: Some(state),
                ..Default::default()
            })
            .unwrap_err();
            assert!(
                matches!(err, RouterError::ClosedAgentRestricted { .. }),
                "key={key}"
            );
            assert!(err.to_string().contains("restricted to the owner"));
        }
        // 无 owner 配置：owner 自己的 key 也不行。
        let state = state_with(
            OWNER_KEY,
            policy(Some("closed-agent"), Some("qsh-closed"), Some("console")),
        );
        let err = SessionRouter::new(SessionRouterOptions {
            state: Some(state),
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(
            err,
            RouterError::ClosedAgentRestricted {
                owner_configured: false
            }
        ));
        assert!(err.to_string().contains("no ownerQQ is configured"));
    }

    /// 模式缺失与未知模式在加载时 fail closed。
    /// （Node 版的 `{ toString: () => 'closed-agent' }` 对象模式由 String 类型排除。）
    #[test]
    fn missing_or_unknown_modes_fail_closed() {
        for (mode, preset) in [
            (None, "qsh-sim"),
            (Some("reserved2"), "qsh-sim"),
            (Some("mystery"), "qsh-sim"),
        ] {
            let state = state_with("group:8", policy(mode, Some(preset), Some("console")));
            assert!(
                SessionRouter::new(SessionRouterOptions {
                    owner_qq: Some(OWNER_QQ),
                    state: Some(state),
                    ..Default::default()
                })
                .is_err(),
                "mode={mode:?}"
            );
        }
    }

    /// preset 缺失 / null / 未知 / 跨模式全部 fail closed。
    #[test]
    fn bad_presets_fail_closed() {
        for preset in [None, Some("standard"), Some("qsh-closed")] {
            let state = state_with(
                "group:8",
                policy(Some("simulation"), preset, Some("console")),
            );
            assert!(
                SessionRouter::new(SessionRouterOptions {
                    owner_qq: Some(OWNER_QQ),
                    state: Some(state),
                    ..Default::default()
                })
                .is_err(),
                "preset={preset:?}"
            );
        }
        // closed-agent 用仿真的 preset 同样拒绝。
        let state = state_with(
            OWNER_KEY,
            policy(Some("closed-agent"), Some("qsh-sim"), Some("console")),
        );
        assert!(
            SessionRouter::new(SessionRouterOptions {
                owner_qq: Some(OWNER_QQ),
                state: Some(state),
                ..Default::default()
            })
            .is_err()
        );
    }

    /// defaults 同样要求已知 mode 与 preset。
    #[test]
    fn defaults_require_known_mode_and_preset() {
        for (mode, preset) in [
            (None, Some("qsh-sim")),
            (Some("simulation"), None),
            (Some("simulation"), Some("standard")),
        ] {
            let mut state = base_input(Some(42), true);
            state.defaults = policy(mode, preset, None);
            assert!(
                SessionRouter::new(SessionRouterOptions {
                    owner_qq: Some(OWNER_QQ),
                    state: Some(state),
                    ..Default::default()
                })
                .is_err(),
                "defaults={mode:?}/{preset:?}"
            );
        }
    }

    /// mode / preset / model / effort 按会话独立解析（显式 null 不回退 defaults）。
    #[test]
    fn fields_resolve_independently_per_conversation() {
        let mut state = base_input(Some(42), true);
        state.defaults.model = Some(Some("default-model".to_owned()));
        state.defaults.reasoning_effort = Some(Some("low".to_owned()));
        let owner_rec = state.conversations.get_mut(OWNER_KEY).unwrap();
        owner_rec.model = Some(Some("owner-model".to_owned()));
        owner_rec.reasoning_effort = Some(Some("high".to_owned()));
        state.conversations.insert(
            "group:8".to_owned(),
            PolicyRecordInput {
                mode: Some("simulation".to_owned()),
                preset: Some("qsh-sim-v2".to_owned()),
                model: Some(None),      // 显式 null：不回退 default-model
                reasoning_effort: None, // 缺失：回退 low
                updated_by: Some("console".to_owned()),
            },
        );
        let r = SessionRouter::new(SessionRouterOptions {
            owner_qq: Some(OWNER_QQ),
            state: Some(state),
            ..Default::default()
        })
        .unwrap();
        let owner = r.policy_for(OWNER_KEY).unwrap();
        assert_eq!(
            (
                owner.mode.as_str(),
                owner.preset.as_str(),
                owner.model.as_deref(),
                owner.reasoning_effort.as_deref()
            ),
            (
                "closed-agent",
                "qsh-closed",
                Some("owner-model"),
                Some("high")
            )
        );
        let g = r.policy_for("group:8").unwrap();
        assert_eq!(
            (
                g.mode.as_str(),
                g.preset.as_str(),
                g.model.as_deref(),
                g.reasoning_effort.as_deref()
            ),
            ("simulation", "qsh-sim-v2", None, Some("low"))
        );
        assert_eq!(
            r.model_for("private:8").unwrap().as_deref(),
            Some("default-model")
        );
    }

    /// 仿真模式拒绝本地工具与管理指令；closed-agent 全开。
    #[test]
    fn permissions_by_mode() {
        let r = router(Some(OWNER_QQ));
        assert_eq!(
            r.permissions_for("group:8").unwrap(),
            Permissions {
                local_execution: false,
                local_files: false,
                admin_commands: false
            }
        );
        assert_eq!(
            r.permissions_for("private:8").unwrap(),
            r.permissions_for("group:8").unwrap()
        );
        assert_eq!(
            r.permissions_for(OWNER_KEY).unwrap(),
            Permissions {
                local_execution: true,
                local_files: true,
                admin_commands: true
            }
        );
    }

    /// 非法调用 key 被拒绝而不是领取默认特权。
    #[test]
    fn invalid_caller_key_rejected() {
        let r = router(Some(OWNER_QQ));
        for (key, msg) in [
            ("group:0", "mode_for"),
            ("private:9007199254740992", "permissions_for"),
        ] {
            let err = match msg {
                "mode_for" => r.mode_for(key).unwrap_err(),
                _ => r.permissions_for(key).unwrap_err(),
            };
            assert!(matches!(err, RouterError::InvalidKey(_)));
            assert!(err.to_string().contains("Invalid conversation key"));
        }
    }

    /// 策略变更只报告 canonical key（'group:0008' → 'group:8'），他人指纹不动。
    #[test]
    fn change_reports_only_canonical_key() {
        let mut r = router(Some(OWNER_QQ));
        let owner_before = r.policy_fingerprint(OWNER_KEY).unwrap();
        let other_before = r.policy_fingerprint("group:9").unwrap();
        let result = r
            .set_conversation(
                "group:0008",
                PolicyRecordInput {
                    mode: Some("simulation".to_owned()),
                    preset: Some("qsh-sim-v2".to_owned()),
                    model: Some(Some("small".to_owned())),
                    reasoning_effort: Some(Some("low".to_owned())),
                    updated_by: Some("console".to_owned()),
                },
                0,
            )
            .unwrap();
        assert_eq!(result.changed_keys, ["group:8"]);
        assert_eq!(r.policy_fingerprint(OWNER_KEY).unwrap(), owner_before);
        assert_eq!(r.policy_fingerprint("group:9").unwrap(), other_before);
        assert_ne!(r.policy_fingerprint("group:8").unwrap(), other_before);
    }

    /// model 与 effort 的编辑各自只改本会话指纹。
    #[test]
    fn model_and_effort_edits_change_own_fingerprint() {
        let mut r = router(Some(OWNER_QQ));
        let group_before = r.policy_fingerprint("group:8").unwrap();
        let first = r
            .set_conversation(
                OWNER_KEY,
                PolicyRecordInput {
                    mode: Some("closed-agent".to_owned()),
                    preset: Some("qsh-closed".to_owned()),
                    model: Some(Some("m1".to_owned())),
                    reasoning_effort: Some(None),
                    updated_by: Some(format!("command:{OWNER_KEY}")),
                },
                0,
            )
            .unwrap();
        let after_model = r.policy_fingerprint(OWNER_KEY).unwrap();
        let second = r
            .set_conversation(
                OWNER_KEY,
                PolicyRecordInput {
                    mode: Some("closed-agent".to_owned()),
                    preset: Some("qsh-closed".to_owned()),
                    model: Some(Some("m1".to_owned())),
                    reasoning_effort: Some(Some("high".to_owned())),
                    updated_by: Some("console".to_owned()),
                },
                0,
            )
            .unwrap();
        assert_eq!(first.changed_keys, [OWNER_KEY]);
        assert_eq!(second.changed_keys, [OWNER_KEY]);
        assert_ne!(after_model, r.policy_fingerprint(OWNER_KEY).unwrap());
        assert_eq!(r.policy_fingerprint("group:8").unwrap(), group_before);
    }

    /// 指纹外字段的编辑不重启会话。（Node 版以额外 note 字段验证；
    /// 强类型输入无指纹外字段，以「重复提交同指纹策略」等价验证。）
    #[test]
    fn same_fingerprint_edit_does_not_restart_conversation() {
        let mut r = router(Some(OWNER_QQ));
        let fp = r.policy_fingerprint(OWNER_KEY).unwrap();
        let result = r
            .set_conversation(
                OWNER_KEY,
                policy(Some("closed-agent"), Some("qsh-closed"), Some("console")),
                0,
            )
            .unwrap();
        assert_eq!(result.changed_keys, Vec::<String>::new());
        assert_eq!(r.policy_fingerprint(OWNER_KEY).unwrap(), fp);
    }

    /// 被拒绝的更新不改变任何会话策略。
    #[test]
    fn rejected_update_leaves_state_unchanged() {
        let mut r = router(Some(OWNER_QQ));
        let before = r.snapshot();
        let err = r
            .set_conversation(
                "group:8",
                policy(Some("closed-agent"), Some("qsh-closed"), Some("console")),
                0,
            )
            .unwrap_err();
        assert!(err.to_string().contains("restricted to the owner"));
        assert_eq!(r.snapshot(), before);
    }

    /// 删除 owner override：只有 owner 回落仿真，其他会话不动。
    #[test]
    fn removing_owner_override_returns_owner_to_simulation() {
        let mut r = router(Some(OWNER_QQ));
        let group_before = r.policy_fingerprint("group:8").unwrap();
        let result = r.delete_conversation(OWNER_KEY, "console", 0).unwrap();
        assert_eq!(result.changed_keys, [OWNER_KEY]);
        assert_eq!(r.mode_for(OWNER_KEY).unwrap(), CanonicalMode::Simulation);
        assert_eq!(r.policy_fingerprint("group:8").unwrap(), group_before);
    }

    /// chat / reserved / reserved2 显式迁移，带审计事件。
    #[test]
    fn legacy_simulation_modes_migrate_with_events() {
        for mode in ["chat", "reserved", "reserved2"] {
            let outcome = migrate_legacy_mode(mode, Some(OWNER_QQ)).unwrap();
            let r = SessionRouter::new(SessionRouterOptions {
                owner_qq: Some(OWNER_QQ),
                state: Some(outcome.state.to_input()),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(r.mode_for(OWNER_KEY).unwrap(), CanonicalMode::Simulation);
            assert_eq!(r.mode_for("group:8").unwrap(), CanonicalMode::Simulation);
            assert_eq!(outcome.events.len(), 1);
            assert_eq!(outcome.events[0].from, mode);
            assert_eq!(outcome.events[0].event_type, "legacy-mode-migrated");
        }
    }

    /// closed-agent 迁移只落到 owner 私聊。
    #[test]
    fn legacy_closed_agent_migrates_to_owner_only() {
        let outcome = migrate_legacy_mode("closed-agent", Some(OWNER_QQ)).unwrap();
        let r = SessionRouter::new(SessionRouterOptions {
            owner_qq: Some(OWNER_QQ),
            state: Some(outcome.state.to_input()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r.mode_for(OWNER_KEY).unwrap(), CanonicalMode::ClosedAgent);
        assert_eq!(r.mode_for("group:8").unwrap(), CanonicalMode::Simulation);
        assert_eq!(
            outcome.state.conversations[OWNER_KEY].updated_by.as_deref(),
            Some("migration")
        );
        assert_eq!(outcome.events[0].owner_key.as_deref(), Some(OWNER_KEY));
    }

    /// 无 owner 的 closed-agent 被降级并记录原因。
    #[test]
    fn legacy_closed_agent_without_owner_downgraded() {
        let outcome = migrate_legacy_mode("closed-agent", None).unwrap();
        let r = SessionRouter::new(SessionRouterOptions {
            state: Some(outcome.state.to_input()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r.mode_for(OWNER_KEY).unwrap(), CanonicalMode::Simulation);
        assert!(
            outcome.events[0]
                .reason
                .as_deref()
                .unwrap()
                .contains("missing ownerQQ")
        );
    }

    /// 未知 legacy 模式与不支持的版本 fail closed。
    #[test]
    fn unknown_legacy_mode_and_bad_version_fail_closed() {
        assert!(matches!(
            migrate_legacy_mode("administrator", Some(OWNER_QQ)),
            Err(RouterError::UnknownLegacyMode)
        ));
        let mut state = base_input(Some(42), true);
        state.version = 2;
        let err = SessionRouter::new(SessionRouterOptions {
            owner_qq: Some(OWNER_QQ),
            state: Some(state),
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("version 1"));
    }

    /// 别名重复键（'private:042' vs 'private:42'）被拒绝而非覆盖策略。
    #[test]
    fn aliased_duplicate_keys_rejected() {
        // 初态带 owner first-run 条目（private:42），'private:042' 与之 canonical 重复
        // ——与 Node 版测试的 createInitialConversationState(ownerQQ) 前提一致。
        let mut state = base_input(Some(42), true);
        state.conversations.insert(
            "private:042".to_owned(),
            policy(Some("simulation"), Some("qsh-sim"), Some("console")),
        );
        let err = SessionRouter::new(SessionRouterOptions {
            owner_qq: Some(OWNER_QQ),
            state: Some(state),
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, RouterError::DuplicateKey(_)));
        assert!(err.to_string().contains("Duplicate conversation key"));
    }

    // （Node 版「显式 null / 损坏 state 不触发 owner 首启特权」：null/false/缺字段
    // 的损坏形状由强类型输入排除——ConversationStateInput 无法表达；version 检查
    // 见 unknown_legacy_mode_and_bad_version_fail_closed。）

    /// 持久记录缺合法 updatedBy 被拒绝。
    #[test]
    fn persisted_record_requires_valid_updated_by() {
        let mut state = base_input(Some(42), false);
        state.conversations.insert(
            "group:8".to_owned(),
            policy(Some("simulation"), Some("qsh-sim"), None),
        );
        let err = SessionRouter::new(SessionRouterOptions {
            owner_qq: Some(OWNER_QQ),
            state: Some(state.clone()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("updatedBy"));
        // 非本会话 owner 的指令通道同样拒绝。
        state.conversations.insert(
            "group:8".to_owned(),
            policy(
                Some("simulation"),
                Some("qsh-sim"),
                Some("command:private:8"),
            ),
        );
        let err = SessionRouter::new(SessionRouterOptions {
            owner_qq: Some(OWNER_QQ),
            state: Some(state),
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("updatedBy"));
    }

    /// set_conversation 要求活体 actor，拒绝时状态不动。
    /// （undefined 由 Option 表达；first-run/migration 是供给路径专属。）
    #[test]
    fn set_conversation_requires_live_actor() {
        let mut r = router(Some(OWNER_QQ));
        let before = r.snapshot();
        for bad in [
            None,
            Some(""),
            Some("migration"),
            Some("first-run"),
            Some("command:private:8"),
        ] {
            let err = r
                .set_conversation(
                    "group:8",
                    policy(Some("simulation"), Some("qsh-sim-v2"), bad),
                    0,
                )
                .unwrap_err();
            assert!(err.to_string().contains("updatedBy"), "actor={bad:?}");
            assert_eq!(r.snapshot(), before);
        }
    }

    /// 合法 owner 指令的 set 返回 actor / 时间 / before-after。
    #[test]
    fn set_returns_structured_audit_event() {
        let mut r = router(Some(OWNER_QQ));
        let before = r.policy_for(OWNER_KEY).unwrap();
        let actor = format!("command:{OWNER_KEY}");
        let result = r
            .set_conversation(
                OWNER_KEY,
                policy(Some("simulation"), Some("qsh-sim"), Some(&actor)),
                1_750_000_000_000,
            )
            .unwrap();
        let after = r.policy_for(OWNER_KEY).unwrap();
        assert_eq!(result.changed_keys, [OWNER_KEY]);
        let e = &result.audit_event;
        assert_eq!(e.event_type, "conversation-set");
        assert_eq!(e.key, OWNER_KEY);
        assert_eq!(e.updated_by, actor);
        assert_eq!(e.at, 1_750_000_000_000);
        assert_eq!(e.before, before);
        assert_eq!(e.after, after);
        assert!(e.had_override);
        assert!(e.policy_changed);
        assert_eq!(e.before.mode, Mode::ClosedAgent);
        assert_eq!(e.after.mode, Mode::Simulation);
    }

    /// delete 要求 actor，返回结构化审计事件。
    #[test]
    fn delete_returns_structured_audit_event() {
        let mut r = router(Some(OWNER_QQ));
        let before_policy = r.policy_for(OWNER_KEY).unwrap();
        let before = r.snapshot();
        assert!(r.delete_conversation(OWNER_KEY, "", 0).is_err());
        assert!(
            r.delete_conversation(OWNER_KEY, "command:private:8", 0)
                .is_err()
        );
        assert_eq!(r.snapshot(), before);
        let result = r
            .delete_conversation(OWNER_KEY, "console", 1_750_000_000_001)
            .unwrap();
        let after = r.policy_for(OWNER_KEY).unwrap();
        assert_eq!(result.changed_keys, [OWNER_KEY]);
        let e = &result.audit_event;
        assert_eq!(e.event_type, "conversation-deleted");
        assert_eq!(e.key, OWNER_KEY);
        assert_eq!(e.updated_by, "console");
        assert_eq!(e.at, 1_750_000_000_001);
        assert_eq!(e.before, before_policy);
        assert_eq!(e.after, after);
        assert!(e.had_override);
        assert!(!e.no_op);
        assert!(e.policy_changed);
        assert_eq!(e.before.mode, Mode::ClosedAgent);
        assert_eq!(e.after.mode, Mode::Simulation);
    }

    /// 删除不存在的 override 显式为 no-op。
    #[test]
    fn deleting_absent_override_is_noop() {
        let mut r = router(Some(OWNER_QQ));
        let before = r.policy_for("group:8").unwrap();
        let result = r
            .delete_conversation("group:8", "console", 1_750_000_000_002)
            .unwrap();
        assert_eq!(result.changed_keys, Vec::<String>::new());
        let e = &result.audit_event;
        assert_eq!(e.key, "group:8");
        assert_eq!(e.at, 1_750_000_000_002);
        assert_eq!(e.before, before);
        assert_eq!(e.after, r.policy_for("group:8").unwrap());
        assert!(!e.had_override);
        assert!(e.no_op);
        assert!(!e.policy_changed);
    }

    // （Node 版「非法审计时钟不能提交变更」：NaN/负数时钟由 u64 参数类型排除。）

    /// fingerprint 格式与 Node 版 JSON.stringify 逐字段一致（跨实现对拍基础）。
    #[test]
    fn fingerprint_matches_node_format() {
        let r = router(Some(OWNER_QQ));
        // owner 私聊：closed-agent + qsh-closed + null/null + owner 42。
        assert_eq!(
            r.policy_fingerprint(OWNER_KEY).unwrap(),
            r#"["closed-agent","qsh-closed",null,null,42]"#
        );
        // 无 owner：末位为 null。
        let r2 = router(None);
        assert_eq!(
            r2.policy_fingerprint("group:8").unwrap(),
            r#"["simulation","qsh-sim",null,null,null]"#
        );
    }

    /// mode_allows_key：非法 key false；closed-agent 仅 owner 私聊放行。
    #[test]
    fn mode_allows_key_boundary() {
        let r = router(Some(OWNER_QQ));
        assert!(r.mode_allows_key("group:8"));
        assert!(r.mode_allows_key("private:8"));
        assert!(r.mode_allows_key(OWNER_KEY));
        assert!(!r.mode_allows_key("group:0"));
        let r2 = router(None);
        assert!(r2.mode_allows_key("group:8"));
    }
}
