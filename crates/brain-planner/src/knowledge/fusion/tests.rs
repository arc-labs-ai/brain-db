//! Unit tests for RRF fusion (phase 23.4).

use brain_core::{EntityId, MemoryId, StatementId};
use brain_index::{RankedItem, RankedItemId};

use super::{fuse_rrf, FusedItem, DEFAULT_K};
use crate::knowledge::router::{PerRetrieverWeights, Retriever};

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

fn memory_item(slot: u64, rank: u32, score: f32) -> RankedItem {
    RankedItem {
        id: RankedItemId::Memory(MemoryId::pack(0, slot, 0)),
        rank,
        score,
        snippet: None,
    }
}

fn statement_item(byte: u8, rank: u32, score: f32) -> RankedItem {
    RankedItem {
        id: RankedItemId::Statement(StatementId::from([byte; 16])),
        rank,
        score,
        snippet: None,
    }
}

fn entity_item(byte: u8, rank: u32, score: f32) -> RankedItem {
    RankedItem {
        id: RankedItemId::Entity(EntityId::from([byte; 16])),
        rank,
        score,
        snippet: None,
    }
}

fn find<'a>(out: &'a [FusedItem], id: &RankedItemId) -> Option<&'a FusedItem> {
    out.iter().find(|i| &i.id == id)
}

