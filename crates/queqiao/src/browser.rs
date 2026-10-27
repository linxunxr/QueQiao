//! 鹊桥专属浏览器：独立 Chromium/Edge 进程 + 独立 profile + CDP 驱动。
//!
//! 设计要点（与 tablet 模块同一套哲学）：
//! - **零接触用户环境**：独立 user-data-dir（state/browser-profile）、独立
//!   进程、CDP 只绑 127.0.0.1 随机端口（--remote-debugging-port=0 +
//!   DevToolsActivePort 发现）；删 profile 目录 = 登录态彻底清空。
//! - **懒启动 + 闲置关闭**：首次工具调用才拉起，`idle_close_min` 分钟无调用
//!   自动关闭（后台看门狗任务，仅主循环启动；测试环境不启）。
//! - **无头/有头可配**：headless 是启动参数，配置变更下一次生命周期生效；
//!   登录流程（show）无视配置强制有头，登录后按配置继续。
//! - **能力面收敛**：导航/快照（DOM 压缩树）/截图/点击/输入/滚动/返回/
//!   等待——不给任意 JS 执行（Runtime.evaluate 只跑本模块内置脚本）。
//!
//! CDP 客户端：页面级 WebSocket + id 配对（queqiao-dsh mux 同款自研模式，
//! 不引重依赖）。HTTP 发现端点用裸 TCP 手写 GET（本地回环，快且无
//! reqwest blocking 嵌 tokio 坑）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::config::BrowserConfig;

// ── CDP 连接 ────────────────────────────────────────────────────────────────

type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

/// CDP 连接的共享面：写半在 Mutex 里（命令串行发送），响应按 id 经 oneshot
/// 回投（读任务路由整条响应——error 对象在顶层，不在 result 里）。
struct WsConn {
    sink: Mutex<futures_util::stream::SplitSink<WsStream, Message>>,
    pending: StdMutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
}

