//! DSH HTTP 客户端与鉴权状态机（移植自 Node 版 `src/dsh-client.js` 的
//! `NodeApiClient`，契约同步迁移）。
//!
//! 鉴权优先级（实战教训结晶，全部保留）：
//! 1. 默认**铸造优先**——离线 HMAC 不依赖「启动器有没有记 stdout 日志」，也不随
//!    DSH 重启失效，是唯一不需要每次升级重新适配的路。两个例外：用户显式配置了
//!    `dsh.authToken`（明确意图），或铸造刚被 401（`prefer_token`）。
//! 2. `_prefer_token` **只在真的收到 401 时**置位/复位：`invalidate_auth` 也会被
//!    remote.mux 流结束（WS 抖动 / DSH 重启）调用，那是常态事件——拿它当「铸造
//!    无效」的证据会把客户端钉死在日志里的过期 token 上，此后所有 RPC 401、
//!    QQ 消息静默堆积。
//! 3. launch token 是进程启动凭据，交换必须放进 URL 查询串，因此**只允许回环**
//!    （`allow_remote` 显式 opt-in 才放行远程）。
//! 4. 铸造的 Cookie 到期前主动重铸（纯本地 HMAC 无网络往返，比撞 401 再重试便宜）。
//!
//! 分层设计：HTTP 传输抽象为 [`Transport`] trait 注入——状态机可离线全量单测
//! （Node 版 fetch mock 测试的同构形态），生产实现见 [`ReqwestTransport`]。
//! Node 版的并发语义（cookiePromise 共享换票、authEpoch 防陈旧交换）在同步模型
//! 下不存在竞态，由类型与调用顺序排除。

use serde_json::{Value, json};
use url::Url;

use crate::auth::{
    MINTED_COOKIE_LIFETIME_MS, MINTED_COOKIE_REFRESH_MARGIN_MS, is_loopback_base,
    mint_browser_session_cookie,
};
use crate::envelope::RpcRequest;
use crate::inbox::{InboxError, inbox_item_ids, inbox_items_of_projections};

/// 出站 HTTP 请求的最小形状。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

/// 入站 HTTP 响应的最小形状。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    /// 大小写不敏感取首个命中的响应头。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// 传输错误：网络/超时（IO），不含 HTTP 状态语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError(pub String);

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TransportError {}

/// HTTP 传输抽象（测试注入内存实现，生产 [`ReqwestTransport`]）。
pub trait Transport {
    fn request(&mut self, req: HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// 上次生效的鉴权策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStrategy {
    /// 离线铸造（本地 HMAC）。
    Minted,
    /// launch token 换票。
    Token,
}

/// 鉴权材料来源（Node 版的文件/日志 IO 注入化）。
pub struct AuthSources {
    /// launch token（config 显式或日志发现；空串表示没有）。
    pub launch_token: String,
    /// 用户是否在 config 显式填了 token（明确意图，优先于铸造）。
    pub token_explicit: bool,
    /// 持久化签名密钥的惰性提供者：invalidate 后会重探（DSH 可能刚重建凭据文件）。
    pub secret: Box<dyn FnMut() -> Option<[u8; 32]> + Send>,
    /// invalidate 时重扫 launch token（日志发现）；无则保持不变。
    pub rediscover_token: Option<Box<dyn FnMut() -> String + Send>>,
}

impl AuthSources {
    /// 测试/无密钥环境：只有显式 token，无密钥、无重扫。
    pub fn token_only(launch_token: &str, token_explicit: bool) -> Self {
        Self {
            launch_token: launch_token.to_owned(),
            token_explicit,
            secret: Box::new(|| None),
            rediscover_token: None,
        }
    }
}

impl std::fmt::Debug for AuthSources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSources")
            .field("launch_token", &self.launch_token)
            .field("token_explicit", &self.token_explicit)
            .finish_non_exhaustive()
    }
}

