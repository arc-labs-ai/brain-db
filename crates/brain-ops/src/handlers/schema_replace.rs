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

use brain_metadata::extractor::ops::extractor_drop_namespace;
use brain_metadata::relation::types::relation_type_drop_schema_declared;
use brain_metadata::schema::predicate::predicate_drop_schema_declared;
use brain_metadata::schema::store::schema_upload;
use brain_protocol::schema::{parse_schema, validate};
use brain_protocol::{SchemaReplaceRequest, SchemaReplaceResponse};

use crate::context::OpsContext;
use crate::error::OpError;

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

    // 4. Atomic drop-then-replace inside one redb wtxn.
    let now = crate::txn::now_unix_nanos_pub();
    let (dropped_count, new_version) = {
        let wtxn = ctx
            .executor
            .metadata
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;

        let mut dropped: usize = 0;
        dropped += predicate_drop_schema_declared(&wtxn, &namespace)
            .map_err(|e| OpError::Internal(format!("predicate drop: {e}")))?;
        dropped += relation_type_drop_schema_declared(&wtxn, &namespace)
            .map_err(|e| OpError::Internal(format!("relation_type drop: {e}")))?;
        dropped += extractor_drop_namespace(&wtxn, &namespace)
            .map_err(|e| OpError::Internal(format!("extractor drop: {e}")))?;

        // 5. Persist the new schema version row and fan its
        //    declarations out through the same apply path SCHEMA_UPLOAD
        //    uses. With the prior declared rows gone, the apply runs
        //    against a clean slate and won't trip the
        //    constraint-mismatch check. `schema_upload` internally
        //    calls `apply_schema_definitions` so we don't invoke it
        //    twice.
        let new_version = schema_upload(&wtxn, &validated, now)
            .map_err(|e| OpError::Internal(format!("schema_upload: {e}")))?;

        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;
        (dropped as u32, new_version)
    };

    Ok(SchemaReplaceResponse {
        namespace,
        schema_version: new_version,
        dropped_count,
        validation_errors: Vec::new(),
    })
}
