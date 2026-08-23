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
use brain_metadata::extractor::ops::extractor_drop_namespace;
use brain_metadata::relation::types::relation_type_drop_schema_declared;
use brain_metadata::schema::predicate::predicate_drop_schema_declared;
use brain_metadata::schema::store::schema_upload;
use brain_metadata::tables::idempotency::{
    response_kind, IdempotencyEntry, DEFAULT_TTL_NANOS, IDEMPOTENCY_TABLE,
};
use brain_protocol::schema::{parse_schema, validate};
use brain_protocol::{SchemaReplaceRequest, SchemaReplaceResponse};
use redb::ReadableTable;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::write::WriteId;

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

    // 4. Idempotency key + request digest, derived exactly like the
    //    unified write path (`WriteId::from_request` + a per-op BLAKE3
    //    request hash). A retried SCHEMA_REPLACE carries the same
    //    `request_id`; folding the effective space into the key keeps the
    //    cache per-tenant so `act_as` can't leak a cached ack across a
    //    tenancy boundary.
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_schema_replace_request(&req);

    // 5. Atomic idempotency-check + drop-then-replace + stamp inside one
    //    redb wtxn. The idempotency row commits in the SAME transaction
    //    as the schema mutation, so a same-`request_id` retry after a
    //    lost ACK replays the cached response instead of re-running the
    //    destructive drop+re-upload, and a same-`request_id`/different-doc
    //    retry returns `Conflict` — both without ever touching the
    //    declared rows a second time.
    let now = crate::txn::now_unix_nanos_pub();
    let wtxn = ctx
        .executor
        .metadata
        .write_txn()
        .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;

    // 5a. Consult the durable idempotency table before mutating anything.
    //     A live (non-expired) row with a matching hash replays; a
    //     mismatching hash is a conflict; both drop the wtxn untouched.
    let cached_entry: Option<IdempotencyEntry> = {
        let idem_table = wtxn
            .open_table(IDEMPOTENCY_TABLE)
            .map_err(|e| OpError::Internal(format!("open IDEMPOTENCY_TABLE: {e}")))?;
        let guard = idem_table
            .get(write_id.to_bytes())
            .map_err(|e| OpError::Internal(format!("idempotency get: {e}")))?;
        guard.map(|row| row.value())
    };
    if let Some(entry) = cached_entry {
        if !entry.is_expired(now, DEFAULT_TTL_NANOS) {
            if entry.request_hash != request_hash {
                return Err(OpError::Conflict(format!(
                    "schema_replace: request_id replay with different params (namespace {namespace:?})"
                )));
            }
            return decode_replace_response(&entry.response_payload);
        }
    }

    // 5b. Destructive drop of the namespace's declared vocabulary. Entity
    //     types stay put (global, shared across namespaces).
    let mut dropped: usize = 0;
    dropped += predicate_drop_schema_declared(&wtxn, &namespace)
        .map_err(|e| OpError::Internal(format!("predicate drop: {e}")))?;
    dropped += relation_type_drop_schema_declared(&wtxn, &namespace)
        .map_err(|e| OpError::Internal(format!("relation_type drop: {e}")))?;
    dropped += extractor_drop_namespace(&wtxn, &namespace)
        .map_err(|e| OpError::Internal(format!("extractor drop: {e}")))?;

    // 5c. Persist the new schema version row and fan its declarations out
    //     through the same apply path SCHEMA_UPLOAD uses. With the prior
    //     declared rows gone, the apply runs against a clean slate and
    //     won't trip the constraint-mismatch check. `schema_upload`
    //     internally calls `apply_schema_definitions` so we don't invoke
    //     it twice.
    let new_version = schema_upload(&wtxn, &validated, now)
        .map_err(|e| OpError::Internal(format!("schema_upload: {e}")))?;

    let response = SchemaReplaceResponse {
        namespace,
        schema_version: new_version,
        dropped_count: dropped as u32,
        validation_errors: Vec::new(),
    };

    // 5d. Stamp the durable idempotency row in the same wtxn so the
    //     replay-guard commits atomically with the effect.
    let idem_entry = IdempotencyEntry {
        response_kind: response_kind::UNKNOWN,
        memory_id_bytes: None,
        response_payload: encode_replace_response(&response)?,
        request_hash,
        created_at_unix_nanos: now,
        lsn: 0,
    };
    {
        let mut idem_table = wtxn
            .open_table(IDEMPOTENCY_TABLE)
            .map_err(|e| OpError::Internal(format!("open IDEMPOTENCY_TABLE: {e}")))?;
        idem_table
            .insert(write_id.to_bytes(), &idem_entry)
            .map_err(|e| OpError::Internal(format!("idempotency insert: {e}")))?;
    }

    wtxn.commit()
        .map_err(|e| OpError::Internal(format!("commit: {e}")))?;

    Ok(response)
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

/// Encode a `SchemaReplaceResponse` for the idempotency table's opaque
/// `response_payload`. Replayed verbatim on a matching-request retry.
fn encode_replace_response(resp: &SchemaReplaceResponse) -> Result<Vec<u8>, OpError> {
    let mut buf = Vec::new();
    ciborium::into_writer(resp, &mut buf)
        .map_err(|e| OpError::Internal(format!("encode cached schema_replace response: {e}")))?;
    Ok(buf)
}

/// Decode a cached `SchemaReplaceResponse` from the idempotency table.
fn decode_replace_response(bytes: &[u8]) -> Result<SchemaReplaceResponse, OpError> {
    ciborium::from_reader(bytes)
        .map_err(|e| OpError::Internal(format!("decode cached schema_replace response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
    use brain_index::{IndexParams, SharedHnsw};
    use brain_metadata::schema::store::schema_active;
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
