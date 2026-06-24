//! Enricher hook — the trait object the pipeline calls between
//! extraction and persistence.
//!
//! This crate intentionally does not depend on `brain-plugins`. The
//! plugin registry implements [`EnricherHook`] from the `brain-plugins`
//! side; the pipeline holds an `Option<Arc<dyn EnricherHook>>` and
//! invokes it at the candidate-aggregation point.
//!
//! Splitting the surface this way keeps the dep DAG one-directional
//! (`brain-plugins` -> `brain-extractors`) while still giving the
//! pipeline a way to call into plugin code.

use std::fmt;

use brain_core::AgentId;

use crate::framework::item::ExtractedItem;

/// Per-plugin counters reported back from one enricher invocation.
/// Mirrors the shape `brain-plugins::EnricherOutput` exposes; we
/// duplicate the fields here so the pipeline can record metrics
/// without taking a dep on the plugin crate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EnricherHookOutcome {
    /// `plugin_id()` of the plugin that produced this row. Carried as a
    /// `&'static str` because plugin ids are compile-time string
    /// constants and the audit code wants zero-allocation attribution.
    pub plugin_id: &'static str,
    pub items_added: u32,
    pub items_mutated: u32,
    pub items_dropped: u32,
    /// True when the plugin returned an error or panicked. The host
    /// pipeline runs fail-open — `ok = false` rows still flow through
    /// to the audit log, but downstream stages keep going.
    pub ok: bool,
}

/// Trait object the pipeline calls. Implementors are responsible for:
/// - panic isolation (each plugin in `std::panic::catch_unwind`),
/// - error logging (`tracing::warn!`),
/// - returning one [`EnricherHookOutcome`] per registered plugin so
///   the pipeline can record per-plugin metrics.
///
/// `brain-plugins::PluginRegistry` implements this trait; tests can
/// hand-roll their own implementations.
pub trait EnricherHook: Send + Sync {
    /// Invoke every registered enricher against the candidate items in
    /// order. Mutates `items` in place. Returns one outcome row per
    /// plugin (failures included).
    fn run(
        &self,
        agent_id: AgentId,
        items: &mut Vec<ExtractedItem>,
        source_text: &str,
        now_unix_nanos: u64,
    ) -> Vec<EnricherHookOutcome>;
}

impl fmt::Debug for dyn EnricherHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnricherHook").finish_non_exhaustive()
    }
}

/// Pipeline entry point. When `hook` is `Some`, runs every plugin
/// in registration order and returns the per-plugin outcome rows.
/// When `None`, returns an empty vec without touching `items`.
///
/// Called by the worker layer between the LLM tier and the resolver
/// stage. The worker is responsible for folding the returned outcomes
/// into metrics + the audit log.
pub fn run_pipeline_enrichers(
    hook: Option<&std::sync::Arc<dyn EnricherHook>>,
    agent_id: AgentId,
    items: &mut Vec<ExtractedItem>,
    source_text: &str,
    now_unix_nanos: u64,
) -> Vec<EnricherHookOutcome> {
    let Some(hook) = hook else {
        return Vec::new();
    };
    hook.run(agent_id, items, source_text, now_unix_nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::item::{EntityMention, ExtractedItem};

    struct UppercasingHook;
    impl EnricherHook for UppercasingHook {
        fn run(
            &self,
            _agent_id: AgentId,
            items: &mut Vec<ExtractedItem>,
            _source_text: &str,
            _now_unix_nanos: u64,
        ) -> Vec<EnricherHookOutcome> {
            let mut mutated = 0;
            for item in items.iter_mut() {
                if let ExtractedItem::EntityMention(em) = item {
                    let upper = em.text.to_uppercase();
                    if em.text != upper {
                        em.text = upper;
                        mutated += 1;
                    }
                }
            }
            vec![EnricherHookOutcome {
                plugin_id: "test:upper",
                items_added: 0,
                items_mutated: mutated,
                items_dropped: 0,
                ok: true,
            }]
        }
    }

    fn em(text: &str) -> ExtractedItem {
        ExtractedItem::EntityMention(EntityMention {
            entity_type_qname: "brain:Person".into(),
            text: text.into(),
            start: 0,
            end: text.len(),
            confidence: 0.5,
            extractor_id: 1,
            extractor_version: 1,
        })
    }

    #[test]
    fn run_pipeline_enrichers_noop_when_hook_is_none() {
        let mut items = vec![em("alice")];
        let outcomes =
            run_pipeline_enrichers(None, brain_core::AgentId::NIL, &mut items, "alice", 0);
        assert!(outcomes.is_empty());
        if let ExtractedItem::EntityMention(m) = &items[0] {
            assert_eq!(m.text, "alice");
        } else {
            panic!();
        }
    }

    #[test]
    fn run_pipeline_enrichers_invokes_hook_when_present() {
        let hook: std::sync::Arc<dyn EnricherHook> = std::sync::Arc::new(UppercasingHook);
        let mut items = vec![em("alice")];
        let outcomes = run_pipeline_enrichers(
            Some(&hook),
            brain_core::AgentId::NIL,
            &mut items,
            "alice",
            0,
        );
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].plugin_id, "test:upper");
        assert_eq!(outcomes[0].items_mutated, 1);
        if let ExtractedItem::EntityMention(m) = &items[0] {
            assert_eq!(m.text, "ALICE");
        } else {
            panic!();
        }
    }

}
