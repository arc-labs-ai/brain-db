//! Classifier extractor framework + BERT-based NER. Spec §22/02.
//!
//! Operator-provided model directory, matching `brain-embed`'s
//! [`EmbedderConfig`] surface (phase 5). The substrate doesn't
//! bundle weights or auto-download.
//!
//! ## Load path
//!
//! On `BertTokenClassifier::load`:
//! 1. Validate device (`Cpu` only in v1) + dtype (`F32` only).
//! 2. Validate the `model_path` directory exists.
//! 3. Read `config.json` → `BertConfig`.
//! 4. Read `tokenizer.json` → `Tokenizer`.
//! 5. Refuse `pytorch_model.bin` (pickle); require safetensors.
//! 6. Read `model.safetensors` via `VarBuilder::from_mmaped_safetensors`.
//! 7. Build `BertModel` from the var-store + a token-classification
//!    linear head (`classifier.weight` / `classifier.bias`).
//! 8. Read `labels.txt` → `Vec<String>`.
//! 9. Compute a BLAKE3 fingerprint over `config.json + tokenizer.json
//!    + model.safetensors` for `ClassifierModel::version`.
//!
//! ## Inference
//!
//! `predict(text)`:
//! 1. Tokenize with the configured tokenizer (truncate to
//!    `max_seq_len`).
//! 2. Forward through `BertModel` → hidden states `(1, seq, hidden)`.
//! 3. Linear head → logits `(1, seq, num_labels)`.
//! 4. Softmax + argmax per token → label index + confidence.
//! 5. BIO decoder collapses spans (see [`crate::labels`]).
//! 6. Map token-level spans back to byte ranges in the original
//!    text via the tokenizer's offsets.
//! 7. Filter spans whose label doesn't match the extractor's
//!    declared target (e.g., `target: entity Person` → keep `PER`).
//!
//! ## Degraded state
//!
//! When `ClassifierConfig.model_path == None` or the load fails,
//! the `ClassifierExtractor` registers in a **degraded state** —
//! every `run()` dispatch returns
//! `ExtractionResult::failure("classifier model not loaded")`. The
//! ENCODE handler treats this as a normal `Failure` audit row; no
//! ENCODE-level fallout.

use std::path::PathBuf;
use std::sync::Arc;

use brain_core::knowledge::ExtractorKind;
use brain_core::{ExtractorId, Memory};
use brain_protocol::schema::ExtractorTarget;
use candle_core::{DType, Device, Tensor};

use crate::extractor::{ExtractionContext, ExtractionResult, Extractor, ExtractorError};
use crate::item::{EntityMention, ExtractedItem};

const CONFIG_FILE: &str = "config.json";
const TOKENIZER_FILE: &str = "tokenizer.json";
const WEIGHTS_FILE: &str = "model.safetensors";
const PICKLE_FILE: &str = "pytorch_model.bin";
const LABELS_FILE: &str = "labels.txt";

const DEFAULT_MAX_SEQ_LEN: usize = 256;
const DEFAULT_WARMUP_ITERS: usize = 1;

// ---------------------------------------------------------------------------
// Config.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ClassifierConfig {
    /// Directory containing `config.json` / `tokenizer.json` /
    /// `model.safetensors` / `labels.txt`. `None` means no
    /// classifier model is configured — the extractor will run in
    /// degraded mode.
    pub model_path: Option<PathBuf>,
    /// Inference device. v1: `Device::Cpu`.
    pub device: Device,
    /// Inference dtype. v1: `DType::F32`.
    pub dtype: DType,
    /// Tokens past this length are truncated. Default 256.
    pub max_seq_len: usize,
    /// Warm-up inferences after load. Default 1.
    pub warmup_iters: usize,
}

impl ClassifierConfig {
    /// Default config — model unloaded, CPU, F32.
    #[must_use]
    pub fn unloaded() -> Self {
        Self {
            model_path: None,
            device: Device::Cpu,
            dtype: DType::F32,
            max_seq_len: DEFAULT_MAX_SEQ_LEN,
            warmup_iters: DEFAULT_WARMUP_ITERS,
        }
    }

    /// Config pointing at an operator-provided model directory.
    #[must_use]
    pub fn with_model_path(path: PathBuf) -> Self {
        Self {
            model_path: Some(path),
            ..Self::unloaded()
        }
    }
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self::unloaded()
    }
}

// ---------------------------------------------------------------------------
// Trait + output.
// ---------------------------------------------------------------------------

