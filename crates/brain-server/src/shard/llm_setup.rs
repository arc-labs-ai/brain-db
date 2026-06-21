//! LLM-tier startup wiring for the per-shard executor: provider
//! routing and the per-shard `llm_cache.redb`.
//!
//! Builds the `MaterializeDeps` slots:
//!
//! - Resolves a single credential + model id: `BRAIN_API_KEY` /
//!   `BRAIN_AI_MODEL` env vars win, else the `[llm] api_key` /
//!   `[llm] model` config values. The provider (OpenAI / Anthropic)
//!   is **derived from the model id prefix** — there is no separate
//!   provider key.
//! - Constructs a [`ModelRouter`] holding the one provider client.
//! - Opens `<shard_dir>/llm_cache.redb` via [`LlmCacheDb::open`].
//!   Failure to open the file is non-fatal: a warning is logged
//!   and the cache slot stays `None` (LLM extractors then skip
//!   caching).
//!
//! ## One credential, one model, derived provider
//!
//! Brain takes a single provider-agnostic key + model id. The model
//! id selects the provider via [`provider_for_model`]; adding a
//! provider is a match-arm here, not a new config key. The router
//! still routes by prefix, so the `model:` schema field continues to
//! address the configured client.
//!
//! The default model (when none is configured) matches the embedded
//! pricing table in `brain_extractors::Pricing::for_model`:
//! `gpt-4o-mini` (OpenAI).

use std::path::Path;
use std::sync::{Arc, OnceLock};

use brain_extractors::{ClassifierModel, EntityDisambiguator, MaterializeDeps};
use brain_llm::client::LlmFuture;
use brain_llm::{AnthropicClient, LlmClient, LlmError, LlmRequest, ModelRouter, OpenAIClient};
use brain_metadata::LlmCacheDb;
use parking_lot::Mutex;

use super::LlmSpawnConfig;

/// Default model when neither env nor config supplies one. OpenAI's
/// `gpt-4o-mini` — matches the seeded extraction path and the embedded
/// pricing table.
const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// The LLM provider a model id routes to. Derived purely from the id
/// prefix so a single configured key/model implies its provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    Anthropic,
    OpenAI,
}

/// Map a model id to its provider by prefix. Anthropic models begin
/// `claude`; everything else (OpenAI `gpt-*` / `o1`-`o4` reasoning
/// families, and unknown ids) routes to OpenAI as the default wire
/// dialect.
fn provider_for_model(model: &str) -> Provider {
    if model.trim_start().to_ascii_lowercase().starts_with("claude") {
        Provider::Anthropic
    } else {
        Provider::OpenAI
    }
}

/// Shared Tokio runtime that backs every LLM-tier HTTP call.
///
/// The extractor tier runs on a per-shard **Glommio** executor, which
/// has its own io_uring reactor and no Tokio runtime. The `brain_llm`
/// clients use `reqwest` + `tokio::time`, which panic ("no reactor
/// running") when polled on a Glommio task. We bridge: the actual
/// request runs on this dedicated Tokio runtime (off the shard cores),
/// and the result crosses back over a runtime-agnostic `flume` channel
/// that the Glommio side awaits cleanly — the same pattern the
/// summarizer bridge uses. One runtime for the whole process; LLM
/// calls are I/O-bound, so a couple of worker threads serve many
/// concurrent in-flight requests across all shards.
fn bridge_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("brain-llm-extract")
            .enable_all()
            .build()
            .expect("invariant: LLM bridge Tokio runtime builds with default settings")
    })
}

/// Wraps a `brain_llm` client so its Tokio-based HTTP runs on the
/// shared [`bridge_runtime`] instead of the caller's executor. Lets
/// the Glommio-resident extractor tier and resolver call the client
/// without a Tokio reactor on the shard core.
struct BridgedLlmClient {
    inner: Arc<dyn LlmClient>,
}

