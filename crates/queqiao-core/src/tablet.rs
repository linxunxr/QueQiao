//! 鸿蒙平板控制纯函数（Node `mcp-tablet.js` 决策面子集）。
//!
//! hdc 执行/文件传输在 bin（`queqiao::tablet`）；这里只放可测的判定与文本
//! 加工：dumpLayout 的 UI 树压缩（200KB+ JSON → 每行一个有意义节点）、
//! bounds 解析、hdc 输出失败嗅探、键名/方向校验。

/// bounds `"[x1,y1][x2,y2]"` → `[x1,y1,x2,y2]`（不匹配返回 None）。
pub fn parse_bounds(bounds: &str) -> Option<[u32; 4]> {
    let b = bounds.trim();
    let rest = b.strip_prefix('[')?;
    let (p1, rest) = rest.split_once(']')?;
    let rest = rest.strip_prefix('[')?;
    let (p2, _) = rest.split_once(']')?;
    let mut out = [0u32; 4];
    let mut it = p1.split(',').chain(p2.split(','));
    for slot in out.iter_mut() {
        *slot = it.next()?.trim().parse().ok()?;
    }
    it.next().is_none().then_some(out)
}

/// hdc 输出失败嗅探（Node failSniff）：`[Fail]/failed/Error opening/No such
/// file` 判失败，`No Error` 豁免；返回前 3 行拼接的失败摘要。
pub fn hdc_output_failed(stdout: &str, stderr: &str) -> Option<String> {
    let text = format!("{stdout}\n{stderr}");
    let bad = text.contains("[Fail]")
        || text.to_lowercase().contains("failed")
        || text.contains("Error opening")
        || text.contains("No such file");
    if !bad || stdout.contains("No Error") {
        return None;
    }
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(3)
        .collect();
    Some(if lines.is_empty() {
        "hdc 命令失败".to_owned()
    } else {
        lines.join(" | ")
    })
}

/// 按键名校验（Node key schema）：Back / Home / Power 或 1~5 位数字键码。
pub fn valid_key(key: &str) -> bool {
    matches!(key, "Back" | "Home" | "Power")
        || (1..=5).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_digit())
}

/// 快滑方向 → uitest dircFling 的编号（left=0 right=1 up=2 down=3）。
pub fn fling_direction(dir: &str) -> Option<u8> {
    match dir {
        "left" => Some(0),
        "right" => Some(1),
        "up" => Some(2),
        "down" => Some(3),
        _ => None,
    }
}

/// dumpLayout 的 JSON 压缩（Node condenseLayout 语义）：每行一个有意义
/// 节点（有文字/id/描述，或可点/可滚/可长按/有选中态，或窗口根带
/// abilityName），缩进树 + 前置坐标；超 `max_chars` 截断并注明。
pub fn condense_layout(root: &serde_json::Value, max_chars: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut truncated = false;
    walk_layout(root, 0, &mut lines, max_chars, &mut truncated);
    let mut text = lines.join("\n");
    if text.chars().count() > max_chars {
        text = text.chars().take(max_chars).collect();
        truncated = true;
    }
    if truncated {
        text.push_str("\n…（已截断：元素过多。可用 tablet_screenshot 看整体，或缩小关注区域）");
    }
    text
}

