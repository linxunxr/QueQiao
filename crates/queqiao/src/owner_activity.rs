//! 管理员跨会话动态（长期记忆扩展；2026-11-15 真机报障的根治面）。
//!
//! 问题：记忆按 `conversationKey` 分域（episodic 短期层在 social-v2.json、
//! 唤醒注入只读当前会话），管理员在群里跟 AI 聊过之后切到私聊，私聊会话
//! 看不到群里那段互动——主动唤醒会按私聊自己的时间线判断成「一小时没回，
//! 看来懒觉是真睡上了」，等于把「在别的会话聊过」误判成「没理你」。
//!
//! 本模块在既有长期记忆库 `state/memory.db` 里加一张**独立表**
//! `owner_activity`（不动 `longterm_memory` 的蒸馏语义）：管理员在任何会话
//! 里说的话各记一行；管理员近期出现过的会话里 AI 自己的发言也记一行（作为
//! 「已经回应过」的证据）。库本身跨会话、跨重启，90 天 / 600 条滚动淘汰。
//!
//! 召回按 token 红线做「索引式注入」：只有 **owner 私聊会话**
//!（`private:<ownerQQ>`）的唤醒 prompt 注入【管理员跨会话动态】块——最近
//! 7 天、最多 8 条、总长 ≤1200 字——提示模型别再把「这个会话没回」当成
//! 「没理你」。群会话不注入（私聊内容可能敏感，不能带进群里）；当前会话
//! 自己的事件不注入（本会话记忆/近消息已经覆盖，避免重复）。
//!
//! 失败哲学与 longterm_store 一致：库不可用 → 记录/查询静默降级为空，
//! 不阻塞唤醒链路。

use rusqlite::{Connection, params};

use crate::social_store::SocialStore;

/// 角色：管理员本人 / AI 自己。
pub const ROLE_OWNER: &str = "owner";
pub const ROLE_SELF: &str = "self";

/// 保留窗口与条数上限（滚动淘汰；私聊注入只看最近 7 天）。
const MAX_EVENTS: i64 = 600;
const MAX_AGE_MS: u64 = 90 * 24 * 60 * 60 * 1000;

/// AI 发言归属判定：群会话里，管理员 6 小时内说过话才记 AI 的发言
///（否则群里日常闲聊会把 600 条窗口挤满，真正跟管理员相关的互动反而被淘汰）。
const SELF_REPLY_WINDOW_MS: u64 = 6 * 60 * 60 * 1000;

/// 注入窗口/条数/逐条截断/整体上限（token 经济——与记忆分层同哲学）。
const BLOCK_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const BLOCK_MAX_EVENTS: usize = 8;
/// 注入选取的管理员锚点条数（每条锚点可带最多 2 条 AI 回复；总数仍受
/// BLOCK_MAX_EVENTS 约束）。
const BLOCK_ANCHOR_MAX: usize = 4;
const BLOCK_TEXT_CHARS: usize = 80;
const BLOCK_MAX_CHARS: usize = 1200;

/// 单条事件落库文本上限（防异常巨消息撑库；群聊正常文本远小于此）。
const EVENT_TEXT_CHARS: usize = 200;

/// 一条管理员活动记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    pub conversation_key: String,
    pub role: String,
    pub text: String,
    pub created_at: u64,
}

/// 建表（幂等；每次读写前调用——连接可能由 longterm_store::open 建出，
/// 这里不依赖调用方初始化顺序）。
fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS owner_activity (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            conversation_key TEXT NOT NULL,
            role TEXT NOT NULL,
            text TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_owner_activity_conv
            ON owner_activity(conversation_key, created_at);
        CREATE INDEX IF NOT EXISTS idx_owner_activity_created
            ON owner_activity(created_at);",
    )
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 写入一行并滚动淘汰（窗口过期 + 超量）。返回是否实际写入。
fn insert(conn: &Connection, key: &str, role: &str, text: &str, at_ms: u64) -> bool {
    let text = text.trim();
    if key.is_empty() || text.is_empty() {
        return false;
    }
    if ensure_schema(conn).is_err() {
        return false;
    }
    let text = truncate_chars(text, EVENT_TEXT_CHARS);
    if conn
        .execute(
            "INSERT INTO owner_activity (conversation_key, role, text, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![key, role, text, at_ms as i64],
        )
        .is_err()
    {
        return false;
    }
    prune(conn, at_ms);
    true
}

