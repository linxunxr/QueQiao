//! 配置加载与深度合并默认（规格 §十；形状兼容 Node 版 config.json，字段语义一致）。
//!
//! 深度合并语义：所有节/字段 `#[serde(default)]`——旧配置缺新键**不丢默认**
//!（Node 版实战教训）。默认值以 Node 版 bridge.js `loadConfig` 的代码默认为准
//!（config.example.json 是示例值，个别键与代码默认不同，如 social 段的
//! activeReplyDelay 与 skipProbability）。
//!
//! 热更：控制台写 config.json 即时生效（桥按 mtime 重载，见 [`ConfigFile`]）；
//! **绝不持久化**运行时字段（定时器句柄、回合状态等白名单外字段——serde 序列化
//! 输出仅用于控制台展示，落盘形状由调用方控制）。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

// ── ID 列表：数字或纯数字字符串，非数字项过滤（fail-closed 归调用方）──────────

fn deserialize_id_list<'de, D>(deserializer: D) -> Result<Vec<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RawList {
        Items(Vec<IdVal>),
        /// 非数组一律按空列表（Node 版 normalizeIdList 的宽容语义）。
        Other(serde::de::IgnoredAny),
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IdVal {
        Num(u64),
        Str(String),
    }
    let raw = RawList::deserialize(deserializer)?;
    Ok(match raw {
        RawList::Items(items) => items
            .into_iter()
            .filter_map(|v| match v {
                IdVal::Num(n) => Some(n),
                IdVal::Str(s) => s.trim().parse().ok(),
            })
            .collect(),
        RawList::Other(_) => Vec::new(),
    })
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AllowList {
    #[serde(deserialize_with = "deserialize_id_list", alias = "privates")]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub private: Vec<u64>,
    #[serde(deserialize_with = "deserialize_id_list", alias = "group")]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<u64>,
}

/// ownerQQ：数字或纯数字字符串；空/缺失为 None，非法值报错（配置错误必须显式失败）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OwnerQQ(pub u64);

fn deserialize_owner_qq<'de, D>(deserializer: D) -> Result<Option<OwnerQQ>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Num(u64),
        Str(String),
        Null,
    }
    match Option::<Raw>::deserialize(deserializer)? {
        None | Some(Raw::Null) => Ok(None),
        Some(Raw::Num(n)) if n > 0 => Ok(Some(OwnerQQ(n))),
        Some(Raw::Str(s)) if !s.trim().is_empty() => s
            .trim()
            .parse()
            .map(|n| Some(OwnerQQ(n)))
            .map_err(|_| serde::de::Error::custom("ownerQQ 必须是 QQ 号（正整数）")),
        Some(Raw::Num(_)) | Some(Raw::Str(_)) => {
            Err(serde::de::Error::custom("ownerQQ 必须是 QQ 号（正整数）"))
        }
    }
}

// ── 各节 ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DshConfig {
    pub base_url: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: String,
    pub auth_token: String,
    pub auth_header: String,
    pub auth_prefix: String,
    pub allow_remote: bool,
}

impl Default for DshConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:3080".into(),
            provider: "deepseek-official".into(),
            model: String::new(),
            reasoning_effort: "max".into(),
            auth_token: String::new(),
            auth_header: "authorization".into(),
            auth_prefix: "Bearer".into(),
            allow_remote: false,
        }
    }
}

