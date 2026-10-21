//! DSH 端口发现（移植自 Node 版 `src/dsh-port-sync.js` 的判定核心与
//! `scripts/test-dsh-port-sync.mjs`，契约同步迁移）。
//!
//! Otter/桌面端用随机端口起 DSH（`dsh web --port 0` / Electron），每次重启都变；
//! 桥接在探活连续失败时自动重发现（规格 §3.3 端口自适应）。
//!
//! 分层调整：PowerShell 进程扫描（[`PORT_SCAN_PS`]）是 Windows 平台 IO，执行在
//! bin 层；本模块只保留候选选择的纯决策。verify 回调由调用方注入——桥接用铸的
//! Cookie 发 modelCatalog 验证业务口，这里只认「true = 可用的 DSH 业务 API 口」。

use url::Url;

/// PowerShell 探测脚本（-EncodedCommand UTF-16LE 传输避免引号转义问题）。
/// 形态一：官方桌面端（DeepSeek Harness.exe，Electron）；形态二：CLI（dsh web）。
/// Electron 应用有多个监听端口（DevTools/内部 RPC 等），这里只能全拿到候选——
/// 哪个是业务 API 口必须由 verify 回调实际探测，不能按大小猜。
pub const PORT_SCAN_PS: &str = r#"
$ProgressPreference = 'SilentlyContinue'
$ports = @()
Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue | Where-Object {
  (Get-Process -Id $_.OwningProcess -ErrorAction SilentlyContinue).ProcessName -match 'DeepSeek'
} | ForEach-Object { $ports += $_.LocalPort }
Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match 'dsh.*bin\.js web' } | ForEach-Object {
  $procId = $_.ProcessId
  Get-NetTCPConnection -OwningProcess $procId -State Listen -ErrorAction SilentlyContinue | ForEach-Object { $ports += $_.LocalPort }
}
$ports | Sort-Object -Unique
"#;

/// 把扫描脚本编码为 `-EncodedCommand` 参数（UTF-16LE + 标准 base64，
/// 避免引号转义问题）；bin 层用 `powershell -NoProfile -EncodedCommand <输出>` 执行。
pub fn encoded_port_scan_command() -> String {
    use base64::Engine;
    let utf16le: Vec<u8> = PORT_SCAN_PS
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    base64::engine::general_purpose::STANDARD.encode(utf16le)
}

/// 在候选端口里找出新的业务 baseUrl：逐个调 `verify(baseUrl)`（返回 true 表示是
/// 可用的 DSH 业务 API 口），第一个通过的生效；与 config 一致或全部失败返回
/// `None`。Electron 杂端口（401/404 的内部口）在 verify 处自然被淘汰。
pub fn discover_dsh_base_url<F>(
    ports: &[u16],
    current_base_url: &str,
    mut verify: F,
) -> Option<String>
where
    F: FnMut(&str) -> bool,
{
    if ports.is_empty() {
        return None;
    }
    let current_port = Url::parse(current_base_url)
        .ok()
        .and_then(|u| u.port())
        .unwrap_or(0);
    for port in ports {
        if *port == current_port {
            continue;
        }
        let base = format!("http://127.0.0.1:{port}");
        if verify(&base) {
            return Some(base);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// 契约与 Node 版 test-dsh-port-sync 一致（verify 注入，不跑 PowerShell）：
    /// 模拟真实场景——Electron 杂端口 verify 失败，业务口通过。
    #[test]
    fn skips_noise_ports_and_picks_business_port() {
        let ports = [18800, 20128, 1707, 20131];
        let good: HashSet<String> = ["http://127.0.0.1:1707"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let verify = |base: &str| good.contains(base);
        assert_eq!(
            discover_dsh_base_url(&ports, "http://127.0.0.1:2075", verify),
            Some("http://127.0.0.1:1707".to_string())
        );
    }

    /// 当前口一致时返回 None（无变更）。
    #[test]
    fn returns_none_when_unchanged() {
        assert_eq!(
            discover_dsh_base_url(&[1707], "http://127.0.0.1:1707", |_| true),
            None
        );
    }

    /// 全部验证失败返回 None。
    #[test]
    fn returns_none_when_all_fail() {
        assert_eq!(
            discover_dsh_base_url(&[18800, 20128], "http://127.0.0.1:2075", |_| false),
            None
        );
    }

    /// verify 抛错按失败处理（同步版等价：verify 侧把异常折成 false）。
    #[test]
    fn verify_errors_count_as_failure() {
        assert_eq!(
            discover_dsh_base_url(&[20128], "http://127.0.0.1:2075", |_| false),
            None
        );
    }

    /// 空候选返回 None。
    #[test]
    fn empty_candidates_return_none() {
        assert_eq!(
            discover_dsh_base_url(&[], "http://127.0.0.1:2075", |_| true),
            None
        );
    }

    /// 非法当前 baseUrl 视为端口 0（不跳过任何候选）。
    #[test]
    fn invalid_current_url_does_not_skip() {
        assert_eq!(
            discover_dsh_base_url(&[1707], "not a url", |_| true),
            Some("http://127.0.0.1:1707".to_string())
        );
    }
}
