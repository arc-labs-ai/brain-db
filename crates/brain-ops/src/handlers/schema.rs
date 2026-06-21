//! Schema wire-op handlers — `SCHEMA_UPLOAD / _GET / _LIST /
//! _VALIDATE`.
//!
//! Each handler:
//!
//! 1. Validates wire-layer input (1 MiB cap on `schema_document`).
//! 2. Parses via `brain_protocol::schema::parse_schema`.
//! 3. Validates via `brain_protocol::schema::validate`.
//! 4. For UPLOAD: opens a redb wtxn, calls
//!    `brain_metadata::schema::store::schema_upload`, commits,
//!    emits the `SchemaUpdated` subscription event.
//! 5. For GET / LIST / VALIDATE: opens an rtxn and reads.
//!
//! Parse/validate failures don't become `OpError`s — they ride in
//! the response body's `validation_errors` field with
//! `schema_version = 0`.

use brain_core::{Cardinality, RequestId, StatementKind};
use brain_metadata::entity::types::entity_type_lookup_by_name_rtxn;
use brain_metadata::extractor::ops::extractor_lookup_by_qname;
use brain_metadata::relation::types::relation_type_lookup_by_qname;
use brain_metadata::schema::predicate::predicate_lookup_by_qname;
use brain_metadata::schema::store::{schema_active, schema_get, schema_list, SchemaStoreError};
use brain_planner::WriterError;
use brain_protocol::envelope::response::EventType;
use brain_protocol::schema::{parse_schema, validate, ParseError, ValidationError};
use brain_protocol::schema::{
    CardinalityAst, ExtractorKindAst, ObjectTypeDecl, SchemaItem, StatementKindAst, ValidatedSchema,
};
use brain_protocol::{
    GraphEventPayload, SchemaGetRequest, SchemaGetResponse, SchemaListItemWire, SchemaListRequest,
    SchemaListResponseFrame, SchemaUpdatedEvent, SchemaUploadRequest, SchemaUploadResponse,
    SchemaValidateRequest, SchemaValidateResponse, SchemaValidationErrorWire,
};

use crate::context::OpsContext;
use crate::error::OpError;
use crate::handlers::entity::emit_graph_event;
use crate::handlers::link::downcast_writer_pub;
use crate::write::{Phase, PhaseAck, Write, WriteId};

/// 1 MiB cap on the uploaded schema document.
pub const MAX_SCHEMA_DOCUMENT_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// SCHEMA_UPLOAD
// ---------------------------------------------------------------------------

