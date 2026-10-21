//! OneBot `file` 字段安全白名单（移植自 Node 版 bridge.js 的
//! `isProbablySafeImageFileRef` / `isPathInside`，防路径穿越/SSRF/任意文件读取）。

/// OneBot 的 file 字段只应接受**简单缓存文件名**：拒绝路径分隔符、盘符、协议
/// 前缀、控制字符、`..` 与全部非白名单字符——防止把任意本地路径/内网 URL 交给
/// 网关 `get_image` 造成 SSRF/任意文件读取。
///
/// 白名单字符集与 Node 版正则 `^[\w.+=@-]+$` 一致（`\w` 按 JS 语义取 ASCII：
/// 字母/数字/下划线）。
pub fn is_safe_cache_file_ref(file: &str) -> bool {
    let s = file.trim();
    if s.is_empty() || s.len() > 512 {
        return false;
    }
    if s.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return false;
    }
    if s.contains('/') || s.contains('\\') {
        return false;
    }
    // 盘符（C:）。
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    if ["file:", "http:", "https:", "base64:", "data:"]
        .iter()
        .any(|p| lower.starts_with(p))
    {
        return false;
    }
    if s.contains("..") {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '=' | '@' | '-'))
}

/// 本地路径是否限定在 SnowLuma home 之内（字符串级规范化前缀检查；
/// 符号链接的 realpath 解析由 IO 层做——这里是快速预检）。
pub fn is_within_home(path: &str, home: &str) -> bool {
    let norm = |p: &str| {
        p.replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    let (np, nh) = (norm(path), norm(home));
    np == nh || np.starts_with(&format!("{nh}\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 白名单：合法缓存文件名通过；路径/URL/盘符/穿越/控制字符全拒。
    #[test]
    fn cache_file_ref_whitelist() {
        for good in [
            "ABC123.image",
            "cache_file-1.jpg",
            "a+b=c@def.mp4",
            "  spaced.png  ", // trim 后合法
        ] {
            assert!(is_safe_cache_file_ref(good), "应通过：{good:?}");
        }
        for bad in [
            "",
            "a/b.jpg",
            "a\\b.jpg",
            "C:\\data\\x.jpg",
            "c:x",
            "file:///etc/passwd",
            "https://example.com/a.jpg",
            "http://127.0.0.1:3000/x",
            "base64://AAAA",
            "data:image/png;base64,AAAA",
            "..%2Fx",
            "..\\x",
            "x..y",      // 含 ..（保守：Node 版同款 includes('..') 判定）
            "名 图.png", // 空格外的非白名单字符
            "x.png\u{0}",
            &"a".repeat(513),
        ] {
            assert!(!is_safe_cache_file_ref(bad), "应拒绝：{bad:?}");
        }
    }

    /// home 限定：混用分隔符与大小写在 Windows 语义下仍正确。
    #[test]
    fn home_containment() {
        let home = "D:\\Tools\\SnowLuma";
        assert!(is_within_home(
            "D:\\Tools\\SnowLuma\\data\\cache\\a.jpg",
            home
        ));
        assert!(is_within_home("D:/Tools/SnowLuma/data/cache/a.jpg", home));
        assert!(is_within_home(home, home));
        assert!(
            !is_within_home("D:\\Tools\\SnowLumaOther\\a.jpg", home),
            "前缀相似不算包含"
        );
        assert!(!is_within_home("E:\\x.jpg", home));
    }
}
