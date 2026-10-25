//! 桥日志：stdout + `state/bridge.log` 双写（Node 版 createCappedLogger 语义）。
//!
//! - **行数上限轮转**：内存计数器 O(1) 追加，到 2000 行 rename 到 `.1`（覆盖上个
//!   备份）；rename 失败退化截断，保证不无限增长——不做“写后读回数行”（200KB
//!   时单次 17ms 的教训）；
//! - **单行截断 1000 字**：异常巨大的错误对象不能把日志变成 5MB；
//! - **敏感信息脱敏**：token/密钥类正则打码（Node 版 redactSensitiveText 语义）；
//! - 本地时间 `HH:MM:SS`（不是 UTC——差 8 小时的教训）。
//!
//! 桌面客户端不受影响：它读的是桥进程的 stdout 管道，本模块 stdout 照写。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// 单行最大字符数（超出截断加标记）。
const LOG_LINE_MAX: usize = 1000;
/// 行数上限（到即轮转）。
const LOG_MAX_LINES: usize = 2000;

static LOGGER: Mutex<Option<Logger>> = Mutex::new(None);
static ACTIVITY: Mutex<Option<Logger>> = Mutex::new(None);

struct Logger {
    file: PathBuf,
    /// 内存行数计数（-1 = 未数，首次写入时数一次）。
    lines: i64,
}

/// 会话活动流水（Node 版 qq-activity.log 对应物）：消息入库/唤醒/发言事件，
/// 单独文件便于回溯"AI 当时为什么说这句"。
pub fn activity(event: &str) {
    let mut guard = ACTIVITY.lock().unwrap();
    let Some(logger) = guard.as_mut() else { return };
    let mut line = redact(&event.replace(['\r', '\n'], " "));
    if line.chars().count() > LOG_LINE_MAX {
        let truncated: String = line.chars().take(LOG_LINE_MAX).collect();
        line = format!("{truncated}…[截断]");
    }
    let line = format!(
        "[{}] {line}
",
        local_clock()
    );
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&logger.file)
        && f.write_all(line.as_bytes()).is_ok()
    {
        logger.lines += 1;
        if logger.lines > LOG_MAX_LINES as i64 {
            let backup = logger.file.with_extension("log.1");
            if std::fs::rename(&logger.file, &backup).is_err() {
                let _ = File::create(&logger.file);
            }
            logger.lines = 0;
        }
    }
}

/// 敏感串打码：token/authorization/密钥值的常见形态（与 Node redactSensitiveText
/// 同类语义：宁可多打码不可漏）。命中的值段保留前 4 位 + 长度提示。
fn redact(line: &str) -> String {
    use std::sync::LazyLock;
    static TOKEN_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        // token=xxx / "accessToken":"xxx" / Bearer xxx / authorization: xxx 的值段。
        regex::Regex::new(
            r#"(?i)((?:token|secret|password|authorization|apikey|api_key)["'=:]*\s*["']?)([A-Za-z0-9_\-\.]{8,})"#,
        )
        .expect("脱敏正则必然合法")
    });
    TOKEN_RE
        .replace_all(line, |c: &regex::Captures| {
            let val = c.get(2).map(|m| m.as_str()).unwrap_or_default();
            format!("{}{}…(len {})", &c[1], &val[..val.len().min(4)], val.len())
        })
        .into_owned()
}

/// 本地时间 HH:MM:SS（本地时区；chrono 不引入——从系统 API 取）。
/// 完整本地时间戳 YYYY-MM-DD HH:MM:SS（kernel32 GetLocalTime；时间行注入用）。
pub fn local_timestamp() -> Option<String> {
    #[cfg(windows)]
    {
        #[repr(C)]
        #[derive(Default)]
        struct SystemTime {
            year: u16,
            month: u16,
            day_of_week: u16,
            day: u16,
            hour: u16,
            minute: u16,
            second: u16,
            ms: u16,
        }
        unsafe extern "system" {
            fn GetLocalTime(lpsystemtime: *mut SystemTime);
        }
        let mut st = SystemTime::default();
        unsafe { GetLocalTime(&mut st) };
        Some(format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            st.year, st.month, st.day, st.hour, st.minute, st.second
        ))
    }
    #[cfg(not(windows))]
    None
}

