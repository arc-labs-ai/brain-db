//! Per-namespace LLM credential resolution (BYOK).
//!
//! Brain's LLM credential used to be a single process-wide key bound at shard
//! startup: one `ModelRouter`, one provider, chosen once from `[llm] api_key`.
//! That is the right shape for a single-tenant deployment and the wrong shape
//! for a tenant who brings their own OpenAI/Anthropic key.
//!
//! This module introduces the seam. A worker that needs an LLM asks a
//! [`LlmCredentialResolver`] for the client belonging to a **namespace**,
//! rather than reaching for the one global router.
//!
//! ## Where the implementations live
//!
//! The production resolver is in `brain-server`, over Brain's own credential
//! store: each namespace registers a provider key and the model-backed stages
//! bill to it.
//!
//! The two resolvers in this module serve one client to **every** namespace,
//! which is not a tenancy model — it is what a test needs, and what an
//! embedder with a single hand-built client needs. Neither is constructed by
//! a production path.
//!
//! ## `Missing` is not `Unavailable`
//!
//! [`CredentialOutcome`] keeps "this namespace has no key" separate from "I
//! could not find out right now". Conflating them turns a transient storage
//! fault into a permanent-looking rejection and fails writes that should have
//! been retried. Callers must branch on both.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::client::LlmClient;
use crate::router::ModelRouter;

/// Identity of the credential a completion was billed to, as an opaque 64-bit
/// value.
///
/// This exists for one reason: the LLM response cache is keyed by
/// `(input_hash, extractor_id, extractor_version, model)` and that tuple has no
/// tenant component. Under BYOK, sharing a cache row across credentials means
/// one tenant's key pays for a completion another tenant then gets free. The
/// scope closes that hole by entering the cache key.
///
/// **Derive it from the grant or tenant id, never from the provider key
/// itself.** The scope is a cache-key component: it is hashed into persisted
/// rows and shows up in diagnostics, and neither is a place for key material.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CredentialScope(u64);

impl CredentialScope {
    /// The scope a resolver uses when it serves one credential to every
    /// namespace — the test doubles in this module. No production path
    /// produces it: a real credential always derives its scope from the
    /// namespace that registered it, which is what keeps one tenant's key
    /// from paying for a cache row another tenant reads.
    pub const HOUSE: Self = Self(0);

    /// Derive a scope from a stable credential identifier — a grant id or
    /// tenant id. Never pass a provider key.
    #[must_use]
    pub fn from_identifier(id: &str) -> Self {
        let h = blake3::hash(id.as_bytes());
        let b = h.as_bytes();
        // Reserve 0 for HOUSE so a derived scope can never collide with it.
        let raw = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
        Self(if raw == 0 { 1 } else { raw })
    }

    /// The raw value, for use as a cache-key component.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }

    /// True for the deployment-wide house key.
    #[must_use]
    pub fn is_house(self) -> bool {
        self.0 == Self::HOUSE.0
    }
}

/// Redacted on purpose: a scope is not secret, but printing a bare integer
/// invites someone to treat it as one and log the thing it was derived from.
impl fmt::Debug for CredentialScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_house() {
            write!(f, "CredentialScope(house)")
        } else {
            write!(f, "CredentialScope({:016x})", self.0)
        }
    }
}

/// A resolved credential: the client to call, and the scope it bills to.
#[derive(Clone)]
pub struct ResolvedCredential {
    pub client: Arc<dyn LlmClient>,
    pub scope: CredentialScope,
}

impl fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("model", &self.client.model())
            .field("scope", &self.scope)
            .finish()
    }
}

/// Result of asking for a namespace's credential.
#[derive(Clone, Debug)]
pub enum CredentialOutcome {
    /// Use this client; bill it to this scope.
    Ready(ResolvedCredential),
    /// The namespace has no usable credential. **Terminal** — reject the write
    /// rather than retrying, and surface it as a typed error the tenant can act
    /// on ("add a provider key"), not a 500.
    Missing,
    /// The credential could not be determined right now — the credential
    /// store could not be read. **Retry.** Treating this as [`Self::Missing`]
    /// rejects writes for a tenant whose key is perfectly good.
    Unavailable,
}

impl CredentialOutcome {
    /// The client, if one was resolved.
    #[must_use]
    pub fn ready(self) -> Option<ResolvedCredential> {
        match self {
            Self::Ready(c) => Some(c),
            Self::Missing | Self::Unavailable => None,
        }
    }