fn walk_layout(
    node: &serde_json::Value,
    depth: usize,
    lines: &mut Vec<String>,
    max_chars: usize,
    truncated: &mut bool,
) {
    if *truncated {
        return;
    }
    if lines.join("\n").chars().count() > max_chars {
        *truncated = true;
        return;
    }
    let Some(obj) = node.as_object() else { return };
    let a = obj.get("attributes").and_then(|v| v.as_object());
    let attr = |k: &str| {
        a.and_then(|m| m.get(k))
            .and_then(|v| v.as_str())
            .unwrap_or("")
    };
    let rect = parse_bounds(attr("bounds"));
    let flags = [
        ("可点", attr("clickable") == "true"),
        ("可滚", attr("scrollable") == "true"),
        ("长按", attr("longClickable") == "true"),
        ("选中", attr("selected") == "true"),
        ("勾选", attr("checked") == "true"),
        ("聚焦", attr("focused") == "true"),
    ]
    .iter()
    .filter(|(_, on)| *on)
    .map(|(name, _)| *name)
    .collect::<Vec<_>>()
    .join(",");
    let text = attr("text");
    let id = attr("id");
    let desc = attr("description");
    let ty = attr("type");
    let ability = attr("abilityName");
    if !ability.is_empty() {
        let coords = rect
            .map(|r| format!(" [{},{},{},{}]", r[0], r[1], r[2], r[3]))
            .unwrap_or_default();
        lines.push(format!("{}◇ 窗口 {ability}{coords}", "  ".repeat(depth)));
    }
    let mut bits = Vec::new();
    if !text.is_empty() {
        bits.push(format!("text=\"{}\"", clip_chars(text, 120)));
    }
    if !id.is_empty() {
        bits.push(format!("id={id}"));
    }
    if !desc.is_empty() {
        bits.push(format!("desc=\"{}\"", clip_chars(desc, 60)));
    }
    if text.is_empty() && id.is_empty() && !ty.is_empty() {
        bits.push(format!("type={ty}"));
    }
    if !bits.is_empty() || !flags.is_empty() {
        let coords = rect
            .map(|r| format!("{},{},{},{}", r[0], r[1], r[2], r[3]))
            .unwrap_or_else(|| "?".to_owned());
        let line = format!(
            "{}[{}]{} {}",
            "  ".repeat(depth),
            coords,
            if flags.is_empty() {
                String::new()
            } else {
                format!(" <{flags}>")
            },
            bits.join(" ")
        );
        lines.push(clip_chars(line.trim(), 240).to_owned());
    }
    if let Some(children) = obj.get("children").and_then(|v| v.as_array()) {
        for child in children {
            walk_layout(child, depth + 1, lines, max_chars, truncated);
        }
    }
}

fn clip_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bounds_parsing() {
        assert_eq!(parse_bounds("[10,20][110,220]"), Some([10, 20, 110, 220]));
        assert_eq!(parse_bounds(" [0,0][1,1] "), Some([0, 0, 1, 1]));
        assert_eq!(parse_bounds("garbage"), None);
        assert_eq!(parse_bounds("[1,2]"), None);
    }

    #[test]
    fn failure_sniffing() {
        assert_eq!(hdc_output_failed("ok", ""), None);
        assert_eq!(hdc_output_failed("No Error happen", ""), None);
        assert!(
            hdc_output_failed("[Fail]Cannot connect", "")
                .unwrap()
                .contains("[Fail]")
        );
        assert!(hdc_output_failed("", "Error opening connection").is_some());
    }

    #[test]
    fn key_and_direction_validation() {
        assert!(valid_key("Back") && valid_key("Home") && valid_key("Power"));
        assert!(valid_key("2") && valid_key("12345"));
        assert!(!valid_key("back"));
        assert!(!valid_key("123456"));
        assert!(!valid_key("VolumeUp"));
        assert_eq!(fling_direction("up"), Some(2));
        assert_eq!(fling_direction("left"), Some(0));
        assert_eq!(fling_direction("diagonal"), None);
    }

    /// UI 树压缩：窗口根带 abilityName、有文字/可点元素成行、截断注明。
    #[test]
    fn layout_condense_and_truncate() {
        let root = json!({
            "attributes": { "abilityName": "com.example.Main", "bounds": "[0,0][800,1200]" },
            "children": [
                { "attributes": { "text": "视频标题", "id": "title", "bounds": "[0,100][800,160]", "clickable": "true" } },
                { "attributes": { "type": "Column", "bounds": "[0,200][800,400]" } },
                { "attributes": { "text": "评论内容", "bounds": "[0,200][800,260]" } },
                { "attributes": { "bounds": "[0,300][800,360]" } }
            ]
        });
        let out = condense_layout(&root, 18_000);
        assert!(out.contains("◇ 窗口 com.example.Main [0,0,800,1200]"));
        assert!(out.contains("text=\"视频标题\""));
        assert!(out.contains("id=title"));
        assert!(out.contains("<可点>"));
        assert!(out.contains("text=\"评论内容\""));
        assert!(
            out.contains("type=Column"),
            "无文字无 id 的容器按 Node 语义带 type 兜底显示"
        );
        assert!(!out.contains("…（已截断"), "远小于上限不应截断");
        // 纯坐标节点（无 bits 无 flags）不出现。
        assert!(!out.contains("[0,300,800,360]"));
        // 超小上限触发截断注明。
        let tiny = condense_layout(&root, 40);
        assert!(tiny.contains("…（已截断"));
    }
}
