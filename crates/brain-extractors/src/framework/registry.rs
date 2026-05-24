//! In-memory extractor registry.
//!
//! One per shard. Built from the `EXTRACTORS_TABLE` rows on shard
//! open and updated whenever `SCHEMA_UPLOAD` / `EXTRACTOR_ENABLE`
//! / `EXTRACTOR_DISABLE` lands. Reads only — write access is the
//! shard executor's responsibility.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use brain_core::ExtractorId;
use brain_core::ExtractorKind;

use crate::framework::extractor::Extractor;

/// Per-tier capability gate stamped on the registry at shard spawn.
/// Mirrors the operator's `extractors.{pattern,classifier,llm}.enabled`
/// config: a tier marked `Disabled` here is dropped at registration
/// time and never re-enabled by `EXTRACTOR_ENABLE`. The `Enabled`
/// variant is the silent default.
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
    enabled: HashSet<ExtractorId>,
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
            enabled: HashSet::default(),
            tier_gate: gate,
        }
    }

    /// The tier gate this registry was built with. The materialiser
    /// passes through here at build-time; runtime callers (the
    /// extractor worker, EXTRACTOR_ENABLE handler) read it to honour
    /// the operator's opt-out without re-deriving from config.
    #[must_use]
    pub fn tier_gate(&self) -> TierGate {
        self.tier_gate
    }

    /// Register an extractor. New registrations default to
    /// `enabled = true`. Replaces any prior entry with the same id
    /// (used when a `SCHEMA_UPLOAD` bumps `extractor_version` —
    /// the registry swaps in the new impl, preserves the prior
    /// `enabled` flag).
    pub fn register(&mut self, ext: Arc<dyn Extractor>) {
        let id = ext.id();
        self.by_id.insert(id, ext);
        self.enabled.insert(id);
    }

    #[must_use]
    pub fn lookup(&self, id: ExtractorId) -> Option<&Arc<dyn Extractor>> {
        self.by_id.get(&id)
    }

    #[must_use]
    pub fn is_enabled(&self, id: ExtractorId) -> bool {
        self.enabled.contains(&id)
    }

    /// Toggle a per-extractor enabled flag. The tier gate still
    /// applies — re-enabling an extractor whose tier is `Disabled` is
    /// a no-op from [`Self::iter_enabled`]'s perspective. We honour
    /// the flag in `enabled` regardless so an `EXTRACTOR_LIST` over
    /// the wire surfaces the per-row state separately from the gate.
    pub fn set_enabled(&mut self, id: ExtractorId, enabled: bool) {
        if enabled {
            self.enabled.insert(id);
        } else {
            self.enabled.remove(&id);
        }
    }

    /// Iterate all enabled extractors. Order is unspecified; the
    /// dispatcher applies its own ordering rules (e.g. dependency
    /// topology). Tier-gated extractors are excluded unconditionally
    /// regardless of their per-row flag.
    pub fn iter_enabled(&self) -> impl Iterator<Item = &Arc<dyn Extractor>> {
        let gate = self.tier_gate;
        self.by_id
            .iter()
            .filter(move |(id, ext)| self.enabled.contains(id) && gate.state(ext.kind()).is_enabled())
            .map(|(_, ext)| ext)
    }

    /// Iterate every registered extractor regardless of enabled
    /// state. Used by `EXTRACTOR_LIST` over the wire.
    pub fn iter_all(&self) -> impl Iterator<Item = (&Arc<dyn Extractor>, bool)> {
        self.by_id
            .iter()
            .map(|(id, ext)| (ext, self.enabled.contains(id)))
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
    fn enabled_defaults_true_on_register() {
        let mut r = ExtractorRegistry::new();
        r.register(stub(1, "acme:p1"));
        assert!(r.is_enabled(ExtractorId::from(1)));
    }

    #[test]
    fn set_enabled_false_excludes_from_iter() {
        let mut r = ExtractorRegistry::new();
        r.register(stub(1, "acme:p1"));
        r.register(stub(2, "acme:p2"));
        r.set_enabled(ExtractorId::from(2), false);
        let enabled_names: Vec<_> = r.iter_enabled().map(|e| e.name().to_string()).collect();
        assert_eq!(enabled_names, vec!["acme:p1".to_string()]);
        // iter_all still sees both.
        assert_eq!(r.iter_all().count(), 2);
    }

    #[test]
    fn lookup_unknown_returns_none() {
        let r = ExtractorRegistry::new();
        assert!(r.lookup(ExtractorId::from(99)).is_none());
    }

    #[test]
    fn re_register_replaces_impl_but_preserves_enabled() {
        let mut r = ExtractorRegistry::new();
        r.register(stub(1, "v1"));
        r.set_enabled(ExtractorId::from(1), false);
        r.register(stub(1, "v2"));
        // New impl is in.
        assert_eq!(r.lookup(ExtractorId::from(1)).unwrap().name(), "v2");
        // Re-register flips enabled back to true (this is the
        // documented semantic — new versions activate by default).
        assert!(r.is_enabled(ExtractorId::from(1)));
    }
}
