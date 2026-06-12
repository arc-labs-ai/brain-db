//! Unit tests for the MMR merge / diversity stage.

use brain_core::MemoryId;
use brain_index::RankedItemId;

use super::{mmr_reorder, tokenize, MMR_LAMBDA_LIST};
use crate::retrieval::fusion::FusedItem;

fn item(slot: u64, rerank: f32) -> FusedItem {
    FusedItem {
        id: RankedItemId::Memory(MemoryId::pack(0, slot, 0)),
        fused_score: f64::from(rerank),
        contributing: Vec::new(),
        rerank_score: Some(rerank),
    }
}

fn ids(items: &[FusedItem]) -> Vec<u64> {
    items
        .iter()
        .map(|it| match it.id {
            RankedItemId::Memory(m) => m.slot(),
            _ => u64::MAX,
        })
        .collect()
}

#[test]
fn first_pick_is_always_argmax_relevance() {
    // Three near-identical texts; item 1 has the highest relevance.
    // Whatever MMR does to positions 2..N, the strongest item must stay
    // at rank 0 — this is what protects a single-answer query.
    let mut items = vec![item(1, 0.9), item(2, 0.8), item(3, 0.7)];
    let toks = vec![
        tokenize("alice likes hiking and climbing"),
        tokenize("alice likes hiking and climbing too"),
        tokenize("alice likes hiking and climbing as well"),
    ];
    mmr_reorder(&mut items, &toks, MMR_LAMBDA_LIST);
    assert_eq!(ids(&items)[0], 1, "highest-relevance item stays at rank 0");
}

#[test]
fn promotes_a_distinct_member_over_a_near_duplicate() {
    // Item 1 is the clear top. Items 2 and 3 are paraphrases of item 1;
    // item 4 is a distinct member ranked just below them. With diversity,
    // the distinct member should be pulled up ahead of the redundant
    // paraphrases.
    let mut items = vec![item(1, 0.90), item(2, 0.80), item(3, 0.78), item(4, 0.70)];
    let toks = vec![
        tokenize("she travelled to lisbon for the conference"),
        tokenize("she travelled to lisbon for the conference trip"),
        tokenize("travelled to lisbon for the conference again"),
        tokenize("he adopted a golden retriever puppy named max"),
    ];
    mmr_reorder(&mut items, &toks, MMR_LAMBDA_LIST);
    let order = ids(&items);
    assert_eq!(order[0], 1, "top relevance stays first");
    let pos_distinct = order.iter().position(|&s| s == 4).unwrap();
    let pos_dup3 = order.iter().position(|&s| s == 3).unwrap();
    assert!(
        pos_distinct < pos_dup3,
        "the distinct member (4) should rank above the near-duplicate (3); got {order:?}",
    );
}

#[test]
fn noop_below_three_items() {
    let mut items = vec![item(1, 0.5), item(2, 0.9)];
    let before = ids(&items);
    mmr_reorder(
        &mut items,
        &[tokenize("a b c"), tokenize("d e f")],
        MMR_LAMBDA_LIST,
    );
    assert_eq!(ids(&items), before, "≤2 items: order untouched");
}

#[test]
fn noop_on_nonpositive_lambda() {
    let mut items = vec![item(1, 0.5), item(2, 0.9), item(3, 0.7)];
    let before = ids(&items);
    let toks = vec![tokenize("a a a"), tokenize("a a a"), tokenize("a a a")];
    mmr_reorder(&mut items, &toks, 0.0);
    assert_eq!(ids(&items), before, "lambda 0: order untouched");
}

#[test]
fn permutation_preserves_membership() {
    // Every input item must survive the reorder exactly once.
    let mut items = vec![item(1, 0.9), item(2, 0.6), item(3, 0.6), item(4, 0.6)];
    let toks = vec![
        tokenize("one alpha"),
        tokenize("two beta"),
        tokenize("three gamma"),
        tokenize("four delta"),
    ];
    mmr_reorder(&mut items, &toks, MMR_LAMBDA_LIST);
    let mut got = ids(&items);
    got.sort_unstable();
    assert_eq!(got, vec![1, 2, 3, 4], "no item dropped or duplicated");
}

#[test]
fn tokenize_drops_short_tokens_and_lowercases() {
    let t = tokenize("The Quick a an of Brown-Fox");
    assert!(t.contains("quick"));
    assert!(t.contains("brown"));
    assert!(t.contains("fox"));
    assert!(!t.contains("a"), "1-char token dropped");
    assert!(!t.contains("an"), "2-char token dropped");
    assert!(!t.contains("of"), "2-char token dropped");
    assert!(t.contains("the"), "3-char token kept");
    assert!(!t.contains("The"), "lowercased (no capitalized 'The')");
}