/// 客户端错误。
#[derive(Debug, Clone, PartialEq)]
pub enum ClientError {
    Transport(TransportError),
    /// token 交换失败（HTTP 无 set-cookie 等）——上层会解除 prefer_token 重试。
    ExchangeFailed(String),
    /// 无任何可用凭据（文案与 Node 版一致，指引用户配置）。
    NoAuth(String),
    /// 拒绝把 launch token 发往非回环地址（安全闸门）。
    RemoteRefused(String),
    /// 响应信封不符。
    InvalidResponse(String),
    /// 业务错误（unwrap 抛出）。
    Rpc {
        label: String,
        code: String,
        message: String,
    },
    /// 投影解析失败。
    Inbox(InboxError),
    /// 铸造失败（baseUrl 无 authority）。
    Mint(String),
    BadJson(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Transport(e) => write!(f, "{e}"),
            ClientError::ExchangeFailed(status) => {
                write!(f, "DSH token exchange failed: HTTP {status}")
            }
            ClientError::NoAuth(msg) => write!(f, "{msg}"),
            ClientError::RemoteRefused(msg) => write!(f, "{msg}"),
            ClientError::InvalidResponse(msg) => write!(f, "{msg}"),
            ClientError::Rpc {
                label,
                code,
                message,
            } => {
                write!(f, "{label} failed: {code}: {message}")
            }
            ClientError::Inbox(e) => write!(f, "{e}"),
            ClientError::Mint(e) => write!(f, "{e}"),
            ClientError::BadJson(e) => write!(f, "invalid JSON from DSH: {e}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<TransportError> for ClientError {
    fn from(e: TransportError) -> Self {
        ClientError::Transport(e)
    }
}

impl From<InboxError> for ClientError {
    fn from(e: InboxError) -> Self {
        ClientError::Inbox(e)
    }
}

/// 一次一元 RPC 的结果槽。
#[derive(Debug, Clone)]
pub struct UnaryOutcome {
    pub rpc_id: String,
    pub result: crate::envelope::RpcResult,
}

impl UnaryOutcome {
    /// 业务 ok 时取 value；否则转 [`ClientError::Rpc`]（文案与 Node unwrap 一致）。
    pub fn unwrap(self, label: &str) -> Result<Value, ClientError> {
        if self.result.ok {
            Ok(self.result.value.unwrap_or(Value::Null))
        } else {
            let err = self.result.error.unwrap_or(crate::envelope::RpcError {
                code: "unknown".to_owned(),
                message: String::new(),
            });
            Err(ClientError::Rpc {
                label: label.to_owned(),
                code: err.code,
                message: err.message,
            })
        }
    }
}

/// 退役会话的清理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopOutcome {
    pub removed: usize,
}

/// DSH 一元 RPC 客户端（鉴权状态机内建）。
pub struct DshClient<T: Transport> {
    transport: T,
    base_url: String,
    sources: AuthSources,
    allow_remote: bool,
    cookie: Option<String>,
    strategy: Option<AuthStrategy>,
    prefer_token: bool,
    cookie_expires_at: u64,
}

impl<T: Transport> DshClient<T> {
    /// 配置的 DSH 基址（mux 连接等需要）。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 重定向基址：DSH 重启后监听端口常态漂移，调用方探测到新地址后切换。
    /// 鉴权状态一并失效（DSH 重启后旧 cookie/token 必然作废，ensure_auth 重铸）。
    pub fn set_base_url(&mut self, base_url: &str) {
        if self.base_url != base_url {
            self.base_url = base_url.trim_end_matches('/').to_owned();
            self.invalidate_auth();
        }
    }

    pub fn new(base_url: &str, transport: T, sources: AuthSources, allow_remote: bool) -> Self {
        Self {
            transport,
            base_url: base_url.trim_end_matches('/').to_owned(),
            sources,
            allow_remote,
            cookie: None,
            strategy: None,
            prefer_token: false,
            cookie_expires_at: 0,
        }
    }

    /// 使当前 Cookie 失效；DSH 重启或 401 后会自动重新鉴权。
    ///
    /// 刻意**不碰 `prefer_token`**：本方法也被 mux 流结束调用，WS 抖动/DSH 重启
    /// 是常态事件，不是「铸造的 Cookie 无效」的证据——策略切换只在真的 401 时
    /// 决定（见 [`DshClient::post_json`]）。
    pub fn invalidate_auth(&mut self) {
        self.cookie = None;
        self.cookie_expires_at = 0;
        self.strategy = None;
        if let Some(rescan) = self.sources.rediscover_token.as_mut() {
            let discovered = rescan();
            if !discovered.is_empty() {
                self.sources.launch_token = discovered;
            }
        }
    }

    /// 上次生效的策略（测试/调试观测口）。
    pub fn strategy(&self) -> Option<AuthStrategy> {
        self.strategy
    }

    /// 是否具备至少一种鉴权手段；两者都没有时请求按匿名发出（服务端回 401）。
    pub fn has_auth(&mut self) -> bool {
        !self.sources.launch_token.is_empty() || (self.sources.secret)().is_some()
    }

    fn mint_now(&mut self, secret: &[u8; 32], now_ms: u64) -> Result<String, ClientError> {
        let cookie =
            mint_browser_session_cookie(&self.base_url, secret, now_ms, MINTED_COOKIE_LIFETIME_MS)
                .map_err(|e| ClientError::Mint(e.to_string()))?;
        self.cookie = Some(cookie.clone());
        self.cookie_expires_at = now_ms + MINTED_COOKIE_LIFETIME_MS;
        self.strategy = Some(AuthStrategy::Minted);
        Ok(cookie)
    }

