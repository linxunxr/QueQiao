// 安全版 Web Search / Fetch MCP server（stdio）。由 DSH 的 MCP 客户端 spawn。
//
// 本文件是 qq-bridge src/mcp-web-search-safe.js 的零依赖迁移版：工具逻辑
// （多引擎聚合/清洗/受限抓取）原样保留，仅把 @modelcontextprotocol/sdk 的
// 注册与传输换成手写 JSON-RPC（协议形状与 queqiao-mcp 一致，同为 DSH
// 验证过的实现）——否则安装包须携带 node_modules，随包分发成本不可接受。
//
// 安全设计：
// - 只暴露只读工具 `web_search` / `image_search` / `web_fetch`：查网络用语/梗/黑话、找表情包素材、抓取网页正文。
// - 不暴露任何本地文件、命令执行、写操作。
// - 查询词做基础清洗：去 CQ 码、控制字符、超长截断。
// - `web_fetch` 仅允许 http/https：
//   - 禁止 URL 内嵌凭据；
//   - 禁止 localhost / .local / 私有 IP / 环回 / 链路本地 / CGNAT 等内网地址；
//   - 域名会先做 DNS 解析并检查全部解析结果，避免解析到内网；
//   - 手动跟随重定向，每一跳都重新校验；
//   - 响应体按字节流限量读取，避免超大响应拖垮进程。
// - 搜索结果/抓取结果仅作为“候选解释”，最终是否入库仍由控制台人工确认。
//
// 搜索质量设计（解析/聚合的纯函数在 web-search-agg.js，Rust 侧
// queqiao-core/src/search.rs 有同步契约测试）：
// - web_search 聚合三路引擎，并行查询、单路失败不影响整体：
//   · Bing：RSS(cn) → RSS(www) → HTML(cn) → HTML(www) 逐级兜底。cn.bing.com 的
//     HTML 前台对无 Cookie 客户端间歇性下发「空壳页」（HTTP 200 但 0 条结果）且
//     相关性常被降级，RSS 则稳定输出 organic 结果（2026-10-16 实测）；
//   · 360：结果块的 data-mdurl 直接带真实目标 URL，能与 Bing 交叉去重；
//   · DuckDuckGo：lite → html 两个无 JS 前台（国内无代理网络不可达，靠熔断退场）。
//   · 两路以上都成功时按归一化 URL 去重、交叉排序（双源命中的排前）。
// - 全部拿不到结果时不再返回看似权威的裸空列表，而是带 note 说明「可能被限流」，
//   让模型能说「暂时搜不到」而不是把节流报告成「网上什么都没有」（群聊实录翻过车）。
import readline from 'node:readline';
import { safeFetch } from './safe-fetch.js';
import {
  parseBingResults,
  parseBingRss,
  parseDdgResults,
  parseSo360Results,
  parseSo360ImageResults,
  extractBingImageUrls,
  aggregateSearchResults,
  createEngineBreaker,
  settleWithin,
} from './web-search-agg.js';

function sanitizeQuery(query) {
  return String(query ?? '')
    // 去掉 CQ 码（[CQ:xxx]）
    .replace(/\[CQ:[^\]]*\]/gi, ' ')
    .replace(/[\u0000-\u001f\u007f]/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .slice(0, 120);
}

async function fetchBingPage(host, query, { rss = false, extraParams = {} } = {}) {
  const url = new URL(`https://${host}/search`);
  url.searchParams.set('q', query);
  if (rss) url.searchParams.set('format', 'rss');
  for (const [key, value] of Object.entries(extraParams)) url.searchParams.set(key, value);
  // 搜索也使用同一条受限传输链路，重定向必须重新校验，正文最多 512K 字符。
  const res = await safeFetch(url.toString(), 512000);
  if (res.statusCode < 200 || res.statusCode >= 300) throw new Error(`搜索服务 HTTP ${res.statusCode}`);
  return rss ? parseBingRss(res.body) : parseBingResults(res.body);
}

