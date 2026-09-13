//! `SCHEMA_DROP` handler — the surgical counterpart to the
//! namespace-wide `SCHEMA_REPLACE`.
//!
//! Removes (narrows) a single declared predicate or relation_type from
//! the active schema set, then bumps the namespace to a new schema
//! version whose document no longer declares the dropped type. Everything
//! else about the namespace's declared vocabulary is left untouched — the
//! difference from `SCHEMA_REPLACE`, which wipes the whole namespace and
//! re-applies a fresh document.
//!
//! Entity types are deliberately not droppable: they are global in the v1
//! storage model (no namespace key), so dropping one would race rows in
//! other namespaces that reference the same shared type — the same reason
//! `SCHEMA_REPLACE` never drops them. The handler rejects an
//! entity-type target with `InvalidRequest`.
//!
//! Safety posture (mirrors `SCHEMA_REPLACE`'s explicit-confirmation
//! contract for an irreversible operation): dropping a type that still
//! has live (non-tombstoned) rows requires `force: true`. With live rows
//! present and `force` unset the handler rejects with `Conflict` and
//! mutates nothing. A type with no live rows drops without `force`. Rows
//! on a dropped type stay as orphans — readable as plain memories, no
//! longer enriched from the typed-graph tables — exactly as under
//! `SCHEMA_REPLACE`.
//!
//! All work commits inside a single redb wtxn so the narrow + re-version
//! is atomic. Idempotency, tenant binding, and the post-commit
//! OUTSIDE_ACTIVE_SCHEMA flag-sweep all follow the `SCHEMA_REPLACE`
//! handler's shape.

use brain_core::RequestId;
use brain_metadata::relation::ops::relation_live_count_by_type;
use brain_metadata::relation::types::relation_type_id_by_qname;
use brain_metadata::schema::predicate::predicate_id_by_qname;
use brain_metadata::schema::store::schema_active_row;
use brain_metadata::statement::crud::statement_live_count_by_predicate;
use brain_protocol::schema::{validate, Schema, SchemaItem};
use brain_protocol::{schema_drop_target, SchemaDropRequest, SchemaDropResponse};

use crate::context::OpsContext;
use crate::error::OpError;
use crate::write::{Phase, PhaseAck, Write, WriteId};

/// Cap on the in-use scan: the safety gate only needs to know "any live
/// rows?", so it stops at the first hit rather than counting an entire
/// namespace's statements. The reported `live_rows` is therefore `1` for
/// any in-use type — enough to drive the force gate and the client
/// message without an unbounded scan on the ack path.
const LIVE_ROW_SCAN_CAP: usize = 1;