impl WsConn {
    /// 发一条 CDP 命令并等响应（15s 超时）。
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({ "id": id, "method": method, "params": params });
        let text = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
        {
            let mut sink = self.sink.lock().await;
            sink.send(Message::text(text))
                .await
                .map_err(|e| format!("CDP 发送失败：{e}"))?;
        }
        let resp = tokio::time::timeout(std::time::Duration::from_secs(15), rx)
            .await
            .map_err(|_| format!("CDP 命令超时 15s（{method}）"))?
            .map_err(|_| format!("CDP 响应通道关闭（{method})"))?;
        if let Some(err) = resp.get("error") {
            return Err(format!(
                "CDP 错误（{method}）：{}",
                err.get("message").and_then(Value::as_str).unwrap_or("?")
            ));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Runtime.evaluate（returnByValue + awaitPromise）→ result.value 字符串。
    async fn evaluate(&self, expression: &str) -> Result<String, String> {
        let out = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;
        if let Some(exc) = out.get("exceptionDetails") {
            return Err(format!(
                "页面脚本异常：{}",
                exc.pointer("/exception/description")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            ));
        }
        Ok(out
            .pointer("/result/value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned())
    }

    async fn current_url(&self) -> String {
        self.evaluate("location.href").await.unwrap_or_default()
    }
}

// ── 会话与管理器 ─────────────────────────────────────────────────────────────

struct Session {
    /// 进程句柄仅作生命周期锚（kill_on_drop：Session 丢弃即关浏览器）。
    #[allow(dead_code)]
    child: tokio::process::Child,
    ws: Arc<WsConn>,
    headless: bool,
    last_used: std::time::Instant,
    started_at: u64,
}

/// 浏览器管理器：Arc 共享给端点/看门狗；懒启动。
pub struct Browser {
    session: Mutex<Option<Session>>,
}

pub fn new_browser() -> Arc<Browser> {
    Arc::new(Browser {
        session: Mutex::new(None),
    })
}

impl Browser {
    /// 闲置看门狗（主循环启动；每分钟查一次，闲置即关）。
    pub fn start_idle_watcher(self: &Arc<Self>, idle_close_min: u64) {
        if idle_close_min == 0 {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tick.tick().await;
                let mut guard = this.session.lock().await;
                let idle = guard
                    .as_ref()
                    .is_some_and(|s| s.last_used.elapsed().as_secs() >= idle_close_min * 60);
                if idle {
                    crate::logging::log("[browser] 闲置自动关闭");
                    *guard = None; // Drop → kill_on_drop 关进程
                }
            }
        });
    }

    /// 确保会话存在且显示模式匹配（force_headful=登录流程：无视配置以有头
    /// 启动；已在跑的无头会话先关——profile 同目录，重启后登录态保留）。
    async fn ensure(&self, cfg: &BrowserConfig, force_headful: bool) -> Result<(), String> {
        let mut guard = self.session.lock().await;
        let want_headless = cfg.headless && !force_headful;
        if let Some(s) = guard.as_ref() {
            if s.headless == want_headless {
                return Ok(());
            }
            crate::logging::log(if force_headful {
                "[browser] 切换到有头模式（登录流程）"
            } else {
                "[browser] 按配置切换显示模式，重启浏览器"
            });
            *guard = None;
        }
        let s = launch(cfg, want_headless).await?;
        *guard = Some(s);
        Ok(())
    }

    /// 在（确保存在的）会话上执行操作；顺带刷新 last_used。失败时丢弃会话
    /// （进程可能已死）——下一次工具调用 ensure() 自动重拉，错误原样返回让
    /// 模型决定重试（AsyncFnOnce 只能消费一次，不在本层自动重试）。
    async fn op<T>(
        &self,
        cfg: &BrowserConfig,
        f: impl AsyncFnOnce(&WsConn) -> Result<T, String>,
    ) -> Result<T, String> {
        self.ensure(cfg, false).await?;
        let guard = self.session.lock().await;
        let s = guard.as_ref().ok_or("浏览器会话缺失")?;
        let out = f(&s.ws).await;
        drop(guard);
        if out.is_ok() {
            if let Some(s) = self.session.lock().await.as_mut() {
                s.last_used = std::time::Instant::now();
            }
        } else {
            // 会话大概率已坏（WS 断/进程死）：丢弃，下次调用重新拉起。
            *self.session.lock().await = None;
        }
        out
    }

    // ── 工具动作 ────────────────────────────────────────────────────────────

    /// 只读状态：配置摘要 + 运行态（URL/显示模式/闲置）。
    pub async fn status(&self, cfg: &BrowserConfig) -> Value {
        let mut info = json!({
            "enabled": cfg.enabled,
            "headlessConfig": cfg.headless,
            "allowConvKeys": if cfg.allow_conv_keys.is_empty() {
                json!(["(默认仅管理员私聊)"])
            } else {
                json!(cfg.allow_conv_keys)
            },
        });
        let guard = self.session.lock().await;
        match guard.as_ref() {
            Some(s) => {
                info["running"] = json!(true);
                info["headless"] = json!(s.headless);
                info["url"] = json!(s.ws.current_url().await);
                info["idleSec"] = json!(s.last_used.elapsed().as_secs());
                info["startedAt"] = json!(s.started_at);
            }
            None => {
                info["running"] = json!(false);
                info["hint"] = json!("未启动（首次工具调用自动拉起）");
            }
        }
        info
    }

    /// 导航到 URL（等 load 完成再返回）。
    pub async fn navigate(&self, cfg: &BrowserConfig, url: &str) -> Result<String, String> {
        let url = url.trim().to_owned();
        if !url.starts_with("http://")
            && !url.starts_with("https://")
            && !url.starts_with("file://")
        {
            return Err(format!("仅允许 http/https/file 网址（收到 {url}）"));
        }
        self.op(cfg, async move |ws| {
            ws.call("Page.navigate", json!({ "url": url })).await?;
            wait_ready(ws, 15).await?;
            Ok(format!("已打开：{}", ws.current_url().await))
        })
        .await
    }

    /// DOM 快照（压缩树：每行坐标/文字/可点，坐标供 click 用）。
    pub async fn snapshot(&self, cfg: &BrowserConfig, max_chars: usize) -> Result<String, String> {
        self.op(cfg, async move |ws| {
            ws.evaluate(&snapshot_js(max_chars)).await
        })
        .await
    }

    /// 截图（JPEG base64）。
    pub async fn screenshot(&self, cfg: &BrowserConfig) -> Result<String, String> {
        let max_bytes = cfg.max_screenshot_bytes;
        self.op(cfg, async move |ws| {
            let out = ws
                .call(
                    "Page.captureScreenshot",
                    json!({ "format": "jpeg", "quality": 70 }),
                )
                .await?;
            let data = out
                .get("data")
                .and_then(Value::as_str)
                .ok_or("截图响应缺 data")?
                .to_owned();
            // base64 长度≈4/3 字节。
            if data.len() as u64 * 3 / 4 > max_bytes {
                return Err(format!(
                    "截图约 {:.1}MB 超过上限（browser.maxScreenshotBytes={max_bytes}）",
                    data.len() as f64 * 3.0 / 4.0 / 1048576.0
                ));
            }
            Ok(json!({ "image": { "mimeType": "image/jpeg", "data": data } }).to_string())
        })
        .await
    }

    /// 点击视口坐标。
    pub async fn click(&self, cfg: &BrowserConfig, x: i64, y: i64) -> Result<String, String> {
        self.op(cfg, async move |ws| {
            for t in ["mousePressed", "mouseReleased"] {
                ws.call(
                    "Input.dispatchMouseEvent",
                    json!({ "type": t, "x": x, "y": y, "button": "left", "clickCount": 1 }),
                )
                .await?;
            }
            Ok(format!("已点击 ({x},{y})"))
        })
        .await
    }

    /// 输入文本（可选先点击聚焦）。
    pub async fn input_text(
        &self,
        cfg: &BrowserConfig,
        text: &str,
        x: Option<i64>,
        y: Option<i64>,
    ) -> Result<String, String> {
        let text: String = text.chars().take(2000).collect();
        self.op(cfg, async move |ws| {
            if let (Some(x), Some(y)) = (x, y) {
                for t in ["mousePressed", "mouseReleased"] {
                    ws.call(
                        "Input.dispatchMouseEvent",
                        json!({ "type": t, "x": x, "y": y, "button": "left", "clickCount": 1 }),
                    )
                    .await?;
                }
            }
            ws.call("Input.insertText", json!({ "text": text })).await?;
            Ok("已输入文本".to_owned())
        })
        .await
    }

    /// 滚动（up=向下看内容，down=回滚）。
    pub async fn scroll(&self, cfg: &BrowserConfig, direction: &str) -> Result<String, String> {
        let (dx, dy) = match direction {
            "up" => (0, -600),
            "down" => (0, 600),
            "left" => (-600, 0),
            "right" => (600, 0),
            _ => return Err(format!("无效方向：{direction}（up/down/left/right）")),
        };
        self.op(cfg, async move |ws| {
            ws.call(
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseWheel", "x": 690, "y": 450, "deltaX": dx, "deltaY": dy }),
            )
            .await?;
            Ok(format!("已向 {direction} 滚动"))
        })
        .await
    }

    /// 浏览器后退。
    pub async fn back(&self, cfg: &BrowserConfig) -> Result<String, String> {
        self.op(cfg, async move |ws| {
            let hist = ws.call("Page.getNavigationHistory", json!({})).await?;
            let idx = hist
                .get("currentIndex")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if idx <= 0 {
                return Err("已在最早一页，无法后退".into());
            }
            let prev = hist
                .pointer("/entries")
                .and_then(Value::as_array)
                .and_then(|e| e.get((idx - 1) as usize))
                .and_then(|e| e.get("id"))
                .and_then(Value::as_i64)
                .ok_or("历史条目缺失")?;
            ws.call("Page.navigateToHistoryEntry", json!({ "entryId": prev }))
                .await?;
            wait_ready(ws, 10).await?;
            Ok("已后退".to_owned())
        })
        .await
    }

    /// 等页面加载稳定（navigate 已内置；独立等待用于慢页面/登录跳转）。
    pub async fn wait_load(&self, cfg: &BrowserConfig, timeout_s: u64) -> Result<String, String> {
        let timeout_s = timeout_s.clamp(1, 60);
        self.op(cfg, async move |ws| {
            wait_ready(ws, timeout_s).await?;
            Ok(format!("页面已加载：{}", ws.current_url().await))
        })
        .await
    }

    /// 登录流程：强制有头窗口（可选直接导航到登录页）。
    pub async fn show(&self, cfg: &BrowserConfig, url: Option<&str>) -> Result<String, String> {
        self.ensure(cfg, true).await?;
        let guard = self.session.lock().await;
        let s = guard.as_ref().ok_or("浏览器会话缺失")?;
        if let Some(url) = url
            && (url.starts_with("http://") || url.starts_with("https://"))
        {
            s.ws.call("Page.navigate", json!({ "url": url })).await?;
            let _ = wait_ready(&s.ws, 10).await;
        }
        let _ = s.ws.call("Page.bringToFront", json!({})).await;
        Ok("浏览器窗口已打开（有头模式）。请在窗口中完成登录/操作；登录后 cookie 自动保留在专属 profile，之后可切回无头。".to_owned())
    }
}

/// 等 document.readyState=complete（轮询；超时不报错——SPA 常驻 loading）。
async fn wait_ready(ws: &WsConn, timeout_s: u64) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_s.max(1));
    loop {
        let state = ws.evaluate("document.readyState").await.unwrap_or_default();
        if state == "complete" {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await; // 稳定
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Ok(()); // 超时按已加载处理（快照自会反映现状）
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

// ── 快照脚本 ────────────────────────────────────────────────────────────────

/// DOM 压缩树脚本（browser_snapshot 内置专用——不给模型任意 JS 的边界内，
/// 这是唯一注入的脚本）：每行 `[x1,y1,x2,y2]<标志> text=…`，坐标与
/// Input.dispatchMouseEvent 的视口坐标系一致，可直接喂给 click。
fn snapshot_js(max_chars: usize) -> String {
    format!(
        r#"(() => {{
  const MAX = {max};
  const lines = [];
  let truncated = false;
  const clip = (s, n) => [...String(s)].slice(0, n).join('');
  const walk = (el, depth) => {{
    if (lines.join('\n').length > MAX) {{ truncated = true; return; }}
    const r = el.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) return;
    const st = getComputedStyle(el);
    if (st.display === 'none' || st.visibility === 'hidden') return;
    // 只取自有文本节点（子元素的文字由它们自己出，防重复）。
    let own = '';
    for (const n of el.childNodes) if (n.nodeType === 3) own += n.textContent;
    own = own.replace(/\s+/g, ' ').trim();
    const interactive = el.closest('a,button,input,textarea,select,[role=button],[onclick]') === el
      || st.cursor === 'pointer';
    const flags = [interactive ? '可点' : '', el.isContentEditable ? '可编' : ''].filter(Boolean).join(',');
    let bits = [];
    if (own) bits.push('text="' + clip(own, 120) + '"');
    if (el.id) bits.push('id=' + el.id);
    if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {{
      bits.push('value="' + clip(el.value || '', 80) + '"');
      bits.push('placeholder="' + clip(el.placeholder || '', 40) + '"');
    }}
    if (el.tagName === 'IMG' && el.alt) bits.push('alt="' + clip(el.alt, 60) + '"');
    if (bits.length || flags) {{
      lines.push('  '.repeat(depth) + '[' + [r.x, r.y, r.x + r.width, r.y + r.height].map(v => Math.round(v)).join(',') + ']' + (flags ? ' <' + flags + '>' : '') + ' ' + bits.join(' '));
    }}
    for (const c of el.children) walk(c, depth + 1);
  }};
  walk(document.body, 0);
  let out = lines.join('\n');
  if (out.length > MAX) {{ out = out.slice(0, MAX); truncated = true; }}
  if (truncated) out += '\n…（已截断：元素过多，可缩小关注范围）';
  return out || '（页面无可读元素——可能是 canvas/视频页面，用 browser_screenshot 看）';
}})()"#,
        max = max_chars
    )
}

