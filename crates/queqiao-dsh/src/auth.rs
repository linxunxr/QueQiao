//! 浏览器会话 Cookie 离线铸造与鉴权边界（移植自 Node 版 `src/dsh-client.js` 鉴权段，
//! 契约同步迁移）。
//!
//! 背景：进程启动 token 是现生成、只在内存里的随机数，进程一重启就失效，且只有
//! 「启动器记了 stdout 日志」时才能被发现——DSH 每升一次级、启动方式每变一次，
//! 这条链路就要重新适配一次。
//!
//! DSH 另有一份**持久化**的会话签名密钥：credentials 里的
//! `client-connection/browser-session`（32 字节，首次生成后长期不变）。浏览器
//! Cookie 就是用它做 HMAC 签名的，服务端校验只要求：
//! - cookie 名 = `dsh-auth-` + base64url(sha256(authority))；
//! - payload.authority === authority（Host 头，例如 127.0.0.1:3080）；
//! - issuedAt <= now < expiresAt 且有效期不超过 cookieMaxAgeDays（默认 30 天）。
//!
//! 所以本机同用户进程可以**离线铸造**一个合法 Cookie，完全绕开启动 token。
//! 权限没有提升：桥接本来就用启动 token 换到同等会话权限，这里只是换了一条
//! 不依赖「启动器有没有记日志」的路。
//!
//! 分层调整：Node 版 `readBrowserSessionSecret` 直连 fs，这里拆为 yaml 文本解析
//! 纯函数（[`parse_browser_session_secret`]，文件读取在 bin 层）；`Date.now()`
//! 改为显式传 `now_ms`。

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::{Digest, Sha256};
use url::Url;

type HmacSha256 = Hmac<Sha256>;

/// credentials 里 browser-session 密钥的键名。
const BROWSER_SESSION_KEY: &str = "client-connection/browser-session";
const COOKIE_PAYLOAD_VERSION: u8 = 1;
const COOKIE_PREFIX: &str = "dsh-auth-";
/// 签名密钥长度：换密钥方案时宁可回退也不发坏 Cookie。
const SECRET_BYTES: usize = 32;

/// 铸造 Cookie 的有效期：远小于 DSH 默认上限（30 天），够用且留足余量。
pub const MINTED_COOKIE_LIFETIME_MS: u64 = 60 * 60 * 1000;
/// 到期前多久主动重铸。铸造是纯本地 HMAC（无网络往返），提前换比撞 401 再重试便宜。
pub const MINTED_COOKIE_REFRESH_MARGIN_MS: u64 = 5 * 60 * 1000;

/// 铸造失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MintError {
    BadBaseUrl,
}

impl std::fmt::Display for MintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MintError::BadBaseUrl => write!(f, "DSH baseUrl has no authority"),
        }
    }
}

impl std::error::Error for MintError {}

fn base64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// URL 的 authority（host:port；IPv6 字面量带方括号），等价 JS `new URL(x).host`。
pub fn authority_of(base_url: &str) -> Result<String, MintError> {
    let u = Url::parse(base_url).map_err(|_| MintError::BadBaseUrl)?;
    let host = u.host_str().ok_or(MintError::BadBaseUrl)?;
    // 本 url crate 版本的 host_str 对 IPv6 已带方括号；不带时补齐（JS host 形态）。
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let authority = match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    };
    if authority.is_empty() {
        return Err(MintError::BadBaseUrl);
    }
    Ok(authority)
}

#[derive(Serialize)]
struct CookiePayload<'a> {
    version: u8,
    authority: &'a str,
    #[serde(rename = "issuedAt")]
    issued_at: u64,
    #[serde(rename = "expiresAt")]
    expires_at: u64,
}

