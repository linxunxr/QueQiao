//! 黑话库 IO（Node loadSlang/saveSlang 语义）：损坏另存 `.corrupt-<ts>` 并
//! Err——上层降级「只读不写」，绝不把空库写回（学到的词条花过 LLM 钱，
//! 不可复现）；保存前 cap 容量、tmp+rename 原子。

use std::path::{Path, PathBuf};

use queqiao_core::slang::{SLANG_MAX_ENTRIES, SlangEntry};

pub fn slang_file(home: &Path) -> PathBuf {
    home.join("state").join("slang.json")
}

pub fn load(file: &Path) -> Result<Vec<SlangEntry>, String> {
    let raw = match std::fs::read_to_string(file) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("黑话库读取失败（{e}）")),
    };
    let trimmed = raw.trim_start_matches('\u{feff}');
    let parsed: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            // 原文另存供人工抢救，再报错——绝不静默按空库继续。
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let backup = file.with_extension(format!("corrupt-{now}"));
            let _ = std::fs::write(&backup, &raw);
            return Err(format!(
                "黑话库内容损坏（{e}），原文已另存 {}，本次按只读不写降级",
                backup.display()
            ));
        }
    };
    let arr = parsed
        .as_array()
        .ok_or_else(|| "黑话库内容损坏（顶层不是数组），本次按只读不写降级".to_string())?;
    Ok(arr
        .iter()
        .filter_map(|v| serde_json::from_value::<SlangEntry>(v.clone()).ok())
        .map(|e| queqiao_core::slang::normalize_entry(&e))
        .collect())
}

pub fn save(file: &Path, entries: Vec<SlangEntry>) -> Result<(), String> {
    let (kept, dropped) = queqiao_core::slang::cap_entries(entries, SLANG_MAX_ENTRIES);
    if dropped > 0 {
        crate::logging::log(&format!(
            "[slang] 容量上限：淘汰 {dropped} 条（按已确认→次数→最近更新保留）"
        ));
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建 state 目录失败：{e}"))?;
    }
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    let body = serde_json::to_string_pretty(&kept).map_err(|e| e.to_string())?;
    let result = std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, file));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("黑话库保存失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 文件系统测试互斥（temp 目录操作偶发竞态，串行化消除）。
    static FS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn fs_guard() -> std::sync::MutexGuard<'static, ()> {
        FS_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn corrupt_backup_not_silent_empty() {
        let _fs = fs_guard();
        let dir = std::env::temp_dir().join(format!("qq-slang-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("slang.json");
        assert_eq!(load(&file).unwrap().len(), 0, "不存在=空库");
        std::fs::write(&file, "{bad").unwrap();
        let err = load(&file).unwrap_err();
        assert!(err.contains("另存"), "{err}");
        // 原文已被抢救到 .corrupt-*。
        let rescued = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains("corrupt-"));
        assert!(rescued);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod real_tests {
    use super::*;

    /// 真机回归（--ignored）：Node 版迁移的真实黑话库注入可消费。
    #[test]
    #[ignore]
    fn real_node_slang() {
        let home = std::path::Path::new(r"D:\100work\103Tools\qq-bridge");
        let file = slang_file(home);
        if !file.exists() {
            eprintln!("跳过：Node 版桥不存在");
            return;
        }
        let entries = load(&file).expect("真实库必须可读");
        eprintln!("库内 {} 条", entries.len());
        let confirmed = entries.iter().filter(|e| e.status == "confirmed").count();
        let ctx = queqiao_core::slang::build_context(&entries, 8);
        eprintln!(
            "已确认 {} 条；注入表：\n{}",
            confirmed,
            ctx.chars().take(200).collect::<String>()
        );
        assert!(!entries.is_empty());
    }
}