fn approx_eq(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// ---------------------------------------------------------------------------
// Single retriever — passthrough behaviour.
// ---------------------------------------------------------------------------

#[test]
fn single_retriever_passthrough_score_matches_formula() {
    let items = vec![
        memory_item(1, 1, 0.9),
        memory_item(2, 2, 0.7),
        memory_item(3, 3, 0.5),
    ];
    let outputs = vec![(Retriever::Semantic, items)];
    let weights = PerRetrieverWeights::default();

    let fused = fuse_rrf(&outputs, DEFAULT_K, &weights);

    assert_eq!(fused.len(), 3);
    // Equal weight = 1.0, k = 60.
    assert!(approx_eq(fused[0].fused_score, 1.0 / (60.0 + 1.0)));
    assert!(approx_eq(fused[1].fused_score, 1.0 / (60.0 + 2.0)));
    assert!(approx_eq(fused[2].fused_score, 1.0 / (60.0 + 3.0)));
}

#[test]
fn formula_matches_spec_example() {
    // §23/01: rank 1 contributes 1/61 ≈ 0.0164;
    //         rank 10 contributes 1/70 ≈ 0.0143.
    let items_rank1 = vec![memory_item(1, 1, 1.0)];
    let items_rank10 = vec![memory_item(2, 10, 1.0)];
    let weights = PerRetrieverWeights::default();

    let f1 = fuse_rrf(&[(Retriever::Semantic, items_rank1)], DEFAULT_K, &weights);
    let f10 = fuse_rrf(&[(Retriever::Semantic, items_rank10)], DEFAULT_K, &weights);

    assert!((f1[0].fused_score - 1.0 / 61.0).abs() < 1e-9);
    assert!((f10[0].fused_score - 1.0 / 70.0).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// Weights.
// ---------------------------------------------------------------------------

#[test]
fn weight_doubles_contribution() {
    let items = vec![memory_item(1, 1, 0.9)];
    let outputs = vec![(Retriever::Semantic, items)];

    let equal = fuse_rrf(&outputs, DEFAULT_K, &PerRetrieverWeights::default());

    let weights = PerRetrieverWeights {
        semantic: 2.0,
        ..Default::default()
    };
    let weighted = fuse_rrf(&outputs, DEFAULT_K, &weights);

    assert!(approx_eq(
        weighted[0].fused_score,
        equal[0].fused_score * 2.0
    ));
}

#[test]
fn zero_weight_zeros_contribution() {
    let items = vec![memory_item(1, 1, 0.9)];
    let outputs = vec![(Retriever::Semantic, items)];
    let weights = PerRetrieverWeights {
        semantic: 0.0,
        ..Default::default()
    };
    let fused = fuse_rrf(&outputs, DEFAULT_K, &weights);
    assert_eq!(fused.len(), 1);
    assert!(approx_eq(fused[0].fused_score, 0.0));
}

// ---------------------------------------------------------------------------
// Multi-retriever union.
// ---------------------------------------------------------------------------

#[test]
fn union_two_retrievers_same_doc_sums_contributions() {
    let id = MemoryId::pack(0, 7, 0);
    let outputs = vec![
        (
            Retriever::Semantic,
            vec![RankedItem {
                id: RankedItemId::Memory(id),
                rank: 1,
                score: 0.9,
                snippet: None,
            }],
        ),
        (
            Retriever::Lexical,
            vec![RankedItem {
                id: RankedItemId::Memory(id),
                rank: 2,
                score: 5.5, // Different scale; RRF ignores it.
                snippet: None,
            }],
        ),
    ];
    let fused = fuse_rrf(&outputs, DEFAULT_K, &PerRetrieverWeights::default());
    assert_eq!(fused.len(), 1);
    let expected = 1.0 / 61.0 + 1.0 / 62.0;
    assert!(approx_eq(fused[0].fused_score, expected));
    assert_eq!(fused[0].contributing.len(), 2);
}

#[test]
fn score_scale_invariance() {
    // Same ranks, very different raw_score scales → identical
    // fused outputs.
    let semantic_items = vec![memory_item(1, 1, 0.99), memory_item(2, 2, 0.95)];
    let lexical_items_small_scale = vec![memory_item(1, 1, 0.01), memory_item(3, 2, 0.005)];
    let lexical_items_huge_scale = vec![memory_item(1, 1, 999.0), memory_item(3, 2, 500.0)];

    let a = fuse_rrf(
        &[
            (Retriever::Semantic, semantic_items.clone()),
            (Retriever::Lexical, lexical_items_small_scale),
        ],
        DEFAULT_K,
        &PerRetrieverWeights::default(),
    );
    let b = fuse_rrf(
        &[
            (Retriever::Semantic, semantic_items),
            (Retriever::Lexical, lexical_items_huge_scale),
        ],
        DEFAULT_K,
        &PerRetrieverWeights::default(),
    );

    // Order + fused_score values must match — only ranks matter.
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(x.id, y.id);
        assert!(approx_eq(x.fused_score, y.fused_score));
    }
}

#[test]
fn document_absent_from_one_retriever_contributes_zero_from_it() {
    // Doc D appears only in retriever A; doc E only in B.
    let outputs = vec![
        (Retriever::Semantic, vec![memory_item(1, 1, 0.9)]),
        (Retriever::Lexical, vec![memory_item(2, 1, 0.9)]),
    ];
    let fused = fuse_rrf(&outputs, DEFAULT_K, &PerRetrieverWeights::default());
    assert_eq!(fused.len(), 2);
    for item in &fused {
        assert!(approx_eq(item.fused_score, 1.0 / 61.0));
        assert_eq!(item.contributing.len(), 1);
    }
}

// ---------------------------------------------------------------------------
// k sensitivity.
// ---------------------------------------------------------------------------

#[test]
fn smaller_k_emphasises_top_results() {
    // Two retrievers each returning ranks 1 and 10. With low
    // k the gap is wider; with high k it flattens.
    let outputs = vec![(
        Retriever::Semantic,
        vec![memory_item(1, 1, 1.0), memory_item(2, 10, 1.0)],
    )];

    let low = fuse_rrf(&outputs, 30, &PerRetrieverWeights::default());
    let high = fuse_rrf(&outputs, 120, &PerRetrieverWeights::default());

    let ratio_low = low[0].fused_score / low[1].fused_score;
    let ratio_high = high[0].fused_score / high[1].fused_score;

    assert!(
        ratio_low > ratio_high,
        "lower k must widen the top-rank advantage; got low={ratio_low} high={ratio_high}",
    );
}

// ---------------------------------------------------------------------------
// Ties + ordering.
// ---------------------------------------------------------------------------

#[test]
fn ties_break_deterministically_by_id() {
    // Two memory ids; both with rank 1 in two retrievers each
    // → identical fused scores. Tie-break by id-bytes ascending.
    let id_low = MemoryId::pack(0, 1, 0);
    let id_high = MemoryId::pack(0, 2, 0);
    let outputs = vec![
        (
            Retriever::Semantic,
            vec![
                RankedItem {
                    id: RankedItemId::Memory(id_low),
                    rank: 1,
                    score: 0.9,
                    snippet: None,
                },
                RankedItem {
                    id: RankedItemId::Memory(id_high),
                    rank: 2,
                    score: 0.8,
                    snippet: None,
                },
            ],
        ),
        (
            Retriever::Lexical,
            vec![
                RankedItem {
                    id: RankedItemId::Memory(id_high),
                    rank: 1,
                    score: 0.9,
                    snippet: None,
                },
                RankedItem {
                    id: RankedItemId::Memory(id_low),
                    rank: 2,
                    score: 0.8,
                    snippet: None,
                },
            ],
        ),
    ];
    let fused = fuse_rrf(&outputs, DEFAULT_K, &PerRetrieverWeights::default());
    // Both have score 1/61 + 1/62. Sorted by raw id bytes
    // ascending; id_low's raw u128 is smaller than id_high's.
    assert_eq!(fused.len(), 2);
    assert!(approx_eq(fused[0].fused_score, fused[1].fused_score));
    if let (RankedItemId::Memory(a), RankedItemId::Memory(b)) = (&fused[0].id, &fused[1].id) {
        assert!(a.raw() < b.raw(), "ties break by id ascending");
    } else {
        panic!("expected Memory ids");
    }
}

// ---------------------------------------------------------------------------
// Empty inputs + mixed variants.
// ---------------------------------------------------------------------------

#[test]
fn empty_outputs_returns_empty() {
    let fused = fuse_rrf(&[], DEFAULT_K, &PerRetrieverWeights::default());
    assert!(fused.is_empty());
}

#[test]
fn empty_retriever_list_is_harmless() {
    let outputs = vec![
        (Retriever::Semantic, vec![]),
        (Retriever::Lexical, vec![memory_item(1, 1, 0.9)]),
    ];
    let fused = fuse_rrf(&outputs, DEFAULT_K, &PerRetrieverWeights::default());
    assert_eq!(fused.len(), 1);
    assert!(approx_eq(fused[0].fused_score, 1.0 / 61.0));
}

// ---------------------------------------------------------------------------
// Property test — RRF must be order-invariant in its outer retriever list.
//
// Callers can't be relied on to deliver retriever outputs in a canonical
// order; if RRF ever depended on insertion order, the result set would
// flicker under reordering. This catches the HashMap-iteration-order
// pitfall by directly permuting `outputs` and asserting equality.
// ---------------------------------------------------------------------------

mod property {
    use super::*;

    use proptest::collection::vec as pvec;
    use proptest::prelude::*;

    /// Available retriever variants — keep all three so any pair
    /// can land in a permutation.
    const RETRIEVERS: [Retriever; 3] = [Retriever::Semantic, Retriever::Lexical, Retriever::Graph];

    /// Build a small pool of `RankedItem`s with overlapping ids so
    /// permutation can exercise the same-doc summation path.
    fn build_outputs(
        retriever_count: usize,
        per_retriever_items: usize,
    ) -> Vec<(Retriever, Vec<RankedItem>)> {
        let mut out = Vec::with_capacity(retriever_count);
        for &r in RETRIEVERS.iter().take(retriever_count) {
            let mut items = Vec::with_capacity(per_retriever_items);
            for j in 0..per_retriever_items {
                // Ids drawn from a small pool (slot 0..=5) to force
                // overlap across retrievers.
                let slot = (j % 6) as u64;
                items.push(memory_item(slot, (j as u32) + 1, 0.5));
            }
            out.push((r, items));
        }
        out
    }

    fn permute<T: Clone>(v: &[T], perm: &[usize]) -> Vec<T> {
        perm.iter().map(|&i| v[i].clone()).collect()
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 64,
            ..ProptestConfig::default()
        })]

        #[test]
        fn fusion_is_order_invariant_in_outer_list(
            retriever_count in 1usize..=3,
            per_retriever_items in 1usize..=20,
            // The permutation generator yields a Vec<usize> over
            // 0..3; we trim to retriever_count post-hoc.
            perm_seed in pvec(0usize..3, 3),
        ) {
            let outputs = build_outputs(retriever_count, per_retriever_items);

            // Derive a deterministic permutation of `0..retriever_count`
            // from the seed.
            let mut perm: Vec<usize> = (0..retriever_count).collect();
            for (i, &seed) in perm_seed.iter().enumerate().take(retriever_count) {
                let j = seed % retriever_count;
                perm.swap(i, j);
            }

            let baseline = fuse_rrf(
                &outputs,
                DEFAULT_K,
                &PerRetrieverWeights::default(),
            );
            let permuted_outputs = permute(&outputs, &perm);
            let permuted = fuse_rrf(
                &permuted_outputs,
                DEFAULT_K,
                &PerRetrieverWeights::default(),
            );

            prop_assert_eq!(
                baseline.len(),
                permuted.len(),
                "permutation changed result cardinality",
            );

            // Element-wise: same id, same fused_score (tie-break must
            // be deterministic — and id-only, never input-order).
            for (a, b) in baseline.iter().zip(permuted.iter()) {
                prop_assert_eq!(a.id, b.id, "ordering diverged on id");
                prop_assert!(
                    (a.fused_score - b.fused_score).abs() < 1e-12,
                    "score diverged: {} vs {}",
                    a.fused_score,
                    b.fused_score,
                );
            }
        }
    }
}

#[test]
fn mixed_id_variants_fuse_independently() {
    let outputs = vec![
        (Retriever::Semantic, vec![memory_item(1, 1, 0.9)]),
        (Retriever::Lexical, vec![statement_item(1, 1, 0.9)]),
        (Retriever::Graph, vec![entity_item(1, 1, 0.5)]),
    ];
    let fused = fuse_rrf(&outputs, DEFAULT_K, &PerRetrieverWeights::default());
    assert_eq!(fused.len(), 3, "different variants should not collide");
    for item in &fused {
        assert_eq!(item.contributing.len(), 1);
    }
    // Sanity: find each id.
    assert!(find(&fused, &RankedItemId::Memory(MemoryId::pack(0, 1, 0))).is_some());
    assert!(find(
        &fused,
        &RankedItemId::Statement(StatementId::from([1u8; 16]))
    )
    .is_some());
    assert!(find(&fused, &RankedItemId::Entity(EntityId::from([1u8; 16]))).is_some());
}
