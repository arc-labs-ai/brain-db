//! `GET_CAPABILITIES` request + response payloads.
//!
//! Capability introspection lets clients learn at session start which
//! features the connected shard supports — whether the cross-encoder
//! reranker is loaded, which extractor tiers are live, the embedding
//! dimensionality, and which user schema namespaces are active.
//! `GET_CAPABILITIES` collapses that into one round-trip and makes the
//! deployment shape part of the public contract.
//!
//! The op is NOT admin. It's available to every authenticated client
//! the same way `PING` / `BYE` are — capability bits don't reveal
//! sensitive state, and clients need them at session warm-up.

/// Capabilities are server-side state; the only thing a client may send
/// is an optional effective-identity selector. Kept as a struct (rather
/// than a unit type) so the encoding stays consistent with every other
/// request body and the envelope's `decode` arm doesn't special-case
/// empty bytes. With `act_as` absent the body is the empty CBOR map
/// (`0xA0`), byte-identical to the historical field-less request.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GetCapabilitiesRequest {
    /// Optional effective identity (`act_as`) the snapshot is computed
    /// for, on behalf of the authenticated connection principal. The only
    /// identity-derived field is `schema_namespaces`, which is filtered to
    /// the effective caller's own namespace. `None` (omitted on the wire)
    /// means the connection's own key-bound identity.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<crate::ops::memory::ActAs>,
}

/// Capability snapshot returned by the server. Each field corresponds
/// to one server-side opt-in or runtime parameter the client may need
/// to know before issuing requests.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Capabilities {
    /// True when the cross-encoder reranker is loaded on this shard.
    /// Rerank is first-class and always-on: when this is `true`,
    /// every RECALL / QUERY result is reranked by the cross-encoder
    /// automatically — there is no per-request toggle. When `false`
    /// (the operator set `[rerank] enabled = false` at spawn), the
    /// shard returns RRF-only ordering. Clients read this bit purely
    /// to know whether the results they get back are reranked.
    pub rerank: bool,
    /// True when the LLM extractor tier is enabled (operator gate is
    /// on AND the registry has at least one wired LLM extractor).
    pub llm_extractor: bool,
    /// True when the classifier (GLiNER) extractor tier is enabled
    /// (operator gate on AND the model is loaded).
    pub classifier_extractor: bool,
    /// True when the pattern extractor tier is enabled (always
    /// available unless the operator explicitly opted out).
    pub pattern_extractor: bool,
    /// User schema namespaces active on the shard, filtered to the
    /// (effective) caller's own namespace — a caller never learns which
    /// other tenants have declared schema. So this is either empty (no
    /// user schema declared for the caller's namespace) or exactly
    /// `[caller_namespace]`. Excludes the always-on `brain` system
    /// namespace.
    pub schema_namespaces: Vec<String>,
    /// Embedding vector dimensionality the shard's embedder produces.
    /// clients that drive `EncodeVectorDirect` need this to validate
    /// pre-computed vectors before the round-trip.
    pub vector_dim: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GetCapabilitiesResponse {
    pub capabilities: Capabilities,
}
