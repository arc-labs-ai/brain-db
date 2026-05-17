# 21.5 — Server-side LLM wiring + cache hook

Lights up the `MaterializeDeps` slots 21.4 left empty:

- Constructs the `ModelRouter` from `ANTHROPIC_API_KEY` /
  `OPENAI_API_KEY` env vars at shard startup.
- Opens the per-shard `llm_cache.redb` via `LlmCacheDb::open`.
- Threads both through `MaterializeDeps` (so LLM-kind rows
  become wired extractors) and into `OpsContext.llm_cache` (so
  future ops can reach the cache directly).

Phase 21.6 (mock-injected integration tests) then exercises the
end-to-end flow against the registry the materializer builds
here.

## Files written / modified

| Path | Change |
|---|---|
| `crates/brain-ops/src/context.rs` | Add `llm_cache: Option<Arc<Mutex<LlmCacheDb>>>` field + `with_llm_cache` builder. Default `None`. |
| `crates/brain-ops/Cargo.toml` | Add `brain-metadata` to the dep set (already a transitive — verify; add only if missing). |
| `crates/brain-server/src/shard/llm_setup.rs` | New: `build_llm_deps(shard_dir) -> LlmDeps`. Reads env, opens cache, returns router + cache. |
| `crates/brain-server/src/shard/mod.rs` | Replace the `MaterializeDeps::default()` line with a call to `build_llm_deps`. Pass `llm_cache` into `OpsContext` via the new builder. |
| `crates/brain-server/Cargo.toml` | Add `brain-llm` dep (clients) and confirm `brain-metadata` is already present. |
| `crates/brain-extractors/src/llm.rs` | No code change — but `LlmExtractor` already accepts `Arc<Mutex<LlmCacheDb>>`; just verify the materializer threads it through. |
| `crates/brain-ops/src/lib.rs` | Re-export the new builder. |

## `shard/llm_setup.rs` shape

```rust
//! Construct LLM-tier deps from env + per-shard storage layout.
//! Spec §22/09 §2 (provider routing) + §15.4 (per-shard cache file).

use std::path::Path;
use std::sync::Arc;

use brain_extractors::MaterializeDeps;
use brain_llm::{AnthropicClient, LlmClient, ModelRouter, OpenAIClient};
use brain_metadata::LlmCacheDb;
use parking_lot::Mutex;

const DEFAULT_ANTHROPIC_MODEL: &str = "claude-haiku-4-5";
const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-mini";

/// Build the LLM-tier `MaterializeDeps` slice from env + shard
/// directory. `classifier_model` is filled in by the caller (the
/// shard already has the candle NER instance).
pub fn build_llm_deps(shard_dir: &Path) -> LlmDeps {
    let router = build_router();
    let cache = open_cache(shard_dir);
    LlmDeps { router, cache }
}

pub struct LlmDeps {
    pub router: Option<Arc<ModelRouter>>,
    pub cache: Option<Arc<Mutex<LlmCacheDb>>>,
}

impl LlmDeps {
    /// Merge with the existing classifier model into a full
    /// `MaterializeDeps`.
    pub fn into_materialize_deps(
        self,
        classifier_model: Option<Arc<dyn brain_extractors::ClassifierModel>>,
    ) -> MaterializeDeps {
        MaterializeDeps {
            classifier_model,
            model_router: self.router,
            llm_cache: self.cache,
        }
    }
}

fn build_router() -> Option<Arc<ModelRouter>> {
    let mut r = ModelRouter::new();
    let mut any = false;

    let anthropic_model = std::env::var("BRAIN_ANTHROPIC_MODEL")
        .unwrap_or_else(|_| DEFAULT_ANTHROPIC_MODEL.to_string());
    if let Some(c) = AnthropicClient::from_env(anthropic_model.clone()) {
        r = r.with_anthropic(Arc::new(c) as Arc<dyn LlmClient>);
        any = true;
    }

    let openai_model = std::env::var("BRAIN_OPENAI_MODEL")
        .unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_string());
    if let Some(c) = OpenAIClient::from_env(openai_model.clone()) {
        r = r.with_openai(Arc::new(c) as Arc<dyn LlmClient>);
        any = true;
    }

    any.then(|| Arc::new(r))
}

fn open_cache(shard_dir: &Path) -> Option<Arc<Mutex<LlmCacheDb>>> {
    let path = shard_dir.join("llm_cache.redb");
    match LlmCacheDb::open(&path) {
        Ok(db) => Some(Arc::new(Mutex::new(db))),
        Err(e) => {
            tracing::warn!(
                target: "brain_server::shard::llm_setup",
                path = %path.display(),
                error = %e,
                "failed to open llm_cache.redb; LLM extractors will skip caching",
            );
            None
        }
    }
}
```

