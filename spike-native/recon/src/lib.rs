//! 鹊桥 native 注入 spike S1-R1 —— SSO 层侦察 DLL。
//!
//! 加载进 QQ 主进程后（DllMain 内，纯读、不 hook、不写内存）：
//!   1. 枚举全部模块（基址/大小/路径）
//!   2. 在可读数据区搜索一组 SSO 命令名字符串
//!   3. 在包含命中的模块的代码段扫描 RIP 相对 lea 指令，
//!      解出全部 lea 目标，命中字符串地址的记录为候选引用点
//!   4. 报告写 %TEMP%\qq_s1_recon_<pid>.txt
//!
//! 说明：lea 扫描按字节滑窗、不按指令边界对齐，存在把指令中段
//! 误读为 lea 的假阳性——报告仅是候选清单，供后续逐点验证，
//! 这是有意的（漏检比误检更贵）。

#![allow(non_snake_case)]

use core::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HMODULE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, WriteFile, CREATE_ALWAYS, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_WRITE,
    FILE_SHARE_READ,
};
use windows::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, PAGE_PROTECTION_FLAGS};
use windows::Win32::System::ProcessStatus::{
    EnumProcessModulesEx, GetModuleFileNameExW, GetModuleInformation, LIST_MODULES_ALL, MODULEINFO,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId};

const DLL_PROCESS_ATTACH: u32 = 1;

/// 代表性 SSO 命令名/前缀：登录框架、消息收发、推送、oidb、highway、心跳
const TARGETS: &[&str] = &[
    "MessageSvc.PbSendMsg",
    "trpc.msg.olpush.OlPushService.MsgPush",
    "trpc.msg.msg_svc.MsgService.SsoReadedReport",
    "OidbSvcTrpcTcp",
    "wtlogin.login",
    "wtlogin.exchange_emp",
    "HttpConn.0x6ff_501",
    "trpc.qq_new_tech.status_svc.StatusService.SsoHeartBeat",
    "MessageSvc.PbDeleteMsg",
    "ConfigSvc.PushResp",
];

struct ModuleInfo {
    base: usize,
    size: usize,
    name: String,
}

struct Hit {
    addr: usize,
    target_idx: usize,
}

fn wide_to_string(w: &[u16]) -> String {
    let len = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..len])
}

fn page_readable(protect: PAGE_PROTECTION_FLAGS) -> bool {
    let p = protect.0;
    // PAGE_READONLY=2 / READWRITE=4 / EXECUTE_READ=0x20 / EXECUTE_READWRITE=0x40 / WRITECOPY 系
    // PAGE_NOACCESS=1 / GUARD=0x100 → 不可读
    (p & 0x01) == 0 && (p & 0x100) == 0 && (p & 0xEE) != 0
}

fn page_executable(protect: PAGE_PROTECTION_FLAGS) -> bool {
    let p = protect.0;
    (p & 0xF0) != 0 // EXECUTE* 系（0x10/0x20/0x40/0x80）
}

/// 收集 [base, base+size) 内全部可读页的实际提交区域（State=COMMIT）
fn committed_ranges(base: usize, size: usize) -> Vec<(usize, usize, PAGE_PROTECTION_FLAGS)> {
    let mut out = Vec::new();
    let mut addr = base;
    let end = base.wrapping_add(size);
    unsafe {
        while addr < end {
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            if VirtualQuery(Some(addr as *const c_void), &mut mbi, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) == 0 {
                break;
            }
            let region_base = mbi.BaseAddress as usize;
            let region_end = region_base + mbi.RegionSize;
            if mbi.State.0 == 0x1000 && page_readable(mbi.Protect) {
                let lo = region_base.max(base);
                let hi = region_end.min(end);
                if hi > lo {
                    out.push((lo, hi - lo, mbi.Protect));
                }
            }
            addr = region_end;
        }
    }
    out
}

