//! 多引擎搜索的解析与聚合（移植自 Node 版 `src/web-search-agg.js`，契约测试同步迁移），
//! 供 mcp-web-search-safe（web-search-safe MCP server）使用。
//!
//! 质量设计：单引擎（Bing）对无 Cookie 客户端既有空壳页节流又有相关性降级，
//! 这里把 DuckDuckGo 的无 JS 前台（lite/html）作为第二路，靠交叉排序取信：
//! 两个引擎都排前的结果置顶，单源结果按位次交错补位，兼顾可信度与覆盖面。
//!
//! 分层调整：Node 版的 `settleWithin`（限时收集多路 Promise）是异步并发工具，
//! 不属纯逻辑层——Rust 版将由 mcp 侧以 tokio 超时等价实现，本模块不含。

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// 单条网页搜索结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// 单条图片搜索结果（原图直链）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImageResult {
    pub url: String,
    pub title: String,
}

/// 一个引擎的结果组（`source` 为引擎名，顺序即源可靠性顺序）。
#[derive(Debug, Clone)]
pub struct SearchGroup<'a> {
    pub source: &'a str,
    pub results: &'a [SearchResult],
}

/// 聚合后的输出条目（`source` 为命中引擎，双源命中形如 `bing+ddg`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregatedResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub source: String,
}

static INLINE_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)</?(?:em|strong|b|i|u|span|font|small|sub|sup)\b[^>]*>").unwrap()
});
static ANY_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
static WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

/// SERP 文本净化：行内高亮标签（em/b/strong…）剥成空串——SERP 高亮常嵌在中文词
/// 中间，换成空格会把「哔哩哔哩」断成「哔哩 哔哩」；其余标签换成空格；解实体后
/// 再去一轮标签（RSS 摘要里的标签常是 XML 转义形态 `&lt;b&gt;`，解完实体才现形）。
pub fn decode_html(s: &str) -> String {
    let s = INLINE_TAG.replace_all(s, "");
    let s = ANY_TAG.replace_all(&s, " ");
    let s = s
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    let s = s
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    let s = ANY_TAG.replace_all(&s, " ");
    WS.replace_all(&s, " ").trim().to_owned()
}

/// 剥 `<![CDATA[...]]>` 包裹。
fn strip_cdata(s: &str) -> String {
    static CDATA: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)<!\[CDATA\[(.*?)\]\]>").unwrap());
    CDATA.replace_all(s, "$1").into_owned()
}

fn capture(re: &Regex, s: &str) -> Option<String> {
    re.captures(s).map(|c| c[1].to_owned())
}

/// 解析 Bing 的 RSS 输出（`format=rss`）：item/title/link/description，稳定且面向程序。
pub fn parse_bing_rss(xml: &str) -> Vec<SearchResult> {
    static TITLE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?s)<title>(.*?)</title>").unwrap());
    static LINK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?s)<link>(.*?)</link>").unwrap());
    static DESC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?s)<description>(.*?)</description>").unwrap());
    let mut results = Vec::new();
    for block in xml.split("<item>").skip(1) {
        let item = block.split_once("</item>").map_or(block, |(h, _)| h);
        let title = capture(&TITLE, item).map(|t| decode_html(&strip_cdata(&t)));
        let url = capture(&LINK, item).map(|l| decode_html(&strip_cdata(&l)).trim().to_owned());
        let snippet = capture(&DESC, item).map(|d| decode_html(&strip_cdata(&d)));
        if let (Some(title), Some(url), Some(snippet)) = (title, url, snippet)
            && !url.is_empty()
            && !title.is_empty()
        {
            results.push(SearchResult {
                title,
                url,
                snippet,
            });
        }
        if results.len() >= 10 {
            break;
        }
    }
    results
}