async function bingSearch(query) {
  const attempts = [
    { host: 'cn.bing.com', waitMs: 0, rss: true },
    { host: 'www.bing.com', waitMs: 1000, rss: true, extraParams: { setmkt: 'zh-CN' } },
    { host: 'cn.bing.com', waitMs: 1000, rss: false },
    { host: 'www.bing.com', waitMs: 0, rss: false, extraParams: { setmkt: 'zh-CN' } },
  ];
  let sawEmptyPage = false;
  let lastError = null;
  for (const { host, waitMs, rss, extraParams } of attempts) {
    if (waitMs) await new Promise((resolve) => setTimeout(resolve, waitMs));
    try {
      const results = await fetchBingPage(host, query, { rss, extraParams });
      if (results.length > 0) return results;
      sawEmptyPage = true;
    } catch (error) {
      lastError = error;
    }
  }
  if (sawEmptyPage) return [];
  throw lastError || new Error('Bing 搜索没有返回结果');
}

// DuckDuckGo 无 JS 前台：lite 表格版优先（最好解析），html 版兜底（同类空页节流换前台）。
// kl=cn-zh 倾向中文区结果。网络/HTTP 错误向外抛（由聚合层容错），正常无结果返回 []。
async function ddgSearch(query) {
  const attempts = [
    `https://lite.duckduckgo.com/lite/?q=${encodeURIComponent(query)}&kl=cn-zh`,
    `https://html.duckduckgo.com/html/?q=${encodeURIComponent(query)}&kl=cn-zh`,
  ];
  let lastError = null;
  for (const url of attempts) {
    try {
      const res = await safeFetch(url, 512000);
      if (res.statusCode < 200 || res.statusCode >= 300) {
        lastError = new Error(`搜索服务 HTTP ${res.statusCode}`);
        continue;
      }
      const results = parseDdgResults(res.body);
      if (results.length > 0) return results;
    } catch (error) {
      lastError = error;
    }
  }
  if (lastError) throw lastError;
  return [];
}

// 360 搜索：结果块的 data-mdurl 直接带真实目标 URL（见 web-search-agg.js 解析器
// 注释），能与 Bing 交叉去重。无 Cookie 连续查询实测稳定。
async function so360Search(query) {
  const url = `https://www.so.com/s?q=${encodeURIComponent(query)}`;
  const res = await safeFetch(url, 512000);
  if (res.statusCode < 200 || res.statusCode >= 300) throw new Error(`搜索服务 HTTP ${res.statusCode}`);
  return parseSo360Results(res.body);
}

// 三路引擎并行聚合：限时收集（ENGINE_DEADLINE_MS 内到多少合并多少），每路挂独立
// 熔断器——DDG 在无代理的国内网络不可达，不熔断会让每次搜索都陪跑超时。语义与
// 单引擎时代保持一致：拿到过「HTTP 200 但空结果」就按「暂时无结果」上报（带
// note），全部是网络/HTTP 错误才作为工具错误抛出。
const ENGINE_DEADLINE_MS = 6000;
const BREAKER_OPEN = Symbol('breaker-open');
const engines = [
  { source: 'bing', breaker: createEngineBreaker(), search: bingSearch },
  { source: 'so360', breaker: createEngineBreaker(), search: so360Search },
  { source: 'ddg', breaker: createEngineBreaker(), search: ddgSearch },
];

async function aggregatedSearch(query) {
  const settled = await settleWithin(
    engines.map(({ breaker, search }) => {
      if (!breaker.canRun()) return Promise.reject(BREAKER_OPEN);
      return search(query);
    }),
    ENGINE_DEADLINE_MS,
  );
  const groups = [];
  let sawEmptyPage = false;
  let lastError = null;
  for (const [index, outcome] of settled.entries()) {
    const engine = engines[index];
    if (outcome.status === 'fulfilled') {
      engine.breaker.recordSuccess();
      if (outcome.value.length > 0) groups.push({ source: engine.source, results: outcome.value });
      else sawEmptyPage = true;
      continue;
    }
    if (outcome.status === 'rejected' && outcome.reason === BREAKER_OPEN) continue;
    // timeout（限时到未完成）同样计入熔断：不可达引擎的典型形态就是永远在超时。
    engine.breaker.recordFailure();
    if (outcome.status === 'rejected') lastError = outcome.reason;
    else if (!lastError) lastError = new Error('有引擎超时未返回');
  }
  if (groups.length > 0) {
    return { query, results: aggregateSearchResults(groups, { limit: 10 }) };
  }
  if (lastError && !sawEmptyPage) throw lastError;
  return {
    query,
    results: [],
    note: lastError
      ? `搜索服务暂时没有返回结果（可能被搜索引擎限流；最后一次错误：${lastError.message}）`
      : '搜索服务暂时没有返回结果（可能被搜索引擎限流，稍后重试）',
  };
}

