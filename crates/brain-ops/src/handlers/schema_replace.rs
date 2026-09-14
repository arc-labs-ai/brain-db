//! `SCHEMA_REPLACE` handler — destructive counterpart to the
//! associative-merge `SCHEMA_UPLOAD`.
//!
//! Replaces every schema-declared predicate, relation_type, and
//! extractor row in the target namespace with the supplied DSL.
//! Implicit-from-write rows (those created by schemaless
//! STATEMENT_CREATE / RELATION_CREATE before the schema landed) stay
//! put — they aren't part of the declared vocabulary. Entity types
//! are global in the v1 storage model (no namespace key) and are not
//! dropped: removing them would race with rows in other namespaces
//! that reference the same shared type.
//!
//! All work commits inside a single redb wtxn so the destructive
//! reset is atomic. If the new schema's apply step fails (e.g. an
//! Any-target relation_type pointing at a missing entity type), the
//! whole txn is dropped and the previous schema state survives.
//!
//! Requires `force_drop_existing: true`. The flag is the wire
//! contract's explicit-confirmation step for an irreversible
//! operation — a `false` value is rejected with `InvalidRequest` so
//! a typo in a client can't accidentally wipe a deployment's schema.

use brain_core::RequestId;
use brain_protocol::schema::{parse_schema, validate};
use brain_protocol::{SchemaReplaceRequest, SchemaReplaceResponse};

use crate::context::OpsContext;
use crate::error::OpError;
use crate::write::{Phase, PhaseAck, Write, WriteId};