/// 从一次 Bing HTML 响应里解析结果块；cn/www 两个前台共用，重试兜底复用。
pub fn parse_bing_results(html: &str) -> Vec<SearchResult> {
    static HREF: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)<a[^>]+href="(https?://[^"]+)""#).unwrap());
    static H2: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<h2[^>]*>(.*?)</h2>").unwrap());
    static P: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<p[^>]*>(.*?)</p>").unwrap());
    let mut results = Vec::new();
    for block in html.split(r#"<li class="b_algo""#).skip(1) {
        let Some(url) = capture(&HREF, block).map(|u| decode_html(&u)) else {
            continue;
        };
        let title = capture(&H2, block)
            .map(|t| decode_html(&t))
            .unwrap_or_default();
        let snippet = capture(&P, block)
            .map(|s| decode_html(&s))
            .unwrap_or_default();
        if !url.is_empty() && !title.is_empty() {
            results.push(SearchResult {
                title,
                url,
                snippet,
            });
        }
        if results.len() >= 10 {
            break;
        }
    }
    results
}

/// 百分号解码（等价 JS `decodeURIComponent`；非法序列或非 UTF-8 返回 `None`）。
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// DuckDuckGo 结果链接是 `//duckduckgo.com/l/?uddg=<urlencoded>` 跳转链，解出真实
/// 目标；少数情况下（设置了直出参数）本身就是 http(s) 直链，原样返回。
fn unwrap_ddg_href(href: &str) -> String {
    static UDDG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[?&]uddg=([^&]+)").unwrap());
    static HTTP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^https?://").unwrap());
    let raw = decode_html(href);
    if let Some(m) = UDDG.captures(&raw) {
        return percent_decode(&m[1]).unwrap_or_default();
    }
    if HTTP.is_match(&raw) {
        raw
    } else {
        String::new()
    }
}

/// 解析 DuckDuckGo 的无 JS 前台：lite（表格布局，class=result-link / result-snippet）
/// 与 html（div 布局，class=result__a / result__snippet）共用同一段抽取逻辑——
/// 两版的 class 名与引号风格不同，正则兼容单双引号；snippet 取「本条链接到下一条
/// 链接之间」的第一段，与两版的文档顺序都吻合。DDG 被限流时返回 anomaly 页，
/// 自然解析出 0 条。
pub fn parse_ddg_results(html: &str) -> Vec<SearchResult> {
    static LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?is)<a\b[^>]*class=["'][^"']*\bresult(?:-link|__a)\b[^"']*["'][^>]*>(.*?)</a>"#,
        )
        .unwrap()
    });
    static SNIPPET_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?is)\bclass=["'][^"']*\bresult(?:-snippet|__snippet)\b[^"']*["'][^>]*>(.*?)</(?:td|a|div|p)>"#,
        )
        .unwrap()
    });
    static HREF: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)href=["']([^"']+)["']"#).unwrap());
    let mut links: Vec<(String, String, usize)> = Vec::new(); // (url, title, match start)
    for m in LINK_RE.captures_iter(html) {
        let whole = m.get(0).unwrap();
        let Some(href) = capture(&HREF, whole.as_str()) else {
            continue;
        };
        let url = unwrap_ddg_href(&href);
        let title = decode_html(&m[1]);
        if !url.is_empty() && !title.is_empty() {
            links.push((url, title, whole.start()));
        }
    }
    let snippets: Vec<(String, usize)> = SNIPPET_RE
        .captures_iter(html)
        .map(|m| {
            let whole = m.get(0).unwrap();
            (decode_html(&m[1]), whole.start())
        })
        .collect();
    let mut results = Vec::new();
    for (i, (url, title, idx)) in links.iter().enumerate() {
        let next_idx = links.get(i + 1).map_or(html.len(), |(_, _, n)| *n);
        let snippet = snippets
            .iter()
            .find(|(_, s_idx)| *s_idx > *idx && *s_idx < next_idx)
            .map(|(text, _)| text.clone())
            .unwrap_or_default();
        results.push(SearchResult {
            title: title.clone(),
            url: url.clone(),
            snippet,
        });
        if results.len() >= 10 {
            break;
        }
    }
    results
}

