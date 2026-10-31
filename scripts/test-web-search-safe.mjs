// web-search-safe server 冒烟（stdio 握手 + tools/list + tools/call 三态：
// 真网络搜索 / SSRF 拒绝 / 未知工具）。用法：node scripts/test-web-search-safe.mjs
import { spawn } from 'node:child_process';

const SERVER = new URL('../mcp/web-search-safe/server.js', import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1');

const child = spawn(process.execPath, [SERVER], { stdio: ['pipe', 'pipe', 'inherit'] });
const pending = new Map();
let buf = '';
child.stdout.on('data', (d) => {
  buf += d.toString();
  let idx;
  while ((idx = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, idx).trim();
    buf = buf.slice(idx + 1);
    if (!line) continue;
    const msg = JSON.parse(line);
    if (msg.id !== undefined && pending.has(msg.id)) {
      pending.get(msg.id)(msg);
      pending.delete(msg.id);
    }
  }
});

function rpc(id, method, params) {
  return new Promise((resolve) => {
    pending.set(id, resolve);
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
}

let pass = 0, fail = 0;
function check(name, cond, extra) {
  if (cond) { pass++; console.log(`  ok  ${name}`); }
  else { fail++; console.log(`  FAIL ${name}${extra ? ' — ' + JSON.stringify(extra).slice(0, 200) : ''}`); }
}

// ① initialize 握手（回显客户端协议版本）
let r = await rpc(1, 'initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'smoke', version: '0' } });
check('initialize 回显 protocolVersion', r.result?.protocolVersion === '2025-06-18', r);
check('initialize serverInfo', r.result?.serverInfo?.name === 'web-search-safe', r);
// initialized 通知（无 id）不应有响应——发出后立刻请求下一 id，确认无串扰。
child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n');

// ② tools/list：三工具 + schema 形状
r = await rpc(2, 'tools/list', {});
const names = (r.result?.tools ?? []).map((t) => t.name).sort();
check('tools/list 三工具', JSON.stringify(names) === '["image_search","web_fetch","web_search"]', names);
const ws = r.result?.tools?.find((t) => t.name === 'web_search');
check('web_search schema', ws?.inputSchema?.required?.[0] === 'query' && ws?.inputSchema?.properties?.query?.type === 'string', ws?.inputSchema);

// ③ 真网络搜索（多引擎聚合，6s 限时 + 兜底链最坏 ~20s，给足 40s）
r = await rpc(3, 'tools/call', { name: 'web_search', arguments: { query: '绝绝子 是什么意思' } });
const searchText = r.result?.content?.[0]?.text ?? '';
let searchResult = null;
try { searchResult = JSON.parse(searchText); } catch { /* 保持 null */ }
check('web_search 返回结果', Array.isArray(searchResult?.results) && searchResult.results.length > 0, searchText.slice(0, 150));
check('web_search 非错误', r.result?.isError !== true, r);

// ④ SSRF 回归：本机地址必须拒（isError，不是把响应体交出去）
r = await rpc(4, 'tools/call', { name: 'web_fetch', arguments: { url: 'http://127.0.0.1:3100/api/config' } });
check('web_fetch 拒本机地址', r.result?.isError === true, r.result?.content?.[0]?.text?.slice(0, 100));

// ⑤ 未知工具报 -32602；ping 应答
r = await rpc(5, 'tools/call', { name: 'no_such_tool', arguments: {} });
check('未知工具 -32602', r.error?.code === -32602, r);
r = await rpc(6, 'ping', {});
check('ping 应答', r.result && !r.error, r);

child.kill();
console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