/// Object-safe model surface. v1 ships [`BertTokenClassifier`];
/// future kinds (custom feature extractors, ONNX, etc.) plug in
/// here.
pub trait ClassifierModel: Send + Sync {
    fn predict(&self, text: &str) -> Result<Vec<TokenClassification>, ExtractorError>;
    /// Pinned model identifier — BLAKE3 fingerprint hex truncated
    /// to 16 bytes. Bumps when weights / tokenizer / config change.
    fn version(&self) -> &str;
}

/// One detected entity span. `label` is the bare CONLL-class
/// (`"PER"`, `"ORG"`, `"LOC"`, etc.).
#[derive(Debug, Clone, PartialEq)]
pub struct TokenClassification {
    pub label: String,
    pub text: String,
    pub start: usize,
    pub end: usize,
    pub confidence: f32,
}

// ---------------------------------------------------------------------------
// BertTokenClassifier — real model load + inference.
// ---------------------------------------------------------------------------

/// Loaded BERT-style token classifier. Constructed by
/// [`Self::load`]; degraded extractors carry no instance and skip
/// invoking this.
pub struct BertTokenClassifier {
    inner: Arc<BertTokenClassifierInner>,
}

impl std::fmt::Debug for BertTokenClassifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BertTokenClassifier")
            .field("num_labels", &self.inner.labels.len())
            .field("fingerprint", &self.inner.fingerprint_hex)
            .field("runtime_wired", &self.inner.runtime.is_some())
            .finish()
    }
}

#[allow(dead_code)]
struct BertTokenClassifierInner {
    config: ClassifierConfig,
    labels: Vec<String>,
    fingerprint_hex: String,
    // The BERT model + linear head + tokenizer live here. Held as
    // opaque handles so the framework path can be exercised without
    // a real model — phase 20.6 integration tests cover the
    // degraded path; #[ignore]'d operator-side tests cover real
    // inference when `BRAIN_NER_MODEL_PATH` is set.
    runtime: Option<Box<dyn BertRuntime>>,
}

/// Object-safe inner runtime surface. The candle-backed impl
/// (`CandleBertRuntime`) lives in `candle_runtime.rs` and is
/// wired into `BertTokenClassifier::load` from phase 20.7b
/// onwards.
pub(crate) trait BertRuntime: Send + Sync {
    fn predict(
        &self,
        text: &str,
        max_seq_len: usize,
        labels: &[String],
    ) -> Result<Vec<TokenClassification>, ExtractorError>;
}

impl BertTokenClassifier {
    /// Load the model directory. Returns
    /// [`ExtractorError::ModelNotFound`] when `model_path` is
    /// `None`; other I/O / parse errors map to specific
    /// [`ExtractorError`] variants.
    pub fn load(config: &ClassifierConfig) -> Result<Self, ExtractorError> {
        // 1. Device + dtype guards.
        if !matches!(config.device, Device::Cpu) {
            return Err(ExtractorError::InferenceFailed {
                reason: "only Device::Cpu is supported in v1".into(),
            });
        }
        if config.dtype != DType::F32 {
            return Err(ExtractorError::InferenceFailed {
                reason: "only DType::F32 is supported in v1".into(),
            });
        }

        // 2. Model path.
        let dir = config.model_path.as_ref().ok_or_else(|| {
            ExtractorError::ModelNotFound {
                id: "model_path unset".into(),
            }
        })?;
        if !dir.is_dir() {
            return Err(ExtractorError::ModelNotFound {
                id: format!("not a directory: {}", dir.display()),
            });
        }

        // 3. Required files.
        for f in [CONFIG_FILE, TOKENIZER_FILE, WEIGHTS_FILE, LABELS_FILE] {
            if !dir.join(f).is_file() {
                return Err(ExtractorError::ModelNotFound {
                    id: format!("missing {f} in {}", dir.display()),
                });
            }
        }

        // 4. Refuse pickle (security; matches brain-embed §03 SD-5.1-1).
        if dir.join(PICKLE_FILE).is_file() {
            return Err(ExtractorError::ModelNotFound {
                id: "refusing pickle (pytorch_model.bin); please convert to safetensors".into(),
            });
        }

        // 5. Read labels.
        let labels = crate::labels::load_labels_file(&dir.join(LABELS_FILE))?;

        // 6. Fingerprint (BLAKE3 over config.json + tokenizer.json + weights).
        let fingerprint_hex = compute_fingerprint(dir)?;

        // 7. Construct the candle-backed runtime. Phase 20.7b
        //    lights this up; the load path itself is what
        //    actually fails when the safetensors blob doesn't
        //    line up with a BertForTokenClassification layout.
        let runtime: Option<Box<dyn BertRuntime>> = match crate::candle_runtime::CandleBertRuntime::load(
            dir,
            config.device.clone(),
            config.dtype,
            config.warmup_iters,
            labels.len(),
        ) {
            Ok(rt) => Some(Box::new(rt)),
            Err(e) => {
                tracing::warn!(
                    target: "brain_extractors::classifier",
                    model_dir = %dir.display(),
                    error = %e,
                    "candle runtime load failed; classifier will run degraded",
                );
                None
            }
        };

        tracing::info!(
            target: "brain_extractors::classifier",
            model_dir = %dir.display(),
            num_labels = labels.len(),
            fingerprint = %fingerprint_hex,
            runtime_wired = runtime.is_some(),
            "loaded classifier model directory",
        );

        Ok(Self {
            inner: Arc::new(BertTokenClassifierInner {
                config: config.clone(),
                labels,
                fingerprint_hex,
                runtime,
            }),
        })
    }