// ── 启动与发现 ───────────────────────────────────────────────────────────────

fn browser_exe(cfg: &BrowserConfig) -> Result<PathBuf, String> {
    let configured = cfg.executable_path.trim();
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        if p.exists() {
            return Ok(p);
        }
        return Err(format!(
            "browser.executablePath 指向的文件不存在：{configured}"
        ));
    }
    let bundled = crate::paths::queqiao_home()
        .join("browser")
        .join("chrome-win")
        .join("chrome.exe");
    if bundled.exists() {
        return Ok(bundled);
    }
    // 过渡兜底（专属 Chromium 资产发布前的开发/验证路径；生产以 browser/
    // 目录为准）：系统 Chrome 优先（标准 Chromium 行为）；Edge 最后——实测
    // Edge 无头在程序化 spawn 下有怪癖（Rust/PS 启动卡在 DevTools 初始化前、
    // 带 URL 报 Multiple targets 拒启，终端手工启动正常）。
    for cand in [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    ] {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Ok(p);
        }
    }
    Err("未找到浏览器：browser/ 目录缺 chrome.exe（专属 Chromium 首次启用时自动下载），也未配置 browser.executablePath，且系统无 Edge".into())
}

async fn launch(cfg: &BrowserConfig, headless: bool) -> Result<Session, String> {
    let exe = browser_exe(cfg)?;
    let profile = crate::paths::queqiao_home()
        .join("state")
        .join("browser-profile");
    std::fs::create_dir_all(&profile).map_err(|e| format!("建浏览器 profile 失败：{e}"))?;
    let mut cmd = tokio::process::Command::new(&exe);
    // --user-data-dir 必须等号连写：两参数分离形式在部分环境的程序化 spawn
    // 下会让 Chromium 系浏览器静默卡死（进程活着但 profile 不初始化、
    // DevToolsActivePort 永不出现——bash 终端启动却正常；2026-10-26 真机
    // 排查定位，等号形式全平台稳）。
    cmd.arg(format!("--user-data-dir={}", profile.display()))
        .arg("--remote-debugging-port=0") // 随机端口，DevToolsActivePort 报实际值
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-session-crashed-bubble")
        .arg("--window-size=1380,900")
        .arg("about:blank");
    // about:blank 保证 CDP 有一个 page target（无 URL 时 headless 可能一个
    // page 都不开）。注意：Edge 无头对位置参数报 Multiple targets 拒启——
    // Edge 只是最后兜底（见 browser_exe 注释），主路径 Chrome/专属 Chromium
    // 无此行为。
    if headless {
        cmd.arg("--headless=new");
    }
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW——仅进程句柄，不影响浏览器自身窗口
    cmd.kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("浏览器启动失败（{}）：{e}", exe.display()))?;

    // 端口发现：轮询 DevToolsActivePort（首行=端口）。
    let port_file = profile.join("DevToolsActivePort");
    let mut port = 0u16;
    for _ in 0..150 {
        if let Ok(text) = std::fs::read_to_string(&port_file)
            && let Some(first) = text.lines().next()
            && let Ok(p) = first.trim().parse::<u16>()
        {
            port = p;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    if port == 0 {
        let listing = std::fs::read_dir(&profile)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let child_alive = child.try_wait().map(|w| w.is_none()).unwrap_or(false);
        return Err(format!(
            "浏览器调试端口发现超时（DevToolsActivePort 未出现；进程存活={child_alive}；profile={}；内容：{listing}）",
            profile.display()
        ));
    }

    // 页面目标 WS 地址（/json/list → 第一个 page）。
    let mut attempts = 0;
    let ws_url = loop {
        attempts += 1;
        if attempts > 40 {
            return Err("浏览器页面目标发现超时（/json/list 无 page）".into());
        }
        if let Ok(body) = http_get_local(port, "/json/list").await
            && let Ok(list) = serde_json::from_str::<Value>(&body)
            && let Some(url) = list
                .as_array()
                .and_then(|a| a.iter().find(|t| t.get("type") == Some(&json!("page"))))
                .and_then(|t| t.get("webSocketDebuggerUrl"))
                .and_then(Value::as_str)
        {
            break url.to_owned();
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    };

    let (stream, _) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio_tungstenite::connect_async(ws_url.as_str()),
    )
    .await
    .map_err(|_| "CDP WebSocket 连接超时 10s".to_owned())?
    .map_err(|e| format!("CDP WebSocket 连接失败：{e}"))?;
    let (sink, mut rx) = stream.split();
    let ws = Arc::new(WsConn {
        sink: Mutex::new(sink),
        pending: StdMutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
    });
    // 读任务：按 id 回投整条响应（error 在顶层）；事件忽略（等待走轮询）。
    {
        let ws = ws.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = rx.next().await {
                let Ok(text) = msg.into_text() else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(id) = v.get("id").and_then(Value::as_u64)
                    && let Some(tx) = ws.pending.lock().unwrap().remove(&id)
                {
                    let _ = tx.send(v);
                }
            }
        });
    }
    ws.call("Page.enable", json!({})).await?;
    ws.call("Runtime.enable", json!({})).await?;
    crate::logging::log(&format!(
        "[browser] 已启动（{}，{}）",
        exe.display(),
        if headless { "无头" } else { "有头" }
    ));
    Ok(Session {
        child,
        ws,
        headless,
        last_used: std::time::Instant::now(),
        started_at: crate::session_manager::SessionManager::now_ms(),
    })
}

