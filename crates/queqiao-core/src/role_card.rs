//! 人格卡（roles/<名称>.md）解析与「按仿真模式取用」（Node 版 role-card.js
//! 与 bridge.js currentRoleHintV2 的纯函数转译）。
//!
//! 语义契约（与 Node 版一致，M5 对拍项）：
//! - 小节按 Markdown 标题切分；标题里的模式标记（`〔二代〕`/`[v2]`/
//!   `（reserved2）` 等写法）决定该节只在对应模式注入；无标记 = 两种模式都注。
//! - 标记在注入前从标题剥离——模型看到的始终是干净标题。
//! - 取用先筛模式、**再**截断（另一模式的内容不占注入预算）。
//! - V2 装配兜底（给没打标记的老卡片）：一代专属指令行整行过滤
//!   （GEN1_ROLE_LINE_RE）；「回复示例」节内的中文空格分条改写为逗号停顿。

use regex::Regex;

/// 人格名净化：仅保留字母/数字/下划线/中文/横线（Node sanitizeRoleName 同款）。
/// 返回 None 表示含路径穿越或为空——调用方必须拒绝读取。
pub fn sanitize_role_name(name: &str) -> Option<String> {
    let cleaned: String = name.chars().filter(|c| char_allowed(*c)).collect();
    if cleaned.is_empty() || cleaned.contains("..") {
        None
    } else {
        Some(cleaned)
    }
}

fn char_allowed(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || ('\u{4e00}'..='\u{9fff}').contains(&c)
}

/// 名字是否**原样**合法（区别于 [sanitize_role_name] 的过滤清洗）：
/// 编辑/新建人格卡的用户起名场景用——非法字符直接判不合法，避免
/// 「a/b」被静默清洗落盘成「ab」、与用户输入不一致。
pub fn role_name_is_valid(name: &str) -> bool {
    !name.is_empty() && !name.contains("..") && name.chars().all(char_allowed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SectionMode {
    All,
    V1,
    V2,
}

#[derive(Debug, Clone)]
pub struct RoleSection {
    pub mode: SectionMode,
    /// None = 文件开头无标题的部分。
    pub heading: Option<String>,
    pub lines: Vec<String>,
}

fn detect_mode(heading: &str) -> SectionMode {
    let lower = heading.to_lowercase();
    let v2 = ["二代", "2代", "2 代", "v2", "reserved2"];
    let v1 = ["一代", "1代", "1 代", "v1", "reserved"];
    let tagged = |tags: &[&str], heading: &str| -> bool {
        // 括号形态 [〔（( … ]〕）) 之内允许空白（Node 正则 \s* 的宽松子集）。
        for (open, close) in [('[', ']'), ('〔', '〕'), ('（', '）'), ('(', ')')] {
            let Some(start) = heading.find(open) else {
                continue;
            };
            let Some(end_rel) = heading[start + open.len_utf8()..].find(close) else {
                continue;
            };
            let end = start + open.len_utf8() + end_rel;
            let inner = heading[start + open.len_utf8()..end].trim().to_lowercase();
            if tags
                .iter()
                .any(|t| inner == *t || inner.replace(' ', "") == *t)
            {
                return true;
            }
        }
        false
    };
    if tagged(&v2, &lower) {
        SectionMode::V2
    } else if tagged(&v1, &lower) {
        SectionMode::V1
    } else {
        SectionMode::All
    }
}

fn strip_mode_tags(heading: &str) -> String {
    // detect 已确保标记成对出现；这里把所有成对括号内匹配 v1/v2 词的整段剥掉。
    let mut out = String::with_capacity(heading.len());
    let chars: Vec<char> = heading.chars().collect();
    let pairs = [('[', ']'), ('〔', '〕'), ('（', '）'), ('(', ')')];
    let v2 = ["二代", "2代", "v2", "reserved2"];
    let v1 = ["一代", "1代", "v1", "reserved"];
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let mut consumed_tag = false;
        for (open, close) in pairs {
            if c != open {
                continue;
            }
            if let Some(end) = chars[i + 1..].iter().position(|&x| x == close) {
                let inner: String = chars[i + 1..i + 1 + end]
                    .iter()
                    .collect::<String>()
                    .trim()
                    .to_lowercase()
                    .replace(' ', "");
                if v2.contains(&inner.as_str()) || v1.contains(&inner.as_str()) {
                    i = i + 1 + end + 1;
                    consumed_tag = true;
                    break;
                }
            }
        }
        if !consumed_tag {
            out.push(c);
            i += 1;
        }
    }
    // 尾部空白收掉、连续空白压一（Node：replace(/[ \t]+$/,'').replace(/[ \t]{2,}/g,' ').trim()）。
    let collapsed: String = out
        .split('\t')
        .map(|seg| seg.split("  ").collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\t");
    collapsed.trim_end().trim().to_string()
}

/// 按 Markdown 标题（#~######）切小节并解析每节的模式。
pub fn parse_role_sections(text: &str) -> Vec<RoleSection> {
    let heading_re = Regex::new(r"^(#{1,6})\s+(.*)$").unwrap();
    let mut sections: Vec<RoleSection> = Vec::new();
    let mut current: Option<usize> = None;
    for line in text.split('\n') {
        if let Some(caps) = heading_re.captures(line) {
            let hashes = caps.get(1).unwrap().as_str();
            let raw_title = caps.get(2).unwrap().as_str();
            let mode = detect_mode(raw_title);
            let title = strip_mode_tags(raw_title);
            sections.push(RoleSection {
                mode,
                heading: Some(title.clone()),
                lines: vec![format!("{hashes} {title}")],
            });
            current = Some(sections.len() - 1);
        } else {
            let idx = match current {
                Some(i) => i,
                None => {
                    sections.push(RoleSection {
                        mode: SectionMode::All,
                        heading: None,
                        lines: Vec::new(),
                    });
                    current = Some(sections.len() - 1);
                    current.unwrap()
                }
            };
            sections[idx].lines.push(line.to_string());
        }
    }
    sections
}

/// 取某模式注入的人格文本（已去模式标记；三连空行压缩为两行）。
pub fn select_role_text(text: &str, mode: SectionMode) -> String {
    let joined = parse_role_sections(text)
        .into_iter()
        .filter(|s| s.mode == SectionMode::All || s.mode == mode)
        .map(|s| s.lines.join("\n"))
        .collect::<Vec<_>>()
        .join("\n");
    compress_blank_lines(&joined).trim().to_string()
}

fn compress_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0;
    for line in s.split('\n') {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_end_matches('\n').to_string()
}

/// 一代仿真遗留指令判定（GEN1_ROLE_LINE_RE /i 同源）——未打模式标记的老卡
/// 在二代注入时整行过滤。
pub fn is_gen1_line(line: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)\[SILENT\]|空格分隔|按空格|用空格|空格分句|空格代表|自动转发|回复会自动")
            .unwrap()
    });
    re.is_match(line)
}