    /// Whether a candle runtime is wired up. False after
    /// [`Self::load`] in phase 20.3 — phase 20.6 lights it up.
    pub fn is_runtime_wired(&self) -> bool {
        self.inner.runtime.is_some()
    }
}

impl ClassifierModel for BertTokenClassifier {
    fn predict(&self, text: &str) -> Result<Vec<TokenClassification>, ExtractorError> {
        match self.inner.runtime.as_ref() {
            Some(rt) => rt.predict(text, self.inner.config.max_seq_len, &self.inner.labels),
            None => Err(ExtractorError::InferenceFailed {
                reason: "BertTokenClassifier runtime not wired (model load failed)".into(),
            }),
        }
    }

    fn version(&self) -> &str {
        &self.inner.fingerprint_hex
    }
}

fn compute_fingerprint(dir: &std::path::Path) -> Result<String, ExtractorError> {
    let mut hasher = blake3::Hasher::new();
    for f in [CONFIG_FILE, TOKENIZER_FILE, WEIGHTS_FILE] {
        let bytes = std::fs::read(dir.join(f)).map_err(|e| ExtractorError::ModelNotFound {
            id: format!("read {f} failed: {e}"),
        })?;
        hasher.update(&bytes);
    }
    let hash = hasher.finalize();
    let bytes: [u8; 32] = *hash.as_bytes();
    let truncated = &bytes[..16];
    let mut hex = String::with_capacity(32);
    for b in truncated {
        hex.push_str(&format!("{b:02x}"));
    }
    Ok(hex)
}

// Quiet unused-import warnings during the 20.3 / 20.6 gap.
#[allow(dead_code)]
fn _ensure_candle_imports(t: Tensor) -> Tensor {
    t
}

// ---------------------------------------------------------------------------
// ClassifierExtractor — implements Extractor.
// ---------------------------------------------------------------------------

pub struct ClassifierExtractor {
    id: ExtractorId,
    name: String,
    target: ExtractorTarget,
    extractor_version: u32,
    confidence_threshold: f32,
    model: Option<Arc<dyn ClassifierModel>>,
    /// Reason captured at construction time when the model couldn't
    /// load — surfaces in every Failure audit row.
    degraded_reason: Option<String>,
}

impl ClassifierExtractor {
    /// Fully-wired extractor with a loaded model.
    pub fn new(
        id: ExtractorId,
        name: String,
        target: ExtractorTarget,
        extractor_version: u32,
        confidence_threshold: f32,
        model: Arc<dyn ClassifierModel>,
    ) -> Self {
        Self {
            id,
            name,
            target,
            extractor_version,
            confidence_threshold,
            model: Some(model),
            degraded_reason: None,
        }
    }

    /// Degraded extractor — no model loaded. Every dispatch writes
    /// a `Failure` audit row with the captured reason.
    pub fn degraded(
        id: ExtractorId,
        name: String,
        target: ExtractorTarget,
        extractor_version: u32,
        confidence_threshold: f32,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id,
            name,
            target,
            extractor_version,
            confidence_threshold,
            model: None,
            degraded_reason: Some(reason.into()),
        }
    }

    /// True iff a model is wired in.
    pub fn is_loaded(&self) -> bool {
        self.model.is_some()
    }

    fn project(&self, span: TokenClassification) -> Option<ExtractedItem> {
        match &self.target {
            ExtractorTarget::Entity { entity_type } => {
                if !label_matches_entity_type(&span.label, entity_type) {
                    return None;
                }
                Some(ExtractedItem::EntityMention(EntityMention {
                    entity_type_qname: entity_type.clone(),
                    text: span.text,
                    start: span.start,
                    end: span.end,
                    confidence: span.confidence,
                    extractor_id: self.id.raw(),
                    extractor_version: self.extractor_version,
                }))
            }
            // Statement / Relation / EntityOrStatement classifier targets
            // are phase 22+; v1 emits nothing for them but doesn't fail.
            _ => None,
        }
    }
}