    /// True only for the terminal "no credential" case. Deliberately false for
    /// [`Self::Unavailable`] so that `if outcome.is_missing() { reject }` is
    /// correct as written.
    #[must_use]
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    /// True when the caller should retry rather than reject.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

/// Future returned by [`LlmCredentialResolver::client_for`].
pub type CredentialFuture<'a> = Pin<Box<dyn Future<Output = CredentialOutcome> + Send + 'a>>;

/// Resolves the LLM credential for a namespace.
///
/// Object-safe (no `async fn` in the trait) so implementations can co-exist
/// behind `Arc<dyn LlmCredentialResolver>`, matching [`LlmClient`]'s shape.
pub trait LlmCredentialResolver: Send + Sync {
    /// Resolve the client for `namespace`. May perform I/O; implementations
    /// that do should cache with a TTL and collapse concurrent misses for the
    /// same namespace into one upstream fetch.
    ///
    /// `preferred_model` is the model the caller declared — for the LLM
    /// extractor tier, the `model:` field on the extractor definition. It is a
    /// request, not a command: a resolver may serve a different model when the
    /// namespace's credential does not cover the requested one.
    /// [`RouterResolver`] honours it exactly; a store-backed resolver serves
    /// whatever model the tenant registered a key for, because that is the
    /// only model their key can actually pay for.
    fn client_for<'a>(
        &'a self,
        namespace: &'a str,
        preferred_model: Option<&'a str>,
    ) -> CredentialFuture<'a>;

    /// Cache-only, non-blocking probe: can this resolver serve `namespace`
    /// (optionally for `preferred_model`) without doing I/O?
    ///
    /// Two callers depend on it, for different reasons:
    ///
    /// - The **synchronous ENCODE path**, which must decide whether to admit a
    ///   write without waiting on a network round trip.
    /// - **Extractor materialization** at shard startup, which reports "no
    ///   llm clients configured" or "no client configured for model X" there
    ///   rather than letting every write fail later.
    ///
    /// `Some(true)` / `Some(false)` answer from local knowledge. `None` means
    /// "unknown" — callers must **admit** on `None`, never reject, because a
    /// resolver that cannot read its own store has learned nothing about
    /// whether the namespace holds a key.
    fn can_serve(&self, namespace: &str, preferred_model: Option<&str>) -> Option<bool>;

    /// Human-readable reason a namespace has no credential, for the audit row
    /// and the operator-facing error. Implementations should not include key
    /// material or anything derived from it.
    fn missing_reason(&self, namespace: &str) -> String {
        format!("no LLM credential configured for namespace {namespace}")
    }
}

/// Serves one [`ModelRouter`] to every namespace.
///
/// A test double, not a tenancy model: every namespace resolves the same
/// credential and bills to the shared [`CredentialScope::HOUSE`]. Its value
/// over [`FixedResolver`] is that it routes by model id, so a test can still
/// exercise "this model has no configured provider".
pub struct RouterResolver {
    router: Arc<ModelRouter>,
    model: String,
}

impl RouterResolver {
    #[must_use]
    pub fn new(router: Arc<ModelRouter>, model: impl Into<String>) -> Self {
        Self {
            router,
            model: model.into(),
        }
    }

    /// The model id every namespace resolves to.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

impl fmt::Debug for RouterResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouterResolver")
            .field("model", &self.model)
            .field("configured", &self.router.has_any_provider())
            .finish()
    }
}

impl LlmCredentialResolver for RouterResolver {
    fn client_for<'a>(
        &'a self,
        _namespace: &'a str,
        preferred_model: Option<&'a str>,
    ) -> CredentialFuture<'a> {
        Box::pin(async move {
            // The declared model wins when given: this is exactly what
            // `materialize_llm_extractor` did before BYOK — route the
            // extractor's own `model:` field through the one configured router.
            let model = preferred_model.unwrap_or(&self.model);
            match self.router.resolve(model) {
                Some(client) => CredentialOutcome::Ready(ResolvedCredential {
                    client,
                    scope: CredentialScope::HOUSE,
                }),
                // A static deployment with no key is a configuration error, not
                // a transient one: it will not fix itself on retry.
                None => CredentialOutcome::Missing,
            }
        })
    }

    fn can_serve(&self, _namespace: &str, preferred_model: Option<&str>) -> Option<bool> {
        // Fully known locally — never `None`, so neither the sync write path
        // nor startup materialization has to guess for a static deployment.
        Some(
            self.router
                .resolve(preferred_model.unwrap_or(&self.model))
                .is_some(),
        )
    }

    fn missing_reason(&self, _namespace: &str) -> String {
        "no llm clients configured for this resolver".to_string()
    }
}

/// A resolver that serves one client to every namespace, bypassing model
/// routing entirely.
///
/// [`RouterResolver`] routes through a [`ModelRouter`], so the client it
/// returns depends on the model id matching a known provider prefix. That is
/// correct for a real deployment and awkward for tests and for embedding a
/// single hand-built client, where the caller already knows exactly which
/// client they want.
///
/// Every namespace shares [`CredentialScope::HOUSE`] — this is a single-key
/// resolver, so there is nothing to isolate.
pub struct FixedResolver {
    client: Arc<dyn LlmClient>,
}

impl FixedResolver {
    #[must_use]
    pub fn new(client: Arc<dyn LlmClient>) -> Self {
        Self { client }
    }
}

impl fmt::Debug for FixedResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FixedResolver")
            .field("model", &self.client.model())
            .finish()
    }
}

