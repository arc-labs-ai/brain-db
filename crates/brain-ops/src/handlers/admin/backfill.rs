//! Wire ↔ worker adapters for the backfill control opcodes.
//!
//! The wire types (`brain_protocol::AdminBackfillRequest`,
//! `BackfillScope`, `BackfillProgress`) use rkyv-friendly primitives;
//! the worker (`brain_workers::BackfillWorker`) speaks the domain
//! types from `brain_core` (`BackfillRequest`, `BackfillRange`,
//! `BackfillProgress`, `ExtractorId`). This module owns the
//! conversion both ways so the dispatch handler stays a one-liner
//! when the worker handle eventually threads into `OpsContext`.
//!
//! Today the dispatch arm in [`crate::dispatch`] returns
//! `NotYetImplemented` for `AdminBackfill` / `AdminBackfillCancel`:
//! the worker handle isn't on `OpsContext` yet. These adapters are
//! ready for the wiring pass that follows.

use brain_core::{
    BackfillId, BackfillRange, BackfillRequest, ExtractorId, MemoryId, WorkerPriority,
};
use brain_protocol::envelope::request::{AdminBackfillRequest, BackfillScope};
use brain_protocol::envelope::response::BackfillProgress as WireBackfillProgress;

/// Per-request cap on extractor ids (matches the doc comment on
/// `brain_core::BackfillRequest::extractor_ids` — "up to 4 per
/// request"). Wire requests beyond this fail validation; the
/// rationale is bounded per-memory item count + comprehensible
/// progress reporting.
pub const MAX_EXTRACTORS_PER_BACKFILL: usize = 4;

/// Conversion failure surfaced when the wire request can't be
/// mapped to a worker `BackfillRequest`. Each variant carries
/// enough context for the wire-layer error frame.
#[derive(Debug, thiserror::Error)]
pub enum BackfillConvertError {
    /// `extractor_ids` was empty or exceeded the per-request cap.
    #[error("invalid extractor_ids: expected 1..={MAX_EXTRACTORS_PER_BACKFILL} ids, got {got}")]
    InvalidExtractorCount { got: usize },
    /// `MemoryRange { start, end_inclusive }` had `start > end`.
    #[error("invalid memory range: start ({start}) > end_inclusive ({end_inclusive})")]
    InvalidMemoryRange { start: u128, end_inclusive: u128 },
}

/// Build a worker `BackfillRequest` from the wire-side request.
/// The request's `BackfillId` is derived from
/// [`AdminBackfillRequest::request_id`] so the id is stable across
/// retries (the worker uses it as both the run handle and the
/// idempotency key).
pub fn to_worker_request(
    wire: &AdminBackfillRequest,
) -> Result<BackfillRequest, BackfillConvertError> {
    let extractor_count = wire.extractor_ids.len();
    if extractor_count == 0 || extractor_count > MAX_EXTRACTORS_PER_BACKFILL {
        return Err(BackfillConvertError::InvalidExtractorCount {
            got: extractor_count,
        });
    }
    let range = match wire.scope {
        BackfillScope::All => BackfillRange::All,
        BackfillScope::MemoryRange {
            start,
            end_inclusive,
        } => {
            if start > end_inclusive {
                return Err(BackfillConvertError::InvalidMemoryRange {
                    start,
                    end_inclusive,
                });
            }
            BackfillRange::ById {
                start: MemoryId::from_raw(start),
                end: MemoryId::from_raw(end_inclusive),
            }
        }
    };
    let extractor_ids: Vec<ExtractorId> = wire
        .extractor_ids
        .iter()
        .copied()
        .map(ExtractorId)
        .collect();
    Ok(BackfillRequest {
        request_id: BackfillId::from_bytes(wire.request_id),
        memory_range: range,
        extractor_ids,
        priority: WorkerPriority::backfill_default(),
        dry_run: wire.dry_run,
    })
}