/// Handle a `SCHEMA_DROP` request. Admin-only at the dispatch layer;
/// this function trusts the caller to be authorised.
pub async fn handle_schema_drop(
    req: SchemaDropRequest,
    ctx: &OpsContext,
) -> Result<SchemaDropResponse, OpError> {
    // 1. Target kind must be one we support. Entity types are global in
    //    v1 and are never dropped (see module docs).
    if req.target_kind != schema_drop_target::PREDICATE
        && req.target_kind != schema_drop_target::RELATION_TYPE
    {
        return Err(OpError::InvalidRequest(format!(
            "schema_drop: unsupported target_kind {} (0 = predicate, 1 = relation_type; \
             entity types are global in v1 and cannot be dropped)",
            req.target_kind
        )));
    }
    if req.target_name.is_empty() {
        return Err(OpError::InvalidRequest(
            "schema_drop: target_name must be non-empty".into(),
        ));
    }

    // 2. Tenant binding. A caller may narrow schema only for their own
    //    namespace. Reject a request targeting any other namespace before
    //    opening the write txn, so a cross-tenant drop can neither mutate a
    //    foreign tenant's vocabulary nor use the response as an existence
    //    oracle. The seeded `brain` system namespace is never a user's own
    //    name, so this also blocks narrowing the system schema.
    let caller_name = crate::handlers::schema::caller_namespace_name(ctx)?;
    if req.namespace != caller_name {
        return Err(OpError::Unauthorized(format!(
            "schema_drop: caller in namespace {caller_name:?} cannot drop from namespace {:?}",
            req.namespace
        )));
    }
    let namespace = req.namespace.clone();
    let caller_ns_id = ctx.executor.caller_namespace.raw();

    // 3. Read-only pre-checks against committed state. The actual narrow +
    //    re-version is then applied through the unified submit(Write) path so
    //    it is WAL-durable and replayed on recovery (the WAL is the source of
    //    truth; a redb-only mutation is lost if redb is ever rebuilt from it).
    let now = crate::txn::now_unix_nanos_pub();

    // 3a. Load the active schema document. No active version → nothing is
    //     declared, so the target cannot be dropped: a no-op success,
    //     mirroring FORGET's leniency on an already-absent target.
    let (current_version, mut schema): (u32, Schema) = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let Some(active_row) = schema_active_row(&rtxn, &namespace)
            .map_err(|e| OpError::Internal(format!("schema_active_row: {e}")))?
        else {
            return Ok(not_declared_response(&req, 0));
        };
        let schema: Schema = serde_json::from_slice(&active_row.source)
            .map_err(|e| OpError::Internal(format!("decode stored schema document: {e}")))?;
        (active_row.version, schema)
    };

    // 3b. Find the declared item in the document (the authority for "what is
    //     declared"). Absent → no-op success (idempotent re-drop).
    let target_kind = req.target_kind;
    let target_name = req.target_name.as_str();
    let item_idx = schema
        .items
        .iter()
        .position(|item| match (target_kind, item) {
            (schema_drop_target::PREDICATE, SchemaItem::Predicate(p)) => p.name == target_name,
            (schema_drop_target::RELATION_TYPE, SchemaItem::RelationType(r)) => {
                r.name == target_name
            }
            _ => false,
        });
    let Some(item_idx) = item_idx else {
        return Ok(not_declared_response(&req, current_version));
    };

    // 3c. In-use safety gate. Count live (non-tombstoned) rows keyed on the
    //     target; reject if any exist and `force` is unset. These helpers take
    //     a write txn but only read — open one for the reads and drop it
    //     (rolling back, writing nothing) before submit opens its own.
    //     Single-writer-per-shard means no other writer runs in between, so the
    //     gate can't go stale.
    let live_rows = {
        let wtxn = ctx
            .executor
            .metadata
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;
        let n = match target_kind {
            schema_drop_target::PREDICATE => {
                match predicate_id_by_qname(&wtxn, &namespace, target_name)
                    .map_err(|e| OpError::Internal(format!("predicate_id_by_qname: {e}")))?
                {
                    Some(pid) => statement_live_count_by_predicate(
                        &wtxn,
                        caller_ns_id,
                        pid,
                        LIVE_ROW_SCAN_CAP,
                    )
                    .map_err(|e| OpError::Internal(format!("live statement count: {e}")))?,
                    None => 0,
                }
            }
            schema_drop_target::RELATION_TYPE => {
                match relation_type_id_by_qname(&wtxn, &namespace, target_name)
                    .map_err(|e| OpError::Internal(format!("relation_type_id_by_qname: {e}")))?
                {
                    Some(rid) => {
                        relation_live_count_by_type(&wtxn, caller_ns_id, rid, LIVE_ROW_SCAN_CAP)
                            .map_err(|e| OpError::Internal(format!("live relation count: {e}")))?
                    }
                    None => 0,
                }
            }
            // Unreachable: guarded at step 1.
            _ => 0,
        };
        drop(wtxn); // read-only: roll back, write nothing.
        n
    };
    if live_rows > 0 && !req.force {
        return Err(OpError::Conflict(format!(
            "schema_drop: {} {:?} still has live rows in namespace {namespace:?}; \
             set force to drop it and leave those rows as orphans",
            target_kind_label(target_kind),
            target_name
        )));
    }

    // 4. Narrow the document and route the drop through submit(Write). The
    //    narrowed document (item removed, DSL source cleared) is carried as
    //    serde_json — it has no DSL text to re-parse — and the `drops` delta
    //    tells apply which typed-graph table row to remove. Live apply and WAL
    //    recovery reconstruct identical narrowed state; submit also supplies
    //    the WriteId idempotency replay and the post-commit flag sweep this
    //    handler used to hand-roll.
    schema.items.remove(item_idx);
    schema.source = None;
    let validated = validate(&schema).map_err(|errs| {
        OpError::Internal(format!(
            "schema_drop: narrowed document failed re-validation: {} error(s)",
            errs.len()
        ))
    })?;
    let narrowed_json = serde_json::to_vec(validated.as_schema())
        .map_err(|e| OpError::Internal(format!("encode narrowed schema: {e}")))?;

    let real_writer = crate::handlers::link::downcast_writer_pub(ctx)?;
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_schema_drop_request(&req);
    let phase = Phase::UpsertSchema {
        namespace: namespace.clone(),
        version: current_version.saturating_add(1),
        blob: narrowed_json,
        declared_predicates: Vec::new(),
        declared_relation_types: Vec::new(),
        declared_entity_types: Vec::new(),
        created_at_unix_nanos: now,
        replace_all: false,
        drops: vec![(target_kind, req.target_name.clone())],
    };
    let write =
        Write::single(write_id, ctx.executor.caller_space, phase).with_request_hash(request_hash);
    let ack = real_writer
        .submit(write)
        .await
        .map_err(crate::handlers::schema::map_writer_err)?;
    let (schema_version, dropped_n) = match ack.single_phase() {
        PhaseAck::UpsertedSchema {
            version, dropped, ..
        } => (*version, *dropped),
        other => {
            return Err(OpError::Internal(format!(
                "submit(UpsertSchema) returned unexpected PhaseAck: {other:?}"
            )))
        }
    };

    Ok(SchemaDropResponse {
        namespace,
        schema_version,
        target_kind,
        target_name: req.target_name.clone(),
        dropped: dropped_n > 0,
        live_rows: live_rows as u32,
        validation_errors: Vec::new(),
    })
}