    /// 换取或铸造一个会话 Cookie（当前时间以 `now_ms` 注入）。
    pub fn ensure_auth(&mut self, now_ms: u64) -> Result<String, ClientError> {
        // 铸造来的 Cookie 到期前主动重铸；token 换来的 Cookie 无本地到期知识。
        if let Some(cookie) = self.cookie.clone() {
            if self.strategy != Some(AuthStrategy::Minted) {
                return Ok(cookie);
            }
            if now_ms
                < self
                    .cookie_expires_at
                    .saturating_sub(MINTED_COOKIE_REFRESH_MARGIN_MS)
            {
                return Ok(cookie);
            }
            self.cookie = None;
        }
        let secret = (self.sources.secret)();
        // 铸造只对回环地址成立：签名密钥取自本机 DSH_HOME，远程 DSH 用的是另一份。
        let can_mint = secret.is_some() && is_loopback_base(&self.base_url);
        if can_mint && !self.prefer_token && !self.sources.token_explicit {
            let secret = secret.expect("can_mint 蕴含 secret 存在");
            return self.mint_now(&secret, now_ms);
        }
        if self.sources.launch_token.is_empty() {
            if let Some(secret) = secret {
                return self.mint_now(&secret, now_ms);
            }
            return Err(ClientError::NoAuth(
                "DSH 鉴权不可用：没有可用的 launch token（config.json 的 dsh.authToken 为空，日志里也没发现），\
也读不到 ~/.dsh/.credentials.yaml 里的 client-connection/browser-session 签名密钥。\
请在 config.json 里显式设置 dsh.authToken，或确认 DSH_HOME 指向真正的 .dsh 目录。"
                    .to_owned(),
            ));
        }
        if !self.allow_remote && !is_loopback_base(&self.base_url) {
            return Err(ClientError::RemoteRefused(format!(
                "拒绝把 DSH launch token 发往非本机地址（{}）：那是进程启动凭据，只应交给 127.0.0.1。\
确实要连远程 DSH 时请在 config.json 里显式设置 dsh.allowRemote: true",
                self.base_url
            )));
        }
        // token 交换：launch token 只能放进 URL 查询串（DSH 协议就这么定的）。
        let mut url = Url::parse(&self.base_url)
            .map_err(|e| ClientError::Transport(TransportError(e.to_string())))?;
        url.set_path("/");
        url.set_query(None);
        url.query_pairs_mut()
            .append_pair("token", &self.sources.launch_token);
        let resp = self.transport.request(HttpRequest {
            method: "GET",
            url: url.to_string(),
            headers: Vec::new(),
            body: None,
        })?;
        let Some(set_cookie) = resp.header("set-cookie") else {
            return Err(ClientError::ExchangeFailed(format!("HTTP {}", resp.status)));
        };
        let cookie = set_cookie.split(';').next().unwrap_or_default().to_owned();
        self.cookie = Some(cookie.clone());
        self.strategy = Some(AuthStrategy::Token);
        Ok(cookie)
    }