/// Snapshot the worker's `BackfillProgress` into the wire mirror.
/// Flattens `Option<MemoryId>` into `(has_value, raw)` so the
/// wire struct is plain-data rkyv-archivable.
#[must_use]
pub fn progress_to_wire(progress: &brain_core::BackfillProgress) -> WireBackfillProgress {
    WireBackfillProgress {
        running: progress.running,
        completed: progress.completed,
        failed: progress.failed,
        skipped_already_completed: progress.skipped_already_completed,
        last_processed_memory_id_present: progress.last_processed_memory_id.is_some(),
        last_processed_memory_id: progress
            .last_processed_memory_id
            .map(|m| m.raw())
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_protocol::envelope::request::AdminBackfillRequest;

    fn sample_uuid(seed: u8) -> [u8; 16] {
        let mut u = [0u8; 16];
        for (i, b) in u.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        u
    }

    #[test]
    fn all_scope_converts() {
        let wire = AdminBackfillRequest {
            scope: BackfillScope::All,
            extractor_ids: vec![1, 2],
            dry_run: false,
            request_id: sample_uuid(1),
        };
        let req = to_worker_request(&wire).unwrap();
        assert!(matches!(req.memory_range, BackfillRange::All));
        assert_eq!(req.extractor_ids.len(), 2);
        assert!(!req.dry_run);
    }

    #[test]
    fn memory_range_converts() {
        let wire = AdminBackfillRequest {
            scope: BackfillScope::MemoryRange {
                start: 10,
                end_inclusive: 20,
            },
            extractor_ids: vec![7],
            dry_run: true,
            request_id: sample_uuid(2),
        };
        let req = to_worker_request(&wire).unwrap();
        match req.memory_range {
            BackfillRange::ById { start, end } => {
                assert_eq!(start.raw(), 10);
                assert_eq!(end.raw(), 20);
            }
            _ => panic!("expected ById range"),
        }
        assert!(req.dry_run);
    }

    #[test]
    fn empty_extractor_list_rejected() {
        let wire = AdminBackfillRequest {
            scope: BackfillScope::All,
            extractor_ids: vec![],
            dry_run: false,
            request_id: sample_uuid(3),
        };
        assert!(matches!(
            to_worker_request(&wire),
            Err(BackfillConvertError::InvalidExtractorCount { got: 0 })
        ));
    }

    #[test]
    fn extractor_overflow_rejected() {
        let wire = AdminBackfillRequest {
            scope: BackfillScope::All,
            extractor_ids: vec![1, 2, 3, 4, 5],
            dry_run: false,
            request_id: sample_uuid(4),
        };
        assert!(matches!(
            to_worker_request(&wire),
            Err(BackfillConvertError::InvalidExtractorCount { got: 5 })
        ));
    }

    #[test]
    fn inverted_memory_range_rejected() {
        let wire = AdminBackfillRequest {
            scope: BackfillScope::MemoryRange {
                start: 100,
                end_inclusive: 1,
            },
            extractor_ids: vec![1],
            dry_run: false,
            request_id: sample_uuid(5),
        };
        assert!(matches!(
            to_worker_request(&wire),
            Err(BackfillConvertError::InvalidMemoryRange {
                start: 100,
                end_inclusive: 1
            })
        ));
    }

    #[test]
    fn request_id_round_trips() {
        let raw = sample_uuid(9);
        let wire = AdminBackfillRequest {
            scope: BackfillScope::All,
            extractor_ids: vec![1],
            dry_run: false,
            request_id: raw,
        };
        let req = to_worker_request(&wire).unwrap();
        assert_eq!(req.request_id.to_bytes(), raw);
    }

    #[test]
    fn progress_to_wire_idle() {
        let p = brain_core::BackfillProgress::default();
        let w = progress_to_wire(&p);
        assert!(!w.running);
        assert_eq!(w.completed, 0);
        assert!(!w.last_processed_memory_id_present);
        assert_eq!(w.last_processed_memory_id, 0);
    }

    #[test]
    fn progress_to_wire_running() {
        let p = brain_core::BackfillProgress {
            request_id: Some(BackfillId::new()),
            completed: 42,
            failed: 1,
            skipped_already_completed: 7,
            last_processed_memory_id: Some(MemoryId::from_raw(99)),
            running: true,
            eta: None,
        };
        let w = progress_to_wire(&p);
        assert!(w.running);
        assert_eq!(w.completed, 42);
        assert_eq!(w.failed, 1);
        assert_eq!(w.skipped_already_completed, 7);
        assert!(w.last_processed_memory_id_present);
        assert_eq!(w.last_processed_memory_id, 99);
    }
}