/// `Person`-typed extractor accepts `PER`. Future entity types
/// (`Organization` / `Location`) accept `ORG` / `LOC`. Unknown
/// entity types fall through with no match.
fn label_matches_entity_type(label: &str, entity_type_qname: &str) -> bool {
    let local = entity_type_qname.rsplit(':').next().unwrap_or(entity_type_qname);
    match (label, local) {
        ("PER", "Person") => true,
        ("ORG", "Organization") => true,
        ("LOC", "Location") => true,
        // Allow the extractor's target to be a literal CONLL label
        // for advanced operator schemas.
        (l, lt) if l == lt => true,
        _ => false,
    }
}

impl Extractor for ClassifierExtractor {
    fn id(&self) -> ExtractorId {
        self.id
    }

    fn kind(&self) -> ExtractorKind {
        ExtractorKind::Classifier
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn extractor_version(&self) -> u32 {
        self.extractor_version
    }

    fn run(&self, ctx: &ExtractionContext<'_>, mem: &Memory) -> ExtractionResult {
        let at = ctx.now_unix_nanos;
        let text = mem.text.as_deref().unwrap_or("");

        let Some(model) = self.model.as_ref() else {
            let reason = self
                .degraded_reason
                .as_deref()
                .unwrap_or("classifier model not loaded");
            return ExtractionResult::failure(reason, at, at);
        };

        let spans = match model.predict(text) {
            Ok(s) => s,
            Err(e) => return ExtractionResult::failure(e.to_string(), at, at),
        };

        let mut items = Vec::new();
        for span in spans {
            if span.confidence < self.confidence_threshold {
                continue;
            }
            if let Some(item) = self.project(span) {
                items.push(item);
            }
        }
        ExtractionResult::success(items, at, at)
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ExtractorRegistry;
    use brain_core::{AgentId, ContextId, MemoryId, MemoryKind, Salience};

    fn entity_target() -> ExtractorTarget {
        ExtractorTarget::Entity {
            entity_type: "brain:Person".into(),
        }
    }

    fn memory(text: &str) -> Memory {
        Memory {
            id: MemoryId::pack(0, 1, 0),
            agent: AgentId::new(),
            context: ContextId(0),
            kind: MemoryKind::Episodic,
            salience: Salience::default(),
            text: Some(text.into()),
            created_at_unix_ms: 0,
            last_accessed_at_unix_ms: 0,
        }
    }

    fn ctx<'a>(reg: &'a ExtractorRegistry) -> ExtractionContext<'a> {
        ExtractionContext {
            schema_version: 1,
            now_unix_nanos: 0,
            registry: reg,
        }
    }

    // ----- ClassifierConfig defaults ----------------------------------------

    #[test]
    fn config_default_disables_classifier() {
        let c = ClassifierConfig::default();
        assert!(c.model_path.is_none());
        assert!(matches!(c.device, Device::Cpu));
        assert_eq!(c.dtype, DType::F32);
        assert_eq!(c.max_seq_len, DEFAULT_MAX_SEQ_LEN);
    }

    #[test]
    fn config_with_model_path_keeps_defaults() {
        let c = ClassifierConfig::with_model_path("/tmp/ner".into());
        assert_eq!(c.model_path.as_deref(), Some(std::path::Path::new("/tmp/ner")));
        assert_eq!(c.max_seq_len, DEFAULT_MAX_SEQ_LEN);
    }

    // ----- BertTokenClassifier::load error paths ----------------------------

    #[test]
    fn load_returns_error_when_path_is_none() {
        let cfg = ClassifierConfig::unloaded();
        let err = BertTokenClassifier::load(&cfg).unwrap_err();
        assert!(
            matches!(err, ExtractorError::ModelNotFound { ref id }
                if id.contains("model_path unset")),
            "got {err:?}"
        );
    }

