//! 日记三路径调度与存储模型（移植自 Node 版 `src/diary-lib.js`，契约测试同步迁移）。
//!
//! 日记是 AI（同一个人格）跨会话的一本统一日记：每天至多一篇，按本地日期去重，
//! 追加写 state/diary.jsonl（每行 `{ date, content, key, createdAt }`）。
//!
//! 分层调整：Node 版的 `readDiary`/`appendDiary` 直连 fs，这里拆为「解析/构建」
//! 纯函数（本模块）与文件读写（bin 层），行为契约不变。本地时区换算同理由
//! 调用方完成——core 只接收民用时间（[`CivilTime`]）与注入的绝对毫秒。

use serde::{Deserialize, Serialize};

/// 单篇日记长度上限（超出截断）。
pub const DIARY_MAX_CHARS: usize = 2000;

/// 凌晨窗口边界：0~4 点的日记仍归属刚结束的那一天。
const DAWN_HOUR: u8 = 4;

/// 本地民用日期（1-based；时区换算是环境依赖，由 bin 层完成）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilDate {
    pub year: i32,
    pub month: u8,
    pub day: u8,
}

/// 本地民用时刻：日期 + 时分（决策只需到分钟）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilTime {
    pub date: CivilDate,
    pub hour: u8,
    pub minute: u8,
}

/// 一篇日记（与 Node 版 jsonl 行形状一致，序列化字段名保持 createdAt）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiaryEntry {
    pub date: String,
    pub content: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub created_at: u64,
}

/// 追加被拒的原因。`Display` 文案与 Node 版一字不差——错误信息会回传给 agent。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiaryAppendError {
    AlreadyWritten { day: String },
    Empty,
}

impl std::fmt::Display for DiaryAppendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiaryAppendError::AlreadyWritten { day } => {
                write!(f, "今天（{day}）已经写过日记了，明天再来")
            }
            DiaryAppendError::Empty => write!(f, "日记内容为空"),
        }
    }
}

impl std::error::Error for DiaryAppendError {}

/// 本地日期键（yyyy-mm-dd）。
pub fn diary_date_key(d: CivilDate) -> String {
    format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
}

/// 当前归属的日记日：凌晨 0~4 点归昨天，其余归今天——那会儿写的日记记的是
/// 刚结束的那一天，别让 date 落到新的一天去。
pub fn current_diary_day(t: CivilTime) -> CivilDate {
    if t.hour < DAWN_HOUR {
        prev_day(t.date)
    } else {
        t.date
    }
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// 前一天（民用日期减一天；Node 版以绝对毫秒减 24h 实现，无夏令时时区下等价）。
/// bin 的 write-diary 端点用它做「凌晨 0~4 点归昨天」的日期回退。
pub fn prev_day(d: CivilDate) -> CivilDate {
    if d.day > 1 {
        CivilDate {
            day: d.day - 1,
            ..d
        }
    } else if d.month > 1 {
        let month = d.month - 1;
        CivilDate {
            day: days_in_month(d.year, month),
            month,
            ..d
        }
    } else {
        let year = d.year - 1;
        CivilDate {
            year,
            month: 12,
            day: days_in_month(year, 12),
        }
    }
}

/// 解析 state/diary.jsonl 文本为条目列表：跳过空行、坏 JSON 与缺 date/content 的行
///（坏行容忍契约：手工编辑或写入中断不炸整本日记）。
pub fn parse_diary(text: &str) -> Vec<DiaryEntry> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(t) else {
            continue;
        };
        let (Some(date), Some(content)) = (
            v.get("date").and_then(|x| x.as_str()),
            v.get("content").and_then(|x| x.as_str()),
        ) else {
            continue;
        };
        out.push(DiaryEntry {
            date: date.to_owned(),
            content: content.to_owned(),
            key: v
                .get("key")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            created_at: v.get("createdAt").and_then(|x| x.as_u64()).unwrap_or(0),
        });
    }
    out
}