impl DshConfig {
    /// 思考强度规范化：手工编辑可能写成非法值，DeepSeek 适配器会硬拒绝——
    /// 统一规范化并回退默认（Node 版 normalizeReasoningEffort 语义）。
    pub fn normalize_reasoning_effort(&mut self) {
        const DEFAULT: &str = "max";
        if !matches!(
            self.reasoning_effort.as_str(),
            "off" | "low" | "medium" | "high" | "max"
        ) {
            self.reasoning_effort = DEFAULT.into();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RoleConfig {
    pub max_inject_chars: usize,
}

impl Default for RoleConfig {
    fn default() -> Self {
        Self {
            max_inject_chars: 6000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SnowLumaConfig {
    pub ws_url: String,
    pub http_url: String,
    pub access_token: String,
    pub follow_discovered_endpoint: bool,
    pub silence_warn_ms: u64,
    pub launcher_path: String,
    pub home_dir: String,
}

impl Default for SnowLumaConfig {
    fn default() -> Self {
        Self {
            ws_url: "ws://127.0.0.1:3001".into(),
            http_url: "http://127.0.0.1:3000".into(),
            access_token: String::new(),
            follow_discovered_endpoint: true,
            silence_warn_ms: 600_000,
            launcher_path: String::new(),
            home_dir: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
/// 平板控制（tablet）：hdc 执行器在 queqiao::tablet，端点 /api/tablet/*，
/// MCP tablet_* 九工具；两层门禁=注册期 enabled + 调用期 allowConvKeys
/// （默认仅管理员私聊）。真机动作（截图/点击等）待设备连接后验证。
pub struct TabletConfig {
    pub enabled: bool,
    pub hdc_path: String,
    pub target: String,
    pub allow_conv_keys: Vec<String>,
    pub max_screenshot_bytes: u64,
}

impl Default for TabletConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hdc_path: String::new(),
            target: String::new(),
            allow_conv_keys: Vec::new(),
            max_screenshot_bytes: 4_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
/// 电脑控制（pc）：**未实现**——仅配置形状占位（同 tablet）。
pub struct PcConfig {
    pub enabled: bool,
    pub allow_conv_keys: Vec<String>,
    pub max_screenshot_bytes: u64,
}

impl Default for PcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_conv_keys: Vec::new(),
            max_screenshot_bytes: 4_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PriceTier {
    pub cache_hit: f64,
    pub cache_miss: f64,
    pub output: f64,
}

impl Default for PriceTier {
    fn default() -> Self {
        Self {
            cache_hit: 0.0,
            cache_miss: 0.0,
            output: 0.0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelPrice {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub label: String,
    pub off: PriceTier,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PricingConfig {
    pub models: BTreeMap<String, ModelPrice>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SecurityConfig {
    pub intercept_notify: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            intercept_notify: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SlangConfig {
    pub enabled: bool,
    // 学习管线参数全部生效：extractMinMessages/extractCooldownMs/learnerPreset/
    // workspaceTitle 由提取管线消费（滚动窗口攒消息 → DSH 学习会话提取候选）；
    // inferenceThresholds/autoResearch 由研究管线消费（阈值命中自动联网考究，
    // 研究结果写回释义、转正仍须管理员在词库页确认）。
    pub extract_min_messages: usize,
    pub extract_cooldown_ms: u64,
    pub inference_thresholds: Vec<u32>,
    pub inject_max: usize,
    pub learner_preset: String,
    pub workspace_title: String,
    pub auto_research: bool,
}

impl Default for SlangConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            extract_min_messages: 10,
            extract_cooldown_ms: 300_000,
            inference_thresholds: vec![2, 4, 8],
            inject_max: 8,
            learner_preset: "qq-chat".into(),
            workspace_title: "QQ 黑话学习".into(),
            auto_research: true,
        }
    }
}

/// 一代仿真参数（backlog：M1~M5 不实现一代状态机，但形状必须兼容加载）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SocialConfig {
    pub enabled: bool,
    pub max_reply_chars: usize,
    pub must_reply_keywords: Vec<String>,
    pub context_window: usize,
    pub trigger_probability: f64,
    pub active_check_min_ms: u64,
    pub active_check_max_ms: u64,
    pub active_reply_delay_min_ms: u64,
    pub active_reply_delay_max_ms: u64,
    pub active_duration_enabled: bool,
    pub active_duration_min_ms: u64,
    pub active_duration_max_ms: u64,
    pub idle_window_ms: u64,
    pub idle_retry_probability: f64,
    pub idle_retry_wait_ms: u64,
    pub proactive_enabled: bool,
    pub proactive_idle_threshold_ms: u64,
    pub proactive_check_min_ms: u64,
    pub proactive_check_max_ms: u64,
    pub proactive_probability: f64,
    pub skip_probability: f64,
    pub surrender_probability: f64,
    pub burst_enabled: bool,
    pub burst_interval_min_ms: u64,
    pub burst_interval_max_ms: u64,
    pub long_gap_probability: f64,
    pub long_gap_min_ms: u64,
    pub long_gap_max_ms: u64,
}

impl Default for SocialConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_reply_chars: 500,
            must_reply_keywords: vec![
                "deepseek".into(),
                "小鲸鱼".into(),
                "大肥鱼".into(),
                "鲸鱼".into(),
                "d指导".into(),
                "在吗".into(),
            ],
            context_window: 20,
            trigger_probability: 0.1,
            active_check_min_ms: 10_000,
            active_check_max_ms: 30_000,
            active_reply_delay_min_ms: 2_000,
            active_reply_delay_max_ms: 8_000,
            active_duration_enabled: true,
            active_duration_min_ms: 15 * 60_000,
            active_duration_max_ms: 30 * 60_000,
            idle_window_ms: 6 * 60_000,
            idle_retry_probability: 0.25,
            idle_retry_wait_ms: 2 * 60_000,
            proactive_enabled: true,
            proactive_idle_threshold_ms: 30 * 60_000,
            proactive_check_min_ms: 45 * 60_000,
            proactive_check_max_ms: 90 * 60_000,
            proactive_probability: 0.2,
            skip_probability: 0.15,
            surrender_probability: 0.0,
            burst_enabled: true,
            burst_interval_min_ms: 1_000,
            burst_interval_max_ms: 3_000,
            long_gap_probability: 0.1,
            long_gap_min_ms: 2_500,
            long_gap_max_ms: 5_000,
        }
    }
}

/// MCP 工具注册期开关（38 个；false 的工具不注册——描述与 schema 都不进模型上下文）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ToolsConfig {
    pub get_prompt: bool,
    pub get_unread: bool,
    pub get_recent: bool,
    pub social_state: bool,
    pub send_group: bool,
    pub send_private: bool,
    pub reply: bool,
    pub send_burst: bool,
    pub send_message: bool,
    pub wait_messages: bool,
    pub feedback: bool,
    pub get_my_recent: bool,
    pub get_message_detail: bool,
    pub get_active_members: bool,
    pub set_wake_config: bool,
    pub mark_read: bool,
    pub memory: bool,
    pub slang_query: bool,
    pub slang_submit: bool,
    pub get_images: bool,
    pub get_forward_msg: bool,
    pub send_poke: bool,
    pub react_message: bool,
    pub recall_message: bool,
    pub group_profile: bool,
    pub list_stickers: bool,
    pub get_sticker_image: bool,
    pub send_sticker: bool,
    pub set_sticker_remark: bool,
    pub sticker_note: bool,
    pub collect_sticker: bool,
    pub fetch_sticker: bool,
    pub get_self_image: bool,
    pub write_diary: bool,
    pub get_diary: bool,
    pub group_members: bool,
    pub group_history: bool,
    pub send_voice: bool,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            get_prompt: true,
            get_unread: true,
            get_recent: true,
            social_state: true,
            send_group: true,
            send_private: true,
            reply: true,
            send_burst: true,
            send_message: true,
            wait_messages: true,
            feedback: true,
            get_my_recent: true,
            get_message_detail: true,
            get_active_members: true,
            set_wake_config: true,
            mark_read: true,
            memory: true,
            slang_query: true,
            slang_submit: true,
            get_images: true,
            get_forward_msg: true,
            send_poke: true,
            react_message: true,
            recall_message: true,
            group_profile: true,
            list_stickers: true,
            get_sticker_image: true,
            send_sticker: true,
            set_sticker_remark: false,
            sticker_note: true,
            collect_sticker: true,
            fetch_sticker: true,
            get_self_image: true,
            write_diary: true,
            get_diary: true,
            group_members: true,
            group_history: true,
            send_voice: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WakeConfig {
    pub default_mode: String,
    pub pre_sleep_wait_enabled: bool,
    pub pre_sleep_wait_ms: u64,
    pub recommended_default_infinite: bool,
    pub sleep_min_ms: u64,
    pub sleep_max_ms: u64,
    pub recommended_sleep_min_ms: u64,
    pub recommended_sleep_max_ms: u64,
    pub recommended_probability: f64,
    pub recommended_keywords: Vec<String>,
    pub recommended_at_mention: bool,
    pub recommended_name_mention: bool,
    pub recommended_question: bool,
    pub recommended_poke: bool,
    pub recommended_hint: String,
    pub batch_window_ms: u64,
    pub max_wake_per_minute: u32,
    pub max_wake_per_hour: u32,
    pub no_action_limit: u32,
    pub max_wake_config_reminders: u32,
}

impl Default for WakeConfig {
    fn default() -> Self {
        Self {
            default_mode: "diving".into(),
            pre_sleep_wait_enabled: true,
            pre_sleep_wait_ms: 300_000,
            recommended_default_infinite: true,
            sleep_min_ms: 60_000,
            sleep_max_ms: 0,
            recommended_sleep_min_ms: 300_000,
            recommended_sleep_max_ms: 7_200_000,
            recommended_probability: 0.05,
            recommended_keywords: vec![
                "小鲸鱼".into(),
                "DeepSeek".into(),
                "deepseek".into(),
                "DS".into(),
                "D老师".into(),
                "d老师".into(),
                "D指导".into(),
                "d指导".into(),
                "D师傅".into(),
                "d师傅".into(),
                "深度求索".into(),
                "大肥鱼".into(),
                "鲸鱼".into(),
                "DeepSeek V3".into(),
                "DeepSeek R1".into(),
                "R1".into(),
            ],
            recommended_at_mention: true,
            recommended_name_mention: true,
            recommended_question: true,
            recommended_poke: true,
            recommended_hint: "如果你要潜水，推荐先调用 qq_wait_for_messages(timeoutMs=300000) 完成一次沉睡前观察：5 分钟内没人说话就可以设置下一次唤醒并沉睡；若期间有人发新消息，先查看 newMessages，判断不需要你参与可直接沉睡，若参与了则下次想睡需再等观察窗口。潜水时长推荐 5~120 分钟，普通消息概率 0.05；@/名字/关键词/提问唤醒建议保持开启。需要等特定某人/某几人时，可额外设置 triggers.speakerIds。".into(),
            batch_window_ms: 8_000,
            max_wake_per_minute: 1,
            max_wake_per_hour: 12,
            no_action_limit: 3,
            max_wake_config_reminders: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SendConfig {
    pub burst_enabled: bool,
    pub burst_max_messages: usize,
    pub burst_interval_min_ms: u64,
    pub burst_interval_max_ms: u64,
    pub long_gap_probability: f64,
    pub long_gap_min_ms: u64,
    pub long_gap_max_ms: u64,
    pub max_send_per_minute: u32,
    pub max_send_per_hour: u32,
    pub max_message_chars: usize,
    pub max_gap_ms: u64,
    pub gap_base_ms: u64,
    pub gap_per_char_ms: u64,
    pub recommended_hint: String,
}

impl Default for SendConfig {
    fn default() -> Self {
        Self {
            burst_enabled: true,
            burst_max_messages: 8,
            burst_interval_min_ms: 1_000,
            burst_interval_max_ms: 3_000,
            long_gap_probability: 0.2,
            long_gap_min_ms: 5_000,
            long_gap_max_ms: 10_000,
            max_send_per_minute: 8,
            max_send_per_hour: 60,
            max_message_chars: 500,
            max_gap_ms: 10_000,
            gap_base_ms: 800,
            gap_per_char_ms: 20,
            recommended_hint:
                "普通闲聊建议一次 1~3 条，条间 1~3 秒；讲故事/回忆可以 5~10 秒间隔；不要连续刷屏。"
                    .into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WaitConfig {
    pub default_ms: u64,
    pub min_ms: u64,
    pub max_ms: u64,
    pub default_quiet_ms: u64,
    pub min_quiet_after_new_ms: u64,
}

impl Default for WaitConfig {
    fn default() -> Self {
        Self {
            default_ms: 30_000,
            min_ms: 5_000,
            max_ms: 600_000,
            default_quiet_ms: 8_000,
            min_quiet_after_new_ms: 10_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StickerCollectConfig {
    pub enabled: bool,
    pub max_per_minute: u32,
    pub max_per_hour: u32,
    pub max_remark_chars: usize,
}

impl Default for StickerCollectConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_per_minute: 2,
            max_per_hour: 10,
            max_remark_chars: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StickerFetchConfig {
    pub enabled: bool,
    pub max_bytes: u64,
}

impl Default for StickerFetchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StickerConfig {
    pub enabled: bool,
    pub sync_ttl_ms: u64,
    pub max_list_count: usize,
    pub include_in_prompt: bool,
    pub prompt_max_stickers: usize,
    pub collect: StickerCollectConfig,
    pub fetch: StickerFetchConfig,
}

impl Default for StickerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sync_ttl_ms: 60_000,
            max_list_count: 100,
            include_in_prompt: true,
            prompt_max_stickers: 8,
            collect: StickerCollectConfig::default(),
            fetch: StickerFetchConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct VoiceTtsConfig {
    pub enabled: bool,
    pub voice: String,
    pub max_chars: usize,
    pub rate: i32,
}

impl Default for VoiceTtsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            voice: "Microsoft Huihui Desktop".into(),
            max_chars: 400,
            rate: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct VoiceConfig {
    pub enabled: bool,
    pub dir: String,
    pub max_seconds: u64,
    pub max_bytes: u64,
    pub max_per_minute: u32,
    pub max_per_hour: u32,
    pub allow_absolute_path: bool,
    pub default_volume: f64,
    pub loudness: String,
    pub ffmpeg_path: String,
    pub auto_readback: bool,
    pub tts: VoiceTtsConfig,
    pub asr_enabled: bool,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: "audio".into(),
            max_seconds: 300,
            max_bytes: 20 * 1024 * 1024,
            max_per_minute: 2,
            max_per_hour: 10,
            allow_absolute_path: false,
            default_volume: 1.0,
            loudness: "normal".into(),
            ffmpeg_path: String::new(),
            // 收语音 ASR 转写与发送后自动回听未实现（queqiao-media 占位），
            // 默认关防误导；实现后随功能放开。
            auto_readback: false,
            tts: VoiceTtsConfig::default(),
            asr_enabled: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProactiveConfig {
    pub enabled: bool,
    pub check_interval_min_ms: u64,
    pub check_interval_max_ms: u64,
    pub idle_threshold_ms: u64,
    pub probability: f64,
}

impl Default for ProactiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_min_ms: 30 * 60_000,
            check_interval_max_ms: 90 * 60_000,
            idle_threshold_ms: 15 * 60_000,
            probability: 0.3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FeedbackConfig {
    pub max_length: usize,
    pub notify_owner_on_error: bool,
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            max_length: 500,
            notify_owner_on_error: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContextConfig {
    pub recent_limit: usize,
    pub unread_limit: usize,
    pub context_window: usize,
    pub inline_wake_messages: bool,
    pub wake_message_limit: usize,
    pub wake_recent_limit: usize,
    pub wake_max_chars: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            recent_limit: 100,
            unread_limit: 30,
            context_window: 20,
            inline_wake_messages: true,
            wake_message_limit: 12,
            wake_recent_limit: 4,
            wake_max_chars: 6000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DiaryConfig {
    pub enabled: bool,
    pub hour: u8,
    pub not_before_hour: u8,
    pub quiet_ms: u64,
    pub max_chars: usize,
}

impl Default for DiaryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hour: 23,
            not_before_hour: 18,
            quiet_ms: 3_600_000,
            max_chars: 2000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SocialV2Config {
    pub enabled: bool,
    pub auto_reply_check_ms: u64,
    pub agent_preset: String,
    pub provide_recommendations: bool,
    pub tools: ToolsConfig,
    pub wake: WakeConfig,
    pub send: SendConfig,
    pub wait: WaitConfig,
    pub sticker: StickerConfig,
    pub voice: VoiceConfig,
    pub proactive: ProactiveConfig,
    pub feedback: FeedbackConfig,
    pub context: ContextConfig,
    pub diary: DiaryConfig,
}

impl Default for SocialV2Config {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_reply_check_ms: 30_000,
            agent_preset: "qq-chat-v2".into(),
            provide_recommendations: true,
            tools: ToolsConfig::default(),
            wake: WakeConfig::default(),
            send: SendConfig::default(),
            wait: WaitConfig::default(),
            sticker: StickerConfig::default(),
            voice: VoiceConfig::default(),
            proactive: ProactiveConfig::default(),
            feedback: FeedbackConfig::default(),
            context: ContextConfig::default(),
            diary: DiaryConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WatchdogConfig {
    pub enabled: bool,
    pub interval_sec: u64,
    // 自动重启未实现（当前看门狗仅探测+告警直发；重启不接管是授权边界内的
    // 有意设计，放开前须重评）——字段保留兼容形状。
    pub auto_restart: bool,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_sec: 60,
            auto_restart: false,
        }
    }
}

// ── 顶层 ─────────────────────────────────────────────────────────────────────

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    pub dsh: DshConfig,
    pub role: RoleConfig,
    pub snowluma: SnowLumaConfig,
    pub tablet: TabletConfig,
    pub pc: PcConfig,
    #[serde(rename = "ownerQQ", deserialize_with = "deserialize_owner_qq", default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_qq: Option<OwnerQQ>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub session_cwd: String,
    pub agent_preset: String,
    pub workspace_title: String,
    pub allow: AllowList,
    pub deny: AllowList,
    pub allow_all_when_empty: bool,
    pub ack_message: String,
    pub send_delay_ms: u64,
    pub question_timeout_ms: u64,
    pub console_port: u16,
    pub console_token: String,
    /// 桥的定时自动检查更新（每小时轮询 + 空闲自动应用）；关闭后仅客户端
    /// 「检查更新」手动升级。默认开。
    #[serde(default = "default_true")]
    pub auto_update: bool,
    pub pricing: PricingConfig,
    pub security: SecurityConfig,
    pub slang: SlangConfig,
    pub social: SocialConfig,
    pub social_v2: SocialV2Config,
    pub watchdog: WatchdogConfig,
}

impl Default for Config {
    /// 全字段显式列出（不委托 serde——容器级 `#[serde(default)]` 会调本方法，
    /// 经 serde 转发就是无限递归 / 栈溢出）。
    fn default() -> Self {
        Self {
            dsh: Default::default(),
            role: Default::default(),
            snowluma: Default::default(),
            tablet: Default::default(),
            pc: Default::default(),
            owner_qq: None,
            session_cwd: String::new(),
            agent_preset: "qq-chat".into(),
            workspace_title: "QQ 聊天".into(),
            allow: Default::default(),
            deny: Default::default(),
            allow_all_when_empty: false,
            ack_message: "🤔 收到，正在思考…".into(),
            send_delay_ms: 300,
            question_timeout_ms: 5 * 60_000,
            console_port: 3100,
            console_token: String::new(),
            auto_update: true,
            pricing: Default::default(),
            security: Default::default(),
            slang: Default::default(),
            social: Default::default(),
            social_v2: Default::default(),
            watchdog: Default::default(),
        }
    }
}

impl Config {
    /// 从 JSON 文本加载（深度合并默认由 serde default 承担）+ 加载期规范化。
    pub fn from_json_text(text: &str) -> Result<Self, String> {
        let text = text.trim_start_matches('\u{FEFF}');
        let mut cfg: Config =
            serde_json::from_str(text).map_err(|e| format!("config.json 解析失败：{e}"))?;
        cfg.dsh.normalize_reasoning_effort();
        Ok(cfg)
    }

    pub fn to_json_text(&self) -> String {
        let mut out = serde_json::to_string_pretty(self).expect("序列化必然成功");
        out.push('\n');
        out
    }

    /// 会话准入统一判定（Node allowed() 语义）：deny 命中一票否决；
    /// allow 命中放行；两表皆空且 allowAllWhenEmpty=true 时放行全部
    /// （配合运行时动态注册）。键形如 `group:123` / `private:456`。
    pub fn conversation_allowed(&self, key: &str) -> bool {
        let Some((kind, id)) = key.split_once(':') else {
            return false;
        };
        let Ok(id) = id.parse::<u64>() else {
            return false;
        };
        let (deny_hit, allow_hit) = match kind {
            "group" => (
                self.deny.groups.contains(&id),
                self.allow.groups.contains(&id),
            ),
            "private" => (
                self.deny.private.contains(&id),
                self.allow.private.contains(&id),
            ),
            _ => return false,
        };
        if deny_hit {
            return false;
        }
        if allow_hit {
            return true;
        }
        self.allow.groups.is_empty() && self.allow.private.is_empty() && self.allow_all_when_empty
    }

    /// pricing.models → 花费记账自定义价目表（cacheMiss 档兼盖 cacheWrite，
    /// 与内置档第二桶同义）。
    pub fn price_table(&self) -> queqiao_core::token_ledger::PriceTable {
        self.pricing
            .models
            .iter()
            .map(|(k, v)| (k.clone(), (v.off.cache_hit, v.off.cache_miss, v.off.output)))
            .collect()
    }
}

// ── 文件层：mtime 热更 ───────────────────────────────────────────────────────

/// 配置文件句柄：按 mtime 检测变更（控制台写 config.json 即时生效）。
#[derive(Debug)]
pub struct ConfigFile {
    path: std::path::PathBuf,
    config: Config,
    mtime: Option<std::time::SystemTime>,
}

impl ConfigFile {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读 {} 失败：{e}", path.display()))?;
        let config = Config::from_json_text(&text)?;
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        Ok(Self {
            path: path.to_owned(),
            config,
            mtime,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// mtime 有变化则重载（解析失败保留旧配置并返回错误——坏配置不劫持运行中的桥）。
    pub fn reload_if_changed(&mut self) -> Result<bool, String> {
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if mtime == self.mtime {
            return Ok(false);
        }
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("重载 {} 失败：{e}", self.path.display()))?;
        let next = Config::from_json_text(&text)?;
        self.config = next;
        self.mtime = mtime;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Node 版 config.example.json 全量反序列化（真实样例钉形状，规格 §八.2）。
    #[test]
    fn parses_node_example_config() {
        let text = include_str!("../tests/fixtures/config.example.json");
        let cfg = Config::from_json_text(text).expect("example 配置必然可解析");
        assert_eq!(cfg.dsh.base_url, "http://127.0.0.1:3080");
        assert_eq!(cfg.dsh.provider, "deepseek-official");
        assert_eq!(cfg.dsh.model, "deepseek-flash");
        assert_eq!(cfg.console_port, 3100);
        assert_eq!(cfg.role.max_inject_chars, 6000);
        assert_eq!(cfg.owner_qq, None, "example 的 ownerQQ 为 null");
        assert!(cfg.allow.groups.is_empty() && cfg.deny.groups.is_empty());
        assert_eq!(cfg.social_v2.agent_preset, "qq-chat-v2");
        assert_eq!(cfg.social_v2.wake.batch_window_ms, 8000);
        assert_eq!(cfg.social_v2.diary.hour, 23);
        assert!(!cfg.social_v2.tools.set_sticker_remark);
        assert!(!cfg.social_v2.tools.send_voice);
        assert!(cfg.social_v2.tools.get_prompt, "example 保持默认 true");
        assert_eq!(cfg.watchdog.interval_sec, 60);
        assert_eq!(cfg.pricing.models.len(), 1);
        let price = &cfg.pricing.models["my-relay-model"];
        assert_eq!(price.off.cache_miss, 3.0);
    }

    /// 深度合并默认：空 JSON → 全默认；旧配置缺新键不丢默认。
    #[test]
    fn missing_keys_fall_back_to_defaults() {
        let cfg: Config = Config::from_json_text("{}").unwrap();
        assert_eq!(cfg.dsh.base_url, "http://127.0.0.1:3080");
        assert_eq!(cfg.dsh.reasoning_effort, "max");
        assert_eq!(cfg.console_port, 3100);
        assert_eq!(cfg.ack_message, "🤔 收到，正在思考…");
        assert_eq!(cfg.agent_preset, "qq-chat");
        assert!(cfg.social_v2.tools.get_prompt, "工具默认开");
        assert!(!cfg.social_v2.tools.set_sticker_remark);
        assert_eq!(cfg.social_v2.wake.pre_sleep_wait_ms, 300_000);
        assert_eq!(cfg.social_v2.diary.quiet_ms, 3_600_000);
        assert_eq!(cfg.social_v2.voice.tts.rate, 0);

        // 只写个别键：其余保持默认（Node 版深度合并语义）。
        let partial = r#"{"consolePort": 9999, "socialV2": {"wake": {"batchWindowMs": 5000}}}"#;
        let cfg = Config::from_json_text(partial).unwrap();
        assert_eq!(cfg.console_port, 9999);
        assert_eq!(cfg.social_v2.wake.batch_window_ms, 5000);
        assert_eq!(
            cfg.social_v2.wake.max_wake_per_minute, 1,
            "同节未覆盖键保持默认"
        );
        assert_eq!(cfg.social_v2.send.max_message_chars, 500, "姊妹节不受影响");
        assert_eq!(cfg.dsh.model, "", "未配置的字符串默认空");
    }

    /// ownerQQ：数字/字符串/空；非法值显式报错。
    #[test]
    fn owner_qq_normalization() {
        let cfg = Config::from_json_text(r#"{"ownerQQ": 10001}"#).unwrap();
        assert_eq!(cfg.owner_qq, Some(OwnerQQ(10001)));
        let cfg = Config::from_json_text(r#"{"ownerQQ": "10001"}"#).unwrap();
        assert_eq!(cfg.owner_qq, Some(OwnerQQ(10001)));
        let cfg = Config::from_json_text(r#"{"ownerQQ": null}"#).unwrap();
        assert_eq!(cfg.owner_qq, None);
        assert!(Config::from_json_text(r#"{"ownerQQ": "abc"}"#).is_err());
        assert!(Config::from_json_text(r#"{"ownerQQ": 0}"#).is_err());
    }

    /// 白名单：数字与字符串 id 混排、非数字过滤、privates/group 别名。
    #[test]
    fn allow_lists_accept_aliases_and_mixed_ids() {
        let cfg = Config::from_json_text(
            r#"{"allow": {"private": [10001, "10002", "x"], "groups": [4242]}, "deny": {"privates": ["9"], "group": [8]}}"#,
        )
        .unwrap();
        assert_eq!(cfg.allow.private, [10001, 10002], "非数字项被过滤");
        assert_eq!(cfg.allow.groups, [4242]);
        assert_eq!(cfg.deny.private, [9], "privates 别名");
        assert_eq!(cfg.deny.groups, [8], "group 别名");
        // 非数组形态宽容为空列表（Node normalizeIdList 语义）。
        let cfg = Config::from_json_text(r#"{"allow": {"groups": 4242}}"#).unwrap();
        assert!(cfg.allow.groups.is_empty());
    }

    /// reasoningEffort 规范化：非法回退 max（防 DeepSeek 适配器硬拒绝）。
    #[test]
    fn reasoning_effort_falls_back() {
        let cfg = Config::from_json_text(r#"{"dsh": {"reasoningEffort": "ultra"}}"#).unwrap();
        assert_eq!(cfg.dsh.reasoning_effort, "max");
        let cfg = Config::from_json_text(r#"{"dsh": {"reasoningEffort": "low"}}"#).unwrap();
        assert_eq!(cfg.dsh.reasoning_effort, "low");
    }

    /// BOM 容忍（Windows 编辑器常写 BOM）。
    #[test]
    fn tolerates_bom() {
        let cfg = Config::from_json_text("\u{FEFF}{\"consolePort\": 1}").unwrap();
        assert_eq!(cfg.console_port, 1);
    }

    /// autoUpdate 缺省开；显式关闭要能解析回读（轮询热更读的就是这个键）。
    #[test]
    fn auto_update_flag_roundtrip() {
        let cfg = Config::from_json_text("{}").unwrap();
        assert!(cfg.auto_update, "缺省必须为开（存量 config 无此键）");
        let off = Config::from_json_text("{\"autoUpdate\": false}").unwrap();
        assert!(!off.auto_update);
        assert!(
            serde_json::to_value(&off)
                .unwrap()
                .get("autoUpdate")
                .and_then(|b| b.as_bool())
                .is_some_and(|b| !b)
        );
    }
}
#[cfg(test)]
mod admission_tests {
    use super::*;

    fn cfg_with(allow_groups: &[u64], deny_groups: &[u64], allow_all: bool) -> Config {
        let mut c = Config::default();
        c.allow.groups = allow_groups.to_vec();
        c.deny.groups = deny_groups.to_vec();
        c.allow_all_when_empty = allow_all;
        c
    }

    /// Node allowed() 语义：deny 一票否决 / allow 命中放行 / 空表+开关放行全部。
    #[test]
    fn conversation_allowed_node_semantics() {
        let c = cfg_with(&[100], &[200], false);
        assert!(c.conversation_allowed("group:100"));
        assert!(!c.conversation_allowed("group:200"), "deny 命中拒绝");
        assert!(!c.conversation_allowed("group:300"), "未列入拒绝");
        // 同时出现在 allow 与 deny：deny 优先。
        let conflict = cfg_with(&[200], &[200], false);
        assert!(!conflict.conversation_allowed("group:200"));
        // 两表皆空：默认拒绝，allowAllWhenEmpty=true 放行（deny 仍拒）。
        let empty = cfg_with(&[], &[], false);
        assert!(!empty.conversation_allowed("group:1"));
        let open = cfg_with(&[], &[9], true);
        assert!(open.conversation_allowed("group:1"));
        assert!(open.conversation_allowed("private:2"));
        assert!(!open.conversation_allowed("group:9"), "开放态 deny 仍拒");
        // 键形非法。
        assert!(!open.conversation_allowed("channel:1"));
        assert!(!open.conversation_allowed("group:abc"));
    }
}
