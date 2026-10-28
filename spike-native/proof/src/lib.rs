//! 鹊桥 native 注入 spike S0 —— 证明 DLL。
//!
//! 被 LoadLibrary 加载进 QQ.exe 后，在 DllMain(ATTACH) 里用纯 Win32 API
//! 同步写一个证明文件到 %TEMP%。刻意不用 std（fs/fmt/线程）：DllMain 处于
//! loader lock，只允许调用不依赖 DLL 解析与线程创建的最小 API 面。

#![allow(non_snake_case)]

use core::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HMODULE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, WriteFile, CREATE_ALWAYS, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_WRITE,
    FILE_SHARE_READ,
};
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::Win32::System::Threading::GetCurrentProcessId;

const DLL_PROCESS_ATTACH: u32 = 1;

fn push_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(s.as_bytes());
}

fn push_u64(buf: &mut Vec<u8>, mut v: u64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    if v == 0 {
        tmp[i - 1] = b'0';
        i -= 1;
    } else {
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
    }
    buf.extend_from_slice(&tmp[i..]);
}

/// 宽字符缓冲区 -> UTF-8 有损转换（仅用于日志内容，损失可接受）
fn wide_to_utf8(wide: &[u16]) -> Vec<u8> {
    String::from_utf16_lossy(wide).into_bytes()
}

fn write_proof(hinst: *mut c_void) {
    unsafe {
        // %TEMP% 路径
        let mut tmp = [0u16; 260];
        let tmp_len = windows::Win32::Storage::FileSystem::GetTempPathW(Some(&mut tmp)) as usize;
        if tmp_len == 0 || tmp_len >= tmp.len() {
            return;
        }

        // 文件名：qq_native_spike_proof_<pid>.txt
        let pid = GetCurrentProcessId();
        let mut name_u8 = b"qq_native_spike_proof_".to_vec();
        push_u64(&mut name_u8, pid as u64);
        name_u8.extend_from_slice(b".txt");
        let name_wide: Vec<u16> = name_u8.iter().map(|&b| b as u16).collect();

        let mut path = vec![0u16; tmp_len + name_wide.len() + 1];
        path[..tmp_len].copy_from_slice(&tmp[..tmp_len]);
        path[tmp_len..tmp_len + name_wide.len()].copy_from_slice(&name_wide);

        // 本 DLL 完整路径
        let mut self_path = [0u16; 1024];
        let self_len =
            windows::Win32::System::LibraryLoader::GetModuleFileNameW(Some(HMODULE(hinst as _)), &mut self_path)
                as usize;

        let ft = GetSystemTimeAsFileTime();

        let mut body: Vec<u8> = Vec::with_capacity(640);
        push_str(&mut body, "qq-bridge native spike S0 proof\n");
        push_str(&mut body, "qq_pid=");
        push_u64(&mut body, pid as u64);
        push_str(&mut body, "\ndll_base=0x");
        let mut addr = hinst as u64;
        let mut hex = [0u8; 16];
        let mut hi = hex.len();
        if addr == 0 {
            hi -= 1;
            hex[hi] = b'0';
        } else {
            while addr > 0 {
                hi -= 1;
                let d = (addr & 0xF) as u8;
                hex[hi] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
                addr >>= 4;
            }
        }
        body.extend_from_slice(&hex[hi..]);
        push_str(&mut body, "\nfiletime=");
        push_u64(&mut body, ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64);
        push_str(&mut body, "\ndll_path=");
        body.extend_from_slice(&wide_to_utf8(&self_path[..self_len]));
        push_str(&mut body, "\n");

        let handle = CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ,
            None,
            CREATE_ALWAYS,
            FILE_FLAGS_AND_ATTRIBUTES(0x80), /* FILE_ATTRIBUTE_NORMAL */
            None,
        );
        let Ok(handle) = handle else { return };
        let mut written = 0u32;
        let _ = WriteFile(handle, Some(&body), Some(&mut written), None);
        let _ = CloseHandle(handle);
    }
}

#[no_mangle]
pub extern "system" fn DllMain(hinst: *mut c_void, reason: u32, _reserved: *mut c_void) -> u32 {
    if reason == DLL_PROCESS_ATTACH {
        write_proof(hinst);
    }
    1 // TRUE
}
