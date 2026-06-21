//! Tests covering the full classifier surface: config, model, extractor,
//! and statement-kind pattern matcher.

#![cfg(test)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use brain_core::{
    AgentId, ContextId, ExtractorId, Memory, MemoryId, MemoryKind, Salience,
};
use brain_protocol::schema::ExtractorTarget;
use candle_core::Device;

use super::config::{default_xdg_model_dir_with, ClassifierConfig};
use super::extractor::ClassifierExtractor;
use super::model::{ClassifiedSpan, ClassifierModel, GlinerClassifier};
use super::{
    simple_label, DEFAULT_GLINER_THRESHOLD, DEFAULT_MAX_SEQ_LEN, NER_MODEL_DIR_NAME,
    NER_MODEL_REQUIRED_FILES,
};
use crate::framework::extractor::ExtractionContext;
use crate::framework::extractor::{Extractor, ExtractorError};
use crate::framework::item::ExtractedItem;
use crate::framework::registry::ExtractorRegistry;

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
        occurred_at_unix_nanos: None,
    }
}

fn ctx<'a>(reg: &'a ExtractorRegistry) -> ExtractionContext<'a> {
    ExtractionContext {
            declared_predicates: None,
        declared_kinds: None,
        entity_type_labels: None,
        schema_version: 1,
        now_unix_nanos: 0,
        registry: reg,
        prior_tier_items: None,
        extractor_context: None,
    }
}

fn default_labels() -> Arc<Vec<String>> {
    Arc::new(vec![
        "brain:Person".into(),
        "brain:Organization".into(),
        "brain:Project".into(),
        "brain:Event".into(),
        "brain:Place".into(),
        "brain:Concept".into(),
    ])
}

// ----- ClassifierConfig defaults ----------------------------------------

#[test]
fn config_default_disables_classifier() {
    let c = ClassifierConfig::default();
    assert!(c.model_path.is_none());
    assert!(matches!(c.device, Device::Cpu));
    assert_eq!(c.max_seq_len, DEFAULT_MAX_SEQ_LEN);
    assert!((c.threshold - DEFAULT_GLINER_THRESHOLD).abs() < 1e-6);
}

#[test]
fn config_with_model_path_keeps_defaults() {
    let c = ClassifierConfig::with_model_path("/tmp/ner".into());
    assert_eq!(
        c.model_path.as_deref(),
        Some(std::path::Path::new("/tmp/ner"))
    );
    assert_eq!(c.max_seq_len, DEFAULT_MAX_SEQ_LEN);
}

// ----- GlinerClassifier::load error paths -------------------------------

#[test]
fn load_returns_error_when_path_is_none() {
    let cfg = ClassifierConfig::unloaded();
    let err = GlinerClassifier::load(&cfg).unwrap_err();
    assert!(
        matches!(err, ExtractorError::ModelNotFound { ref id }
            if id.contains("model_path unset")),
        "got {err:?}"
    );
}

#[test]
fn load_returns_error_when_directory_missing() {
    let cfg = ClassifierConfig::with_model_path("/this/does/not/exist/420".into());
    let err = GlinerClassifier::load(&cfg).unwrap_err();
    assert!(matches!(err, ExtractorError::ModelNotFound { .. }));
}

#[test]
fn load_returns_error_when_required_files_missing() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = ClassifierConfig::with_model_path(dir.path().to_path_buf());
    let err = GlinerClassifier::load(&cfg).unwrap_err();
    assert!(matches!(err, ExtractorError::ModelNotFound { .. }));
}

// ----- Degraded extractor dispatch --------------------------------------

fn degraded_ext() -> ClassifierExtractor {
    ClassifierExtractor::degraded(
        ExtractorId::from(42),
        "brain:gliner".into(),
        entity_target(),
        1,
        0.5,
        "classifier model not loaded",
    )
}

#[test]
fn degraded_extractor_dispatch_writes_skipped_disabled() {
    let reg = ExtractorRegistry::new();
    let ext = degraded_ext();
    assert!(!ext.is_loaded());
    let r = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("Alice met Bob")));
    assert_eq!(
        r.status,
        crate::framework::extractor::ExtractionStatus::SkippedDisabled
    );
    assert!(r.status_reason.contains("not loaded"));
}

#[test]
fn degraded_extractor_returns_zero_items() {
    let reg = ExtractorRegistry::new();
    let r = futures_lite::future::block_on(degraded_ext().run(&ctx(&reg), &memory("anything")));
    assert!(r.items.is_empty());
}

// ----- Label snapshotting & projection ---------------------------------

