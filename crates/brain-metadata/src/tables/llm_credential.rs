//! Tenant LLM provider credentials.
//!
//! [`LLM_CREDENTIALS_TABLE`] — `(namespace, space) -> LlmCredentialRow`.
//!
//! Brain's extraction and HyPE stages call an LLM, and under bring-your-own-key
//! that call bills to the tenant's own provider account. This table is where a
//! tenant's key lives, sealed, so that a deployment can run those stages
//! without an operator ever configuring a key of their own.
//!
//! `space` is the scope the credential was registered AT: [`SPACE_BASE`] for
//! one key covering the whole namespace, a real space to give one team or app
//! its own. Resolution walks narrowest-first, so a space with no key of its
//! own uses the namespace's. This matches how schema declarations resolve, so
//! a tenant who has learned one scoping rule already knows this one.
//!
//! [`SPACE_BASE`]: crate::tables::scope::SPACE_BASE

use brain_crypto::SealedSecret;
use redb::TableDefinition;

use crate::impl_redb_rkyv_value;

pub const LLM_CREDENTIALS_TABLE: TableDefinition<'static, (&str, [u8; 16]), LlmCredentialRow> =
    TableDefinition::new("llm_credentials");

/// One tenant's registered provider key.
///
/// Everything here except [`Self::sealed`] is safe to show an operator: the
/// row answers "which key is installed" without the key being readable again.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone, PartialEq, Eq)]
pub struct LlmCredentialRow {
    /// Provider this key authenticates against (`"openai"`, `"anthropic"`).
    pub provider: String,
    /// Model the tenant registered the key for. Extraction and HyPE run on
    /// this model, so the work is always billed to something the key can
    /// actually pay for.
    pub model: String,
    /// The provider key, sealed against this row's namespace and provider.
    pub sealed: SealedSecret,
    /// Display-safe tail of the key, so a dashboard can show which key is
    /// installed. Never reconstructable into the original.
    pub fingerprint: String,
    /// When the credential was registered.
    pub created_at_unix_nanos: u64,
}

/// Prints shape only, so a credential row cannot put tenant key material into
/// a log line.
impl std::fmt::Debug for LlmCredentialRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmCredentialRow")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("fingerprint", &self.fingerprint)
            .field("created_at_unix_nanos", &self.created_at_unix_nanos)
            .finish_non_exhaustive()
    }
}

impl_redb_rkyv_value!(LlmCredentialRow, "brain_metadata::LlmCredentialRow");