/// 滚动淘汰：超 90 天或超 600 条（按插入序保留最新）。
fn prune(conn: &Connection, now_ms: u64) {
    let floor = now_ms.saturating_sub(MAX_AGE_MS) as i64;
    let _ = conn.execute("DELETE FROM owner_activity WHERE created_at < ?1", [floor]);
    let _ = conn.execute(
        "DELETE FROM owner_activity WHERE id NOT IN (
            SELECT id FROM owner_activity ORDER BY id DESC LIMIT ?1
        )",
        [MAX_EVENTS],
    );
}

/// 记录一条管理员消息（调用方保证 `user_id` 是消息发送者；非管理员返回
/// false 且不落库）。
pub fn record_owner_message(
    conn: &Connection,
    conversation_key: &str,
    user_id: &str,
    owner_qq: Option<u64>,
    text: &str,
    at_ms: u64,
) -> bool {
    let Some(owner) = owner_qq else {
        return false;
    };
    if user_id.trim() != owner.to_string() {
        return false;
    }
    insert(conn, conversation_key, ROLE_OWNER, text, at_ms)
}

/// 记录一条 AI 自己的发言（「已经回应过」的证据）。
///
/// - owner 私聊会话：全记（1:1 会话里每句都是对管理员说的）；
/// - 其它会话：该会话 6 小时内出现过管理员消息才记（避免群闲聊挤占窗口）。
pub fn record_self_message(
    conn: &Connection,
    conversation_key: &str,
    owner_qq: Option<u64>,
    text: &str,
    at_ms: u64,
) -> bool {
    let Some(owner) = owner_qq else {
        return false;
    };
    let owner_private = format!("private:{owner}");
    let relates_to_owner = conversation_key == owner_private
        || has_recent_owner(
            conn,
            conversation_key,
            at_ms.saturating_sub(SELF_REPLY_WINDOW_MS),
        );
    if !relates_to_owner {
        return false;
    }
    insert(conn, conversation_key, ROLE_SELF, text, at_ms)
}

/// 该会话在 `since_ms` 之后有没有管理员消息。
pub fn has_recent_owner(conn: &Connection, conversation_key: &str, since_ms: u64) -> bool {
    if ensure_schema(conn).is_err() {
        return false;
    }
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM owner_activity
            WHERE conversation_key = ?1 AND role = ?2 AND created_at >= ?3
        )",
        params![conversation_key, ROLE_OWNER, since_ms as i64],
        |row| row.get::<_, i64>(0),
    )
    .map(|v| v != 0)
    .unwrap_or(false)
}

