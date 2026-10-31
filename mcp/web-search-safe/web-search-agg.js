// 搜索解析与聚合（纯函数，供 mcp-web-search-safe.js 使用、离线单测覆盖）。
// 质量设计：单引擎（Bing）对无 Cookie 客户端既有空壳页节流又有相关性降级，
// 这里把 DuckDuckGo 的无 JS 前台（lite/html）作为第二路，靠交叉排序取信：
// 两个引擎都排前的结果置顶，单源结果按位次交错补位，兼顾可信度与覆盖面。
function decodeHtml(s) {
  return String(s ?? '')
    // 行内高亮标签（em/b/strong…）剥成空串：SERP 高亮常嵌在中文词中间，
    // 换成空格会把「哔哩哔哩」断成「哔哩 哔哩」。
    .replace(/<\/?(?:em|strong|b|i|u|span|font|small|sub|sup)\b[^>]*>/gi, '')
    .replace(/<[^>]+>/g, ' ')
    .replace(/&amp;/g, '&')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/&nbsp;/g, ' ')
    // RSS 摘要里的标签常是 XML 转义形态（&lt;b&gt;），解完实体才现形，再去一轮。
    .replace(/<[^>]+>/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();
}

function stripCdata(s) {
  return String(s ?? '').replace(/<!\[CDATA\[([\s\S]*?)\]\]>/g, '$1');
}

// 解析 Bing 的 RSS 输出（`format=rss`）：item/title/link/description，稳定且面向程序。
export function parseBingRss(xml) {
  const results = [];
  for (const block of xml.split('<item>').slice(1)) {
    const end = block.indexOf('</item>');
    const item = end >= 0 ? block.slice(0, end) : block;
    const titleMatch = item.match(/<title>([\s\S]*?)<\/title>/i);
    const linkMatch = item.match(/<link>([\s\S]*?)<\/link>/i);
    const descMatch = item.match(/<description>([\s\S]*?)<\/description>/i);
    const title = titleMatch ? decodeHtml(stripCdata(titleMatch[1])) : '';
    const urlStr = linkMatch ? decodeHtml(stripCdata(linkMatch[1])).trim() : '';
    const snippet = descMatch ? decodeHtml(stripCdata(descMatch[1])) : '';
    if (urlStr && title) results.push({ title, url: urlStr, snippet });
    if (results.length >= 10) break;
  }
  return results;
}

// 从一次 Bing HTML 响应里解析结果块；cn/www 两个前台共用，抽出函数供重试兜底复用。
export function parseBingResults(html) {
  const results = [];
  const blocks = html.split('<li class="b_algo"').slice(1);
  for (const block of blocks) {
    const hrefMatch = block.match(/<a[^>]+href="(https?:\/\/[^"]+)"/i);
    if (!hrefMatch) continue;
    const urlStr = decodeHtml(hrefMatch[1]);
    const titleMatch = block.match(/<h2[^>]*>([\s\S]*?)<\/h2>/i);
    const title = titleMatch ? decodeHtml(titleMatch[1]) : '';
    const snippetMatch = block.match(/<p[^>]*>([\s\S]*?)<\/p>/i);
    const snippet = snippetMatch ? decodeHtml(snippetMatch[1]) : '';
    if (urlStr && title) results.push({ title, url: urlStr, snippet });
    if (results.length >= 10) break;
  }
  return results;
}

// DuckDuckGo 结果链接是 `//duckduckgo.com/l/?uddg=<urlencoded>` 跳转链，解出真实目标；
// 少数情况下（设置了直出参数）本身就是 http(s) 直链，原样返回。
function unwrapDdgHref(href) {
  const raw = decodeHtml(href);
  const m = raw.match(/[?&]uddg=([^&]+)/);
  if (m) {
    try { return decodeURIComponent(m[1]); } catch { return ''; }
  }
  return /^https?:\/\//i.test(raw) ? raw : '';
}