    /// 发 JSON POST（带鉴权与 401 换票重试一次）。
    pub fn post_json(
        &mut self,
        path: &str,
        body: &str,
        now_ms: u64,
    ) -> Result<HttpResponse, ClientError> {
        let mut is_retry = false;
        loop {
            let mut req = HttpRequest {
                method: "POST",
                url: format!("{}{}", self.base_url, path),
                headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                body: Some(body.to_owned()),
            };
            if self.has_auth() {
                match self.ensure_auth(now_ms) {
                    Ok(cookie) => req.headers.push(("cookie".to_owned(), cookie)),
                    Err(ClientError::ExchangeFailed(_))
                        if !is_retry && !self.sources.launch_token.is_empty() =>
                    {
                        // 交换失败通常意味着 token 已过期（DSH 重启过）。解除
                        // prefer_token，否则客户端会抱着死 token 不放。
                        self.prefer_token = false;
                        self.invalidate_auth();
                        is_retry = true;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            let resp = self.transport.request(req)?;
            if resp.status == 401 && !is_retry && self.has_auth() {
                // 策略切换只在真的收到 401 时决定。铸造被 401 ⇒ 本机密钥对不上
                // （DSH_HOME 指错了），改用 launch token；token 换的票被 401 ⇒
                // 那个 token 已死，改回铸造。
                if self.strategy == Some(AuthStrategy::Minted)
                    && !self.sources.launch_token.is_empty()
                {
                    self.prefer_token = true;
                } else if self.strategy == Some(AuthStrategy::Token) {
                    self.prefer_token = false;
                }
                self.invalidate_auth();
                is_retry = true;
                continue;
            }
            return Ok(resp);
        }
    }

    /// 一元 RPC：POST `/api/<endpoint>`，只解析最外层信封（rpcId 配对 + result 槽）。
    pub fn call_unary(
        &mut self,
        method: &str,
        payload: Option<Value>,
        now_ms: u64,
    ) -> Result<UnaryOutcome, ClientError> {
        let message = RpcRequest::new(method, payload);
        let body =
            serde_json::to_string(&message).map_err(|e| ClientError::BadJson(e.to_string()))?;
        let resp = self.post_json(&format!("/api/{}", message.method), &body, now_ms)?;
        let full: Value =
            serde_json::from_str(&resp.body).map_err(|e| ClientError::BadJson(e.to_string()))?;
        if full.get("type").and_then(Value::as_str) != Some("server-response")
            || full.get("rpcId").and_then(Value::as_str) != Some(message.rpc_id.as_str())
            || full.get("result").is_none()
        {
            return Err(ClientError::InvalidResponse(format!(
                "invalid server-response for {}",
                message.method
            )));
        }
        let result: crate::envelope::RpcResult = serde_json::from_value(full["result"].clone())
            .map_err(|e| ClientError::BadJson(e.to_string()))?;
        Ok(UnaryOutcome {
            rpc_id: message.rpc_id,
            result,
        })
    }

    /// 读取一个会话的待处理 inbox 项（退役/重置前要清掉的队列）。
    ///
    /// 主路径 `session/projections`（DSH 0.2.0 起的一元 RPC）：一条普通 HTTP RPC，
    /// 不需要为「看一眼队列」开 WebSocket。`session/not-found` 同样意味着没有
    /// 队列可清。
    ///
    /// Node 版在 endpoint 不存在（更早的 DSH）时回退 `session/control` 开场
    /// baseline（WS）；回退路径随 mux 客户端批次接入——当前主路径的协议级失败
    /// 直接上抛。
    fn read_session_queue(
        &mut self,
        session_id: &str,
        now_ms: u64,
    ) -> Result<Vec<String>, ClientError> {
        let response = self.call_unary(
            "session/projections",
            Some(json!({ "sessionId": session_id })),
            now_ms,
        )?;
        if response.result.ok {
            // value === null ⇒ 会话不在宿主注册表里 ⇒ 没有待处理项。
            let values = response.result.value.as_ref().and_then(|v| v.get("values"));
            return Ok(inbox_item_ids(&inbox_items_of_projections(values)?)?);
        }
        if let Some(err) = &response.result.error
            && err.code == "session/not-found"
        {
            return Ok(Vec::new());
        }
        // 协议级失败（endpoint 不存在等）：按 Node 语义应回退 control 流；
        // mux 客户端接入前显式上抛，不静默当空队列。
        Err(response.unwrap("session/projections").unwrap_err())
    }

    /// 退役会话前先移除 pending inbox，再取消正在运行的 turn。
    /// DSH 的 session/cancel 保留 inbox，archiveSession 只隐藏会话，均不能代替清队列。
    pub fn stop_session_work(
        &mut self,
        session_id: &str,
        now_ms: u64,
    ) -> Result<StopOutcome, ClientError> {
        if session_id.is_empty() {
            return Err(ClientError::InvalidResponse(
                "sessionId is required".to_owned(),
            ));
        }
        let item_ids = self.read_session_queue(session_id, now_ms)?;
        let mut removed = 0usize;
        let mut failure: Option<ClientError> = None;
        let mut seen = std::collections::HashSet::new();
        for item_id in item_ids {
            if !seen.insert(item_id.clone()) {
                continue;
            }
            match self.call_unary(
                "session/updateQueue",
                Some(json!({
                    "sessionId": session_id,
                    "itemId": item_id,
                    "action": { "kind": "remove" }
                })),
                now_ms,
            ) {
                Ok(r) if r.result.ok => removed += 1,
                Ok(r) => {
                    // 已被 agent 取走的队列项不再存在；接下来的 cancel 会取消当前 turn。
                    if r.result.error.as_ref().map(|e| e.code.as_str())
                        != Some("session/queue-item-not-found")
                    {
                        failure = Some(r.unwrap("session/updateQueue").unwrap_err());
                        break;
                    }
                }
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        match self.call_unary(
            "session/cancel",
            Some(json!({ "sessionId": session_id })),
            now_ms,
        ) {
            Ok(r) if r.result.ok => {}
            Ok(r) => {
                if r.result.error.as_ref().map(|e| e.code.as_str()) != Some("session/not-found") {
                    failure = failure.or(Some(r.unwrap("session/cancel").unwrap_err()));
                }
            }
            Err(e) => failure = failure.or(Some(e)),
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(StopOutcome { removed }),
        }
    }
}

// ── 生产传输实现 ────────────────────────────────────────────────────────────

/// reqwest blocking 传输（超时由构造方配置；DSH 是本机信任端点，不属 SSRF 面）。
#[cfg(feature = "reqwest")]
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::blocking::Client,
}

#[cfg(feature = "reqwest")]
impl ReqwestTransport {
    pub fn new(timeout_ms: u64) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_millis(timeout_ms))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client 构造必然成功");
        Self { client }
    }
}

#[cfg(feature = "reqwest")]
impl Transport for ReqwestTransport {
    fn request(&mut self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| TransportError(e.to_string()))?;
        let mut out = self
            .client
            .request(method, &req.url)
            .header("user-agent", "queqiao-dsh");
        for (k, v) in &req.headers {
            out = out.header(k, v);
        }
        if let Some(body) = &req.body {
            out = out.body(body.clone());
        }
        let resp = out.send().map_err(|e| TransportError(e.to_string()))?;
        let status = resp.status().as_u16();
        let mut headers = Vec::new();
        for (k, v) in resp.headers() {
            if let Ok(v) = v.to_str() {
                headers.push((k.as_str().to_owned(), v.to_owned()));
            }
        }
        let body = resp.text().map_err(|e| TransportError(e.to_string()))?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    type Handler = Box<dyn Fn(&HttpRequest) -> Result<HttpResponse, TransportError>>;

    /// 内存传输：handler 处理请求，所有请求被记录（Node 版 fetch mock 的同构形态）。
    struct MockTransport {
        handler: Handler,
        requests: Rc<RefCell<Vec<HttpRequest>>>,
    }

    impl MockTransport {
        fn new(handler: Handler) -> (Self, Rc<RefCell<Vec<HttpRequest>>>) {
            let requests = Rc::new(RefCell::new(Vec::new()));
            let t = Self {
                handler,
                requests: requests.clone(),
            };
            (t, requests)
        }
    }

    impl Transport for MockTransport {
        fn request(&mut self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
            (self.requests.borrow_mut()).push(req.clone());
            (self.handler)(&req)
        }
    }

    fn json_ok(rpc_body: Value) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: serde_json::to_string(&rpc_body).unwrap(),
        }
    }

