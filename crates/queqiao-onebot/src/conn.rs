//! SnowLuma 连接自发现（移植自 Node 版 `src/snowluma-conn.js` + 同名测试，
//! 契约同步迁移）。
//!
//! 背景（实测确认）：SnowLuma 的 OneBot 配置**按账号分文件**
//! （`<SnowLuma>/config/onebot_<QQ号>.json`），每个账号有各自的
//! `httpServers[].accessToken`，且 **HTTP 与 WebSocket 是两个不同的 token**。
//! 换账号登录后旧 token 直接 401（retcode 1401），硬编码 token 的工具全部失效。
//!
//! 本模块的做法：**不猜**——拿候选 token 真去问一次 get_login_info，谁通用谁。
//! 文件 IO（readdir/read）与真实 HTTP 探测留 bin 层注入；这里持解析与决策纯函数。

use serde::Deserialize;
use serde_json::{Value, json};

/// 一个 server 端点（HTTP 或 WS）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub token: String,
    pub host: String,
    pub port: u16,
}

/// 一个账号的双端点信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountEndpoints {
    pub uin: String,
    pub http: Option<Endpoint>,
    pub ws: Option<Endpoint>,
}

/// 从文件名识别账号：`onebot_<数字>.json`（大小写不敏感）。
pub fn account_file_uin(name: &str) -> Option<&str> {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".json")?;
    let digits = stem.strip_prefix("onebot_")?;
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        Some(&name[7..name.len() - 5])
    } else {
        None
    }
}

#[derive(Deserialize)]
struct ServerConfig {
    #[serde(rename = "accessToken", default)]
    access_token: String,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
}

#[derive(Deserialize, Default)]
struct Networks {
    #[serde(rename = "httpServers", default)]
    http_servers: Vec<ServerConfig>,
    #[serde(rename = "wsServers", default)]
    ws_servers: Vec<ServerConfig>,
}

#[derive(Deserialize)]
struct AccountConfig {
    #[serde(default)]
    networks: Networks,
}

fn endpoint_of(server: &ServerConfig, default_port: u16) -> Endpoint {
    Endpoint {
        token: server.access_token.clone(),
        host: server
            .host
            .clone()
            .unwrap_or_else(|| "127.0.0.1".to_owned()),
        port: server.port.unwrap_or(default_port),
    }
}

/// 解析一份 per-account 配置 JSON（文件级错误由调用方跳过——单个账号文件坏了
/// 不影响其它账号）。httpServers[0] / wsServers[0] 各取一份，端口默认 3000/3001。
pub fn parse_account_config(uin: &str, json_text: &str) -> Option<AccountEndpoints> {
    let cfg: AccountConfig = serde_json::from_str(json_text).ok()?;
    Some(AccountEndpoints {
        uin: uin.to_owned(),
        http: cfg
            .networks
            .http_servers
            .first()
            .map(|s| endpoint_of(s, 3000)),
        ws: cfg
            .networks
            .ws_servers
            .first()
            .map(|s| endpoint_of(s, 3001)),
    })
}

/// 按账号稳定排序（uin 字典序——QQ 号位数一致时即数值序）。
pub fn sort_accounts(accounts: &mut [AccountEndpoints]) {
    accounts.sort_by(|a, b| a.uin.cmp(&b.uin));
}

/// 发现结果。
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredConnection {
    pub token: String,
    pub base_url: String,
    /// get_login_info 返回的 data（user_id/nickname）。
    pub self_info: Value,
    /// 候选来源（文件名或 config/env）。
    pub source: String,
    /// 同一账号的 WS 端点——桥接走 WS，另一个 token 另一个端口，不能混用；
    /// 只修 HTTP 的话桥接仍然 1006 重连。
    pub ws: Option<Endpoint>,
}

/// 一次尝试的记录（供诊断输出）。
#[derive(Debug, Clone, PartialEq)]
pub struct TryRecord {
    pub source: String,
    pub url: String,
    pub token_head: String,
    pub ok: bool,
}

/// HTTP 探测注入：`(url, token)` → 成功返回 get_login_info 的 data。
pub trait LoginProbe {
    fn probe(&mut self, base_url: &str, token: &str) -> Option<Value>;
}