/// Records the labels passed to `predict()` so tests can assert
/// the snapshot wired through correctly.
struct LabelCaptureModel {
    seen: parking_lot::Mutex<Vec<Vec<String>>>,
    spans_per_call: Vec<ClassifiedSpan>,
}

impl LabelCaptureModel {
    fn new(spans: Vec<ClassifiedSpan>) -> Self {
        Self {
            seen: parking_lot::Mutex::new(Vec::new()),
            spans_per_call: spans,
        }
    }
}

impl ClassifierModel for LabelCaptureModel {
    fn predict(&self, _text: &str, labels: &[&str]) -> Result<Vec<ClassifiedSpan>, ExtractorError> {
        self.seen
            .lock()
            .push(labels.iter().map(|s| (*s).to_string()).collect());
        Ok(self.spans_per_call.clone())
    }
    fn version(&self) -> &str {
        "label-capture"
    }
}

#[test]
fn simple_label_strips_namespace_prefix() {
    let cases = [
        ("brain:Person", "Person"),
        ("acme:Customer", "Customer"),
        ("Person", "Person"),
        ("a:b:c", "b:c"),
        ("", ""),
        (":Leading", "Leading"),
    ];
    for (input, want) in cases {
        assert_eq!(simple_label(input), want, "input={input:?}");
    }
}

#[test]
fn predict_passes_stripped_labels_to_model() {
    let model = Arc::new(LabelCaptureModel::new(Vec::new()));
    let labels = default_labels();
    let ext = ClassifierExtractor::new(
        ExtractorId::from(1),
        "brain:gliner".into(),
        entity_target(),
        1,
        0.5,
        model.clone(),
        labels.clone(),
    );
    let reg = ExtractorRegistry::new();
    let _ = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("text")));
    let seen = model.seen.lock();
    assert_eq!(seen.len(), 1);
    let want: Vec<String> = labels.iter().map(|q| simple_label(q).to_string()).collect();
    assert_eq!(seen[0], want);
    // Belt-and-braces: none of the labels carry a colon.
    assert!(
        seen[0].iter().all(|l| !l.contains(':')),
        "labels={:?}",
        seen[0]
    );
}

#[test]
fn per_cycle_ctx_labels_override_construction_labels() {
    // The classifier is built with the 6 seeded labels, but the worker hands
    // it a fresh per-cycle label set via the ExtractionContext (e.g. after a
    // user SCHEMA_UPLOAD adds `Drug`). The model must see the per-cycle labels,
    // not the frozen construction ones — this is what lets a new entity type
    // reach GLiNER without a shard restart.
    let model = Arc::new(LabelCaptureModel::new(Vec::new()));
    let ext = ClassifierExtractor::new(
        ExtractorId::from(1),
        "brain:gliner".into(),
        entity_target(),
        1,
        0.5,
        model.clone(),
        default_labels(), // construction-time fallback (the 6 seeds)
    );
    let reg = ExtractorRegistry::new();
    let cycle_labels: Vec<String> = vec!["brain:Person".into(), "brain:Drug".into()];
    let cycle_ctx = ExtractionContext {
        declared_predicates: None,
        declared_kinds: None,
        entity_type_labels: Some(&cycle_labels),
        schema_version: 1,
        now_unix_nanos: 0,
        registry: &reg,
        prior_tier_items: None,
        extractor_context: None,
    };
    let _ = futures_lite::future::block_on(ext.run(&cycle_ctx, &memory("text")));
    let seen = model.seen.lock();
    assert_eq!(seen.len(), 1);
    // The model saw the per-cycle labels (stripped), including the new `Drug`,
    // not the 6 construction-time defaults.
    assert_eq!(seen[0], vec!["Person".to_string(), "Drug".to_string()]);
}

#[test]
fn predict_remaps_label_back_to_qname_before_projection() {
    let spans = vec![ClassifiedSpan {
        label: "Person".into(),
        text: "Alice".into(),
        char_start: 0,
        char_end: 5,
        confidence: 0.97,
    }];
    let model = Arc::new(LabelCaptureModel::new(spans));
    let ext = ClassifierExtractor::new(
        ExtractorId::from(1),
        "brain:gliner".into(),
        entity_target(),
        1,
        0.5,
        model,
        default_labels(),
    );
    let reg = ExtractorRegistry::new();
    let r = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("Alice ...")));
    assert_eq!(r.items.len(), 1);
    match &r.items[0] {
        ExtractedItem::EntityMention(em) => {
            assert_eq!(em.entity_type_qname, "brain:Person");
            assert_eq!(em.text, "Alice");
        }
        other => panic!("expected EntityMention, got {other:?}"),
    }
}

