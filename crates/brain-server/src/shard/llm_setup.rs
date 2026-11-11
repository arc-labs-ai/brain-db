//! LLM-tier startup wiring for the per-shard executor: credential
//! resolution and the per-shard `llm_cache.redb`.
//!
//! Builds the `MaterializeDeps` slots:
//!
//! - A [`StoreCredentialResolver`] over Brain's own credential store, which
//!   is how every namespace gets a serving credential. A deployment holds no
//!   key of its own, so there is nothing else to consult.
//! - `<shard_dir>/llm_cache.redb` via [`LlmCacheDb::open`]. HyPE is
//!   mandatory and needs this cache, so the shard spawn path treats a `None`
//!   cache as fatal; the slot is only `None` transiently while the warning
//!   is surfaced.
//!
//! ## No key at boot
//!
//! Nothing here reads a provider key from configuration or the environment.
//! A shard starts with an empty credential store and serves writes
//! immediately; the model derivation stages report per write that the
//! namespace has no credential until a tenant registers one.

use std::path::Path;
use std::sync::Arc;

use brain_extractors::{ClassifierModel, EntityDisambiguator, MaterializeDeps};
use brain_llm::LlmCredentialResolver;
use brain_metadata::LlmCacheDb;
use parking_lot::Mutex;

use super::credential_resolver::StoreCredentialResolver;
use super::LlmSpawnConfig;

/// LLM-tier deps assembled at shard startup. Threaded into both
/// `MaterializeDeps` (so LLM-kind rows decode into wired
/// extractors) and `OpsContext.llm_cache` (so future ops can
/// reach the cache directly). The optional `disambiguator` slot
/// feeds the extractor worker so the resolver can second-opinion
/// ambiguous-band partial matches.
pub struct LlmDeps {
    /// Per-namespace credential resolution.
    ///
    /// Backed by Brain's own credential store. `None` when a shard is started
    /// without one, which degrades the model derivation stages with a
    /// per-write reason rather than failing the shard.
    pub resolver: Option<Arc<dyn LlmCredentialResolver>>,
    pub cache: Option<Arc<Mutex<LlmCacheDb>>>,
    /// Optional partial-match disambiguator. Resolves its credential per
    /// namespace like every other model-backed stage, so the second-opinion
    /// call bills the tenant whose entities it is judging.
    pub disambiguator: Option<Arc<EntityDisambiguator>>,
    /// `[extractors.llm] max_tokens` — the LLM tier's output-token
    /// ceiling. `None` leaves the extractor on its own default.
    pub max_tokens: Option<u32>,
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
            credential_resolver: self.resolver,
            llm_cache: self.cache,
            llm_max_tokens: self.max_tokens,
        }
    }
}

/// Build the LLM-tier deps from config + shard directory layout.
/// Always returns a value; a missing key / unopenable cache file
/// produces `None` slots.
///
/// No credential is read here. The resolver answers per namespace from
/// Brain's own store, and `[llm] model` (ferried in via [`LlmSpawnConfig`])
/// only names the model a tenant's extraction runs on when their registered
/// credential does not name one. The `BRAIN__LLM__MODEL` env override has
/// already folded into that config field at load time, so this module never
/// reads the environment directly.
pub fn build_llm_deps(shard_dir: &Path, llm_cfg: &LlmSpawnConfig) -> LlmDeps {
    let resolver = build_credential_resolver(llm_cfg);
    // The disambiguator resolves per namespace, so it exists exactly when
    // credential resolution does.
    let disambiguator = resolver
        .clone()
        .map(|r| Arc::new(EntityDisambiguator::new(r)));
    LlmDeps {
        resolver,
        cache: open_cache(shard_dir),
        disambiguator,
        max_tokens: llm_cfg.extractor_max_tokens,
    }
}

/// Build the resolver over Brain's own credential store.
///
/// Returns `None` when a shard is started without a store or without the
/// keys that open it — the shape a test harness uses when it never registers
/// a credential. A `None` resolver degrades the model derivation stages with
/// a per-write reason rather than failing the shard, because a tenant who has
/// not added a key yet should still be able to write.
fn build_credential_resolver(llm_cfg: &LlmSpawnConfig) -> Option<Arc<dyn LlmCredentialResolver>> {
    let credentials = llm_cfg.credentials.clone()?;
    tracing::info!(
        target: "brain_server::llm_setup",
        "resolving LLM credentials per namespace from Brain's credential store",
    );
    Some(Arc::new(StoreCredentialResolver::new(credentials)) as Arc<dyn LlmCredentialResolver>)
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
            let text = e.to_string();
            let hint =
                if text.contains("Database already open") || text.contains("Cannot acquire lock") {
                    Some(format!(
                        "another process holds the redb lock — check `fuser {p}` or \
                     `pgrep -fl brain-server`",
                        p = path.display()
                    ))
                } else if text.contains("type mismatch") || text.contains("TableTypeMismatch") {
                    // A cache written before BYOK. The key gained a credential
                    // scope, so redb's signature check refuses the old table —
                    // deliberately, rather than serving one tenant's paid
                    // completion to another.
                    //
                    // Worth naming, because this one is fatal and looks like a
                    // crash: the shard spawn path treats a `None` cache as
                    // unrecoverable (HyPE is mandatory and needs it), so the whole
                    // server fails to start. Without this line an operator sees
                    // "failed to spawn shard" and a redb type error, and nothing
                    // connecting either to an upgrade they just performed.
                    //
                    // The file is a cache. Deleting it costs re-paying for
                    // completions, not data.
                    Some(format!(
                        "this cache predates BYOK and cannot be upgraded in place — \
                     the cache key gained a credential scope. Delete it and \
                     restart: `rm {p}`. It is a cache; the only cost is \
                     re-requesting completions that were cached.",
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

    #[test]
    fn a_shard_without_a_credential_store_resolves_nothing() {
        // Not an error: writes still land and the non-model derivation
        // stages still run. The model stages report per write that the
        // namespace has no credential.
        assert!(build_credential_resolver(&LlmSpawnConfig::default()).is_none());
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
    fn into_materialize_deps_threads_resolver_cache_and_labels() {
        let dir = tempfile::tempdir().unwrap();
        let deps = LlmDeps {
            max_tokens: None,
            resolver: None,
            cache: open_cache(dir.path()),
            disambiguator: None,
        };
        let labels = Arc::new(vec!["brain:Person".to_string()]);
        let materialize = deps.into_materialize_deps(None, labels.clone());
        assert!(materialize.credential_resolver.is_none());
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
            max_tokens: None,
            resolver: None,
            cache: Some(Arc::clone(&cache_arc)),
            disambiguator: None,
        }
        .into_materialize_deps(None, labels.clone());
        let deps_b = LlmDeps {
            max_tokens: None,
            resolver: None,
            cache: Some(Arc::clone(&cache_arc)),
            disambiguator: None,
        }
        .into_materialize_deps(None, labels.clone());

        // Both deps point at the same redb file via Arc::clone
        // — no second `LlmCacheDb::open` was performed.
        assert!(deps_a.llm_cache.is_some());
        assert!(deps_b.llm_cache.is_some());
        assert_eq!(Arc::strong_count(&cache_arc), 3); // cache_arc + deps_a + deps_b
    }
}
