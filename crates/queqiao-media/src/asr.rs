//! sherpa-onnx paraformer-zh 离线识别器封装（懒加载与全局单例归 bin 侧）。
//!
//! 模型（Apache-2.0，ModelScope iic paraformer-large 经 k2-fsa 转换）：
//! `{dir}/model.int8.onnx`（~217MB）+ `{dir}/tokens.txt`。与 Node 版
//! voice-asr.js 同款——转写结果可直接对拍。

use std::path::Path;

use sherpa_onnx::{
    OfflineModelConfig, OfflineParaformerModelConfig, OfflineRecognizer, OfflineRecognizerConfig,
};

/// 模型两件套是否在场（asr 可用性判定——Node 版 asrModelAvailable 同义）。
pub fn model_files_present(dir: &Path) -> bool {
    dir.join("model.int8.onnx").is_file() && dir.join("tokens.txt").is_file()
}

pub struct AsrEngine {
    recognizer: OfflineRecognizer,
}

impl AsrEngine {
    /// 加载模型（秒级，首次加载含 onnxruntime 初始化——调用方负责缓存）。
    pub fn load(dir: &Path) -> Result<Self, String> {
        if !model_files_present(dir) {
            return Err(format!(
                "模型不完整（需 model.int8.onnx + tokens.txt）：{}",
                dir.display()
            ));
        }
        let config = OfflineRecognizerConfig {
            model_config: OfflineModelConfig {
                paraformer: OfflineParaformerModelConfig {
                    model: Some(dir.join("model.int8.onnx").to_string_lossy().into_owned()),
                },
                tokens: Some(dir.join("tokens.txt").to_string_lossy().into_owned()),
                num_threads: 2,
                debug: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let recognizer = OfflineRecognizer::create(&config)
            .ok_or_else(|| "识别器初始化失败（模型损坏或内存不足）".to_string())?;
        Ok(Self { recognizer })
    }

    /// 16k mono f32 采样 → 文本。空采样/静音返回空串不报错。
    pub fn transcribe(&self, samples_16k: &[f32]) -> String {
        if samples_16k.is_empty() {
            return String::new();
        }
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(16_000, samples_16k);
        self.recognizer.decode(&stream);
        stream
            .get_result()
            .map(|r| r.text)
            .unwrap_or_default()
            .trim()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 模型缺席路径：load 报错带路径；model_files_present 三态。
    #[test]
    fn model_presence_and_missing_load() {
        let dir = std::env::temp_dir().join(format!("qq-asr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!model_files_present(&dir));
        assert!(AsrEngine::load(&dir).is_err());
        // 半套（只 tokens）也不算在场。
        std::fs::write(dir.join("tokens.txt"), "a 0\n").unwrap();
        assert!(!model_files_present(&dir));
        // 带模型的端到端识别在真机/本机手动验证（模型不入 CI）。
        let _ = std::fs::remove_dir_all(&dir);
    }
}
