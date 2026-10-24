//! 表情库 IO（Node loadStickerStore/saveStickerStore 语义）：
//! 文件不存在 = 空库（首用正常）；读不动/损坏 = Err（调用方降级只读不写，
//! 防 AI 认知被一次截断/占用清零）；保存 tmp+rename 原子、失败自清残留。

use std::path::{Path, PathBuf};

use queqiao_core::sticker::{StickerEntry, ValueLoose};

pub fn sticker_file(home: &Path) -> PathBuf {
    home.join("state").join("stickers.json")
}

pub fn load(file: &Path) -> Result<Vec<StickerEntry>, String> {
    let raw = match std::fs::read_to_string(file) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("表情库读取失败（{e}）")),
    };
    let trimmed = raw.trim_start_matches('\u{feff}');
    let parsed: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|e| format!("表情库内容损坏（{e}），已拒绝按空库继续以免覆盖已有收藏"))?;
    let arr = parsed
        .as_array()
        .ok_or_else(|| "表情库内容损坏（顶层不是数组），已拒绝按空库继续".to_string())?;
    Ok(arr
        .iter()
        .filter_map(|v| {
            serde_json::from_value::<ValueLoose>(v.clone())
                .ok()
                .and_then(|loose| queqiao_core::sticker::normalize_entry(&loose))
        })
        .collect())
}

pub fn save(file: &Path, entries: &[StickerEntry]) -> Result<(), String> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建 state 目录失败：{e}"))?;
    }
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    let body = serde_json::to_string_pretty(entries).map_err(|e| e.to_string())?;
    let result = std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, file));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("表情库保存失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_corruption_guard() {
        let dir = std::env::temp_dir().join(format!("qq-stk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("stickers.json");
        // 不存在 = 空库。
        assert_eq!(load(&file).unwrap().len(), 0);
        // 保存 → 读回。
        let e = StickerEntry {
            id: "s1".into(),
            desc: "笑".into(),
            ..Default::default()
        };
        save(&file, &[e]).unwrap();
        assert_eq!(load(&file).unwrap().len(), 1);
        // 损坏 = Err（不是空库——防覆盖语义）。
        std::fs::write(&file, "{bad").unwrap();
        assert!(load(&file).is_err());
        // BOM 兼容。
        std::fs::write(&file, "\u{feff}[]").unwrap();
        assert_eq!(load(&file).unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod real_tests {
    use super::*;
    use queqiao_core::sticker::{build_sticker_context, find_sticker};

    /// 真机回归（--ignored 手动跑）：Node 版迁移的真实表情库 22 条即刻可消费。
    #[test]
    #[ignore]
    fn real_node_sticker_store() {
        let home = std::path::Path::new(r"D:\100work\103Tools\qq-bridge");
        let file = sticker_file(home);
        if !file.exists() {
            eprintln!("跳过：Node 版桥不存在");
            return;
        }
        let entries = load(&file).expect("真实库必须可读（损坏即 Err 是设计行为，但真机不该损坏）");
        eprintln!("库内 {} 条", entries.len());
        assert!(!entries.is_empty());
        let ctx = build_sticker_context(&entries, 8);
        eprintln!(
            "注入摘要前 120：{}",
            ctx.chars().take(120).collect::<String>()
        );
        assert!(ctx.starts_with("【可用表情包】你的 QQ 收藏表情里有"));
        // 任取一条按 id 可找到。
        let any = &entries[0];
        assert!(find_sticker(&entries, &any.id).is_some());
    }
}
