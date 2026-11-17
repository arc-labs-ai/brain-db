//! Where the classifier tier's forward pass actually runs.
//!
//! A shard answers every request routed to it on one reactor thread, and runs
//! its extractor worker as a task on that same thread. GLiNER's forward pass
//! is 3.82 s of uninterruptible CPU work per micro-batch with no await point
//! inside it, so a pass that executes on the reactor thread holds every
//! request on the shard for its full length: against a 1.80 ms median, two
//! writes in a ten-second window took 3.93 s and 3.86 s, one per one-second
//! tick.
//!
//! Nothing in the type system says which thread a `ClassifierModel` runs on,
//! so these tests ask the model itself. They are a pair on purpose: the first
//! shows the pass leaving the reactor thread when a shard's offloader is wired
//! through `MaterializeDeps`, the second shows it staying on that thread when
//! none is. Without the second, the first would still pass if the assertion
//! had stopped measuring anything.

use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use brain_core::{
    ExtractorId, ExtractorKind, Memory, MemoryId, MemoryKind, Salience, SessionId, SpaceId,
};
use brain_extractors::{
    build_registry_from_definitions, ClassifiedSpan, ClassifierModel, ExtractionContext,
    ExtractorError, ExtractorRegistry, MaterializeDeps,
};
use brain_metadata::tables::extractor::ExtractorDefinition;
use brain_protocol::schema::{
    ExtractorDef, ExtractorField, ExtractorKindAst, ExtractorTarget as AstTarget,
};
use brain_workers::GlommioClassifierOffload;

/// A classifier model that answers nothing and records the thread its batched
/// forward pass ran on. Standing in for GLiNER is the point: what is under
/// test is the thread, not the spans.
#[derive(Default)]
struct ThreadRecordingModel {
    ran_on: Mutex<Option<ThreadId>>,
}

impl ThreadRecordingModel {
    fn ran_on(&self) -> Option<ThreadId> {
        *self.ran_on.lock().expect("thread record")
    }
}

impl ClassifierModel for ThreadRecordingModel {
    fn predict(
        &self,
        _text: &str,
        _labels: &[&str],
    ) -> Result<Vec<ClassifiedSpan>, ExtractorError> {
        Ok(Vec::new())
    }

    fn predict_batch(
        &self,
        inputs: &[(&str, &[&str])],
    ) -> Result<Vec<Vec<ClassifiedSpan>>, ExtractorError> {
        *self.ran_on.lock().expect("thread record") = Some(std::thread::current().id());
        Ok(inputs.iter().map(|_| Vec::new()).collect())
    }

    fn version(&self) -> &str {
        "thread-recording"
    }
}

/// A persisted classifier-kind row, the shape the shard materialises from.
fn classifier_row() -> ExtractorDefinition {
    let ast = ExtractorDef {
        name: "basic_ner".into(),
        kind: ExtractorKindAst::Classifier,
        target: AstTarget::Entity {
            entity_type: "brain:Person".into(),
        },
        fields: vec![
            ExtractorField::Model("brain-basic-ner-v1".into()),
            ExtractorField::ConfidenceThreshold(0.6),
        ],
    };
    ExtractorDefinition::new(
        ExtractorId::from(1),
        "brain".into(),
        "basic_ner".into(),
        ExtractorKind::Classifier,
        1,
        serde_json::to_vec(&ast).expect("encode definition blob"),
        0,
    )
}

fn memory(text: &str) -> Memory {
    Memory {
        id: MemoryId::pack(0, 1, 0),
        space: SpaceId::new(),
        session_id: SessionId(0),
        kind: MemoryKind::Episodic,
        salience: Salience::default(),
        text: Some(text.into()),
        created_at_unix_ms: 0,
        last_accessed_at_unix_ms: 0,
        occurred_at_unix_nanos: None,
    }
}

/// Materialise the tier the way a shard does, drive one micro-batch on a
/// glommio executor, and report the thread the executor ran on alongside the
/// thread the forward pass ran on.
fn drive_one_batch(model: Arc<ThreadRecordingModel>, offload_wired: bool) -> (ThreadId, ThreadId) {
    let deps = MaterializeDeps {
        classifier_model: Some(model.clone()),
        classifier_offload: if offload_wired {
            Some(Arc::new(GlommioClassifierOffload))
        } else {
            None
        },
        entity_type_qnames: Arc::new(vec!["brain:Person".to_string()]),
        ..MaterializeDeps::default()
    };
    let (registry, errors) = build_registry_from_definitions(&[classifier_row()], &deps);
    assert!(errors.is_empty(), "registry build errors: {errors:?}");
    let ext = registry
        .lookup(ExtractorId::from(1))
        .expect("classifier row registered");

    let mems = vec![memory("Priya Sharma joined the platform team")];
    let empty = ExtractorRegistry::new();

    // `LocalExecutor::run` drives the future on this thread, so the reactor
    // thread is the test thread. It is read from inside the executor rather
    // than assumed.
    let executor = glommio::LocalExecutor::default();
    let (reactor_thread, results) = executor.run(async {
        let ctx = ExtractionContext {
            memory_namespaces: None,
            declared_entity_types: None,
            candidate_predicates: None,
            declared_kinds: None,
            entity_type_labels: None,
            schema_version: 1,
            now_unix_nanos: 0,
            registry: &empty,
            prior_tier_items: None,
            extractor_context: None,
        };
        let results = ext.run_batch(&ctx, &mems).await;
        (std::thread::current().id(), results)
    });

    assert_eq!(results.len(), 1, "one result per memory");
    let ran_on = model.ran_on().expect("the forward pass must have run");
    (reactor_thread, ran_on)
}

#[test]
fn an_offloaded_forward_pass_leaves_the_reactor_thread() {
    let model = Arc::new(ThreadRecordingModel::default());
    let (reactor_thread, ran_on) = drive_one_batch(model, true);
    assert_ne!(
        ran_on, reactor_thread,
        "the forward pass ran on the reactor thread, so every request on this \
         shard waits out its 3.8 s",
    );
}

#[test]
fn without_an_offloader_the_forward_pass_holds_the_reactor_thread() {
    let model = Arc::new(ThreadRecordingModel::default());
    let (reactor_thread, ran_on) = drive_one_batch(model, false);
    assert_eq!(
        ran_on, reactor_thread,
        "with no offloader the pass runs inline; if it does not, the sibling \
         test proves nothing",
    );
}
