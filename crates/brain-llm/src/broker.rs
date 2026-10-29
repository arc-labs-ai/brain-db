//! Broker-backed credential resolution (BYOK).
//!
//! Resolves a namespace's LLM credential by asking `brain-edge`, which holds
//! the tenant's sealed provider key. **Brain never receives the key**: the edge
//! tells it only which provider and model to build a client for, and that
//! client's requests are re-signed at the edge on the way upstream.
//!
//! That is what keeps provider keys out of Brain's data at rest — WAL,
//! snapshots and backups never see one, because one never arrives.
//!
//! # How the client reaches the edge unchanged
//!
//! [`crate::OpenAIClient`] / [`crate::AnthropicClient`] build their URL as
//! `format!("{}/v1/...", base_url)` and authenticate with `bearer_auth`. They
//! cannot set a custom header, so both routing inputs travel in the two
//! channels they do control:
//!
//! - **namespace** → in `base_url`, as `{edge}/llm/ns/{namespace}/{provider}`
//! - **broker token** → as the bearer, in place of a provider key
//!
//! So the provider clients are reused verbatim; nothing in `providers/` knows
//! a broker exists.
//!
//! # Caching, and why the negative TTL is shorter
//!
//! HyPE runs on every write, so an uncached resolve per memory would put a
//! request to the edge on the write path. Results are cached per namespace.
//!
//! A **negative** result (this tenant has no key) is cached far more briefly
//! than a positive one: it is the state a tenant actively fixes by registering
//! a key, and caching "missing" for five minutes would mean their writes keep
//! failing for five minutes after they did the thing that fixes it. A positive
//! entry has no such urgency — it bounds how long a *revoked* key keeps
//! working, which is a slower-moving concern.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde::Deserialize;

use crate::client::LlmClient;
use crate::providers::{AnthropicClient, OpenAIClient};
use crate::resolver::{
    CredentialFuture, CredentialOutcome, CredentialScope, LlmCredentialResolver, ResolvedCredential,
};
use crate::router::Provider;

/// Path prefix the edge serves the broker under.
///
/// `brain-edge` declares the same constant on its side. The two live in
/// separate repos so neither can reference the other's; a test on each side
/// pins the resulting URL shape, which is what catches a drift.
pub const BROKER_PATH_PREFIX: &str = "/llm/ns";

/// Default lifetime of a positive cache entry.
///
/// Bounds how long a revoked provider key keeps working after the tenant
/// removes it.
pub const DEFAULT_TTL: Duration = Duration::from_secs(300);

/// Lifetime of a negative cache entry ("this namespace has no key").
///
/// Deliberately short: this is the state a tenant resolves by registering a
/// key, and a long negative cache would keep rejecting their writes well after
/// they fixed the problem.
pub const DEFAULT_NEGATIVE_TTL: Duration = Duration::from_secs(10);

/// How long the synchronous ENCODE gate keeps REFUSING a namespace it has
/// seen as missing.
///
/// Separate from [`DEFAULT_NEGATIVE_TTL`], and much longer, because the two
/// consumers want opposite things. `client_for` wants the negative entry to
/// expire fast so a derivation re-fetches and notices a key the moment it is
/// registered. The gate wants it to persist, or it only ever fires for writes
/// arriving within 10s of each other — which is to say never, on the sparse
/// traffic of a sandbox or a demo, and a keyless namespace then degrades in
/// silence exactly as the gate exists to prevent.
pub const DEFAULT_NEGATIVE_GATE_TTL: Duration = Duration::from_secs(300);

/// How often one write is admitted anyway while the gate is refusing.
///
/// The gate runs BEFORE any derivation, so a namespace it refuses issues no
/// `client_for` and therefore never fetches the `Ready` that would clear it:
/// a long negative window with no escape is self-sustaining, and a tenant who
/// registers a key stays rejected with nothing able to unstick them. Letting
/// one write through per interval is that escape. It pays one silently
/// degraded write per interval to keep the namespace recoverable without the
/// gate having to perform I/O it is forbidden from performing.
pub const DEFAULT_NEGATIVE_PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// What the edge said about a namespace, and when.
#[derive(Clone)]
enum Cached {
    /// The namespace has a credential serving this provider/model.
    Ready { provider: String, model: String },
    /// The namespace has no credential.
    Missing,
}