/// 启动回填：`owner_activity` 为空时，从 social-v2 各会话近 7 天 recent
/// 消息重建管理员互动（管理员消息 + 其在场的会话里 AI 的发言）。返回回填
/// 条数；表已有数据返回 0（只在首次部署/清库后跑一次，避免重启重复灌）。
///
/// 归属规则与实时记录对齐：owner 私聊里的 AI 发言全记；群会话的 AI 发言只
/// 在「最近一条管理员消息 6 小时内」记（顺序扫描天然按时间先后判断）。
pub fn backfill_from_recent_messages(
    conn: &Connection,
    social: &SocialStore,
    owner_qq: Option<u64>,
    now_ms: u64,
) -> usize {
    let Some(owner) = owner_qq else {
        return 0;
    };
    if ensure_schema(conn).is_err() {
        return 0;
    }
    let existing: i64 = conn
        .query_row("SELECT COUNT(*) FROM owner_activity", [], |row| row.get(0))
        .unwrap_or(0);
    if existing > 0 {
        return 0;
    }
    let owner_id = owner.to_string();
    let owner_private = format!("private:{owner}");
    let floor = now_ms.saturating_sub(BLOCK_WINDOW_MS);
    let mut events: Vec<(u64, String, &'static str, String)> = Vec::new();
    for key in social.conversation_keys() {
        let Some(st) = social.conversation(&key) else {
            continue;
        };
        let mut last_owner_at: Option<u64> = None;
        for m in st.recent_messages.iter().filter(|m| m.time >= floor) {
            if m.is_owner && m.user_id.as_deref() == Some(owner_id.as_str()) {
                last_owner_at = Some(m.time);
                events.push((m.time, key.clone(), ROLE_OWNER, m.text.clone()));
            } else if m.is_self {
                let owner_ctx = key == owner_private
                    || last_owner_at
                        .is_some_and(|t| m.time.saturating_sub(t) <= SELF_REPLY_WINDOW_MS);
                if owner_ctx {
                    events.push((m.time, key.clone(), ROLE_SELF, m.text.clone()));
                }
            }
        }
    }
    events.sort_by_key(|(t, _, _, _)| *t);
    let mut written = 0;
    for (at_ms, key, role, text) in events {
        if insert(conn, &key, role, &text, at_ms) {
            written += 1;
        }
    }
    written
}

/// 取最近活动（排除当前会话；按时间升序返回，便于 prompt 阅读）。
fn recent(conn: &Connection, exclude_key: &str, since_ms: u64, limit: usize) -> Vec<Entry> {
    if ensure_schema(conn).is_err() {
        return Vec::new();
    }
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, conversation_key, role, text, created_at
         FROM owner_activity
         WHERE conversation_key <> ?1 AND created_at >= ?2
         ORDER BY id DESC LIMIT ?3",
    ) else {
        return Vec::new();
    };
    let rows = stmt
        .query_map(params![exclude_key, since_ms as i64, limit as i64], |row| {
            Ok(Entry {
                id: row.get(0)?,
                conversation_key: row.get(1)?,
                role: row.get(2)?,
                text: row.get(3)?,
                created_at: row.get::<_, i64>(4)?.max(0) as u64,
            })
        })
        .map(|iter| iter.filter_map(Result::ok).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut rows = rows;
    rows.reverse();
    rows
}

/// 会话标签（群号 / 私聊；当前会话已被排除，私聊一般只出现在非 owner 场景）。
fn conversation_label(key: &str) -> String {
    if let Some(group) = key.strip_prefix("group:") {
        format!("群 {group}")
    } else if key.starts_with("private:") {
        "私聊".to_owned()
    } else {
        key.to_owned()
    }
}

fn role_label(role: &str) -> &'static str {
    match role {
        ROLE_OWNER => "管理员",
        ROLE_SELF => "你",
        _ => "?",
    }
}

/// 相对时间（最近几天足够；不引时区库）。
fn ago_text(now_ms: u64, at_ms: u64) -> String {
    let mins = now_ms.saturating_sub(at_ms) / 60_000;
    if mins < 1 {
        "刚刚".to_owned()
    } else if mins < 60 {
        format!("{mins} 分钟前")
    } else if mins < 24 * 60 {
        format!("{} 小时前", mins / 60)
    } else {
        format!("{} 天前", mins / (24 * 60))
    }
}

/// 注入选取：以管理员发言为锚点（最近 [`BLOCK_ANCHOR_MAX`] 条），每条带
/// 其后 6h 内 AI 回复（最多 2 条），按「整组」从新到旧装进 [`BLOCK_MAX_EVENTS`]
/// 预算——避免「AI 连刷多条」把管理员原话挤出窗口，块里只剩一堆「你：…」
/// 看不出在跟谁互动。
fn select_for_block(events: &[Entry]) -> Vec<Entry> {
    let mut anchors: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.role == ROLE_OWNER)
        .map(|(i, _)| i)
        .collect();
    if anchors.is_empty() {
        let start = events.len().saturating_sub(BLOCK_MAX_EVENTS);
        return events[start..].to_vec();
    }
    anchors = anchors[anchors.len().saturating_sub(BLOCK_ANCHOR_MAX)..].to_vec();
    let mut picked: Vec<usize> = Vec::new();
    // 从最新锚点往回装组：最后一组的空间不够就停（更老的更装不下）。
    for &ai in anchors.iter().rev() {
        let anchor = &events[ai];
        let mut group = vec![ai];
        for (i, e) in events.iter().enumerate().skip(ai + 1) {
            if e.conversation_key != anchor.conversation_key || e.role != ROLE_SELF {
                continue;
            }
            let gap = e.created_at.saturating_sub(anchor.created_at);
            if gap > SELF_REPLY_WINDOW_MS {
                break;
            }
            group.push(i);
            if group.len() >= 3 {
                break;
            }
        }
        if picked.len() + group.len() > BLOCK_MAX_EVENTS {
            break;
        }
        picked.extend(group);
    }
    if picked.is_empty() {
        let start = events.len().saturating_sub(BLOCK_MAX_EVENTS);
        return events[start..].to_vec();
    }
    picked.sort_unstable();
    picked.dedup();
    picked.into_iter().map(|i| events[i].clone()).collect()
}

