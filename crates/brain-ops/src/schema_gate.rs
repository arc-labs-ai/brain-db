//! Per-shard "schema declared" gate. Phase 23.11.
//!
//! Spec §28/08 §1 calls for the per-shard schema-declaration state
//! to live behind an `ArcSwap<bool>` so the connection-layer hot
//! path can read it without locking. This module owns that state.
//!
//! Lifecycle:
//!
//! 1. At shard spawn, [`SchemaGate::initial`] reads the metadata
//!    DB once and seeds the gate from `schema_namespaces`.
//! 2. On a successful (`dry_run = false`) `SCHEMA_UPLOAD` commit,
//!    [`SchemaGate::set_declared`] flips the gate to `true`.
//! 3. RECALL (and any future opcode that branches on schema
//!    presence) calls [`SchemaGate::is_declared`] per request.
//!
//! The gate is monotone in v1 — once `true`, it stays `true`. There
//! is no `SCHEMA_DROP` opcode (spec §28/08 §1). A future operator
//! tool that empties the metadata DB would need to re-seed at
//! restart; the gate doesn't carry that semantics.

use std::sync::Arc;

use arc_swap::ArcSwap;
use brain_metadata::schema_store::schema_namespaces;
use brain_metadata::MetadataDb;

use crate::error::OpError;

/// Per-shard schema-declared gate. Cheap to clone (it's an
/// `Arc<ArcSwap<bool>>` underneath).
#[derive(Clone)]
pub struct SchemaGate {
    inner: Arc<ArcSwap<bool>>,
}

impl SchemaGate {
    /// Build a gate with an explicit initial value. Tests use this
    /// to avoid touching a real metadata DB.
    #[must_use]
    pub fn new(declared: bool) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(declared)),
        }
    }

    /// Read the per-shard `schema_namespaces` list and return a
    /// gate seeded from it — `true` iff at least one namespace has
    /// an active schema version.
    pub fn initial(metadata: &MetadataDb) -> Result<Self, OpError> {
        let rtxn = metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("schema_gate read_txn: {e}")))?;
        let namespaces = schema_namespaces(&rtxn)
            .map_err(|e| OpError::Internal(format!("schema_gate query: {e}")))?;
        Ok(Self::new(!namespaces.is_empty()))
    }

    /// Lock-free read. Used on the RECALL hot path.
    #[must_use]
    pub fn is_declared(&self) -> bool {
        **self.inner.load()
    }

    /// Atomic flip. Called from `handle_schema_upload` after a
    /// successful commit.
    pub fn set_declared(&self, declared: bool) {
        self.inner.store(Arc::new(declared));
    }
}

impl Default for SchemaGate {
    fn default() -> Self {
        Self::new(false)
    }
}

impl std::fmt::Debug for SchemaGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemaGate")
            .field("declared", &self.is_declared())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_round_trips() {
        let g = SchemaGate::new(false);
        assert!(!g.is_declared());
        g.set_declared(true);
        assert!(g.is_declared());
        g.set_declared(false);
        assert!(!g.is_declared());
    }

    #[test]
    fn default_is_undeclared() {
        assert!(!SchemaGate::default().is_declared());
    }

    #[cfg(not(miri))]
    #[test]
    fn initial_returns_false_on_empty_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = MetadataDb::open(dir.path().join("md.redb")).unwrap();
        let gate = SchemaGate::initial(&metadata).expect("seed");
        assert!(!gate.is_declared());
    }

    #[cfg(not(miri))]
    #[test]
    fn clone_shares_state() {
        let g = SchemaGate::new(false);
        let cloned = g.clone();
        g.set_declared(true);
        // Both views see the flip — they share the same Arc<ArcSwap<bool>>.
        assert!(cloned.is_declared());
    }
}