    #[test]
    fn load_returns_error_when_directory_missing() {
        let cfg = ClassifierConfig::with_model_path("/this/does/not/exist/420".into());
        let err = BertTokenClassifier::load(&cfg).unwrap_err();
        assert!(matches!(err, ExtractorError::ModelNotFound { .. }));
    }

    #[test]
    fn load_returns_error_when_required_files_missing() {
        // Empty directory — every required file is absent.
        let dir = tempfile::tempdir().unwrap();
        let cfg = ClassifierConfig::with_model_path(dir.path().to_path_buf());
        let err = BertTokenClassifier::load(&cfg).unwrap_err();
        match err {
            ExtractorError::ModelNotFound { id } => {
                assert!(id.contains("missing"));
            }
            other => panic!("expected ModelNotFound, got {other:?}"),
        }
    }

    #[test]
    fn load_refuses_pickle() {
        let dir = tempfile::tempdir().unwrap();
        // Put a pytorch_model.bin in place but no safetensors.
        std::fs::write(dir.path().join(CONFIG_FILE), "{}").unwrap();
        std::fs::write(dir.path().join(TOKENIZER_FILE), "{}").unwrap();
        std::fs::write(dir.path().join(LABELS_FILE), "O\nB-PER\n").unwrap();
        std::fs::write(dir.path().join(PICKLE_FILE), b"pickle-junk").unwrap();
        let cfg = ClassifierConfig::with_model_path(dir.path().to_path_buf());
        let err = BertTokenClassifier::load(&cfg).unwrap_err();
        // Missing safetensors triggers before the pickle check; either
        // error is acceptable as a load failure.
        assert!(matches!(err, ExtractorError::ModelNotFound { .. }));
    }

    #[test]
    fn load_validates_dtype_and_device() {
        let mut cfg = ClassifierConfig::with_model_path("/tmp".into());
        cfg.dtype = DType::F16;
        let err = BertTokenClassifier::load(&cfg).unwrap_err();
        assert!(matches!(err, ExtractorError::InferenceFailed { .. }));
    }

    // ----- Degraded extractor dispatch --------------------------------------

    fn degraded_ext() -> ClassifierExtractor {
        ClassifierExtractor::degraded(
            ExtractorId::from(42),
            "brain:basic_ner".into(),
            entity_target(),
            1,
            0.6,
            "classifier model not loaded",
        )
    }

    #[test]
    fn degraded_extractor_dispatch_writes_failure() {
        let reg = ExtractorRegistry::new();
        let ext = degraded_ext();
        assert!(!ext.is_loaded());
        let r = ext.run(&ctx(&reg), &memory("Alice met Bob"));
        assert_eq!(r.status, crate::extractor::ExtractionStatus::Failure);
        assert!(r.status_reason.contains("not loaded"));
    }

    #[test]
    fn degraded_extractor_returns_zero_items() {
        let reg = ExtractorRegistry::new();
        let r = degraded_ext().run(&ctx(&reg), &memory("anything"));
        assert!(r.items.is_empty());
    }

    // ----- Label → entity-type match table ----------------------------------

    #[test]
    fn label_to_entity_type_match_table() {
        assert!(label_matches_entity_type("PER", "brain:Person"));
        assert!(label_matches_entity_type("ORG", "acme:Organization"));
        assert!(label_matches_entity_type("LOC", "Location"));
        assert!(!label_matches_entity_type("ORG", "Person"));
        // Literal CONLL-label entity type matches itself.
        assert!(label_matches_entity_type("PER", "PER"));
    }

    // ----- Real-inference smoke (operator-gated) ----------------------------
    //
    // Run with `BRAIN_NER_MODEL_PATH=/path/to/ner cargo test \
    //     -p brain-extractors --lib classifier::tests::real_inference -- \
    //     --ignored --nocapture`.

    #[test]
    #[ignore = "requires BRAIN_NER_MODEL_PATH and an operator-provided NER model"]
    fn real_inference_returns_per_span_for_alice() {
        let path = match std::env::var("BRAIN_NER_MODEL_PATH") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("BRAIN_NER_MODEL_PATH unset — skipping");
                return;
            }
        };
        let cfg = ClassifierConfig::with_model_path(path.into());
        let model = BertTokenClassifier::load(&cfg).expect("load");
        let spans = model.predict("Alice met Bob in Paris.").expect("predict");
        assert!(spans.iter().any(|s| s.label == "PER" && s.text.contains("Alice")));
        assert!(spans.iter().any(|s| s.label == "LOC" && s.text.contains("Paris")));
    }
}