/// 用签名密钥为指定 DSH 地址铸造一个会话 Cookie。
///
/// 返回可直接放进 Cookie 头的 `name=value` 串：
/// `dsh-auth-<base64url(sha256(authority))>=v1.<base64url(json payload)>.<base64url(hmac)>`。
pub fn mint_browser_session_cookie(
    base_url: &str,
    secret: &[u8],
    now_ms: u64,
    lifetime_ms: u64,
) -> Result<String, MintError> {
    let authority = authority_of(base_url)?;
    let payload = CookiePayload {
        version: COOKIE_PAYLOAD_VERSION,
        authority: &authority,
        issued_at: now_ms,
        expires_at: now_ms + lifetime_ms,
    };
    let json = serde_json::to_vec(&payload).expect("cookie payload 序列化必然成功");
    let body = base64url(&json);
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC 接受任意长度密钥");
    mac.update(body.as_bytes());
    let signature = base64url(&mac.finalize().into_bytes());
    let name = format!(
        "{COOKIE_PREFIX}{}",
        base64url(&Sha256::digest(authority.as_bytes()))
    );
    Ok(format!("{name}=v1.{body}.{signature}"))
}

/// 解析 `~/.dsh/.credentials.yaml` 文本，取出 browser-session 签名密钥。
///
/// 密钥缺失 / 结构不符 / 长度不是 32 字节时返回 `None`（调用方回退启动 token
/// 交换——DSH 换了密钥方案时宁可回退也不发坏 Cookie）。
///
/// 手写缩进匹配（替代 serde_yaml，连带去掉 unsafe-libyaml 整条依赖链）：
/// 该文件由 DSH 机器生成、形状固定（`records:` 下 2 空格缩进的
/// `client-connection/browser-session:` → kind/payload.version/payload.secret），
/// 逐行状态机足够；结构意外时解析不到字段，走既有 None 回退，语义不变。
pub fn parse_browser_session_secret(yaml_text: &str) -> Option<[u8; SECRET_BYTES]> {
    let mut in_record = false;
    let mut kind = String::new();
    let mut version: Option<i64> = None;
    let mut secret = String::new();
    for line in yaml_text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - trimmed.len();
        if let Some(rest) = trimmed.strip_prefix(BROWSER_SESSION_KEY) {
            // 记录头：records 下第一层（2 空格缩进）；同前缀但更深的嵌套不认。
            in_record = rest == ":" && indent == 2;
            continue;
        }
        if in_record {
            if indent <= 2 {
                in_record = false; // 下一个顶级/records 层条目，记录块结束
                continue;
            }
            if let Some(v) = trimmed.strip_prefix("kind:") {
                kind = v.trim().trim_matches('"').trim_matches('\'').to_owned();
            } else if let Some(v) = trimmed.strip_prefix("version:") {
                version = v.trim().parse().ok();
            } else if let Some(v) = trimmed.strip_prefix("secret:") {
                secret = v.trim().trim_matches('"').trim_matches('\'').to_owned();
            }
        }
    }
    if kind != "grant" || version != Some(1) {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(&secret).ok()?;
    decoded.try_into().ok()
}

