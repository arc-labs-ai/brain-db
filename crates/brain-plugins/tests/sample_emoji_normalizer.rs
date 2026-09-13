//! Sample enricher plugin — `EmojiNormalizer` — and the two
//! integration tests called out in the W3.3 plan:
//!
//! - `emoji_normalizer_mutates_entity_text` — the substitution lands.
//! - `registry_skips_failing_plugin_and_continues` — fail-open
//!   semantics.

use std::sync::Arc;

use brain_core::SpaceId;
use brain_extractors::framework::item::{EntityMention, ExtractedItem};
use brain_plugins::{
    EnricherInput, EnricherOutput, EnricherPlugin, PluginError, PluginRegistry, PluginResult,
    RecallPlugin,
};
use serde_json::Value;

// ---------------------------------------------------------------------------
// EmojiNormalizer — replace 🚀 with "rocket" inside EntityMention.text.
// ---------------------------------------------------------------------------

struct EmojiNormalizer;

impl RecallPlugin for EmojiNormalizer {
    fn plugin_id(&self) -> &'static str {
        "test:emoji-normalizer"
    }
    fn plugin_name(&self) -> &'static str {
        "Emoji to text"
    }
    fn initialize(&self, _config: &Value) -> PluginResult<()> {
        Ok(())
    }
}

impl EnricherPlugin for EmojiNormalizer {
    fn enrich(&self, input: EnricherInput<'_>) -> PluginResult<EnricherOutput> {
        let mut mutated = 0;
        for item in input.items.iter_mut() {
            if let ExtractedItem::EntityMention(em) = item {
                if em.text.contains('\u{1F680}') {
                    em.text = em.text.replace('\u{1F680}', "rocket");
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

// ---------------------------------------------------------------------------
// Failing fixture used by the fail-open test.
// ---------------------------------------------------------------------------

struct AlwaysFails;

impl RecallPlugin for AlwaysFails {
    fn plugin_id(&self) -> &'static str {
        "test:always-fails"
    }
    fn plugin_name(&self) -> &'static str {
        "Always fails"
    }
    fn initialize(&self, _config: &Value) -> PluginResult<()> {
        Ok(())
    }
}

impl EnricherPlugin for AlwaysFails {
    fn enrich(&self, _input: EnricherInput<'_>) -> PluginResult<EnricherOutput> {
        Err(PluginError::Failed {
            plugin_id: "test:always-fails",
            message: "synthetic upstream timeout".into(),
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn rocket_mention() -> ExtractedItem {
    ExtractedItem::EntityMention(EntityMention {
        entity_type_qname: "brain:Project".into(),
        text: "Project \u{1F680} Launch".into(),
        start: 0,
        end: 17,
        confidence: 0.8,
        extractor_id: 1,
        extractor_version: 1,
    })
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn emoji_normalizer_mutates_entity_text() {
    let mut reg = PluginRegistry::new();
    reg.register_enricher(Arc::new(EmojiNormalizer), &Value::Null)
        .expect("register");

    let mut items = vec![rocket_mention()];
    let outcomes = reg.run_enrichers(EnricherInput {
        space_id: SpaceId::NIL,
        items: &mut items,
        source_text: "Project \u{1F680} Launch",
        now_unix_nanos: 0,
    });

    assert_eq!(outcomes.len(), 1);
    let out = outcomes[0].result.as_ref().expect("ok");
    assert_eq!(out.items_mutated, 1);
    assert_eq!(out.items_added, 0);
    assert_eq!(out.items_dropped, 0);

    let ExtractedItem::EntityMention(em) = &items[0] else {
        panic!("expected EntityMention");
    };
    assert_eq!(em.text, "Project rocket Launch");
    // Original byte range is now stale — the host's downstream stages
    // are responsible for re-aligning offsets. The plugin trait does
    // not promise span coherence; it promises content mutation.
}

#[test]
fn registry_skips_failing_plugin_and_continues() {
    let mut reg = PluginRegistry::new();
    reg.register_enricher(Arc::new(AlwaysFails), &Value::Null)
        .expect("register failing");
    reg.register_enricher(Arc::new(EmojiNormalizer), &Value::Null)
        .expect("register normalizer");

    let mut items = vec![rocket_mention()];
    let outcomes = reg.run_enrichers(EnricherInput {
        space_id: SpaceId::NIL,
        items: &mut items,
        source_text: "irrelevant",
        now_unix_nanos: 0,
    });

    // Both plugins ran. The failing one returned an error; the
    // normalizer ran AFTER and still mutated the item.
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].plugin_id, "test:always-fails");
    assert!(matches!(
        &outcomes[0].result,
        Err(PluginError::Failed {
            plugin_id: "test:always-fails",
            ..
        })
    ));
    assert_eq!(outcomes[1].plugin_id, "test:emoji-normalizer");
    assert!(outcomes[1].result.is_ok());

    let ExtractedItem::EntityMention(em) = &items[0] else {
        panic!("expected EntityMention");
    };
    assert_eq!(em.text, "Project rocket Launch");
}
