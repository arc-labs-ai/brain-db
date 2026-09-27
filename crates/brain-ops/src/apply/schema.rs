//! Apply schema-shaped phases: `UpsertSchema` — the single phase behind
//! every schema mutation (UPLOAD / REPLACE / DROP).
//!
//! It reconstructs the schema document from `Phase::UpsertSchema.blob`
//! (DSL source for UPLOAD / REPLACE, re-parsed here; serde_json for a
//! targeted DROP, whose narrowed form has no DSL source), applies the
//! destructive delta first (REPLACE's `replace_all` drops all declared
//! vocabulary; DROP's `drops` removes specific targets), then delegates to
//! [`brain_metadata::schema::store::schema_upload`] which atomically writes
//! the schema-version row, updates the active-version pointer, fans out
//! predicate/relation-type/entity-type/extractor interns, and re-flags
//! pre-existing statements outside the new vocabulary — all inside the same
//! wtxn. Recovery runs the identical sequence, so replay converges on the
//! same state the live write produced.

use brain_metadata::extractor::ops::extractor_drop_namespace;
use brain_metadata::relation::types::{relation_type_drop_one, relation_type_drop_schema_declared};
use brain_metadata::schema::predicate::{predicate_drop_one, predicate_drop_schema_declared};
use brain_metadata::schema::store::schema_upload;
use brain_protocol::schema::{parse_schema, validate};
use brain_protocol::schema_drop_target;
use redb::WriteTransaction;

use super::ApplyError;
use crate::write::{Phase, PhaseAck, Write};

/// Apply [`Phase::UpsertSchema`].
///
/// `Phase.blob` carries the raw DSL source text as UTF-8 bytes. The
/// handler already parsed + validated before calling submit; we
/// re-parse + re-validate as a safety check (deterministic + cheap;
/// a divergence here indicates a build-time bug in the protocol
/// crate). The actual persistence + fan-out lives in
/// `brain_metadata::schema::store::schema_upload` so the write path
/// shares one canonical implementation with the system-schema
/// bootstrap.
pub fn apply_upsert_schema(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::UpsertSchema {
        blob,
        created_at_unix_nanos,
        replace_all,
        drops,
        ..
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected UpsertSchema"));
    };

    // Blob format is chosen by the op: UPLOAD / REPLACE carry the DSL source
    // text (re-parsed here, so replay stays authoritative against the running
    // binary's parser); targeted DROP carries the already-narrowed schema as
    // serde_json (its narrowed form has no DSL source to re-parse), keyed off
    // a non-empty `drops` delta. Both paths yield a `Schema` that is validated
    // uniformly below.
    let parsed = if drops.is_empty() {
        let source = std::str::from_utf8(blob).map_err(|e| {
            ApplyError::Invariant(format!("UpsertSchema blob is not UTF-8 source text: {e}"))
        })?;
        parse_schema(source)
            .map_err(|e| ApplyError::Invariant(format!("UpsertSchema re-parse failed: {e:?}")))?
    } else {
        serde_json::from_slice(blob).map_err(|e| {
            ApplyError::Invariant(format!(
                "UpsertSchema drop blob is not valid schema json: {e}"
            ))
        })?
    };
    let validated = validate(&parsed).map_err(|errs| {
        ApplyError::Invariant(format!("UpsertSchema re-validate failed: {errs:?}"))
    })?;

    let namespace = validated.as_schema().namespace.clone();

    // Destructive delta first, then the (additive) upload — the same order the
    // REPLACE / DROP handlers use, so live apply and WAL replay converge on
    // identical state. UPLOAD carries an empty delta and skips both branches.
    let dropped = apply_schema_delta(wtxn, &namespace, *replace_all, drops)?;

    let version = schema_upload(wtxn, &validated, *created_at_unix_nanos)
        .map_err(|e| ApplyError::Metadata(format!("schema_upload: {e}")))?;

    Ok(PhaseAck::UpsertedSchema {
        namespace,
        version,
        dropped: u32::try_from(dropped).unwrap_or(u32::MAX),
    })
}

/// Apply the destructive schema delta carried on an `UpsertSchema` phase.
///
/// `replace_all` (SCHEMA_REPLACE) drops every declared predicate / relation
/// type / extractor in the namespace; `drops` (SCHEMA_DROP) removes specific
/// declared targets. Both run before the additive `schema_upload`, so a plain
/// UPLOAD (empty delta) is a no-op here. Shared by the live apply path and WAL
/// recovery so the two never diverge.
fn apply_schema_delta(
    wtxn: &WriteTransaction,
    namespace: &str,
    replace_all: bool,
    drops: &[(u8, String)],
) -> Result<usize, ApplyError> {
    let mut dropped = 0usize;
    if replace_all {
        dropped += predicate_drop_schema_declared(wtxn, namespace)
            .map_err(|e| ApplyError::Metadata(format!("predicate drop-all: {e}")))?;
        dropped += relation_type_drop_schema_declared(wtxn, namespace)
            .map_err(|e| ApplyError::Metadata(format!("relation_type drop-all: {e}")))?;
        dropped += extractor_drop_namespace(wtxn, namespace)
            .map_err(|e| ApplyError::Metadata(format!("extractor drop-all: {e}")))?;
    }
    for (kind, name) in drops {
        match *kind {
            schema_drop_target::PREDICATE => {
                if predicate_drop_one(wtxn, namespace, name)
                    .map_err(|e| ApplyError::Metadata(format!("predicate drop {name:?}: {e}")))?
                    .is_some()
                {
                    dropped += 1;
                }
            }
            schema_drop_target::RELATION_TYPE => {
                if relation_type_drop_one(wtxn, namespace, name)
                    .map_err(|e| ApplyError::Metadata(format!("relation_type drop {name:?}: {e}")))?
                    .is_some()
                {
                    dropped += 1;
                }
            }
            other => {
                return Err(ApplyError::Invariant(format!(
                    "UpsertSchema carried unknown drop kind {other}"
                )));
            }
        }
    }
    Ok(dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_metadata::MetadataDb;
    use tempfile::TempDir;

    use crate::write::{Phase, Write, WriteId};

    #[test]
    fn upsert_schema_round_trips_and_increments_version() {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();

        let source = r#"
namespace acme

define entity_type Project {
}
"#;
        let phase = Phase::UpsertSchema {
            namespace: "acme".into(),
            version: 1,
            blob: source.as_bytes().to_vec(),
            declared_predicates: Vec::new(),
            declared_relation_types: Vec::new(),
            declared_entity_types: Vec::new(),
            created_at_unix_nanos: 1_700_000_000_000,
            replace_all: false,
            drops: Vec::new(),
        };
        let write = Write::single(
            WriteId::new(),
            brain_core::SpaceId::default(),
            phase.clone(),
        );

        {
            let wtxn = db.write_txn().unwrap();
            let ack = apply_upsert_schema(&wtxn, &phase, &write).unwrap();
            assert_eq!(
                ack,
                PhaseAck::UpsertedSchema {
                    namespace: "acme".into(),
                    version: 1,
                    dropped: 0,
                }
            );
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        let active = brain_metadata::schema::store::schema_active(&rtxn, "acme").unwrap();
        assert_eq!(active, Some(1));
    }
}