/// 本地回环 HTTP GET（手写，避免 reqwest blocking 嵌 async）。按
/// Content-Length 精确读体——DevTools 端点对 Connection: close 不总关连接，
/// read_to_end 会挂到超时（真机排查：curl 通、裸读 5s 超时）。
async fn http_get_local(port: u16, path: &str) -> Result<String, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| e.to_string())?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    // 先读出响应头（读到 \r\n\r\n 为止，逐字节足够——头很小）。
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if std::time::Instant::now() >= deadline {
            return Err("本地 HTTP 响应头超时 5s".into());
        }
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut byte))
            .await
            .map_err(|_| "本地 HTTP 读取超时".to_owned())?
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head_text = String::from_utf8_lossy(&head).into_owned();
    let content_length: usize = head_text
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("content-length") {
                v.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_exact(&mut body),
        )
        .await
        .map_err(|_| "本地 HTTP 读体超时 5s".to_owned())?
        .map_err(|e| e.to_string())?;
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_script_shape() {
        let js = snapshot_js(5000);
        assert!(js.contains("MAX = 5000"));
        assert!(js.contains("getBoundingClientRect"), "坐标取视口系");
        assert!(js.contains("可点"));
        assert!(!js.contains("fetch("), "快照脚本只读 DOM，无网络/执行面");
    }
}