struct Entry {
    value: Cached,
    fetched_at: Instant,
    /// When the gate last let a write through to re-probe this namespace.
    ///
    /// Only meaningful for [`Cached::Missing`]. Starts at `fetched_at` so the
    /// write that discovered the miss counts as the first probe, and the next
    /// one is a full interval away rather than immediate.
    last_gate_probe: Instant,
}

/// `GET /llm/ns/{namespace}/resolve` response. Carries no key material.
#[derive(Debug, Deserialize)]
struct ResolveBody {
    provider: String,
    model: String,
}

/// What one broker HTTP call produced, reduced to owned data.
///
/// The call runs on a different executor from its caller, so nothing that
/// borrows the resolver — and no `reqwest::Response`, whose body is read on
/// the runtime that fetched it — may cross back. Only this does.
enum BrokerHttp {
    /// The edge named a provider and model for the namespace.
    Resolved(ResolveBody),
    /// `409` — the tenant has registered no key.
    Missing,
    /// The edge could not be reached at all.
    Unreachable(String),
    /// Reached, but answered with a status we do not understand.
    Status(u16),
    /// Reached and successful, but the body did not decode.
    Undecodable(String),
}

/// Ask the edge who serves `url`, entirely within one runtime.
///
/// Split out of [`BrokerResolver::fetch`] so the whole request — connect, TLS,
/// response body — happens on the executor that owns the reactor. Reading the
/// body is part of that: a `reqwest::Response` handed back across executors
/// would panic on `.json()` for exactly the reason this function exists.
async fn broker_get(http: reqwest::Client, url: String, token: String) -> BrokerHttp {
    let resp = match http.get(&url).bearer_auth(&token).send().await {
        Ok(r) => r,
        Err(e) => return BrokerHttp::Unreachable(e.to_string()),
    };
    let status = resp.status();
    if status.as_u16() == 409 {
        return BrokerHttp::Missing;
    }
    if !status.is_success() {
        return BrokerHttp::Status(status.as_u16());
    }
    match resp.json::<ResolveBody>().await {
        Ok(body) => BrokerHttp::Resolved(body),
        Err(e) => BrokerHttp::Undecodable(e.to_string()),
    }
}

