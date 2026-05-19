//! User-domain renderers. Admin-domain renderers (shard health,
//! snapshot manifest, etc.) live in brain-cli — see
//! .claude/plans/brain-explore-ui-layer.md §4.3.1.
//!
//! Each submodule implements [`crate::Render`] for one user-domain
//! response shape (encode result, recall hit list, plan steps,
//! subscription event, …). The rendering primitives in `theme`, `term`,
//! `table`, and `util` give every renderer the same color, hyperlink,
//! and truncation behaviour so the two consumer binaries (brain-shell
//! and brain-cli) render identically.

pub mod adhoc;
pub mod audit_card;
pub mod encode;
pub mod entity_card;
pub mod error;
pub mod forget;
pub mod graph_tree;
pub mod link;
pub mod memory;
pub mod plan;
pub mod reason;
pub mod recall_with_graph;
pub mod relation_card;
pub mod statement_card;
pub mod subscribe;
pub mod txn;

pub use adhoc::AdHocTable;
pub use audit_card::{AuditCard, TierOutcome};
pub use encode::EncodeRendered;
pub use entity_card::{EntityCard, MemorySummary, RelationSummary, StatementSummary};
pub use error::RenderableError;
pub use graph_tree::{GraphNode, GraphTree};
pub use link::{LinkRendered, UnlinkRendered};
pub use memory::RecallResults;
pub use plan::PlanSteps;
pub use reason::ReasonSteps;
pub use recall_with_graph::{
    EnrichedEntity, EnrichedRelation, EnrichedStatement, GraphEnrichment, RecallWithGraph,
};
pub use relation_card::{EntityRef, RelationCard};
pub use statement_card::{ObjectRef, StatementCard};
pub use subscribe::{SubscriptionEventList, SubscriptionEventRendered};
pub use txn::{TxnAbortRendered, TxnBeginRendered, TxnCommitRendered};

use brain_core::MemoryId;

// ─── id formatters shared across renderers ──────────────────────

/// Full `0x` + 32 hex form of a [`brain_protocol::request::WireMemoryId`].
/// Used in JSON output where a tool wants the canonical id.
#[must_use]
pub fn fmt_id(raw: u128) -> String {
    format!("0x{raw:032x}")
}

/// Compact `s{shard}/m{slot}/v{version}` form for table rendering.
#[must_use]
pub fn fmt_short_id(raw: u128) -> String {
    let id = MemoryId::from_be_bytes(raw.to_be_bytes());
    format!("s{}/m{}/v{}", id.shard(), id.slot(), id.version())
}

/// First 4 hex chars + `…`. Used for agent_id and model fingerprints
/// in compact views where the full form would dominate the line.
#[must_use]
pub fn fmt_short_hex_16(bytes: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}…",
        bytes[0], bytes[1], bytes[2], bytes[3]
    )
}

/// `0x` + 32 hex chars. Used in JSON output so scripts can grep
/// without parsing rkyv.
#[must_use]
pub fn fmt_hex_16(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(34);
    s.push_str("0x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[must_use]
pub fn fmt_kind(k: brain_protocol::request::MemoryKindWire) -> &'static str {
    match k {
        brain_protocol::request::MemoryKindWire::Episodic => "episodic",
        brain_protocol::request::MemoryKindWire::Semantic => "semantic",
        brain_protocol::request::MemoryKindWire::Consolidated => "consolidated",
    }
}

#[must_use]
pub fn fmt_edge_kind(k: brain_protocol::request::EdgeKindWire) -> &'static str {
    match k {
        brain_protocol::request::EdgeKindWire::Caused => "Caused",
        brain_protocol::request::EdgeKindWire::FollowedBy => "FollowedBy",
        brain_protocol::request::EdgeKindWire::DerivedFrom => "DerivedFrom",
        brain_protocol::request::EdgeKindWire::SimilarTo => "SimilarTo",
        brain_protocol::request::EdgeKindWire::Contradicts => "Contradicts",
        brain_protocol::request::EdgeKindWire::Supports => "Supports",
        brain_protocol::request::EdgeKindWire::References => "References",
        brain_protocol::request::EdgeKindWire::PartOf => "PartOf",
    }
}

/// Format a 16-byte transaction id as `0x…` hex (canonical wire form).
#[must_use]
pub fn fmt_txn_id(bytes: &[u8; 16]) -> String {
    fmt_hex_16(bytes)
}