/// Handle a `SCHEMA_REPLACE` request. Admin-only at the dispatch
/// layer; this function trusts the caller to be authorised.
pub async fn handle_schema_replace(
    req: SchemaReplaceRequest,
    ctx: &OpsContext,
) -> Result<SchemaReplaceResponse, OpError> {
    // 1. Confirmation flag — explicit, no defaults.
    if !req.force_drop_existing {
        return Err(OpError::InvalidRequest(
            "schema_replace: force_drop_existing must be true to confirm a destructive replace"
                .into(),
        ));
    }

    // 2. Document size cap — same as SCHEMA_UPLOAD.
    if req.schema_document.is_empty() {
        return Err(OpError::InvalidRequest(
            "schema_document must be non-empty".into(),
        ));
    }
    if req.schema_document.len() > crate::handlers::schema::MAX_SCHEMA_DOCUMENT_BYTES {
        return Err(OpError::InvalidRequest(format!(
            "schema_document exceeds cap ({} > {} bytes)",
            req.schema_document.len(),
            crate::handlers::schema::MAX_SCHEMA_DOCUMENT_BYTES,
        )));
    }

    // 3. Parse + validate. Failures don't return SchemaConflict /
    //    InvalidRequest — they ride on `validation_errors` in the
    //    response body, matching the SCHEMA_UPLOAD shape.
    let parsed = match parse_schema(&req.schema_document) {
        Ok(s) => s,
        Err(e) => {
            return Ok(SchemaReplaceResponse {
                namespace: String::new(),
                schema_version: 0,
                dropped_count: 0,
                validation_errors: vec![brain_protocol::SchemaValidationErrorWire {
                    code: "ParseError".into(),
                    message: e.to_string(),
                    line: 0,
                    column: 0,
                    length: 0,
                    severity: 2,
                }],
            });
        }
    };
    let validated = match validate(&parsed) {
        Ok(v) => v,
        Err(errs) => {
            return Ok(SchemaReplaceResponse {
                namespace: parsed.namespace.clone(),
                schema_version: 0,
                dropped_count: 0,
                validation_errors: errs
                    .iter()
                    .map(|e| brain_protocol::SchemaValidationErrorWire {
                        code: format!("{:?}", e.code),
                        message: e.message.clone(),
                        line: e.source_span.map(|s| s.line).unwrap_or(0),
                        column: e.source_span.map(|s| s.column).unwrap_or(0),
                        length: e.source_span.map(|s| s.length).unwrap_or(0),
                        severity: 2,
                    })
                    .collect(),
            });
        }
    };
    let namespace = validated.as_schema().namespace.clone();

    // 3a. Tenant binding. A caller may destructively replace schema only
    //     for their own namespace. Reject a DSL targeting any other
    //     namespace before opening the write txn, so a cross-tenant
    //     replace can neither mutate a foreign tenant's declared
    //     vocabulary nor use the `dropped_count` / conflict response as an
    //     existence oracle. The seeded `brain` system namespace is never a
    //     user's own name (dispatch refuses a caller that resolves to
    //     SYSTEM), so this also blocks replacing the system schema.
    let caller_name = crate::handlers::schema::caller_namespace_name(ctx)?;
    if namespace != caller_name {
        return Err(OpError::Unauthorized(format!(
            "schema_replace: caller in namespace {caller_name:?} cannot replace schema for namespace {namespace:?}"
        )));
    }

    // 4. Route the destructive replace through the unified submit(Write)
    //    path so it is WAL-durable and replayed on recovery: the WAL is the
    //    source of truth, and a redb-only mutation is silently lost if redb is
    //    ever rebuilt from the WAL. The `UpsertSchema` apply (with
    //    `replace_all`) drops every declared predicate / relation type /
    //    extractor in the namespace and then uploads the new document inside
    //    one wtxn — atomic, so a failing upload leaves the prior schema
    //    intact. submit also supplies the WriteId idempotency replay and the
    //    post-commit OUTSIDE_ACTIVE_SCHEMA flag sweep this handler used to
    //    hand-roll.
    let now = crate::txn::now_unix_nanos_pub();
    let from_version = crate::handlers::schema::current_active(ctx, &namespace)?.unwrap_or(0);
    let real_writer = crate::handlers::link::downcast_writer_pub(ctx)?;
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_schema_replace_request(&req);
    let phase = Phase::UpsertSchema {
        namespace: namespace.clone(),
        // Informational; apply recomputes the assigned version. Set to the
        // next version so the WAL body carries the value recovery's
        // skip-if-(namespace,version)-exists check compares against.
        version: from_version.saturating_add(1),
        blob: req.schema_document.as_bytes().to_vec(),
        declared_predicates: Vec::new(),
        declared_relation_types: Vec::new(),
        declared_entity_types: Vec::new(),
        created_at_unix_nanos: now,
        // REPLACE: drop all declared vocabulary before the (additive) upload.
        replace_all: true,
        drops: Vec::new(),
    };
    let write =
        Write::single(write_id, ctx.executor.caller_space, phase).with_request_hash(request_hash);
    let ack = real_writer
        .submit(write)
        .await
        .map_err(crate::handlers::schema::map_writer_err)?;
    let (schema_version, dropped_count) = match ack.single_phase() {
        PhaseAck::UpsertedSchema {
            version, dropped, ..
        } => (*version, *dropped),
        other => {
            return Err(OpError::Internal(format!(
                "submit(UpsertSchema) returned unexpected PhaseAck: {other:?}"
            )))
        }
    };

    Ok(SchemaReplaceResponse {
        namespace,
        schema_version,
        dropped_count,
        validation_errors: Vec::new(),
    })
}