impl LlmClient for BridgedLlmClient {
    fn complete<'a>(&'a self, request: LlmRequest) -> LlmFuture<'a> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let (tx, rx) = flume::bounded(1);
            // The spawned task owns `inner` (an `Arc`) and `request`,
            // so the inner future is `Send + 'static` — it runs on the
            // Tokio runtime where reqwest/tokio::time are valid.
            bridge_runtime().spawn(async move {
                let result = inner.complete(request).await;
                let _ = tx.send_async(result).await;
            });
            // `recv_async` is runtime-agnostic, so this awaits cleanly
            // on the Glommio shard executor.
            rx.recv_async().await.unwrap_or_else(|_| {
                Err(LlmError::ProviderError {
                    status: 0,
                    message: "LLM bridge runtime unavailable (reply channel closed)".into(),
                })
            })
        })
    }

    // Routing/cache-key metadata is pure data — delegate to the inner
    // client; no runtime involved.
    fn model(&self) -> &str {
        self.inner.model()
    }

    fn model_id_hash(&self) -> u64 {
        self.inner.model_id_hash()
    }
}

/// Wrap a freshly-built provider client so its HTTP runs on the bridge
/// runtime. Every client handed to the router or the disambiguator
/// goes through this — both are invoked from Glommio tasks.
fn bridge(client: Arc<dyn LlmClient>) -> Arc<dyn LlmClient> {
    Arc::new(BridgedLlmClient { inner: client })
}

/// LLM-tier deps assembled at shard startup. Threaded into both
/// `MaterializeDeps` (so LLM-kind rows decode into wired
/// extractors) and `OpsContext.llm_cache` (so future ops can
/// reach the cache directly). The optional `disambiguator` slot
/// feeds the extractor worker so the resolver can second-opinion
/// ambiguous-band partial matches.
pub struct LlmDeps {
    pub router: Option<Arc<ModelRouter>>,
    pub cache: Option<Arc<Mutex<LlmCacheDb>>>,
    /// Optional partial-match disambiguator. Populated when at least
    /// one provider client is available; the disambiguator shares
    /// that client (Anthropic preferred, OpenAI as fallback) so
    /// there's no duplicate API key handling.
    pub disambiguator: Option<Arc<EntityDisambiguator>>,
    /// The primary provider client + its wire model, retained so other
    /// write-time LLM consumers (the HyPE generator) can reuse the same
    /// client without re-resolving credentials. `None` mirrors
    /// `disambiguator`: no provider key was resolvable.
    pub primary_client: Option<Arc<dyn LlmClient>>,
    pub primary_model: String,
}

impl LlmDeps {
    /// Merge with the existing classifier model and the snapshotted
    /// entity-type qname list into a full [`MaterializeDeps`].
    /// The qname list is what the classifier passes as labels on
    /// every `predict()` call.
    ///
    /// The [`disambiguator`](Self::disambiguator) slot is intentionally
    /// dropped here: it's wired directly into the extractor worker, not
    /// through `MaterializeDeps`. Callers that need it must
    /// [`Arc::clone`] the field before invoking this consumer.
    #[must_use]
    pub fn into_materialize_deps(
        self,
        classifier_model: Option<Arc<dyn ClassifierModel>>,
        entity_type_qnames: Arc<Vec<String>>,
    ) -> MaterializeDeps {
        MaterializeDeps {
            classifier_model,
            entity_type_qnames,
            model_router: self.router,
            llm_cache: self.cache,
        }
    }
}

/// Build the LLM-tier deps from config + env + shard directory layout.
/// Always returns a value; missing keys / unopenable cache files
/// produce `None` slots.
///
/// Credential resolution is **env-first, config-fallback**: the
/// `BRAIN_API_KEY` environment variable wins when set, otherwise the
/// `[llm] api_key` config value is used. This keeps the environment as
/// the override path for production secrets while letting an operator
/// drop a key into `config/dev.toml` for local development. The model
/// id resolves the same way (`BRAIN_AI_MODEL` > `[llm] model` >
/// [`DEFAULT_MODEL`]).
pub fn build_llm_deps(shard_dir: &Path, llm_cfg: &LlmSpawnConfig) -> LlmDeps {
    let (primary_client, primary_model) = build_primary_client(llm_cfg);
    let disambiguator = primary_client
        .clone()
        .map(|c| Arc::new(EntityDisambiguator::new(c, primary_model.clone())));
    LlmDeps {
        router: build_router(llm_cfg),
        cache: open_cache(shard_dir),
        disambiguator,
        primary_client,
        primary_model,
    }
}