    fn unary_response(rpc_id: &str, result: Value) -> HttpResponse {
        json_ok(json!({ "type": "server-response", "rpcId": rpc_id, "result": result }))
    }

    /// 请求计数快照（path → 次数）。
    fn path_counts(reqs: &[HttpRequest]) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = Vec::new();
        for r in reqs {
            let path = Url::parse(&r.url).unwrap().path().to_string();
            if let Some(slot) = out.iter_mut().find(|(p, _)| *p == path) {
                slot.1 += 1;
            } else {
                out.push((path, 1));
            }
        }
        out
    }

    const SECRET: [u8; 32] = [7; 32];

    fn minted_sources() -> AuthSources {
        AuthSources {
            launch_token: String::new(),
            token_explicit: false,
            secret: Box::new(|| Some(SECRET)),
            rediscover_token: None,
        }
    }

    /// 首启铸造：无 token、有密钥 → minted Cookie，不发任何交换请求。
    #[test]
    fn minted_by_default() {
        let (transport, reqs) = MockTransport::new(Box::new(|req| {
            let rpc_id =
                serde_json::from_str::<Value>(req.body.as_deref().unwrap()).unwrap()["rpcId"]
                    .as_str()
                    .unwrap()
                    .to_owned();
            Ok(unary_response(
                &rpc_id,
                json!({ "ok": true, "value": { "accepted": true } }),
            ))
        }));
        let mut client = DshClient::new("http://127.0.0.1:9", transport, minted_sources(), false);
        let out = client
            .call_unary("session/cancel", Some(json!({ "sessionId": "s1" })), 1000)
            .unwrap();
        assert!(out.result.ok);
        assert_eq!(client.strategy(), Some(AuthStrategy::Minted));
        let reqs = reqs.borrow();
        assert_eq!(reqs.len(), 1, "只发一次 RPC，无交换请求");
        let cookie = reqs[0]
            .headers
            .iter()
            .find(|(k, _)| k == "cookie")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert!(cookie.starts_with("dsh-auth-"));
        // POST /api/session/cancel + JSON 信封。
        assert!(reqs[0].url.ends_with("/api/session/cancel"));
    }

