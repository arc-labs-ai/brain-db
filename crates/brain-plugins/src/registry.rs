//! Plugin registry.
//!
//! Holds the compile-time-registered enrichers and connectors. One
//! registry per shard; the host builds the registry at shard open and
//! hands an `Arc<PluginRegistry>` to the writer.
//!
//! All plugin invocations go through this type so the host's panic
//! catcher, tracing instrumentation, and fail-open policy are applied
//! uniformly. Plugins MUST NOT be called directly.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use brain_core::SpaceId;
use brain_extractors::enricher_hook::{EnricherHook, EnricherHookOutcome};
use brain_extractors::framework::item::ExtractedItem;

use crate::connector::{ConnectorPlugin, ConnectorRequest, ConnectorResponse};
use crate::enricher::{EnricherInput, EnricherOutput, EnricherPlugin};
use crate::errors::{PluginError, PluginResult};

/// Per-batch ceiling on the number of [`ExtractedItem`]s the enricher
/// chain may leave behind.
///
/// Enrichers receive `&mut Vec<ExtractedItem>` and may push freely; a
/// misbehaving (or hostile) enricher could grow the vector without
/// bound and exhaust the writer's Glommio shard. The host caps the
/// batch after each enricher runs: anything past this ceiling is
/// truncated and logged, matching the pipeline's fail-open policy (a
/// contract-violating plugin degrades the batch, it never aborts the
/// write). A single encode realistically yields dozens to low hundreds
/// of items, so this leaves generous headroom while still bounding a
/// runaway enricher.
pub const MAX_ENRICHER_BATCH_ITEMS: usize = 10_000;

/// One outcome row returned by [`PluginRegistry::run_enrichers`].
///
/// Carries the plugin id so callers can attribute metrics and audit
/// rows to the right plugin without re-resolving the order.
#[derive(Debug)]
pub struct EnricherOutcome {
    pub plugin_id: &'static str,
    pub result: PluginResult<EnricherOutput>,
}

/// Process-wide plugin registry.
///
/// Construction is two-phase: `new()` returns an empty registry,
/// then each `register_*` call validates uniqueness and runs the
/// plugin's `initialize` hook. Once all plugins are in, wrap in an
/// `Arc` and hand it to the writer.
#[derive(Default)]
pub struct PluginRegistry {
    enrichers: Vec<Arc<dyn EnricherPlugin>>,
    connectors: HashMap<String, Arc<dyn ConnectorPlugin>>,
}