fn local_clock() -> String {
    // 取本地时间：用 PowerShell 太重，直接读 TZ 环境不可靠——Windows 下
    // GetLocalTime 经 kernel32 最准。简化：chrono 已在依赖树里（间接），
    // 但不直接依赖；这里用 std + UTC 偏移探测一次并缓存。
    use std::sync::OnceLock;
    struct LocalNow(fn() -> String);
    static IMPL: OnceLock<LocalNow> = OnceLock::new();
    let f = IMPL.get_or_init(|| {
        // Windows：kernel32!GetLocalTime。
        #[cfg(windows)]
        {
            #[repr(C)]
            #[derive(Default)]
            struct SystemTime {
                year: u16,
                month: u16,
                day_of_week: u16,
                day: u16,
                hour: u16,
                minute: u16,
                second: u16,
                ms: u16,
            }
            unsafe extern "system" {
                fn GetLocalTime(lpsystemtime: *mut SystemTime);
            }
            LocalNow(|| unsafe {
                let mut st = SystemTime::default();
                GetLocalTime(&mut st);
                format!("{:02}:{:02}:{:02}", st.hour, st.minute, st.second)
            })
        }
        #[cfg(not(windows))]
        LocalNow(|| {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            // UTC 近似（非 Windows 非首要目标平台）。
            let h = (secs / 3600) % 24;
            let m = (secs / 60) % 60;
            let s = secs % 60;
            format!("{h:02}:{m:02}:{s:02}")
        })
    });
    (f.0)()
}

/// 初始化日志文件（home/state/bridge.log + activity.log）。重复调用无害。
pub fn init(home: &std::path::Path) {
    let state_dir = home.join("state");
    let _ = std::fs::create_dir_all(&state_dir);
    let make = |name: &str| -> Logger {
        let file = state_dir.join(name);
        let lines = std::fs::read_to_string(&file)
            .map(|t| t.lines().filter(|l| !l.is_empty()).count() as i64)
            .unwrap_or(0);
        Logger { file, lines }
    };
    let mut guard = LOGGER.lock().unwrap();
    if guard.is_none() {
        *guard = Some(make("bridge.log"));
    }
    drop(guard);
    let mut act = ACTIVITY.lock().unwrap();
    if act.is_none() {
        *act = Some(make("activity.log"));
    }
}

/// 崩溃捕获：panic 信息落 bridge.log（默认 panic 只打印到 stderr 一瞬间即崩，
/// 事后无从排查——桌面客户端场景里 stderr 管道读不读得到全凭运气）。
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log(&format!("PANIC: {info}"));
        default(info);
    }));
}

/// 写一条桥日志：文件 + stdout 双写；时间戳、脱敏、截断、轮转都在这里。
///
/// 文件先行、stdout 容错：桥的 stdout 管道连着客户端，客户端退出/被杀后
/// 管道断裂，`println!` 写失败默认 panic，而 panic hook 自身也要打印（二次
/// panic）——真机曾致桥在客户端重启后无痕死亡（文件日志零痕迹，2026-10-25）。
pub fn log(message: &str) {
    let mut guard = LOGGER.lock().unwrap();
    let Some(logger) = guard.as_mut() else { return };
    let mut line = redact(&message.replace(['\r', '\n'], " "));
    if line.chars().count() > LOG_LINE_MAX {
        let truncated: String = line.chars().take(LOG_LINE_MAX).collect();
        line = format!("{truncated}…[截断]");
    }
    let line = format!("[{}] [bridge] {line}\n", local_clock());
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&logger.file)
        && f.write_all(line.as_bytes()).is_ok()
    {
        logger.lines += 1;
        if logger.lines > LOG_MAX_LINES as i64 {
            // 轮转：当前改名 .1（覆盖上个备份）；失败退化截断防无限增长。
            let backup = logger.file.with_extension("log.1");
            if std::fs::rename(&logger.file, &backup).is_err() {
                let _ = File::create(&logger.file);
            }
            logger.lines = 0;
        }
    }
    drop(guard);
    use std::io::Write;
    let _ = writeln!(std::io::stdout(), "{}", line.trim_end());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 截断与脱敏语义。
    #[test]
    fn redact_and_truncate() {
        let s = redact("token=abcdef1234567890 ok");
        assert!(s.contains("abcd…(len 16)"), "token 值打码：{s}");
        assert!(!s.contains("abcdef1234567890"));
        let s = redact(r#"{"accessToken":"XyZ_12345678ab"}"#);
        assert!(!s.contains("XyZ_12345678ab"), "JSON 里的 token 打码：{s}");
        // 长行截断。
        let long = "x".repeat(1500);
        let dir = std::env::temp_dir().join(format!("queqiao-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        init(&dir);
        log(&long);
        let content = std::fs::read_to_string(dir.join("state/bridge.log")).unwrap();
        assert!(content.contains("[截断]"));
        assert!(content.lines().count() == 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
