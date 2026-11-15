//! 本地 embedding（长期记忆语义检索 M7 二期；规格 §十二 P3）。
//!
//! 用 fastembed 的 candle BERT 后端在 CPU 上跑 `BAAI/bge-small-zh-v1.5`
//!（512 维、中文、MIT 许可），不依赖任何外部服务；模型文件由本模块从国内
//! 可达的 HF 镜像（默认 hf-mirror.com）下载一次后缓存到
//! `state/semantic-cache/`，之后完全离线。
//!
//! **为什么自己下载**：hf-hub 0.5 的下载器按 Range 取元数据，国内镜像对
//! Range 请求回 200 全量且不带 `Content-Range`，hf-hub 直接报
//! `Header Content-Range is missing`。本模块按 hf-hub 的缓存目录布局
//!（`models--<repo>/refs/main` + `snapshots/<revision>/<file>`）自己落盘，
//! 缓存命中后 fastembed 的 `repo.get(...)` 只读本地文件、不再走网络。
//!
//! 失败哲学：embedding 不可用（无网/模型损坏/内存不足）时全部调用返回
//! `Err`，调用方降级关键词检索——长期记忆主链路永不因语义检索失败而阻塞。

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use candle_core::{DType, Device};
use fastembed::BertTextEmbedding;

/// 模型（中文小模型，维度 512；换模型须同步 [`DIM`] 与 [`MODEL_REVISION`]）。
pub const MODEL_ID: &str = "BAAI/bge-small-zh-v1.5";
/// 模型仓库 commit（固定版本保证缓存/向量空间可复现）。
pub const MODEL_REVISION: &str = "7999e1d3359715c523056ef9478215996d62a620";
/// 向量维度（sqlite-vec 表维度必须与之一致）。
pub const DIM: usize = 512;
/// BGE 检索式查询建议前缀（非对称检索；条文来自 BGE 官方说明）。
const QUERY_INSTRUCTION: &str = "为这个句子生成表示以用于检索相关文章：";
const MAX_TOKENS: usize = 512;
const DEFAULT_ENDPOINT: &str = "https://hf-mirror.com";
/// (文件名, SHA-256)——模型缓存完整性校验；镜像/本地文件被篡改或截断时
/// 重新下载一次，仍不符则放弃并在冷却期后重试。
const MODEL_FILES: [(&str, &str); 3] = [
    (
        "config.json",
        "3853a7979202c348751b753e36f579c41d8da7d36af617d3d907e1fc9b441f2a",
    ),
    (
        "tokenizer.json",
        "48cea5d44424912a6fd1ea647bf4fe50b55ab8b1e5879c3275f80e339e8fae26",
    ),
    (
        "model.safetensors",
        "354763b9b1357bc9c44f62c6be2276321081ed2567773608c0d0785b61d5a026",
    ),
];
/// 失败后重试冷却（避免每次工具调用都卡在一次必然失败的下载上）。
const RETRY_COOLDOWN: Duration = Duration::from_secs(600);

static MODEL: Mutex<Option<BertTextEmbedding>> = Mutex::new(None);
static READY: AtomicBool = AtomicBool::new(false);
static LAST_ERROR: Mutex<Option<(Instant, String)>> = Mutex::new(None);

/// 模型缓存目录（`QUEQIAO_HOME/state/semantic-cache`）。
pub fn cache_dir() -> PathBuf {
    crate::paths::queqiao_home()
        .join("state")
        .join("semantic-cache")
}

/// 进程启动最早期调用（main 顶部、tokio 线程起来之前）：把 fastembed /
/// hf-hub 读取的两个 env 落定。缓存已由本模块预先铺好，env 只影响
/// hf-hub 的缓存定位与兜底网络地址。
pub fn init_env() {
    if std::env::var_os("FASTEMBED_CACHE_DIR").is_none() {
        // SAFETY: 在 main 顶部、任何线程/运行时创建之前单线程调用一次。
        unsafe { std::env::set_var("FASTEMBED_CACHE_DIR", cache_dir()) };
    }
    if std::env::var_os("HF_ENDPOINT").is_none() {
        // SAFETY: 同上，单线程启动期。
        unsafe { std::env::set_var("HF_ENDPOINT", DEFAULT_ENDPOINT) };
    }
}

