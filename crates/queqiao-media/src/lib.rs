//! queqiao-media —— 媒体处理（后期阶段占位）。
//!
//! 规划（对应 Node 版三个模块）：
//! - 图片压缩降级（`image-compress.js` → image crate / libvips 绑定）；
//! - 语音 ASR（sherpa-onnx → sherpa-rs，paraformer-zh 模型）；
//! - 视频抽帧（ffmpeg 子进程 + MJPEG 流按 SOI/EOI 切分）。