impl PluginRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            enrichers: Vec::new(),
            connectors: HashMap::new(),
        }
    }

    /// Register an enricher. Calls `initialize(config)`; if that
    /// errors, the plugin is rejected and the registry is left
    /// unchanged.
    ///
    /// Enrichers execute in registration order, so order matters when
    /// one plugin depends on the side effects of another.
    pub fn register_enricher(
        &mut self,
        plugin: Arc<dyn EnricherPlugin>,
        config: &serde_json::Value,
    ) -> PluginResult<()> {
        let id = plugin.plugin_id();
        if self.enrichers.iter().any(|p| p.plugin_id() == id) {
            return Err(PluginError::DuplicateId { plugin_id: id });
        }
        plugin.initialize(config)?;
        tracing::info!(plugin_id = %id, name = %plugin.plugin_name(), "enricher registered");
        self.enrichers.push(plugin);
        Ok(())
    }

    /// Register a connector. Connectors are keyed by `plugin_id` so
    /// callers can route to a specific one.
    pub fn register_connector(
        &mut self,
        plugin: Arc<dyn ConnectorPlugin>,
        config: &serde_json::Value,
    ) -> PluginResult<()> {
        let id = plugin.plugin_id();
        if self.connectors.contains_key(id) {
            return Err(PluginError::DuplicateId { plugin_id: id });
        }
        plugin.initialize(config)?;
        tracing::info!(plugin_id = %id, name = %plugin.plugin_name(), "connector registered");
        self.connectors.insert(id.to_string(), plugin);
        Ok(())
    }

    /// Invoke every registered enricher in registration order.
    ///
    /// The pipeline runs fail-open: a plugin that errors or panics is
    /// logged and skipped; the next plugin still runs against the
    /// items the failing plugin left behind. The returned vector has
    /// one entry per enricher, in registration order — successful or
    /// not — so the caller can fold metrics + audit rows.
    pub fn run_enrichers(&self, input: EnricherInput<'_>) -> Vec<EnricherOutcome> {
        let EnricherInput {
            space_id,
            items,
            source_text,
            now_unix_nanos,
        } = input;
        let mut outcomes = Vec::with_capacity(self.enrichers.len());

        for plugin in &self.enrichers {
            let plugin_id = plugin.plugin_id();
            // Re-borrow the items vec for each plugin. The mutable
            // borrow only lives for the duration of this iteration so
            // successive plugins see prior plugins' mutations.
            let per_plugin_input = EnricherInput {
                space_id,
                items,
                source_text,
                now_unix_nanos,
            };

            // Panic catcher — plugins are third-party code; a panic
            // here must not poison the writer's executor. The pipeline
            // continues with un-enriched items (fail-open).
            let result =
                std::panic::catch_unwind(AssertUnwindSafe(|| plugin.enrich(per_plugin_input)));

            let outcome = match result {
                Ok(Ok(out)) => Ok(out),
                Ok(Err(e)) => {
                    tracing::warn!(
                        plugin_id = %plugin_id,
                        error = %e,
                        "enricher returned error; pipeline continues fail-open"
                    );
                    Err(e)
                }
                Err(panic) => {
                    let message = panic_message(&panic);
                    tracing::warn!(
                        plugin_id = %plugin_id,
                        message = %message,
                        "enricher panicked; pipeline continues fail-open"
                    );
                    Err(PluginError::Panicked { plugin_id, message })
                }
            };

            // Enforce the per-batch ceiling after every plugin so the
            // cumulative enricher chain cannot grow the vector without
            // bound. Truncate-and-warn (fail-open) rather than reject:
            // the surrounding pipeline degrades a contract-violating
            // plugin's output, it never aborts the write.
            if items.len() > MAX_ENRICHER_BATCH_ITEMS {
                let dropped = items.len() - MAX_ENRICHER_BATCH_ITEMS;
                tracing::warn!(
                    plugin_id = %plugin_id,
                    ceiling = MAX_ENRICHER_BATCH_ITEMS,
                    dropped,
                    "enricher exceeded per-batch item ceiling; truncating (fail-open)"
                );
                items.truncate(MAX_ENRICHER_BATCH_ITEMS);
            }

            outcomes.push(EnricherOutcome {
                plugin_id,
                result: outcome,
            });
        }

        outcomes
    }

    /// Fetch from one specific connector. Connector errors propagate
    /// (the connector scheduler decides retry policy). Panics are
    /// caught and surfaced as [`PluginError::Panicked`].
    ///
    /// `req.max_items` is a hard cap: a connector that returns more than
    /// it requested is in contract violation. The host enforces the cap
    /// itself (truncate-and-warn, fail-open) so an over-eager or hostile
    /// connector cannot flood the encode pipeline with an unbounded
    /// `Vec<ConnectorItem>`.
    pub fn fetch_from_connector(
        &self,
        id: &str,
        req: ConnectorRequest,
    ) -> PluginResult<ConnectorResponse> {
        let plugin = self
            .connectors
            .get(id)
            .ok_or_else(|| PluginError::ConnectorNotFound { id: id.to_string() })?;
        let plugin_id = plugin.plugin_id();
        let max_items = req.max_items;
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| plugin.fetch(req)));
        match result {
            Ok(Ok(mut resp)) => {
                let cap = max_items as usize;
                if resp.items.len() > cap {
                    let dropped = resp.items.len() - cap;
                    tracing::warn!(
                        plugin_id = %plugin_id,
                        max_items,
                        dropped,
                        "connector returned more than max_items; truncating (fail-open)"
                    );
                    resp.items.truncate(cap);
                }
                Ok(resp)
            }
            Ok(Err(e)) => Err(e),
            Err(panic) => {
                let message = panic_message(&panic);
                tracing::warn!(
                    plugin_id = %plugin_id,
                    message = %message,
                    "connector panicked"
                );
                Err(PluginError::Panicked { plugin_id, message })
            }
        }
    }

    /// Enricher ids in registration order — used by `EXTRACTOR_LIST`
    /// over the wire and by admin tools.
    #[must_use]
    pub fn enricher_ids(&self) -> Vec<&'static str> {
        self.enrichers.iter().map(|p| p.plugin_id()).collect()
    }

    /// Connector ids in arbitrary order — the registry is a HashMap.
    #[must_use]
    pub fn connector_ids(&self) -> Vec<String> {
        self.connectors.keys().cloned().collect()
    }

    /// True iff at least one enricher is registered.
    #[must_use]
    pub fn has_enrichers(&self) -> bool {
        !self.enrichers.is_empty()
    }

    /// Number of registered enrichers.
    #[must_use]
    pub fn enricher_count(&self) -> usize {
        self.enrichers.len()
    }

    /// Number of registered connectors.
    #[must_use]
    pub fn connector_count(&self) -> usize {
        self.connectors.len()
    }

    /// Call `shutdown` on every registered plugin. Errors are logged
    /// but do not propagate.
    pub fn shutdown_all(&self) {
        for plugin in &self.enrichers {
            if let Err(e) = plugin.shutdown() {
                tracing::warn!(
                    plugin_id = %plugin.plugin_id(),
                    error = %e,
                    "enricher shutdown returned error"
                );
            }
        }
        for plugin in self.connectors.values() {
            if let Err(e) = plugin.shutdown() {
                tracing::warn!(
                    plugin_id = %plugin.plugin_id(),
                    error = %e,
                    "connector shutdown returned error"
                );
            }
        }
    }
}