fn is_cjk_like(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF | 0x3000..=0x303F)
}

fn no_replace(c: char) -> bool {
    matches!(
        c,
        '/' | '\\'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '"'
            | '\''
            | '<'
            | '>'
            | '|'
            | '&'
            | '='
            | ':'
            | ';'
            | ','
            | '.'
            | '。'
            | '，'
            | '、'
    )
}

/// 「回复示例」节内：中文语境的空格分条改写为逗号停顿（Node
/// convertExampleSpacesToComma 同源——空格两侧任一为中文且两侧都不是
/// 排除符号时替换）。
pub fn convert_example_spaces_to_comma(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == ' ' || c == '\t' {
            let prev = if i > 0 { Some(chars[i - 1]) } else { None };
            let next = chars.get(i + 1).copied();
            let prev_cjk = prev.is_some_and(is_cjk_like);
            let next_cjk = next.is_some_and(is_cjk_like);
            let prev_blocked = prev.is_some_and(no_replace);
            let next_blocked = next.is_some_and(no_replace);
            if (prev_cjk || next_cjk) && !prev_blocked && !next_blocked {
                out.push('，');
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// V2 仿真注入装配（Node currentRoleHintV2 同源）：模式筛小节 → 截断 →
/// 老卡兜底（一代指令行过滤 + 回复示例节空格转逗号）。
pub fn role_hint_v2(text: &str, max_chars: usize) -> String {
    let selected = select_role_text(text, SectionMode::V2);
    if selected.is_empty() {
        return String::new();
    }
    let truncated: String = selected.chars().take(max_chars).collect();
    let mut kept: Vec<String> = Vec::new();
    let mut in_example = false;
    let example_re = Regex::new(r"^##\s*.*回复示例").unwrap();
    let section_re = Regex::new(r"^##\s").unwrap();
    for line in truncated.split('\n') {
        if example_re.is_match(line) {
            in_example = true;
            kept.push(line.replace(
                "（空格代表前后分两条消息回答）",
                "（示例中已用逗号表示停顿；想分多条请用数组）",
            ));
            continue;
        }
        if in_example && section_re.is_match(line) {
            in_example = false;
        }
        if in_example {
            kept.push(convert_example_spaces_to_comma(line));
        } else if !is_gen1_line(line) {
            kept.push(line.to_string());
        }
    }
    kept.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# 你是谁\n你是小鲸鱼，喜欢玩平板。\n\n## 表情包体系 〔二代〕\n用 qq_send_sticker 工具发表情。\n\n## 分条方式 〔一代〕\n用空格分句，不想说就 [SILENT]\n\n## 回复示例（空格代表前后分两条消息回答）\n今天天气真好 啊\n";

    #[test]
    fn sanitize_allows_cjk_and_word() {
        assert_eq!(sanitize_role_name("小鲸鱼"), Some("小鲸鱼".into()));
        assert_eq!(
            sanitize_role_name("blue-whale_2"),
            Some("blue-whale_2".into())
        );
        // Node 语义：'.' 不在白名单，净化即消除穿越（'..' 检查在 Node 恒假，防御写法）。
        assert_eq!(sanitize_role_name("../escape"), Some("escape".into()));
        assert_eq!(sanitize_role_name("///"), None, "净化后为空必须拒绝");
    }

    /// 严格校验与过滤清洗的语义分界：清洗把非法字符剥掉改名放行，
    /// 校验原样不合法即拒——编辑/新建人格卡（用户起名）用后者，防
    /// 「a/b」静默落盘成「ab」。
    #[test]
    fn role_name_is_valid_rejects_instead_of_renaming() {
        assert!(role_name_is_valid("小鲸鱼"));
        assert!(role_name_is_valid("blue-whale_2"));
        assert!(!role_name_is_valid("../escape"), "含非法字符即拒（清洗会改名放行）");
        assert!(!role_name_is_valid("a/b"));
        assert!(!role_name_is_valid("a..b"), "显式穿越模式");
        assert!(!role_name_is_valid(""));
        assert!(!role_name_is_valid("！！！"));
    }

    #[test]
    fn mode_tags_split_sections() {
        let v2 = select_role_text(SAMPLE, SectionMode::V2);
        assert!(v2.contains("表情包体系"));
        assert!(v2.contains("你是谁"), "无标记节两种模式都注入");
        assert!(!v2.contains("空格分句"), "一代专属节不进 v2");
        assert!(!v2.contains("〔二代〕"), "模式标记注入前剥离");
        let v1 = select_role_text(SAMPLE, SectionMode::V1);
        assert!(v1.contains("空格分句"));
        assert!(!v1.contains("qq_send_sticker"));
    }

    #[test]
    fn v2_hint_filters_gen1_and_converts_examples() {
        let hint = role_hint_v2(SAMPLE, 6000);
        assert!(!hint.contains("[SILENT]"), "一代指令行兜底过滤");
        assert!(!hint.contains("用空格分句"));
        assert!(
            hint.contains("（示例中已用逗号表示停顿；想分多条请用数组）"),
            "示例节标题改写"
        );
        assert!(hint.contains("今天天气真好，啊"), "示例内中文空格转逗号");
    }

    #[test]
    fn v2_hint_truncates_after_mode_select() {
        let long = format!("{}\n{}", "# 你是谁 〔二代〕", "长".repeat(100));
        let hint = role_hint_v2(&long, 10);
        assert!(hint.chars().count() <= 12, "截断在模式筛选之后");
    }

    #[test]
    fn tag_variants_recognized() {
        for tag in ["[v2]", "〔二代〕", "（reserved2）", "(2 代)"] {
            let text = format!("# A {tag}\n内容\n");
            assert!(
                select_role_text(&text, SectionMode::V2).contains("内容"),
                "{tag} 应识别为 v2"
            );
            assert!(
                select_role_text(&text, SectionMode::V1).is_empty(),
                "{tag} 节不进 v1"
            );
        }
    }

    #[test]
    fn blank_lines_compressed() {
        let text = "# A\nx\n\n\n\ny";
        assert!(select_role_text(text, SectionMode::V1).contains("x\n\ny"));
    }
}
