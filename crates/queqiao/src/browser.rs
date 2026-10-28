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
//! - **单页纪律**（2026-10-28 起，对标 ego-browser 的标签页治理）：注入
//!   脚本把 target=_blank 链接点击改写为当前页打开（泄漏主源掐在源头），
//!   每次操作前 reconcile——活跃 target 失联即接管别的页、page target 超
//!   [`TAB_BUDGET`] 回收最老的非活跃页；跨源弹窗（OAuth 等）漏网的由
//!   预算回收兜底。AI 侧另有 browser_tabs 工具列出/关闭标签页。
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

    /// 发 WS close 帧（尽力而为）：换页接管后让旧连接的读任务尽快收尾，
    /// 避免每次接管泄漏一个挂着的读任务。
    async fn close(&self) {
        if let Ok(mut sink) = self.sink.try_lock() {
            let _ = sink.send(Message::Close(None)).await;
        }
    }
}

// ── 会话与管理器 ─────────────────────────────────────────────────────────────

struct Session {
    /// 进程生命周期锚：重启/闲置关闭时显式 kill+wait（Chromium 单例机制
    /// 要求旧实例真正退出后才能同 user-data-dir 重启）；丢弃时 kill_on_drop
    /// 兜底。
    child: tokio::process::Child,
    ws: Arc<WsConn>,
    /// DevTools HTTP 端口（/json/list 扫描、/json/close 关页）。
    port: u16,
    /// 活跃页面 target id（reconcile 校验存活/接管用）。
    target_id: String,
    headless: bool,
    last_used: std::time::Instant,
    started_at: u64,
}

/// 标签页预算（含活跃页）：超过即回收最老的非活跃页。3 = 活跃 + 2 个
/// 余量（登录弹窗/漏网 popup 留活口），资讯类浏览单页纪律下常年 1~2。
const TAB_BUDGET: usize = 3;

impl Session {
    /// 标签页治理（每次工具操作前跑）：
    /// 1. 活跃 target 消失（页面被关/崩溃/被站点 window.close）→ 接管
    ///    现存页，没有就新开空白页——替代旧的“丢会话整个重启浏览器”；
    /// 2. page target 超预算 → 关最老的非活跃页（/json/list 大体按创建序）。
    ///
    /// 扫描失败不阻塞（进程死会在随后的 CDP 调用暴露、走原有重建路径）。
    async fn reconcile(&mut self) -> Result<(), String> {
        let pages = match list_pages(self.port).await {
            Ok(p) => p,
            Err(e) => {
                crate::logging::log(&format!("[browser] 标签页扫描失败（忽略）：{e}"));
                return Ok(());
            }
        };
        let active_alive = pages
            .iter()
            .any(|p| p.get("id").and_then(Value::as_str) == Some(self.target_id.as_str()));
        if !active_alive {
            return self.adopt_from(&pages).await;
        }
        let mut excess = pages.len().saturating_sub(TAB_BUDGET);
        if excess == 0 {
            return Ok(());
        }
        let mut closed = 0;
        for p in &pages {
            if excess == 0 {
                break;
            }
            let Some(id) = p.get("id").and_then(Value::as_str) else {
                continue;
            };
            if id == self.target_id {
                continue;
            }
            if close_target(self.port, id).await.is_ok() {
                closed += 1;
                excess -= 1;
            }
        }
        if closed > 0 {
            crate::logging::log(&format!(
                "[browser] 标签页超预算（>{TAB_BUDGET}），已回收 {closed} 个"
            ));
        }
        Ok(())
    }

