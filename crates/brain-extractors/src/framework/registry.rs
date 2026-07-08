//! In-memory extractor registry.
//!
//! One per shard. Built from the `EXTRACTORS_TABLE` rows on shard
//! open and refreshed whenever `SCHEMA_UPLOAD` lands. Reads only —
//! write access is the shard executor's responsibility. Extraction is
//! always-on; the only per-extractor gate is the deploy-time tier gate
//! (`extractors.{pattern,classifier,llm}.enabled`).

use std::collections::HashMap;
use std::sync::Arc;

use brain_core::ExtractorId;
use brain_core::ExtractorKind;

use crate::framework::extractor::Extractor;

/// Per-tier capability gate stamped on the registry at shard spawn.
/// Mirrors the operator's `extractors.{pattern,classifier,llm}.enabled`
/// config: a tier marked `Disabled` here is dropped at registration
/// time. The `Enabled` variant is the silent default.
///
/// "Disabled by config" is silent (operator opt-out); "enabled but
/// failed to load the model" is a spawn failure handled in the shard
/// boot path, not here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TierState {
    #[default]
    Enabled,
    Disabled,
}

impl TierState {
    /// Promote a boolean gate into the typed state.
    #[must_use]
    pub fn from_enabled(enabled: bool) -> Self {
        if enabled {
            Self::Enabled
        } else {
            Self::Disabled
        }
    }

    #[must_use]
    pub fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// Per-tier gate snapshot. Defaults to every tier enabled — matches
/// the default config and keeps existing tests/registry callers
/// working without explicit setup.
#[derive(Clone, Copy, Debug, Default)]
pub struct TierGate {
    pub pattern: TierState,
    pub classifier: TierState,
    pub llm: TierState,
}

impl TierGate {
    /// Build a gate where every tier is enabled. Used by tests and
    /// any deployment that doesn't customise the gate.
    #[must_use]
    pub fn all_enabled() -> Self {
        Self::default()
    }

    /// Return the state for the named tier.
    #[must_use]
    pub fn state(&self, kind: ExtractorKind) -> TierState {
        match kind {
            ExtractorKind::Pattern => self.pattern,
            ExtractorKind::Classifier => self.classifier,
            ExtractorKind::Llm => self.llm,
        }
    }
}

#[derive(Default)]
pub struct ExtractorRegistry {
    by_id: HashMap<ExtractorId, Arc<dyn Extractor>>,
    /// Per-tier capability gates. Built at shard spawn from operator
    /// config; immutable for the lifetime of the registry instance.
    tier_gate: TierGate,
}

impl ExtractorRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a registry with a non-default tier gate. Used by the
    /// shard boot path to honour `extractors.<tier>.enabled = false`.
    #[must_use]
    pub fn with_tier_gate(gate: TierGate) -> Self {
        Self {
            by_id: HashMap::default(),
            tier_gate: gate,
        }
    }

    /// The tier gate this registry was built with. The materialiser
    /// passes through here at build-time; runtime callers (the
    /// extractor worker) read it to honour the operator's opt-out
    /// without re-deriving from config.
    #[must_use]
    pub fn tier_gate(&self) -> TierGate {
        self.tier_gate
    }

    /// Register an extractor. Replaces any prior entry with the same id
    /// (used when a `SCHEMA_UPLOAD` bumps `extractor_version` — the
    /// registry swaps in the new impl).
    pub fn register(&mut self, ext: Arc<dyn Extractor>) {
        let id = ext.id();
        self.by_id.insert(id, ext);
    }

    #[must_use]
    pub fn lookup(&self, id: ExtractorId) -> Option<&Arc<dyn Extractor>> {
        self.by_id.get(&id)
    }

    /// Iterate every extractor whose tier is enabled. Order is
    /// unspecified; the dispatcher applies its own ordering rules
    /// (e.g. dependency topology). Tier-gated extractors are excluded.
    pub fn iter_enabled(&self) -> impl Iterator<Item = &Arc<dyn Extractor>> {
        let gate = self.tier_gate;
        self.by_id
            .values()
            .filter(move |ext| gate.state(ext.kind()).is_enabled())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::framework::extractor::{
        ExtractionContext, ExtractionFuture, ExtractionResult, Extractor,
    };
    use brain_core::ExtractorKind;
    use brain_core::Memory;

    struct Stub {
        id: ExtractorId,
        name: String,
    }

    impl Extractor for Stub {
        fn id(&self) -> ExtractorId {
            self.id
        }
        fn kind(&self) -> ExtractorKind {
            ExtractorKind::Pattern
        }
        fn name(&self) -> &str {
            &self.name
        }
        fn extractor_version(&self) -> u32 {
            1
        }
        fn run<'a>(
            &'a self,
            _ctx: &'a ExtractionContext<'a>,
            _mem: &'a Memory,
        ) -> ExtractionFuture<'a> {
            Box::pin(async { ExtractionResult::success(Vec::new(), 0, 0) })
        }
    }

    fn stub(id: u32, name: &str) -> Arc<dyn Extractor> {
        Arc::new(Stub {
            id: ExtractorId::from(id),
            name: name.into(),
        })
    }

    #[test]
    fn register_then_lookup() {
        let mut r = ExtractorRegistry::new();
        r.register(stub(1, "acme:p1"));
        let got = r.lookup(ExtractorId::from(1)).unwrap();
        assert_eq!(got.name(), "acme:p1");
    }

    #[test]
    fn register_is_visible_in_iter_enabled() {
        let mut r = ExtractorRegistry::new();
        r.register(stub(1, "acme:p1"));
        r.register(stub(2, "acme:p2"));
        let names: Vec<_> = r.iter_enabled().map(|e| e.name().to_string()).collect();
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn lookup_unknown_returns_none() {
        let r = ExtractorRegistry::new();
        assert!(r.lookup(ExtractorId::from(99)).is_none());
    }

    #[test]
    fn re_register_replaces_impl() {
        let mut r = ExtractorRegistry::new();
        r.register(stub(1, "v1"));
        r.register(stub(1, "v2"));
        assert_eq!(r.lookup(ExtractorId::from(1)).unwrap().name(), "v2");
    }
}