/// The runtime the broker's HTTP runs on.
///
/// `reqwest` is hyper-based, and hyper's DNS and TCP calls panic with "there is
/// no reactor running, must be called from the context of a Tokio 1.x runtime"
/// unless they are polled inside a Tokio runtime. [`BrokerResolver::client_for`]
/// is awaited from the LLM extractor tier, which runs on a **glommio** shard
/// executor — so there is no such runtime at the call site, and the first
/// uncached resolve panicked the shard and aborted the process (issue #34).
///
/// It is not a URL-shape problem: with a hostname the panic lands in hyper's
/// DNS resolver, with an IP literal it lands in Tokio's TCP connect. The whole
/// call stack needs the reactor, so the fix is to give it one.
///
/// One runtime per process, one worker thread. This carries a single small JSON
/// request per namespace per TTL — it is a control path, not a traffic path.
/// Multi-threaded rather than current-thread on purpose: a current-thread
/// runtime only advances inside `block_on`, and nothing here calls that, so
/// work spawned onto it from a glommio thread would never run.
///
/// Built once and leaked for the process lifetime. Dropping a Tokio runtime
/// blocks on its workers and panics if done from async context; a resolver
/// dropped on a shard thread would hit exactly that.
fn broker_runtime() -> Option<&'static tokio::runtime::Runtime> {
    static RUNTIME: OnceLock<Option<tokio::runtime::Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("brain-llm-broker")
                .enable_all()
                .build()
            {
                Ok(rt) => Some(rt),
                Err(e) => {
                    // Resolution then fails Unavailable rather than Missing, so
                    // writes are admitted instead of being rejected as if the
                    // tenant had no key.
                    tracing::error!(
                        target: "brain_llm::broker",
                        error = %e,
                        "could not build the broker runtime; credential resolution is disabled",
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// Resolves credentials by asking `brain-edge`.
pub struct BrokerResolver {
    /// Edge base URL, e.g. `http://brain-edge:8080`.
    base_url: String,
    /// Shared token Brain authenticates to the broker with. **Not** a provider
    /// key — it identifies Brain, and the edge swaps it for the tenant's key.
    service_token: String,
    ttl: Duration,
    negative_ttl: Duration,
    negative_gate_ttl: Duration,
    negative_probe_interval: Duration,
    http: reqwest::Client,
    cache: RwLock<HashMap<String, Entry>>,
}

/// Never print the service token.
impl fmt::Debug for BrokerResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrokerResolver")
            .field("base_url", &self.base_url)
            .field("service_token", &"[redacted]")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl BrokerResolver {
    /// Build a resolver against `base_url`, authenticating with
    /// `service_token`.
    #[must_use]
    pub fn new(base_url: impl Into<String>, service_token: impl Into<String>) -> Self {
        Self {
            // Trailing slashes would produce `//llm/...`, which some proxies
            // normalise and some reject; strip once here rather than hoping.
            base_url: base_url.into().trim_end_matches('/').to_string(),
            service_token: service_token.into(),
            ttl: DEFAULT_TTL,
            negative_ttl: DEFAULT_NEGATIVE_TTL,
            negative_gate_ttl: DEFAULT_NEGATIVE_GATE_TTL,
            negative_probe_interval: DEFAULT_NEGATIVE_PROBE_INTERVAL,
            http: reqwest::Client::new(),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Override the positive cache lifetime.
    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Override the negative cache lifetime.
    #[must_use]
    pub fn with_negative_ttl(mut self, ttl: Duration) -> Self {
        self.negative_ttl = ttl;
        self
    }

    /// Override how long the sync ENCODE gate keeps refusing a missing
    /// namespace.
    #[must_use]
    pub fn with_negative_gate_ttl(mut self, ttl: Duration) -> Self {
        self.negative_gate_ttl = ttl;
        self
    }

    /// Override how often the gate admits one write to re-probe.
    #[must_use]
    pub fn with_negative_probe_interval(mut self, interval: Duration) -> Self {
        self.negative_probe_interval = interval;
        self
    }

    /// The per-provider base URL a provider client should be built with.
    fn provider_base(&self, namespace: &str, provider: &str) -> String {
        format!(
            "{}{BROKER_PATH_PREFIX}/{namespace}/{provider}",
            self.base_url
        )
    }

    /// Read a still-fresh cache entry, if any.
    fn cached(&self, namespace: &str) -> Option<Cached> {
        let guard = self.cache.read();
        let entry = guard.get(namespace)?;
        let ttl = match entry.value {
            Cached::Ready { .. } => self.ttl,
            Cached::Missing => self.negative_ttl,
        };
        (entry.fetched_at.elapsed() < ttl).then(|| entry.value.clone())
    }

    fn store(&self, namespace: &str, value: Cached) {
        let now = Instant::now();
        self.cache.write().insert(
            namespace.to_string(),
            Entry {
                value,
                fetched_at: now,
                last_gate_probe: now,
            },
        );
    }

    /// The gate's view of a namespace: should the synchronous ENCODE path
    /// admit this write?
    ///
    /// Deliberately NOT [`Self::cached`]. That one answers `client_for`, which
    /// wants a missing entry to expire quickly so the next derivation
    /// re-fetches; this one wants it to persist so the gate actually fires.
    /// Reading both off one TTL is what made the gate a no-op on sparse
    /// traffic.
    ///
    /// Takes a write lock on the `Missing` path because admitting a probe has
    /// to be recorded — otherwise every concurrent write in the interval
    /// reads "probe due" and they are all admitted, which is the thundering
    /// herd the stamp exists to prevent.
    fn gate_decision(&self, namespace: &str) -> Option<bool> {
        {
            let guard = self.cache.read();
            let entry = guard.get(namespace)?;
            match entry.value {
                // A positive answer still obeys the ordinary TTL: a revoked
                // key must stop admitting writes on the same schedule it
                // stops serving them.
                Cached::Ready { .. } => {
                    return (entry.fetched_at.elapsed() < self.ttl).then_some(true);
                }
                Cached::Missing => {
                    // Older than the gate window: no longer evidence. Unknown,
                    // so admit — same rule as a cold cache.
                    if entry.fetched_at.elapsed() >= self.negative_gate_ttl {
                        return None;
                    }
                    if entry.last_gate_probe.elapsed() < self.negative_probe_interval {
                        return Some(false);
                    }
                }
            }
        }
        // A probe is due. Re-check under the write lock: another thread may
        // have taken it between dropping the read lock and taking this one.
        let mut guard = self.cache.write();
        let entry = guard.get_mut(namespace)?;
        if !matches!(entry.value, Cached::Missing) {
            return None;
        }
        if entry.last_gate_probe.elapsed() < self.negative_probe_interval {
            return Some(false);
        }
        entry.last_gate_probe = Instant::now();
        // Admitted on purpose: this write runs derivation, which re-fetches
        // and either re-stores `Missing` or heals the namespace to `Ready`.
        None
    }

    /// Build the provider client for a resolved namespace.
    ///
    /// Returns `None` for a provider this build does not know, which is a
    /// server/client version skew rather than a tenant error.
    fn build_client(
        &self,
        namespace: &str,
        provider: &str,
        model: &str,
    ) -> Option<Arc<dyn LlmClient>> {
        let base = self.provider_base(namespace, provider);
        // Via `Provider::from_name` rather than matching strings here: the
        // canonical spelling lives in one place, and an unrecognised provider
        // lands on `Unknown` instead of silently falling through a match arm.
        match Provider::from_name(provider) {
            Provider::Anthropic => Some(Arc::new(AnthropicClient::with_endpoint(
                model,
                &self.service_token,
                base,
            )) as Arc<dyn LlmClient>),
            Provider::OpenAi => Some(Arc::new(OpenAIClient::with_endpoint(
                model,
                &self.service_token,
                base,
            )) as Arc<dyn LlmClient>),
            Provider::Unknown => None,
        }
    }

    /// Ask the edge what serves `namespace`.
    ///
    /// The HTTP runs on [`broker_runtime`], not on the caller's executor. See
    /// that function for why.
    async fn fetch(&self, namespace: &str) -> CredentialOutcome {
        let url = format!("{}{BROKER_PATH_PREFIX}/{namespace}/resolve", self.base_url);

        let Some(runtime) = broker_runtime() else {
            // No runtime means no way to ask. Unavailable, not Missing: the
            // tenant may well have a key, we just cannot look it up.
            tracing::warn!(
                target: "brain_llm::broker",
                namespace,
                "no broker runtime; cannot resolve a credential",
            );
            return CredentialOutcome::Unavailable;
        };

        // Owned clones so the task is `'static`. `reqwest::Client` is an Arc
        // inside, so this shares one connection pool rather than building one
        // per resolve.
        let http = self.http.clone();
        let token = self.service_token.clone();
        let fetched = match runtime.spawn(broker_get(http, url, token)).await {
            Ok(outcome) => outcome,
            Err(e) => {
                // The task panicked or was cancelled. Treated like any other
                // failure to reach the edge rather than propagated: a resolver
                // fault must not take the write path down with it.
                tracing::warn!(
                    target: "brain_llm::broker",
                    namespace,
                    error = %e,
                    "broker fetch task failed",
                );
                return CredentialOutcome::Unavailable;
            }
        };

        let body: ResolveBody = match fetched {
            BrokerHttp::Resolved(body) => body,
            BrokerHttp::Missing => {
                // The tenant has registered no key. Terminal for this write,
                // and briefly cached so a batch does not re-ask per memory.
                self.store(namespace, Cached::Missing);
                return CredentialOutcome::Missing;
            }
            BrokerHttp::Unreachable(e) => {
                // Transient: the edge is down or unreachable. NOT cached — a
                // blip must not become a sticky "no key" for this namespace.
                tracing::warn!(
                    target: "brain_llm::broker",
                    namespace,
                    error = %e,
                    "credential broker unreachable",
                );
                return CredentialOutcome::Unavailable;
            }
            BrokerHttp::Status(status) => {
                tracing::warn!(
                    target: "brain_llm::broker",
                    namespace,
                    status,
                    "credential broker returned an unexpected status",
                );
                return CredentialOutcome::Unavailable;
            }
            BrokerHttp::Undecodable(e) => {
                tracing::warn!(
                    target: "brain_llm::broker",
                    namespace,
                    error = %e,
                    "credential broker returned an undecodable body",
                );
                return CredentialOutcome::Unavailable;
            }
        };

        let Some(client) = self.build_client(namespace, &body.provider, &body.model) else {
            // A provider this build does not know. Version skew, not a tenant
            // problem — and not cached, so an upgrade fixes it immediately.
            tracing::warn!(
                target: "brain_llm::broker",
                namespace,
                provider = %body.provider,
                "credential broker named a provider this build cannot serve",
            );
            return CredentialOutcome::Unavailable;
        };

        self.store(
            namespace,
            Cached::Ready {
                provider: body.provider,
                model: body.model,
            },
        );
        CredentialOutcome::Ready(ResolvedCredential {
            client,
            scope: CredentialScope::from_identifier(namespace),
        })
    }
}

impl LlmCredentialResolver for BrokerResolver {
    fn client_for<'a>(
        &'a self,
        namespace: &'a str,
        // The tenant's registered model wins: it is the only model their key
        // can actually pay for, so an extractor's declared preference cannot
        // override it.
        _preferred_model: Option<&'a str>,
    ) -> CredentialFuture<'a> {
        Box::pin(async move {
            match self.cached(namespace) {
                Some(Cached::Ready { provider, model }) => {
                    match self.build_client(namespace, &provider, &model) {
                        Some(client) => CredentialOutcome::Ready(ResolvedCredential {
                            client,
                            scope: CredentialScope::from_identifier(namespace),
                        }),
                        None => CredentialOutcome::Unavailable,
                    }
                }
                Some(Cached::Missing) => CredentialOutcome::Missing,
                None => self.fetch(namespace).await,
            }
        })
    }

    fn can_serve(&self, namespace: &str, _preferred_model: Option<&str>) -> Option<bool> {
        // Cache-only by contract: this is called from the synchronous ENCODE
        // path, which must not block on the network. `None` means "unknown
        // without I/O" and admits the write.
        //
        // `gate_decision`, not `cached`: the gate keeps a missing namespace
        // refused for far longer than `client_for` keeps it cached, and lets
        // one write through per probe interval so the namespace can heal
        // itself without this call doing any I/O.
        self.gate_decision(namespace)
    }

    fn missing_reason(&self, namespace: &str) -> String {
        format!(
            "namespace {namespace} has no LLM provider key registered; \
             add one before writing (BYOK)"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver() -> BrokerResolver {
        BrokerResolver::new("http://edge:8080", "broker-token")
    }

    #[test]
    fn a_trailing_slash_does_not_produce_a_double_slash() {
        let r = BrokerResolver::new("http://edge:8080/", "t");
        assert_eq!(
            r.provider_base("acme", crate::router::PROVIDER_OPENAI),
            "http://edge:8080/llm/ns/acme/openai"
        );
    }

    #[test]
    fn the_provider_base_matches_what_the_clients_append() {
        // The provider clients append `/v1/chat/completions`; the result must
        // be the edge route. If either side moves, this is the tripwire.
        let r = resolver();
        assert_eq!(
            format!(
                "{}/v1/chat/completions",
                r.provider_base("acme", crate::router::PROVIDER_OPENAI)
            ),
            "http://edge:8080/llm/ns/acme/openai/v1/chat/completions"
        );
        assert_eq!(
            format!(
                "{}/v1/messages",
                r.provider_base("acme", crate::router::PROVIDER_ANTHROPIC)
            ),
            "http://edge:8080/llm/ns/acme/anthropic/v1/messages"
        );
    }

    #[test]
    fn an_unknown_provider_builds_no_client() {
        // Version skew, not a tenant error.
        assert!(resolver().build_client("acme", "cohere", "m").is_none());
    }

    /// The defect this gate was reported for: on sparse traffic every write
    /// was admitted, so a keyless namespace degraded silently forever.
    #[test]
    fn the_gate_keeps_refusing_long_after_the_client_cache_has_expired() {
        let r = resolver()
            .with_negative_ttl(Duration::from_millis(1))
            .with_negative_gate_ttl(Duration::from_secs(300))
            .with_negative_probe_interval(Duration::from_secs(300));
        r.store("acme", Cached::Missing);
        std::thread::sleep(Duration::from_millis(5));

        // `client_for`'s view has expired — it will re-fetch, which is what
        // lets a newly registered key be noticed.
        assert!(r.cached("acme").is_none());
        // The gate's has not. Before this split, both read one TTL and the
        // gate answered `None` here, admitting the write.
        assert_eq!(r.can_serve("acme", None), Some(false));
    }

    /// The escape hatch. Without it a long gate window is self-sustaining:
    /// refused writes run no derivation, so nothing ever fetches the `Ready`
    /// that would clear the entry.
    #[test]
    fn the_gate_admits_one_write_per_probe_interval_so_a_namespace_can_heal() {
        let r = resolver()
            .with_negative_gate_ttl(Duration::from_secs(300))
            .with_negative_probe_interval(Duration::from_millis(20));
        r.store("acme", Cached::Missing);

        // Inside the interval: refused, and repeatedly so — one probe does not
        // open the gate for everyone behind it.
        assert_eq!(r.can_serve("acme", None), Some(false));
        assert_eq!(r.can_serve("acme", None), Some(false));

        std::thread::sleep(Duration::from_millis(25));
        // Probe due: exactly one write is admitted...
        assert_eq!(r.can_serve("acme", None), None);
        // ...and the next is refused again, because the probe was recorded.
        assert_eq!(r.can_serve("acme", None), Some(false));
    }

    /// Healing actually sticks: once a key is registered the probe write's
    /// derivation stores `Ready`, and the gate stops refusing.
    #[test]
    fn a_namespace_that_gains_a_key_stops_being_refused() {
        let r = resolver().with_negative_probe_interval(Duration::from_millis(1));
        r.store("acme", Cached::Missing);
        assert_eq!(r.can_serve("acme", None), Some(false));

        r.store(
            "acme",
            Cached::Ready {
                provider: "anthropic".into(),
                model: "claude-opus-5-5".into(),
            },
        );
        assert_eq!(r.can_serve("acme", None), Some(true));
    }

    /// A stale negative is not evidence. Past the gate window the answer goes
    /// back to "unknown", which admits — same rule as a cold cache.
    #[test]
    fn a_negative_older_than_the_gate_window_is_unknown_again() {
        let r = resolver().with_negative_gate_ttl(Duration::from_millis(5));
        r.store("acme", Cached::Missing);
        assert_eq!(r.can_serve("acme", None), Some(false));
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(r.can_serve("acme", None), None);
    }

    /// A revoked key must stop admitting writes on the ordinary TTL — the
    /// gate's longer window applies to negatives only.
    #[test]
    fn a_positive_still_expires_on_the_ordinary_ttl() {
        let r = resolver().with_ttl(Duration::from_millis(5));
        r.store(
            "acme",
            Cached::Ready {
                provider: "anthropic".into(),
                model: "m".into(),
            },
        );
        assert_eq!(r.can_serve("acme", None), Some(true));
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(r.can_serve("acme", None), None);
    }

    #[test]
    fn a_cold_cache_is_unknown_rather_than_missing() {
        // The sync ENCODE probe must admit the write on an unknown, not reject
        // it: a cold cache is not evidence that a tenant has no key.
        assert_eq!(resolver().can_serve("acme", None), None);
    }

    #[test]
    fn a_cached_positive_and_negative_answer_the_sync_probe() {
        let r = resolver();
        r.store(
            "acme",
            Cached::Ready {
                provider: "openai".into(),
                model: "gpt-4o-mini".into(),
            },
        );
        r.store("globex", Cached::Missing);
        assert_eq!(r.can_serve("acme", None), Some(true));
        assert_eq!(r.can_serve("globex", None), Some(false));
        assert_eq!(r.can_serve("other", None), None);
    }

    #[test]
    fn a_negative_entry_expires_sooner_than_a_positive_one() {
        // A tenant who just registered a key must not keep failing for the
        // full positive TTL.
        assert!(
            DEFAULT_NEGATIVE_TTL < DEFAULT_TTL,
            "negative caching must be the shorter of the two"
        );
        // The gate holds a negative far longer than `client_for` does, so the
        // bound on a tenant's recovery is no longer the negative TTL — it is
        // the probe interval, which still has to beat the positive TTL or
        // registering a key would appear not to work.
        assert!(
            DEFAULT_NEGATIVE_PROBE_INTERVAL < DEFAULT_TTL,
            "a tenant who registers a key must get a probe through well \
             before a positive entry would have expired anyway"
        );

        let r = resolver()
            .with_ttl(Duration::from_secs(60))
            .with_negative_ttl(Duration::ZERO);
        r.store("globex", Cached::Missing);
        assert!(
            r.cached("globex").is_none(),
            "a zero negative TTL expires immediately, so `client_for` re-asks"
        );
        // `can_serve` deliberately does NOT follow it any more: reading both
        // off one TTL is what made the gate a no-op between sparse writes.
        // Recovery is the probe interval's job, covered by
        // `the_gate_admits_one_write_per_probe_interval_so_a_namespace_can_heal`.
        assert_eq!(
            r.can_serve("globex", None),
            Some(false),
            "the gate keeps refusing regardless of the client-side TTL"
        );

        r.store(
            "acme",
            Cached::Ready {
                provider: "openai".into(),
                model: "m".into(),
            },
        );
        assert_eq!(
            r.can_serve("acme", None),
            Some(true),
            "positive still fresh"
        );
    }

    #[test]
    fn each_namespace_gets_its_own_credential_scope() {
        // The scope enters the LLM cache key, so two tenants must not share
        // cached completions.
        assert_ne!(
            CredentialScope::from_identifier("acme"),
            CredentialScope::from_identifier("globex")
        );
    }

    /// Drive a future to completion with no reactor of any kind.
    ///
    /// As far as Tokio is concerned this is the shape of a glommio shard
    /// executor: a thread polling futures with nothing registered to service
    /// I/O. Hand-rolled rather than pulled from a crate so the test cannot
    /// accidentally acquire the very runtime it is meant to do without.
    fn block_on_without_reactor<F: std::future::Future>(fut: F) -> F::Output {
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};

        struct Unparker(std::thread::Thread);
        impl Wake for Unparker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }

        let waker = Waker::from(Arc::new(Unparker(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(fut);
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => std::thread::park(),
            }
        }
    }

    #[test]
    fn resolving_without_a_tokio_reactor_does_not_panic() {
        // The regression this file existed without. Every other broker test is
        // `#[tokio::test]`, which hands the resolver a reactor that production
        // does NOT have: `client_for` is awaited from the LLM extractor tier on
        // a glommio shard executor. So the suite stayed green while broker mode
        // panicked the shard and aborted the process on the first uncached
        // write (#34):
        //
        //   thread 'brain-shard-0-1' panicked at hyper-util .../connect/dns.rs:
        //   there is no reactor running, must be called from the context of a
        //   Tokio 1.x runtime
        //
        // Port 1 refuses immediately, so this exercises the connect path — the
        // one that panicked — without needing a server.
        let r = BrokerResolver::new("http://127.0.0.1:1", "t");
        let outcome = block_on_without_reactor(r.client_for("acme", None));

        assert!(
            outcome.is_retryable(),
            "an unreachable broker is a blip, not a verdict on the tenant"
        );
        assert!(
            !outcome.is_missing(),
            "and must never read as 'this tenant has no key'"
        );
        assert_eq!(
            r.can_serve("acme", None),
            None,
            "a failed reach must not be cached either way"
        );
    }

    #[tokio::test]
    async fn an_unreachable_broker_is_unavailable_not_missing() {
        // The distinction the whole two-variant split exists for: a down edge
        // must not look like a permanently unconfigured tenant.
        let r = BrokerResolver::new("http://127.0.0.1:1", "t");
        let outcome = r.client_for("acme", None).await;
        assert!(outcome.is_retryable(), "a blip must retry");
        assert!(!outcome.is_missing(), "and must not reject the tenant");
        // And it must NOT be cached, or one blip would stick.
        assert_eq!(r.can_serve("acme", None), None);
    }

    #[test]
    fn debug_does_not_leak_the_service_token() {
        let d = format!("{:?}", BrokerResolver::new("http://edge", "super-secret"));
        assert!(!d.contains("super-secret"), "{d}");
        assert!(d.contains("[redacted]"), "{d}");
    }
}
