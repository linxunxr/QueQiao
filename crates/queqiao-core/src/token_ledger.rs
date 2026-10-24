//! 花费记账（Node 版 model-prices.js + token-ledger 的消费面子集）。
//!
//! DSH/provider 只回 token 数不做货币换算——「花多少钱」由桥接用价目表算。
//! 口径（官方一致，元/百万 token）：三桶分别计价再相加（cacheHit 命中 /
//! cacheMiss 未命中 / output 输出；cacheWrite 缺省按未命中价）；空闲单价 ×2
//! 为高峰（北京时间周一至五 9:00-12:00、14:00-18:00，排除法定节假日）。

use std::collections::BTreeMap;

use serde::Deserialize;

// ── 价目表（2026/2027 节假日表随价目版本 2026-11-04）───────────────────────

const PEAK_WINDOWS: [(u8, u8); 2] = [(9, 12), (14, 18)];

const HOLIDAYS_2026: &[&str] = &[
    "2026-01-01",
    "2026-01-02",
    "2026-01-03",
    "2026-02-15",
    "2026-02-16",
    "2026-02-17",
    "2026-02-18",
    "2026-02-19",
    "2026-02-20",
    "2026-02-21",
    "2026-02-22",
    "2026-02-23",
    "2026-04-04",
    "2026-04-05",
    "2026-04-06",
    "2026-05-01",
    "2026-05-02",
    "2026-05-03",
    "2026-05-04",
    "2026-05-05",
    "2026-06-19",
    "2026-06-20",
    "2026-06-21",
    "2026-09-25",
    "2026-09-26",
    "2026-09-27",
    "2026-10-01",
    "2026-10-02",
    "2026-10-03",
    "2026-10-04",
    "2026-10-05",
    "2026-10-06",
    "2026-10-07",
];

const HOLIDAYS_2027: &[&str] = &[
    "2027-01-01",
    "2027-01-02",
    "2027-01-03",
    "2027-02-05",
    "2027-02-06",
    "2027-02-07",
    "2027-02-08",
    "2027-02-09",
    "2027-02-10",
    "2027-02-11",
    "2027-02-12",
    "2027-02-13",
    "2027-04-04",
    "2027-04-05",
    "2027-04-06",
    "2027-05-01",
    "2027-05-02",
    "2027-05-03",
    "2027-05-04",
    "2027-05-05",
    "2027-06-09",
    "2027-06-10",
    "2027-06-11",
    "2027-09-15",
    "2027-09-16",
    "2027-09-17",
    "2027-10-01",
    "2027-10-02",
    "2027-10-03",
    "2027-10-04",
    "2027-10-05",
    "2027-10-06",
    "2027-10-07",
];

/// 单价（空闲时段，元/百万 token；高峰 ×2）。
fn off_price(model: &str) -> (f64, f64, f64) {
    // (cacheHit, cacheMiss|cacheWrite, output)
    if model.contains("pro") {
        (0.15, 4.5, 13.5)
    } else {
        (0.02, 1.0, 4.0)
    }
}

/// 高峰判定：北京时间周一至五 9-12/14-18 且非节假日。
pub fn is_peak_utc(t_ms: u64) -> bool {
    let secs = t_ms / 1000 + 8 * 3600; // 北京时间
    let days = secs / 86400;
    let weekday = ((days + 4) % 7) as u8; // 1970-01-01 是周四 → 1=周一
    if !(1..=5).contains(&weekday) {
        return false;
    }
    let ymd = civil_date_from_days(days);
    let date_str = format!("{:04}-{:02}-{:02}", ymd.0, ymd.1, ymd.2);
    if HOLIDAYS_2026.contains(&date_str.as_str()) || HOLIDAYS_2027.contains(&date_str.as_str()) {
        return false;
    }
    let hour = (secs % 86400 / 3600) as u8;
    PEAK_WINDOWS.iter().any(|&(s, e)| hour >= s && hour < e)
}

