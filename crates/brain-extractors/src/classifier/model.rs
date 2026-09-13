//! ClassifierModel trait + ClassifiedSpan output type + GlinerClassifier.

use std::sync::Arc;

use candle_core::Device;

use super::config::ClassifierConfig;
use super::gliner::{GlinerConfig, GlinerError, GlinerModel};
use super::WEIGHTS_FILE;
use crate::framework::extractor::ExtractorError;

pub trait ClassifierModel: Send + Sync {
    /// Run NER with the given label set. Labels are the plain (de-prefixed)
    /// entity type names the active schema declared, e.g. `["Person",
    /// "Organization"]`. Returned spans carry the same plain label; the
    /// caller (`ClassifierExtractor::run`) remaps back to the schema qname.
    fn predict(&self, text: &str, labels: &[&str]) -> Result<Vec<ClassifiedSpan>, ExtractorError>;

    /// Run NER over a batch of `(text, labels)` pairs amortising the
    /// model forward pass across `inputs.len()` rows. The classifier
    /// tier dominates extractor latency (single-input GLiNER inference
    /// is ~4s on CPU); batching the backbone GEMM is the lever that
    /// keeps the worker's drain throughput ahead of the encode arrival
    /// rate.
    ///
    /// The default impl falls back to per-row [`Self::predict`] for any
    /// model that doesn't override; downstream callers can still call
    /// `predict_batch` unconditionally. Real impls (`GlinerClassifier`)
    /// override to run a single batched forward pass.
    fn predict_batch(
        &self,
        inputs: &[(&str, &[&str])],
    ) -> Result<Vec<Vec<ClassifiedSpan>>, ExtractorError> {
        let mut out = Vec::with_capacity(inputs.len());
        for (text, labels) in inputs {
            out.push(self.predict(text, labels)?);
        }
        Ok(out)
    }

    /// Pinned model identifier — BLAKE3 fingerprint hex truncated to
    /// 16 bytes. Bumps when weights change.
    fn version(&self) -> &str;
}

/// Output shape from a zero-shot classifier. `label` is verbatim from
/// the labels passed to [`ClassifierModel::predict`] — i.e. the plain
/// (de-prefixed) entity name. The extractor pipeline remaps it back to
/// the schema qname before downstream projection.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassifiedSpan {
    /// Plain entity-type label (e.g. `"Person"`).
    pub label: String,
    /// Span text sliced from the original input.
    pub text: String,
    /// Inclusive character offset of the span start.
    pub char_start: usize,
    /// Exclusive character offset of the span end.
    pub char_end: usize,
    /// Post-sigmoid confidence in `[0, 1]`.
    pub confidence: f32,
}

/// GLiNER-backed [`ClassifierModel`]. Wraps a loaded
/// [`crate::classifier::gliner::GlinerModel`] plus a fingerprint over the
/// `pytorch_model.bin` blob.
pub struct GlinerClassifier {
    model: Arc<GlinerModel>,
    fingerprint_hex: String,
}

impl std::fmt::Debug for GlinerClassifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlinerClassifier")
            .field("fingerprint", &self.fingerprint_hex)
            .finish()
    }
}

impl GlinerClassifier {
    /// Load from an operator-provided model directory. Returns
    /// [`ExtractorError::ModelNotFound`] when `model_path` is unset or
    /// the directory is missing; load failures (corrupt weights, bad
    /// tokenizer, missing `[ENT]` token) map to
    /// [`ExtractorError::InferenceFailed`].
    pub fn load(config: &ClassifierConfig) -> Result<Self, ExtractorError> {
        if !matches!(config.device, Device::Cpu) {
            return Err(ExtractorError::InferenceFailed {
                reason: "only Device::Cpu is supported in v1".into(),
            });
        }

        let dir = config
            .model_path
            .as_ref()
            .ok_or_else(|| ExtractorError::ModelNotFound {
                id: "model_path unset".into(),
            })?;
        if !dir.is_dir() {
            return Err(ExtractorError::ModelNotFound {
                id: format!("not a directory: {}", dir.display()),
            });
        }

        let gliner_config = GlinerConfig {
            max_len: config.max_seq_len,
            threshold: config.threshold,
            device: config.device.clone(),
            dtype: config.dtype,
            ..GlinerConfig::default()
        };

        let model = GlinerModel::load(dir, gliner_config).map_err(map_gliner_load_err)?;
        let fingerprint_hex = fingerprint_weights(&dir.join(WEIGHTS_FILE))?;

        tracing::info!(
            target: "brain_extractors::classifier",
            model_dir = %dir.display(),
            fingerprint = %fingerprint_hex,
            "loaded gliner classifier",
        );

        Ok(Self {
            model: Arc::new(model),
            fingerprint_hex,
        })
    }
}

impl ClassifierModel for GlinerClassifier {
    fn predict(&self, text: &str, labels: &[&str]) -> Result<Vec<ClassifiedSpan>, ExtractorError> {
        let spans = self.model.predict(text, labels).map_err(|e| match e {
            GlinerError::TooManyLabels { .. } | GlinerError::InputTooLong { .. } => {
                ExtractorError::InferenceFailed {
                    reason: e.to_string(),
                }
            }
            other => ExtractorError::InferenceFailed {
                reason: format!("gliner predict: {other}"),
            },
        })?;
        Ok(spans
            .into_iter()
            .map(|s| ClassifiedSpan {
                label: s.label,
                text: s.text,
                char_start: s.char_start,
                char_end: s.char_end,
                confidence: s.score,
            })
            .collect())
    }

    fn predict_batch(
        &self,
        inputs: &[(&str, &[&str])],
    ) -> Result<Vec<Vec<ClassifiedSpan>>, ExtractorError> {
        let raw = self.model.predict_batch(inputs).map_err(|e| match e {
            GlinerError::TooManyLabels { .. } | GlinerError::InputTooLong { .. } => {
                ExtractorError::InferenceFailed {
                    reason: e.to_string(),
                }
            }
            other => ExtractorError::InferenceFailed {
                reason: format!("gliner predict_batch: {other}"),
            },
        })?;
        Ok(raw
            .into_iter()
            .map(|spans| {
                spans
                    .into_iter()
                    .map(|s| ClassifiedSpan {
                        label: s.label,
                        text: s.text,
                        char_start: s.char_start,
                        char_end: s.char_end,
                        confidence: s.score,
                    })
                    .collect()
            })
            .collect())
    }

    fn version(&self) -> &str {
        &self.fingerprint_hex
    }
}

fn map_gliner_load_err(e: GlinerError) -> ExtractorError {
    match e {
        GlinerError::MissingFile(p) => ExtractorError::ModelNotFound { id: p },
        other => ExtractorError::InferenceFailed {
            reason: other.to_string(),
        },
    }
}

/// BLAKE3 fingerprint of the weights file truncated to 16 bytes,
/// rendered as hex. Bumps the `ClassifierModel::version` value
/// whenever the operator swaps in fresh weights so downstream audit
/// rows can tell two extractor outputs apart.
fn fingerprint_weights(path: &std::path::Path) -> Result<String, ExtractorError> {
    let bytes = std::fs::read(path).map_err(|e| ExtractorError::ModelNotFound {
        id: format!("read {} failed: {e}", path.display()),
    })?;
    let hash = blake3::hash(&bytes);
    let bytes16: [u8; 16] = hash.as_bytes()[..16]
        .try_into()
        .expect("invariant: blake3 digest is 32 bytes, slicing the first 16 always succeeds");
    let mut hex = String::with_capacity(32);
    for b in &bytes16 {
        hex.push_str(&format!("{b:02x}"));
    }
    Ok(hex)
}
