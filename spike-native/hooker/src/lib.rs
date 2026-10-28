//! 鹊桥 native 注入 spike S1-R3 —— int3 观测 hook（MsgPush handler）。
//!
//! 行为：对 wrapper.node 的 MsgPush 回调 trampoline（RVA 0x2665C1E，首字节
//! 为单字节指令 `push rsi`）打 int3 断点。VEH 捕获后 dump 全部寄存器 +
//! rcx 指向的内存快照，然后恢复原字节、置 TF 单步执行原指令、再次进异常
//! 时清 TF 并重新落 0xCC（若未达次数上限）。
//!
//! 安全边界：纯观测（不改函数行为、不改返回值）；最多命中 MAX_HITS 次后
//! 永久摘除；VEH 内不做分配，只用固定缓冲与已加载的 kernel32 调用。

#![allow(non_snake_case)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicBool, Ordering};

use windows::core::PCWSTR;
const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
const EXCEPTION_CONTINUE_SEARCH: i32 = 1;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, WriteFile, CREATE_ALWAYS, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_WRITE, FILE_SHARE_READ,
};
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, FlushInstructionCache, CONTEXT, EXCEPTION_POINTERS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{VirtualProtect, PAGE_PROTECTION_FLAGS, PAGE_EXECUTE_READWRITE};
use windows::Win32::System::ProcessStatus::{EnumProcessModulesEx, GetModuleFileNameExW, LIST_MODULES_ALL};
use windows::Win32::System::Threading::GetCurrentProcessId;

const DLL_PROCESS_ATTACH: u32 = 1;
const TARGET_RVA: usize = 0x731EA6; // SSO 通用请求装配层（push rbp 单字节首指令）
const MAX_HITS: u32 = 6;
const SNAP_BYTES: usize = 96;

static HOOK_ADDR_PTR: AtomicU64 = AtomicU64::new(0); // 完整 64 位地址，VEH 比对用
static ORIG_BYTE: AtomicU32 = AtomicU32::new(0);
static HITS: AtomicU32 = AtomicU32::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);
static PENDING_STEP: AtomicBool = AtomicBool::new(false);

// 命中快照缓冲（固定大小，VEH 内无分配）
#[derive(Clone, Copy)]
struct Snap {
    valid: bool,
    rip: u64, rcx: u64, rdx: u64, r8: u64, r9: u64,
    rax: u64, rbx: u64, rsi: u64, rdi: u64,
    mem_rcx: [u8; SNAP_BYTES],
    mem_rcx_ok: bool,
    // rcx 若像指针（指向低 64 位合法区），跟一层
    mem_ind: [u8; SNAP_BYTES],
    mem_ind_ok: bool,
}
static mut SNAPS: [Snap; MAX_HITS as usize] = [Snap {
    valid: false, rip: 0, rcx: 0, rdx: 0, r8: 0, r9: 0, rax: 0, rbx: 0, rsi: 0, rdi: 0,
    mem_rcx: [0; SNAP_BYTES], mem_rcx_ok: false, mem_ind: [0; SNAP_BYTES], mem_ind_ok: false,
}; MAX_HITS as usize];

unsafe fn read_ok(p: *const c_void, buf: &mut [u8]) -> bool {
    use windows::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION};
    let mut mbi = MEMORY_BASIC_INFORMATION::default();
    if VirtualQuery(Some(p), &mut mbi, core::mem::size_of::<MEMORY_BASIC_INFORMATION>()) == 0 {
        return false;
    }
    if mbi.State.0 != 0x1000 { return false; }
    let prot = mbi.Protect.0;
    if (prot & 0x01) != 0 || (prot & 0x100) != 0 { return false; } // NOACCESS/GUARD
    // 检查 buf 范围都在同区域
    let base = mbi.BaseAddress as usize;
    if p as usize + buf.len() > base + mbi.RegionSize { return false; }
    core::ptr::copy_nonoverlapping(p as *const u8, buf.as_mut_ptr(), buf.len());
    true
}