/// BLAKE3 over the canonical SCHEMA_REPLACE request fields. Excludes
/// `request_id` (the idempotency-table key). `force_drop_existing` folds
/// in so a replay that flips the confirmation flag is treated as a
/// distinct intent; in practice the handler rejects `false` before this
/// runs, so the hash always covers a `true`.
fn hash_schema_replace_request(req: &SchemaReplaceRequest) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"schema_replace:");
    h.update(req.schema_document.as_bytes());
    h.update(b"\0");
    h.update(&[u8::from(req.force_drop_existing)]);
    *h.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
    use brain_index::{IndexParams, SharedHnsw};
    use brain_metadata::schema::store::{schema_active, schema_upload};
    use brain_metadata::MetadataDb;
    use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
    use std::sync::Arc;

    struct MockDispatcher;
    impl Dispatcher for MockDispatcher {
        fn embed(&self, _t: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            Ok([0.0; VECTOR_DIM])
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
            Ok(texts.iter().map(|_| [0.0; VECTOR_DIM]).collect())
        }
        fn embed_query(&self, _t: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            Ok([0.0; VECTOR_DIM])
        }
        fn fingerprint(&self) -> [u8; 16] {
            [0xAB; 16]
        }
    }

    const V1: &str =
        "namespace acme\ndefine predicate prefers { kind: Preference object: Value<text> }\n";
    const V2: &str =
        "namespace acme\ndefine predicate likes { kind: Preference object: Value<text> }\n";
    const V3: &str =
        "namespace acme\ndefine predicate loves { kind: Preference object: Value<text> }\n";

    /// Build a ctx whose authenticated caller is bound to the given
    /// namespace, interning it so the tenant-binding guard can resolve its
    /// name. The `acme` schemas seeded by these tests are declared under
    /// that same namespace, so the replace guard admits them.
    fn build_ctx_for(caller_ns: &str) -> (tempfile::TempDir, OpsContext, SharedMetadataDb) {
        let dir = tempfile::tempdir().unwrap();
        let metadata: SharedMetadataDb =
            Arc::new(MetadataDb::open(dir.path().join("meta.redb")).unwrap());
        let ns_id = {
            let wtxn = metadata.write_txn().unwrap();
            let id =
                brain_metadata::namespace::namespace_intern_or_get(&wtxn, caller_ns, 0).unwrap();
            wtxn.commit().unwrap();
            id
        };
        let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let writer = Arc::new(crate::writer::RealWriterHandle::new(
            metadata.clone(),
            hnsw_writer,
        ));
        let executor = ExecutorContext::new(
            Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
            shared,
            metadata.clone(),
            writer as Arc<dyn WriterHandle>,
        )
        .with_caller_namespace(ns_id);
        let ctx = crate::test_support::ops_context_for_tests(executor, dir.path());
        (dir, ctx, metadata)
    }

    fn build_ctx() -> (tempfile::TempDir, OpsContext, SharedMetadataDb) {
        build_ctx_for("acme")
    }

    /// Like [`build_ctx_for`] but wires a `schema_flag_sweep` channel onto
    /// the concrete `RealWriterHandle` before it's Arc-wrapped, and returns
    /// the receiver so a test can assert the post-commit sweep enqueue.
    fn build_ctx_with_sweep(
        caller_ns: &str,
    ) -> (
        tempfile::TempDir,
        OpsContext,
        SharedMetadataDb,
        flume::Receiver<crate::writer::SchemaFlagSweepJob>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let metadata: SharedMetadataDb =
            Arc::new(MetadataDb::open(dir.path().join("meta.redb")).unwrap());
        let ns_id = {
            let wtxn = metadata.write_txn().unwrap();
            let id =
                brain_metadata::namespace::namespace_intern_or_get(&wtxn, caller_ns, 0).unwrap();
            wtxn.commit().unwrap();
            id
        };
        let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let (tx, rx) = flume::unbounded::<crate::writer::SchemaFlagSweepJob>();
        let mut real = crate::writer::RealWriterHandle::new(metadata.clone(), hnsw_writer);
        real.set_schema_flag_sweep_sender(tx);
        let writer = Arc::new(real);
        let executor = ExecutorContext::new(
            Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
            shared,
            metadata.clone(),
            writer as Arc<dyn WriterHandle>,
        )
        .with_caller_namespace(ns_id);
        let ctx = crate::test_support::ops_context_for_tests(executor, dir.path());
        (dir, ctx, metadata, rx)
    }

    /// Seed an initial active schema directly through the storage helper
    /// (bypassing the upload handler) so the replace path has declared
    /// rows to drop.
    fn seed_schema(metadata: &SharedMetadataDb, doc: &str) {
        let parsed = parse_schema(doc).unwrap();
        let validated = validate(&parsed).unwrap();
        let wtxn = metadata.write_txn().unwrap();
        schema_upload(&wtxn, &validated, 1).unwrap();
        wtxn.commit().unwrap();
    }

    fn replace_req(doc: &str, rid: [u8; 16]) -> SchemaReplaceRequest {
        SchemaReplaceRequest {
            schema_document: doc.into(),
            force_drop_existing: true,
            request_id: rid,
        }
    }

    fn active(metadata: &SharedMetadataDb) -> Option<u32> {
        let rtxn = metadata.read_txn().unwrap();
        schema_active(&rtxn, "acme").unwrap()
    }

    #[tokio::test]
    async fn same_request_id_same_doc_replays_without_reapplying() {
        let (_dir, ctx, metadata) = build_ctx();
        seed_schema(&metadata, V1); // active version 1
        let rid = [7u8; 16];

        let first = handle_schema_replace(replace_req(V2, rid), &ctx)
            .await
            .expect("first replace");
        assert_eq!(first.schema_version, 2, "replace bumps version once");
        assert!(
            first.dropped_count >= 1,
            "the pre-existing declared predicate must be dropped"
        );
        let version_after_first = active(&metadata);
        assert_eq!(version_after_first, Some(2));

        // Replay: same request_id + same doc. Must return the cached
        // response and must NOT drop+re-upload (version stays at 2).
        let second = handle_schema_replace(replace_req(V2, rid), &ctx)
            .await
            .expect("replay");
        assert_eq!(
            first, second,
            "replay must return the byte-identical cached response"
        );
        assert_eq!(
            active(&metadata),
            version_after_first,
            "replay must not bump the schema version a second time"
        );
    }

    #[tokio::test]
    async fn cross_namespace_replace_is_rejected_without_mutation() {
        // Caller bound to `other`; the DSL declares `acme`. The replace
        // must be refused as Unauthorized before any drop/re-upload, and
        // acme's seeded schema must survive untouched.
        let (_dir, ctx, metadata) = build_ctx_for("other");
        seed_schema(&metadata, V1); // acme active version 1
        let before = active(&metadata);
        assert_eq!(before, Some(1));

        let err = handle_schema_replace(replace_req(V2, [3u8; 16]), &ctx)
            .await
            .expect_err("cross-namespace replace must be rejected");
        assert!(
            matches!(err, OpError::Unauthorized(_)),
            "expected Unauthorized, got {err:?}"
        );
        assert_eq!(
            active(&metadata),
            before,
            "a rejected cross-namespace replace must not mutate the foreign schema"
        );
    }

    #[tokio::test]
    async fn replace_enqueues_flag_sweep_for_new_version() {
        // Mirrors the SCHEMA_UPLOAD sweep test: after a destructive
        // SCHEMA_REPLACE drops the old predicate, a SchemaFlagSweepJob for
        // the affected namespace + new version must be enqueued so the
        // migration worker marks old-schema statements stale.
        let (_dir, ctx, metadata, rx) = build_ctx_with_sweep("acme");
        seed_schema(&metadata, V1); // active version 1 (declares `prefers`)

        let resp = handle_schema_replace(replace_req(V2, [11u8; 16]), &ctx)
            .await
            .expect("replace");
        assert_eq!(resp.schema_version, 2);
        assert!(
            resp.dropped_count >= 1,
            "the pre-existing `prefers` predicate must be dropped"
        );

        let job = rx
            .try_recv()
            .expect("SCHEMA_REPLACE must enqueue a flag-sweep job");
        assert_eq!(job.namespace, "acme");
        assert_eq!(
            job.new_version, 2,
            "sweep must target the version the replace committed"
        );
        // Exactly one job — no double-enqueue.
        assert!(
            rx.try_recv().is_err(),
            "a single replace must enqueue exactly one sweep"
        );
    }

    #[tokio::test]
    async fn replace_replay_does_not_reenqueue_flag_sweep() {
        // A same-request_id replay short-circuits at the idempotency check
        // and must NOT enqueue a second sweep.
        let (_dir, ctx, metadata, rx) = build_ctx_with_sweep("acme");
        seed_schema(&metadata, V1);
        let rid = [12u8; 16];

        handle_schema_replace(replace_req(V2, rid), &ctx)
            .await
            .expect("first replace");
        let _first_job = rx.try_recv().expect("first replace enqueues a sweep");

        handle_schema_replace(replace_req(V2, rid), &ctx)
            .await
            .expect("replay");
        assert!(
            rx.try_recv().is_err(),
            "replay must not enqueue a second sweep"
        );
    }

    #[tokio::test]
    async fn same_request_id_different_doc_conflicts() {
        let (_dir, ctx, metadata) = build_ctx();
        seed_schema(&metadata, V1); // active version 1
        let rid = [9u8; 16];

        handle_schema_replace(replace_req(V2, rid), &ctx)
            .await
            .expect("first replace");
        let version_after = active(&metadata);
        assert_eq!(version_after, Some(2));

        // Same request_id, different document → Conflict, no mutation.
        let err = handle_schema_replace(replace_req(V3, rid), &ctx)
            .await
            .expect_err("same request_id with different params must conflict");
        assert!(
            matches!(err, OpError::Conflict(_)),
            "expected Conflict, got {err:?}"
        );
        assert_eq!(
            active(&metadata),
            version_after,
            "a conflicting replay must not mutate the schema"
        );
    }
}
