//! Retrieval-query request wire types.
//!
//! Maps the planner's `QueryRequest` shape onto CBOR-encoded structs.
//! Discriminants are u8s with explicit semantics documented inline; the
//! wire-domain enums encode as their integer discriminant on the wire.
//!
//! `QueryRequest` is not a standalone client verb — it is the shared
//! request body embedded by `QueryExplainRequest` (0x0161) and
//! `QueryTraceRequest` (0x0162). The shared wire-domain types
//! (`TimeRangeWire`, `RetrieverWire`, `RetrieverSelectionWire`,
//! `FusionConfigWire`) live here because the request needs them to be
//! parsed.

use crate::envelope::request::WireUuid;

// ---------------------------------------------------------------------------
// Shared wire-domain types — used by the query request body.
// ---------------------------------------------------------------------------

/// Inclusive-start / inclusive-end window. `None` bounds =
/// open-ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TimeRangeWire {
    pub from_unix_ms: Option<u64>,
    pub to_unix_ms: Option<u64>,
}

/// Which retriever family. Discriminant byte stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum RetrieverWire {
    Semantic = 0,
    Lexical = 1,
    Graph = 2,
}

/// Auto-routing vs explicit retriever list.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RetrieverSelectionWire {
    Auto,
    Explicit(Vec<RetrieverWire>),
}

/// Per-query fusion override.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FusionConfigWire {
    pub k: u32,
    pub semantic_weight: f32,
    pub lexical_weight: f32,
    pub graph_weight: f32,
}

// ---------------------------------------------------------------------------
// Shared query request body (embedded by QUERY_EXPLAIN / QUERY_TRACE).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QueryRequest {
    pub text: String,
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub entity_anchor: Option<WireUuid>,
    /// StatementKind bytes (0=Fact / 1=Preference / 2=Event).
    pub kind_filter: Vec<u8>,
    /// Predicate filter as canonical `"namespace:name"` qnames.
    /// Schemaless deployments don't expose PredicateIds to clients —
    /// the planner resolves qnames through the registry per request,
    /// returning an empty result set for unknown qnames in
    /// schemaless mode and a `PredicateNotInSchema` error in strict
    /// mode.
    pub predicate_filter: Vec<String>,
    pub time_filter: Option<TimeRangeWire>,
    /// Bi-temporal time-travel anchor (record-time). When `Some(t)`,
    /// statement/relation results are filtered to the state the
    /// substrate believed at `t`, and `t` is the reference point for
    /// recency ranking. `None` is the current-state default.
    pub as_of_record_time_unix_nanos: Option<u64>,
    pub confidence_min: Option<f32>,
    pub include_tombstoned: bool,
    pub include_superseded: bool,
    pub limit: u32,
    pub retrievers: RetrieverSelectionWire,
    pub fusion_config: Option<FusionConfigWire>,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

// ---------------------------------------------------------------------------
// QUERY_EXPLAIN (0x0161).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QueryExplainRequest {
    pub query: QueryRequest,
}

// ---------------------------------------------------------------------------
// QUERY_TRACE (0x0162).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QueryTraceRequest {
    pub query: QueryRequest,
}

#[cfg(test)]
mod tests_req {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned + Clone,
    {
        let bytes = crate::codec::cbor::to_cbor_bytes(value);
        crate::codec::cbor::from_cbor_bytes(&bytes).expect("cbor decode")
    }

    #[test]
    fn retriever_selection_auto_and_explicit_round_trip() {
        assert_eq!(
            round_trip(&RetrieverSelectionWire::Auto),
            RetrieverSelectionWire::Auto
        );
        let explicit =
            RetrieverSelectionWire::Explicit(vec![RetrieverWire::Semantic, RetrieverWire::Lexical]);
        assert_eq!(round_trip(&explicit), explicit);
    }
}

// ============================================================
// Response payloads
// ============================================================

// ---------------------------------------------------------------------------
// QUERY_EXPLAIN (0x0161) — response side.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QueryExplainResponse {
    pub plan_text: String,
    pub estimated_cost_ms: f32,
}

// ---------------------------------------------------------------------------
// QUERY_TRACE (0x0162) — response side.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QueryTraceResponse {
    pub trace_text: String,
    pub total_latency_ms: f64,
}

#[cfg(test)]
mod tests_resp {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned + Clone,
    {
        let bytes = crate::codec::cbor::to_cbor_bytes(value);
        crate::codec::cbor::from_cbor_bytes(&bytes).expect("cbor decode")
    }

    #[test]
    fn query_explain_round_trips() {
        let v = QueryExplainResponse {
            plan_text: "PLAN: ...".into(),
            estimated_cost_ms: 12.5,
        };
        assert_eq!(round_trip(&v), v);
    }

    #[test]
    fn query_trace_round_trips() {
        let v = QueryTraceResponse {
            trace_text: "PLAN ... EXECUTION ...".into(),
            total_latency_ms: 22.4,
        };
        assert_eq!(round_trip(&v), v);
    }
}
