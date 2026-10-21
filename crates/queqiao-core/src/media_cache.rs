//! 图片抓取短期缓存（LRU + TTL）（移植自 Node 版 `src/media-cache.js`，契约测试同步迁移）。
//!
//! 同一条消息被反复取图（模型多次调用 qq_get_message_images、收藏表情复看）时
//! 避免重复下载；更重要的是 QQ 多媒体 URL 的 rkey 有时效，过一会儿再取就永远
//! 取不到——缓存把「刚看过还能再看」的窗口拉长到 TTL 量级。只缓存已经过
//! finalize（大小/像素预算）的结果。
//!
//! 分层调整：Node 版以 `now` 闭包注入时钟，Rust 版改为每次调用显式传 `now_ms`
//!（core 零时钟依赖的统一注入形态）。

/// LRU + TTL 缓存。条目序即 LRU 序：头部最久未用，尾部最近使用。
///
/// 泛型 `V` 为已 finalize 的媒体结果；容量小（默认 60），线性查找即可。
pub struct MediaCache<V> {
    /// (key, value, 写入时刻 now_ms)
    entries: Vec<(String, V, u64)>,
    max_entries: usize,
    ttl_ms: u64,
}

impl<V> MediaCache<V> {
    pub fn new(max_entries: usize, ttl_ms: u64) -> Self {
        Self {
            entries: Vec::new(),
            max_entries,
            ttl_ms,
        }
    }

    pub fn size(&self) -> usize {
        self.entries.len()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// 取值：命中且未过期则把条目触碰到 LRU 尾部并返回；过期则惰性删除返回 `None`。
    /// 过期判定与 Node 版一致：`now_ms - 写入时刻 > ttl_ms`（严格大于）。
    pub fn get(&mut self, key: &str, now_ms: u64) -> Option<&V> {
        let idx = self.entries.iter().position(|(k, _, _)| k == key)?;
        if now_ms.saturating_sub(self.entries[idx].2) > self.ttl_ms {
            self.entries.remove(idx);
            return None;
        }
        let entry = self.entries.remove(idx);
        self.entries.push(entry);
        self.entries.last().map(|(_, v, _)| v)
    }

    /// 写入：已存在则先移除再插尾（刷新时间与 LRU 位）；超容量淘汰头部最久未用。
    pub fn set(&mut self, key: &str, value: V, now_ms: u64) {
        self.entries.retain(|(k, _, _)| k != key);
        self.entries.push((key.to_owned(), value, now_ms));
        if self.entries.len() > self.max_entries {
            self.entries.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约与 Node 版 test-media-cache 一致：基础命中 + get 触碰改变 LRU 序。
    #[test]
    fn basic_hit_and_touch() {
        let mut c = MediaCache::new(3, 500);
        let now = 1000;
        c.set("a", 1, now);
        c.set("b", 2, now);
        c.set("c", 3, now);
        assert_eq!(c.get("a", now), Some(&1));
        assert_eq!(c.get("b", now), Some(&2));
        assert_eq!(c.size(), 3);
    }

    /// LRU 淘汰最久未用：a/b 已被 get 触碰，set d 时最旧的是 c → 淘汰 c。
    #[test]
    fn evicts_least_recently_used() {
        let mut c = MediaCache::new(3, 500);
        let now = 1000;
        c.set("a", 1, now);
        c.set("b", 2, now);
        c.set("c", 3, now);
        assert_eq!(c.get("a", now), Some(&1));
        assert_eq!(c.get("b", now), Some(&2));
        c.set("d", 4, now);
        assert_eq!(c.get("c", now), None);
        assert_eq!(c.get("d", now), Some(&4));
        assert_eq!(c.size(), 3);
    }

    /// TTL 过期后 miss（501ms > 500ms，惰性删除）。
    #[test]
    fn ttl_expiry_misses() {
        let mut c = MediaCache::new(3, 500);
        c.set("a", 1, 1000);
        assert_eq!(c.get("a", 1501), None);
    }

    /// clear 清空。
    #[test]
    fn clear_empties() {
        let mut c = MediaCache::new(3, 500);
        c.set("e", 5, 1000);
        c.clear();
        assert_eq!(c.size(), 0);
        assert_eq!(c.get("e", 1000), None);
    }
}
