//! 鹊桥 native 注入 spike S1-R3b —— 页 GUARD 读断点（找 MsgPush dispatch）。
//!
//! 对 wrapper.node 内两份 "trpc.msg.olpush.OlPushService.MsgPush" 字符串
//! 所在页（RVA 0x4296000 / 0x443A000）加 PAGE_GUARD。任何线程读该页触发
//! STATUS_GUARD_PAGE_VIOLATION，VEH 记录异常 RIP（读字符串的指令）——
//! dispatch 查表比对命令名时必然读这份字符串，RIP 即 dispatch 代码位置。
//!
//! guard 异常是 one-shot（触发后系统自动清除 GUARD 位，本次访问放行）。
//! VEH 记录后设 TF 单步：访问完成后的单步异常里清 TF 并重设 GUARD
//! （未达 MAX_HITS 上限），形成 capture loop。纯观测，不改任何行为。

#![allow(non_snake_case)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicBool, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, WriteFile, CREATE_ALWAYS, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_WRITE, FILE_SHARE_READ,
};
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, FlushInstructionCache, EXCEPTION_POINTERS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{VirtualProtect, PAGE_GUARD, PAGE_PROTECTION_FLAGS};
use windows::Win32::System::ProcessStatus::{EnumProcessModulesEx, GetModuleFileNameExW, LIST_MODULES_ALL};
use windows::Win32::System::Threading::GetCurrentProcessId;

const DLL_PROCESS_ATTACH: u32 = 1;
const MAX_HITS: u32 = 16;

/// 两份 MsgPush 字符串所在页（页对齐 RVA）
const WATCH_RVAS: [usize; 2] = [0x4296000, 0x443A000];
const WATCH_STR_RVA: usize = 0x42961F0; // 供报告标注

static BASE: AtomicU64 = AtomicU64::new(0);
static HITS: AtomicU32 = AtomicU32::new(0);
static PENDING_STEP: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
struct Snap { valid: bool, rip: u64, access: usize, addr: u64, rcx: u64, rdx: u64, r8: u64, r9: u64, rax: u64 }
static mut SNAPS: [Snap; MAX_HITS as usize] = [Snap { valid: false, rip: 0, access: 0, addr: 0, rcx: 0, rdx: 0, r8: 0, r9: 0, rax: 0 }; MAX_HITS as usize];

unsafe fn watch_page_of(addr: usize) -> Option<usize> {
    let base = BASE.load(Ordering::Relaxed) as usize;
    if base == 0 { return None; }
    for r in WATCH_RVAS {
        let p = base + r;
        if addr >= p && addr < p + 0x1000 { return Some(p); }
    }
    None
}

unsafe fn set_guard(page: usize) -> bool {
    let mut old = PAGE_PROTECTION_FLAGS(0);
    if VirtualProtect(page as *const c_void, 0x1000, PAGE_PROTECTION_FLAGS(0x02 | PAGE_GUARD.0), &mut old).is_err() {
        return false;
    }
    true
}

extern "system" fn veh(info: *mut EXCEPTION_POINTERS) -> i32 {
    unsafe {
        let rec = &*(*info).ExceptionRecord;
        let ctx = &mut *(*info).ContextRecord;
        let code = rec.ExceptionCode.0;
        if code == 0x80000001u32 as i32 {
            // GUARD_PAGE: [0]=访问类型(0读/1写/8执行) [1]=目标地址
            let addr = rec.ExceptionInformation[1] as usize;
            let page = match watch_page_of(addr) { Some(p) => p, None => return 1 };
            let n = HITS.load(Ordering::Relaxed);
            if n < MAX_HITS {
                let s = &mut SNAPS[n as usize];
                s.valid = true;
                s.rip = ctx.Rip; s.access = rec.ExceptionInformation[0];
                s.addr = rec.ExceptionInformation[1] as u64;
                s.rcx = ctx.Rcx; s.rdx = ctx.Rdx; s.r8 = ctx.R8; s.r9 = ctx.R9; s.rax = ctx.Rax;
                HITS.store(n + 1, Ordering::Relaxed);
                write_report();
            }
            // guard 已被系统清除 → 本次访问将放行；设 TF，访问完成后重设 guard
            if n + 1 < MAX_HITS {
                PENDING_STEP.store(true, Ordering::Relaxed);
                ctx.EFlags |= 0x100;
            }
            return -1; // EXCEPTION_CONTINUE_EXECUTION
        }
        if code == 0x80000004u32 as i32 && PENDING_STEP.load(Ordering::Relaxed) {
            PENDING_STEP.store(false, Ordering::Relaxed);
            ctx.EFlags &= !0x100u32;
            // 重设两个 watch 页的 GUARD
            let base = BASE.load(Ordering::Relaxed) as usize;
            for r in WATCH_RVAS { set_guard(base + r); }
            return -1;
        }
        1 // EXCEPTION_CONTINUE_SEARCH
    }
}