/// A `SCHEMA_DROP` that matched no declared type: a success no-op. The
/// version is left where it was and `dropped` is false, so a client can
/// tell "nothing to do" from "narrowed to a new version".
fn not_declared_response(req: &SchemaDropRequest, current_version: u32) -> SchemaDropResponse {
    SchemaDropResponse {
        namespace: req.namespace.clone(),
        schema_version: current_version,
        target_kind: req.target_kind,
        target_name: req.target_name.clone(),
        dropped: false,
        live_rows: 0,
        validation_errors: Vec::new(),
    }
}

fn target_kind_label(kind: u8) -> &'static str {
    match kind {
        schema_drop_target::PREDICATE => "predicate",
        schema_drop_target::RELATION_TYPE => "relation_type",
        _ => "type",
    }
}

/// BLAKE3 over the canonical SCHEMA_DROP request fields. Excludes
/// `request_id` (the idempotency-table key). `force` folds in so a replay
/// that flips the confirmation flag is treated as a distinct intent.
fn hash_schema_drop_request(req: &SchemaDropRequest) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"schema_drop:");
    h.update(req.namespace.as_bytes());
    h.update(b"\0");
    h.update(&[req.target_kind]);
    h.update(b"\0");
    h.update(req.target_name.as_bytes());
    h.update(b"\0");
    h.update(&[u8::from(req.force)]);
    *h.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
    use brain_index::{IndexParams, SharedHnsw};
    use brain_metadata::schema::store::{schema_active, schema_upload as store_schema_upload};
    use brain_metadata::MetadataDb;
    use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
    use brain_protocol::schema::parse_schema;
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

    // Two predicates and one relation_type so a drop of one leaves the
    // others intact.
    const DOC: &str = "namespace acme\n\
        define entity_type Person { attributes {} }\n\
        define predicate prefers { kind: Preference object: Value<text> }\n\
        define predicate dislikes { kind: Preference object: Value<text> }\n\
        define relation_type mentors { from: Person to: Person cardinality: many-to-many }\n";

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

    fn seed(metadata: &SharedMetadataDb, doc: &str) {
        let parsed = parse_schema(doc).unwrap();
        let validated = validate(&parsed).unwrap();
        let wtxn = metadata.write_txn().unwrap();
        store_schema_upload(&wtxn, &validated, 1).unwrap();
        wtxn.commit().unwrap();
    }

    fn drop_req(kind: u8, name: &str, force: bool, rid: [u8; 16]) -> SchemaDropRequest {
        SchemaDropRequest {
            namespace: "acme".into(),
            target_kind: kind,
            target_name: name.into(),
            force,
            request_id: rid,
        }
    }

    fn active(metadata: &SharedMetadataDb) -> Option<u32> {
        let rtxn = metadata.read_txn().unwrap();
        schema_active(&rtxn, "acme").unwrap()
    }

    fn predicate_declared(metadata: &SharedMetadataDb, name: &str) -> bool {
        let wtxn = metadata.write_txn().unwrap();
        let got = predicate_id_by_qname(&wtxn, "acme", name).unwrap();
        // read-only use of a wtxn; drop without commit
        got.is_some()
    }

    #[tokio::test]
    async fn dropping_a_predicate_bumps_version_and_removes_only_that_predicate() {
        use brain_metadata::schema::predicate::predicates_active_for_schema;

        let (_dir, ctx, metadata) = build_ctx();
        seed(&metadata, DOC); // active version 1
        assert!(predicate_declared(&metadata, "prefers"));
        assert!(predicate_declared(&metadata, "dislikes"));

        let resp = handle_schema_drop(
            drop_req(schema_drop_target::PREDICATE, "prefers", false, [1u8; 16]),
            &ctx,
        )
        .await
        .expect("drop prefers");
        assert!(resp.dropped, "the declared predicate must be dropped");
        assert_eq!(resp.schema_version, 2, "a narrow bumps the version once");
        assert_eq!(active(&metadata), Some(2));

        // `prefers` is gone; `dislikes` survives.
        assert!(!predicate_declared(&metadata, "prefers"));
        assert!(predicate_declared(&metadata, "dislikes"));

        // The two conditions `handle_statement_create` checks to reject a
        // write against a non-declared predicate are now both true for
        // `prefers` under the new active version: the qname resolves to no
        // predicate row, and even if it did the active-schema set excludes
        // it. A subsequent STATEMENT_CREATE with `acme:prefers` therefore
        // fails with `PredicateNotInSchema`.
        let rtxn = metadata.read_txn().unwrap();
        let active_set = predicates_active_for_schema(&rtxn, "acme", 2).unwrap();
        let dislikes_id = {
            let wtxn = metadata.write_txn().unwrap();
            predicate_id_by_qname(&wtxn, "acme", "dislikes").unwrap()
        }
        .expect("dislikes still declared");
        assert!(
            active_set.contains(&dislikes_id),
            "surviving predicate stays in the active-schema set"
        );
        // Nothing in the active set is the dropped predicate: its row is
        // gone, so it cannot appear.
        let prefers_id = {
            let wtxn = metadata.write_txn().unwrap();
            predicate_id_by_qname(&wtxn, "acme", "prefers").unwrap()
        };
        assert!(
            prefers_id.is_none(),
            "dropped predicate no longer resolves — a write against it is rejected"
        );
    }

    #[tokio::test]
    async fn dropped_predicate_is_gone_from_the_new_version_document() {
        let (_dir, ctx, metadata) = build_ctx();
        seed(&metadata, DOC);

        handle_schema_drop(
            drop_req(schema_drop_target::PREDICATE, "prefers", false, [2u8; 16]),
            &ctx,
        )
        .await
        .expect("drop");

        // The new active version's document must not declare `prefers`.
        let rtxn = metadata.read_txn().unwrap();
        let row = schema_active_row(&rtxn, "acme").unwrap().unwrap();
        let schema: Schema = serde_json::from_slice(&row.source).unwrap();
        let still_there = schema
            .items
            .iter()
            .any(|i| matches!(i, SchemaItem::Predicate(p) if p.name == "prefers"));
        assert!(!still_there, "narrowed document must not declare `prefers`");
        // `dislikes` still declared.
        let dislikes = schema
            .items
            .iter()
            .any(|i| matches!(i, SchemaItem::Predicate(p) if p.name == "dislikes"));
        assert!(dislikes, "narrowed document must keep `dislikes`");
    }

    #[tokio::test]
    async fn dropping_a_relation_type_narrows_and_keeps_predicates() {
        let (_dir, ctx, metadata) = build_ctx();
        seed(&metadata, DOC);

        let resp = handle_schema_drop(
            drop_req(
                schema_drop_target::RELATION_TYPE,
                "mentors",
                false,
                [3u8; 16],
            ),
            &ctx,
        )
        .await
        .expect("drop relation_type");
        assert!(resp.dropped);
        assert_eq!(resp.schema_version, 2);

        let rtxn = metadata.read_txn().unwrap();
        let row = schema_active_row(&rtxn, "acme").unwrap().unwrap();
        let schema: Schema = serde_json::from_slice(&row.source).unwrap();
        assert!(
            !schema
                .items
                .iter()
                .any(|i| matches!(i, SchemaItem::RelationType(r) if r.name == "mentors")),
            "mentors must be gone"
        );
        assert!(
            schema
                .items
                .iter()
                .any(|i| matches!(i, SchemaItem::Predicate(p) if p.name == "prefers")),
            "predicates must survive a relation_type drop"
        );
    }

    #[tokio::test]
    async fn dropping_an_undeclared_type_is_a_noop_success() {
        let (_dir, ctx, metadata) = build_ctx();
        seed(&metadata, DOC); // version 1

        let resp = handle_schema_drop(
            drop_req(
                schema_drop_target::PREDICATE,
                "never_declared",
                false,
                [4u8; 16],
            ),
            &ctx,
        )
        .await
        .expect("no-op drop");
        assert!(!resp.dropped, "nothing was declared under that name");
        assert_eq!(
            resp.schema_version, 1,
            "a no-op drop must not bump the version"
        );
        assert_eq!(active(&metadata), Some(1));
    }

    #[tokio::test]
    async fn entity_type_target_is_rejected() {
        let (_dir, ctx, metadata) = build_ctx();
        seed(&metadata, DOC);
        // 2 is beyond the supported kinds; entity types are global in v1.
        let err = handle_schema_drop(drop_req(2, "Person", false, [5u8; 16]), &ctx)
            .await
            .expect_err("entity type / unknown kind must be rejected");
        assert!(matches!(err, OpError::InvalidRequest(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn cross_namespace_drop_is_rejected() {
        // Caller bound to `other`; request targets `acme`.
        let (_dir, ctx, metadata) = build_ctx_for("other");
        seed(&metadata, DOC);
        let err = handle_schema_drop(
            drop_req(schema_drop_target::PREDICATE, "prefers", false, [6u8; 16]),
            &ctx,
        )
        .await
        .expect_err("cross-namespace drop must be rejected");
        assert!(matches!(err, OpError::Unauthorized(_)), "got {err:?}");
        // acme's schema untouched.
        assert_eq!(active(&metadata), Some(1));
    }

    #[tokio::test]
    async fn same_request_id_same_params_replays() {
        let (_dir, ctx, metadata) = build_ctx();
        seed(&metadata, DOC);
        let rid = [7u8; 16];

        let first = handle_schema_drop(
            drop_req(schema_drop_target::PREDICATE, "prefers", false, rid),
            &ctx,
        )
        .await
        .expect("first drop");
        assert!(first.dropped);
        assert_eq!(first.schema_version, 2);

        // Replay after a completed drop: the predicate is already gone, so the
        // retry is a safe no-op success at the same version — nothing left to
        // drop and no re-narrow. This is the spec's idempotency contract
        // (retries are *safe*, "already done → success", like FORGET's
        // was_already_forgotten and already-tombstoned entities), not a
        // byte-cached echo of the first response.
        let second = handle_schema_drop(
            drop_req(schema_drop_target::PREDICATE, "prefers", false, rid),
            &ctx,
        )
        .await
        .expect("replay");
        assert!(
            !second.dropped,
            "replay of a completed drop reports nothing left to drop"
        );
        assert_eq!(
            second.schema_version, 2,
            "replay must not bump the version again"
        );
        assert_eq!(second.target_name, first.target_name);
        assert_eq!(active(&metadata), Some(2), "replay must not re-narrow");
    }

    /// Seed one live Preference statement on `acme:prefers` in the
    /// caller's namespace so the in-use safety gate has a live row to find.
    fn seed_live_prefers(metadata: &SharedMetadataDb, ns_id: brain_core::NamespaceId) {
        use brain_core::{
            Entity, EntityId, EntityType, EvidenceRef, ExtractorId, SessionId, Statement,
            StatementId, StatementKind, StatementObject, StatementValue, SubjectRef,
        };
        use brain_metadata::entity::ops::{entity_put, normalize_name};
        use brain_metadata::schema::predicate::predicate_id_by_qname;
        use brain_metadata::statement::crud::statement_create;
        use brain_metadata::tables::scope::RowScope;

        let scope = RowScope::from_bytes(ns_id.raw(), [0x11; 16]);
        let subject = EntityId::new();
        let wtxn = metadata.write_txn().unwrap();
        let entity = Entity::new_active(
            subject,
            EntityType::PERSON_ID,
            "Priya".into(),
            normalize_name("Priya"),
            1_700_000_000_000_000_000,
        );
        entity_put(&wtxn, scope, SessionId::DEFAULT, &entity).unwrap();
        let pred = predicate_id_by_qname(&wtxn, "acme", "prefers")
            .unwrap()
            .expect("prefers declared by seed");
        let s = Statement::new_root(
            StatementId::new(),
            StatementKind::Preference,
            SubjectRef::Entity(subject),
            pred,
            StatementObject::Value(StatementValue::Text("tea".into())),
            0.9,
            EvidenceRef::default(),
            ExtractorId::from(0),
            1_700_000_000_000_000_000,
            1,
        );
        statement_create(
            &wtxn,
            scope,
            SessionId::DEFAULT,
            &s,
            1_700_000_000_000_000_000,
        )
        .unwrap();
        wtxn.commit().unwrap();
    }

    #[tokio::test]
    async fn dropping_a_predicate_with_live_rows_requires_force() {
        let (_dir, ctx, metadata) = build_ctx();
        seed(&metadata, DOC);
        seed_live_prefers(&metadata, ctx.executor.caller_namespace);

        // Without force: refused with Conflict, nothing mutated.
        let err = handle_schema_drop(
            drop_req(schema_drop_target::PREDICATE, "prefers", false, [20u8; 16]),
            &ctx,
        )
        .await
        .expect_err("in-use predicate must not drop without force");
        assert!(matches!(err, OpError::Conflict(_)), "got {err:?}");
        assert_eq!(
            active(&metadata),
            Some(1),
            "refused drop must not re-version"
        );
        assert!(predicate_declared(&metadata, "prefers"), "still declared");

        // With force (and a fresh request_id): drops, leaving orphan rows.
        let resp = handle_schema_drop(
            drop_req(schema_drop_target::PREDICATE, "prefers", true, [21u8; 16]),
            &ctx,
        )
        .await
        .expect("force drop");
        assert!(resp.dropped);
        assert!(resp.live_rows >= 1, "force drop reports the orphaned rows");
        assert_eq!(resp.schema_version, 2);
        assert!(!predicate_declared(&metadata, "prefers"));
    }

    #[tokio::test]
    async fn same_request_id_different_params_conflicts() {
        let (_dir, ctx, metadata) = build_ctx();
        seed(&metadata, DOC);
        let rid = [8u8; 16];

        handle_schema_drop(
            drop_req(schema_drop_target::PREDICATE, "prefers", false, rid),
            &ctx,
        )
        .await
        .expect("first drop");

        let err = handle_schema_drop(
            drop_req(schema_drop_target::PREDICATE, "dislikes", false, rid),
            &ctx,
        )
        .await
        .expect_err("same request_id with different target must conflict");
        assert!(matches!(err, OpError::Conflict(_)), "got {err:?}");
    }
}
