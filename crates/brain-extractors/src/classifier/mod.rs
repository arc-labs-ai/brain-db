//! Classifier extractor — Tier 2.
//!
//! GLiNER-backed zero-shot NER plus a rule-based statement-kind
//! classifier. Labels are passed per `predict()` call (not loaded
//! from a static file) so the classifier tracks the active schema's
//! entity-type qnames verbatim — no per-schema retraining and no
//! OntoNotes relabel layer.
//!
//! ## Submodules
//!
//! - [`config`] — `ClassifierConfig` (model path, dtype, threshold,
//!   warm-up) + XDG-cascade auto-discovery.
//! - [`model`] — `ClassifierModel` trait + `GlinerClassifier`
//!   (forward pass) + `ClassifiedSpan` output type.
//! - [`extractor`] — `ClassifierExtractor`, the `Extractor` impl that
//!   loads a model from config and runs NER on memory text.
//! - [`gliner`] — the GLiNER model implementation (BERT backbone +
//!   BiLSTM + span head + tokenizer + decoder).
//!
//! ## Degraded state
//!
//! When `ClassifierConfig.model_path == None` or the load fails, the
//! [`ClassifierExtractor`] registers in a **degraded state** — every
//! `run()` dispatch returns
//! `ExtractionResult::skipped(SkippedDisabled, "classifier model not
//! loaded")`. "Not configured" isn't a failure: no inference was
//! attempted, so nothing was dropped.

pub mod config;
pub mod extractor;
pub mod gliner;
pub mod model;

#[cfg(test)]
mod tests;

pub use config::{default_xdg_model_dir, ClassifierConfig};
pub use extractor::ClassifierExtractor;
pub use gliner::{GlinerConfig, GlinerError, GlinerModel, Span as GlinerSpan};
pub use model::{ClassifiedSpan, ClassifierModel, GlinerClassifier};

// Shared constants used by config, extractor, and tests.

/// Directory name under `$XDG_DATA_HOME/brain/models/` populated by
/// `.devcontainer/bootstrap-model.sh` for the GLiNER NER model.
pub const NER_MODEL_DIR_NAME: &str = "gliner-small-v2.1";

/// Files the bootstrap script writes for a healthy GLiNER install.
/// Used by the XDG-discovery probe to decide whether to point the
/// classifier at the default directory or fall back to the unloaded
/// (degraded) config. Tokenizer-side companions (`spm.model`) are
/// optional from the loader's perspective so they're not on this list.
pub const NER_MODEL_REQUIRED_FILES: &[&str] = &[
    "pytorch_model.bin",
    "tokenizer.json",
    "config.json",
    "gliner_config.json",
];

pub(super) const WEIGHTS_FILE: &str = "pytorch_model.bin";
pub(super) const DEFAULT_MAX_SEQ_LEN: usize = 384;
pub(super) const DEFAULT_WARMUP_ITERS: usize = 1;
/// Post-sigmoid acceptance threshold default for the GLiNER classifier.
pub const DEFAULT_GLINER_THRESHOLD: f32 = 0.5;

/// Strip the first namespace prefix from a schema qname. `"brain:Person"`
/// becomes `"Person"`; an input without a `':'` is returned unchanged.
/// Only splits on the first colon so `"a:b:c"` becomes `"b:c"`.
pub(super) fn simple_label(qname: &str) -> &str {
    qname.split_once(':').map(|(_, rest)| rest).unwrap_or(qname)
}