/// 自动找出一条**能用**的连接。候选顺序：显式配置优先（尊重用户选择），
/// 然后是 per-account 文件里的各账号；逐个真探 get_login_info，谁通用谁。
///
/// 全部 401 时的错误文案与 Node 版一致（指引用户确认登录状态）。
pub fn discover_connection<P: LoginProbe>(
    probe: &mut P,
    explicit_token: Option<&str>,
    base_url: &str,
    accounts: &[AccountEndpoints],
) -> Result<DiscoveredConnection, String> {
    let mut candidates = Vec::<(String, String, String)>::new(); // (token, source, url)
    let mut seen = std::collections::HashSet::new();
    let mut add = |token: String, source: &str, url: String| {
        if token.is_empty() {
            return;
        }
        if seen.insert(format!("{url}|{token}")) {
            candidates.push((token, source.to_owned(), url));
        }
    };
    if let Some(token) = explicit_token {
        add(token.to_owned(), "config/env", base_url.to_owned());
    }
    for account in accounts {
        if let Some(http) = &account.http {
            add(
                http.token.clone(),
                &format!("onebot_{}.json", account.uin),
                format!("http://{}:{}", http.host, http.port),
            );
        }
    }

    let mut tried: Vec<TryRecord> = Vec::new();
    let mut last_401 = false;
    for (token, source, url) in candidates {
        let result = probe.probe(&url, &token);
        let ok = result.is_some();
        tried.push(TryRecord {
            source: source.clone(),
            url: url.clone(),
            token_head: format!("{}…", &token[..token.len().min(4)]),
            ok,
        });
        if let Some(self_info) = result {
            // 找到有效 HTTP token 后把**同一个账号**的 WS 信息一并带回。
            let uin = source
                .strip_prefix("onebot_")
                .and_then(|s| s.strip_suffix(".json"))
                .map(str::to_owned)
                .or_else(|| {
                    self_info
                        .get("user_id")
                        .and_then(Value::as_u64)
                        .map(|id| id.to_string())
                })
                .unwrap_or_default();
            let ws = accounts
                .iter()
                .find(|a| a.uin == uin)
                .and_then(|a| a.ws.clone());
            return Ok(DiscoveredConnection {
                token,
                base_url: url,
                self_info,
                source,
                ws,
            });
        }
        last_401 = true;
    }
    if last_401 {
        Err(
            "所有候选 token 都被拒绝（401）。当前账号的 token 可能还没写进 SnowLuma 的 config：\
请确认 SnowLuma 已登录目标 QQ，且 OneBot HTTP 服务已开启。"
                .to_owned(),
        )
    } else {
        Err(format!("连不上 OneBot HTTP API（{base_url}）"))
    }
}

