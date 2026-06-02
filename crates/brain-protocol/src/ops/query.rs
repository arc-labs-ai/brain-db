//! Hybrid-query request wire types.
//!
//! Maps the planner's `QueryRequest` shape onto CBOR-encoded structs.
//! Discriminants are u8s with explicit semantics documented inline; the
//! wire-domain enums encode as their integer discriminant on the wire.
//!
//! Several shared types live here (`TimeRangeWire`, `RetrieverWire`,
//! `RetrieverSelectionWire`, `FusionConfigWire`, `ItemIdWire`,
//! `RetrieverContributionWire`, `RetrieverOutcomeWire`) because the
//! request needs them to be parsed; the response side
//! ([`crate::responses::query`]) re-exports them.

use crate::envelope::request::WireUuid;

// ---------------------------------------------------------------------------
// Shared wire-domain types — used by both request and response sides.
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

/// 4-variant `RankedItemId` projected to wire.
///
/// `kind` discriminant:
/// - 0 = Memory (`bytes` = u128 BE for MemoryId).
/// - 1 = Statement (`bytes` = u128 BE for StatementId).
/// - 2 = Entity (`bytes` = uuid bytes).
/// - 3 = Relation (`bytes` = uuid bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ItemIdWire {
    pub kind: u8,
    #[serde(with = "serde_bytes")]
    pub bytes: [u8; 16],
}

/// Per-retriever contribution to a fused item.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RetrieverContributionWire {
    pub retriever: RetrieverWire,
    pub rank: u32,
    pub raw_score: f32,
}

/// Retriever outcome summary.
///
/// `status` byte: 0=Success, 1=Skipped, 2=Timeout, 3=Failure.
/// `message` carries the skip reason or failure text; empty
/// for Success / Timeout.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RetrieverOutcomeWire {
    pub retriever: RetrieverWire,
    pub status: u8,
    pub message: String,
    pub latency_ms: f64,
    pub result_count: u32,
}

// ---------------------------------------------------------------------------
// QUERY (0x0160).
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

// ---------------------------------------------------------------------------
// RECALL_HYBRID (0x0163).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RecallHybridRequest {
    pub text: String,
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub agent_id_filter: Option<WireUuid>,
    pub limit: u32,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
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

    fn sample_request() -> QueryRequest {
        QueryRequest {
            text: "budget pushback".into(),
            entity_anchor: Some([7u8; 16]),
            kind_filter: vec![0, 1],
            predicate_filter: vec!["acme:role".into(), "acme:knows".into()],
            time_filter: Some(TimeRangeWire {
                from_unix_ms: Some(100),
                to_unix_ms: Some(900),
            }),
            confidence_min: Some(0.5),
            include_tombstoned: false,
            include_superseded: true,
            limit: 25,
            retrievers: RetrieverSelectionWire::Explicit(vec![
                RetrieverWire::Semantic,
                RetrieverWire::Graph,
            ]),
            fusion_config: Some(FusionConfigWire {
                k: 30,
                semantic_weight: 1.5,
                lexical_weight: 0.5,
                graph_weight: 2.0,
            }),
            request_id: [42u8; 16],
        }
    }

    #[test]
    fn query_request_round_trips() {
        let v = sample_request();
        assert_eq!(round_trip(&v), v);
    }

    #[test]
    fn recall_hybrid_request_round_trips() {
        let v = RecallHybridRequest {
            text: "x".into(),
            agent_id_filter: Some([9u8; 16]),
            limit: 10,
            request_id: [11u8; 16],
        };
        assert_eq!(round_trip(&v), v);
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
// QUERY (0x0160) — response side.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QueryResultItem {
    pub id: ItemIdWire,
    pub fused_score: f64,
    pub contributing: Vec<RetrieverContributionWire>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QueryResponse {
    pub items: Vec<QueryResultItem>,
    pub total_latency_ms: f64,
    pub retriever_outcomes: Vec<RetrieverOutcomeWire>,
}

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

// ---------------------------------------------------------------------------
// RECALL_HYBRID (0x0163) — response side.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MemoryHit {
    /// Big-endian bytes of the u128 MemoryId.
    #[serde(with = "serde_bytes")]
    pub memory_id: [u8; 16],
    pub fused_score: f64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RecallHybridResponse {
    pub items: Vec<MemoryHit>,
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
    fn query_response_round_trips() {
        let v = QueryResponse {
            items: vec![QueryResultItem {
                id: ItemIdWire {
                    kind: 0,
                    bytes: [1u8; 16],
                },
                fused_score: 0.0164,
                contributing: vec![RetrieverContributionWire {
                    retriever: RetrieverWire::Semantic,
                    rank: 1,
                    raw_score: 0.9,
                }],
            }],
            total_latency_ms: 12.3,
            retriever_outcomes: vec![RetrieverOutcomeWire {
                retriever: RetrieverWire::Semantic,
                status: 0,
                message: String::new(),
                latency_ms: 5.2,
                result_count: 1,
            }],
        };
        assert_eq!(round_trip(&v), v);
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

    #[test]
    fn recall_hybrid_response_round_trips() {
        let v = RecallHybridResponse {
            items: vec![MemoryHit {
                memory_id: [3u8; 16],
                fused_score: 0.05,
            }],
        };
        assert_eq!(round_trip(&v), v);
    }
}