async function bingImageSearch(query) {
  for (const host of ['www.bing.com', 'cn.bing.com']) {
    const url = new URL(`https://${host}/images/search`);
    url.searchParams.set('q', query);
    if (host === 'www.bing.com') url.searchParams.set('setmkt', 'zh-CN');
    try {
      const res = await safeFetch(url.toString(), 512000);
      if (res.statusCode < 200 || res.statusCode >= 300) continue;
      const images = extractBingImageUrls(res.body);
      if (images.length > 0) return images;
    } catch { /* 换下一个前台 */ }
  }
  return [];
}

// 360 图片搜索：与 web_search 的多引擎思路一致，Bing 对中文梗图覆盖一般，
// 360（qhimg 生态）作为第二路；imgkey/原图直链见 web-search-agg.js 解析器注释。
async function so360ImageSearch(query) {
  const url = `https://image.so.com/i?src=imageonebox&q=${encodeURIComponent(query)}`;
  const res = await safeFetch(url, 512000);
  if (res.statusCode < 200 || res.statusCode >= 300) throw new Error(`搜索服务 HTTP ${res.statusCode}`);
  const results = parseSo360ImageResults(res.body);
  if (!results.length) return [];
  return results.map((r) => r.url);
}

// 图片搜索双源聚合：并行 + 6s 限时（不可达/被限流的一路不拖整体），交错合并去重。
// 有一路出结果即可用；全空才带 note（沿用「别把节流说成网上没有」的口径）。
async function aggregatedImageSearch(query) {
  const settled = await settleWithin([bingImageSearch(query), so360ImageSearch(query)], ENGINE_DEADLINE_MS);
  const feeds = settled.map((o) => (o.status === 'fulfilled' ? o.value : []));
  const merged = [];
  const seen = new Set();
  const maxLen = Math.max(...feeds.map((f) => f.length), 0);
  for (let i = 0; i < maxLen; i += 1) {
    for (const feed of feeds) {
      const u = feed[i];
      if (!u || seen.has(u)) continue;
      seen.add(u);
      merged.push(u);
    }
  }
  if (merged.length > 0) return { query, images: merged.slice(0, 16) };
  return { query, images: [], note: '图片搜索暂时没有返回结果（可能被搜索引擎限流，稍后重试；也可以换个更具体的关键词，如「xxx 表情包」）' };
}

// ── 工具定义（name/description/inputSchema 与原 SDK zod 版逐字一致，
// schema 由 zod describe 展开为 JSON Schema description）─────────────────
const TOOL_HANDLERS = {
  web_search: async ({ query }) => {
    const clean = sanitizeQuery(query);
    if (!clean) {
      return { content: [{ type: 'text', text: '查询词为空，已拒绝。' }], isError: true };
    }
    try {
      const result = await aggregatedSearch(clean);
      return { content: [{ type: 'text', text: JSON.stringify(result, null, 2) }] };
    } catch (error) {
      return {
        content: [{ type: 'text', text: `搜索失败：${error?.message ?? error}` }],
        isError: true,
      };
    }
  },
  image_search: async ({ query }) => {
    const clean = sanitizeQuery(query);
    if (!clean) {
      return { content: [{ type: 'text', text: '查询词为空，已拒绝。' }], isError: true };
    }
    try {
      const result = await aggregatedImageSearch(clean);
      return { content: [{ type: 'text', text: JSON.stringify(result, null, 2) }] };
    } catch (error) {
      return {
        content: [{ type: 'text', text: `图片搜索失败：${error?.message ?? error}` }],
        isError: true,
      };
    }
  },
  web_fetch: async ({ url }) => {
    try {
      const result = await safeFetch(url);
      return { content: [{ type: 'text', text: JSON.stringify(result, null, 2) }] };
    } catch (error) {
      return {
        content: [{ type: 'text', text: `抓取失败：${error?.message ?? error}` }],
        isError: true,
      };
    }
  },
};

