//! 鹊桥 native 注入 spike S0 —— DLL 注入器。
//!
//! 经典 CreateRemoteThread + LoadLibraryW 手法：依赖「系统 DLL 同一 boot
//! 会话内各进程基址一致」的前提，把本进程 kernel32!LoadLibraryW 的地址
//! 作为目标线程入口，参数为写入目标进程的 DLL 路径宽字符串。
//!
//! 安全纪律（见 spike 计划文档）：--pid 必须显式指定；--list 仅枚举不注入；
//! 注入前打印目标核对信息，防止误注入主力 bot 实例。

use std::env;
use std::process::exit;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeThread, OpenProcess, WaitForSingleObject,
    PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ,
    PROCESS_VM_WRITE,
};

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn usage() -> ! {
    eprintln!("用法:");
    eprintln!("  injector.exe --list                    枚举 QQ.exe 进程（不注入）");
    eprintln!("  injector.exe --pid <N> --dll <路径>    注入指定进程");
    exit(2)
}

fn list_qq_processes() {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .expect("CreateToolhelp32Snapshot 失败");
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_err() {
            eprintln!("Process32FirstW 失败");
            CloseHandle(snap);
            return;
        }
        println!("QQ.exe 进程列表（注入前请用以下命令核对哪个是小号实例）:");
        println!("  wmic process where name=\"QQ.exe\" get ProcessId,CommandLine");
        let mut found = 0;
        loop {
            let name_len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);
            if name.eq_ignore_ascii_case("QQ.exe") {
                println!("  PID {}", entry.th32ProcessID);
                found += 1;
            }
            if Process32NextW(snap, &mut entry).is_err() {
                break;
            }
        }
        let _ = CloseHandle(snap);
        if found == 0 {
            println!("  （未发现 QQ.exe）");
        }
    }
}

fn inject(pid: u32, dll_path: &str) {
    let dll_abs = std::fs::canonicalize(dll_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| {
            eprintln!("DLL 路径不存在或无法解析: {dll_path}");
            exit(1)
        });
    if !dll_abs.to_lowercase().ends_with(".dll") {
        eprintln!("目标不是 .dll: {dll_abs}");
        exit(1);
    }
    let wide = to_wide(&dll_abs);
    let wide_bytes = wide.len() * std::mem::size_of::<u16>();

    unsafe {
        // 打开目标进程：注入所需最小权限集合
        let process = OpenProcess(
            PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION
                | PROCESS_VM_WRITE | PROCESS_VM_READ,
            false,
            pid,
        )
        .unwrap_or_else(|e| {
            eprintln!("OpenProcess({pid}) 失败: {e}（进程不存在或权限不足，试试管理员运行）");
            exit(1)
        });

        // 目标进程内分配并写入 DLL 路径
        let remote_mem = VirtualAllocEx(
            process,
            None,
            wide_bytes,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if remote_mem.is_null() {
            eprintln!("VirtualAllocEx 失败");
            let _ = CloseHandle(process);
            exit(1);
        }
        let mut written = 0usize;
        let ok = WriteProcessMemory(
            process,
            remote_mem,
            wide.as_ptr() as *const core::ffi::c_void,
            wide_bytes,
            Some(&mut written),
        )
        .is_ok();
        if !ok || written != wide_bytes {
            eprintln!("WriteProcessMemory 失败（written={written}）");
            VirtualFreeEx(process, remote_mem, 0, MEM_RELEASE);
            let _ = CloseHandle(process);
            exit(1);
        }

        // kernel32!LoadLibraryW —— 同 boot 会话各进程基址一致
        let kernel32 = GetModuleHandleW(PCWSTR(to_wide("kernel32.dll").as_ptr()))
            .expect("GetModuleHandleW(kernel32) 失败");
        let load_library = GetProcAddress(kernel32, windows::core::PCSTR(b"LoadLibraryW\0".as_ptr()))
            .expect("GetProcAddress(LoadLibraryW) 失败");
        // LPTHREAD_START_ROUTINE 形状（返回 u32）：LoadLibraryW 返回的 HMODULE 被截断，
        // 仅用于非零判定；成功与否以 proof 文件为准。
        let load_library: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32 =
            std::mem::transmute(load_library);

        println!("目标 PID      : {pid}");
        println!("DLL           : {dll_abs}");
        println!("LoadLibraryW @ : {:?}", load_library as *const core::ffi::c_void);
        println!("（请人工确认以上 PID 是小号实例！3 秒后开始注入，Ctrl+C 取消）");
        std::thread::sleep(std::time::Duration::from_secs(3));

        let thread = CreateRemoteThread(
            process,
            None,
            0,
            Some(load_library),
            Some(remote_mem),
            0,
            None,
        )
        .unwrap_or_else(|e| {
            eprintln!("CreateRemoteThread 失败: {e}");
            VirtualFreeEx(process, remote_mem, 0, MEM_RELEASE);
            let _ = CloseHandle(process);
            exit(1)
        });

        if WaitForSingleObject(thread, 15_000) != WAIT_OBJECT_0 {
            eprintln!("远程线程 15s 未结束（可疑：目标可能有反注入护栏）");
        } else {
            let mut exit_code = 0u32;
            let _ = GetExitCodeThread(thread, &mut exit_code);
            if exit_code == 0 {
                eprintln!("LoadLibraryW 返回 NULL —— DLL 加载失败（检查 64/64 位匹配与依赖）");
            } else {
                println!("注入完成：LoadLibraryW 返回 {exit_code:#x}（模块句柄截断值）");
                println!("检查证明文件: %TEMP%\\qq_native_spike_proof_{pid}.txt");
            }
        }

        let _ = CloseHandle(thread);
        VirtualFreeEx(process, remote_mem, 0, MEM_RELEASE);
        let _ = CloseHandle(process);
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--list") {
        list_qq_processes();
        return;
    }
    let mut pid: Option<u32> = None;
    let mut dll: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--pid" => {
                pid = args.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            "--dll" => {
                dll = args.get(i + 1).cloned();
                i += 2;
            }
            _ => {
                eprintln!("未知参数: {}", args[i]);
                usage();
            }
        }
    }
    let (Some(pid), Some(dll)) = (pid, dll) else {
        usage();
    };
    inject(pid, &dll);
}
