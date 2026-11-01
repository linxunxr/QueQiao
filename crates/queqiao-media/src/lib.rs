//! queqiao-media —— 语音解码与 ASR（Node 版 voice-asr.js 的 Rust 落地）。
//!
//! 分层：本 crate 只做可离线单测的计算与组装（格式解码、重采样、识别器
//! 封装）；取字节（URL 抓取/get_record）、下载模型、转写调度等 IO 在
//! queqiao bin 侧编排。
//!
//! - [`decode`]：语音字节 → 16k mono f32 采样（silk/wav 三路嗅探分发；
//!   AMR/MP3 等长尾格式返回 `NeedFfmpeg` 由上层 spawn ffmpeg）。
//! - [`silk`]：SILK v3 解码（vendor Skype C 源 FFI）。
//! - [`asr`]：sherpa-onnx paraformer-zh 离线识别器封装（懒加载由上层管理）。

pub mod asr;
pub mod decode;
pub mod silk;