#[test]
fn predict_drops_spans_whose_label_did_not_match_a_known_simple() {
    let spans = vec![
        ClassifiedSpan {
            label: "Animal".into(),
            text: "Hedwig".into(),
            char_start: 0,
            char_end: 6,
            confidence: 0.95,
        },
        ClassifiedSpan {
            label: "Person".into(),
            text: "Harry".into(),
            char_start: 10,
            char_end: 15,
            confidence: 0.95,
        },
    ];
    let model = Arc::new(LabelCaptureModel::new(spans));
    let ext = ClassifierExtractor::new(
        ExtractorId::from(1),
        "brain:gliner".into(),
        entity_target(),
        1,
        0.5,
        model,
        default_labels(),
    );
    let reg = ExtractorRegistry::new();
    let r = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("...")));
    assert_eq!(r.items.len(), 1, "unknown labels must be dropped");
    match &r.items[0] {
        ExtractedItem::EntityMention(em) => {
            assert_eq!(em.entity_type_qname, "brain:Person");
            assert_eq!(em.text, "Harry");
        }
        other => panic!("expected EntityMention, got {other:?}"),
    }
}

#[test]
fn empty_label_snapshot_yields_skipped() {
    let model = Arc::new(LabelCaptureModel::new(Vec::new()));
    let ext = ClassifierExtractor::new(
        ExtractorId::from(1),
        "brain:gliner".into(),
        entity_target(),
        1,
        0.5,
        model.clone(),
        Arc::new(Vec::new()),
    );
    let reg = ExtractorRegistry::new();
    let r = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("hi")));
    assert_eq!(
        r.status,
        crate::framework::extractor::ExtractionStatus::SkippedDisabled
    );
    assert!(model.seen.lock().is_empty(), "model.predict should not run");
}

#[test]
fn project_emits_brain_qname_verbatim_from_span_label() {
    let ext = ClassifierExtractor::new(
        ExtractorId::from(1),
        "brain:gliner".into(),
        entity_target(),
        1,
        0.5,
        Arc::new(LabelCaptureModel::new(Vec::new())),
        default_labels(),
    );
    let span = ClassifiedSpan {
        label: "brain:Organization".into(),
        text: "Acme".into(),
        char_start: 0,
        char_end: 4,
        confidence: 0.9,
    };
    let item = ext.project(span).expect("entity span projects");
    match item {
        ExtractedItem::EntityMention(em) => {
            assert_eq!(em.entity_type_qname, "brain:Organization");
            assert_eq!(em.text, "Acme");
        }
        other => panic!("expected EntityMention, got {other:?}"),
    }
}

#[test]
fn run_filters_below_confidence_threshold() {
    // Spans carry the *simple* label GLiNER would emit — the extractor
    // remaps them back to the full qname before projection.
    let spans = vec![
        ClassifiedSpan {
            label: "Person".into(),
            text: "Alice".into(),
            char_start: 0,
            char_end: 5,
            confidence: 0.95,
        },
        ClassifiedSpan {
            label: "Person".into(),
            text: "Bob".into(),
            char_start: 10,
            char_end: 13,
            confidence: 0.4,
        },
    ];
    let model = Arc::new(LabelCaptureModel::new(spans));
    let ext = ClassifierExtractor::new(
        ExtractorId::from(1),
        "brain:gliner".into(),
        entity_target(),
        1,
        0.6,
        model,
        default_labels(),
    );
    let reg = ExtractorRegistry::new();
    let r = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("Alice met Bob")));
    assert_eq!(
        r.status,
        crate::framework::extractor::ExtractionStatus::Success
    );
    assert_eq!(r.items.len(), 1);
    match &r.items[0] {
        ExtractedItem::EntityMention(em) => assert_eq!(em.text, "Alice"),
        other => panic!("expected EntityMention, got {other:?}"),
    }
}

// ----- ClassifierConfig accessors --------------------------------------

#[test]
fn config_has_path_and_model_path_accessors() {
    let unloaded = ClassifierConfig::unloaded();
    assert!(!unloaded.has_path());
    assert_eq!(unloaded.model_path(), std::path::Path::new(""));

    let loaded = ClassifierConfig::with_model_path("/srv/ner".into());
    assert!(loaded.has_path());
    assert_eq!(loaded.model_path(), std::path::Path::new("/srv/ner"));
}

// ----- Auto-discovery cascade ------------------------------------------

/// Builds a deterministic env reader from a vector of owned
/// `(key, value)` pairs so each test can describe exactly the env
/// it cares about without touching the global process env. Owning
/// the strings sidesteps lifetime fights with `tempfile::TempDir`
/// paths that don't outlive the test scope.
fn env_fn(pairs: Vec<(String, String)>) -> impl Fn(&str) -> Option<String> {
    move |k| {
        pairs
            .iter()
            .find_map(|(name, value)| (name == k).then(|| value.clone()))
    }
}

