//! 长期记忆库（M7 第二批次，规格 §十二 P3——SQLite 触发信号"长期化"已
//! 到，本批落地）。episodic 层（social-v2.json 的 active_topics 等）**不动**，
//! 本库只承载新的全局长期层：AI 同一人格跨会话共享，条目由 AI 主导蒸馏
//! （周期复盘联动 + 工具主动存——自动蒸馏不做，防噪音），按需查询详情；
//! 唤醒 prompt 只注入计数索引行（token 红线：不因记忆扩容回退分层注入）。
//!
//! 库文件 state/memory.db（rusqlite bundled 单文件，WAL）。检索分两层：
//! 一期 LIKE 关键词（不依赖外部条件）；二期语义检索（M7 二期，2026-11-15）
//! 用 sqlite-vec 向量表 `longterm_vec` + 本地 bge-small-zh 模型——模型不可
//! 用时自动降级回关键词，主链路不受影响。

use std::path::{Path, PathBuf};
use std::sync::Once;

use rusqlite::Connection;

/// 条目种类（CHECK 约束同款集合）。
pub const KINDS: [&str; 3] = ["topic", "fact", "impression"];

/// 语义向量维度（与本地 embedding 模型一致）。
pub const VEC_DIM: usize = crate::embedding::DIM;

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

/// 进程级注册 sqlite-vec 静态扩展（必须在开库前调用；之后新开的连接自动
/// 带回 `vec0` 虚拟表与向量函数）。非核心链路——扩展注册失败不 panic。
fn register_vec_extension() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: sqlite-vec 的 C 初始化函数签名与 sqlite3_auto_extension
        // 期望一致（与 crate 自带测试同款写法）；进程内只注册一次。
        unsafe {
            let init: Option<
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut std::os::raw::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> std::os::raw::c_int,
            > = std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
            rusqlite::ffi::sqlite3_auto_extension(init);
        }
    });
}

/// sqlite-vec 是否可用（扩展注册成功且当前连接带上了 vec0）。
pub fn vec_available(conn: &Connection) -> bool {
    conn.query_row("SELECT vec_version()", [], |r| r.get::<_, String>(0))
        .is_ok()
}

/// 建/校验向量表（维度不匹配时重建，旧向量由启动回填重算）。返回是否可用。
fn ensure_vec_schema(conn: &Connection) -> bool {
    if !vec_available(conn) {
        return false;
    }
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS longterm_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    );
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM longterm_meta WHERE key = 'vec_dim'",
            [],
            |r| r.get(0),
        )
        .ok();
    let table_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'longterm_vec'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let dim_ok = stored.as_deref() == Some(VEC_DIM.to_string().as_str());
    if !dim_ok || table_exists == 0 {
        let _ = conn.execute("DROP TABLE IF EXISTS longterm_vec", []);
        let sql = format!(
            "CREATE VIRTUAL TABLE longterm_vec USING vec0(
                embedding float[{VEC_DIM}] distance_metric=cosine
            );"
        );
        if conn.execute_batch(&sql).is_err() {
            return false;
        }
        let _ = conn.execute(
            "INSERT OR REPLACE INTO longterm_meta (key, value) VALUES ('vec_dim', ?1)",
            [VEC_DIM.to_string()],
        );
    }
    vec_available(conn)
}

/// 打开（或建库）——调用方持锁串行（与三 store 同哲学：一把 Mutex 包
/// Connection，低频工具面无并发压力）。PRAGMA journal_mode=WAL 提升读写
/// 并存友好；失败容忍返回 None（库不可用时长期记忆面降级为空，不阻塞
/// 其它链路）。语义向量表尽力而为，建不出来不影响关键词链路。
pub fn open(path: &Path) -> Option<Connection> {
    register_vec_extension();
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
    let _ = ensure_vec_schema(&conn);
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
    save_with_embedding(conn, kind, content, source_key, now_ms, None)
}

