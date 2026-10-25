//! 出站文本敏感审计（Node `src/sensitive.js` SENSITIVE_RE 语义移植）。
//!
//! 启发式内容闸门：拦"长得像本机路径 / 像凭据赋值"的出站文本，不能代替
//! 会话权限控制。凭据关键词必须带赋值关系（`[:=：]` 或"是/为"）或后随
//! ≥3 位连续 ASCII 值才判定，避免误伤"这个 token 用得妙"类正常聊天。
//!
//! 与 [`redact_known_tokens`] 的精确名单是**有意互补**的两道闸门：裸 32 位
//! 十六进制会话令牌既不是路径也没有赋值形态，正则刻意不抓——表情 md5、
//! 图片 URL、消息里的十六进制串本身就是 32-hex，正则抓了会把正常表情包
//! 整条拦掉（误伤面远大于收益），令牌泄漏走精确名单那条路。

use std::sync::OnceLock;

use regex::Regex;

static SENSITIVE: OnceLock<Regex> = OnceLock::new();

fn sensitive_re() -> &'static Regex {
    SENSITIVE.get_or_init(|| {
        Regex::new(
            r#"(?xi)
            (?:
                # Windows 盘符路径（前一个字符不能是字母数字，防 base64 尾接盘符误报）
                (?: ^ | [^A-Za-z0-9] ) [A-Za-z]:[\\/][^\s"'<>|]+
                # UNC：\\host\share（主机与共享名之间单个反斜杠）
                | \\\\[^\\\s"'<>|]+\\[^\s"'<>|]+
                # Unix 家目录/系统目录前缀
                | (?: ^ | [^A-Za-z0-9] ) (?: /home/ | /Users/ | /etc/ | /var/ | /root/ ) [^\s"'<>|]*
                # 凭据关键词 + 赋值关系，或后随 ≥3 位 ASCII 值（"Bearer yJxx…"）
                | (?: token | 密码 | 密钥 | 口令 | password | passwd | secret
                    | api[_-]?key | authorization | bearer | access[_-]?key | credential )
                  ["']? (?: \s* (?: [:=：] | 是 | 为 ) \s* ["']? [^\s，。；、"']{3,}
                      | \s+ [A-Za-z0-9_\-./]{3,} )
            )"#,
        )
        .expect("SENSITIVE_RE 静态合法")
    })
}

/// 出站文本是否命中敏感特征（本机路径 / 凭据赋值）。
pub fn is_sensitive(text: &str) -> bool {
    sensitive_re().is_match(text)
}

/// 把命中敏感特征的片段替换为 `***`（Node `redactSensitiveText` 语义——
/// 日志/反馈/活动记录等**入库前**打码，与出站整条拦截互补）。
pub fn redact_sensitive(text: &str) -> String {
    let re = sensitive_re();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for m in re.find_iter(text) {
        out.push_str(&text[last..m.start()]);
        out.push_str("***");
        last = m.end();
    }
    out.push_str(&text[last..]);
    out
}

/// 已知令牌原文逐个替换为 `***`（Node `redactKnownTokensOnly` 语义）。
/// 空令牌跳过；调用方传当前全部会话的 agent token。
pub fn redact_known_tokens<'a>(
    text: &str,
    tokens: impl IntoIterator<Item = &'a str>,
) -> String {
    let mut s = text.to_owned();
    for token in tokens {
        if !token.is_empty() && s.contains(token) {
            s = s.replace(token, "***");
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_paths_hit() {
        assert!(is_sensitive(r"看下 C:\Users\linxun\secret.txt"));
        assert!(is_sensitive("日志在 C:/temp/log.txt 里"));
        assert!(is_sensitive(r"开头即路径 D:\work\a.txt"));
    }

    #[test]
    fn windows_path_prefixed_by_alnum_misses() {
        // 盘符前是字母数字（base64 尾接等）不算路径——Node lookbehind 同语义。
        assert!(!is_sensitive("ABC:\\foo"));
        assert!(!is_sensitive("12345:678"));
    }

    #[test]
    fn unc_paths_hit() {
        assert!(is_sensitive(r"在 \\\\server\\share\\file.txt"));
        assert!(is_sensitive(r"\\\\nas\\public\\装机包.zip"));
    }

    #[test]
    fn unix_paths_hit() {
        assert!(is_sensitive("配置在 /home/user/.ssh/id_rsa"));
        assert!(is_sensitive("看 /etc/passwd 内容"));
        assert!(is_sensitive("/var/log/syslog 出错了"));
    }

    #[test]
    fn credential_assignment_hits() {
        assert!(is_sensitive("我的密码：abc123xyz"));
        assert!(is_sensitive("password=hunter2222"));
        assert!(is_sensitive("api_key: sk-abcdef123"));
        assert!(is_sensitive("token 是 zzz123xxx"));
        assert!(is_sensitive("密钥为 k-9f8e7d6c"));
        assert!(is_sensitive("Authorization: Bearer eyJhbGciOiJIUzI1NiJ9"));
    }

    #[test]
    fn credential_without_value_misses() {
        // 关键词无赋值关系不判定，防误伤正常聊天。
        assert!(!is_sensitive("这个 token 用得妙"));
        assert!(!is_sensitive("密码忘了怎么办"));
        assert!(!is_sensitive("api key 很好用"));
        assert!(!is_sensitive("什么是 bearer"));
    }

    #[test]
    fn bare_hex_and_plain_chat_miss() {
        // 裸 32-hex（表情 md5 / 图片 URL / 会话令牌）两道闸门都不归正则管。
        assert!(!is_sensitive("md5=0123456789abcdef0123456789abcdef"));
        assert!(!is_sensitive("今天天气不错，出来吃饭吗"));
        assert!(!is_sensitive("C 语言入门推荐一下"));
    }

    #[test]
    fn redact_replaces_known_tokens_only() {
        let out = redact_known_tokens(
            "令牌 aaaa1111bbbb2222 泄漏了，正常话不受影响",
            ["aaaa1111bbbb2222", ""],
        );
        assert_eq!(out, "令牌 *** 泄漏了，正常话不受影响");
        let unchanged = redact_known_tokens("没有令牌的话", ["zzzz9999"]);
        assert_eq!(unchanged, "没有令牌的话");
    }

    #[test]
    fn redact_sensitive_masks_hits_only() {
        let out = redact_sensitive("密码是 abc123xyz，其余照旧 C:\\Users\\a\\k.txt 尾部");
        assert!(!out.contains("abc123xyz"));
        assert!(!out.contains("Users"));
        assert!(out.contains("其余照旧"));
        assert!(out.ends_with("尾部"));
        assert_eq!(redact_sensitive("干净文本"), "干净文本");
    }
}
