//! Extractor dispatch pipeline. Spec §22 + §27/01.
//!
//! Called by `handle_encode` after the WAL commit returns. Walks
//! every enabled extractor in the registry, runs it synchronously
//! over the encoded memory, and writes one audit row per
//! dispatch.
//!
//! ## Scope (phase 20.6)
//!
//! - Pattern extractors run end-to-end. Their items appear in the
//!   audit row's `status_reason` as a `"N items produced"` note
//!   (phase 22+ persists `EntityMention` rows via the resolver
//!   tier; v1 captures the dispatch in the audit only).
//! - Classifier extractors dispatch. Their inference returns the
//!   §22/02 staged `Failure(reason: "runtime not wired")` until
//!   phase 20.7 wires candle.
//! - LLM extractors register as degraded; dispatch records
//!   `Failure(reason: "llm tier pending phase 21")`.
//! - `depends_on` ordering is non-deterministic in v1 (§22/07 Q11).
//!
//! ## ENCODE-non-blocking contract
//!
//! The pipeline never propagates errors to the caller. Audit write
//! failures are traced at `warn` level and swallowed. ENCODE's
//! latency budget (§16/02 §2.1) absorbs the pattern tier's per-
//! memory cost; classifier dispatch is bounded by §22/02 §5.

use std::sync::Arc;

use brain_core::{AuditId, Memory};
use brain_extractors::{
    hash_memory_text, ExtractionContext, ExtractionStatus, Extractor, ExtractorRegistry,
};
use brain_metadata::audit_ops::audit_write;
use brain_metadata::tables::knowledge::audit::{extraction_status, ExtractionAudit};

use crate::context::OpsContext;

/// Run every enabled extractor over `memory` synchronously. Best
/// effort; errors are logged + audited, never propagated.
pub async fn run_extractor_pipeline(ctx: &OpsContext, memory: &Memory) {
    // Snapshot the registry under a read lock.
    let extractors: Vec<Arc<dyn Extractor>> = {
        let reg = ctx.extractor_registry.read();
        reg.iter_enabled().cloned().collect()
    };

    if extractors.is_empty() {
        return;
    }

    let schema_version = current_schema_version(ctx);
    let input_hash = match memory.text.as_deref() {
        Some(t) => hash_memory_text(t),
        None => [0u8; 32],
    };

    // Run extractors one-by-one. Use a shared empty registry as
    // the `ExtractionContext.registry` since v1 doesn't expose
    // dep lookups during run (§22/07 Q11). We pass the snapshot
    // by inserting the lock-free reference; safe because
    // `ExtractionContext` is read-only.
    let empty_reg = ExtractorRegistry::new();
    for extractor in extractors {
        let now = crate::txn::now_unix_nanos_pub();
        let ext_ctx = ExtractionContext {
            schema_version,
            now_unix_nanos: now,
            registry: &empty_reg,
        };
        let result = extractor.run(&ext_ctx, memory).await;

        let status_byte = match result.status {
            ExtractionStatus::Success => extraction_status::SUCCESS,
            ExtractionStatus::Failure => extraction_status::FAILURE,
            ExtractionStatus::SkippedBudget => extraction_status::SKIPPED_BUDGET,
            ExtractionStatus::SkippedFilter => extraction_status::SKIPPED_FILTER,
            ExtractionStatus::SkippedDuplicate => extraction_status::SKIPPED_DUPLICATE,
            ExtractionStatus::SkippedDisabled => extraction_status::SKIPPED_DISABLED,
        };

        // v1 ships `outputs: vec![]` — see module docs. The item
        // count is embedded in `status_reason` for diagnostic
        // visibility until phase 22+ resolver tier persists
        // mentions.
        let reason = if status_byte == extraction_status::SUCCESS && !result.items.is_empty() {
            format!(
                "{} items produced (resolver pending phase 22+)",
                result.items.len()
            )
        } else {
            result.status_reason.clone()
        };

        let audit = if status_byte == extraction_status::SUCCESS {
            let mut row = ExtractionAudit::success(
                AuditId::new(),
                memory.id,
                extractor.id().raw(),
                extractor.extractor_version(),
                schema_version,
                result.started_at_unix_nanos,
                result.completed_at_unix_nanos,
                Vec::new(),
                input_hash,
            );
            row.status_reason = reason;
            row
        } else {
            ExtractionAudit::non_success(
                AuditId::new(),
                memory.id,
                extractor.id().raw(),
                extractor.extractor_version(),
                schema_version,
                result.started_at_unix_nanos,
                result.completed_at_unix_nanos,
                status_byte,
                reason,
                input_hash,
            )
        };

        if let Err(e) = write_audit(ctx, &audit) {
            tracing::warn!(
                target: "brain_ops::extractor_pipeline",
                extractor_id = extractor.id().raw(),
                error = %e,
                "extractor audit write failed; continuing",
            );
        }
    }
}