extern "system" fn veh(info: *mut EXCEPTION_POINTERS) -> i32 {
    unsafe {
        let rec = &*(*info).ExceptionRecord;
        let ctx = &mut *(*info).ContextRecord;
        let code = rec.ExceptionCode.0;
        let hook = HOOK_ADDR_PTR.load(Ordering::Relaxed) as usize;
        if hook == 0 { return EXCEPTION_CONTINUE_SEARCH; }

        if code == 0x80000003u32 as i32 {
            // int3：RIP 指向 int3 之后一字节
            let at = ctx.Rip as usize - 1;
            if at != hook { return EXCEPTION_CONTINUE_SEARCH; }
            let n = HITS.load(Ordering::Relaxed);
            if n < MAX_HITS {
                let s = &mut SNAPS[n as usize];
                s.valid = true;
                s.rip = ctx.Rip; s.rcx = ctx.Rcx; s.rdx = ctx.Rdx;
                s.r8 = ctx.R8; s.r9 = ctx.R9; s.rax = ctx.Rax;
                s.rbx = ctx.Rbx; s.rsi = ctx.Rsi; s.rdi = ctx.Rdi;
                s.mem_rcx_ok = read_ok(ctx.Rcx as *const c_void, &mut s.mem_rcx);
                // 若 rcx 首八字节像用户态指针（0x000000xx_xxxxxxxx），跟一层
                if s.mem_rcx_ok {
                    let mut p = [0u8; 8];
                    core::ptr::copy_nonoverlapping(ctx.Rcx as *const u8, p.as_mut_ptr(), 8);
                    let pv = usize::from_le_bytes(p);
                    if (0x10000..0x7FFF_FFFF_FFFF).contains(&pv) {
                        s.mem_ind_ok = read_ok(pv as *const c_void, &mut s.mem_ind);
                    }
                }
                HITS.store(n + 1, Ordering::Relaxed);
                write_report();
            }
            // 摘除断点（次数满则永久；否则临时，单步后重落）
            let keep = n + 1 < MAX_HITS;
            let p = hook as *mut u8;
            *p = ORIG_BYTE.load(Ordering::Relaxed) as u8;
            let _ = FlushInstructionCache(
                windows::Win32::System::Threading::GetCurrentProcess(),
                Some(p as *const c_void), 1);
            PENDING_STEP.store(keep, Ordering::Relaxed);
            // 回退 RIP 到原指令并置 TF 单步
            ctx.Rip = hook as u64;
            ctx.EFlags |= 0x100;
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        if code == 0x80000004u32 as i32 && PENDING_STEP.load(Ordering::Relaxed) {
            // 单步恢复：RIP 已在原指令之后
            PENDING_STEP.store(false, Ordering::Relaxed);
            ctx.EFlags &= !0x100u32;
            let p = hook as *mut u8;
            *p = 0xCC;
            let _ = FlushInstructionCache(
                windows::Win32::System::Threading::GetCurrentProcess(),
                Some(p as *const c_void), 1);
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        EXCEPTION_CONTINUE_SEARCH
    }
}

unsafe fn find_module_base(name_part: &str) -> Option<usize> {
    let mut needed = 0u32;
    let mut hmods = vec![windows::Win32::Foundation::HMODULE::default(); 1024];
    let _ = EnumProcessModulesEx(
        windows::Win32::System::Threading::GetCurrentProcess(),
        hmods.as_mut_ptr(), hmods.len() as u32 * 8, &mut needed, LIST_MODULES_ALL);
    let count = (needed as usize) / 8;
    for i in 0..count.min(hmods.len()) {
        let mut buf = [0u16; 1024];
        let len = GetModuleFileNameExW(Some(windows::Win32::System::Threading::GetCurrentProcess()), Some(hmods[i]), &mut buf) as usize;
        if len == 0 { continue; }
        let path = String::from_utf16_lossy(&buf[..len]);
        let base = path.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase();
        if base.contains(name_part) {
            return Some(hmods[i].0 as usize);
        }
    }
    None
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 3);
    for (i, c) in b.iter().enumerate() {
        if i % 16 == 0 && i > 0 { s.push('\n'); }
        s.push_str(&format!("{:02X} ", c));
    }
    s
}

unsafe fn write_report() {
    let pid = GetCurrentProcessId();
    let mut tmp = [0u16; 260];
    let tl = windows::Win32::Storage::FileSystem::GetTempPathW(Some(&mut tmp)) as usize;
    let mut name: Vec<u8> = b"qq_s1r3_hook_".to_vec();
    let mut digits = [0u8; 12]; let mut di = digits.len(); let mut v = pid as u64;
    if v == 0 { di -= 1; digits[di] = b'0'; } else { while v > 0 { di -= 1; digits[di] = b'0' + (v % 10) as u8; v /= 10; } }
    name.extend_from_slice(&digits[di..]);
    name.extend_from_slice(b".txt");
    let nw: Vec<u16> = name.iter().map(|&b| b as u16).collect();
    let mut path = vec![0u16; tl + nw.len() + 1];
    path[..tl].copy_from_slice(&tmp[..tl]);
    path[tl..tl + nw.len()].copy_from_slice(&nw);

    let mut body = String::with_capacity(8192);
    body.push_str(&format!("qq s1-r3 hook report pid={}\nhook=wrapper.node+0x{:X} hits={}\n\n",
        pid, TARGET_RVA, HITS.load(Ordering::Relaxed)));
    let snaps = &SNAPS;
    for (i, s) in snaps.iter().enumerate() {
        if !s.valid { continue; }
        body.push_str(&format!("[hit {i}] rip=0x{:X}\n  rcx=0x{:X} rdx=0x{:X} r8=0x{:X} r9=0x{:X}\n  rax=0x{:X} rbx=0x{:X} rsi=0x{:X} rdi=0x{:X}\n",
            s.rip, s.rcx, s.rdx, s.r8, s.r9, s.rax, s.rbx, s.rsi, s.rdi));
        if s.mem_rcx_ok {
            body.push_str(&format!("  [rcx] 0x{:X}:\n{}\n", s.rcx, hex(&s.mem_rcx)));
        }
        if s.mem_ind_ok {
            body.push_str(&format!("  [rcx->ptr]:\n{}\n", hex(&s.mem_ind)));
        }
        body.push('\n');
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
            let base = match find_module_base("wrapper.node") {
                Some(b) => b,
                None => return 1,
            };
            let hook = base + TARGET_RVA;
            let p = hook as *mut u8;
            // ① 先注册 VEH 并发布地址（防裸奔竞态：先有 handler 才能落断点）
            HOOK_ADDR_PTR.store(hook as u64, Ordering::Relaxed);
            let h = AddVectoredExceptionHandler(1, Some(veh));
            if h.is_null() {
                HOOK_ADDR_PTR.store(0, Ordering::Relaxed);
                return 1;
            }
            ARMED.store(true, Ordering::Relaxed);
            // ② 页改 RWX 且保持不恢复——VEH 内反复摘除/重落断点都要写字节
            let mut old = PAGE_PROTECTION_FLAGS(0);
            if VirtualProtect(p as *const c_void, 1, PAGE_EXECUTE_READWRITE, &mut old).is_err() {
                ARMED.store(false, Ordering::Relaxed);
                HOOK_ADDR_PTR.store(0, Ordering::Relaxed);
                return 1;
            }
            // ③ 保存原字节，落 int3
            ORIG_BYTE.store(*p as u32, Ordering::Relaxed);
            *p = 0xCC;
            let _ = FlushInstructionCache(windows::Win32::System::Threading::GetCurrentProcess(), Some(p as *const c_void), 1);
            write_report(); // 先写 armed 状态；每次命中后 veh 会重写完整报告
        }
    }
    1
}