/// 构建一篇待落盘的日记（`appendDiary` 的纯逻辑部分；落盘由 bin 层执行）。
///
/// 同日已有日记时拒绝；内容去 NUL、trim、截断到 [`DIARY_MAX_CHARS`]，截断后
/// 为空则拒绝。`day` 为日期键（Node 版的「date 缺省回退当前日」由调用方承担）。
pub fn build_diary_entry(
    entries: &[DiaryEntry],
    day: &str,
    content: &str,
    key: &str,
    now_ms: u64,
) -> Result<DiaryEntry, DiaryAppendError> {
    if entries.iter().any(|e| e.date == day) {
        return Err(DiaryAppendError::AlreadyWritten {
            day: day.to_owned(),
        });
    }
    let clean: String = content.chars().filter(|&c| c != '\u{0000}').collect();
    let clean = clean.trim();
    let clean: String = clean.chars().take(DIARY_MAX_CHARS).collect();
    if clean.is_empty() {
        return Err(DiaryAppendError::Empty);
    }
    Ok(DiaryEntry {
        date: day.to_owned(),
        content: clean,
        key: key.to_owned(),
        created_at: now_ms,
    })
}

/// 取最近 `days` 篇（时间正序）；`excerpt_chars > 0` 时内容截前 N 字加省略号
/// 作锚点，0 表示不截断。
pub fn recent_diary(entries: &[DiaryEntry], days: usize, excerpt_chars: usize) -> Vec<DiaryEntry> {
    let start = entries.len().saturating_sub(days.max(1));
    entries[start..]
        .iter()
        .map(|e| {
            let mut e = e.clone();
            if excerpt_chars > 0 && e.content.chars().count() > excerpt_chars {
                e.content = e.content.chars().take(excerpt_chars).collect::<String>() + "…";
            }
            e
        })
        .collect()
}

/// 「当前日记日」是否已写过（调度器判定用）。
pub fn diary_written_today(entries: &[DiaryEntry], now: CivilTime) -> bool {
    let day = diary_date_key(current_diary_day(now));
    entries.iter().any(|e| e.date == day)
}

/// 日记触发方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiaryTickMode {
    /// 到 diary.hour（默认 23 点）即写；凌晨 0~4 点仍算「一天刚结束」。
    Scheduled,
    /// 不早于 notBeforeHour 且会话已安静 quietMs——「今晚的下半场大概率没了」，提前写。
    Quiet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiaryTickDecision {
    Write(DiaryTickMode),
    Skip,
}

/// 日记触发判定入参（桥内 60s 轮询调用；writtenToday/lastTriedAt 由调用方维护）。
#[derive(Debug, Clone, Copy)]
pub struct DiaryTickInput {
    /// 当前绝对毫秒（冷却与安静差值判定）。
    pub now_ms: u64,
    /// 当前本地时刻（民用时间，bin 层换算）。
    pub now_local: CivilTime,
    /// 最近一次会话活动的绝对毫秒；0 表示无活动记录。
    pub last_activity_at: u64,
    /// 当前日记日是否已写过。
    pub written_today: bool,
    /// 上次尝试触发的绝对毫秒；0 表示未尝试过。
    pub last_tried_at: u64,
    /// 同日重试冷却（默认 1h）。
    pub retry_cooldown_ms: u64,
    /// 定点小时（默认 23）。
    pub hour: u8,
    /// 安静提前最早小时（默认 18）。
    pub not_before_hour: u8,
    /// 安静时长（默认 60min）。
    pub quiet_ms: u64,
}

impl Default for DiaryTickInput {
    fn default() -> Self {
        Self {
            now_ms: 0,
            now_local: CivilTime {
                date: CivilDate {
                    year: 1970,
                    month: 1,
                    day: 1,
                },
                hour: 0,
                minute: 0,
            },
            last_activity_at: 0,
            written_today: false,
            last_tried_at: 0,
            retry_cooldown_ms: 60 * 60 * 1000,
            hour: 23,
            not_before_hour: 18,
            quiet_ms: 60 * 60 * 1000,
        }
    }
}

