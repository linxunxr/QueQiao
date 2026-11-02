//! 长期记忆库（M7 第二批次，规格 §十二 P3——SQLite 触发信号"长期化"已
//! 到，本批落地）。episodic 层（social-v2.json 的 active_topics 等）**不动**，
//! 本库只承载新的全局长期层：AI 同一人格跨会话共享，条目由 AI 主导蒸馏
//! （周期复盘联动 + 工具主动存——自动蒸馏不做，防噪音），按需查询详情；
//! 唤醒 prompt 只注入计数索引行（token 红线：不因记忆扩容回退分层注入）。
//!
//! 库文件 state/memory.db（rusqlite bundled 单文件，WAL）；语义检索
//! （sqlite-vec + 本地 embedding）留二期，一期 LIKE 关键词。

use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// 条目种类（CHECK 约束同款集合）。
pub const KINDS: [&str; 3] = ["topic", "fact", "impression"];

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LongtermEntry {
    pub id: i64,
    pub kind: String,
    pub content: String,
    /// 来源会话（蒸馏时的宿主/来源 key，展示用）。
    pub source_key: String,
    pub created_at: i64,
    pub last_recalled_at: i64,
    pub recall_count: i64,
}

/// 库文件路径（QUEQIAO_HOME/state/memory.db）。
pub fn db_path() -> PathBuf {
    crate::paths::queqiao_home().join("state").join("memory.db")
}