/// 在区域里搜 TARGETS 子串，返回命中
fn scan_strings(data: &[u8], region_addr: usize, hits: &mut Vec<Hit>) {
    for (ti, t) in TARGETS.iter().enumerate() {
        let pat = t.as_bytes();
        if pat.is_empty() || data.len() < pat.len() {
            continue;
        }
        let mut i = 0;
        while i + pat.len() <= data.len() {
            if &data[i..i + pat.len()] == pat {
                hits.push(Hit { addr: region_addr + i, target_idx: ti });
                i += pat.len(); // 不重叠
            } else {
                i += 1;
            }
        }
    }
}

/// 扫描代码区中指向 target 集合的 RIP 相对 lea。
/// 形态 A: [REX.W] 8D /r(mod=00,rm=101) disp32 → 7 字节
/// 形态 B: 8D /r(mod=00,rm=101) disp32 → 6 字节
/// 返回 (lea 所在地址, lea 目标地址)
fn scan_lea(data: &[u8], code_addr: usize, targets: &std::collections::HashSet<usize>) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let n = data.len();
    if n < 8 {
        return out;
    }
    for i in 0..n.saturating_sub(7) {
        let b = data;
        // 形态 A：48/49/4C/4D 8D xx(5号rm) xx xx xx xx
        let b0 = b[i];
        let is_rex = b0 == 0x48 || b0 == 0x49 || b0 == 0x4C || b0 == 0x4D;
        if is_rex && b[i + 1] == 0x8D && (b[i + 2] & 0xC7) == 0x05 {
            let disp = i32::from_le_bytes([b[i + 3], b[i + 4], b[i + 5], b[i + 6]]);
            let target = (code_addr + i + 7).wrapping_add(disp as usize as isize as usize);
            if targets.contains(&target) {
                out.push((code_addr + i, target));
                continue;
            }
        }
        // 形态 B
        if b0 == 0x8D && (b[i + 1] & 0xC7) == 0x05 {
            let disp = i32::from_le_bytes([b[i + 2], b[i + 3], b[i + 4], b[i + 5]]);
            let target = (code_addr + i + 6).wrapping_add(disp as usize as isize as usize);
            if targets.contains(&target) {
                out.push((code_addr + i, target));
            }
        }
    }
    out
}

