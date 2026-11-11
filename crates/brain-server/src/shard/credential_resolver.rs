//! Resolving a namespace's LLM credential from Brain's own credential store.
//!
//! This is the only way a namespace gets a serving credential. A deployment
//! holds no key of its own: a tenant registers theirs over the wire, Brain
//! seals it, and the model derivation stages resolve it here and bill the
//! tenant's provider account.
//!
//! # A namespace with no key is a degraded namespace, not a broken server
//!
//! Until a tenant registers a key, writes still land and the non-model
//! derivation stages still run. The model stages report
//! [`CredentialOutcome::Missing`] per write, which is terminal and carries a
//! reason the tenant can act on. A storage fault reports
//! [`CredentialOutcome::Unavailable`] instead, so a transient problem is
//! retried rather than recorded as "this tenant has no key".
//!
//! # Rotation needs no invalidation protocol
//!
//! Provider clients are cached so that resolving does not rebuild an HTTP
//! client per call, and the cache entry is keyed by the credential's
//! registration timestamp alongside its namespace and model. Registering a
//! new key moves that timestamp, so the old entry becomes unreachable the
//! moment the store changes and every shard picks up the new key on its next
//! resolve. There is no generation counter to keep in step and no window in
//! which a revoked key keeps working.

use std::collections::HashMap;
use std::sync::Arc;

use brain_llm::{
    AnthropicClient, CredentialFuture, CredentialOutcome, CredentialScope, LlmClient,
    LlmCredentialResolver, OpenAIClient, ResolvedCredential,
};
use brain_metadata::llm_credential::LlmCredentialAccess;
use brain_metadata::tables::scope::SPACE_BASE;
use parking_lot::Mutex;

use super::llm_bridge::bridge;

/// Identifies a cached provider client. The timestamp is what makes a
/// rotation self-invalidating: a replaced key carries a new one, so the
/// previous entry can never be selected again.
type ClientKey = (String, String, u64);

/// Resolves each namespace's credential from the shared credential store.
pub struct StoreCredentialResolver {
    credentials: Arc<LlmCredentialAccess>,
    clients: Mutex<HashMap<ClientKey, Arc<dyn LlmClient>>>,
}

impl std::fmt::Debug for StoreCredentialResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreCredentialResolver")
            .field("cached_clients", &self.clients.lock().len())
            .finish_non_exhaustive()
    }
}

/// What a store lookup produced, before a client is built for it.
enum Lookup {
    /// A credential is installed; these are the parts needed to serve it.
    Found {
        provider: String,
        model: String,
        created_at_unix_nanos: u64,
        api_key: String,
    },
    /// The namespace has registered nothing. Terminal.
    Missing,
    /// The store could not answer, or the stored key could not be opened
    /// because the sealing key is no longer loaded. Both are operator
    /// problems that may resolve, so the caller retries.
    Unavailable,
}

impl StoreCredentialResolver {
    #[must_use]
    pub fn new(credentials: Arc<LlmCredentialAccess>) -> Self {
        Self {
            credentials,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Read and open the credential for `namespace`.
    ///
    /// Resolution is at the namespace base. Space-scoped credentials are
    /// storable but are not yet reachable from derivation, which does not
    /// carry the space this far; the registration path refuses that scope
    /// rather than accepting a key that would be ignored here.
    fn lookup(&self, namespace: &str) -> Lookup {
        match self.credentials.open(namespace, SPACE_BASE) {
            Ok(Some((status, key))) => match String::from_utf8(key.to_vec()) {
                Ok(api_key) => Lookup::Found {
                    provider: status.provider,
                    model: status.model,
                    created_at_unix_nanos: status.created_at_unix_nanos,
                    api_key,
                },
                // A sealed key that is not valid UTF-8 was never a provider
                // key; serving it would send bytes no provider accepts.
                Err(_) => Lookup::Unavailable,
            },
            Ok(None) => Lookup::Missing,
            Err(e) => {
                tracing::error!(
                    target: "brain_server::credential_resolver",
                    namespace,
                    error = %e,
                    "could not open this namespace's stored LLM credential",
                );
                Lookup::Unavailable
            }
        }
    }

    /// The client for an opened credential, built once and reused.
    fn client_for_lookup(
        &self,
        namespace: &str,
        provider: &str,
        model: &str,
        created_at_unix_nanos: u64,
        api_key: &str,
    ) -> Option<Arc<dyn LlmClient>> {
        let key: ClientKey = (
            namespace.to_string(),
            model.to_string(),
            created_at_unix_nanos,
        );
        if let Some(found) = self.clients.lock().get(&key) {
            return Some(found.clone());
        }

        // The provider is stored with the credential rather than guessed from
        // the model id, so a tenant on a provider-specific model name is
        // never routed to the wrong wire dialect.
        let built: Arc<dyn LlmClient> = match provider {
            "anthropic" => Arc::new(AnthropicClient::with_key(
                model.to_string(),
                api_key.to_string(),
            )?),
            "openai" => Arc::new(OpenAIClient::with_key(
                model.to_string(),
                api_key.to_string(),
            )?),
            other => {
                tracing::error!(
                    target: "brain_server::credential_resolver",
                    namespace,
                    provider = other,
                    "stored credential names a provider this build cannot serve",
                );
                return None;
            }
        };

        let bridged = bridge(built);
        self.clients.lock().insert(key, bridged.clone());
        Some(bridged)
    }
}

impl LlmCredentialResolver for StoreCredentialResolver {
    fn client_for<'a>(
        &'a self,
        namespace: &'a str,
        _preferred_model: Option<&'a str>,
    ) -> CredentialFuture<'a> {
        // The registered model wins over the caller's preference: it is the
        // only model the tenant's key is known to pay for. An extractor
        // asking for something else would bill a call the key may not cover.
        Box::pin(async move {
            match self.lookup(namespace) {
                Lookup::Found {
                    provider,
                    model,
                    created_at_unix_nanos,
                    api_key,
                } => match self.client_for_lookup(
                    namespace,
                    &provider,
                    &model,
                    created_at_unix_nanos,
                    &api_key,
                ) {
                    Some(client) => CredentialOutcome::Ready(ResolvedCredential {
                        client,
                        scope: CredentialScope::from_identifier(namespace),
                    }),
                    // The row is present but unservable. Terminal: retrying
                    // cannot make this build speak an unknown provider.
                    None => CredentialOutcome::Missing,
                },
                Lookup::Missing => CredentialOutcome::Missing,
                Lookup::Unavailable => CredentialOutcome::Unavailable,
            }
        })
    }

    /// Answers from the store directly rather than from a cache.
    ///
    /// The store is local, so the answer is definitive and a write for a
    /// namespace with no credential is refused at admission with a reason
    /// rather than admitted and failed later in a derivation stage. Only a
    /// storage fault leaves the question open.
    fn can_serve(&self, namespace: &str, _preferred_model: Option<&str>) -> Option<bool> {
        match self.lookup(namespace) {
            Lookup::Found { .. } => Some(true),
            Lookup::Missing => Some(false),
            // A storage fault is not evidence of a missing key, so this stays
            // "unknown" and the caller admits the write.
            Lookup::Unavailable => None,
        }
    }

    fn missing_reason(&self, namespace: &str) -> String {
        format!(
            "namespace `{namespace}` has no LLM provider credential: register one with \
             LLM_CREDENTIAL_PUT before extraction and HyPE can run"
        )
    }
}