// ── 账号绑定（站点注册表 + bind/bind_status/unbind）─────────────────────────

/// 绑定站点注册表条目：加站点 = 加一条配置（域名/登录页/会话 cookie 键/
/// 登录按钮点击脚本/昵称刮取脚本），不写新代码。
pub struct SiteDef {
    pub id: &'static str,
    pub name: &'static str,
    /// 个人主页/设置页 URL（客户端「个人主页」按钮直达）。
    pub profile_url: &'static str,
    /// CDP Storage.clearDataForOrigin 用的 origin（协议+域名）。
    pub origin: &'static str,
    /// 登录落地页（小黑盒无独立登录页——首页 + 自动点开登录弹窗）。
    pub login_url: &'static str,
    /// 打开登录页后注入的内置脚本（点开登录弹窗；不给模型任意 JS 的边界
    /// 内，这是注册表里的静态脚本）。
    pub login_click_js: &'static str,
    /// 任一存在且非空即视为已登录的会话 cookie 名（pkey 是小黑盒 Web/App
    /// 共用的会话凭据，xhh_auto 抓包同源）。
    pub session_cookie_any: &'static [&'static str],
    /// 登录后刮昵称的内置脚本（localStorage 常见用户缓存 + DOM 兜底；
    /// 刮不到返回空——不阻塞登录态判定）。
    pub nickname_js: &'static str,
}

