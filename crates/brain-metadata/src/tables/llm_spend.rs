//! Durable spend for model-backed stages that produce no extraction audit row.
//! The extraction tiers are not in this ledger: their durable per-call record
//! is `cost_micro_usd` in `EXTRACTOR_AUDIT_TABLE`. This table covers HyPE
//! generation, the HyPE refresh sweep, and entity disambiguation. Durable total
//! spend is the sum of those audit rows and this ledger; summing only one
//! undercounts, while adding extraction costs here double counts them.

use redb::TableDefinition;

pub const LLM_SPEND_TABLE: TableDefinition<'static, (&str, u8), u64> =
    TableDefinition::new("llm_spend");

/// `LLM_SPEND_TABLE` stage bytes. Stable; never reassigned.
pub mod llm_spend_stage {
    pub const HYPE_GENERATE: u8 = 1;
    pub const HYPE_REFRESH: u8 = 2;
    pub const ENTITY_DISAMBIGUATION: u8 = 3;
}