const TOOLS = [
  {
    name: 'web_search',
    description:
      '只读搜索网络用语/梗/黑话的含义，聚合 Bing、360、DuckDuckGo 多引擎结果（标题/URL/摘要/来源，多引擎都命中的排前）。仅用于理解词义，不执行任何本地操作。结果为空且带 note 字段时表示搜索服务暂时受限（如被限流），应如实说「暂时搜不到」，不要据此断定「网上没有相关内容」。',
    inputSchema: {
      type: 'object',
      properties: { query: { type: 'string', description: '要搜索确认的网络用语/黑话/梗' } },
      required: ['query'],
    },
  },
  {
    name: 'image_search',
    description:
      '只读搜索图片，返回候选图片的 http(s) 直链。主要用于找表情包素材：搜「xxx 表情包」挑一条直链，交给 mcp__snowluma__qq_fetch_sticker 收入 QQ 收藏表情。不执行任何本地操作。',
    inputSchema: {
      type: 'object',
      properties: { query: { type: 'string', description: '图片搜索词，如「猫猫 歪头 表情包」' } },
      required: ['query'],
    },
  },
  {
    name: 'web_fetch',
    description:
      '只读抓取 HTTP(S) 网页正文，返回纯文本/HTML 前 50000 字符。禁止访问内网/本机地址，不执行任何本地操作。',
    inputSchema: {
      type: 'object',
      properties: { url: { type: 'string', description: '要抓取的 http(s) URL' } },
      required: ['url'],
    },
  },
];

// ── 手写 stdio JSON-RPC（newline-delimited；协议形状与 queqiao-mcp 一致）──
// 通知（无 id）不回应；stderr 留给异常日志，绝不污染 stdout 协议流。
const SERVER_INFO = { name: 'web-search-safe', version: '0.3.0-queqiao' };

function reply(id, result) {
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, result }) + '\n');
}

function replyError(id, code, message) {
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, error: { code, message } }) + '\n');
}

async function handleMessage(msg) {
  const { id, method, params } = msg;
  switch (method) {
    case 'initialize':
      reply(id, {
        protocolVersion:
          typeof params?.protocolVersion === 'string' ? params.protocolVersion : '2024-11-05',
        capabilities: { tools: {} },
        serverInfo: SERVER_INFO,
      });
      return;
    case 'tools/list':
      reply(id, { tools: TOOLS });
      return;
    case 'tools/call': {
      const name = params?.name;
      const handler = TOOL_HANDLERS[name];
      if (!handler) {
        replyError(id, -32602, `未知工具：${name}`);
        return;
      }
      const args = params?.arguments ?? {};
      try {
        reply(id, await handler(args));
      } catch (error) {
        replyError(id, -32603, `工具执行异常：${error?.message ?? error}`);
      }
      return;
    }
    case 'ping':
      reply(id, {});
      return;
    default:
      // 有 id 的未知方法按 JSON-RPC 规范报错（帮助发现协议不匹配），
      // 通知（无 id）静默忽略。
      if (id !== undefined) replyError(id, -32601, `未知方法：${method}`);
  }
}

const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
  const text = line.trim();
  if (!text) return;
  let msg;
  try {
    msg = JSON.parse(text);
  } catch {
    return; // 坏行跳过（对端 SDK 也按行容错）
  }
  handleMessage(msg).catch((e) => {
    process.stderr.write(`[web-search-safe] 处理消息异常：${e?.stack ?? e}\n`);
  });
});
rl.on('close', () => process.exit(0));