fn env_pair(k: &str, v: &str) -> (String, String) {
    (k.to_string(), v.to_string())
}

/// Materialise a directory containing every required GLiNER file as
/// a one-byte stub so `Path::is_file` succeeds on each entry.
fn write_fake_gliner_dir(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).expect("mkdir model dir");
    for f in NER_MODEL_REQUIRED_FILES {
        std::fs::write(dir.join(f), b"x").expect("write stub file");
    }
}

#[test]
fn auto_discover_returns_unloaded_when_env_unset_and_default_path_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let env = env_fn(vec![env_pair(
        "XDG_DATA_HOME",
        tmp.path().to_str().unwrap(),
    )]);
    let is_file = |p: &Path| p.is_file();
    let cfg = ClassifierConfig::auto_discover_with(&env, &is_file);
    assert!(
        !cfg.has_path(),
        "expected unloaded; got {:?}",
        cfg.model_path
    );
}

#[test]
fn auto_discover_returns_with_path_when_env_unset_and_default_path_valid() {
    let tmp = tempfile::tempdir().unwrap();
    let model_dir = tmp
        .path()
        .join("brain")
        .join("models")
        .join(NER_MODEL_DIR_NAME);
    write_fake_gliner_dir(&model_dir);

    let env = env_fn(vec![env_pair(
        "XDG_DATA_HOME",
        tmp.path().to_str().unwrap(),
    )]);
    let is_file = |p: &Path| p.is_file();
    let cfg = ClassifierConfig::auto_discover_with(&env, &is_file);
    assert_eq!(cfg.model_path.as_deref(), Some(model_dir.as_path()));
}

#[test]
fn auto_discover_skips_default_when_one_required_file_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let model_dir = tmp
        .path()
        .join("brain")
        .join("models")
        .join(NER_MODEL_DIR_NAME);
    write_fake_gliner_dir(&model_dir);
    // Remove one of the required files to simulate a broken install.
    std::fs::remove_file(model_dir.join("tokenizer.json")).unwrap();

    let env = env_fn(vec![env_pair(
        "XDG_DATA_HOME",
        tmp.path().to_str().unwrap(),
    )]);
    let is_file = |p: &Path| p.is_file();
    let cfg = ClassifierConfig::auto_discover_with(&env, &is_file);
    assert!(
        !cfg.has_path(),
        "missing tokenizer.json should drop the config back to unloaded"
    );
}

#[test]
fn default_xdg_model_dir_prefers_xdg_data_home() {
    let env = env_fn(vec![
        env_pair("XDG_DATA_HOME", "/srv/data"),
        env_pair("HOME", "/home/dev"),
    ]);
    let dir = default_xdg_model_dir_with(&env).expect("env supplies both");
    assert_eq!(
        dir,
        PathBuf::from("/srv/data/brain/models/gliner-small-v2.1")
    );
}

#[test]
fn default_xdg_model_dir_falls_back_to_home_local_share() {
    let env = env_fn(vec![env_pair("HOME", "/home/dev")]);
    let dir = default_xdg_model_dir_with(&env).expect("HOME is set");
    assert_eq!(
        dir,
        PathBuf::from("/home/dev/.local/share/brain/models/gliner-small-v2.1")
    );
}

// ----- Real-inference smoke (operator-gated) ----------------------------
//
// Run with `BRAIN_NER_MODEL_PATH=/path/to/gliner cargo test \
//     -p brain-extractors --lib classifier::tests::real_inference -- \
//     --ignored --nocapture`.

#[test]
#[ignore = "requires BRAIN_NER_MODEL_PATH and an operator-provided GLiNER model"]
fn real_inference_returns_brain_qnames_for_alice() {
    let path = match std::env::var("BRAIN_NER_MODEL_PATH") {
        Ok(p) => p,
        Err(_) => {
            eprintln!("BRAIN_NER_MODEL_PATH unset — skipping");
            return;
        }
    };
    let cfg = ClassifierConfig::with_model_path(path.into());
    let model = GlinerClassifier::load(&cfg).expect("load");
    let labels = ["brain:Person", "brain:Place"];
    let spans = model
        .predict("Alice met Bob in Paris.", &labels)
        .expect("predict");
    // GLiNER emits the qnames we passed in verbatim.
    assert!(spans
        .iter()
        .any(|s| s.label == "brain:Person" && s.text.contains("Alice")));
    assert!(spans
        .iter()
        .any(|s| s.label == "brain:Place" && s.text.contains("Paris")));
}
