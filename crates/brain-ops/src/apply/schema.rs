//! Apply schema-shaped phases: `UpsertSchema`.
//!
//! `UpsertSchema` re-parses the DSL source text the handler stuffed
//! into `Phase::UpsertSchema.blob`, re-validates it (a cheap
//! deterministic safety check), then delegates to
//! [`brain_metadata::schema::store::schema_upload`] which atomically
//! writes the schema-version row, updates the active-version pointer,
//! fans out predicate/relation-type/entity-type/extractor interns, and
//! re-flags pre-existing statements outside the new vocabulary —
//! all inside the same wtxn.

use brain_metadata::schema::store::schema_upload;
use brain_protocol::schema::{parse_schema, validate};
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
        ..
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected UpsertSchema"));
    };

    let source = std::str::from_utf8(blob).map_err(|e| {
        ApplyError::Invariant(format!("UpsertSchema blob is not UTF-8 source text: {e}"))
    })?;
    let parsed = parse_schema(source)
        .map_err(|e| ApplyError::Invariant(format!("UpsertSchema re-parse failed: {e:?}")))?;
    let validated = validate(&parsed).map_err(|errs| {
        ApplyError::Invariant(format!("UpsertSchema re-validate failed: {errs:?}"))
    })?;

    let namespace = validated.as_schema().namespace.clone();
    let version = schema_upload(wtxn, &validated, *created_at_unix_nanos)
        .map_err(|e| ApplyError::Metadata(format!("schema_upload: {e}")))?;

    Ok(PhaseAck::UpsertedSchema { namespace, version })
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
                }
            );
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        let active = brain_metadata::schema::store::schema_active(&rtxn, "acme").unwrap();
        assert_eq!(active, Some(1));
    }
}