/// 解析 360 搜索（www.so.com/s?q=）HTML 结果页。结果块为 `<li class="res-list">`，
/// 标题在 h3.res-title > a，链接的 data-mdurl 属性**直接携带真实目标 URL**（href 是
/// so.com/link 跳转链），摘要在 data-result-summary 的 span 里——因此能与 Bing 结果
/// 做归一化去重和交叉排序。实测无 Cookie 客户端连续查询稳定返回（搜狗同类场景
/// ~75% 是 antispider 验证页，百度直接 227 字节拦截页，均不可用）。
pub fn parse_so360_results(html: &str) -> Vec<SearchResult> {
    static ANCHOR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<h3[^>]*>\s*<a\b([^>]*)>(.*?)</a>").unwrap());
    static MDURL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)data-mdurl="([^"]+)""#).unwrap());
    static HREF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)href="([^"]+)""#).unwrap());
    static SUMMARY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)data-result-summary[^>]*>(.*?)</span>").unwrap());
    static DESC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?is)class="res-desc"[^>]*>(.*?)</p>"#).unwrap());
    let mut results = Vec::new();
    for block in html.split(r#"<li class="res-list""#).skip(1) {
        let Some(anchor) = ANCHOR.captures(block) else {
            continue;
        };
        let attrs = &anchor[1];
        let title = decode_html(&anchor[2]);
        // 优先 data-mdurl 真实直链；缺失时退回 href 跳转链（web_fetch 跟随重定向
        // 仍可用，只是无法参与跨引擎去重）。
        let url = capture(&MDURL, attrs)
            .or_else(|| capture(&HREF, attrs))
            .map(|u| decode_html(&u))
            .unwrap_or_default();
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let snippet = capture(&SUMMARY, block)
            .or_else(|| capture(&DESC, block))
            .map(|s| decode_html(&s))
            .unwrap_or_default();
        results.push(SearchResult {
            title,
            url,
            snippet,
        });
        if results.len() >= 10 {
            break;
        }
    }
    results
}