    /// 接管一个现存页为新活跃页（优先最新非 devtools 页——popup 语义），
    /// 没有候选时新开空白页；全失败才报错（调用方丢弃会话走重拉）。
    async fn adopt_from(&mut self, pages: &[Value]) -> Result<(), String> {
        let target = match pick_replacement_target(pages) {
            Some(t) => t.clone(),
            None => open_blank_tab(self.port).await?,
        };
        let id = target
            .get("id")
            .and_then(Value::as_str)
            .ok_or("标签页信息缺 id")?
            .to_owned();
        let ws_url = target
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .ok_or("标签页信息缺 webSocketDebuggerUrl")?
            .to_owned();
        let ws = attach_page(&ws_url).await?;
        self.ws.close().await;
        self.ws = ws;
        self.target_id = id.clone();
        crate::logging::log(&format!("[browser] 活跃页失联，已接管标签页 {id}"));
        Ok(())
    }
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
    /// 闲置看门狗（主循环启动；每分钟查一次，闲置即关）。关进程时显式
    /// kill+等退出：kill_on_drop 只发 kill 不等，垂死实例会占着 profile
    /// 目录（Windows 句柄），下一次同目录启动被 Chromium 单例机制委派
    /// 后自尽。
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
                    if let Some(mut s) = guard.take() {
                        let _ = s.child.start_kill();
                        let _ =
                            tokio::time::timeout(std::time::Duration::from_secs(5), s.child.wait())
                                .await;
                    } // Drop → kill_on_drop 兜底
                }
            }
        });
    }

    /// 确保会话存在且显示模式匹配（force_headful=登录流程：无视配置以有头
    /// 启动；已在跑的无头会话先关——profile 同目录，重启后登录态保留）。
    /// 换模式重启前先彻底关旧进程并等退出：Chromium 单例机制会把同
    /// user-data-dir 的新 spawn 委派给垂死旧实例、新进程随即自尽。
    async fn ensure(&self, cfg: &BrowserConfig, force_headful: bool) -> Result<(), String> {
        let mut guard = self.session.lock().await;
        let want_headless = cfg.headless && !force_headful;
        if let Some(mut s) = guard.take() {
            if s.headless == want_headless {
                *guard = Some(s);
                return Ok(());
            }
            crate::logging::log(if force_headful {
                "[browser] 切换到有头模式（登录流程）"
            } else {
                "[browser] 按配置切换显示模式，重启浏览器"
            });
            let _ = s.child.start_kill();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), s.child.wait()).await;
        }
        let s = launch(cfg, want_headless).await?;
        *guard = Some(s);
        Ok(())
    }

    /// 在（确保存在的）会话上执行操作；顺带刷新 last_used。操作前先跑
    /// 标签页治理（活跃页接管/超预算回收）；治理致命失败=浏览器大概率已
    /// 坏，丢弃会话走重拉。操作失败同样丢弃会话（进程可能已死）——下一次
    /// 工具调用 ensure() 自动重拉，错误原样返回让模型决定重试
    /// （AsyncFnOnce 只能消费一次，不在本层自动重试）。
    async fn op<T>(
        &self,
        cfg: &BrowserConfig,
        f: impl AsyncFnOnce(&WsConn) -> Result<T, String>,
    ) -> Result<T, String> {
        self.ensure(cfg, false).await?;
        let mut guard = self.session.lock().await;
        let recon = match guard.as_mut() {
            Some(s) => s.reconcile().await,
            None => Err("浏览器会话缺失".to_owned()),
        };
        if let Err(e) = recon {
            *guard = None;
            drop(guard);
            return Err(e);
        }
        let Some(s) = guard.as_ref() else {
            unreachable!("reconcile 成功必有会话");
        };
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
                info["tabBudget"] = json!(TAB_BUDGET);
                if let Ok(pages) = list_pages(s.port).await {
                    info["tabCount"] = json!(pages.len());
                }
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

    /// 标签页清单（browser_tabs action=list）：index/标题/URL/是否活跃。
    pub async fn tabs(&self, cfg: &BrowserConfig) -> Result<Value, String> {
        self.ensure(cfg, false).await?;
        let guard = self.session.lock().await;
        let s = guard.as_ref().ok_or("浏览器会话缺失")?;
        let pages = list_pages(s.port)
            .await
            .map_err(|e| format!("标签页扫描失败：{e}"))?;
        let tabs: Vec<Value> = pages
            .iter()
            .enumerate()
            .map(|(i, p)| {
                json!({
                    "index": i,
                    "title": p.get("title").and_then(Value::as_str).unwrap_or(""),
                    "url": p.get("url").and_then(Value::as_str).unwrap_or(""),
                    "active": p.get("id").and_then(Value::as_str) == Some(s.target_id.as_str()),
                })
            })
            .collect();
        let count = tabs.len();
        Ok(json!({ "tabs": tabs, "count": count, "budget": TAB_BUDGET }))
    }

    /// 关标签页（browser_tabs action=close）：index 给定关那一页（允许是
    /// 活跃页——关完自动接管剩下的页）；不给则关全部非活跃页。最后一页
    /// 不关（保底：无 page target 的浏览器只能整体重启）。
    pub async fn tabs_close(
        &self,
        cfg: &BrowserConfig,
        index: Option<usize>,
    ) -> Result<String, String> {
        self.ensure(cfg, false).await?;
        let mut guard = self.session.lock().await;
        let s = guard.as_mut().ok_or("浏览器会话缺失")?;
        let pages = list_pages(s.port)
            .await
            .map_err(|e| format!("标签页扫描失败：{e}"))?;
        if pages.is_empty() {
            return Err("浏览器没有标签页".into());
        }
        let victims: Vec<String> = match index {
            Some(i) => {
                if pages.len() == 1 {
                    return Err("只剩最后一个标签页，不关（保底页面）".into());
                }
                let id = pages
                    .get(i)
                    .ok_or_else(|| format!("index {i} 超出范围（共 {} 页）", pages.len()))?
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("标签页信息缺 id")?
                    .to_owned();
                vec![id]
            }
            None => pages
                .iter()
                .filter(|p| p.get("id").and_then(Value::as_str) != Some(s.target_id.as_str()))
                .filter_map(|p| p.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect(),
        };
        if victims.is_empty() {
            return Ok("没有可关的标签页（只有当前活跃页）".into());
        }
        let closed_active = victims.iter().any(|v| v == &s.target_id);
        let mut closed = 0;
        for id in &victims {
            if close_target(s.port, id).await.is_ok() {
                closed += 1;
            }
        }
        // /json/close 是异步的（返回 200 只代表“正在关”）：有界轮询等目标
        // 真消失，再按实际存量报数——否则紧跟着的 list 会看到僵尸页。
        let contains_victim = |pages: &[Value]| {
            pages.iter().any(|p| {
                p.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| victims.iter().any(|v| v.as_str() == id))
            })
        };
        let mut remain = list_pages(s.port).await.unwrap_or_default();
        for _ in 0..8 {
            if !contains_victim(&remain) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            remain = list_pages(s.port).await.unwrap_or_default();
        }
        if closed_active && closed > 0 {
            // 活跃页被关：接管剩下的一页（tab 工具不走 op()，手动接管）。
            let remain = list_pages(s.port).await?;
            s.adopt_from(&remain).await?;
        }
        drop(guard);
        if let Some(s) = self.session.lock().await.as_mut() {
            s.last_used = std::time::Instant::now();
        }
        Ok(format!("已关 {closed} 个标签页，剩余 {} 个", remain.len()))
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

/// 单页纪律脚本（attach_page 时 addScriptToEvaluateOnNewDocument 注入每个
/// 新导航 + 当前页立即执行一次；window 守卫幂等）：
/// - 捕获阶段把带 target 的链接点击改写为当前页打开——资讯站文章链接几乎
///   全 target=_blank，这是标签页泄漏的主源，掐在源头；
/// - 同源 window.open 就地导航；跨源弹窗（OAuth 登录等）保持原行为不误伤，
///   漏网的由预算回收兜底。
const SINGLE_TAB_JS: &str = r#"(() => {
  if (window.__qqSingleTab) return 'dup';
  window.__qqSingleTab = 1;
  document.addEventListener('click', (e) => {
    const el = e.target;
    if (!(el instanceof Element)) return;
    const a = el.closest('a');
    if (a && a.target && a.target !== '_self') { try { a.target = '_self'; } catch {} }
  }, true);
  const origOpen = window.open.bind(window);
  window.open = (u, t, f) => {
    try {
      if (typeof u === 'string' && u && new URL(u, location.href).origin === location.origin) {
        location.href = u;
        return window;
      }
    } catch {}
    return origOpen(u, t, f);
  };
  return 'ok';
})()"#;

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
    // 清掉上一次实例可能残留的 DevToolsActivePort：profile 目录未被完全
    // 清理时（Windows 上垂死进程句柄未释放、删目录部分失败），端口发现会
    // 读到已死浏览器的旧端口，后续 /json/list 全部落空（真机踩过）。
    let _ = std::fs::remove_file(profile.join("DevToolsActivePort"));
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

    // 端口发现：轮询 DevToolsActivePort（首行=端口）。子进程提前退出=被
    // Chromium 单例机制委派给残留旧实例（或启动失败），直接报因而不是干等
    // 30s 超时。
    let port_file = profile.join("DevToolsActivePort");
    let mut port = 0u16;
    for _ in 0..150 {
        if let Ok(status) = child.try_wait()
            && status.is_some()
        {
            return Err(
                "浏览器进程启动后立即退出（profile 可能被残留实例占用；重试一次，仍失败则删除 state/browser-profile 后重试）"
                    .into(),
            );
        }
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

    // 页面目标发现（/json/list → 第一个 page），随后 attach（连 WS + 注入）。
    // 窗口放宽到 ~18s：高负载真机见过 >12s 才出第一个 page 的偶发（常路径
    // 0.3s 内命中，宽上限不拖慢正常启动）。
    let mut attempts = 0;
    let first = loop {
        attempts += 1;
        if attempts > 60 {
            return Err("浏览器页面目标发现超时（/json/list 无 page）".into());
        }
        if let Ok(pages) = list_pages(port).await
            && let Some(first) = pages.first().cloned()
        {
            break first;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    };
    let target_id = first
        .get("id")
        .and_then(Value::as_str)
        .ok_or("页面目标缺 id")?
        .to_owned();
    let ws_url = first
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or("页面目标缺 webSocketDebuggerUrl")?
        .to_owned();
    let ws = attach_page(&ws_url).await?;
    crate::logging::log(&format!(
        "[browser] 已启动（{}，{}）",
        exe.display(),
        if headless { "无头" } else { "有头" }
    ));
    Ok(Session {
        child,
        ws,
        port,
        target_id,
        headless,
        last_used: std::time::Instant::now(),
        started_at: crate::session_manager::SessionManager::now_ms(),
    })
}

/// 连上指定页面 target 的 WS 并完成启用 + 单页纪律注入。启动与接管共用。
async fn attach_page(ws_url: &str) -> Result<Arc<WsConn>, String> {
    let (stream, _) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio_tungstenite::connect_async(ws_url),
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
    // 新导航生效（addScriptToEvaluateOnNewDocument 只对未来导航起作用），
    // 当前页再立即执行一次（脚本自带幂等守卫）。
    ws.call(
        "Page.addScriptToEvaluateOnNewDocument",
        json!({ "source": SINGLE_TAB_JS }),
    )
    .await?;
    let _ = ws.evaluate(SINGLE_TAB_JS).await;
    Ok(ws)
}

/// /json/list 的 page 目标（type=page 过滤；大体按创建序，旧→新）。
async fn list_pages(port: u16) -> Result<Vec<Value>, String> {
    let body = http_req_local(port, "GET", "/json/list").await?;
    let list = serde_json::from_str::<Value>(&body).map_err(|e| e.to_string())?;
    Ok(list
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t.get("type") == Some(&json!("page")))
        .collect())
}