/// 模型是否已加载完成（只读原子量：工具面高频调用，不拿锁）。
pub fn is_ready() -> bool {
    READY.load(Ordering::Relaxed)
}

fn log(msg: &str) {
    crate::logging::log(msg);
}

fn record_error(msg: String) -> String {
    if let Ok(mut e) = LAST_ERROR.lock() {
        *e = Some((Instant::now(), msg.clone()));
    }
    msg
}

/// 预热：下载（首次）并加载模型。幂等；失败记录冷却时间。
pub fn prewarm(endpoint: &str) -> Result<(), String> {
    if READY.load(Ordering::Relaxed) {
        return Ok(());
    }
    let mut guard = MODEL.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        READY.store(true, Ordering::Relaxed);
        return Ok(());
    }
    if let Ok(err) = LAST_ERROR.lock()
        && let Some((at, msg)) = err.as_ref()
        && at.elapsed() < RETRY_COOLDOWN
    {
        return Err(format!(
            "{msg}（冷却中，{}s 后重试）",
            RETRY_COOLDOWN.as_secs()
        ));
    }
    let endpoint = endpoint.trim().trim_end_matches('/');
    let endpoint = if endpoint.is_empty() {
        DEFAULT_ENDPOINT
    } else {
        endpoint
    };
    let started = Instant::now();
    if let Err(e) = prepare_cache(endpoint) {
        return Err(record_error(format!("模型缓存准备失败：{e}")));
    }
    log("[semantic] 加载本地 embedding 模型（bge-small-zh-v1.5，CPU）…");
    let model = match BertTextEmbedding::from_hf(MODEL_ID, &Device::Cpu, DType::F32, MAX_TOKENS) {
        Ok(m) => m,
        Err(e) => return Err(record_error(format!("模型加载失败：{e}"))),
    };
    *guard = Some(model);
    READY.store(true, Ordering::Relaxed);
    log(&format!(
        "[semantic] 本地 embedding 就绪（{MODEL_ID}，dim {DIM}，耗时 {}s）",
        started.elapsed().as_secs()
    ));
    Ok(())
}

/// 铺缓存：按 hf-hub 目录布局落盘（供 fastembed 直接命中，不触发其下载器）。
fn prepare_cache(endpoint: &str) -> Result<(), String> {
    let repo_dir = cache_dir().join("models--BAAI--bge-small-zh-v1.5");
    let snapshot = repo_dir.join("snapshots").join(MODEL_REVISION);
    std::fs::create_dir_all(&snapshot).map_err(|e| format!("建缓存目录失败：{e}"))?;
    let refs = repo_dir.join("refs");
    std::fs::create_dir_all(&refs).map_err(|e| format!("建 refs 失败：{e}"))?;
    let ref_file = refs.join("main");
    let need_ref = std::fs::read_to_string(&ref_file)
        .ok()
        .is_none_or(|v| v.trim() != MODEL_REVISION);
    if need_ref {
        std::fs::write(&ref_file, MODEL_REVISION).map_err(|e| format!("写 refs/main 失败：{e}"))?;
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(1800))
        .build()
        .map_err(|e| format!("HTTP 客户端失败：{e}"))?;
    for (file, sha) in MODEL_FILES {
        let dst = snapshot.join(file);
        let mut ok = false;
        for attempt in 0..2 {
            if verify_sha256(&dst, sha) {
                ok = true;
                break;
            }
            if attempt == 1 {
                break;
            }
            download_model_file(&client, endpoint, file, &dst)?;
        }
        if !ok {
            return Err(format!("{file} 完整性校验失败（SHA-256 不符）"));
        }
    }
    Ok(())
}

