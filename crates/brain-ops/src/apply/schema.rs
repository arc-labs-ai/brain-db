//! Apply schema-shaped phases: UpsertSchema, SetExtractorEnabled.
//!
//! `SetExtractorEnabled` is implemented (one-table flag flip).
//! `UpsertSchema` is scaffolded — full implementation lands in P2b
//! (interns predicates / relation-types / entity-types, writes schema
//! row, flips schema gate in same txn).

use brain_metadata::extractor_ops::extractor_set_enabled;
use redb::WriteTransaction;

use super::ApplyError;
use crate::write::{Phase, PhaseAck, Write};

pub fn apply_upsert_schema(
    _wtxn: &WriteTransaction,
    _phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    Err(ApplyError::NotYetImplemented("UpsertSchema"))
}

pub fn apply_set_extractor_enabled(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::SetExtractorEnabled { id, enabled } = phase else {
        return Err(ApplyError::PhaseMisShape("expected SetExtractorEnabled"));
    };
    extractor_set_enabled(wtxn, *id, *enabled)
        .map_err(|e| ApplyError::Metadata(format!("extractor_set_enabled: {e}")))?;
    Ok(PhaseAck::ExtractorEnabledSet {
        id: *id,
        enabled: *enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_metadata::extractor_ops::extractor_intern;
    use brain_metadata::MetadataDb;
    use tempfile::TempDir;

    use crate::write::{Phase, Write, WriteId};

    #[test]
    fn set_extractor_enabled_round_trips() {
        let dir = TempDir::new().unwrap();
        let mut db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();

        // Seed an extractor row.
        let id;
        {
            let wtxn = db.write_txn().unwrap();
            id = extractor_intern(
                &wtxn,
                "test",
                "pat",
                brain_core::knowledge::ExtractorKind::Pattern,
                1,
                Vec::new(),
                1_700_000_000_000,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }

        // Disable via the apply function.
        let phase = Phase::SetExtractorEnabled { id, enabled: false };
        let write = Write::single(
            WriteId::new(),
            brain_core::AgentId::default(),
            phase.clone(),
        );
        {
            let wtxn = db.write_txn().unwrap();
            let ack = apply_set_extractor_enabled(&wtxn, &phase, &write).unwrap();
            assert!(matches!(
                ack,
                PhaseAck::ExtractorEnabledSet { enabled: false, .. }
            ));
            wtxn.commit().unwrap();
        }

        // Confirm: row.enabled is a u8 byte (0 disabled, 1 enabled).
        let rtxn = db.read_txn().unwrap();
        let row = brain_metadata::extractor_ops::extractor_get(&rtxn, id)
            .unwrap()
            .unwrap();
        assert_eq!(row.enabled, 0);
    }
}