// 解析 DuckDuckGo 的无 JS 前台：lite（表格布局，class=result-link / result-snippet）
// 与 html（div 布局，class=result__a / result__snippet）共用同一段抽取逻辑——
// 两版的 class 名与引号风格不同，正则兼容单双引号；snippet 取「本条链接到下一条链接之间」
// 的第一段，与两版的文档顺序都吻合。DDG 被限流时返回 anomaly 页，自然解析出 0 条。
export function parseDdgResults(html) {
  const linkRe = /<a\b[^>]*class=["'][^"']*\bresult(?:-link|__a)\b[^"']*["'][^>]*>([\s\S]*?)<\/a>/gi;
  const links = [];
  let m;
  while ((m = linkRe.exec(html)) !== null) {
    const hrefMatch = m[0].match(/href=["']([^"']+)["']/i);
    if (!hrefMatch) continue;
    const url = unwrapDdgHref(hrefMatch[1]);
    const title = decodeHtml(m[1]);
    if (url && title) links.push({ url, title, idx: m.index });
  }
  const snippetRe = /\bclass=["'][^"']*\bresult(?:-snippet|__snippet)\b[^"']*["'][^>]*>([\s\S]*?)<\/(?:td|a|div|p)>/gi;
  const snippets = [];
  while ((m = snippetRe.exec(html)) !== null) {
    snippets.push({ text: decodeHtml(m[1]), idx: m.index });
  }
  const results = [];
  for (let i = 0; i < links.length; i += 1) {
    const nextIdx = i + 1 < links.length ? links[i + 1].idx : html.length;
    const snippet = snippets.find((s) => s.idx > links[i].idx && s.idx < nextIdx);
    results.push({ title: links[i].title, url: links[i].url, snippet: snippet ? snippet.text : '' });
    if (results.length >= 10) break;
  }
  return results;
}

// 解析 360 搜索（www.so.com/s?q=）HTML 结果页。结果块为 <li class="res-list">，
// 标题在 h3.res-title > a，链接的 data-mdurl 属性**直接携带真实目标 URL**（href 是
// so.com/link 跳转链），摘要在 data-result-summary 的 span 里——因此能与 Bing 结果
// 做归一化去重和交叉排序。实测无 Cookie 客户端连续查询稳定返回（搜狗同类场景
// ~75% 是 antispider 验证页，百度直接 227 字节拦截页，均不可用）。
export function parseSo360Results(html) {
  const results = [];
  const blocks = html.split('<li class="res-list"').slice(1);
  for (const block of blocks) {
    const anchor = block.match(/<h3[^>]*>\s*<a\b([^>]*)>([\s\S]*?)<\/a>/i);
    if (!anchor) continue;
    const attrs = anchor[1];
    const title = decodeHtml(anchor[2]);
    // 优先 data-mdurl 真实直链；缺失时退回 href 跳转链（web_fetch 跟随重定向仍可用，
    // 只是无法参与跨引擎去重）。
    const mdurl = attrs.match(/data-mdurl="([^"]+)"/i);
    const href = attrs.match(/href="([^"]+)"/i);
    const urlStr = mdurl ? decodeHtml(mdurl[1]) : href ? decodeHtml(href[1]) : '';
    if (!urlStr || !title) continue;
    const summary = block.match(/data-result-summary[^>]*>([\s\S]*?)<\/span>/i);
    const desc = block.match(/class="res-desc"[^>]*>([\s\S]*?)<\/p>/i);
    const snippet = summary ? decodeHtml(summary[1]) : desc ? decodeHtml(desc[1]) : '';
    results.push({ title, url: urlStr, snippet });
    if (results.length >= 10) break;
  }
  return results;
}

// 解析 360 图片搜索（image.so.com/i?src=imageonebox&q=…）结果页。页面里嵌着
// JSON 结果数组，每条以 {"id":"<hex>" 开头；`img` 字段是**原图直链**（t01 前缀；
// t04/_240_ 是缩略图，别拿去当表情包素材），title 是 \uXXXX 转义的中文。
// 按条目切开后逐字段正则抽取，不做整体 JSON.parse（条目数组嵌在 HTML 里，
// 首尾不完整），title 片段单独 JSON.parse 解转义。
export function parseSo360ImageResults(html, limit = 12) {
  const out = [];
  const seen = new Set();
  for (const block of String(html ?? '').split('{"id":').slice(1)) {
    // URL 里的斜杠是 JSON 转义形态（http:\/\/…），用 [^"]+ 整段抓再解 \/。
    const imgMatch = block.match(/"img":"(https?:[^"]+)"/);
    if (!imgMatch) continue;
    const url = imgMatch[1].split('\\/').join('/');
    let title = '';
    const titleMatch = block.match(/"title":"((?:[^"\\]|\\.)*)"/);
    if (titleMatch) {
      try { title = JSON.parse(`"${titleMatch[1]}"`); } catch { title = ''; }
    }
    if (seen.has(url)) continue;
    seen.add(url);
    out.push({ url, title });
    if (out.length >= limit) break;
  }
  return out;
}

// 结果去重键：清 hash、host 小写、剥 utm_* 跟踪参数、去非根路径的尾斜杠。
// 只剥 utm_*（不碰 from/spm 等业务参数）：误并的代价是少一条结果，
// 而带不同业务参数的页面可能是不同内容，宁可漏并不可误并。
export function normalizeUrlKey(url) {
  try {
    const u = new URL(String(url));
    u.hash = '';
    u.hostname = u.hostname.toLowerCase();
    for (const k of [...u.searchParams.keys()]) {
      if (/^utm_/i.test(k)) u.searchParams.delete(k);
    }
    let s = u.toString();
    if (u.pathname !== '/' && s.endsWith('/')) s = s.slice(0, -1);
    return s;
  } catch {
    return String(url ?? '');
  }
}