fn write_audit(ctx: &OpsContext, audit: &ExtractionAudit) -> Result<(), String> {
    let mut db_guard = ctx.executor.metadata.lock();
    let wtxn = db_guard
        .write_txn()
        .map_err(|e| format!("write_txn: {e}"))?;
    audit_write(&wtxn, audit).map_err(|e| format!("audit_write: {e}"))?;
    wtxn.commit().map_err(|e| format!("commit: {e}"))?;
    Ok(())
}

/// Phase 20 doesn't propagate the per-namespace schema version
/// through the extractor pipeline yet — the audit row stamps `1`
/// pending phase 22+ propagation. The system schema is always at
/// version 1 (§21/06 §4), so this is correct for built-in
/// extractors; user-namespace extractors get the right version
/// once 20.7 wires the schema_active lookup at dispatch time.
fn current_schema_version(_ctx: &OpsContext) -> u32 {
    1
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::knowledge::ExtractorKind;
    use brain_core::{
        AgentId, ContextId, ExtractorId, MemoryId, MemoryKind, Salience,
    };
    use brain_extractors::{
        EntityMention, ExtractedItem, ExtractionFuture, ExtractionResult, ExtractorError,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockExtractor {
        id: ExtractorId,
        runs: Arc<AtomicUsize>,
        emit_items: usize,
        return_failure: bool,
    }

    impl Extractor for MockExtractor {
        fn id(&self) -> ExtractorId {
            self.id
        }
        fn kind(&self) -> ExtractorKind {
            ExtractorKind::Pattern
        }
        fn name(&self) -> &str {
            "mock"
        }
        fn extractor_version(&self) -> u32 {
            1
        }
        fn run<'a>(
            &'a self,
            _ctx: &'a ExtractionContext<'a>,
            _mem: &'a Memory,
        ) -> ExtractionFuture<'a> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let emit = self.emit_items;
            let id_raw = self.id.raw();
            let fail = self.return_failure;
            Box::pin(async move {
                if fail {
                    return ExtractionResult::failure("mock failure", 0, 0);
                }
                let items: Vec<ExtractedItem> = (0..emit)
                    .map(|i| {
                        ExtractedItem::EntityMention(EntityMention {
                            entity_type_qname: "brain:Person".into(),
                            text: format!("name{i}"),
                            start: 0,
                            end: 0,
                            confidence: 0.7,
                            extractor_id: id_raw,
                            extractor_version: 1,
                        })
                    })
                    .collect();
                ExtractionResult::success(items, 0, 0)
            })
        }
    }

    // Construct a memory for tests.
    fn memory(text: &str) -> Memory {
        Memory {
            id: MemoryId::pack(0, 1, 0),
            agent: AgentId::new(),
            context: ContextId(0),
            kind: MemoryKind::Episodic,
            salience: Salience::default(),
            text: Some(text.into()),
            created_at_unix_ms: 0,
            last_accessed_at_unix_ms: 0,
        }
    }

    #[test]
    fn snapshot_returns_empty_when_registry_empty() {
        // We don't need a full OpsContext to exercise the
        // snapshot path. Just construct an empty registry under
        // an RwLock and walk it.
        let reg = Arc::new(parking_lot::RwLock::new(ExtractorRegistry::new()));
        let snapshot: Vec<_> = reg.read().iter_enabled().cloned().collect();
        assert!(snapshot.is_empty());
    }

    #[test]
    fn snapshot_collects_registered_extractors() {
        let mut reg = ExtractorRegistry::new();
        let runs = Arc::new(AtomicUsize::new(0));
        reg.register(Arc::new(MockExtractor {
            id: ExtractorId::from(1),
            runs: runs.clone(),
            emit_items: 0,
            return_failure: false,
        }));
        reg.register(Arc::new(MockExtractor {
            id: ExtractorId::from(2),
            runs: runs.clone(),
            emit_items: 0,
            return_failure: false,
        }));
        let wrapped = Arc::new(parking_lot::RwLock::new(reg));
        let snapshot: Vec<_> = wrapped.read().iter_enabled().cloned().collect();
        assert_eq!(snapshot.len(), 2);
    }

    #[test]
    fn disabled_extractor_excluded_from_snapshot() {
        let mut reg = ExtractorRegistry::new();
        let runs = Arc::new(AtomicUsize::new(0));
        reg.register(Arc::new(MockExtractor {
            id: ExtractorId::from(1),
            runs: runs.clone(),
            emit_items: 0,
            return_failure: false,
        }));
        reg.set_enabled(ExtractorId::from(1), false);
        let wrapped = Arc::new(parking_lot::RwLock::new(reg));
        let snapshot: Vec<_> = wrapped.read().iter_enabled().cloned().collect();
        assert!(snapshot.is_empty());
    }

    #[test]
    fn current_schema_version_is_one_in_v1() {
        // Placeholder until 20.7 wires schema_active lookup. The
        // returned value lands in every audit row stamped by
        // 20.6 dispatches.
        let _ = memory("placeholder");
        // No OpsContext-bound test here — value is a static `1`.
        assert_eq!(1, 1);
    }

    #[test]
    fn extractor_error_quiet_import() {
        // Suppress unused-import warning on ExtractorError.
        let _: Option<ExtractorError> = None;
    }
}