/// 同 [`save`]，可选同时写入语义向量（保存时项向量在调用方算好传入；
/// None 时由启动/后续回填补齐）。
pub fn save_with_embedding(
    conn: &Connection,
    kind: &str,
    content: &str,
    source_key: &str,
    now_ms: u64,
    embedding: Option<&[f32]>,
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
        if let Some(emb) = embedding {
            let _ = set_embedding(conn, id, emb);
        }
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO longterm_memory (kind, content, source_key, created_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![kind, content, source_key, now_ms as i64],
    )
    .map_err(|e| e.to_string())?;
    let id = conn.last_insert_rowid();
    if let Some(emb) = embedding {
        let _ = set_embedding(conn, id, emb);
    }
    Ok(id)
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

/// 按 id 删除（返回是否删到；连带清理向量行）。
pub fn remove(conn: &Connection, id: i64) -> Result<bool, String> {
    let _ = conn.execute("DELETE FROM longterm_vec WHERE rowid = ?1", [id]);
    conn.execute("DELETE FROM longterm_memory WHERE id = ?1", [id])
        .map(|n| n > 0)
        .map_err(|e| e.to_string())
}

// ── 语义检索（M7 二期）─────────────────────────────────────────────────────

fn embedding_json(embedding: &[f32]) -> Option<String> {
    serde_json::to_string(embedding).ok()
}

/// 写入/覆盖一行向量（id 必须对应 longterm_memory.id；失败静默返回 false）。
pub fn set_embedding(conn: &Connection, id: i64, embedding: &[f32]) -> bool {
    if embedding.len() != VEC_DIM || !ensure_vec_schema(conn) {
        return false;
    }
    let Some(json) = embedding_json(embedding) else {
        return false;
    };
    let _ = conn.execute("DELETE FROM longterm_vec WHERE rowid = ?1", [id]);
    conn.execute(
        "INSERT INTO longterm_vec (rowid, embedding) VALUES (?1, ?2)",
        rusqlite::params![id, json],
    )
    .is_ok()
}

fn get_by_id(conn: &Connection, id: i64) -> Option<LongtermEntry> {
    conn.query_row(
        &format!("SELECT {COLS} FROM longterm_memory WHERE id = ?1"),
        [id],
        row_to_entry,
    )
    .ok()
}

/// KNN：返回 (id, 余弦距离) 升序（0=相同方向）。
pub fn semantic_search(conn: &Connection, embedding: &[f32], k: usize) -> Option<Vec<(i64, f32)>> {
    if embedding.len() != VEC_DIM || !ensure_vec_schema(conn) {
        return None;
    }
    let json = embedding_json(embedding)?;
    let mut stmt = conn
        .prepare("SELECT rowid, distance FROM longterm_vec WHERE embedding MATCH ?1 AND k = ?2")
        .ok()?;
    let rows: Vec<(i64, f32)> = stmt
        .query_map(rusqlite::params![json, k as i64], |r| {
            Ok((r.get(0)?, r.get::<_, f64>(1)? as f32))
        })
        .ok()?
        .filter_map(Result::ok)
        .collect();
    Some(rows)
}

/// 语义查询（供 qq_memory_query 用）：查询文本 → 本地 embedding → KNN →
/// 按 kind 过滤 → 取详情并记账。模型未就绪/向量表不可用时返回 None
///（调用方降级关键词检索）。
pub fn semantic_query(
    conn: &Connection,
    query_text: &str,
    kind: Option<&str>,
    k: usize,
    now_ms: u64,
) -> Option<Vec<LongtermEntry>> {
    if !crate::embedding::is_ready() {
        return None;
    }
    let embedding = crate::embedding::embed_query(query_text).ok()?;
    // 多取一些候补给 kind 过滤留余量。
    let hits = semantic_search(conn, &embedding, (k * 4).max(k))?;
    let mut out = Vec::new();
    for (id, _distance) in hits {
        let Some(entry) = get_by_id(conn, id) else {
            continue;
        };
        if kind.is_some_and(|k| entry.kind != k) {
            continue;
        }
        out.push(entry);
        if out.len() >= k {
            break;
        }
    }
    for e in &out {
        let _ = conn.execute(
            "UPDATE longterm_memory SET last_recalled_at = ?1, recall_count = recall_count + 1 WHERE id = ?2",
            rusqlite::params![now_ms as i64, e.id],
        );
    }
    Some(out)
}

/// 缺失向量的条目 id（启动回填用）。
pub fn missing_embedding_ids(conn: &Connection) -> Vec<i64> {
    if !ensure_vec_schema(conn) {
        return Vec::new();
    }
    let Ok(mut stmt) = conn.prepare(
        "SELECT id FROM longterm_memory
         WHERE id NOT IN (SELECT rowid FROM longterm_vec) ORDER BY id",
    ) else {
        return Vec::new();
    };
    stmt.query_map([], |r| r.get::<_, i64>(0))
        .map(|iter| iter.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// 启动回填：为缺失/过期向量的条目补 embedding（模型就绪后才调）。
/// 返回回填条数；模型或库不可用时返回 0（不阻塞）。
pub fn backfill_embeddings(conn: &Connection) -> usize {
    if !crate::embedding::is_ready() {
        return 0;
    }
    let ids = missing_embedding_ids(conn);
    let mut done = 0;
    for chunk in ids.chunks(8) {
        let mut texts = Vec::with_capacity(chunk.len());
        let mut found = Vec::with_capacity(chunk.len());
        for &id in chunk {
            if let Some(e) = get_by_id(conn, id) {
                found.push(id);
                texts.push(e.content);
            }
        }
        if texts.is_empty() {
            continue;
        }
        match crate::embedding::embed_passages(&texts) {
            Ok(vectors) => {
                for (id, emb) in found.iter().zip(vectors) {
                    if set_embedding(conn, *id, &emb) {
                        done += 1;
                    }
                }
            }
            Err(e) => {
                crate::logging::log(&format!("[semantic] 向量回填中断：{e}"));
                break;
            }
        }
    }
    done
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

    /// 语义向量链路：表可用、向量写入/删除、KNN 序、缺失回填名单。
    #[test]
    fn vec_semantic_chain() {
        let conn = tmp_db("vec");
        assert!(vec_available(&conn), "sqlite-vec 静态扩展应已注册");
        let id1 = save(&conn, "fact", "管理员不要电竞比赛内容", "k", 1).unwrap();
        let id2 = save(&conn, "fact", "今天中午吃豆腐脑", "k", 2).unwrap();
        let mut a = vec![0f32; VEC_DIM];
        a[0] = 1.0;
        let mut b = vec![0f32; VEC_DIM];
        b[1] = 1.0;
        assert!(set_embedding(&conn, id1, &a));
        assert!(set_embedding(&conn, id2, &b));
        assert!(missing_embedding_ids(&conn).is_empty(), "两条都已有向量");

        let hits = semantic_search(&conn, &a, 2).expect("KNN");
        assert_eq!(hits[0].0, id1, "同向向量排第一");
        assert!(hits[0].1 < hits[1].1, "距离升序");
        // 模型未就绪时语义查询走 None（调用方降级关键词），不 panic。
        assert!(semantic_query(&conn, "比赛", None, 5, 10).is_none());
        assert_eq!(backfill_embeddings(&conn), 0);
        // 维度不符拒写；删除连带清向量行。
        assert!(!set_embedding(&conn, id1, &[1.0, 0.0]));
        assert!(remove(&conn, id1).unwrap());
        let after = semantic_search(&conn, &a, 5).expect("KNN");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].0, id2);
        // 新存但未带向量的条目进入回填名单。
        let id3 = save(&conn, "topic", "还没算向量的条目", "k", 3).unwrap();
        assert_eq!(missing_embedding_ids(&conn), vec![id3]);
    }
}