    /// Cookie 到期前用缓存；进入 margin 窗口（到期前 5min）主动重铸。
    #[test]
    fn minted_cookie_refresh_before_expiry() {
        let (transport, _) = MockTransport::new(Box::new(|_| {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: String::new(),
            })
        }));
        let mut client =
            DshClient::new("http://127.0.0.1:3080", transport, minted_sources(), false);
        let c1 = client.ensure_auth(1000).unwrap();
        // 剩余寿命 6min（> 5min margin）→ 缓存同一 Cookie。
        let c2 = client.ensure_auth(1000 + 54 * 60_000).unwrap();
        assert_eq!(c1, c2);
        // 剩余寿命 4min（< margin）→ 重铸（now 变化 → 签名变化）。
        let c3 = client.ensure_auth(1000 + 56 * 60_000).unwrap();
        assert_ne!(c1, c3);
    }

    /// 铸造被 401 且有 launch token → 切 token 策略换票并重试成功。
    #[test]
    fn minted_401_switches_to_token() {
        let first = std::sync::Arc::new(std::sync::Mutex::new(true));
        let (transport, reqs) = MockTransport::new(Box::new(move |req| {
            let path = Url::parse(&req.url).unwrap().path().to_string();
            if path == "/" {
                return Ok(HttpResponse {
                    status: 303,
                    headers: vec![("set-cookie".into(), "exchanged=1; Path=/; HttpOnly".into())],
                    body: String::new(),
                });
            }
            let rpc_id =
                serde_json::from_str::<Value>(req.body.as_deref().unwrap()).unwrap()["rpcId"]
                    .as_str()
                    .unwrap()
                    .to_owned();
            let cookie = req
                .headers
                .iter()
                .find(|(k, _)| k == "cookie")
                .map(|(_, v)| v.clone());
            if cookie.is_some_and(|c| c.starts_with("dsh-auth-")) && *first.lock().unwrap() {
                *first.lock().unwrap() = false;
                return Ok(HttpResponse {
                    status: 401,
                    headers: vec![],
                    body: String::new(),
                });
            }
            Ok(unary_response(
                &rpc_id,
                json!({ "ok": true, "value": true }),
            ))
        }));
        let sources = AuthSources {
            launch_token: "fixture-token-0000000001".to_owned(),
            token_explicit: false,
            secret: Box::new(|| Some(SECRET)),
            rediscover_token: None,
        };
        let mut client = DshClient::new("http://127.0.0.1:9", transport, sources, false);
        let out = client.call_unary("settings/describe", None, 1000).unwrap();
        assert!(out.result.ok);
        assert_eq!(client.strategy(), Some(AuthStrategy::Token));
        let reqs = reqs.borrow();
        // 铸造 401 → 交换（GET /?token=）→ token Cookie 重试成功。
        assert_eq!(
            path_counts(&reqs),
            vec![
                ("/api/settings/describe".to_string(), 2),
                ("/".to_string(), 1)
            ]
        );
    }

    /// token 换的票被 401 → 切回铸造：invalidate 重探发现 DSH 刚重建的凭据密钥
    /// （Node 版「密钥文件可能刚被 DSH 首次启动写入」的真实场景）。
    #[test]
    fn token_401_switches_back_to_minted_after_secret_rediscovery() {
        let first = std::sync::Arc::new(std::sync::Mutex::new(true));
        let secret_available = std::sync::Arc::new(std::sync::Mutex::new(false));
        let (transport, _) = MockTransport::new(Box::new(move |req| {
            let path = Url::parse(&req.url).unwrap().path().to_string();
            if path == "/" {
                return Ok(HttpResponse {
                    status: 303,
                    headers: vec![("set-cookie".into(), "exchanged=1".into())],
                    body: String::new(),
                });
            }
            let rpc_id =
                serde_json::from_str::<Value>(req.body.as_deref().unwrap()).unwrap()["rpcId"]
                    .as_str()
                    .unwrap()
                    .to_owned();
            let cookie = req
                .headers
                .iter()
                .find(|(k, _)| k == "cookie")
                .map(|(_, v)| v.clone());
            if cookie.as_deref() == Some("exchanged=1") && *first.lock().unwrap() {
                *first.lock().unwrap() = false;
                return Ok(HttpResponse {
                    status: 401,
                    headers: vec![],
                    body: String::new(),
                });
            }
            Ok(unary_response(
                &rpc_id,
                json!({ "ok": true, "value": true }),
            ))
        }));
        // 密钥初始不可用（走 token 交换）；invalidate 后重探变为可用（DSH 重建了凭据）。
        let probe = {
            let available = secret_available.clone();
            move || {
                if *available.lock().unwrap() {
                    Some(SECRET)
                } else {
                    None
                }
            }
        };
        let sources = AuthSources {
            launch_token: "fixture-token-0000000002".to_owned(),
            token_explicit: false,
            secret: Box::new(probe),
            rediscover_token: None,
        };
        let mut client = DshClient::new("http://127.0.0.1:9", transport, sources, false);
        // 第一步：secret 不可用 → 走交换换票。
        *secret_available.lock().unwrap() = false;
        let exchanged = client.ensure_auth(1000).unwrap();
        assert_eq!(exchanged, "exchanged=1");
        assert_eq!(client.strategy(), Some(AuthStrategy::Token));
        // 凭据文件此刻出现（DSH 首启写入）；401 → 切回铸造。
        *secret_available.lock().unwrap() = true;
        let out = client.call_unary("settings/describe", None, 2000).unwrap();
        assert!(out.result.ok);
        assert_eq!(client.strategy(), Some(AuthStrategy::Minted));
    }

    /// invalidate_auth 是常态事件（WS 抖动）：不清策略偏好，重探后继续铸造。
    #[test]
    fn invalidate_keeps_minted_strategy() {
        let (transport, _) = MockTransport::new(Box::new(|_| {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: String::new(),
            })
        }));
        let mut client =
            DshClient::new("http://127.0.0.1:3080", transport, minted_sources(), false);
        client.ensure_auth(1000).unwrap();
        for _ in 0..3 {
            client.invalidate_auth();
            let again = client.ensure_auth(1000).unwrap();
            assert!(again.starts_with("dsh-auth-"));
            assert_eq!(client.strategy(), Some(AuthStrategy::Minted));
        }
    }

    /// 非回环 + token 交换 → 显式拒绝且不发任何请求（launch token 是进程凭据）。
    #[test]
    fn non_loopback_refused_without_request() {
        let (transport, reqs) = MockTransport::new(Box::new(|_| {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: String::new(),
            })
        }));
        let mut client = DshClient::new(
            "http://dsh.example.com",
            transport,
            AuthSources::token_only("fixture-token", true),
            false,
        );
        let err = client.ensure_auth(1000).unwrap_err();
        assert!(matches!(err, ClientError::RemoteRefused(_)));
        assert!(err.to_string().contains("非本机地址"));
        assert!(reqs.borrow().is_empty(), "拒绝时不应该发出任何请求");
        // 显式 allow_remote 放行（闸门不再拦）。
        let (transport2, _) = MockTransport::new(Box::new(|_| {
            Ok(HttpResponse {
                status: 401,
                headers: vec![],
                body: String::new(),
            })
        }));
        let mut client2 = DshClient::new(
            "http://dsh.example.com",
            transport2,
            AuthSources::token_only("fixture-token", true),
            true,
        );
        assert!(matches!(
            client2.ensure_auth(1000),
            Err(ClientError::ExchangeFailed(_))
        ));
    }

    /// 无任何凭据：指引用户配置的中文错误。
    #[test]
    fn no_auth_reports_configuration_error() {
        let (transport, _) = MockTransport::new(Box::new(|_| {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: String::new(),
            })
        }));
        let mut client = DshClient::new(
            "http://127.0.0.1:3080",
            transport,
            AuthSources::token_only("", false),
            false,
        );
        let err = client.ensure_auth(1000).unwrap_err();
        assert!(matches!(err, ClientError::NoAuth(_)));
        assert!(err.to_string().contains("DSH 鉴权不可用"));
    }

    /// 策略链（与 Node 版当次语义一致）：请求 1 铸 401 → 切 token → 交换失败 →
    /// 当次抛错；交换失败的解除分支在**下一轮请求**生效（prefer_token 复位 →
    /// 回铸造成功）——「解除偏好否则抱着死 token 不放」的价值就在后续请求。
    #[test]
    fn exchange_failure_falls_back_to_minted_on_next_request() {
        let minted_seen = Rc::new(RefCell::new(false));
        let (transport, _) = MockTransport::new(Box::new(move |req| {
            let path = Url::parse(&req.url).unwrap().path().to_string();
            if path == "/" {
                // 交换失败：无 set-cookie（token 已随 DSH 重启失效）。
                return Ok(HttpResponse {
                    status: 401,
                    headers: vec![],
                    body: String::new(),
                });
            }
            let rpc_id =
                serde_json::from_str::<Value>(req.body.as_deref().unwrap()).unwrap()["rpcId"]
                    .as_str()
                    .unwrap()
                    .to_owned();
            let cookie = req
                .headers
                .iter()
                .find(|(k, _)| k == "cookie")
                .map(|(_, v)| v.clone());
            if cookie.is_some_and(|c| c.starts_with("dsh-auth-")) && !*minted_seen.borrow() {
                // 首个铸造 Cookie 被 401 → 切 token 策略。
                *minted_seen.borrow_mut() = true;
                return Ok(HttpResponse {
                    status: 401,
                    headers: vec![],
                    body: String::new(),
                });
            }
            Ok(unary_response(
                &rpc_id,
                json!({ "ok": true, "value": true }),
            ))
        }));
        let sources = AuthSources {
            launch_token: "fixture-token-0000000003".to_owned(),
            token_explicit: false,
            secret: Box::new(|| Some(SECRET)),
            rediscover_token: None,
        };
        let mut client = DshClient::new("http://127.0.0.1:9", transport, sources, false);
        // 请求 1：铸 401 → 换 token → 交换失败 → 当次抛错（与 Node 版一致）。
        assert!(matches!(
            client.call_unary("settings/describe", None, 1000),
            Err(ClientError::ExchangeFailed(_))
        ));
        // 请求 2：交换失败的解除分支命中（新请求 is_retry=false）→ 回铸造成功。
        let out = client.call_unary("settings/describe", None, 2000).unwrap();
        assert!(out.result.ok);
        assert_eq!(client.strategy(), Some(AuthStrategy::Minted));
    }

    /// 响应信封不符（rpcId 不配对/缺 result）必须显式失败。
    #[test]
    fn envelope_mismatch_rejected() {
        for bad in [
            json!({ "type": "server-response", "rpcId": "other", "result": { "ok": true } }),
            json!({ "type": "client-request", "rpcId": "x", "result": { "ok": true } }),
            json!({ "type": "server-response", "rpcId": "x" }),
        ] {
            let label = format!("{bad}");
            let (transport, _) = MockTransport::new(Box::new(move |_| Ok(json_ok(bad.clone()))));
            let mut client =
                DshClient::new("http://127.0.0.1:9", transport, minted_sources(), false);
            assert!(
                matches!(
                    client.call_unary("settings/describe", None, 1000),
                    Err(ClientError::InvalidResponse(_))
                ),
                "bad={label}"
            );
        }
    }

    /// stop_session_work：projections 取两项 → 逐项 remove → cancel，返回 removed=2。
    #[test]
    fn stop_session_work_removes_queue_and_cancels() {
        let calls: Rc<RefCell<Vec<(String, Value)>>> = Rc::new(RefCell::new(Vec::new()));
        let calls2 = calls.clone();
        let (transport, _) = MockTransport::new(Box::new(move |req| {
            let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
            let method = body["method"].as_str().unwrap().to_owned();
            let rpc_id = body["rpcId"].as_str().unwrap().to_owned();
            calls2
                .borrow_mut()
                .push((method.clone(), body["payload"].clone()));
            match method.as_str() {
                "session/projections" => Ok(unary_response(
                    &rpc_id,
                    json!({ "ok": true, "value": { "asOfSeq": 5, "values": {
                        "inbox": { "next-turn": [ { "id": "own-1" }, { "id": "own-2" } ], "next-step": [] }
                    } } }),
                )),
                _ => Ok(unary_response(
                    &rpc_id,
                    json!({ "ok": true, "value": { "accepted": true } }),
                )),
            }
        }));
        let mut client = DshClient::new("http://127.0.0.1:9", transport, minted_sources(), false);
        let outcome = client.stop_session_work("session-a", 1000).unwrap();
        assert_eq!(outcome.removed, 2);
        let calls = calls.borrow();
        let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(
            methods,
            [
                "session/projections",
                "session/updateQueue",
                "session/updateQueue",
                "session/cancel"
            ]
        );
        assert_eq!(calls[1].1["args"]["request"]["itemId"], "own-1");
        assert_eq!(calls[2].1["args"]["request"]["itemId"], "own-2");
        assert_eq!(calls[1].1["args"]["request"]["action"]["kind"], "remove");
        assert_eq!(calls[3].1["args"]["request"]["sessionId"], "session-a");
    }

    /// queue-item-not-found（agent 已取走）与 cancel 的 not-found 均被容忍。
    #[test]
    fn stop_session_work_tolerates_gone_items() {
        let (transport, _) = MockTransport::new(Box::new(|req| {
            let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
            let method = body["method"].as_str().unwrap();
            let rpc_id = body["rpcId"].as_str().unwrap().to_owned();
            match method {
                "session/projections" => Ok(unary_response(
                    &rpc_id,
                    json!({ "ok": true, "value": { "values": { "inbox": { "next-turn": [ { "id": "gone-1" } ] } } } }),
                )),
                "session/updateQueue" => Ok(unary_response(
                    &rpc_id,
                    json!({ "ok": false, "error": { "code": "session/queue-item-not-found", "message": "gone" } }),
                )),
                "session/cancel" => Ok(unary_response(
                    &rpc_id,
                    json!({ "ok": false, "error": { "code": "session/not-found", "message": "gone" } }),
                )),
                _ => unreachable!(),
            }
        }));
        let mut client = DshClient::new("http://127.0.0.1:9", transport, minted_sources(), false);
        let outcome = client.stop_session_work("session-a", 1000).unwrap();
        assert_eq!(outcome.removed, 0);
    }

    /// projections 的 session/not-found ⇒ 会话不在注册表 ⇒ 无队列可清。
    #[test]
    fn stop_session_work_with_missing_session_is_noop() {
        let (transport, _) = MockTransport::new(Box::new(|req| {
            let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
            let rpc_id = body["rpcId"].as_str().unwrap().to_owned();
            Ok(unary_response(
                &rpc_id,
                json!({ "ok": false, "error": { "code": "session/not-found", "message": "x" } }),
            ))
        }));
        let mut client = DshClient::new("http://127.0.0.1:9", transport, minted_sources(), false);
        let outcome = client.stop_session_work("session-a", 1000).unwrap();
        assert_eq!(outcome.removed, 0);
    }

    /// unwrap 错误链：updateQueue 的其他业务错误使 stop 失败并带出文案。
    #[test]
    fn stop_session_work_surfaces_business_errors() {
        let (transport, _) = MockTransport::new(Box::new(|req| {
            let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
            let method = body["method"].as_str().unwrap();
            let rpc_id = body["rpcId"].as_str().unwrap().to_owned();
            match method {
                "session/projections" => Ok(unary_response(
                    &rpc_id,
                    json!({ "ok": true, "value": { "values": { "inbox": { "next-turn": [ { "id": "x-1" } ] } } } }),
                )),
                _ => Ok(unary_response(
                    &rpc_id,
                    json!({ "ok": false, "error": { "code": "gateway/boom", "message": "bad" } }),
                )),
            }
        }));
        let mut client = DshClient::new("http://127.0.0.1:9", transport, minted_sources(), false);
        let err = client.stop_session_work("session-a", 1000).unwrap_err();
        assert!(matches!(err, ClientError::Rpc { .. }));
        assert!(err.to_string().contains("gateway/boom"));
    }

    /// 基址重定向（DSH 重启端口漂移后 mux 重连路径调用）：
    /// 尾斜杠归一；相同值幂等不触发失效。
    #[test]
    fn set_base_url_switches_and_normalizes() {
        let (transport, _) = MockTransport::new(Box::new(|_| {
            Ok(HttpResponse {
                status: 404,
                headers: vec![],
                body: String::new(),
            })
        }));
        let mut client = DshClient::new("http://127.0.0.1:9", transport, minted_sources(), false);
        assert_eq!(client.base_url(), "http://127.0.0.1:9");
        client.set_base_url("http://127.0.0.1:12345/");
        assert_eq!(client.base_url(), "http://127.0.0.1:12345");
        // 相同值（含尾斜杠差异）不重复失效鉴权。
        client.set_base_url("http://127.0.0.1:12345/");
        assert_eq!(client.base_url(), "http://127.0.0.1:12345");
    }
}