impl LlmCredentialResolver for FixedResolver {
    fn client_for<'a>(
        &'a self,
        _namespace: &'a str,
        _preferred_model: Option<&'a str>,
    ) -> CredentialFuture<'a> {
        let client = self.client.clone();
        Box::pin(async move {
            CredentialOutcome::Ready(ResolvedCredential {
                client,
                scope: CredentialScope::HOUSE,
            })
        })
    }

    fn can_serve(&self, _namespace: &str, _preferred_model: Option<&str>) -> Option<bool> {
        Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AnthropicClient;

    fn configured_router() -> Arc<ModelRouter> {
        let client: Arc<dyn LlmClient> = Arc::new(AnthropicClient::with_endpoint(
            "claude-haiku-4-5",
            "test-key",
            "http://localhost",
        ));
        Arc::new(ModelRouter::new().with_anthropic(client))
    }

    #[test]
    fn house_scope_is_zero_and_reports_house() {
        assert_eq!(CredentialScope::HOUSE.get(), 0);
        assert!(CredentialScope::HOUSE.is_house());
    }

    #[test]
    fn derived_scopes_are_stable_and_distinct() {
        let a = CredentialScope::from_identifier("tenant-a");
        let b = CredentialScope::from_identifier("tenant-b");
        assert_eq!(a, CredentialScope::from_identifier("tenant-a"));
        assert_ne!(a, b);
    }

    #[test]
    fn derived_scope_never_collides_with_house() {
        // HOUSE is reserved; a derived scope must never alias it, or one
        // tenant's rows would merge into the shared house cache.
        for id in ["", "tenant", "0", "house"] {
            assert!(!CredentialScope::from_identifier(id).is_house());
        }
    }

    #[test]
    fn scope_debug_does_not_print_bare_house_integer() {
        assert_eq!(
            format!("{:?}", CredentialScope::HOUSE),
            "CredentialScope(house)"
        );
    }

    #[test]
    fn missing_is_terminal_and_unavailable_is_retryable() {
        // The whole point of the two-variant split: an unreadable store must
        // not read as "this tenant has no key".
        assert!(CredentialOutcome::Missing.is_missing());
        assert!(!CredentialOutcome::Missing.is_retryable());
        assert!(CredentialOutcome::Unavailable.is_retryable());
        assert!(!CredentialOutcome::Unavailable.is_missing());
    }

    #[tokio::test]
    async fn static_resolver_serves_every_namespace_from_one_key() {
        let r = RouterResolver::new(configured_router(), "claude-haiku-4-5");
        let a = r
            .client_for("tenant-a", None)
            .await
            .ready()
            .expect("configured");
        let b = r
            .client_for("tenant-b", None)
            .await
            .ready()
            .expect("configured");
        assert_eq!(a.scope, b.scope);
        assert!(
            a.scope.is_house(),
            "static deployments share the house scope"
        );
    }

    #[tokio::test]
    async fn static_resolver_without_key_is_missing_not_unavailable() {
        let r = RouterResolver::new(Arc::new(ModelRouter::new()), "claude-haiku-4-5");
        let outcome = r.client_for("tenant-a", None).await;
        assert!(outcome.is_missing());
        assert!(
            !outcome.is_retryable(),
            "a missing config will not self-heal"
        );
        assert_eq!(r.can_serve("tenant-a", None), Some(false));
    }

    #[test]
    fn static_resolver_answers_the_sync_probe_without_io() {
        let r = RouterResolver::new(configured_router(), "claude-haiku-4-5");
        assert_eq!(r.can_serve("anything", None), Some(true));
    }

    #[tokio::test]
    async fn static_resolver_honours_the_declared_model() {
        // `materialize_llm_extractor` passes the extractor's own `model:`
        // field as `preferred_model`, and a router-backed resolver must route
        // on that rather than on its own default.
        let r = RouterResolver::new(configured_router(), "gpt-4o-mini");
        let got = r
            .client_for("tenant-a", Some("claude-haiku-4-5"))
            .await
            .ready()
            .expect("declared model routes to the configured anthropic client");
        assert_eq!(got.client.model(), "claude-haiku-4-5");
    }

    #[tokio::test]
    async fn static_resolver_declared_model_without_a_client_is_missing() {
        // Anthropic configured, an OpenAI model declared: no silent fallback
        // to the wrong provider.
        let r = RouterResolver::new(configured_router(), "claude-haiku-4-5");
        assert!(r
            .client_for("tenant-a", Some("gpt-4o-mini"))
            .await
            .is_missing());
    }

    #[test]
    fn missing_reason_names_the_config_keys_an_operator_sets() {
        let r = RouterResolver::new(Arc::new(ModelRouter::new()), "gpt-4o-mini");
        let reason = r.missing_reason("tenant-a");
        assert!(reason.contains("no llm clients configured"), "{reason}");
    }

    #[test]
    fn static_resolver_model_must_match_a_configured_provider() {
        // Anthropic client configured, OpenAI model requested: the router has
        // no slot for it, so this is a config error rather than a silent
        // fallback to the wrong provider.
        let r = RouterResolver::new(configured_router(), "gpt-4o-mini");
        assert_eq!(r.can_serve("tenant-a", None), Some(false));
    }
}
