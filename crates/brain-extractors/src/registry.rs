//! In-memory extractor registry. Spec §22/00, §27/01.
//!
//! One per shard. Built from the `EXTRACTORS_TABLE` rows on shard
//! open and updated whenever `SCHEMA_UPLOAD` / `EXTRACTOR_ENABLE`
//! / `EXTRACTOR_DISABLE` lands. Reads only — write access is the
//! shard executor's responsibility.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use brain_core::ExtractorId;

use crate::extractor::Extractor;

#[derive(Default)]
pub struct ExtractorRegistry {
    by_id: HashMap<ExtractorId, Arc<dyn Extractor>>,
    enabled: HashSet<ExtractorId>,
}

impl ExtractorRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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

    pub fn set_enabled(&mut self, id: ExtractorId, enabled: bool) {
        if enabled {
            self.enabled.insert(id);
        } else {
            self.enabled.remove(&id);
        }
    }

    /// Iterate all enabled extractors. Order is unspecified; the
    /// dispatcher applies its own ordering rules (e.g. dependency
    /// topology).
    pub fn iter_enabled(&self) -> impl Iterator<Item = &Arc<dyn Extractor>> {
        self.by_id
            .iter()
            .filter(|(id, _)| self.enabled.contains(id))
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

    use crate::extractor::{ExtractionContext, ExtractionFuture, ExtractionResult, Extractor};
    use brain_core::knowledge::ExtractorKind;
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