pub const SITES: &[SiteDef] = &[SiteDef {
    id: "xiaoheihe",
    name: "小黑盒",
    profile_url: "https://www.xiaoheihe.cn/user",
    origin: "https://www.xiaoheihe.cn",
    login_url: "https://www.xiaoheihe.cn",
    login_click_js: r"(() => { document.querySelector('.login-btn')?.click(); return 'ok'; })()",
    session_cookie_any: &["pkey", "heybox_uid"],
    nickname_js: r#"(() => {
  const out = {};
  // 1) localStorage 用户缓存（SPA 常见）。
  for (const k of Object.keys(localStorage)) {
    if (!/user|account|profile|self|heybox/i.test(k)) continue;
    try {
      const v = JSON.parse(localStorage[k]);
      const o = v.user || v.data || v;
      if (o.nickname || o.nick_name) out.nickname = o.nickname || o.nick_name;
      if (o.username) out.nickname = out.nickname || o.username;
      if (o.avatar || o.avatar_url || o.head_img) out.avatar = o.avatar || o.avatar_url || o.head_img;
      if (o.level || o.lv) out.level = String(o.level || o.lv);
    } catch {}
  }
  // 2) DOM 兜底：头像 img（alt="XXX头像" / src=URL）+ 用户名元素。
  if (!out.nickname || !out.avatar) {
    const av = document.querySelector('img[class*="avatar"],[class*="avatar"] img,[alt*="头像"]');
    if (av) {
      if (!out.nickname && av.alt) out.nickname = av.alt.replace(/头像$/,'').trim();
      if (!out.avatar && av.src && av.src.startsWith('http')) out.avatar = av.src;
    }
  }
  if (!out.nickname) {
    const el = document.querySelector('[class*="user"],[class*="nick"]');
    if (el && el.textContent && el.textContent.trim() && el.textContent.trim().length < 30) {
      out.nickname = el.textContent.trim();
    }
  }
  return JSON.stringify(out);
})()"#,
}];