/**
 * 聚合多引擎结果：URL 归一化去重后交叉排序。
 * groups 形如 [{ source: 'bing', results }, { source: 'ddg', results }]（按源可靠性顺序）；
 * 输出条目带 source 字段（双源命中为 'bing+ddg'）。多源都命中的排最前（按最好位次），
 * 单源结果按位次交错补位（位次平局时靠前的源优先），最后截 limit。
 */
export function aggregateSearchResults(groups, { limit = 10 } = {}) {
  const valid = (Array.isArray(groups) ? groups : []).filter((g) => g && Array.isArray(g.results));
  const sourceOrder = new Map(valid.map((g, i) => [String(g.source), i]));
  const byKey = new Map();
  for (const { source, results } of valid) {
    results.forEach((r, i) => {
      if (!r || !r.url || !r.title) return;
      const key = normalizeUrlKey(r.url);
      const cur = byKey.get(key);
      if (cur) {
        if (!cur.sources.includes(source)) cur.sources.push(source);
        cur.bestRank = Math.min(cur.bestRank, i);
      } else {
        byKey.set(key, { title: r.title, url: r.url, snippet: r.snippet || '', sources: [String(source)], bestRank: i });
      }
    });
  }
  const merged = [...byKey.values()];
  merged.sort((a, b) => {
    const crossA = a.sources.length > 1;
    const crossB = b.sources.length > 1;
    if (crossA !== crossB) return crossA ? -1 : 1;
    if (a.bestRank !== b.bestRank) return a.bestRank - b.bestRank;
    return (sourceOrder.get(a.sources[0]) ?? 99) - (sourceOrder.get(b.sources[0]) ?? 99);
  });
  return merged.slice(0, limit).map((e) => ({
    title: e.title,
    url: e.url,
    snippet: e.snippet,
    source: e.sources.join('+'),
  }));
}

// Bing 图片搜索的结果页把每条结果的元数据放在元素的 m="…" 属性里（HTML 转义的
// JSON），其中 murl 是原图直链。不支持 format=rss（返回的是 HTML 页），所以只能
// 从 HTML 里抽；www 与 cn 两个前台都试，空结果时换下一个（与 web_search 的
// 空壳节流是同类现象）。
export function extractBingImageUrls(html, limit = 12) {
  const out = [];
  const seen = new Set();
  for (const m of html.matchAll(/murl&quot;:&quot;(https?:\/\/[^&]+)&quot;/g)) {
    let u = m[1];
    try { u = decodeURIComponent(u); } catch {}
    if (seen.has(u)) continue;
    seen.add(u);
    out.push(u);
    if (out.length >= limit) break;
  }
  return out;
}

// 引擎熔断器：某引擎连续失败（网络不可达/被墙/超时）到阈值后进入冷却，冷却期内
// 直接拒绝发起，避免每次搜索都陪跑一个注定超时的引擎。成功即清零；冷却结束自动
// 复试（代理恢复/网络变化后最多延迟一个冷却周期重新发现引擎可用）。
export function createEngineBreaker({ failThreshold = 2, cooldownMs = 30 * 60 * 1000, now = () => Date.now() } = {}) {
  let fails = 0;
  let cooldownUntil = 0;
  return {
    canRun() {
      return now() >= cooldownUntil;
    },
    recordSuccess() {
      fails = 0;
      cooldownUntil = 0;
    },
    recordFailure() {
      fails += 1;
      if (fails >= failThreshold) {
        cooldownUntil = now() + cooldownMs;
        fails = 0;
      }
    },
    get state() {
      return { fails, cooldownUntil };
    },
  };
}

// 限时收集多路 Promise 的 settle 结果：ms 内完成多少算多少，未完成的一律标
// 'timeout'（调用方按失败处理，可计入熔断）。搜索响应延迟因此只取决于最快的
// 可用引擎，而不是最慢的坏引擎。
export function settleWithin(promises, ms) {
  return new Promise((resolve) => {
    const results = promises.map(() => ({ status: 'timeout' }));
    let remaining = promises.length;
    let settled = false;
    const finish = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve([...results]);
    };
    const timer = setTimeout(finish, ms);
    promises.forEach((p, i) => {
      Promise.resolve(p).then(
        (value) => {
          if (settled) return;
          results[i] = { status: 'fulfilled', value };
          remaining -= 1;
          if (remaining === 0) finish();
        },
        (reason) => {
          if (settled) return;
          results[i] = { status: 'rejected', reason };
          remaining -= 1;
          if (remaining === 0) finish();
        },
      );
    });
  });
}