/// Bridge to the pipeline's `EnricherHook` trait. Letting the
/// pipeline call `Arc<dyn EnricherHook>` instead of
/// `Arc<PluginRegistry>` keeps `brain-extractors` from depending on
/// `brain-plugins` (the dep DAG runs the other way).
impl EnricherHook for PluginRegistry {
    fn run(
        &self,
        space_id: SpaceId,
        items: &mut Vec<ExtractedItem>,
        source_text: &str,
        now_unix_nanos: u64,
    ) -> Vec<EnricherHookOutcome> {
        let outcomes = self.run_enrichers(EnricherInput {
            space_id,
            items,
            source_text,
            now_unix_nanos,
        });
        outcomes
            .into_iter()
            .map(|o| match o.result {
                Ok(out) => EnricherHookOutcome {
                    plugin_id: o.plugin_id,
                    items_added: out.items_added,
                    items_mutated: out.items_mutated,
                    items_dropped: out.items_dropped,
                    ok: true,
                },
                Err(_) => EnricherHookOutcome {
                    plugin_id: o.plugin_id,
                    items_added: 0,
                    items_mutated: 0,
                    items_dropped: 0,
                    ok: false,
                },
            })
            .collect()
    }
}

/// Best-effort extraction of a panic payload's text. Mirrors the
/// stdlib's default-hook logic — accept `&str` and `String` payloads,
/// fall back to a generic marker for anything else.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::RecallPlugin;
    use brain_core::SpaceId;
    use brain_extractors::framework::item::{EntityMention, ExtractedItem};
    use serde_json::Value;

    // -- helpers -----------------------------------------------------

    fn space() -> SpaceId {
        SpaceId::NIL
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

    // -- plugin fixtures ---------------------------------------------

    struct Mutator;

    impl RecallPlugin for Mutator {
        fn plugin_id(&self) -> &'static str {
            "test:mutator"
        }
        fn plugin_name(&self) -> &'static str {
            "Mutator"
        }
        fn initialize(&self, _config: &Value) -> PluginResult<()> {
            Ok(())
        }
    }

    impl EnricherPlugin for Mutator {
        fn enrich(&self, input: EnricherInput<'_>) -> PluginResult<EnricherOutput> {
            let mut mutated = 0;
            for item in input.items.iter_mut() {
                if let ExtractedItem::EntityMention(m) = item {
                    if m.text.contains("rocket-shaped") {
                        m.text = m.text.replace("rocket-shaped", "rocket");
                        mutated += 1;
                    }
                }
            }
            Ok(EnricherOutput {
                items_added: 0,
                items_mutated: mutated,
                items_dropped: 0,
            })
        }
    }

    struct Adder;

    impl RecallPlugin for Adder {
        fn plugin_id(&self) -> &'static str {
            "test:adder"
        }
        fn plugin_name(&self) -> &'static str {
            "Adder"
        }
        fn initialize(&self, _config: &Value) -> PluginResult<()> {
            Ok(())
        }
    }

    impl EnricherPlugin for Adder {
        fn enrich(&self, input: EnricherInput<'_>) -> PluginResult<EnricherOutput> {
            input.items.push(em("synthetic"));
            Ok(EnricherOutput {
                items_added: 1,
                items_mutated: 0,
                items_dropped: 0,
            })
        }
    }

    struct Failing;

    impl RecallPlugin for Failing {
        fn plugin_id(&self) -> &'static str {
            "test:failing"
        }
        fn plugin_name(&self) -> &'static str {
            "Failing"
        }
        fn initialize(&self, _config: &Value) -> PluginResult<()> {
            Ok(())
        }
    }

    impl EnricherPlugin for Failing {
        fn enrich(&self, _input: EnricherInput<'_>) -> PluginResult<EnricherOutput> {
            Err(PluginError::Failed {
                plugin_id: "test:failing",
                message: "synthetic failure".into(),
            })
        }
    }

    struct Panicker;

    impl RecallPlugin for Panicker {
        fn plugin_id(&self) -> &'static str {
            "test:panicker"
        }
        fn plugin_name(&self) -> &'static str {
            "Panicker"
        }
        fn initialize(&self, _config: &Value) -> PluginResult<()> {
            Ok(())
        }
    }

    impl EnricherPlugin for Panicker {
        fn enrich(&self, _input: EnricherInput<'_>) -> PluginResult<EnricherOutput> {
            panic!("boom: {}", "synthetic panic");
        }
    }

    struct BadInit;

    impl RecallPlugin for BadInit {
        fn plugin_id(&self) -> &'static str {
            "test:bad-init"
        }
        fn plugin_name(&self) -> &'static str {
            "BadInit"
        }
        fn initialize(&self, _config: &Value) -> PluginResult<()> {
            Err(PluginError::InvalidConfig {
                plugin_id: "test:bad-init",
                message: "test".into(),
            })
        }
    }

    impl EnricherPlugin for BadInit {
        fn enrich(&self, _input: EnricherInput<'_>) -> PluginResult<EnricherOutput> {
            unreachable!("init refused");
        }
    }

    // -- tests -------------------------------------------------------

    #[test]
    fn register_enricher_calls_initialize_and_lists_id() {
        let mut reg = PluginRegistry::new();
        reg.register_enricher(Arc::new(Mutator), &Value::Null)
            .unwrap();
        assert_eq!(reg.enricher_ids(), vec!["test:mutator"]);
        assert_eq!(reg.enricher_count(), 1);
        assert!(reg.has_enrichers());
    }

    #[test]
    fn duplicate_enricher_id_is_rejected() {
        let mut reg = PluginRegistry::new();
        reg.register_enricher(Arc::new(Mutator), &Value::Null)
            .unwrap();
        let err = reg
            .register_enricher(Arc::new(Mutator), &Value::Null)
            .unwrap_err();
        assert!(matches!(
            err,
            PluginError::DuplicateId {
                plugin_id: "test:mutator"
            }
        ));
        // Registry unchanged.
        assert_eq!(reg.enricher_count(), 1);
    }

    #[test]
    fn init_error_blocks_registration() {
        let mut reg = PluginRegistry::new();
        let err = reg
            .register_enricher(Arc::new(BadInit), &Value::Null)
            .unwrap_err();
        assert!(matches!(err, PluginError::InvalidConfig { .. }));
        assert_eq!(reg.enricher_count(), 0);
    }

    #[test]
    fn run_enrichers_threads_items_through_in_order() {
        let mut reg = PluginRegistry::new();
        reg.register_enricher(Arc::new(Mutator), &Value::Null)
            .unwrap();
        reg.register_enricher(Arc::new(Adder), &Value::Null)
            .unwrap();

        let mut items = vec![em("a rocket-shaped object")];
        let outcomes = reg.run_enrichers(EnricherInput {
            space_id: space(),
            items: &mut items,
            source_text: "irrelevant",
            now_unix_nanos: 1_000,
        });

        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].plugin_id, "test:mutator");
        assert_eq!(outcomes[1].plugin_id, "test:adder");
        assert!(outcomes[0].result.is_ok());
        assert!(outcomes[1].result.is_ok());
        // Mutator collapsed "rocket-shaped" → "rocket".
        if let ExtractedItem::EntityMention(em0) = &items[0] {
            assert_eq!(em0.text, "a rocket object");
        } else {
            panic!("expected EntityMention");
        }
        // Adder appended its synthetic item.
        assert_eq!(items.len(), 2);
        if let ExtractedItem::EntityMention(em1) = &items[1] {
            assert_eq!(em1.text, "synthetic");
        } else {
            panic!("expected EntityMention");
        }
    }

    #[test]
    fn failing_plugin_does_not_abort_next_plugin() {
        let mut reg = PluginRegistry::new();
        reg.register_enricher(Arc::new(Failing), &Value::Null)
            .unwrap();
        reg.register_enricher(Arc::new(Adder), &Value::Null)
            .unwrap();

        let mut items = Vec::<ExtractedItem>::new();
        let outcomes = reg.run_enrichers(EnricherInput {
            space_id: space(),
            items: &mut items,
            source_text: "irrelevant",
            now_unix_nanos: 1_000,
        });

        assert_eq!(outcomes.len(), 2);
        assert!(outcomes[0].result.is_err());
        assert!(outcomes[1].result.is_ok());
        // Adder still appended its item.
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn panicking_plugin_does_not_abort_next_plugin() {
        let mut reg = PluginRegistry::new();
        reg.register_enricher(Arc::new(Panicker), &Value::Null)
            .unwrap();
        reg.register_enricher(Arc::new(Adder), &Value::Null)
            .unwrap();

        let mut items = Vec::<ExtractedItem>::new();
        let outcomes = reg.run_enrichers(EnricherInput {
            space_id: space(),
            items: &mut items,
            source_text: "irrelevant",
            now_unix_nanos: 1_000,
        });

        assert_eq!(outcomes.len(), 2);
        match &outcomes[0].result {
            Err(PluginError::Panicked { plugin_id, message }) => {
                assert_eq!(*plugin_id, "test:panicker");
                assert!(message.contains("boom"));
            }
            other => panic!("expected Panicked, got {:?}", other),
        }
        assert!(outcomes[1].result.is_ok());
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn fetch_from_connector_missing_id_returns_not_found() {
        let reg = PluginRegistry::new();
        let err = reg
            .fetch_from_connector(
                "nope",
                ConnectorRequest {
                    query: String::new(),
                    since_unix_nanos: None,
                    max_items: 10,
                },
            )
            .unwrap_err();
        assert!(matches!(err, PluginError::ConnectorNotFound { .. }));
    }

    #[test]
    fn registry_implements_enricher_hook_for_pipeline_use() {
        let mut reg = PluginRegistry::new();
        reg.register_enricher(Arc::new(Mutator), &Value::Null)
            .unwrap();
        reg.register_enricher(Arc::new(Adder), &Value::Null)
            .unwrap();

        let reg_arc: Arc<dyn brain_extractors::enricher_hook::EnricherHook> = Arc::new(reg);
        let mut items = vec![em("a rocket-shaped object")];
        let outcomes = EnricherHook::run(&*reg_arc, space(), &mut items, "irrelevant", 1_000);

        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].plugin_id, "test:mutator");
        assert!(outcomes[0].ok);
        assert_eq!(outcomes[0].items_mutated, 1);
        assert_eq!(outcomes[1].plugin_id, "test:adder");
        assert!(outcomes[1].ok);
        assert_eq!(outcomes[1].items_added, 1);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn registry_hook_marks_failing_plugin_not_ok() {
        let mut reg = PluginRegistry::new();
        reg.register_enricher(Arc::new(Failing), &Value::Null)
            .unwrap();
        reg.register_enricher(Arc::new(Adder), &Value::Null)
            .unwrap();
        let reg_arc: Arc<dyn brain_extractors::enricher_hook::EnricherHook> = Arc::new(reg);

        let mut items = Vec::<ExtractedItem>::new();
        let outcomes = EnricherHook::run(&*reg_arc, space(), &mut items, "irrelevant", 0);
        assert_eq!(outcomes.len(), 2);
        assert!(!outcomes[0].ok);
        assert!(outcomes[1].ok);
        assert_eq!(items.len(), 1);
    }

    struct Flooder {
        count: usize,
    }

    impl RecallPlugin for Flooder {
        fn plugin_id(&self) -> &'static str {
            "test:flooder"
        }
        fn plugin_name(&self) -> &'static str {
            "Flooder"
        }
        fn initialize(&self, _config: &Value) -> PluginResult<()> {
            Ok(())
        }
    }

    impl EnricherPlugin for Flooder {
        fn enrich(&self, input: EnricherInput<'_>) -> PluginResult<EnricherOutput> {
            for _ in 0..self.count {
                input.items.push(em("flood"));
            }
            Ok(EnricherOutput {
                items_added: self.count as u32,
                items_mutated: 0,
                items_dropped: 0,
            })
        }
    }

    struct OverflowConnector {
        count: usize,
    }

    impl RecallPlugin for OverflowConnector {
        fn plugin_id(&self) -> &'static str {
            "test:overflow-connector"
        }
        fn plugin_name(&self) -> &'static str {
            "OverflowConnector"
        }
        fn initialize(&self, _config: &Value) -> PluginResult<()> {
            Ok(())
        }
    }

    impl ConnectorPlugin for OverflowConnector {
        fn fetch(&self, _req: ConnectorRequest) -> PluginResult<ConnectorResponse> {
            let items = (0..self.count)
                .map(|i| crate::connector::ConnectorItem {
                    external_id: format!("id-{i}"),
                    text: "raw".into(),
                    created_at_unix_nanos: 0,
                    source_url: None,
                    tags: Vec::new(),
                })
                .collect();
            Ok(ConnectorResponse {
                items,
                next_since_unix_nanos: 1,
            })
        }
    }

    #[test]
    fn enricher_output_is_capped_at_batch_ceiling() {
        let mut reg = PluginRegistry::new();
        reg.register_enricher(
            Arc::new(Flooder {
                count: MAX_ENRICHER_BATCH_ITEMS + 500,
            }),
            &Value::Null,
        )
        .unwrap();

        let mut items = Vec::<ExtractedItem>::new();
        let outcomes = reg.run_enrichers(EnricherInput {
            space_id: space(),
            items: &mut items,
            source_text: "irrelevant",
            now_unix_nanos: 0,
        });

        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].result.is_ok());
        // Host truncated the runaway growth to the ceiling.
        assert_eq!(items.len(), MAX_ENRICHER_BATCH_ITEMS);
    }

    #[test]
    fn cumulative_enricher_chain_is_capped_at_batch_ceiling() {
        let mut reg = PluginRegistry::new();
        // Two enrichers each pushing past the ceiling; the cumulative
        // vector must still be bounded.
        reg.register_enricher(
            Arc::new(Flooder {
                count: MAX_ENRICHER_BATCH_ITEMS,
            }),
            &Value::Null,
        )
        .unwrap();
        reg.register_enricher(Arc::new(Adder), &Value::Null)
            .unwrap();

        let mut items = Vec::<ExtractedItem>::new();
        let outcomes = reg.run_enrichers(EnricherInput {
            space_id: space(),
            items: &mut items,
            source_text: "irrelevant",
            now_unix_nanos: 0,
        });

        assert_eq!(outcomes.len(), 2);
        assert!(items.len() <= MAX_ENRICHER_BATCH_ITEMS);
    }

    #[test]
    fn connector_response_is_capped_at_max_items() {
        let mut reg = PluginRegistry::new();
        reg.register_connector(Arc::new(OverflowConnector { count: 50 }), &Value::Null)
            .unwrap();

        let resp = reg
            .fetch_from_connector(
                "test:overflow-connector",
                ConnectorRequest {
                    query: String::new(),
                    since_unix_nanos: None,
                    max_items: 10,
                },
            )
            .unwrap();

        // Host enforced the hard cap host-side.
        assert_eq!(resp.items.len(), 10);
    }

    #[test]
    fn connector_response_within_cap_is_untouched() {
        let mut reg = PluginRegistry::new();
        reg.register_connector(Arc::new(OverflowConnector { count: 3 }), &Value::Null)
            .unwrap();

        let resp = reg
            .fetch_from_connector(
                "test:overflow-connector",
                ConnectorRequest {
                    query: String::new(),
                    since_unix_nanos: None,
                    max_items: 10,
                },
            )
            .unwrap();

        assert_eq!(resp.items.len(), 3);
    }

    #[test]
    fn shutdown_all_invokes_each_plugin() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SHUTDOWN_CALLS: AtomicUsize = AtomicUsize::new(0);

        struct CountingShutdown;

        impl RecallPlugin for CountingShutdown {
            fn plugin_id(&self) -> &'static str {
                "test:shutdown-counter"
            }
            fn plugin_name(&self) -> &'static str {
                "CountingShutdown"
            }
            fn initialize(&self, _config: &Value) -> PluginResult<()> {
                Ok(())
            }
            fn shutdown(&self) -> PluginResult<()> {
                SHUTDOWN_CALLS.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        impl EnricherPlugin for CountingShutdown {
            fn enrich(&self, _input: EnricherInput<'_>) -> PluginResult<EnricherOutput> {
                Ok(EnricherOutput::zero())
            }
        }

        let mut reg = PluginRegistry::new();
        reg.register_enricher(Arc::new(CountingShutdown), &Value::Null)
            .unwrap();
        SHUTDOWN_CALLS.store(0, Ordering::SeqCst);
        reg.shutdown_all();
        assert_eq!(SHUTDOWN_CALLS.load(Ordering::SeqCst), 1);
    }
}