/// 解析 360 图片搜索（image.so.com/i?src=imageonebox&q=…）结果页。页面里嵌着
/// JSON 结果数组，每条以 `{"id":"<hex>"` 开头；`img` 字段是**原图直链**（t01 前缀；
/// t04/_240_ 是缩略图，别拿去当表情包素材），title 是 \uXXXX 转义的中文。
/// 按条目切开后逐字段正则抽取，不做整体 JSON.parse（条目数组嵌在 HTML 里，
/// 首尾不完整），title 片段单独按 JSON 字符串字面量解转义。
pub fn parse_so360_image_results(html: &str, limit: usize) -> Vec<ImageResult> {
    static IMG: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""img":"(https?:[^"]+)""#).unwrap());
    static TITLE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""title":"((?:[^"\\]|\\.)*)""#).unwrap());
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for block in html.split(r#"{"id":"#).skip(1) {
        // URL 里的斜杠是 JSON 转义形态（http:\/\/…），整段抓再解 \/。
        let Some(img) = capture(&IMG, block) else {
            continue;
        };
        let url = img.replace("\\/", "/");
        let title = capture(&TITLE, block)
            .and_then(|frag| serde_json::from_str::<String>(&format!("\"{frag}\"")).ok())
            .unwrap_or_default();
        if !seen.insert(url.clone()) {
            continue;
        }
        out.push(ImageResult { url, title });
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// 结果去重键：清 hash、host 小写、剥 utm_* 跟踪参数、去非根路径的尾斜杠。
/// 只剥 utm_*（不碰 from/spm 等业务参数）：误并的代价是少一条结果，
/// 而带不同业务参数的页面可能是不同内容，宁可漏并不可误并。
/// 解析失败（相对 URL 等）原样返回。
pub fn normalize_url_key(url: &str) -> String {
    let Ok(mut u) = url::Url::parse(url) else {
        return url.to_owned();
    };
    u.set_fragment(None);
    let pairs: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !k.to_ascii_lowercase().starts_with("utm_"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if pairs.is_empty() {
        u.set_query(None);
    } else {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &pairs {
            ser.append_pair(k, v);
        }
        u.set_query(Some(&ser.finish()));
    }
    let mut s = u.to_string();
    if u.path() != "/" && s.ends_with('/') {
        s.pop();
    }
    s
}

#[derive(Debug)]
struct MergedEntry {
    title: String,
    url: String,
    snippet: String,
    sources: Vec<String>,
    best_rank: usize,
    first_seen: usize,
}

/// 聚合多引擎结果：URL 归一化去重后交叉排序。
/// `groups` 按源可靠性顺序传入；输出条目带 `source` 字段（双源命中为 `bing+ddg`）。
/// 多源都命中的排最前（按最好位次），单源结果按位次交错补位（位次平局时靠前的
/// 源优先），最后截 `limit`。首见条目的 title/url/snippet 保留。
pub fn aggregate_search_results(groups: &[SearchGroup<'_>], limit: usize) -> Vec<AggregatedResult> {
    let source_order = |name: &str| groups.iter().position(|g| g.source == name).unwrap_or(99);
    // 以 Vec 维持插入序（JS Map 语义），条目量小线性查找即可。
    let mut by_key: Vec<(String, MergedEntry)> = Vec::new();
    let mut first_seen = 0usize;
    for group in groups {
        for (i, r) in group.results.iter().enumerate() {
            if r.url.is_empty() || r.title.is_empty() {
                continue;
            }
            let key = normalize_url_key(&r.url);
            if let Some((_, cur)) = by_key.iter_mut().find(|(k, _)| *k == key) {
                if !cur.sources.iter().any(|s| s == group.source) {
                    cur.sources.push(group.source.to_owned());
                }
                cur.best_rank = cur.best_rank.min(i);
            } else {
                by_key.push((
                    key,
                    MergedEntry {
                        title: r.title.clone(),
                        url: r.url.clone(),
                        snippet: r.snippet.clone(),
                        sources: vec![group.source.to_owned()],
                        best_rank: i,
                        first_seen,
                    },
                ));
                first_seen += 1;
            }
        }
    }
    let mut merged: Vec<MergedEntry> = by_key.into_iter().map(|(_, e)| e).collect();
    merged.sort_by(|a, b| {
        let cross_a = a.sources.len() > 1;
        let cross_b = b.sources.len() > 1;
        cross_b
            .cmp(&cross_a)
            .then(a.best_rank.cmp(&b.best_rank))
            .then(source_order(&a.sources[0]).cmp(&source_order(&b.sources[0])))
            // 位次、源优先级全平局时保持首见序（JS 稳定排序语义）。
            .then(a.first_seen.cmp(&b.first_seen))
    });
    merged
        .into_iter()
        .take(limit)
        .map(|e| AggregatedResult {
            title: e.title,
            url: e.url,
            snippet: e.snippet,
            source: e.sources.join("+"),
        })
        .collect()
}

/// Bing 图片搜索的结果页把每条结果的元数据放在元素的 `m="…"` 属性里（HTML 转义
/// 的 JSON），其中 murl 是原图直链。不支持 format=rss（返回的是 HTML 页），所以
/// 只能从 HTML 里抽；www 与 cn 两个前台都试，空结果时换下一个（与 web_search 的
/// 空壳节流是同类现象）。解码失败的原样保留（Node 版 try/catch 语义）。
pub fn extract_bing_image_urls(html: &str, limit: usize) -> Vec<String> {
    static MURL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"murl&quot;:&quot;(https?://[^&]+)&quot;"#).unwrap());
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for m in MURL.captures_iter(html) {
        let u = percent_decode(&m[1]).unwrap_or_else(|| m[1].to_owned());
        if !seen.insert(u.clone()) {
            continue;
        }
        out.push(u);
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// 引擎熔断器：某引擎连续失败（网络不可达/被墙/超时）到阈值后进入冷却，冷却期内
/// 直接拒绝发起，避免每次搜索都陪跑一个注定超时的引擎。成功即清零；冷却结束
/// 自动复试（代理恢复/网络变化后最多延迟一个冷却周期重新发现引擎可用）。
///
/// 分层调整：Node 版以 `now` 闭包注入时钟，Rust 版改为方法显式传 `now_ms`。
#[derive(Debug, Clone)]
pub struct EngineBreaker {
    fail_threshold: u32,
    cooldown_ms: u64,
    fails: u32,
    cooldown_until: u64,
}

impl EngineBreaker {
    pub fn new(fail_threshold: u32, cooldown_ms: u64) -> Self {
        Self {
            fail_threshold,
            cooldown_ms,
            fails: 0,
            cooldown_until: 0,
        }
    }

    pub fn can_run(&self, now_ms: u64) -> bool {
        now_ms >= self.cooldown_until
    }

    pub fn record_success(&mut self) {
        self.fails = 0;
        self.cooldown_until = 0;
    }

    /// 记一次失败；到阈值即进冷却并清零计数。
    pub fn record_failure(&mut self, now_ms: u64) {
        self.fails += 1;
        if self.fails >= self.fail_threshold {
            self.cooldown_until = now_ms + self.cooldown_ms;
            self.fails = 0;
        }
    }

    /// 调试视图 `(fails, cooldown_until)`。
    pub fn state(&self) -> (u32, u64) {
        (self.fails, self.cooldown_until)
    }
}

/// 搜索结果条目在 mcp 侧的 JSON 形状（序列化字段名与 Node 版一致）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResultJson {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约与 Node 版 test-web-search-agg 一致：RSS 解析条数、CDATA/实体、摘要去标签。
    #[test]
    fn parses_bing_rss() {
        let xml = r#"<?xml version="1.0"?>
<rss><channel>
<item><title><![CDATA[萌娘百科 &amp; 其他]]></title><link>https://example.com/a?utm_source=bing</link><description>这是 &lt;b&gt;摘要&lt;/b&gt; 一</description></item>
<item><title>第二条</title><link>https://example.org/b</link><description><![CDATA[摘要二]]></description></item>
</channel></rss>"#;
        let rs = parse_bing_rss(xml);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].title, "萌娘百科 & 其他");
        assert_eq!(rs[0].snippet, "这是 摘要 一");
        assert_eq!(rs[1].url, "https://example.org/b");
    }

    /// Bing HTML 解析：b_algo 块 + h2 标题。
    #[test]
    fn parses_bing_html() {
        let html = r#"<ul id="b_results">
<li class="b_algo"><h2><a href="https://example.com/x">结果一</a></h2><p>摘要一</p></li>
<li class="b_algo"><h2><a href="https://example.com/y">结果二</a></h2><p>摘要二</p></li>
</ul>"#;
        let rs = parse_bing_results(html);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].title, "结果一");
    }

    /// DDG lite：表格布局、单引号属性、uddg 跳转链解包。
    #[test]
    fn parses_ddg_lite() {
        let html = r#"<table>
<tr><td>1.&nbsp;</td><td><a rel="nofollow" href='//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&amp;rut=abc123' class='result-link'>第一条 &amp; 标题</a></td></tr>
<tr><td class='result-snippet'>这是 &lt;b&gt;lite&lt;/b&gt; 摘要</td></tr>
<tr><td>2.&nbsp;</td><td><a rel="nofollow" href='//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fdoc&amp;rut=def456' class='result-link'>第二条标题</a></td></tr>
<tr><td class='result-snippet'>第二条摘要</td></tr>
</table>"#;
        let rs = parse_ddg_results(html);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].url, "https://example.com/page");
        assert_eq!(rs[0].title, "第一条 & 标题");
        assert_eq!(rs[1].snippet, "第二条摘要");
    }

    /// DDG html：div 布局、双引号、直链保留。
    #[test]
    fn parses_ddg_html() {
        let html = r##"<div class="result results_links web-result">
<h2 class="result__title"><a rel="nofollow" class="result__a" href="https://example.com/direct">直链结果</a></h2>
<a class="result__snippet" href="#">html 版摘要</a>
</div>
<div class="result">
<h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.net%2Fz">跳转结果</a></h2>
<a class="result__snippet" href="#">另一条摘要</a>
</div>"##;
        let rs = parse_ddg_results(html);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].url, "https://example.com/direct");
        assert_eq!(rs[1].url, "https://example.net/z");
        assert_eq!(rs[1].snippet, "另一条摘要");
    }

    /// 360：data-mdurl 真实直链优先、em 高亮剥离、summary 优先、res-desc 兜底。
    #[test]
    fn parses_so360() {
        let html = r#"<ul class="result">
<li class="res-list" data-result-type="normal" data-result-pos="1"><h3 class="res-title " > <a href="https://www.so.com/link?m=zVNm67YN4uw" data-mdurl="https://www.bilibili.com/video/BV1MKK5eZE2w/" rel="noopener" target="_blank">【<em>DeepSeek</em>保姆级教程】_<em>哔哩</em>哔哩_bilibili</a></h3> <p class="res-desc"><span class="gray g-c-gray">2025年2月12日&nbsp;-&nbsp;</span><span data-result-summary>【保姆级教程】手把手教你<em>DeepSeek</em>接入微信!各种调用!</span></p></li>
<li class="res-list" data-result-type="richsum" data-result-pos="2"><h3 class="res-title"><a href="https://www.so.com/link?m=ueeV4SX3Gq" data-mdurl="https://example.com/doc?a=1&amp;b=2" target="_blank">没有摘要 span 的结果</a></h3><p class="res-desc">res-desc 兜底摘要</p></li>
<li class="res-list" data-result-type="normal" data-result-pos="3"><h3 class="res-title"><a href="https://www.so.com/link?m=onlyHref">没有 mdurl 的结果</a></h3></li>
</ul>"#;
        let rs = parse_so360_results(html);
        assert_eq!(rs.len(), 3);
        assert_eq!(rs[0].url, "https://www.bilibili.com/video/BV1MKK5eZE2w/");
        assert_eq!(rs[0].title, "【DeepSeek保姆级教程】_哔哩哔哩_bilibili");
        assert_eq!(
            rs[0].snippet,
            "【保姆级教程】手把手教你DeepSeek接入微信!各种调用!"
        );
        assert_eq!(rs[1].url, "https://example.com/doc?a=1&b=2");
        assert_eq!(rs[1].snippet, "res-desc 兜底摘要");
        assert_eq!(rs[2].url, "https://www.so.com/link?m=onlyHref");
    }

    /// 360 图片：JSON 嵌 HTML、\/ 转义 URL、\uXXXX 标题、无 img 条目跳过。
    #[test]
    fn parses_so360_images() {
        let html = r#"var x = [{"id":"aa","title":"happy\u732b\u8868\u60c5\u5305","img":"http:\/\/p0.so.qhimg.com\/t01abc.jpg"},{"id":"bb","title":"可爱猫","img":"https:\/\/p1.so.qhimg.com\/t02def.jpg"},{"id":"cc","noimg":true}];"#;
        let rs = parse_so360_image_results(html, 12);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].url, "http://p0.so.qhimg.com/t01abc.jpg");
        assert_eq!(rs[0].title, "happy猫表情包");
    }

    /// URL 归一化：utm 剥离、hash 清除、尾斜杠、业务参数保留、非法 URL 原样。
    #[test]
    fn normalizes_url_keys() {
        assert_eq!(
            normalize_url_key("https://Example.com/a?utm_source=x&utm_medium=y"),
            "https://example.com/a"
        );
        assert_eq!(
            normalize_url_key("https://example.com/a#sec"),
            "https://example.com/a"
        );
        assert_eq!(
            normalize_url_key("https://example.com/a/"),
            "https://example.com/a"
        );
        assert_eq!(
            normalize_url_key("https://example.com/"),
            "https://example.com/"
        );
        assert_eq!(
            normalize_url_key("https://example.com/a?from=web"),
            "https://example.com/a?from=web"
        );
        assert_eq!(normalize_url_key("not a url"), "not a url");
    }

    /// 聚合：交叉命中置顶、去重、单源交错、source 标注、limit。
    #[test]
    fn aggregates_cross_engine_results() {
        let bing = vec![
            SearchResult {
                title: "B1 独有".into(),
                url: "https://b1.com/".into(),
                snippet: String::new(),
            },
            SearchResult {
                title: "共同结果".into(),
                url: "https://shared.com/page?utm_source=bing".into(),
                snippet: String::new(),
            },
            SearchResult {
                title: "B3".into(),
                url: "https://b3.com/".into(),
                snippet: String::new(),
            },
        ];
        let ddg = vec![
            SearchResult {
                title: "D1 独有".into(),
                url: "https://d1.com/x".into(),
                snippet: String::new(),
            },
            SearchResult {
                title: "共同结果".into(),
                url: "https://shared.com/page".into(),
                snippet: String::new(),
            },
        ];
        let out = aggregate_search_results(
            &[
                SearchGroup {
                    source: "bing",
                    results: &bing,
                },
                SearchGroup {
                    source: "ddg",
                    results: &ddg,
                },
            ],
            4,
        );
        assert_eq!(out[0].source, "bing+ddg");
        assert_eq!(out[0].url, "https://shared.com/page?utm_source=bing");
        assert_eq!(out.len(), 4);
        // B1 与 D1 位次同为 0，Bing 源优先 → 交错序 b1、d1。
        assert_eq!(out[1].url, "https://b1.com/");
        assert_eq!(out[2].url, "https://d1.com/x");
        assert_eq!(out[1].source, "bing");
        assert_eq!(out[2].source, "ddg");

        let single = aggregate_search_results(
            &[SearchGroup {
                source: "ddg",
                results: &ddg,
            }],
            10,
        );
        assert_eq!(single.len(), 2);
        assert!(aggregate_search_results(&[], 10).is_empty());
        // 空 results 组安全（Node 版「无 results 字段的组」的强类型等价物）。
        assert!(
            aggregate_search_results(
                &[SearchGroup {
                    source: "x",
                    results: &[]
                }],
                10
            )
            .is_empty()
        );
    }

    /// Bing 图片：murl HTML 转义 JSON 抽取、解码、去重、limit。
    #[test]
    fn extracts_bing_image_urls() {
        let html = r#"m="{""murl&quot;:&quot;https://example.com/a%20b.jpg&quot;}" m="{""murl&quot;:&quot;https://example.com/a%20b.jpg&quot;}" m="{""murl&quot;:&quot;https://example.com/c.jpg&quot;}""#;
        let urls = extract_bing_image_urls(html, 12);
        assert_eq!(
            urls,
            vec!["https://example.com/a b.jpg", "https://example.com/c.jpg"]
        );
    }

    /// 熔断器：阈值触发、冷却边界、成功清零。
    #[test]
    fn engine_breaker_lifecycle() {
        let mut br = EngineBreaker::new(2, 5000);
        let mut clock = 1000u64;
        assert!(br.can_run(clock));
        br.record_failure(clock);
        assert!(br.can_run(clock), "未到阈值仍可运行");
        br.record_failure(clock);
        assert!(!br.can_run(clock), "连续失败触发熔断");
        clock += 4999;
        assert!(!br.can_run(clock), "冷却期内仍拒绝");
        clock += 1;
        assert!(br.can_run(clock), "冷却结束恢复");
        br.record_failure(clock);
        br.record_success();
        br.record_failure(clock);
        assert!(br.can_run(clock), "成功清零计数（1 次失败不熔断）");
    }

    // Node 版测试第 9 段「限时收集」（settleWithin）是异步并发工具，按分层归
    // mcp 侧（tokio 超时实现），其契约在 mcp 层落地时转译，不在 core 单测范围。
}
