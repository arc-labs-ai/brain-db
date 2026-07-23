//! Cross-namespace (tenant) isolation on the write+read path.
//!
//! Brain's namespace is the tenant data boundary: every memory row carries
//! its owning `namespace_id`, and a RECALL/QUERY must never surface a row that
//! belongs to a different namespace, on ANY retrieval lane.
//!
//! ## Why this test lives at the brain-ops (handler/dispatch) layer
//!
//! The analogous per-*space* proof (`brain-server/tests/space_isolation.rs`)
//! runs over the wire because the permissive (`AuthMethod::None`) handshake
//! lets a connection bind an arbitrary `space_id` directly. Namespace is
//! different: the caller's namespace is resolved at dispatch from the
//! *interned* namespace name carried on the API key (strict mode), and the
//! name must already be interned in the shard's metadata for the lookup to
//! resolve to a non-system id. The shared wire harness only opens its
//! `AuthStore` in permissive mode and exposes no hook to mint namespaced keys
//! or intern namespaces into a running shard's metadata, so a wire-level
//! strict-mode test cannot set two distinct caller namespaces today without
//! surgery on that shared scaffold.
//!
//! Rather than fake it, this test drives the *real* `dispatch` →
//! `handle_encode` / `handle_recall` path — the same code the wire layer
//! calls — but constructs two strict-mode `RequestCaller`s with different
//! interned namespaces directly. That is the lowest layer that can correctly
//! exercise distinct namespaces, and it exercises the production namespace
//! resolution (`dispatch` interns-lookup → `with_caller_namespace`) plus the
//! unconditional namespace wall on both the structured and semantic recall
//! lanes.
//!
//! The mock dispatcher derives a deterministic vector from text, so each
//! tenant's private memory is a strong semantic match for its own cue; the
//! test asserts purely on membership (which `memory_id`s appear), never on
//! score ordering.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::test_support::{run_in_glommio, single_body};
use brain_ops::{dispatch, OpsContext, RealWriterHandle, RequestCaller};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{EncodeRequest, RecallRequest, RequestBody};
use brain_protocol::envelope::response::{EncodeResponse, RecallResponseFrame, ResponseBody};

// ---------------------------------------------------------------------------
// Mock dispatcher: text-driven deterministic vectors (same shape as the
// recall integration test's mock).
// ---------------------------------------------------------------------------

struct MockDispatcher;

impl Dispatcher for MockDispatcher {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        let mut v = [0.0f32; VECTOR_DIM];
        for (i, byte) in text.as_bytes().iter().enumerate() {
            v[i % VECTOR_DIM] += f32::from(*byte) / 255.0;
        }
        Ok(v)
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    fn fingerprint(&self) -> [u8; 16] {
        [0xAB; 16]
    }
}

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

struct Fixture {
    ctx: OpsContext,
    tempdir: tempfile::TempDir,
    metadata: SharedMetadataDb,
}

impl Fixture {
    /// Rebuild the lexical lane from redb so recall sees a fully-indexed
    /// corpus the way a production shard does (no text-indexer worker runs
    /// in-process).
    fn reindex_lexical(&mut self) {
        self.ctx.lexical_retriever = brain_ops::test_support::reindex_memory_lexical_for_tests(
            self.tempdir.path(),
            &self.metadata,
        );
    }

    /// Intern a tenant namespace into the shared metadata, returning its
    /// stable id. This mirrors the AUTH path, which interns a key's
    /// namespace so `dispatch` can resolve the name to its id.
    fn intern_namespace(&self, name: &str) -> brain_core::NamespaceId {
        let wtxn = self.metadata.write_txn().expect("write txn");
        let id = brain_metadata::namespace::namespace_intern_or_get(&wtxn, name, 0)
            .expect("intern namespace");
        wtxn.commit().expect("commit");
        id
    }
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());

    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));

    let embedder: Arc<dyn Dispatcher> = Arc::new(MockDispatcher);
    let executor = ExecutorContext::new(
        embedder,
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );

    Fixture {
        ctx: brain_ops::test_support::ops_context_for_tests(executor, tempdir.path()),
        tempdir,
        metadata,
    }
}

/// Both tenants share ONE space id so default space-scoping can never be
/// what hides the foreign memory — only the namespace wall can. This makes the
/// proof specifically about the tenant boundary, not the (separately tested)
/// per-space boundary.
const SHARED_SPACE: [u8; 16] = [0xCD; 16];

/// A caller bound to `namespace`, carrying the shared space. Going through
/// `from_scope` (the production key-resolution path) with a real space id makes
/// `dispatch` resolve the namespace name to its interned id and stamp it onto
/// the executor — the production tenant-resolution path.
fn caller_for(namespace: &str) -> RequestCaller {
    let space = brain_core::SpaceId(uuid::Uuid::from_bytes(SHARED_SPACE));
    RequestCaller::from_scope(
        space,
        [0u8; 16],
        [0u8; 16],
        namespace.to_string(),
        brain_metadata::api_keys::bits::FULL,
    )
}

fn encode_req(request_id: [u8; 16], text: &str) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        context_id: 0,
        request_id,
        txn_id: None,
        occurred_at_unix_nanos: None,
        act_as: None,
        wait: brain_protocol::WaitMode::Ack,
        allow_duplicates: false,
    }
}