/// 端点写回 config.json 的纯部分：返回 (新 JSON 文本, 是否有变更, 是否保留 BOM)。
/// 只写传入的字段、其余一律不动；全部一致时不产生变更（幂等）。
/// BOM 必须原样保留——用户文件带 UTF-8 BOM 时改一个 token 就吞掉 BOM 是对
/// 用户文件的字节级静默改动（Node 版实战教训）。
pub fn apply_endpoint_to_config(
    cfg_text: &str,
    access_token: Option<&str>,
    ws_url: Option<&str>,
    http_url: Option<&str>,
) -> Result<(String, bool, bool), String> {
    let had_bom = cfg_text.starts_with('\u{FEFF}');
    let body = cfg_text.trim_start_matches('\u{FEFF}');
    let mut cfg: Value =
        serde_json::from_str(body).map_err(|e| format!("config.json 解析失败：{e}"))?;
    // snowluma 节不存在时先建空对象。
    if cfg.get("snowluma").is_none() {
        cfg["snowluma"] = json!({});
    }
    let mut changed = false;
    for (key, value) in [
        ("accessToken", access_token),
        ("wsUrl", ws_url),
        ("httpUrl", http_url),
    ] {
        let Some(value) = value else { continue };
        if value.is_empty() {
            continue;
        }
        let cur = cfg
            .get("snowluma")
            .and_then(|s| s.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if cur != value {
            changed = true;
            cfg["snowluma"][key] = json!(value);
        }
    }
    if !changed {
        return Ok((cfg_text.to_owned(), false, had_bom));
    }
    let mut serialized = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    serialized.push('\n');
    Ok((
        if had_bom {
            format!("\u{FEFF}{serialized}")
        } else {
            serialized
        },
        true,
        had_bom,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_json(http_token: &str, ws_token: &str, http_port: u16) -> String {
        json!({
            "networks": {
                "httpServers": [{ "name": "http-default", "accessToken": http_token, "host": "127.0.0.1", "port": http_port }],
                "wsServers": [{ "accessToken": ws_token, "port": 3001 }]
            }
        })
        .to_string()
    }

    /// 契约与 Node 版 test-snowluma-conn 一致：只认 onebot_<数字>.json。
    #[test]
    fn account_file_names() {
        assert_eq!(account_file_uin("onebot_111.json"), Some("111"));
        assert_eq!(
            account_file_uin("OneBot_222.JSON"),
            Some("222"),
            "大小写不敏感"
        );
        assert_eq!(account_file_uin("onebot_broken.json"), None);
        assert_eq!(account_file_uin("runtime.json"), None);
        assert_eq!(account_file_uin("onebot_.json"), None);
    }

    /// per-account 解析：双 token 各自取、端口默认 3000/3001、按 uin 排序。
    #[test]
    fn parses_account_configs() {
        let a = parse_account_config("111", &account_json("token-aaa", "ws-111", 3000)).unwrap();
        let b = parse_account_config("222", &account_json("token-bbb", "ws-222", 3000)).unwrap();
        assert_eq!(a.http.as_ref().unwrap().token, "token-aaa");
        assert_eq!(a.ws.as_ref().unwrap().token, "ws-111");
        assert_eq!(a.http.as_ref().unwrap().port, 3000);
        assert_eq!(a.ws.as_ref().unwrap().port, 3001);
        assert_eq!(a.ws.as_ref().unwrap().host, "127.0.0.1", "WS host 默认");
        assert_ne!(a.http.as_ref().unwrap().token, a.ws.as_ref().unwrap().token);
        let mut accounts = vec![b, a];
        sort_accounts(&mut accounts);
        assert_eq!(accounts[0].uin, "111");
        // 坏 JSON 跳过（None）。
        assert!(parse_account_config("333", "{ not json").is_none());
        // 缺 server 节：端点为 None 而不是整体失败。
        let none = parse_account_config("444", "{}").unwrap();
        assert!(none.http.is_none() && none.ws.is_none());
    }

    struct FakeProbe {
        valid_token: &'static str,
    }

    impl LoginProbe for FakeProbe {
        fn probe(&mut self, _url: &str, token: &str) -> Option<Value> {
            (token == self.valid_token).then(|| json!({ "user_id": 222, "nickname": "第二账号" }))
        }
    }

    /// 自动发现：按「谁能用」选择，回报账号与来源，带回同账号 WS 端点。
    #[test]
    fn discovery_picks_working_token() {
        let accounts = vec![
            parse_account_config("111", &account_json("token-aaa", "ws-111", 3000)).unwrap(),
            parse_account_config("222", &account_json("token-bbb", "ws-222", 3001)).unwrap(),
        ];
        let mut probe = FakeProbe {
            valid_token: "token-bbb",
        };
        let found = discover_connection(&mut probe, None, "http://127.0.0.1:3000", &accounts)
            .expect("应发现成功");
        assert_eq!(found.token, "token-bbb");
        assert_eq!(found.self_info["user_id"], 222);
        assert_eq!(found.source, "onebot_222.json");
        assert_eq!(
            found.ws.as_ref().unwrap().token,
            "ws-222",
            "带回同账号 WS token"
        );
        assert_eq!(
            found.base_url, "http://127.0.0.1:3001",
            "候选 URL 用账号自己的端口"
        );
    }

    /// 显式 token 优先（尊重用户选择）；全部失效时报 401 指引文案。
    #[test]
    fn explicit_token_first_and_all_rejected() {
        let accounts =
            vec![parse_account_config("111", &account_json("token-aaa", "ws-111", 3000)).unwrap()];
        let mut probe = FakeProbe {
            valid_token: "explicit-token",
        };
        let found = discover_connection(
            &mut probe,
            Some("explicit-token"),
            "http://127.0.0.1:3000",
            &accounts,
        )
        .unwrap();
        assert_eq!(found.source, "config/env");

        let mut all_bad = FakeProbe {
            valid_token: "nothing",
        };
        let err = discover_connection(&mut all_bad, None, "http://127.0.0.1:3000", &accounts)
            .unwrap_err();
        assert!(err.contains("401"), "错误信息指出 token 问题：{err}");
    }

    /// 写回 config.json：只改该改的字段、幂等、BOM 保留。
    #[test]
    fn endpoint_persist_is_minimal_and_idempotent() {
        let cfg = r#"{
  "snowluma": { "httpUrl": "http://127.0.0.1:3000", "accessToken": "old-token" },
  "allow": { "groups": [1] }
}"#;
        let (next, updated, _) =
            apply_endpoint_to_config(cfg, Some("new-token"), None, None).unwrap();
        assert!(updated);
        let parsed: Value = serde_json::from_str(&next).unwrap();
        assert_eq!(parsed["snowluma"]["accessToken"], "new-token");
        assert_eq!(parsed["allow"]["groups"][0], 1, "其它配置字段未被破坏");
        // 幂等。
        let (again, updated2, _) =
            apply_endpoint_to_config(&next, Some("new-token"), None, None).unwrap();
        assert!(!updated2);
        assert_eq!(again, next);
        // BOM 保留。
        let bom_cfg = format!("\u{FEFF}{cfg}");
        let (bom_next, updated3, had_bom) =
            apply_endpoint_to_config(&bom_cfg, Some("new-token"), None, None).unwrap();
        assert!(updated3 && had_bom);
        assert!(bom_next.starts_with('\u{FEFF}'), "BOM 不被吞掉");
    }
}
