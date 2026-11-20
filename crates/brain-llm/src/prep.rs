//! Sizing the working set derivation builds *before* it calls a provider.
//!
//! A batch does not go straight to the model. It first assembles, and holds
//! for the whole stage:
//!
//! - every memory's own text, cloned out of the store;
//! - for each memory, the top-*m* similar memories' **full texts** plus an
//!   optional rolling summary;
//! - a batch-wide candidate-predicate set — one embedding vector and one name
//!   per existing predicate — scanned so the prompt can offer the database's
//!   real relation vocabulary;
//! - one query embedding per memory, to rank that set;
//! - per-namespace entity and declared-vocabulary blocks.
//!
//! That set routinely outweighs the request it produces, and it is resident
//! for longer. Admission has to be able to price it, so this turns a
//! *description* of the workload into a byte figure.
//!
//! ## Plain inputs, on purpose
//!
//! Everything here is counts and lengths. The types that actually hold this
//! memory live in the worker tier, which depends on this crate; naming them
//! here would invert that and make the LLM transport crate depend on the
//! pipeline it serves. A caller measures its own batch and hands over
//! numbers.
//!
//! ## Roughly right, and cheap
//!
//! This runs once per batch, on the path it is about to admit. It is pure
//! integer arithmetic over a dozen fields: no allocation, no traversal of the
//! data it describes. An estimate that walked every string to be exact would
//! cost a scan of the very working set it is trying to bound.
//!
//! The model is deliberately an over-estimate. Admission that under-prices
//! prep lets the machine overcommit, which is the failure that kills the
//! process; admission that over-prices it merely runs a little narrower than
//! it could.

/// A batch's preparation workload, described in counts and lengths.
///
/// Byte lengths are of the text itself; the model adds its own allowance for
/// the containers and copies around it. Every field is a per-batch
/// observation the caller already has to hand — nothing here needs a scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrepWorkload {
    /// Memories in the batch.
    pub memories: usize,
    /// Mean bytes of one memory's own text.
    pub memory_text_bytes: u64,
    /// Neighbours fetched per memory (the top-*m* cap).
    pub neighbours_per_memory: usize,
    /// Mean bytes of one neighbour's text. Neighbour texts are *full* texts,
    /// not snippets, which is why this dominates most batches.
    pub neighbour_text_bytes: u64,
    /// Bytes of the rolling summary carried per memory, or zero when no
    /// summarizer is wired.
    pub summary_bytes: u64,
    /// Existing predicates in the batch-wide reuse pool. Held once for the
    /// whole batch, not once per memory.
    pub predicate_candidates: usize,
    /// Components in one embedding vector.
    pub embedding_dimensions: usize,
    /// Distinct namespaces represented in the batch.
    pub namespaces: usize,
    /// Mean bytes of one namespace's rendered entity and declared-vocabulary
    /// blocks.
    pub vocabulary_bytes_per_namespace: u64,
}

/// An embedding component is an `f32`.
const BYTES_PER_EMBEDDING_COMPONENT: u64 = 4;

/// Allowance for one entry in a collection: the `String`/`Vec` header, the
/// key beside it, the hash-map slot, and the allocator's rounding. Generous
/// rather than tight — small entries are where a per-item cost is easiest to
/// forget and most likely to add up across a wide batch.
const PER_ENTRY_OVERHEAD_BYTES: u64 = 128;

/// How many times a given piece of text is resident at once.
///
/// Text arrives as an owned copy out of the store and is then rendered into a
/// prompt block that lives beside it until the call is built. Both are alive
/// during prep, so text is priced twice.
const TEXT_COPIES: u64 = 2;

/// Allowance for one predicate name in the candidate pool. Predicate names
/// are short identifiers, so a fixed figure beats asking a caller to measure
/// them.
const PREDICATE_NAME_BYTES: u64 = 64;