/// 接管候选（纯函数）：取最新的非 devtools 页——popup 语义（新开的页
/// 多半是导航去向）；全是 devtools 页时退最新一个。
fn pick_replacement_target(pages: &[Value]) -> Option<&Value> {
    pages
        .iter()
        .rev()
        .find(|p| {
            let url = p.get("url").and_then(Value::as_str).unwrap_or("");
            !url.starts_with("devtools://")
        })
        .or_else(|| pages.last())
}

/// 关指定标签页（DevTools HTTP 端点，与 /json/list 同族）。
async fn close_target(port: u16, target_id: &str) -> Result<(), String> {
    http_req_local(port, "GET", &format!("/json/close/{target_id}"))
        .await
        .map(|_| ())
}

/// 新开空白标签页（Chrome M111+ /json/new 要求 PUT，老版本回落 GET）。
async fn open_blank_tab(port: u16) -> Result<Value, String> {
    let body = match http_req_local(port, "PUT", "/json/new").await {
        Ok(b) => b,
        Err(_) => http_req_local(port, "GET", "/json/new").await?,
    };
    serde_json::from_str::<Value>(&body).map_err(|e| format!("新开标签页响应解析失败：{e}"))
}

/// 本地回环 HTTP 请求（手写，避免 reqwest blocking 嵌 async；/json/list、
/// /json/close、/json/new[PUT] 共用）。按 Content-Length 精确读体——DevTools
/// 端点对 Connection: close 不总关连接，read_to_end 会挂到超时（真机排查：
/// curl 通、裸读 5s 超时）。
async fn http_req_local(port: u16, method: &str, path: &str) -> Result<String, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| e.to_string())?;
    let req =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
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

    /// 单页纪律脚本：捕获点击改写 anchor target + 同源 window.open 就地
    /// 导航 + 幂等守卫；不能误伤跨源弹窗（OAuth 登录依赖）。
    #[test]
    fn single_tab_script_shape() {
        assert!(SINGLE_TAB_JS.contains("__qqSingleTab"), "幂等守卫");
        assert!(
            SINGLE_TAB_JS.contains("addEventListener('click'"),
            "捕获点击改写"
        );
        assert!(
            SINGLE_TAB_JS.contains("a.target = '_self'"),
            "blank 链接改当前页"
        );
        assert!(SINGLE_TAB_JS.contains("window.open"), "window.open 覆盖");
        assert!(
            SINGLE_TAB_JS.contains("origin === location.origin"),
            "只改同源 open，跨源弹窗放行"
        );
    }

    fn page_target(id: &str, url: &str) -> Value {
        json!({ "id": id, "type": "page", "url": url, "webSocketDebuggerUrl": "ws://x" })
    }

    /// 接管候选：优先最新非 devtools 页（popup 语义）；全 devtools 退最新。
    #[test]
    fn replacement_target_picking() {
        let pages = vec![
            page_target("t1", "https://a.example/1"),
            page_target("t2", "devtools://devtools/bundled/inspector.html"),
            page_target("t3", "https://a.example/3"),
        ];
        assert_eq!(
            pick_replacement_target(&pages)
                .and_then(|p| p.get("id"))
                .and_then(Value::as_str),
            Some("t3"),
            "跳过 devtools 取最新普通页"
        );
        let devtools_only = vec![
            page_target("d1", "devtools://a"),
            page_target("d2", "devtools://b"),
        ];
        assert_eq!(
            pick_replacement_target(&devtools_only)
                .and_then(|p| p.get("id"))
                .and_then(Value::as_str),
            Some("d2"),
            "全 devtools 退最新"
        );
        assert!(
            pick_replacement_target(&[]).is_none(),
            "无页可接 → 上层新开空白页"
        );
        // 空白页也是合法候选（target=_blank 泄漏页常停在 about:blank）。
        let blank = vec![page_target("b1", "about:blank")];
        assert!(pick_replacement_target(&blank).is_some());
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