/// 日记触发判定（三路径之二：定点 / 安静提前；第三路径「启动补写」见
/// [`diary_catchup_decision`]）。日记不该死等深夜定点——运行环境常在触发点之前关机。
pub fn diary_tick_decision(input: &DiaryTickInput) -> DiaryTickDecision {
    if input.written_today {
        return DiaryTickDecision::Skip;
    }
    if input.last_tried_at != 0
        && input.now_ms.saturating_sub(input.last_tried_at) < input.retry_cooldown_ms
    {
        return DiaryTickDecision::Skip;
    }
    let h = input.now_local.hour as f64 + input.now_local.minute as f64 / 60.0;
    // 凌晨 0~4 点仍算「一天刚结束」：昨天的日记还没写就还能写（date 由调用方锁定为昨天）。
    if h >= input.hour as f64 || h < DAWN_HOUR as f64 {
        return DiaryTickDecision::Write(DiaryTickMode::Scheduled);
    }
    if input.last_activity_at > 0
        && h >= input.not_before_hour as f64
        && input.now_ms.saturating_sub(input.last_activity_at) >= input.quiet_ms
    {
        return DiaryTickDecision::Write(DiaryTickMode::Quiet);
    }
    DiaryTickDecision::Skip
}

/// 启动补写判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiaryCatchup {
    None,
    /// 补写昨天（date 为昨天的日期键；模型不可指定）。
    Yesterday {
        date: String,
    },
}

