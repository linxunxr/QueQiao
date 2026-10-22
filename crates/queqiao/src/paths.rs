//! 桥 home 定位（CWD 无关）——DSH spawn 的 MCP 进程工作目录不确定，
//! 所有文件路径必须基于 home 解析：
//! ①`QUEQIAO_HOME` 环境变量（preset 里可注入）；②可执行文件所在目录
//!（部署布局：exe 与 config.json/state/ 同目录）；③当前目录（开发期回退）。

use std::path::PathBuf;

pub fn queqiao_home() -> PathBuf {
    if let Ok(home) = std::env::var("QUEQIAO_HOME") {
        return PathBuf::from(home);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        return dir.to_path_buf();
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn config_path() -> PathBuf {
    queqiao_home().join("config.json")
}

pub fn state_dir() -> PathBuf {
    queqiao_home().join("state")
}

pub fn sessions_path() -> PathBuf {
    state_dir().join("sessions.json")
}

pub fn social_v2_path() -> PathBuf {
    state_dir().join("social-v2.json")
}

pub fn diary_path() -> PathBuf {
    state_dir().join("diary.jsonl")
}

/// 会话工作目录（sessionCwd 为空时用 state/agents）。
pub fn agents_dir(session_cwd: &str) -> PathBuf {
    if session_cwd.is_empty() {
        state_dir().join("agents")
    } else {
        PathBuf::from(session_cwd)
    }
}