/// baseUrl 是否指向本机回环地址。
///
/// 用途：DSH 的 launch token 是进程启动凭据，换 Cookie 时必须放进 URL 查询串，
/// 一旦发往非回环地址就等于把凭据交给中途的任何一环（代理日志、抓包、对端记录）。
/// 所以默认只允许回环，远程部署需要显式 opt-in。
pub fn is_loopback_base(base_url: &str) -> bool {
    let Ok(u) = Url::parse(base_url) else {
        return false;
    };
    let Some(host) = u.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    // IPv6 回环（本 url crate 的 host_str 对 IPv6 带方括号，两种形态都认）。
    if host == "::1" || host == "[::1]" {
        return true;
    }
    // 127.0.0.0/8 整个网段都是回环（与 Node 版正则一致）。
    static LOOPBACK_IPV4: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$").expect("静态正则必然合法")
    });
    LOOPBACK_IPV4.is_match(&host)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约与 Node 版 test-dsh-auth 纯函数段一致：回环判定。
    #[test]
    fn loopback_detection() {
        assert!(is_loopback_base("http://127.0.0.1:3080"));
        assert!(!is_loopback_base("http://10.0.0.5:3080"));
        // 127.0.0.0/8 全段、localhost、IPv6 回环。
        assert!(is_loopback_base("http://127.1.2.3:80"));
        assert!(is_loopback_base("http://localhost:3080"));
        assert!(is_loopback_base("http://[::1]:3080"));
        assert!(!is_loopback_base("http://dsh.example.com"));
        assert!(!is_loopback_base("not a url"));
    }

    /// Cookie 形状必须与服务端 decodeCookie/isAuthenticated 的校验一致
    /// （Node 版 test-dsh-auth 的形状断言，离线部分）。
    #[test]
    fn minted_cookie_shape() {
        let secret = [7u8; 32];
        let now = 1_700_000_000_000u64;
        let cookie = mint_browser_session_cookie(
            "http://127.0.0.1:3080",
            &secret,
            now,
            MINTED_COOKIE_LIFETIME_MS,
        )
        .unwrap();
        let (name, value) = cookie.split_once('=').expect("name=value 形态");
        assert!(name.starts_with("dsh-auth-"), "name={name}");
        let parts: Vec<&str> = value.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "v1");
        // payload 是 base64url 的 JSON：authority 绑定、有效期 1h（≤ 30 天上限）。
        let decoded = URL_SAFE_NO_PAD.decode(parts[1]).expect("body 是 base64url");
        let payload: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(payload["authority"], "127.0.0.1:3080");
        assert_eq!(payload["version"], 1);
        let issued = payload["issuedAt"].as_u64().unwrap();
        let expires = payload["expiresAt"].as_u64().unwrap();
        assert!(expires > issued);
        assert!(expires - issued <= 30 * 86_400_000);
        assert_eq!(expires - issued, MINTED_COOKIE_LIFETIME_MS);
    }

    /// authority 提取：host:port 绑定进 Cookie 名与签名载荷。
    #[test]
    fn authority_extraction() {
        assert_eq!(
            authority_of("http://127.0.0.1:3080").unwrap(),
            "127.0.0.1:3080"
        );
        assert_eq!(
            authority_of("http://dsh.example.com").unwrap(),
            "dsh.example.com"
        );
        assert!(authority_of("::::").is_err());
    }

    /// credentials 解析：合法 yaml 取出 32 字节密钥；缺失/异构/长度不符返回 None。
    #[test]
    fn parses_browser_session_secret() {
        // base64url(32 字节 0x07)。
        let secret_b64 = URL_SAFE_NO_PAD.encode([7u8; 32]);
        let yaml = format!(
            "records:\n  {BROWSER_SESSION_KEY}:\n    kind: grant\n    payload:\n      version: 1\n      secret: {secret_b64}\n"
        );
        assert_eq!(parse_browser_session_secret(&yaml), Some([7u8; 32]));
        // kind 不符 / version 不符 / 密钥缺失 / 长度非 32 字节。
        let wrong_kind = yaml.replace("kind: grant", "kind: other");
        assert_eq!(parse_browser_session_secret(&wrong_kind), None);
        let wrong_version = yaml.replace("version: 1", "version: 2");
        assert_eq!(parse_browser_session_secret(&wrong_version), None);
        let short_secret = yaml.replace(&secret_b64, "AAAA");
        assert_eq!(parse_browser_session_secret(&short_secret), None);
        assert_eq!(parse_browser_session_secret(""), None);
        assert_eq!(parse_browser_session_secret("records: {}"), None);
    }

    // Node 版 test-dsh-auth 的端到端段（铸造 Cookie 过真实 DSH 鉴权、陈旧 token
    // 失效、invalidateAuth 不改策略、到期重铸）需要 DSH 在线，属 M2 冒烟范围
    //（bin 内临时驱动），不在离线单测。
}
