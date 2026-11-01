//! 语音字节 → 16k mono f32 采样（Node 版 voice-asr.js 三路嗅探分发的移植）。
//!
//! - `#!SILK_V3`（容忍腾讯 `0x02` 前缀——QQ 语音的常态变体）→ silk-codec
//!   直接按 16k 解码（重采样由解码器 API 参数承担，无需二次处理）；
//! - `RIFF` WAV（仅 PCM）→ hound 读取 + [`resample_to_16k`] 线性插值重采样；
//! - 其余（AMR/MP3/M4A 等长尾——QQ 语音 99% 是 silk，此路极少触发）→
//!   [`DecodeError::NeedFfmpeg`]，由上层 spawn ffmpeg 转 16k s16le。

/// 解码失败形态；`NeedFfmpeg` 不是错误——是交给上层 ffmpeg 兜底的信号。
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// 空字节/过短，无从嗅探。
    Empty,
    /// 长尾格式（AMR/MP3/M4A…），上层 ffmpeg `pipe:0 → 16k s16le pipe:1`。
    NeedFfmpeg,
    /// 已识别格式但解码失败（silk 坏帧、非 PCM 的压缩 wav 等）。
    Failed(String),
}

/// 剥除腾讯 `0x02` 前缀后的 `#!SILK_V3` 嗅探。
fn is_silk(bytes: &[u8]) -> bool {
    let b = bytes.strip_prefix(&[0x02]).unwrap_or(bytes);
    b.starts_with(b"#!SILK")
}

/// QQ 语音字节 → 16k mono f32 归一化采样。
pub fn decode_to_16k(bytes: &[u8]) -> Result<Vec<f32>, DecodeError> {
    if bytes.len() < 12 {
        return Err(DecodeError::Empty);
    }
    if is_silk(bytes) {
        let samples = crate::silk::decode_to_pcm(bytes, 16_000).map_err(DecodeError::Failed)?;
        return Ok(samples.iter().map(|&s| s as f32 / 32768.0).collect());
    }
    if bytes.starts_with(b"RIFF") {
        // 压缩 wav（非 PCM）与 Node 版 pcmFromWav「仅 PCM」语义一致——交 ffmpeg。
        let cursor = std::io::Cursor::new(bytes.to_vec());
        let reader = hound::WavReader::new(cursor).map_err(|_| DecodeError::NeedFfmpeg)?;
        let spec = reader.spec();
        if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
            return Err(DecodeError::NeedFfmpeg);
        }
        let samples_i16: Vec<i16> = reader
            .into_samples::<i16>()
            .map(|s| s.unwrap_or(0))
            .collect();
        let f32s: Vec<f32> = samples_i16.iter().map(|&s| s as f32 / 32768.0).collect();
        // 单声道直接用；立体声取左声道（QQ 语音都是 mono，此为防御）。
        let mono = if spec.channels >= 2 {
            f32s.chunks(spec.channels as usize)
                .filter_map(|c| c.first().copied())
                .collect()
        } else {
            f32s
        };
        return if spec.sample_rate == 16_000 {
            Ok(mono)
        } else {
            Ok(resample_to_16k(&mono, spec.sample_rate as i32))
        };
    }
    Err(DecodeError::NeedFfmpeg)
}

/// s16le PCM 字节 → f32 归一化。
pub fn pcm_s16le_to_f32(pcm: &[u8]) -> Vec<f32> {
    pcm.as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c) as f32 / 32768.0)
        .collect()
}

/// 任意采样率 → 16k 线性插值重采样（mono f32）。
pub fn resample_to_16k(samples: &[f32], from: i32) -> Vec<f32> {
    if from == 16_000 || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = from as f64 / 16_000.0;
    let out_len = (samples.len() as f64 / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let i0 = pos.floor() as usize;
        let i1 = (i0 + 1).min(samples.len() - 1);
        let t = pos - i0 as f64;
        let v = samples[i0] as f64 * (1.0 - t) + samples[i1] as f64 * t;
        out.push(v as f32);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 嗅探三分支：腾讯前缀 silk / 裸 silk / RIFF / 长尾格式 / 空字节。
    #[test]
    fn sniff_branches() {
        let silk = b"\x02#!SILK_V3\x00\x00...";
        assert!(is_silk(silk));
        assert!(is_silk(b"#!SILK_V3..."));
        assert!(!is_silk(b"RIFF....WAVE"));
        // 长尾：AMR 头走 NeedFfmpeg。
        assert_eq!(
            decode_to_16k(b"#!AMR\n000000"),
            Err(DecodeError::NeedFfmpeg)
        );
        assert_eq!(decode_to_16k(b"short"), Err(DecodeError::Empty));
        // RIFF 但非 PCM（bits!=16）→ NeedFfmpeg。
        let non_pcm = [
            b"RIFF".as_slice(),
            &[24u8, 0, 0, 0],
            b"WAVE".as_slice(),
            b"fmt ".as_slice(),
        ]
        .concat();
        assert_eq!(decode_to_16k(&non_pcm), Err(DecodeError::NeedFfmpeg));
    }

    /// s16le → f32 归一化。
    #[test]
    fn pcm_conversion() {
        let pcm = [
            0i16.to_le_bytes(),
            i16::MAX.to_le_bytes(),
            i16::MIN.to_le_bytes(),
        ]
        .concat();
        let f = pcm_s16le_to_f32(&pcm);
        assert_eq!(f[0], 0.0);
        assert!((f[1] - 0.99997).abs() < 1e-4);
        assert!((f[2] + 1.0).abs() < 1e-4);
    }

    /// 重采样：恒等（16k）、降采样长度与端点、升采样。
    #[test]
    fn resample_math() {
        let s: Vec<f32> = (0..1600).map(|i| i as f32).collect();
        assert_eq!(resample_to_16k(&s, 16_000).len(), 1600);
        // 32k → 16k：长度减半，首点对齐。
        let s32: Vec<f32> = (0..3200).map(|i| i as f32).collect();
        let out = resample_to_16k(&s32, 32_000);
        assert_eq!(out.len(), 1600);
        assert_eq!(out[0], 0.0);
        // 8k → 16k：长度翻倍。
        let s8: Vec<f32> = vec![0.5; 800];
        assert_eq!(resample_to_16k(&s8, 8_000).len(), 1600);
        assert!(
            (out[1599] - 3198.0).abs() < 0.5,
            "末点线性插值：{}",
            out[1599]
        );
    }

    /// 真实 wav fixture（22050Hz mono 16bit，3.2s）：解码+重采样出 ~16k 采样。
    #[test]
    fn decodes_wav_fixture() {
        let bytes = include_bytes!("../tests/fixtures/asr-sample.wav");
        let out = decode_to_16k(bytes).expect("PCM wav 必须可解");
        // 3.2s @22050 → 3.2s @16000 ≈ 51200 采样（重采样长度 ±1 容差）。
        // 69532 帧 @22050 → 16k ≈ 50449 采样（±64 重采样边界容差）。
        assert!((out.len() as i64 - 50_449).abs() < 64, "len={}", out.len());
        // 有声段非全零。
        assert!(out.iter().any(|&s| s.abs() > 0.01), "语音内容非静音");
    }
}
