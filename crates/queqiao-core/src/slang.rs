//! 群聊黑话库纯函数（Node 版 slang-learner.js 消费面子集）——归一化/容量
//! 上限/注入表构造。
//!
//! 安全语义：词条文本会被逐字注入提示词——`[CQ:` 中文化（防 CQ 码伪造）+
//! HTML 实体转义（防标签注入）；容量上限防"只涨不裁"（词条按
//! 已确认→出现次数→最近更新 保留）。

pub const SLANG_MAX_ENTRIES: usize = 2000;

/// 单行化 + 控制字符剔除 + 截断（Node clip / escapeLearnerText 子集）。
fn clean(value: &str, max: usize) -> String {
    let stripped: String = value
        .chars()
        .filter(|c| {
            let code = *c as u32;
            !(matches!(code, 0..=0x8 | 0xb | 0xc | 0xe..=0x1f | 0x7f))
        })
        .collect();
    let trimmed = stripped.trim();
    let mut out: String = trimmed
        .chars()
        .take(max)
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&#39;".into(),
            _ => c.to_string(),
        })
        .collect();
    // CQ 码伪造防护：半角 [CQ: 中文化（转义后 &[ 不受影响，单独处理）。
    if out.contains("[CQ:") {
        out = out.replace("[CQ:", "[CQ：");
    }
    out
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SlangEvidence {
    pub key: String,
    pub sender: String,
    #[serde(rename = "text")]
    pub text: String,
    pub time: u64,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SlangEntry {
    pub id: String,
    pub content: String,
    pub meaning: String,
    pub usage: String,
    pub example: String,
    pub risk: String,
    pub sources: Vec<String>,
    /// candidate / confirmed / rejected。
    pub status: String,
    /// manual / ai。
    pub source: String,
    pub count: u64,
    #[serde(default)]
    pub evidence: Vec<SlangEvidence>,
    pub created_at: String,
    pub updated_at: String,
}

/// 归一化（Node normalizeSlangEntry）：字段截断、status 白名单、证据限 20 条。
pub fn normalize_entry(raw: &SlangEntry) -> SlangEntry {
    let status = match raw.status.as_str() {
        "confirmed" => "confirmed",
        "rejected" => "rejected",
        _ => "candidate",
    };
    SlangEntry {
        id: if raw.id.is_empty() {
            new_id()
        } else {
            raw.id.clone()
        },
        content: clip_plain(&raw.content, 50),
        meaning: clip_plain(&raw.meaning, 300),
        usage: clip_plain(&raw.usage, 300),
        example: clip_plain(&raw.example, 300),
        risk: clip_plain(&raw.risk, 300),
        sources: raw
            .sources
            .iter()
            .map(|s| clip_plain(s, 300))
            .filter(|s| !s.is_empty())
            .rev()
            .take(10)
            .collect(),
        status: status.into(),
        source: if raw.source == "manual" {
            "manual"
        } else {
            "ai"
        }
        .into(),
        count: raw.count,
        evidence: raw
            .evidence
            .iter()
            .take(20)
            .map(|e| SlangEvidence {
                key: clip_plain(&e.key, 100),
                sender: clip_plain(&e.sender, 50),
                text: clip_plain(&e.text, 80),
                time: e.time,
            })
            .collect(),
        created_at: raw.created_at.clone(),
        updated_at: raw.updated_at.clone(),
    }
}

fn clip_plain(s: &str, max: usize) -> String {
    let stripped: String = s
        .chars()
        .filter(|c| {
            let code = *c as u32;
            !(matches!(code, 0..=0x8 | 0xb | 0xc | 0xe..=0x1f | 0x7f))
        })
        .collect();
    stripped.trim().chars().take(max).collect()
}

fn new_id() -> String {
    // 时间戳 36 进制 + 随机后缀（Node createId 同构；随机源由调用方注入的
    // 场景在 bin——core 只在归一化兜底时生成）。
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    format!("{now:x}")
}

/// 容量上限（Node capSlangEntries）：已确认→出现次数→最近更新 保留。
pub fn cap_entries(entries: Vec<SlangEntry>, max: usize) -> (Vec<SlangEntry>, usize) {
    if entries.len() <= max {
        return (entries, 0);
    }
    let mut sorted = entries;
    sorted.sort_by(|a, b| {
        rank(b)
            .cmp(&rank(a))
            .then(b.count.cmp(&a.count))
            .then(b.updated_at.cmp(&a.updated_at))
    });
    let dropped = sorted.len() - max;
    sorted.truncate(max);
    (sorted, dropped)
}

fn rank(e: &SlangEntry) -> u8 {
    match e.status.as_str() {
        "confirmed" => 2,
        "candidate" => 1,
        _ => 0,
    }
}

/// 注入表（Node buildSlangContext）：已确认且有释义，按出现次数排序取 max；
/// 词条转义 + CQ 防伪；无可用词条返回空。
pub fn build_context(entries: &[SlangEntry], max: usize) -> String {
    let mut confirmed: Vec<&SlangEntry> = entries
        .iter()
        .filter(|e| e.status == "confirmed" && !e.content.is_empty() && !e.meaning.is_empty())
        .collect();
    confirmed.sort_by_key(|e| std::cmp::Reverse(e.count));
    let take = max.clamp(1, 30);
    let top = &confirmed[..take.min(confirmed.len())];
    if top.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = top
        .iter()
        .map(|e| {
            let mut line = format!("- {}：{}", clean(&e.content, 50), clean(&e.meaning, 300));
            if !e.usage.is_empty() {
                line.push_str(&format!("（用法：{}）", clean(&e.usage, 300)));
            }
            if !e.example.is_empty() {
                line.push_str(&format!("（例：{}）", clean(&e.example, 300)));
            }
            line
        })
        .collect();
    format!(
        "【群聊黑话表】群里已确认/常用的网络用语和梗（按出现次数排序，知道即可，不要刻意堆砌）：\n{}",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(content: &str, meaning: &str, status: &str, count: u64) -> SlangEntry {
        SlangEntry {
            id: format!("e-{content}"),
            content: content.into(),
            meaning: meaning.into(),
            status: status.into(),
            count,
            ..Default::default()
        }
    }

    #[test]
    fn context_filters_and_sorts() {
        let list = vec![
            entry("甲", "含义A", "confirmed", 3),
            entry("乙", "含义B", "confirmed", 9),
            entry("丙", "", "confirmed", 99), // 无释义不注入
            entry("丁", "候选中", "candidate", 50),
        ];
        let ctx = build_context(&list, 8);
        assert!(ctx.starts_with("【群聊黑话表】"));
        let first = ctx.lines().nth(1).unwrap();
        assert!(first.contains("乙"), "按 count 降序：{first}");
        assert!(ctx.contains("甲"));
        assert!(!ctx.contains("丙"));
        assert!(!ctx.contains("候选中"), "candidate 不注入");
    }

    #[test]
    fn cq_and_html_escaped() {
        let e = entry("[CQ:image,file=x]", "<b>&注入</b>", "confirmed", 1);
        let ctx = build_context(&[e], 8);
        assert!(ctx.contains("[CQ：image"), "CQ 码中文化防伪造");
        assert!(ctx.contains("&lt;b&gt;"), "HTML 转义");
        assert!(!ctx.contains("<b>"));
    }

    #[test]
    fn cap_keeps_confirmed_first() {
        let mut list = Vec::new();
        for i in 0..10 {
            list.push(entry(&format!("c{i}"), "m", "confirmed", i as u64));
        }
        for i in 0..5 {
            list.push(entry(&format!("x{i}"), "m", "rejected", 99));
        }
        let (kept, dropped) = cap_entries(list, 10);
        assert_eq!(kept.len(), 10);
        assert_eq!(dropped, 5);
        assert!(
            kept.iter().all(|e| e.status == "confirmed"),
            "rejected 先被淘汰"
        );
    }
}
