//! ASR 端到端验证（本机手动跑，模型不入 CI）：
//! `cargo run -p queqiao-media --example asr_e2e -- <model_dir> <wav>`
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let model_dir = args
        .get(1)
        .map(|s| Path::new(s).to_path_buf())
        .unwrap_or_else(|| Path::new(r"D:\400env\sherpa-onnx\model\paraformer-zh").to_path_buf());
    let wav = args.get(2).cloned().unwrap_or_else(|| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "\\tests\\fixtures\\asr-sample.wav"
        )
        .to_string()
    });

    let bytes = std::fs::read(&wav).expect("wav 读取失败");
    let samples = queqiao_media::decode::decode_to_16k(&bytes).expect("解码失败");
    println!(
        "wav: {} → {} 采样 @16k（{:.1}s）",
        wav,
        samples.len(),
        samples.len() as f64 / 16000.0
    );

    let t0 = std::time::Instant::now();
    let engine = queqiao_media::asr::AsrEngine::load(&model_dir).expect("模型加载失败");
    println!("模型加载：{:.1}s", t0.elapsed().as_secs_f64());

    let t1 = std::time::Instant::now();
    let text = engine.transcribe(&samples);
    println!("识别耗时：{:.2}s", t1.elapsed().as_secs_f64());
    println!("转写结果：[{text}]");
    assert!(!text.is_empty(), "3 秒语音应识别出内容");
}