fn recall_req(cue: &str) -> RecallRequest {
    RecallRequest {
        trace: false,
        cue_text: cue.into(),
        subject_name: String::new(),
        max_results: 50,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        as_of_record_time_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        include_edges: false,
        include_graph: false,
        include_text: false,
        request_id: None,
        txn_id: None,
        // Default scope. To isolate the NAMESPACE wall from the space wall,
        // both tenants share one space id (see `SHARED_SPACE`), so default
        // space-scoping admits both rows on the space axis and only the
        // namespace wall can separate them. Cross-space opt-in is rejected
        // under scoped auth anyway, so we keep the default here.
        act_as: None,
    }
}

async fn encode(fix: &Fixture, caller: RequestCaller, request_id: [u8; 16], text: &str) -> u128 {
    let outcome = dispatch(
        RequestBody::Encode(encode_req(request_id, text)),
        caller,
        &fix.ctx,
    )
    .await
    .expect("encode dispatch");
    match single_body(outcome) {
        ResponseBody::Encode(EncodeResponse { memory_id, .. }) => memory_id,
        other => panic!("expected Encode response, got {other:?}"),
    }
}

async fn recall_ids(fix: &Fixture, caller: RequestCaller, cue: &str) -> Vec<u128> {
    let outcome = dispatch(RequestBody::Recall(recall_req(cue)), caller, &fix.ctx)
        .await
        .expect("recall dispatch");
    let frame: RecallResponseFrame = match single_body(outcome) {
        ResponseBody::Recall(r) => r,
        other => panic!("expected Recall response, got {other:?}"),
    };
    assert!(frame.is_final, "v1 RECALL response must be final");
    frame.memories.iter().map(|m| m.memory_id).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Two tenants (`acme`, `globex`) each store a private memory. A default
/// RECALL as one tenant must never return the other tenant's memory — even
/// when the cue is chosen to match the foreign memory's text and space
/// scoping is fully widened. The recall pipeline fans out to all three lanes
/// and the assertion holds for whichever lane surfaced a hit, so this covers
/// both the structured and the semantic vector path.
#[test]
fn recall_does_not_leak_across_namespaces() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();

        let acme = fix.intern_namespace("acme");
        let globex = fix.intern_namespace("globex");
        assert_ne!(acme, globex, "distinct tenants must intern to distinct ids");
        assert_ne!(
            acme,
            brain_core::NamespaceId::SYSTEM,
            "user namespace must not collapse to SYSTEM"
        );

        let acme_caller = caller_for("acme");
        let globex_caller = caller_for("globex");

        // Each tenant stores a private memory.
        let acme_mem = encode(
            &fix,
            acme_caller.clone(),
            [1; 16],
            "acme private: the launch code is hunter2",
        )
        .await;
        let globex_mem = encode(
            &fix,
            globex_caller.clone(),
            [2; 16],
            "globex private: review the quarterly design doc",
        )
        .await;
        assert_ne!(acme_mem, globex_mem);

        fix.reindex_lexical();

        // globex recalls with a cue aimed squarely at acme's private text.
        let globex_view =
            recall_ids(&fix, globex_caller.clone(), "private launch code hunter2").await;
        assert!(
            !globex_view.contains(&acme_mem),
            "TENANT BREACH: globex's recall returned acme's memory {acme_mem}; got {globex_view:?}"
        );
        for id in &globex_view {
            assert_eq!(
                *id, globex_mem,
                "globex recall returned an id it doesn't own: {id} (globex owns {globex_mem})"
            );
        }

        // Symmetrically, acme recalls with a cue aimed at globex's text.
        let acme_view = recall_ids(&fix, acme_caller.clone(), "quarterly design doc review").await;
        assert!(
            !acme_view.contains(&globex_mem),
            "TENANT BREACH: acme's recall returned globex's memory {globex_mem}; got {acme_view:?}"
        );
        for id in &acme_view {
            assert_eq!(
                *id, acme_mem,
                "acme recall returned an id it doesn't own: {id} (acme owns {acme_mem})"
            );
        }
    })
}

/// A tenant CAN see its own memory — proves the wall above is the namespace
/// filter doing its job, not the fixture hiding everything. Each tenant
/// recalls its own private cue and must get its own memory back.
#[test]
fn recall_returns_own_namespace_memory() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();
        let _acme = fix.intern_namespace("acme");
        let _globex = fix.intern_namespace("globex");

        let acme_caller = caller_for("acme");
        let globex_caller = caller_for("globex");

        let acme_mem = encode(
            &fix,
            acme_caller.clone(),
            [1; 16],
            "acme private: the launch code is hunter2",
        )
        .await;
        let globex_mem = encode(
            &fix,
            globex_caller.clone(),
            [2; 16],
            "globex private: review the quarterly design doc",
        )
        .await;

        fix.reindex_lexical();

        let acme_view = recall_ids(&fix, acme_caller, "launch code hunter2").await;
        assert!(
            acme_view.contains(&acme_mem),
            "acme must see its own memory {acme_mem}; got {acme_view:?}"
        );

        let globex_view = recall_ids(&fix, globex_caller, "quarterly design doc").await;
        assert!(
            globex_view.contains(&globex_mem),
            "globex must see its own memory {globex_mem}; got {globex_view:?}"
        );
    })
}