/// 启动补写判定：昨晚关机错过了日记、今早开机补昨天。
/// 条件：昨天有会话活动、昨天的日记缺失、今天还没写（今天的日记仍按当晚节奏写）。
pub fn diary_catchup_decision(
    today: CivilDate,
    yesterday_activity_count: u32,
    entries: &[DiaryEntry],
) -> DiaryCatchup {
    if yesterday_activity_count == 0 {
        return DiaryCatchup::None;
    }
    let y_key = diary_date_key(prev_day(today));
    if entries.iter().any(|e| e.date == y_key) {
        return DiaryCatchup::None;
    }
    if entries.iter().any(|e| e.date == diary_date_key(today)) {
        return DiaryCatchup::None;
    }
    DiaryCatchup::Yesterday { date: y_key }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u8, d: u8) -> CivilDate {
        CivilDate {
            year: y,
            month: m,
            day: d,
        }
    }

    fn time(y: i32, m: u8, d: u8, h: u8, min: u8) -> CivilTime {
        CivilTime {
            date: date(y, m, d),
            hour: h,
            minute: min,
        }
    }

    /// 当天某时刻的绝对毫秒（以当日 0 点为基准），等价 Node 版测试的 at(h, m)。
    fn at(h: u8, m: u8) -> u64 {
        (h as u64) * 3_600_000 + (m as u64) * 60_000
    }

    fn tick(now_ms: u64, now_local: CivilTime, last_act: u64) -> DiaryTickInput {
        DiaryTickInput {
            now_ms,
            now_local,
            last_activity_at: last_act,
            ..Default::default()
        }
    }

    /// 契约与 Node 版 test-diary-lib 一致：18 点前不提前写（哪怕已安静）。
    #[test]
    fn quiet_path_not_before_18() {
        let input = tick(at(17, 59), time(2026, 10, 19, 17, 59), at(17, 30));
        assert_eq!(diary_tick_decision(&input), DiaryTickDecision::Skip);
    }

    /// 18 点后安静满 1h 提前写（quiet 路径）。
    #[test]
    fn quiet_path_writes_after_quiet() {
        let input = tick(at(19, 0), time(2026, 10, 19, 19, 0), at(17, 30));
        assert_eq!(
            diary_tick_decision(&input),
            DiaryTickDecision::Write(DiaryTickMode::Quiet)
        );
    }

    /// 18 点后但还没安静够不写。
    #[test]
    fn quiet_path_requires_full_quiet() {
        let input = tick(at(19, 0), time(2026, 10, 19, 19, 0), at(18, 30));
        assert_eq!(diary_tick_decision(&input), DiaryTickDecision::Skip);
    }

    /// 23 点定点写（不管安静——lastActivity 才 6 分钟前）。
    #[test]
    fn scheduled_path_at_hour() {
        let input = tick(at(23, 1), time(2026, 10, 19, 23, 1), at(22, 55));
        assert_eq!(
            diary_tick_decision(&input),
            DiaryTickDecision::Write(DiaryTickMode::Scheduled)
        );
    }

    /// 已写过不再触发。
    #[test]
    fn written_today_skips() {
        let mut input = tick(at(23, 5), time(2026, 10, 19, 23, 5), 0);
        input.written_today = true;
        assert_eq!(diary_tick_decision(&input), DiaryTickDecision::Skip);
    }

    /// 冷却期内不重试（25min < 60min）。
    #[test]
    fn cooldown_blocks_retry() {
        let input = DiaryTickInput {
            now_ms: at(23, 30),
            now_local: time(2026, 10, 19, 23, 30),
            last_tried_at: at(23, 5),
            ..Default::default()
        };
        assert_eq!(diary_tick_decision(&input), DiaryTickDecision::Skip);
    }

    /// 冷却期过后可重试（61min 后已过 0 点，由凌晨窗口兜住 → scheduled）。
    #[test]
    fn retry_after_cooldown() {
        let input = DiaryTickInput {
            now_ms: at(23, 5) + 61 * 60_000,
            now_local: time(2026, 10, 20, 0, 6),
            last_tried_at: at(23, 5),
            ..Default::default()
        };
        assert_ne!(diary_tick_decision(&input), DiaryTickDecision::Skip);
    }

    /// 补写判定：昨天有活动且缺日记 → 补写昨天（date 锁定为昨天键）。
    #[test]
    fn catchup_writes_yesterday() {
        let now = date(2026, 10, 20);
        assert_eq!(
            diary_catchup_decision(now, 12, &[]),
            DiaryCatchup::Yesterday {
                date: "2026-10-19".to_string()
            }
        );
    }

    /// 昨天已写过 → 不补。
    #[test]
    fn catchup_skips_when_yesterday_written() {
        let now = date(2026, 10, 20);
        let entries = [entry("2026-10-19", "x")];
        assert_eq!(
            diary_catchup_decision(now, 12, &entries),
            DiaryCatchup::None
        );
    }

    /// 昨天无活动 → 不补。
    #[test]
    fn catchup_requires_yesterday_activity() {
        assert_eq!(
            diary_catchup_decision(date(2026, 10, 20), 0, &[]),
            DiaryCatchup::None
        );
    }

    /// 今天已写（手动？）→ 不补。
    #[test]
    fn catchup_skips_when_today_written() {
        let now = date(2026, 10, 20);
        let entries = [entry("2026-10-20", "x")];
        assert_eq!(
            diary_catchup_decision(now, 12, &entries),
            DiaryCatchup::None
        );
    }

    fn entry(day: &str, content: &str) -> DiaryEntry {
        DiaryEntry {
            date: day.to_string(),
            content: content.to_string(),
            key: String::new(),
            created_at: 0,
        }
    }

    /// 首篇构建成功（带 key）。
    #[test]
    fn build_first_entry() {
        let e = build_diary_entry(&[], "2026-10-19", "今天群里聊了猫。", "group:1", 0).unwrap();
        assert_eq!(e.date, "2026-10-19");
        assert_eq!(e.key, "group:1");
    }

    /// 同日重复被拒，文案与 Node 版一致（回传给 agent）。
    #[test]
    fn build_rejects_same_day() {
        let entries = [entry("2026-10-19", "x")];
        let err =
            build_diary_entry(&entries, "2026-10-19", "再写一篇会被拒绝。", "", 0).unwrap_err();
        assert_eq!(
            err,
            DiaryAppendError::AlreadyWritten {
                day: "2026-10-19".to_string()
            }
        );
        assert!(err.to_string().contains("已经写过"));
    }

    /// 超长截断到 DIARY_MAX_CHARS。
    #[test]
    fn build_truncates_long_content() {
        let content: String = "x".repeat(DIARY_MAX_CHARS + 500);
        let e = build_diary_entry(&[], "2026-10-20", &content, "", 1000).unwrap();
        assert_eq!(e.content.chars().count(), DIARY_MAX_CHARS);
        assert_eq!(e.created_at, 1000);
    }

    /// 空内容（仅空白）拒绝。
    #[test]
    fn build_rejects_empty_content() {
        let err = build_diary_entry(&[], "2026-10-21", "   ", "", 0).unwrap_err();
        assert_eq!(err, DiaryAppendError::Empty);
        assert!(err.to_string().contains("为空"));
    }

    /// 坏行容忍：非 JSON 与缺字段行被跳过。
    #[test]
    fn parse_skips_bad_lines() {
        let text = "not json\n{\"date\":2026}\n";
        assert!(parse_diary(text).is_empty());
        let good = "{\"date\":\"2026-10-19\",\"content\":\"a\",\"key\":\"k\",\"createdAt\":123}";
        let entries = parse_diary(&format!("not json\n{good}\n{{\"date\":2026}}\n"));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].date, "2026-10-19");
        assert_eq!(entries[0].key, "k");
        assert_eq!(entries[0].created_at, 123);
    }

    /// recent 取最近 N 篇（正序），excerpt 截断带省略号。
    #[test]
    fn recent_takes_last_n_with_excerpt() {
        let entries = vec![
            entry("2026-10-19", "今天群里聊了猫。"),
            entry("2026-10-20", &"x".repeat(DIARY_MAX_CHARS)),
        ];
        let r = recent_diary(&entries, 2, 10);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].date, "2026-10-19");
        assert!(r[1].content.chars().count() <= 11);
        assert!(r[1].content.ends_with('…'));
    }

    /// excerptChars=0 不截断。
    #[test]
    fn recent_zero_excerpt_keeps_full() {
        let entries = vec![
            entry("2026-10-19", "a"),
            entry("2026-10-20", &"x".repeat(DIARY_MAX_CHARS)),
        ];
        let full = recent_diary(&entries, 2, 0);
        assert_eq!(full[1].content.chars().count(), DIARY_MAX_CHARS);
    }

    /// diary_written_today 按本地日期；凌晨 0~4 点归昨天。
    #[test]
    fn written_today_by_local_date() {
        let entries = [entry("2026-10-19", "x")];
        // 23:30 仍是 10-19 → 已写。
        assert!(diary_written_today(&entries, time(2026, 10, 19, 23, 30)));
        // 次日 08:00 → 10-20 未写。
        assert!(!diary_written_today(&entries, time(2026, 10, 22, 8, 0)));
        // 次日 02:00 仍归 10-19 → 已写（凌晨窗口语义，Node 版 currentDiaryDay 注释契约）。
        assert!(diary_written_today(&entries, time(2026, 10, 20, 2, 0)));
    }

    /// dateKey 格式：月日补零。
    #[test]
    fn date_key_pads() {
        assert_eq!(diary_date_key(date(2026, 10, 5)), "2026-10-05");
    }

    /// prev_day 的月/年回退（闰年二月含 29 日）。
    #[test]
    fn prev_day_rolls_back() {
        assert_eq!(prev_day(date(2026, 3, 1)), date(2026, 2, 28));
        assert_eq!(prev_day(date(2024, 3, 1)), date(2024, 2, 29));
        assert_eq!(prev_day(date(2026, 1, 1)), date(2025, 12, 31));
    }
}