fn run_recon() {
    unsafe {
        let pid = GetCurrentProcessId();
        let mut report: Vec<u8> = Vec::with_capacity(64 * 1024);

        // 1. 模块清单（真实 SizeOfImage 来自 GetModuleInformation）
        let mut modules: Vec<ModuleInfo> = Vec::new();
        let mut needed = 0u32;
        let mut hmods = vec![HMODULE::default(); 1024];
        let _ = EnumProcessModulesEx(GetCurrentProcess(), hmods.as_mut_ptr(), hmods.len() as u32 * 8, &mut needed, LIST_MODULES_ALL);
        let count = (needed as usize) / std::mem::size_of::<HMODULE>();
        if count >= hmods.len() {
            hmods = vec![HMODULE::default(); count + 64];
            let _ = EnumProcessModulesEx(GetCurrentProcess(), hmods.as_mut_ptr(), hmods.len() as u32 * 8, &mut needed, LIST_MODULES_ALL);
        }
        let count = (needed as usize) / std::mem::size_of::<HMODULE>();
        for i in 0..count {
            let h = hmods[i];
            if h.is_invalid() {
                continue;
            }
            let mut namebuf = [0u16; 1024];
            let len = GetModuleFileNameExW(Some(GetCurrentProcess()), Some(h), &mut namebuf) as usize;
            let path = if len > 0 { wide_to_string(&namebuf[..len]) } else { String::from("?") };
            let mut mi = MODULEINFO::default();
            if GetModuleInformation(GetCurrentProcess(), h, &mut mi, std::mem::size_of::<MODULEINFO>() as u32).is_err() {
                continue;
            }
            modules.push(ModuleInfo { base: mi.lpBaseOfDll as usize, size: mi.SizeOfImage as usize, name: path });
        }

        report.extend_from_slice(format!("qq s1 recon report pid={}\nmodules={}\n\n", pid, modules.len()).as_bytes());

        // 2. 数据区字符串搜索（全模块）
        let mut all_hits: Vec<(usize, Hit, usize)> = Vec::new();
        for (mi, m) in modules.iter().enumerate() {
            for (lo, len, _prot) in committed_ranges(m.base, m.size) {
                let data = std::slice::from_raw_parts(lo as *const u8, len);
                let mut hits: Vec<Hit> = Vec::new();
                scan_strings(data, lo, &mut hits);
                for h in hits {
                    all_hits.push((mi, h, 0));
                }
            }
        }

        report.extend_from_slice(format!("string_hits={}\n", all_hits.len()).as_bytes());
        for (mi, h, _) in &all_hits {
            let m = &modules[*mi];
            report.extend_from_slice(
                format!("HIT {} +0x{:X} addr=0x{:X} in {}\n",
                    TARGETS[h.target_idx], h.addr.wrapping_sub(m.base), h.addr, m.name).as_bytes());
        }

        // 3. 在命中模块的代码段扫 lea 引用
        let hit_modules: std::collections::HashSet<usize> = all_hits.iter().map(|(mi, _, _)| *mi).collect();
        let mut target_addrs: std::collections::HashSet<usize> = all_hits.iter().map(|(_, h, _)| h.addr).collect();
        // 字符串结尾后若干偏移也纳入（lea 可能指向串中间/结尾的 C 字符串变体）
        for (_, h, _) in &all_hits {
            for off in 1..8usize {
                target_addrs.insert(h.addr + off);
            }
        }

        let mut refs: Vec<(usize, usize, usize)> = Vec::new(); // (module_idx, lea_addr, target_addr)
        for &mi in &hit_modules {
            let m = &modules[mi];
            for (lo, len, prot) in committed_ranges(m.base, m.size) {
                if !page_executable(prot) {
                    continue;
                }
                let data = std::slice::from_raw_parts(lo as *const u8, len);
                for (lea, tgt) in scan_lea(data, lo, &target_addrs) {
                    refs.push((mi, lea, tgt));
                }
            }
        }

        report.extend_from_slice(format!("\nlea_refs={}\n", refs.len()).as_bytes());
        for (mi, lea, tgt) in refs.iter().take(4000) {
            let m = &modules[*mi];
            report.extend_from_slice(
                format!("REF {} +0x{:X} lea=0x{:X} -> 0x{:X} (str +0x{:X})\n",
                    short_name(&m.name), lea.wrapping_sub(m.base), lea, tgt,
                    tgt.wrapping_sub(m.base)).as_bytes());
        }
        if refs.len() > 4000 {
            report.extend_from_slice(format!("... ({}/{} 省略)\n", refs.len() - 4000, refs.len()).as_bytes());
        }

        // 4. 写报告
        let mut tmp = [0u16; 260];
        let tl = windows::Win32::Storage::FileSystem::GetTempPathW(Some(&mut tmp)) as usize;
        let mut name_u8 = b"qq_s1_recon_".to_vec();
        let mut pidstr = [0u8; 12];
        let mut pi = pidstr.len();
        let mut v = pid as u64;
        if v == 0 { pi -= 1; pidstr[pi] = b'0'; } else {
            while v > 0 { pi -= 1; pidstr[pi] = b'0' + (v % 10) as u8; v /= 10; }
        }
        name_u8.extend_from_slice(&pidstr[pi..]);
        name_u8.extend_from_slice(b".txt");
        let name_wide: Vec<u16> = name_u8.iter().map(|&b| b as u16).collect();
        let mut path = vec![0u16; tl + name_wide.len() + 1];
        path[..tl].copy_from_slice(&tmp[..tl]);
        path[tl..tl + name_wide.len()].copy_from_slice(&name_wide);

        if let Ok(h) = CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ,
            None,
            CREATE_ALWAYS,
            FILE_FLAGS_AND_ATTRIBUTES(0x80),
            None,
        ) {
            let mut written = 0u32;
            let _ = WriteFile(h, Some(&report), Some(&mut written), None);
            let _ = CloseHandle(h);
        }
    }
}

fn short_name(path: &str) -> String {
    let base = path.rsplit(['\\', '/']).next().unwrap_or(path);
    base.to_string()
}

#[no_mangle]
pub extern "system" fn DllMain(_hinst: *mut c_void, reason: u32, _reserved: *mut c_void) -> u32 {
    if reason == DLL_PROCESS_ATTACH {
        run_recon();
    }
    1
}