unsafe fn find_module_base(part: &str) -> Option<usize> {
    let mut needed = 0u32;
    let mut hmods = vec![windows::Win32::Foundation::HMODULE::default(); 1024];
    let _ = EnumProcessModulesEx(windows::Win32::System::Threading::GetCurrentProcess(),
        hmods.as_mut_ptr(), hmods.len() as u32 * 8, &mut needed, LIST_MODULES_ALL);
    let count = (needed as usize) / 8;
    for i in 0..count.min(hmods.len()) {
        let mut buf = [0u16; 1024];
        let len = GetModuleFileNameExW(Some(windows::Win32::System::Threading::GetCurrentProcess()), Some(hmods[i]), &mut buf) as usize;
        if len == 0 { continue; }
        let path = String::from_utf16_lossy(&buf[..len]);
        if path.rsplit(['\\','/']).next().unwrap_or("").to_lowercase().contains(part) {
            return Some(hmods[i].0 as usize);
        }
    }
    None
}

unsafe fn write_report() {
    let pid = GetCurrentProcessId();
    let mut tmp = [0u16; 260];
    let tl = windows::Win32::Storage::FileSystem::GetTempPathW(Some(&mut tmp)) as usize;
    let mut name: Vec<u8> = b"qq_s1r3_guard_".to_vec();
    let mut digits = [0u8; 12]; let mut di = digits.len(); let mut v = pid as u64;
    if v == 0 { di -= 1; digits[di] = b'0'; } else { while v > 0 { di -= 1; digits[di] = b'0' + (v % 10) as u8; v /= 10; } }
    name.extend_from_slice(&digits[di..]);
    name.extend_from_slice(b".txt");
    let nw: Vec<u16> = name.iter().map(|&b| b as u16).collect();
    let mut path = vec![0u16; tl + nw.len() + 1];
    path[..tl].copy_from_slice(&tmp[..tl]);
    path[tl..tl+nw.len()].copy_from_slice(&nw);

    let base = BASE.load(Ordering::Relaxed) as usize;
    let mut body = String::with_capacity(4096);
    body.push_str(&format!("qq s1-r3b guard report pid={}\nbase=0x{:X} watch={:?} str_rva=0x{:X}\nhits={}\n\n",
        pid, base, WATCH_RVAS, WATCH_STR_RVA, HITS.load(Ordering::Relaxed)));
    for (i, s) in SNAPS.iter().enumerate() {
        if !s.valid { continue; }
        let base64 = base as u64; let rva = if base > 0 && s.rip >= base64 { format!("wrapper.node+0x{:X}", s.rip - base as u64) } else { format!("0x{:X}", s.rip) };
        body.push_str(&format!("[hit {i}] rip={} (访问类型{} @0x{:X})\n  rcx=0x{:X} rdx=0x{:X} r8=0x{:X} r9=0x{:X} rax=0x{:X}\n",
            rva, s.access, s.addr, s.rcx, s.rdx, s.r8, s.r9, s.rax));
    }
    if let Ok(h) = CreateFileW(PCWSTR(path.as_ptr()), FILE_GENERIC_WRITE.0, FILE_SHARE_READ,
        None, CREATE_ALWAYS, FILE_FLAGS_AND_ATTRIBUTES(0x80), None) {
        let mut w = 0u32;
        let _ = WriteFile(h, Some(body.as_bytes()), Some(&mut w), None);
        let _ = CloseHandle(h);
    }
}

#[no_mangle]
pub extern "system" fn DllMain(_h: *mut c_void, reason: u32, _r: *mut c_void) -> u32 {
    if reason == DLL_PROCESS_ATTACH {
        unsafe {
            let base = match find_module_base("wrapper.node") { Some(b) => b, None => return 1 };
            BASE.store(base as u64, Ordering::Relaxed);
            // 先注册 VEH 再设 GUARD
            if AddVectoredExceptionHandler(1, Some(veh)).is_null() { return 1; }
            for r in WATCH_RVAS { set_guard(base + r); }
            write_report();
        }
    }
    1
}