/// 拼【管理员跨会话动态】块（纯函数；空事件返回空串）。
pub fn format_block(events: &[Entry], now_ms: u64) -> String {
    if events.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "【管理员跨会话动态】管理员最近也在别的会话里出现过/跟你说过话——私聊里别把「这个会话没回你」误当成「没理你」，也别据此抱怨他失联：\n",
    );
    let mut len = out.chars().count();
    for e in events {
        let line = format!(
            "· {} · {} · {}：{}\n",
            ago_text(now_ms, e.created_at),
            conversation_label(&e.conversation_key),
            role_label(&e.role),
            truncate_chars(&e.text, BLOCK_TEXT_CHARS)
        );
        let line_len = line.chars().count();
        if len + line_len > BLOCK_MAX_CHARS {
            break;
        }
        out.push_str(&line);
        len += line_len;
    }
    out.push_str("（以上是桥汇总的跨会话摘要；只在私聊使用，不要带到群里）\n\n");
    out
}

/// 唤醒注入入口：只有 owner 私聊会话才返回内容（群会话返回空串——私聊
/// 内容绝不带进群）；库不可用静默降级为空。
pub fn build_cross_session_block(
    conn: &Connection,
    current_key: &str,
    owner_qq: Option<u64>,
    now_ms: u64,
) -> String {
    let Some(owner) = owner_qq else {
        return String::new();
    };
    if current_key != format!("private:{owner}") {
        return String::new();
    }
    let events = recent(
        conn,
        current_key,
        now_ms.saturating_sub(BLOCK_WINDOW_MS),
        MAX_EVENTS as usize,
    );
    format_block(&select_for_block(&events), now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: u64 = 3388404701;
    const OWNER_KEY: &str = "private:3388404701";
    const GROUP: &str = "group:771440459";

    fn conn() -> Connection {
        let c = Connection::open_in_memory().expect("内存库");
        ensure_schema(&c).expect("建表");
        c
    }

    fn count(c: &Connection) -> i64 {
        c.query_row("SELECT COUNT(*) FROM owner_activity", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn record_owner_only_accepts_owner_id() {
        let c = conn();
        assert!(!record_owner_message(
            &c,
            GROUP,
            "1208248078",
            Some(OWNER),
            "路人消息",
            1_000
        ));
        assert!(record_owner_message(
            &c,
            GROUP,
            "3388404701",
            Some(OWNER),
            "管理员消息",
            2_000
        ));
        assert!(!record_owner_message(
            &c,
            GROUP,
            "3388404701",
            None,
            "无 owner",
            3_000
        ));
        assert_eq!(count(&c), 1);
    }

    #[test]
    fn recent_excludes_current_and_old() {
        let c = conn();
        let now = 40 * 24 * 3_600_000u64; // 远离 0 点，避免测试里的减法下溢
        record_owner_message(
            &c,
            GROUP,
            "3388404701",
            Some(OWNER),
            "群里的",
            now - 3_600_000,
        );
        record_owner_message(
            &c,
            OWNER_KEY,
            "3388404701",
            Some(OWNER),
            "私聊里的",
            now - 1_800_000,
        );
        record_owner_message(
            &c,
            "group:2",
            "3388404701",
            Some(OWNER),
            "很久以前",
            now - 30 * 24 * 3_600_000,
        );
        let got = recent(&c, OWNER_KEY, now - 7 * 24 * 3_600_000, 10);
        assert_eq!(got.len(), 1, "排除当前会话与过期事件");
        assert_eq!(got[0].text, "群里的");
        assert_eq!(got[0].role, ROLE_OWNER);
    }

    #[test]
    fn self_message_requires_owner_context() {
        let c = conn();
        let now = 1_000_000u64;
        // 群里还没有管理员消息：AI 日常发言不记。
        assert!(!record_self_message(
            &c,
            GROUP,
            Some(OWNER),
            "普通闲聊",
            now
        ));
        assert_eq!(count(&c), 0);
        // 管理员刚在群里说过话：随后 AI 的回复记一笔（窗口内）。
        record_owner_message(
            &c,
            GROUP,
            "3388404701",
            Some(OWNER),
            "管理员问",
            now - 1_000,
        );
        assert!(record_self_message(&c, GROUP, Some(OWNER), "AI 回复", now));
        // 超出 6 小时窗口：不再记。
        assert!(!record_self_message(
            &c,
            GROUP,
            Some(OWNER),
            "隔天闲聊",
            now + SELF_REPLY_WINDOW_MS + 1
        ));
        // owner 私聊会话：总是记（1:1 每句都对管理员说）。
        assert!(record_self_message(
            &c,
            OWNER_KEY,
            Some(OWNER),
            "私聊主动消息",
            now
        ));
        assert_eq!(count(&c), 3);
    }

    #[test]
    fn block_only_for_owner_private_session() {
        let c = conn();
        let now = 1_000_000u64;
        record_owner_message(
            &c,
            GROUP,
            "3388404701",
            Some(OWNER),
            "群里聊过",
            now - 60_000,
        );
        record_self_message(&c, GROUP, Some(OWNER), "我回了", now - 30_000);

        assert!(
            build_cross_session_block(&c, GROUP, Some(OWNER), now).is_empty(),
            "群会话不注入私聊动态"
        );
        assert!(
            build_cross_session_block(&c, "private:999", Some(OWNER), now).is_empty(),
            "非 owner 私聊不注入"
        );

        let block = build_cross_session_block(&c, OWNER_KEY, Some(OWNER), now);
        assert!(block.starts_with("【管理员跨会话动态】"), "块开头：{block}");
        assert!(block.contains("群 771440459"));
        assert!(block.contains("管理员：群里聊过"));
        assert!(block.contains("你：我回了"));
        assert!(block.contains("没理你"), "带行为纠偏指引");
    }

    #[test]
    fn block_respects_event_and_char_limits() {
        let c = conn();
        let now = 10_000_000u64;
        for i in 0..20 {
            record_owner_message(
                &c,
                GROUP,
                "3388404701",
                Some(OWNER),
                &format!("第 {i} 条{}", "很长的内容".repeat(30)),
                now - (20 - i) * 60_000,
            );
        }
        let block = build_cross_session_block(&c, OWNER_KEY, Some(OWNER), now);
        assert!(block.chars().count() <= BLOCK_MAX_CHARS + 200, "总长受控");
        let lines = block.lines().filter(|l| l.starts_with('·')).count();
        assert!(lines <= BLOCK_MAX_EVENTS, "条数受控：{lines}");
        assert_eq!(count(&c), 20);
    }

    #[test]
    fn prune_keeps_latest_events() {
        let c = conn();
        for i in 0..(MAX_EVENTS + 5) {
            insert(&c, GROUP, ROLE_OWNER, &format!("m{i}"), 10_000 + i as u64);
        }
        assert_eq!(count(&c), MAX_EVENTS);
        let newest: String = c
            .query_row(
                "SELECT text FROM owner_activity ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(newest, format!("m{}", MAX_EVENTS + 4));
    }

    #[test]
    fn backfill_from_recent_messages_only_once() {
        use queqiao_core::action::IncomingMessage;

        fn msg(key: &str, text: &str, user_id: &str, is_self: bool) -> IncomingMessage {
            IncomingMessage {
                key: key.to_owned(),
                seq: 0,
                message_id: String::new(),
                sender: if is_self { "我" } else { "测试" }.to_owned(),
                user_id: user_id.to_owned(),
                text: text.to_owned(),
                is_self,
                at_me: false,
                reply_to_me: false,
                time_ms: 0,
            }
        }

        let path = std::env::temp_dir().join(format!(
            "qq-owner-backfill-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let mut social = SocialStore::load(&path, Some(OWNER)).expect("空 social-v2 加载必然成功");
        let now = 100_000_000u64;
        // 群：管理员发言 → 6h 内 AI 回复（都回填）；路人消息不回填。
        social.append_message(
            GROUP,
            &msg(GROUP, "群里聊过", "3388404701", false),
            now - 3 * 3_600_000,
        );
        social.append_message(
            GROUP,
            &msg(GROUP, "群回复", "", true),
            now - 3 * 3_600_000 + 60_000,
        );
        social.append_message(
            GROUP,
            &msg(GROUP, "路人消息", "1208248078", false),
            now - 2 * 3_600_000,
        );
        // 另一个群（非白名单群也能被事件流看到）：只回填管理员消息。
        social.append_message(
            "group:2",
            &msg("group:2", "另一个群", "3388404701", false),
            now - 3_600_000,
        );
        // owner 私聊：管理员与 AI 各一条。
        social.append_message(
            OWNER_KEY,
            &msg(OWNER_KEY, "私聊专属消息", "3388404701", false),
            now - 1_800_000,
        );
        social.append_message(
            OWNER_KEY,
            &msg(OWNER_KEY, "私聊回复", "", true),
            now - 1_700_000,
        );

        let c = conn();
        let n = backfill_from_recent_messages(&c, &social, Some(OWNER), now);
        assert_eq!(n, 5, "群 2 条 + 另一群 1 条 + owner 私聊 2 条");
        assert_eq!(count(&c), 5);
        let block = build_cross_session_block(&c, OWNER_KEY, Some(OWNER), now);
        assert!(block.contains("群 771440459"));
        assert!(block.contains("群 2"));
        assert!(!block.contains("私聊专属消息"), "当前会话事件不注入");
        assert_eq!(
            backfill_from_recent_messages(&c, &social, Some(OWNER), now),
            0,
            "表非空不再回填（重启幂等）"
        );
    }

    #[test]
    fn block_keeps_owner_anchor_when_self_messages_flood() {
        let c = conn();
        let now = 40_000_000u64;
        // 真实场景：管理员发言 → AI 连发多条 → 下一轮管理员发言 → AI 又连发，
        // 只按时间戳取最近 8 条会把管理员原话挤掉，块里只剩「你：…」。
        record_owner_message(
            &c,
            GROUP,
            "3388404701",
            Some(OWNER),
            "管理员原始问题",
            now - 4 * 3_600_000,
        );
        record_self_message(
            &c,
            GROUP,
            Some(OWNER),
            "AI 回复1",
            now - 4 * 3_600_000 + 60_000,
        );
        record_self_message(
            &c,
            GROUP,
            Some(OWNER),
            "AI 回复2",
            now - 4 * 3_600_000 + 120_000,
        );
        record_owner_message(
            &c,
            GROUP,
            "3388404701",
            Some(OWNER),
            "管理员追问",
            now - 2 * 3_600_000,
        );
        record_self_message(
            &c,
            GROUP,
            Some(OWNER),
            "AI 回复3",
            now - 2 * 3_600_000 + 60_000,
        );
        for i in 0..10 {
            record_self_message(
                &c,
                GROUP,
                Some(OWNER),
                &format!("后续闲聊 {i}"),
                now - 100 * 60_000 + i * 60_000,
            );
        }
        let block = build_cross_session_block(&c, OWNER_KEY, Some(OWNER), now);
        assert!(block.contains("管理员原始问题"), "旧锚点仍在：{block}");
        assert!(block.contains("AI 回复1"), "锚点回复仍在：{block}");
        assert!(block.contains("管理员追问"), "最新锚点在：{block}");
        assert!(block.contains("AI 回复3"), "最新回复在：{block}");
        let lines = block.lines().filter(|l| l.starts_with('·')).count();
        assert!(lines <= BLOCK_MAX_EVENTS, "条数受控：{lines}");
        assert!(!block.contains("后续闲聊 9"), "更晚的连刷被挤出预算");
    }

    #[test]
    fn empty_block_when_no_events() {
        let c = conn();
        assert!(build_cross_session_block(&c, OWNER_KEY, Some(OWNER), 1_000).is_empty());
        assert!(format_block(&[], 1_000).is_empty());
    }
}