fn civil_date_from_days(days: u64) -> (i64, u32, u32) {
    // 朴素推算（days since 1970-01-01；量级 ~2 万天，循环开销可忽略，
    // 换取历法绝对正确——峰谷/节假日判定依赖它）。
    let mut y: i64 = 1970;
    let mut rest = days;
    loop {
        let len = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
            366
        } else {
            365
        };
        if rest < len {
            break;
        }
        rest -= len;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let month_len = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    for (m, &ml) in (1..).zip(month_len.iter()) {
        if rest < ml {
            return (y, m, rest as u32 + 1);
        }
        rest -= ml;
    }
    (y, 12, 31)
}

// ── jsonl 行解析与聚合 ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Buckets {
    #[serde(rename = "cacheMiss", default)]
    pub cache_miss: u64,
    #[serde(rename = "cacheHit", default)]
    pub cache_hit: u64,
    #[serde(rename = "cacheWrite", default)]
    pub cache_write: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub reasoning: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LedgerRow {
    #[serde(rename = "k")]
    pub kind: String,
    #[serde(rename = "t")]
    pub t_ms: u64,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub b: Buckets,
}

/// 单行计费（元）：三桶分别计价；k=base 的行为 0 token（不计费）。
pub fn row_cost(row: &LedgerRow) -> f64 {
    let (hit, miss, out) = off_price(&row.model);
    let peak = if is_peak_utc(row.t_ms) { 2.0 } else { 1.0 };
    let m = 1e6;
    (row.b.cache_hit as f64 * hit
        + (row.b.cache_miss + row.b.cache_write) as f64 * miss
        + row.b.output as f64 * out)
        / m
        * peak
}

#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyAgg {
    pub key: String,
    pub turns: u64,
    pub cache_hit: u64,
    pub cache_miss: u64,
    pub cache_write: u64,
    pub output: u64,
    pub cost: f64,
}

#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAgg {
    pub model: String,
    pub rows: u64,
    pub cost: f64,
}

#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub rows: u64,
    pub turns: u64,
    pub cache_hit: u64,
    pub cache_miss: u64,
    pub cache_write: u64,
    pub output: u64,
    pub cost: f64,
    /// 未知模型按 flash 兜底计价的行数（控制台提示用）。
    pub estimated_rows: u64,
    pub by_key: Vec<KeyAgg>,
    pub by_model: Vec<ModelAgg>,
}