pub async fn handle_schema_upload(
    req: SchemaUploadRequest,
    ctx: &OpsContext,
) -> Result<SchemaUploadResponse, OpError> {
    check_document_cap(&req.schema_document)?;

    // 1. Parse.
    let schema = match parse_schema(&req.schema_document) {
        Ok(s) => s,
        Err(e) => return Ok(parse_failed_upload_response(e)),
    };

    // 2. Validate.
    let validated = match validate(&schema) {
        Ok(v) => v,
        Err(errs) => {
            return Ok(SchemaUploadResponse {
                namespace: schema.namespace.clone(),
                schema_version: 0,
                validation_errors: errs.iter().map(validation_error_to_wire).collect(),
                backward_compatible: true,
                migration_summary_blob: Vec::new(),
            });
        }
    };
    let namespace = validated.as_schema().namespace.clone();

    // 3. Dry-run → don't persist.
    if req.dry_run {
        let would_be = current_active(ctx, &namespace)?
            .unwrap_or(0)
            .saturating_add(1);
        return Ok(SchemaUploadResponse {
            namespace,
            schema_version: would_be,
            validation_errors: Vec::new(),
            backward_compatible: true,
            migration_summary_blob: Vec::new(),
        });
    }

    // 4. Associative-merge pre-flight against current state. For each
    //    declared item, classify as Insert / Idempotent / Conflict.
    //    Conflict aborts the upload before any commit; if every item is
    //    Idempotent we return the current active version without
    //    bumping (re-upload of an unchanged schema is a no-op so
    //    operators can safely re-apply the same DSL).
    let merge_summary = classify_schema_merge(ctx, &validated)?;
    if let (true, Some(version)) = (merge_summary.all_idempotent, merge_summary.current_version) {
        return Ok(SchemaUploadResponse {
            namespace,
            schema_version: version,
            validation_errors: Vec::new(),
            backward_compatible: true,
            migration_summary_blob: Vec::new(),
        });
    }

    // 5. Persist via the unified submit(Write) path.
    let now = crate::txn::now_unix_nanos_pub();
    let from_version = current_active(ctx, &namespace)?.unwrap_or(0);

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_schema_upload_request(&req);
    let phase = Phase::UpsertSchema {
        namespace: namespace.clone(),
        // Informational; apply::apply_upsert_schema recomputes inside
        // its wtxn via the canonical schema_store::schema_upload helper
        // (which calls next_version_in). Kept on the Phase for trace /
        // metric labels.
        version: from_version.saturating_add(1),
        // Source text — apply re-parses + re-validates. We deliberately
        // don't ship an rkyv-encoded ValidatedSchema through the Phase
        // because brain-ops doesn't carry the serde infrastructure;
        // parsing is cheap and SCHEMA_UPLOAD is an operator-rate op.
        blob: req.schema_document.as_bytes().to_vec(),
        declared_predicates: Vec::new(),
        declared_relation_types: Vec::new(),
        declared_entity_types: Vec::new(),
        created_at_unix_nanos: now,
    };
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    let new_version = match ack.single_phase() {
        PhaseAck::UpsertedSchema { version, .. } => *version,
        other => {
            return Err(OpError::Internal(format!(
                "submit(UpsertSchema) returned unexpected PhaseAck: {other:?}"
            )))
        }
    };

    // 5. Emit event post-commit.
    emit_graph_event(
        ctx,
        EventType::SchemaUpdated,
        GraphEventPayload::SchemaUpdated(SchemaUpdatedEvent {
            namespace: namespace.clone(),
            from_version,
            to_version: new_version,
            backward_compatible: true,
        }),
        now,
    )
    .await;

    Ok(SchemaUploadResponse {
        namespace,
        schema_version: new_version,
        validation_errors: Vec::new(),
        backward_compatible: true,
        migration_summary_blob: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// SCHEMA_GET
// ---------------------------------------------------------------------------

pub async fn handle_schema_get(
    req: SchemaGetRequest,
    ctx: &OpsContext,
) -> Result<SchemaGetResponse, OpError> {
    if req.namespace.is_empty() {
        return Err(OpError::InvalidRequest(
            "schema_get: namespace must be non-empty".into(),
        ));
    }
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    let resolved_version = if req.version == 0 {
        schema_active(&rtxn, &req.namespace)
            .map_err(map_schema_store_error)?
            .ok_or_else(|| OpError::NotFound {
                what: "schema",
                detail: format!("no active schema for namespace {:?}", req.namespace),
            })?
    } else {
        req.version
    };

    let row = schema_get(&rtxn, &req.namespace, resolved_version)
        .map_err(map_schema_store_error)?
        .ok_or_else(|| OpError::NotFound {
            what: "schema",
            detail: format!("namespace={:?} version={resolved_version}", req.namespace),
        })?;

    Ok(SchemaGetResponse {
        namespace: row.namespace,
        schema_version: row.version,
        schema_document: row.source_text.unwrap_or_default(),
        source_blob: row.source,
        uploaded_at_unix_nanos: row.uploaded_at_unix_nanos,
        validator_version: row.validator_version,
    })
}

// ---------------------------------------------------------------------------
// SCHEMA_LIST
// ---------------------------------------------------------------------------

pub async fn handle_schema_list(
    req: SchemaListRequest,
    ctx: &OpsContext,
) -> Result<SchemaListResponseFrame, OpError> {
    if req.namespace.is_empty() {
        return Err(OpError::InvalidRequest(
            "schema_list: namespace must be non-empty".into(),
        ));
    }
    let rows = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        schema_list(&rtxn, &req.namespace).map_err(map_schema_store_error)?
    };
    let items: Vec<SchemaListItemWire> = if req.limit == 0 {
        rows.iter()
            .map(|r| SchemaListItemWire {
                schema_version: r.version,
                uploaded_at_unix_nanos: r.uploaded_at_unix_nanos,
                validator_version: r.validator_version,
                has_source_text: r.source_text.is_some(),
            })
            .collect()
    } else {
        rows.iter()
            .take(req.limit as usize)
            .map(|r| SchemaListItemWire {
                schema_version: r.version,
                uploaded_at_unix_nanos: r.uploaded_at_unix_nanos,
                validator_version: r.validator_version,
                has_source_text: r.source_text.is_some(),
            })
            .collect()
    };
    let total = items.len() as u32;
    Ok(SchemaListResponseFrame {
        namespace: req.namespace,
        items,
        total,
        next_cursor: Vec::new(),
        is_final: true,
    })
}

// ---------------------------------------------------------------------------
// SCHEMA_VALIDATE
// ---------------------------------------------------------------------------

pub async fn handle_schema_validate(
    req: SchemaValidateRequest,
    ctx: &OpsContext,
) -> Result<SchemaValidateResponse, OpError> {
    check_document_cap(&req.schema_document)?;

    let schema = match parse_schema(&req.schema_document) {
        Ok(s) => s,
        Err(e) => {
            return Ok(SchemaValidateResponse {
                namespace: String::new(),
                would_be_version: 0,
                validation_errors: vec![parse_error_to_wire(e)],
            });
        }
    };

    match validate(&schema) {
        Ok(v) => {
            let namespace = v.as_schema().namespace.clone();
            let would_be = current_active(ctx, &namespace)?
                .unwrap_or(0)
                .saturating_add(1);
            Ok(SchemaValidateResponse {
                namespace,
                would_be_version: would_be,
                validation_errors: Vec::new(),
            })
        }
        Err(errs) => Ok(SchemaValidateResponse {
            namespace: schema.namespace,
            would_be_version: 0,
            validation_errors: errs.iter().map(validation_error_to_wire).collect(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// BLAKE3 over the canonical SCHEMA_UPLOAD request fields. Excludes
/// `request_id` (cache key) and `dry_run` (dry_run never reaches the
/// writer). `allow_breaking` folds in because flipping it counts as a
/// different operator intent and should surface as `Conflict` on
/// matching-request_id reuse.
fn hash_schema_upload_request(req: &SchemaUploadRequest) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"schema_upload:");
    h.update(req.schema_document.as_bytes());
    h.update(b"\0");
    h.update(&[u8::from(req.allow_breaking)]);
    *h.finalize().as_bytes()
}

fn check_document_cap(doc: &str) -> Result<(), OpError> {
    if doc.is_empty() {
        return Err(OpError::InvalidRequest(
            "schema_document must be non-empty".into(),
        ));
    }
    if doc.len() > MAX_SCHEMA_DOCUMENT_BYTES {
        return Err(OpError::InvalidRequest(format!(
            "schema_document exceeds cap ({} > {MAX_SCHEMA_DOCUMENT_BYTES} bytes)",
            doc.len()
        )));
    }
    Ok(())
}

/// Map `WriterError` from `submit(UpsertSchema)` back into the wire
/// taxonomy. Internal failures bubble as `OpError::Internal`; on a
/// genuine conflict from idempotency we surface `OpError::Conflict`.
fn map_writer_err(err: WriterError) -> OpError {
    match err {
        WriterError::Conflict(msg) => OpError::Conflict(msg),
        WriterError::Overloaded => OpError::Overloaded("writer overloaded".into()),
        WriterError::Internal(msg) => OpError::Internal(msg),
    }
}

fn current_active(ctx: &OpsContext, namespace: &str) -> Result<Option<u32>, OpError> {
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
    schema_active(&rtxn, namespace).map_err(map_schema_store_error)
}

fn map_schema_store_error(e: SchemaStoreError) -> OpError {
    match e {
        SchemaStoreError::VersionOverflow { namespace } => OpError::Conflict(format!(
            "schema_version overflow for namespace {namespace:?}"
        )),
        other => OpError::Internal(other.to_string()),
    }
}

fn parse_failed_upload_response(e: ParseError) -> SchemaUploadResponse {
    SchemaUploadResponse {
        namespace: String::new(),
        schema_version: 0,
        validation_errors: vec![parse_error_to_wire(e)],
        backward_compatible: true,
        migration_summary_blob: Vec::new(),
    }
}

fn parse_error_to_wire(e: ParseError) -> SchemaValidationErrorWire {
    let (code, line, col) = match &e {
        ParseError::Syntax { line, col, .. } => ("Syntax", *line, *col),
        ParseError::InvalidNumber { line, col, .. } => ("InvalidNumber", *line, *col),
        ParseError::InvalidJson { line, col, .. } => ("InvalidJson", *line, *col),
        ParseError::InvalidDuration { line, col, .. } => ("InvalidDuration", *line, *col),
        ParseError::InvalidCost { line, col, .. } => ("InvalidCost", *line, *col),
        ParseError::MissingField { line, col, .. } => ("MissingField", *line, *col),
    };
    SchemaValidationErrorWire {
        code: code.to_string(),
        message: e.to_string(),
        line: line as u32,
        column: col as u32,
        length: 0,
        severity: 2,
    }
}

fn validation_error_to_wire(e: &ValidationError) -> SchemaValidationErrorWire {
    let (line, column, length) = e
        .source_span
        .map(|s| (s.line, s.column, s.length))
        .unwrap_or((0, 0, 0));
    SchemaValidationErrorWire {
        code: format!("{:?}", e.code),
        message: e.message.clone(),
        line,
        column,
        length,
        severity: 2,
    }
}

// ---------------------------------------------------------------------------
// Associative-merge pre-flight.
// ---------------------------------------------------------------------------

/// Outcome of a single declaration's merge classification.
#[derive(Debug)]
struct MergeSummary {
    /// `Some` if the namespace already has a schema active.
    current_version: Option<u32>,
    /// `true` when every declared item already exists with matching
    /// constraints — the upload is a no-op.
    all_idempotent: bool,
}

/// Walk `validated.items` and classify each declaration against the
/// current persisted state. Returns the conflict-free summary; on the
/// first conflict produces `OpError::SchemaConflict` so the upload
/// aborts before any writer txn opens. This keeps the merge
/// all-or-nothing under the associative-merge contract: a single
/// conflict reverts the whole upload, the previous active version
/// remains live.
fn classify_schema_merge(
    ctx: &OpsContext,
    validated: &ValidatedSchema,
) -> Result<MergeSummary, OpError> {
    let schema = validated.as_schema();
    let namespace = schema.namespace.as_str();

    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    let current_version = schema_active(&rtxn, namespace).map_err(map_schema_store_error)?;

    let mut all_idempotent = true;
    for item in &schema.items {
        match item {
            SchemaItem::EntityType(e) => {
                let existing = entity_type_lookup_by_name_rtxn(&rtxn, &e.name)
                    .map_err(|err| OpError::Internal(format!("entity_type lookup: {err}")))?;
                match existing {
                    None => all_idempotent = false,
                    Some(row) => {
                        // Apply currently passes `Vec::new()` for the
                        // schema_blob, so any pre-existing row with an
                        // empty blob is a match. A non-empty blob from a
                        // prior cut would diverge — flag as conflict.
                        if !row.schema_blob.is_empty() {
                            return Err(OpError::SchemaConflict {
                                kind: "entity_type",
                                name: e.name.clone(),
                                namespace: namespace.to_string(),
                                conflict: "stored schema_blob is non-empty; merge requires a matching blob".to_string(),
                            });
                        }
                    }
                }
            }
            SchemaItem::Predicate(p) => {
                let existing = predicate_lookup_by_qname(&rtxn, namespace, &p.name)
                    .map_err(|err| OpError::Internal(format!("predicate lookup: {err}")))?;
                match existing {
                    None => all_idempotent = false,
                    Some(row) => {
                        let new_kind = map_statement_kind(p.kind);
                        let new_object = object_type_constraint_byte(&p.object);
                        let new_description = p.description.as_deref().unwrap_or("");
                        let new_stateful = p.resolved_stateful();
                        if row.kind_constraint != new_kind
                            || row.object_type_constraint_byte != new_object
                            || row.description != new_description
                            || row.is_stateful != new_stateful
                        {
                            let mut diff = Vec::new();
                            if row.kind_constraint != new_kind {
                                diff.push(format!(
                                    "kind: stored={:?} new={:?}",
                                    row.kind_constraint, new_kind
                                ));
                            }
                            if row.object_type_constraint_byte != new_object {
                                diff.push(format!(
                                    "object_type: stored={} new={}",
                                    row.object_type_constraint_byte, new_object
                                ));
                            }
                            if row.description != new_description {
                                diff.push("description differs".to_string());
                            }
                            if row.is_stateful != new_stateful {
                                diff.push(format!(
                                    "stateful: stored={} new={}",
                                    row.is_stateful, new_stateful
                                ));
                            }
                            return Err(OpError::SchemaConflict {
                                kind: "predicate",
                                name: p.name.clone(),
                                namespace: namespace.to_string(),
                                conflict: diff.join(", "),
                            });
                        }
                    }
                }
            }
            SchemaItem::RelationType(r) => {
                let existing = relation_type_lookup_by_qname(&rtxn, namespace, &r.name)
                    .map_err(|err| OpError::Internal(format!("relation_type lookup: {err}")))?;
                match existing {
                    None => all_idempotent = false,
                    Some(row) => {
                        // from/to entity types are resolved by name at
                        // apply time; we compare the declared names by
                        // re-resolving the stored row's ids. For the
                        // pre-flight a strict-equal name check is enough:
                        // identical declarations resolve to identical
                        // ids, divergent declarations either trip here or
                        // get caught at apply time.
                        let new_cardinality = map_cardinality(r.cardinality);
                        let new_description = r.description.as_deref().unwrap_or("");
                        if row.cardinality != new_cardinality
                            || row.is_symmetric != r.symmetric
                            || row.description != new_description
                        {
                            let mut diff = Vec::new();
                            if row.cardinality != new_cardinality {
                                diff.push(format!(
                                    "cardinality: stored={:?} new={:?}",
                                    row.cardinality, new_cardinality
                                ));
                            }
                            if row.is_symmetric != r.symmetric {
                                diff.push(format!(
                                    "symmetric: stored={} new={}",
                                    row.is_symmetric, r.symmetric
                                ));
                            }
                            if row.description != new_description {
                                diff.push("description differs".to_string());
                            }
                            return Err(OpError::SchemaConflict {
                                kind: "relation_type",
                                name: r.name.clone(),
                                namespace: namespace.to_string(),
                                conflict: diff.join(", "),
                            });
                        }
                    }
                }
            }
            SchemaItem::Extractor(e) => {
                let existing = extractor_lookup_by_qname(&rtxn, namespace, &e.name)
                    .map_err(|err| OpError::Internal(format!("extractor lookup: {err}")))?;
                match existing {
                    None => all_idempotent = false,
                    Some(row) => {
                        let new_kind = map_extractor_kind_byte(e.kind);
                        if row.kind != new_kind {
                            return Err(OpError::SchemaConflict {
                                kind: "extractor",
                                name: e.name.clone(),
                                namespace: namespace.to_string(),
                                conflict: format!("kind: stored={} new={}", row.kind, new_kind),
                            });
                        }
                        // Blob comparison (the encoded `ExtractorDef`)
                        // happens at apply time. Apply maps a mismatch
                        // to `ApplyError::Metadata` which surfaces as
                        // `OpError::Internal` — sub-optimal but rare;
                        // matching it precisely here would require
                        // re-encoding the AST as JSON, which couples
                        // brain-ops to serde_json for one pre-flight
                        // check.
                    }
                }
            }
            SchemaItem::Kind(_k) => {
                // Conservative pre-flight: treat a kind declaration as a
                // potential change. The authoritative idempotency /
                // conflict check is `kind_intern` at apply time, which
                // returns `KindOpError::Conflict` on a divergent
                // re-declaration.
                all_idempotent = false;
            }
        }
    }

    Ok(MergeSummary {
        current_version,
        all_idempotent,
    })
}

fn map_statement_kind(k: StatementKindAst) -> Option<StatementKind> {
    match k {
        StatementKindAst::Fact => Some(StatementKind::Fact),
        StatementKindAst::Preference => Some(StatementKind::Preference),
        StatementKindAst::Event => Some(StatementKind::Event),
        StatementKindAst::Attribute => Some(StatementKind::Attribute),
        StatementKindAst::Relation => Some(StatementKind::Relation),
        StatementKindAst::Directive => Some(StatementKind::Directive),
        StatementKindAst::Any => None,
    }
}

fn object_type_constraint_byte(o: &ObjectTypeDecl) -> u8 {
    match o {
        ObjectTypeDecl::Any => 0,
        ObjectTypeDecl::Entity { .. } => 1,
        ObjectTypeDecl::Value { .. } => 2,
        ObjectTypeDecl::Memory => 3,
        ObjectTypeDecl::Statement => 4,
    }
}

fn map_cardinality(c: CardinalityAst) -> Cardinality {
    match c {
        CardinalityAst::OneToOne => Cardinality::OneToOne,
        CardinalityAst::OneToMany => Cardinality::OneToMany,
        CardinalityAst::ManyToOne => Cardinality::ManyToOne,
        CardinalityAst::ManyToMany => Cardinality::ManyToMany,
    }
}

fn map_extractor_kind_byte(k: ExtractorKindAst) -> u8 {
    match k {
        ExtractorKindAst::Pattern => brain_core::ExtractorKind::Pattern.as_u8(),
        ExtractorKindAst::Classifier => brain_core::ExtractorKind::Classifier.as_u8(),
        ExtractorKindAst::Llm => brain_core::ExtractorKind::Llm.as_u8(),
    }
}

// ---------------------------------------------------------------------------
// Tests — handler-level integration tests live in
// `crates/brain-server/tests/`. Pure-function helpers covered here.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use brain_protocol::schema::{SourceSpan, ValidationErrorCode};

    #[test]
    fn check_document_cap_rejects_empty_and_oversized() {
        assert!(check_document_cap("").is_err());
        let big = "x".repeat(MAX_SCHEMA_DOCUMENT_BYTES + 1);
        assert!(check_document_cap(&big).is_err());
        assert!(check_document_cap("namespace acme").is_ok());
    }

    #[test]
    fn parse_error_to_wire_carries_position() {
        let wire = parse_error_to_wire(ParseError::Syntax {
            line: 7,
            col: 3,
            message: "boom".into(),
        });
        assert_eq!(wire.code, "Syntax");
        assert_eq!(wire.line, 7);
        assert_eq!(wire.column, 3);
        assert_eq!(wire.severity, 2);
    }

    #[test]
    fn validation_error_to_wire_uses_span_when_present() {
        let e = ValidationError {
            code: ValidationErrorCode::DuplicateDefinition,
            message: "dup".into(),
            source_span: Some(SourceSpan {
                line: 4,
                column: 5,
                length: 6,
            }),
        };
        let wire = validation_error_to_wire(&e);
        assert_eq!(wire.code, "DuplicateDefinition");
        assert_eq!(wire.line, 4);
        assert_eq!(wire.column, 5);
        assert_eq!(wire.length, 6);
        assert_eq!(wire.severity, 2);
    }

    #[test]
    fn validation_error_to_wire_uses_zero_when_span_absent() {
        let e = ValidationError {
            code: ValidationErrorCode::NamespaceMissing,
            message: "missing".into(),
            source_span: None,
        };
        let wire = validation_error_to_wire(&e);
        assert_eq!(wire.line, 0);
        assert_eq!(wire.column, 0);
        assert_eq!(wire.length, 0);
    }

    #[test]
    fn parse_failed_upload_response_zero_version() {
        let resp = parse_failed_upload_response(ParseError::Syntax {
            line: 1,
            col: 1,
            message: "x".into(),
        });
        assert_eq!(resp.schema_version, 0);
        assert!(resp.namespace.is_empty());
        assert_eq!(resp.validation_errors.len(), 1);
    }
}
