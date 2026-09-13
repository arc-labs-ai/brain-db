//! Configuration for the embedding layer.
//!
//! `model_path` is the operator-control surface; FP32 is the default
//! (FP16/INT8 deferred).

use std::path::PathBuf;

use candle_core::{DType, Device};

/// Knobs for [`crate::ModelHandle`]. Future additions (cache, batcher)
/// will extend with their own fields without forcing a re-spelling of
/// the model-load surface.
///
/// `device` and `dtype` are forward-compatibility slots: v1 accepts
/// only `Device::Cpu` + `DType::F32`. Other values fail at load with
/// [`crate::EmbedError::UnsupportedDevice`].
#[derive(Debug, Clone)]
pub struct EmbedderConfig {
    /// Directory containing `config.json`, `tokenizer.json`, and
    /// `model.safetensors`. The operator downloads BGE-small (or an
    /// alternative model) here out-of-band; the substrate does not
    /// auto-download.
    pub model_path: PathBuf,

    /// Inference device. v1: `Device::Cpu`. `Device::Cuda(_)` is
    /// reserved for future work and rejected at load.
    pub device: Device,

    /// Inference dtype. v1: `DType::F32`. FP16 / INT8 deferred per
    pub dtype: DType,

    /// Number of warm-up inferences to run after load (
    /// step 6). Default: 3.
    pub warmup_iters: usize,
}

impl EmbedderConfig {
    /// Default v1 config for the given model directory: CPU, FP32,
    /// 3 warm-up iterations.
    #[must_use]
    pub fn new(model_path: PathBuf) -> Self {
        Self {
            model_path,
            device: Device::Cpu,
            dtype: DType::F32,
            warmup_iters: 3,
        }
    }
}
