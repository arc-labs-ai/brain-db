//! In-memory extractor registry.
//!
//! One per shard. Built from the `EXTRACTORS_TABLE` rows on shard
//! open and refreshed whenever `SCHEMA_UPLOAD` lands. Reads only —
//! write access is the shard executor's responsibility. Extraction is
//! always-on (C0): every registered extractor runs, with no per-tier
//! enable/disable gate.

use std::collections::HashMap;
use std::sync::Arc;

use brain_core::ExtractorId;

use crate::framework::extractor::Extractor;

#[derive(Default)]
pub struct ExtractorRegistry {
    by_id: HashMap<ExtractorId, Arc<dyn Extractor>>,
}

impl ExtractorRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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

    /// Iterate every registered extractor. Order is unspecified; the
    /// dispatcher applies its own ordering rules (e.g. dependency
    /// topology). Extraction is always-on (C0), so every registered
    /// extractor is enabled — there is no per-tier gate to filter on.
    pub fn iter_enabled(&self) -> impl Iterator<Item = &Arc<dyn Extractor>> {
        self.by_id.values()
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