/// 解析 jsonl 全量并聚合。坏行跳过（流水允许尾行半写）。
pub fn summarize(text: &str) -> Summary {
    let mut s = Summary::default();
    let mut by_key: BTreeMap<String, KeyAgg> = BTreeMap::new();
    let mut by_model: BTreeMap<String, ModelAgg> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<LedgerRow>(line) else {
            continue;
        };
        s.rows += 1;
        if row.kind == "turn" {
            s.turns += 1;
        }
        if !row.model.contains("flash") && !row.model.contains("pro") && !row.model.is_empty() {
            s.estimated_rows += 1;
        }
        let cost = row_cost(&row);
        s.cache_hit += row.b.cache_hit;
        s.cache_miss += row.b.cache_miss;
        s.cache_write += row.b.cache_write;
        s.output += row.b.output;
        s.cost += cost;
        let e = by_key.entry(row.key.clone()).or_insert_with(|| KeyAgg {
            key: row.key.clone(),
            ..Default::default()
        });
        if row.kind == "turn" {
            e.turns += 1;
        }
        e.cache_hit += row.b.cache_hit;
        e.cache_miss += row.b.cache_miss;
        e.cache_write += row.b.cache_write;
        e.output += row.b.output;
        e.cost += cost;
        let m = by_model
            .entry(if row.model.is_empty() {
                "(未记模型)".into()
            } else {
                row.model.clone()
            })
            .or_insert_with(|| ModelAgg {
                model: if row.model.is_empty() {
                    "(未记模型)".into()
                } else {
                    row.model.clone()
                },
                ..Default::default()
            });
        m.rows += 1;
        m.cost += cost;
    }
    let mut keys: Vec<KeyAgg> = by_key.into_values().collect();
    keys.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    s.by_key = keys;
    let mut models: Vec<ModelAgg> = by_model.into_values().collect();
    models.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    s.by_model = models;
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_windows_beijing_time() {
        // 北京 2026-10-22 周四 10:00 = UTC 02:00 → 高峰
        assert!(is_peak_utc(1_792_634_400_000));
        // 北京同日 13:00（午休）→ 空闲
        assert!(!is_peak_utc(1_792_645_200_000));
        // 北京 2026-10-24 周六 10:00 → 周末空闲
        assert!(!is_peak_utc(1_792_807_200_000));
        // 北京 2026-10-01 国庆 10:00（周四但节假日）→ 空闲
        assert!(!is_peak_utc(1_790_820_000_000));
    }

    #[test]
    fn cost_three_buckets() {
        // flash 空闲：hit 0.02 / miss 1 / output 4（元/百万）
        let row = LedgerRow {
            kind: "turn".into(),
            t_ms: 1_792_645_200_000, // 非高峰
            key: "group:1".into(),
            model: "deepseek-flash".into(),
            b: Buckets {
                cache_miss: 1_000_000,
                cache_hit: 1_000_000,
                output: 500_000,
                ..Default::default()
            },
        };
        let c = row_cost(&row);
        assert!((c - (0.02 + 1.0 + 2.0)).abs() < 1e-9, "三桶分别计价：{c}");
        // 高峰翻倍
        let peak_row = LedgerRow {
            t_ms: 1_792_634_400_000,
            ..row
        };
        assert!((row_cost(&peak_row) - c * 2.0).abs() < 1e-9);
    }

    #[test]
    fn summarize_jsonl_shape() {
        let text = concat!(
            r#"{"k":"base","t":1791978533278,"sid":"s1","key":"private:1","seq":2,"model":"high-availability","provider":"omniroute","b":{"cacheMiss":0,"cacheHit":0,"cacheWrite":0,"output":0,"reasoning":0}}"#,
            "\n",
            r#"{"k":"turn","t":1791978537290,"sid":"s1","key":"private:1","seq":14,"turn":1,"step":1,"retry":0,"b":{"cacheMiss":17914,"cacheHit":0,"cacheWrite":0,"output":305,"reasoning":0}}"#,
            "\n{broken\n\n",
        );
        let s = summarize(text);
        assert_eq!(s.rows, 2, "坏行跳过");
        assert_eq!(s.turns, 1);
        assert_eq!(s.by_key.len(), 1);
        assert_eq!(s.by_key[0].cache_miss, 17914);
        assert!(s.cost > 0.0);
        assert!(s.estimated_rows >= 1, "未知模型 high-availability 按兜底计");
    }
}

#[cfg(test)]
mod real_tests {
    use super::*;

    /// 真机回归（--ignored 手动跑）：Node 版迁移的真实 token-usage.jsonl 聚合。
    #[test]
    #[ignore]
    fn real_node_ledger() {
        let file = std::path::Path::new(r"D:\100work\103Tools\qq-bridge\state\token-usage.jsonl");
        if !file.exists() {
            eprintln!("跳过：Node 版桥不存在");
            return;
        }
        let text = std::fs::read_to_string(file).unwrap();
        let s = summarize(&text);
        eprintln!(
            "行 {} 回合 {} 花费 ¥{:.4} 会话 {} 模型 {}",
            s.rows,
            s.turns,
            s.cost,
            s.by_key.len(),
            s.by_model.len()
        );
        assert!(s.rows > 0);
        assert!(s.cost > 0.0);
        for k in &s.by_key {
            eprintln!("  {} turns={} ¥{:.4}", k.key, k.turns, k.cost);
        }
    }
}
