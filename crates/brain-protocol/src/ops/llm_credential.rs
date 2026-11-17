//! LLM_CREDENTIAL_PUT / GET / DELETE / VERIFY — a tenant's own provider key.
//!
//! Brain's extraction and HyPE stages call an LLM, and the call bills to the
//! key registered here. A deployment holds no key of its own: until a tenant
//! registers one, writes still land and the non-model derivation stages still
//! run, and the model stages report that the namespace has no credential.
//!
//! # The asymmetry is the contract
//!
//! A plaintext key goes **in** exactly once, on [`LlmCredentialPutRequest`].
//! Nothing that comes back out can reconstruct it: a read answers with the
//! provider, the model and a display fingerprint, because once sealed the key
//! is only ever opened to sign an outbound provider call.
//!
//! # Where a credential lands
//!
//! A credential belongs to a namespace, and there is no narrower scope to
//! choose: derivation resolves the key for the namespace that owns the memory
//! being processed, so a key installed against a single space could never be
//! the one a write consults. These requests therefore carry no scope selector
//! — one was offered briefly and could only ever be refused, which left a
//! field whose single other value was a trap.

use crate::envelope::request::WireUuid;
use crate::ops::memory::ActAs;

/// What a read reports about an installed credential.
///
/// Carries no key material. The fingerprint is a display-safe tail, enough to
/// tell which key is installed and too little to narrow a search for the rest.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmCredentialView {
    /// `"openai"` or `"anthropic"`.
    pub provider: String,
    /// The model extraction and HyPE run on, so the work is always billed to
    /// something this key can pay for.
    pub model: String,
    /// Display-safe tail of the key (e.g. `…4f2a`).
    pub fingerprint: String,
    pub created_at_unix_nanos: u64,
    /// `true` when a space-scoped read resolved the namespace's key rather
    /// than one of its own. A caller needs this before removing a key:
    /// deleting at the space would do nothing, and deleting at the namespace
    /// would affect every sibling space.
    pub inherited: bool,
}

// ============================================================
// LLM_CREDENTIAL_PUT
// ============================================================

/// `LLM_CREDENTIAL_PUT_REQ`. Registers or replaces the key at `scope`.
///
/// The only wire shape that carries a plaintext provider key. Brain seals it
/// on arrival, bound to the namespace and provider, and the plaintext is not
/// retained.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmCredentialPutRequest {
    /// `"openai"` or `"anthropic"`.
    pub provider: String,
    /// Model this key should serve (e.g. `gpt-4.1-mini`, `claude-haiku-4-5`).
    pub model: String,
    /// The provider key. Sealed on arrival and never returned.
    pub api_key: String,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
    /// Optional effective identity, on behalf of the authenticated
    /// connection principal. See [`ActAs`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

/// Redacting `Debug`, written by hand rather than derived.
///
/// A derived `Debug` on this type is one tracing call away from putting a live
/// provider key in a log file, and request logging is exactly the kind of
/// thing added later by someone who has not read this file.
impl std::fmt::Debug for LlmCredentialPutRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmCredentialPutRequest")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("api_key", &"[redacted]")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmCredentialPutResponse {
    pub provider: String,
    pub model: String,
    pub fingerprint: String,
    pub created_at_unix_nanos: u64,
    /// `true` when this displaced a key already registered at the same scope,
    /// so a caller can tell a first registration from a rotation.
    pub replaced: bool,
}

// ============================================================
// LLM_CREDENTIAL_GET
// ============================================================

/// `LLM_CREDENTIAL_GET_REQ`. Reports the credential the caller resolves from
/// `scope`, walking narrowest-first.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmCredentialGetRequest {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmCredentialGetResponse {
    /// `None` when the namespace has registered nothing. The model
    /// derivation stages are degraded in that state; the rest of Brain is
    /// unaffected.
    pub credential: Option<LlmCredentialView>,
}

// ============================================================
// LLM_CREDENTIAL_DELETE
// ============================================================

/// `LLM_CREDENTIAL_DELETE_REQ`. Removes the key at exactly `scope`.
///
/// Exact rather than resolved: a space revoking its own key must not be able
/// to remove the tenant's by accident.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmCredentialDeleteRequest {
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmCredentialDeleteResponse {
    /// `false` when that scope had no key of its own. Reported rather than
    /// always succeeding, because a caller revoking during an incident needs
    /// to tell "revoked" from "there was nothing there".
    pub existed: bool,
}

// ============================================================
// LLM_CREDENTIAL_VERIFY
// ============================================================

/// How a registered key fared against its provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyOutcome {
    /// The provider accepted the key and serves the registered model.
    Ok,
    /// The namespace has no credential to verify.
    NoCredential,
    /// The provider rejected the key. The tenant must register a new one.
    Unauthorized,
    /// The key authenticated but the registered model is not available to
    /// it, which would otherwise surface only as failed extractions.
    ModelUnavailable,
    /// The provider could not be reached. Says nothing about the key.
    Unreachable,
}

/// `LLM_CREDENTIAL_VERIFY_REQ`. Exercises the registered key against its
/// provider with the cheapest call that proves it works.
///
/// This exists because the alternative first signal of a bad key is thin
/// recall, several derivation stages downstream, where nobody is looking.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmCredentialVerifyRequest {
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmCredentialVerifyResponse {
    pub outcome: VerifyOutcome,
    /// The provider and model that were exercised. `None` when there was no
    /// credential to verify.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    /// The provider's own message on a failure, for an operator to act on.
    /// Never carries key material.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detail: Option<String>,
}