/// What a batch's preparation stage holds, in bytes.
///
/// Saturating throughout: an absurd description yields `u64::MAX` rather than
/// a wrapped — and therefore tiny — figure that would admit it. Admission
/// clamps an over-large reservation to the whole budget and runs it alone, so
/// a saturated estimate degrades to "this batch runs by itself", never to
/// "this batch is free".
#[must_use]
pub fn prep_bytes(workload: &PrepWorkload) -> u64 {
    let memories = workload.memories as u64;
    let neighbours = workload.neighbours_per_memory as u64;
    let embedding =
        (workload.embedding_dimensions as u64).saturating_mul(BYTES_PER_EMBEDDING_COMPONENT);

    // Per memory: its own text, its neighbours' full texts, its summary, the
    // query vector used to rank predicates, and the containers holding them.
    let neighbour_text = neighbours.saturating_mul(workload.neighbour_text_bytes);
    let per_memory_text = workload
        .memory_text_bytes
        .saturating_add(neighbour_text)
        .saturating_add(workload.summary_bytes)
        .saturating_mul(TEXT_COPIES);
    let per_memory = per_memory_text.saturating_add(embedding).saturating_add(
        neighbours
            .saturating_add(1)
            .saturating_mul(PER_ENTRY_OVERHEAD_BYTES),
    );

    // Batch-wide: the candidate-predicate pool, scanned once and shared by
    // every memory in the batch.
    let per_candidate = embedding
        .saturating_add(PREDICATE_NAME_BYTES)
        .saturating_add(PER_ENTRY_OVERHEAD_BYTES);
    let candidate_pool = (workload.predicate_candidates as u64).saturating_mul(per_candidate);

    // Batch-wide: one vocabulary block per distinct namespace, rendered once
    // and referenced by each of that namespace's memories.
    let vocabulary = (workload.namespaces as u64).saturating_mul(
        workload
            .vocabulary_bytes_per_namespace
            .saturating_mul(TEXT_COPIES)
            .saturating_add(PER_ENTRY_OVERHEAD_BYTES),
    );

    memories
        .saturating_mul(per_memory)
        .saturating_add(candidate_pool)
        .saturating_add(vocabulary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workload() -> PrepWorkload {
        PrepWorkload {
            memories: 32,
            memory_text_bytes: 2_000,
            neighbours_per_memory: 10,
            neighbour_text_bytes: 2_000,
            summary_bytes: 0,
            predicate_candidates: 500,
            embedding_dimensions: 384,
            namespaces: 2,
            vocabulary_bytes_per_namespace: 4_000,
        }
    }

    #[test]
    fn an_empty_batch_costs_nothing() {
        assert_eq!(prep_bytes(&PrepWorkload::default()), 0);
    }

    #[test]
    fn neighbour_fan_out_dominates_what_a_memory_costs() {
        // The claim the estimator rests on: a memory is expensive because it
        // drags in ten full neighbour texts, not because of its own text. So
        // fan-out, not batch width, is the first lever against prep pressure.
        let batch_only = PrepWorkload {
            memories: 0,
            ..workload()
        };
        let narrow = PrepWorkload {
            neighbours_per_memory: 0,
            ..workload()
        };
        let narrow_per_memory = prep_bytes(&narrow) - prep_bytes(&batch_only);
        let wide_per_memory = prep_bytes(&workload()) - prep_bytes(&batch_only);
        assert!(
            wide_per_memory > narrow_per_memory * 4,
            "with neighbours {wide_per_memory}, without {narrow_per_memory}"
        );
    }

    #[test]
    fn the_candidate_pool_is_charged_once_per_batch_not_per_memory() {
        // It is scanned once and shared, so widening the batch must not
        // multiply it — otherwise admission would price batches out of
        // existence.
        let one = PrepWorkload {
            memories: 1,
            ..workload()
        };
        let many = PrepWorkload {
            memories: 64,
            ..workload()
        };
        let candidate_pool_cost = prep_bytes(&PrepWorkload {
            memories: 0,
            namespaces: 0,
            ..workload()
        });
        let per_memory = (prep_bytes(&many) - prep_bytes(&one)) / 63;
        assert_eq!(
            prep_bytes(&many),
            prep_bytes(&one) + per_memory * 63,
            "memory cost is linear"
        );
        assert!(candidate_pool_cost > 0, "the pool is not free");
        assert!(
            prep_bytes(&many) - prep_bytes(&one) < candidate_pool_cost * 63,
            "the pool must not scale with batch width"
        );
    }

    #[test]
    fn embedding_width_is_charged_per_memory_and_per_candidate() {
        let narrow = PrepWorkload {
            embedding_dimensions: 384,
            ..workload()
        };
        let wide = PrepWorkload {
            embedding_dimensions: 1_536,
            ..workload()
        };
        let grew = prep_bytes(&wide) - prep_bytes(&narrow);
        let extra_components = (1_536 - 384) * BYTES_PER_EMBEDDING_COMPONENT;
        let vectors = workload().memories as u64 + workload().predicate_candidates as u64;
        assert_eq!(grew, extra_components * vectors);
    }

    #[test]
    fn an_absurd_description_saturates_rather_than_wrapping() {
        // A wrapped estimate would be small, and a small estimate admits.
        // Saturating means the worst case is "runs alone", never "runs free".
        let absurd = PrepWorkload {
            memories: usize::MAX,
            memory_text_bytes: u64::MAX,
            neighbours_per_memory: usize::MAX,
            neighbour_text_bytes: u64::MAX,
            summary_bytes: u64::MAX,
            predicate_candidates: usize::MAX,
            embedding_dimensions: usize::MAX,
            namespaces: usize::MAX,
            vocabulary_bytes_per_namespace: u64::MAX,
        };
        assert_eq!(prep_bytes(&absurd), u64::MAX);
    }

    #[test]
    fn a_rolling_summary_is_charged_per_memory() {
        // It is fetched alongside each memory's neighbours, not once.
        let without = workload();
        let with = PrepWorkload {
            summary_bytes: 1_000,
            ..without
        };
        assert_eq!(
            prep_bytes(&with) - prep_bytes(&without),
            1_000 * TEXT_COPIES * without.memories as u64
        );
    }
}