## Shard startup wiring

```rust
// 21.5: open the per-shard LLM cache + build the model router
// from env. Returns `None` for slots we can't fill; the
// materializer registers LLM rows as degraded for those.
let llm_deps = llm_setup::build_llm_deps(&shard_dir);
let llm_cache_for_ops = llm_deps.cache.clone();
let materialize_deps =
    llm_deps.into_materialize_deps(classifier_model.clone());

let (reg, errors) = brain_extractors::build_registry_from_definitions(
    &defs,
    &materialize_deps,
);
// ... existing warn loop ...

let ops = Arc::new(
    OpsContext::new(executor_ctx)
        .with_extractor_registry(extractor_registry)
        .with_classifier_config(classifier_config)
        .with_llm_cache(llm_cache_for_ops),
);
```

`classifier_model` is unchanged from 20.7b — still loaded via
`BertTokenClassifier::load(&classifier_config)` (or `None`
degraded).

## `OpsContext` change

```rust
pub struct OpsContext {
    // existing fields...
    /// Per-shard LLM extractor response cache. `None` when the
    /// cache file failed to open or no LLM extractors are
    /// configured. Future ops (e.g. `RECALL` provenance lookups)
    /// can read through this without going through the registry.
    pub llm_cache: Option<Arc<Mutex<LlmCacheDb>>>,
}

impl OpsContext {
    pub fn with_llm_cache(
        mut self,
        cache: Option<Arc<Mutex<LlmCacheDb>>>,
    ) -> Self {
        self.llm_cache = cache;
        self
    }
}
```

Default in `OpsContext::new` is `None` — substrate-only
deployments and the existing tests carry on unchanged.

## Default model selection

§22/09 §2 says routing is prefix-only. One client per provider
in v1; the operator's `model:` field selects the provider, the
wire model is whatever the server-side client was constructed
for. Defaults:

- Anthropic → `claude-haiku-4-5` (cheapest known in our
  pricing table).
- OpenAI → `gpt-4o-mini`.

Operator overrides via `BRAIN_ANTHROPIC_MODEL` /
`BRAIN_OPENAI_MODEL` env vars. Documented in the
`shard/llm_setup.rs` module comment so phase 22+'s per-extractor
model selection has a hook to break against.

## Tests

In `brain-server/src/shard/llm_setup.rs` (or a sibling
integration test if env mutation cross-talk is risky):

1. `build_router_returns_none_when_no_keys` — both env vars
   unset → `None`.
2. `build_router_with_anthropic_only` — only
   `ANTHROPIC_API_KEY` set → router resolves `claude-haiku-4-5`
   but not `gpt-4o-mini`.
3. `build_router_with_both_keys` — both set → router resolves
   both prefixes.
4. `open_cache_creates_redb_at_shard_dir` — passing a tempdir
   produces a usable `LlmCacheDb` with the standard tables
   present (round-trip a row).
5. `open_cache_returns_none_when_directory_unwritable` —
   point at `/dev/null/llm_cache.redb` → logs warn, returns
   `None`. (Skipped on platforms that allow the path.)

Tests use `std::sync::Mutex` around env mutation to serialize
when not running in `--test-threads=1`; or simply use the
`serial_test` crate if it's already a workspace dep. Otherwise,
use unique env var names per test via a small `Guard` helper.

OpsContext tests in `brain-ops/src/context.rs`:

6. `with_llm_cache_replaces_cache` — round-trip the builder.
7. `default_ops_context_has_no_llm_cache`.

## Out of scope

- Per-extractor model selection (operator declares
  `model: claude-sonnet-4-6`, server uses sonnet for that row
  specifically) — phase 22+.
- Cache sweeper (TTL eviction + LRU) — phase 24 per §27/07 Q4.
- Pricing override TOML — post-v1.
- Multi-tenant API key rotation — post-v1.

## Single commit

`feat(server,ops): 21.5 — server-side LLM router + cache wiring`

## Verification

```
just docker cargo test -p brain-ops -p brain-server --lib --bins
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu \
    -p brain-ops -p brain-server --all-targets -- -D warnings
```