fn verify_sha256(path: &std::path::Path, expect: &str) -> bool {
    use sha2::{Digest, Sha256};
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match std::io::Read::read(&mut f, &mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    format!("{:x}", hasher.finalize()) == expect
}

fn download_model_file(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    file: &str,
    dst: &std::path::Path,
) -> Result<(), String> {
    let url = format!("{endpoint}/{MODEL_ID}/resolve/main/{file}");
    log(&format!(
        "[semantic] 下载模型文件 {file}（首次一次性，之后离线）…"
    ));
    let mut resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("下载 {file} 失败：{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载 {file} 失败：HTTP {}", resp.status()));
    }
    let tmp = dst.with_extension("part");
    let mut f =
        std::fs::File::create(&tmp).map_err(|e| format!("创建 {file} 临时文件失败：{e}"))?;
    std::io::copy(&mut resp, &mut f).map_err(|e| format!("写 {file} 失败：{e}"))?;
    let _ = f.sync_all();
    drop(f);
    std::fs::rename(&tmp, dst).map_err(|e| format!("落盘 {file} 失败：{e}"))?;
    let size = std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
    log(&format!("[semantic] {file} 就绪（{size} 字节）"));
    Ok(())
}

/// 文本 → 向量（passage 侧，无指令前缀）。模型未就绪/失败返回 Err。
pub fn embed_passages(texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    let guard = MODEL.lock().unwrap_or_else(|e| e.into_inner());
    let model = guard.as_ref().ok_or("embedding 模型尚未就绪")?;
    model.embed(texts).map_err(|e| e.to_string())
}

/// 查询 → 向量（带 BGE 检索指令前缀；只有 query 侧加）。
pub fn embed_query(query: &str) -> Result<Vec<f32>, String> {
    let text = format!("{QUERY_INSTRUCTION}{query}");
    let mut out = embed_passages(std::slice::from_ref(&text))?;
    out.pop().ok_or_else(|| "embedding 返回空".to_owned())
}

/// 单条 passage（保存长期记忆后异步补向量用）。
pub fn embed_one_passage(text: &str) -> Result<Vec<f32>, String> {
    let mut out = embed_passages(std::slice::from_ref(&text.to_owned()))?;
    out.pop().ok_or_else(|| "embedding 返回空".to_owned())
}

/// 模型目录是否已存在（诊断/日志用）。
pub fn model_dir() -> PathBuf {
    cache_dir()
        .join("models--BAAI--bge-small-zh-v1.5")
        .join("snapshots")
        .join(MODEL_REVISION)
}

/// 测试/诊断：模型文件是否齐全。
pub fn files_ready() -> bool {
    MODEL_FILES
        .iter()
        .all(|(f, _)| std::fs::metadata(model_dir().join(f)).is_ok_and(|m| m.len() > 0))
}

/// 清空模型内存态（测试用；不删缓存文件）。
#[cfg(test)]
pub fn reset_for_test() {
    READY.store(false, Ordering::Relaxed);
    *MODEL.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn cache_paths_are_under_home() {
        let dir = cache_dir();
        assert!(dir.ends_with(Path::new("state").join("semantic-cache")));
        assert!(model_dir().ends_with(Path::new(MODEL_REVISION)));
    }

    #[test]
    fn embed_without_model_is_err_not_panic() {
        reset_for_test();
        assert!(!is_ready());
        assert!(embed_passages(&["x".to_owned()]).is_err());
        assert!(embed_query("x").is_err());
    }

    #[test]
    fn sha256_verification_works() {
        let p = std::env::temp_dir().join(format!("qq-sha256-{}.txt", std::process::id()));
        std::fs::write(&p, "abc").unwrap();
        // SHA-256("abc")
        assert!(verify_sha256(
            &p,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        ));
        assert!(!verify_sha256(&p, "00"));
        assert!(!verify_sha256(&p.with_extension("missing"), "00"));
        let _ = std::fs::remove_file(&p);
    }
}