/// 打开（或建库）——调用方持锁串行（与三 store 同哲学：一把 Mutex 包
/// Connection，低频工具面无并发压力）。PRAGMA journal_mode=WAL 提升读写
/// 并存友好；失败容忍返回 None（库不可用时长期记忆面降级为空，不阻塞
/// 其它链路）。
pub fn open(path: &Path) -> Option<Connection> {
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
    let conn = Connection::open(path).ok()?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS longterm_memory (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            kind TEXT NOT NULL CHECK (kind IN ('topic','fact','impression')),
            content TEXT NOT NULL,
            source_key TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL,
            last_recalled_at INTEGER NOT NULL DEFAULT 0,
            recall_count INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_longterm_kind ON longterm_memory(kind);",
    )
    .ok()?;
    Some(conn)
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<LongtermEntry> {
    Ok(LongtermEntry {
        id: row.get(0)?,
        kind: row.get(1)?,
        content: row.get(2)?,
        source_key: row.get(3)?,
        created_at: row.get(4)?,
        last_recalled_at: row.get(5)?,
        recall_count: row.get(6)?,
    })
}

const COLS: &str = "id, kind, content, source_key, created_at, last_recalled_at, recall_count";

/// 写入一条（kind 白名单与 content 非空由调用方校验；内容长度上限 500 字
/// 亦然）。返回新 id。重复内容（同 kind+content）幂等：命中即返回旧 id
/// 并刷新 source_key（AI 重复蒸馏不产生重复条目）。
pub fn save(
    conn: &Connection,
    kind: &str,
    content: &str,
    source_key: &str,
    now_ms: u64,
) -> Result<i64, String> {
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM longterm_memory WHERE kind = ?1 AND content = ?2",
            [kind, content],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| {
            if e == rusqlite::Error::QueryReturnedNoRows {
                Ok(None)
            } else {
                Err(e)
            }
        })
        .map_err(|e| e.to_string())?;
    if let Some(id) = existing {
        let _ = conn
            .execute(
                "UPDATE longterm_memory SET source_key = ?3 WHERE id = ?1",
                rusqlite::params![id, kind, source_key],
            )
            .map_err(|e| e.to_string())?;
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO longterm_memory (kind, content, source_key, created_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![kind, content, source_key, now_ms as i64],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

/// 查询：kind 过滤（None=全部）+ LIKE 关键词（None=不过滤），按
/// recall_count 降序 + created_at 降序（高频先出），limit 上限 100。
/// 查询命中即记账（last_recalled_at / recall_count +1——蒸馏重要性排序
/// 的数据积累）；返回值为记账前快照（累计在下次查询可见）。
pub fn query(
    conn: &Connection,
    kind: Option<&str>,
    keyword: Option<&str>,
    now_ms: u64,
) -> Result<Vec<LongtermEntry>, String> {
    let mut sql = format!("SELECT {COLS} FROM longterm_memory WHERE 1=1");
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(k) = kind {
        sql.push_str(&format!(" AND kind = ?{}", params.len() + 1));
        params.push(Box::new(k.to_owned()));
    }
    if let Some(kw) = keyword {
        // ESCAPE 必须显式声明（SQLite LIKE 默认无转义字符，\% 不生效）。
        sql.push_str(&format!(
            " AND content LIKE ?{} ESCAPE '\\'",
            params.len() + 1
        ));
        let escaped = kw
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        params.push(Box::new(format!("%{escaped}%")));
    }
    sql.push_str(" ORDER BY recall_count DESC, created_at DESC LIMIT 100");
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(refs.as_slice(), row_to_entry)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    // 记账（失败不影响查询结果）。
    for e in &rows {
        let _ = conn.execute(
            "UPDATE longterm_memory SET last_recalled_at = ?1, recall_count = recall_count + 1 WHERE id = ?2",
            rusqlite::params![now_ms as i64, e.id],
        );
    }
    Ok(rows)
}

/// 按 id 删除（返回是否删到）。
pub fn remove(conn: &Connection, id: i64) -> Result<bool, String> {
    conn.execute("DELETE FROM longterm_memory WHERE id = ?1", [id])
        .map(|n| n > 0)
        .map_err(|e| e.to_string())
}

/// 计数（kind 过滤可选）——唤醒索引行与复盘素材用。
pub fn count(conn: &Connection, kind: Option<&str>) -> (i64, i64, i64) {
    let one = |k: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM longterm_memory WHERE kind = ?1",
            [k],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };
    match kind {
        Some(k) => (one(k), 0, 0),
        None => (one("topic"), one("fact"), one("impression")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db(tag: &str) -> Connection {
        let p = std::env::temp_dir().join(format!("qq-ltm-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        open(&p).expect("建库")
    }

    /// CRUD 全链：幂等去重、kind/关键词过滤、记账、删除。
    #[test]
    fn crud_roundtrip() {
        let conn = tmp_db("crud");
        let now: u64 = 1_000;

        let id1 = save(&conn, "fact", "群友老刘喜欢钓鱼", "group:4242", now).unwrap();
        // 同 kind+content 幂等（返回旧 id，不产生重复）。
        let id1b = save(&conn, "fact", "群友老刘喜欢钓鱼", "private:1", now + 5).unwrap();
        assert_eq!(id1, id1b);
        save(&conn, "topic", "影之刃零通关话题", "group:4242", now).unwrap();
        save(&conn, "impression", "管理员爱开玩笑", "group:4242", now).unwrap();
        // 非法 kind 由调用方白名单拦（CHECK 也会拦）。
        assert!(save(&conn, "bogus", "x", "k", now).is_err());

        // 全量计数（topic/fact/impression 各 1）。
        assert_eq!(count(&conn, None), (1, 1, 1));
        assert_eq!(count(&conn, Some("fact")), (1, 0, 0));

        // 关键词过滤 + 记账。
        let hits = query(&conn, None, Some("钓鱼"), now + 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "群友老刘喜欢钓鱼");
        assert_eq!(
            query(&conn, Some("topic"), None, now + 10).unwrap().len(),
            1
        );
        // 返回记账前快照：again 读到 q1 记账后的 1，累计在下次查询可见。
        let again = query(&conn, None, Some("钓鱼"), now + 20).unwrap();
        assert_eq!(again[0].recall_count, 1, "返回值为本次记账前快照");
        let third = query(&conn, None, Some("钓鱼"), now + 30).unwrap();
        assert_eq!(third[0].recall_count, 2, "两次查询累计");
        assert_eq!(third[0].last_recalled_at as u64, now + 20, "同为记账前快照");
        // 高频优先排序：fact 查过两次，全量输出排最前。
        let all = query(&conn, None, None, now + 20).unwrap();
        assert_eq!(all[0].kind, "fact");

        // 删除。
        assert!(remove(&conn, id1).unwrap());
        assert!(!remove(&conn, id1).unwrap(), "二次删无");
        assert_eq!(count(&conn, Some("fact")), (0, 0, 0));
    }

    /// LIKE 转义：含 % 的关键词不误匹配。
    #[test]
    fn keyword_escape() {
        let conn = tmp_db("esc");
        save(&conn, "fact", "进度 100% 达成", "k", 1).unwrap();
        save(&conn, "fact", "别的", "k", 1).unwrap();
        assert_eq!(query(&conn, None, Some("100%"), 1).unwrap().len(), 1);
    }
}