pub fn find_site(id: &str) -> Option<&'static SiteDef> {
    SITES.iter().find(|s| s.id == id)
}

/// 登录态判定（纯函数）：任一会话 cookie 名存在且值非空。
pub fn cookies_indicate_login(cookies: &[(String, String)], keys: &[&str]) -> bool {
    cookies
        .iter()
        .any(|(name, value)| !value.is_empty() && keys.iter().any(|k| k.eq_ignore_ascii_case(name)))
}

impl Browser {
    /// 绑定流程：强制有头 → 登录落地页 → 点开登录弹窗 → 窗口前置。
    pub async fn bind(&self, cfg: &BrowserConfig, site: &SiteDef) -> Result<String, String> {
        self.ensure(cfg, true).await?;
        let guard = self.session.lock().await;
        let s = guard.as_ref().ok_or("浏览器会话缺失")?;
        s.ws.call("Page.navigate", json!({ "url": site.login_url }))
            .await?;
        let _ = wait_ready(&s.ws, 15).await;
        let _ = s.ws.evaluate(site.login_click_js).await;
        let _ = s.ws.call("Page.bringToFront", json!({})).await;
        drop(guard);
        if let Some(s) = self.session.lock().await.as_mut() {
            s.last_used = std::time::Instant::now();
        }
        Ok(format!(
            "已打开 {} 登录窗口（有头模式），请在窗口中完成登录（扫码/验证码）；登录成功后此处状态自动变绿",
            site.name
        ))
    }