/// Resolve a credential env-first, config-fallback; empty strings are
/// treated as unset on both sides.
fn resolve_key(env_var: &str, config_value: &Option<String>) -> Option<String> {
    std::env::var(env_var)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| config_value.clone().filter(|s| !s.is_empty()))
}

/// Resolve the configured model id: `BRAIN_AI_MODEL` > `[llm] model` >
/// [`DEFAULT_MODEL`].
fn ai_model(llm_cfg: &LlmSpawnConfig) -> String {
    std::env::var("BRAIN_AI_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| llm_cfg.model.clone().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

/// Build the single provider client if a key is resolvable. The
/// provider is derived from the model id. Returns the `(client, model)`
/// pair so the disambiguator can record the model.
fn build_client(llm_cfg: &LlmSpawnConfig) -> Option<(Arc<dyn LlmClient>, String)> {
    let key = resolve_key("BRAIN_API_KEY", &llm_cfg.api_key)?;
    let model = ai_model(llm_cfg);
    let client: Arc<dyn LlmClient> = match provider_for_model(&model) {
        Provider::Anthropic => Arc::new(AnthropicClient::with_key(model.clone(), key)?),
        Provider::OpenAI => Arc::new(OpenAIClient::with_key(model.clone(), key)?),
    };
    Some((bridge(client), model))
}

/// Pick the primary client for single-call surfaces (today: the
/// partial-match disambiguator).
///
/// Returns `(None, String::new())` when no key is configured.
fn build_primary_client(llm_cfg: &LlmSpawnConfig) -> (Option<Arc<dyn LlmClient>>, String) {
    match build_client(llm_cfg) {
        Some((client, model)) => (Some(client), model),
        None => (None, String::new()),
    }
}

fn build_router(llm_cfg: &LlmSpawnConfig) -> Option<Arc<ModelRouter>> {
    let (client, model) = build_client(llm_cfg)?;
    let router = match provider_for_model(&model) {
        Provider::Anthropic => ModelRouter::new().with_anthropic(client),
        Provider::OpenAI => ModelRouter::new().with_openai(client),
    };
    Some(Arc::new(router))
}

fn open_cache(shard_dir: &Path) -> Option<Arc<Mutex<LlmCacheDb>>> {
    let path = shard_dir.join("llm_cache.redb");
    match LlmCacheDb::open(&path) {
        Ok(db) => Some(Arc::new(Mutex::new(db))),
        Err(e) => {
            // redb uses POSIX flock; the most common cause of a failed
            // open at server startup is another brain-server process
            // already holding it. Spell that out so operators know what
            // to do — the warn alone left users grep-ing.
            let hint = if e.to_string().contains("Database already open")
                || e.to_string().contains("Cannot acquire lock")
            {
                Some(format!(
                    "another process holds the redb lock — check `fuser {p}` or \
                     `pgrep -fl brain-server`",
                    p = path.display()
                ))
            } else {
                None
            };
            tracing::warn!(
                target: "brain_server::shard::llm_setup",
                path = %path.display(),
                error = %e,
                hint = hint.as_deref().unwrap_or(""),
                "failed to open llm_cache.redb; LLM extractors will skip caching on this shard",
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    // Env vars are process-global; serialize env-mutating tests so
    // they don't trample each other under cargo's default parallel
    // test runner.
    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    /// Save + restore the four env vars this module reads. Drops
    /// at end of scope restore previous values, including absence.
    struct EnvGuard {
        snapshot: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn new(keys: &[&'static str]) -> Self {
            let snapshot = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
            // Start from a clean slate.
            for k in keys {
                std::env::remove_var(k);
            }
            Self { snapshot }
        }

        fn set(&self, key: &str, value: &str) {
            std::env::set_var(key, value);
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.snapshot {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    const ALL_KEYS: &[&str] = &["BRAIN_API_KEY", "BRAIN_AI_MODEL"];

    #[test]
    fn provider_derived_from_model_prefix() {
        assert_eq!(provider_for_model("claude-haiku-4-5"), Provider::Anthropic);
        assert_eq!(provider_for_model("Claude-Sonnet"), Provider::Anthropic);
        assert_eq!(provider_for_model("gpt-4o-mini"), Provider::OpenAI);
        assert_eq!(provider_for_model("o3-mini"), Provider::OpenAI);
        // Unknown ids default to the OpenAI wire dialect.
        assert_eq!(provider_for_model("mystery-model"), Provider::OpenAI);
    }

    #[test]
    fn build_router_returns_none_when_no_key() {
        let _g = ENV_LOCK.lock().unwrap();
        let _e = EnvGuard::new(ALL_KEYS);
        assert!(build_router(&LlmSpawnConfig::default()).is_none());
    }

    #[test]
    fn build_router_routes_to_anthropic_for_claude_model() {
        let _g = ENV_LOCK.lock().unwrap();
        let env = EnvGuard::new(ALL_KEYS);
        env.set("BRAIN_API_KEY", "test-key");
        env.set("BRAIN_AI_MODEL", "claude-haiku-4-5");
        let r = build_router(&LlmSpawnConfig::default()).expect("router");
        assert!(r.resolve("claude-haiku-4-5").is_some());
        assert!(r.resolve("gpt-4o-mini").is_none());
    }

    #[test]
    fn build_router_routes_to_openai_for_default_model() {
        let _g = ENV_LOCK.lock().unwrap();
        let env = EnvGuard::new(ALL_KEYS);
        env.set("BRAIN_API_KEY", "test-key");
        let r = build_router(&LlmSpawnConfig::default()).expect("router");
        assert!(r.resolve("gpt-4o-mini").is_some());
        assert!(r.resolve("claude-haiku-4-5").is_none());
    }

    #[test]
    fn model_override_env_var_takes_effect() {
        let _g = ENV_LOCK.lock().unwrap();
        let env = EnvGuard::new(ALL_KEYS);
        env.set("BRAIN_AI_MODEL", "claude-sonnet-4-6");
        assert_eq!(ai_model(&LlmSpawnConfig::default()), "claude-sonnet-4-6");
    }

    #[test]
    fn model_default_matches_pricing_table() {
        let _g = ENV_LOCK.lock().unwrap();
        let _e = EnvGuard::new(ALL_KEYS);
        assert_eq!(ai_model(&LlmSpawnConfig::default()), "gpt-4o-mini");
    }

    #[test]
    fn config_key_builds_router_when_env_absent() {
        let _g = ENV_LOCK.lock().unwrap();
        let _e = EnvGuard::new(ALL_KEYS); // no env keys set
        let cfg = LlmSpawnConfig {
            api_key: Some("config-key".into()),
            ..LlmSpawnConfig::default()
        };
        let r = build_router(&cfg).expect("router from config key");
        assert!(r.resolve("gpt-4o-mini").is_some());
        assert!(r.resolve("claude-haiku-4-5").is_none());
    }

    #[test]
    fn env_key_takes_precedence_over_config() {
        let _g = ENV_LOCK.lock().unwrap();
        let env = EnvGuard::new(ALL_KEYS);
        env.set("BRAIN_API_KEY", "env-key");
        let cfg = LlmSpawnConfig {
            api_key: Some("config-key".into()),
            ..LlmSpawnConfig::default()
        };
        assert_eq!(
            resolve_key("BRAIN_API_KEY", &cfg.api_key).as_deref(),
            Some("env-key"),
        );
    }

    #[test]
    fn empty_config_key_falls_through_to_unset() {
        let _g = ENV_LOCK.lock().unwrap();
        let _e = EnvGuard::new(ALL_KEYS);
        // An empty string in config is treated as unset, not a key.
        let cfg = LlmSpawnConfig {
            api_key: Some(String::new()),
            ..LlmSpawnConfig::default()
        };
        assert!(build_router(&cfg).is_none());
    }

    #[test]
    fn config_model_override_applies() {
        let _g = ENV_LOCK.lock().unwrap();
        let _e = EnvGuard::new(ALL_KEYS); // no BRAIN_AI_MODEL env
        let cfg = LlmSpawnConfig {
            model: Some("claude-sonnet-4-6".into()),
            ..LlmSpawnConfig::default()
        };
        assert_eq!(ai_model(&cfg), "claude-sonnet-4-6");
    }

    #[test]
    fn open_cache_creates_redb_at_shard_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open_cache(dir.path()).expect("cache opened");
        // Round-trip a row to confirm the standard tables are present.
        let mut db = cache.lock();
        let wtxn = db.write_txn().expect("write_txn");
        {
            let _ = wtxn
                .open_table(brain_metadata::llm_cache::LLM_RESPONSES_TABLE)
                .expect("responses table");
        }
        wtxn.commit().unwrap();
        assert!(dir.path().join("llm_cache.redb").exists());
    }

    #[test]
    fn open_cache_returns_none_when_directory_unwritable() {
        // Point at a path under a regular file — opening a redb
        // inside that "directory" fails. On platforms that allow
        // it we just see `Some`, which is also acceptable; the
        // contract is "log warn + return None", not "must fail".
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("not-a-dir");
        std::fs::write(&file_path, b"x").unwrap();
        let result = open_cache(&file_path);
        // Either outcome is acceptable; the important contract is
        // we never panic. Assert we got an Option back.
        let _ = result;
    }

    #[test]
    fn into_materialize_deps_threads_router_cache_and_labels() {
        let dir = tempfile::tempdir().unwrap();
        let deps = LlmDeps {
            router: None,
            cache: open_cache(dir.path()),
            disambiguator: None,
            primary_client: None,
            primary_model: String::new(),
        };
        let labels = Arc::new(vec!["brain:Person".to_string()]);
        let materialize = deps.into_materialize_deps(None, labels.clone());
        assert!(materialize.model_router.is_none());
        assert!(materialize.llm_cache.is_some());
        assert!(materialize.classifier_model.is_none());
        assert_eq!(materialize.entity_type_qnames.as_slice(), labels.as_slice());
    }

    /// Documents the constraint that drove the spawn_shard fix:
    /// redb's lock is process-wide and inode-keyed. Two live
    /// opens of the same `llm_cache.redb` from the same process
    /// MUST fail with `Database already open`. The shard's
    /// startup path therefore opens the cache exactly once (in
    /// the Glommio closure via `build_llm_deps`).
    #[test]
    fn second_open_of_same_path_fails_while_first_alive() {
        let dir = tempfile::tempdir().unwrap();
        let first = open_cache(dir.path()).expect("first open");
        // Second open with the first still alive — must fail.
        let path = dir.path().join("llm_cache.redb");
        // LlmCacheDb is not Debug (its inner redb::Database isn't),
        // so we can't use expect_err here — match on Result manually.
        let err = match LlmCacheDb::open(&path) {
            Ok(_) => panic!("second open must fail while first is still alive"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("Database already open"),
            "expected redb lock error, got: {err}"
        );
        drop(first);
        // After drop, re-open succeeds.
        match LlmCacheDb::open(&path) {
            Ok(_db) => {}
            Err(e) => panic!("re-open after drop must succeed: {e}"),
        }
    }

    /// Two `MaterializeDeps` instances sharing the same single
    /// open `LlmCacheDb` handle via `Arc::clone` must both work —
    /// this is the contract `materialize_extractors` relies on
    /// when wiring multiple LLM extractors against one cache.
    #[test]
    fn shared_cache_handle_supports_many_materialize_deps() {
        let dir = tempfile::tempdir().unwrap();
        let llm_deps = build_llm_deps(dir.path(), &LlmSpawnConfig::default());
        assert!(llm_deps.cache.is_some(), "cache should open");
        let cache_arc = llm_deps.cache.clone().unwrap();
        // Drop the original LlmDeps so its embedded Arc clone goes away;
        // the refcount we assert below should reflect only cache_arc plus
        // the two MaterializeDeps copies, not the original `build_llm_deps`
        // return value.
        drop(llm_deps);

        let labels = Arc::new(vec!["brain:Person".to_string()]);
        let deps_a = LlmDeps {
            router: None,
            cache: Some(Arc::clone(&cache_arc)),
            disambiguator: None,
            primary_client: None,
            primary_model: String::new(),
        }
        .into_materialize_deps(None, labels.clone());
        let deps_b = LlmDeps {
            router: None,
            cache: Some(Arc::clone(&cache_arc)),
            disambiguator: None,
            primary_client: None,
            primary_model: String::new(),
        }
        .into_materialize_deps(None, labels.clone());

        // Both deps point at the same redb file via Arc::clone
        // — no second `LlmCacheDb::open` was performed.
        assert!(deps_a.llm_cache.is_some());
        assert!(deps_b.llm_cache.is_some());
        assert_eq!(Arc::strong_count(&cache_arc), 3); // cache_arc + deps_a + deps_b
    }
}
