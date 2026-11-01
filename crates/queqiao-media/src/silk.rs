//! SILK v3 解码（vendor Skype SILK C 源的 FFI 封装，rust-silk 项目验证过的
//! 编译与调用方案；许可见 vendor/silk 源文件头——BSD 风格，保留声明再分发）。
//!
//! QQ 语音是 silk v3，常带腾讯 `0x02` 前缀字节；解出指定采样率的 PCM i16。

use std::ffi::c_void;
use std::os::raw::{c_int, c_short, c_uchar};

const SILK_HEADER: &str = "#!SILK_V3";
const TENCENT_PREFIX: u8 = 0x02;
const MAX_BYTES_PER_FRAME: usize = 250;
const MAX_INPUT_FRAMES: usize = 5;
const FRAME_LENGTH_MS: i32 = 20;
const MAX_FRAME_LENGTH_MS: i32 = 60;
const MAX_API_FS_HZ: i32 = 48_000;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DecControl {
    api_sample_rate: c_int,
    frame_size: c_int,
    frames_per_packet: c_int,
    more_internal_decoder_frames: c_int,
    inband_fec_offset: c_int,
}

unsafe extern "C" {
    fn SKP_Silk_SDK_Get_Decoder_Size(dec_size_bytes: *mut c_int) -> c_int;
    fn SKP_Silk_SDK_InitDecoder(dec_state: *mut c_void) -> c_int;
    fn SKP_Silk_SDK_Decode(
        dec_state: *mut c_void,
        dec_control: *mut DecControl,
        lost_flag: c_int,
        in_data: *const c_uchar,
        n_bytes_in: c_int,
        samples_out: *mut c_short,
        n_samples_out: *mut c_short,
    ) -> c_int;
}

/// silk 字节（`#!SILK_V3` 或 `0x02` + 头）→ 指定采样率 mono PCM i16。
/// sample_rate 须为 8000/12000/16000/24000/48000（SILK API 档位）。
pub fn decode_to_pcm(bytes: &[u8], sample_rate: i32) -> Result<Vec<i16>, String> {
    if ![8000, 12000, 16000, 24000, 48000].contains(&sample_rate) {
        return Err(format!("sample_rate 档位非法：{sample_rate}"));
    }
    // 剥头：腾讯 0x02 前缀或裸 #!SILK_V3。
    let body = if bytes.first() == Some(&TENCENT_PREFIX) {
        &bytes[1..]
    } else {
        bytes
    };
    if !body.starts_with(SILK_HEADER.as_bytes()) {
        return Err("非法 silk 头".into());
    }
    let mut input = &body[SILK_HEADER.len()..];

    unsafe {
        let mut dec_size: c_int = 0;
        if SKP_Silk_SDK_Get_Decoder_Size(&mut dec_size) != 0 || dec_size <= 0 {
            return Err("获取解码器尺寸失败".into());
        }
        let mut state = vec![0u8; dec_size as usize];
        if SKP_Silk_SDK_InitDecoder(state.as_mut_ptr() as *mut c_void) != 0 {
            return Err("初始化解码器失败".into());
        }

        let mut control = DecControl {
            api_sample_rate: sample_rate,
            frames_per_packet: 1,
            ..Default::default()
        };
        let max_frame_samples = (MAX_FRAME_LENGTH_MS * MAX_API_FS_HZ / 1000) as usize;
        let mut out = vec![0i16; max_frame_samples];
        let mut payload = vec![0u8; MAX_BYTES_PER_FRAME * MAX_INPUT_FRAMES];
        let mut samples: Vec<i16> = Vec::new();

        loop {
            // 包格式：LE u16 长度 + payload；0xFFFF 结束标记 / 截断即尾。
            if input.len() < 2 {
                break;
            }
            let n_bytes = i16::from_le_bytes([input[0], input[1]]);
            input = &input[2..];
            if n_bytes < 0 {
                break;
            }
            let n = n_bytes as usize;
            if n > 0 {
                if input.len() < n {
                    return Err("silk 包截断".into());
                }
                payload[..n].copy_from_slice(&input[..n]);
                input = &input[n..];
            }

            // n == 0 是丢包帧（lostFlag=1 喂 NULL payload 解 PLC）；否则正常解。
            let lost = if n == 0 { 1 } else { 0 };
            let in_len = if n == 0 { 0 } else { n as c_int };
            let ptr = if n == 0 {
                std::ptr::null()
            } else {
                payload.as_ptr()
            };
            let mut out_len: c_short = 0;
            let ret = SKP_Silk_SDK_Decode(
                state.as_mut_ptr() as *mut c_void,
                &mut control,
                lost,
                ptr,
                in_len,
                out.as_mut_ptr(),
                &mut out_len,
            );
            if ret != 0 {
                return Err(format!("silk 解码失败：{ret}"));
            }
            samples.extend_from_slice(&out[..out_len as usize]);
            // 一包可能还有内部缓冲帧（more_internal_decoder_frames）。
            while control.more_internal_decoder_frames != 0 {
                let mut out_len2: c_short = 0;
                let ret2 = SKP_Silk_SDK_Decode(
                    state.as_mut_ptr() as *mut c_void,
                    &mut control,
                    lost,
                    std::ptr::null(),
                    0,
                    out.as_mut_ptr(),
                    &mut out_len2,
                );
                if ret2 != 0 {
                    return Err(format!("silk 续帧解码失败：{ret2}"));
                }
                samples.extend_from_slice(&out[..out_len2 as usize]);
            }
        }
        let _ = FRAME_LENGTH_MS;
        Ok(samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 头处理与档位校验（真实 silk 字节的解码在真机链路验证——需要 QQ 语音样本）。
    #[test]
    fn header_and_rate_guards() {
        assert!(
            decode_to_pcm(b"\x02#!SILK_V3", 16_000).is_ok(),
            "空体合法（0 帧）"
        );
        assert!(decode_to_pcm(b"#!SILK_V3", 16_000).is_ok());
        assert!(
            decode_to_pcm(b"\x02#!SILK_V3", 22_050).is_err(),
            "非档位采样率拒绝"
        );
        assert_eq!(
            decode_to_pcm(b"RIFFxxxxWAVE", 16_000).unwrap_err(),
            "非法 silk 头"
        );
        // 0xFFFF 结束标记后的垃圾被忽略。
        assert!(decode_to_pcm(b"\x02#!SILK_V3\xff\xffgarbage", 16_000).is_ok());
    }
}