    /// 登录态查询（不启动浏览器——浏览器没跑时返回 running:false；轮询场景
    /// 浏览器必然在跑，空转时不拉起进程）。scrape=true 时已登录则导航到
    /// 站点刮一次昵称（仅在绑定确认时调一次，轮询别带）。
    pub async fn bind_status(&self, _cfg: &BrowserConfig, site: &SiteDef, scrape: bool) -> Value {
        let mut out = json!({ "site": site.id, "name": site.name });
        let running = self.session.lock().await.is_some();
        out["running"] = json!(running);
        if !running {
            out["loggedIn"] = json!(false);
            out["hint"] = json!("浏览器未运行——点「绑定/登录」拉起登录窗口");
            return out;
        }
        let guard = self.session.lock().await;
        let Some(s) = guard.as_ref() else {
            out["loggedIn"] = json!(false);
            return out;
        };
        let cookies = match s.ws.call("Storage.getCookies", json!({})).await {
            Ok(v) => v
                .get("cookies")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| {
                            Some((
                                c.get("name")?.as_str()?.to_owned(),
                                c.get("value")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                            ))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            Err(e) => {
                out["error"] = json!(e);
                Vec::new()
            }
        };
        drop(guard);
        let logged_in = cookies_indicate_login(&cookies, site.session_cookie_any);
        out["loggedIn"] = json!(logged_in);
        if logged_in
            && scrape
            && let Some(s) = self.session.lock().await.as_ref()
            && let Ok(url) = s.ws.evaluate("location.href").await
        {
            let on_site = url.starts_with(site.origin);
            if !on_site {
                let _ =
                    s.ws.call("Page.navigate", json!({ "url": site.origin }))
                        .await;
                let _ = wait_ready(&s.ws, 10).await;
            }
            if let Ok(profile_json) = s.ws.evaluate(site.nickname_js).await
                && !profile_json.is_empty()
            {
                // nickname_js 返回 JSON {nickname, avatar, level}；兼容旧字符串。
                match serde_json::from_str::<Value>(&profile_json) {
                    Ok(v) => {
                        if let Some(n) = v.get("nickname").and_then(Value::as_str)
                            && !n.is_empty()
                        {
                            out["nickname"] = json!(n);
                        }
                        if let Some(a) = v.get("avatar").and_then(Value::as_str)
                            && !a.is_empty()
                        {
                            out["avatar"] = json!(a);
                        }
                        if let Some(l) = v.get("level").and_then(Value::as_str) {
                            out["level"] = json!(l);
                        }
                    }
                    Err(_) => {
                        // 兼容：脚本返回纯字符串（旧格式）。
                        if !profile_json.contains("{") {
                            out["nickname"] = json!(profile_json);
                        }
                    }
                }
            }
        } else {
            out["hint"] = json!("未检测到会话 cookie——未登录或已失效，点「绑定/登录」重新扫码");
        }
        out
    }

    /// 解绑：按 origin 清 cookie + localStorage（不碰 profile 里其他站点）。
    /// 浏览器没跑时以当前显示模式拉起（一次性操作，可接受）。
    pub async fn unbind(&self, cfg: &BrowserConfig, site: &SiteDef) -> Result<String, String> {
        self.ensure(cfg, false).await?;
        let guard = self.session.lock().await;
        let s = guard.as_ref().ok_or("浏览器会话缺失")?;
        s.ws.call(
            "Storage.clearDataForOrigin",
            json!({ "origin": site.origin, "storageTypes": "cookies,local_storage" }),
        )
        .await?;
        drop(guard);
        if let Some(s) = self.session.lock().await.as_mut() {
            s.last_used = std::time::Instant::now();
        }
        Ok(format!(
            "已解绑 {}（该站 cookie 与本地存储已清，其他站点登录态不受影响）",
            site.name
        ))
    }
}

#[cfg(test)]
mod bind_tests {
    use super::*;

    #[test]
    fn site_registry_and_lookup() {
        let site = find_site("xiaoheihe").expect("注册表有小黑盒");
        assert_eq!(site.origin, "https://www.xiaoheihe.cn");
        assert!(site.session_cookie_any.contains(&"pkey"));
        assert!(site.login_click_js.contains("login-btn"));
        assert!(site.nickname_js.contains("localStorage"));
        assert!(find_site("nonexistent").is_none());
    }

    /// 登录态判定：会话 cookie 存在且非空才算；空值/其他 cookie 不算。
    #[test]
    fn login_cookie_detection() {
        let keys = &["pkey", "heybox_uid"];
        assert!(cookies_indicate_login(
            &[("pkey".into(), "abc".into())],
            keys
        ));
        assert!(
            cookies_indicate_login(
                &[
                    ("other".into(), "x".into()),
                    ("HeyBox_Uid".into(), "42".into())
                ],
                keys
            ),
            "大小写不敏感"
        );
        assert!(
            !cookies_indicate_login(&[("pkey".into(), String::new())], keys),
            "空值不算"
        );
        assert!(!cookies_indicate_login(
            &[("session".into(), "x".into())],
            keys
        ));
        assert!(!cookies_indicate_login(&[], keys));
    }
}
