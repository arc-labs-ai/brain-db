//! Sigmoid + threshold + greedy flat-NER span decoding.
//!
//! Mirrors the reference implementation in `gline-rs/src/decoded/span.rs`:
//!
//! 1. For every `(start_word, width, label)` triple, compute
//!    `p = sigmoid(score)`. Drop the triple if `start + width >=
//!    num_words` or if `p < threshold`.
//! 2. Sort the surviving candidates by score descending.
//! 3. Greedily accept candidates that do not overlap any already-
//!    accepted span (flat-NER constraint: no nesting, no overlap).
//! 4. Sort the accepted spans by `char_start` ascending for stable
//!    output.
//!
//! Scoring and resolution are separate steps because an input split
//! across several windows is scored once per window but must be
//! resolved once overall: windows overlap, so the same entity is
//! proposed by two of them and only one copy may survive.

use super::Span;

/// Numerically-stable scalar sigmoid: avoids `exp` overflow on
/// large-magnitude logits via the standard split.
#[inline]
pub(crate) fn sigmoid(x: f32) -> f32 {
    if x >= 0.0 {
        1.0 / (1.0 + (-x).exp())
    } else {
        let e = x.exp();
        e / (1.0 + e)
    }
}

/// `logits`: `[num_words][max_width][num_labels]`.
///
/// `word_offsets[i] = (char_start_of_word_i, char_end_of_word_i)`,
/// already byte-exact in the original input string.
pub fn candidate_spans(
    logits: &[Vec<Vec<f32>>],
    threshold: f32,
    labels: &[&str],
    word_offsets: &[(usize, usize)],
    text: &str,
) -> Vec<Span> {
    let num_words = word_offsets.len();
    let mut candidates: Vec<Span> = Vec::new();
    for (start_idx, widths) in logits.iter().enumerate() {
        if start_idx >= num_words {
            continue;
        }
        for (k, scores) in widths.iter().enumerate() {
            let end_idx = start_idx + k;
            if end_idx >= num_words {
                continue;
            }
            for (c, &score) in scores.iter().enumerate() {
                let p = sigmoid(score);
                if p < threshold {
                    continue;
                }
                let Some(label) = labels.get(c) else { continue };
                let (cs, _) = word_offsets[start_idx];
                let (_, ce) = word_offsets[end_idx];
                if ce <= cs {
                    continue;
                }
                let text_slice = text.get(cs..ce).unwrap_or("").to_string();
                candidates.push(Span {
                    label: (*label).to_string(),
                    text: text_slice,
                    char_start: cs,
                    char_end: ce,
                    score: p,
                });
            }
        }
    }

    candidates
}

/// Reduce scored candidates to a flat, non-overlapping set.
///
/// Sorts by score descending (stable on ties) and greedily accepts a
/// candidate when it overlaps nothing already accepted, then orders the
/// result by `char_start`.
pub fn resolve_flat(mut candidates: Vec<Span>) -> Vec<Span> {
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut accepted: Vec<Span> = Vec::with_capacity(candidates.len());
    for c in candidates {
        let overlap = accepted
            .iter()
            .any(|a| c.char_start < a.char_end && a.char_start < c.char_end);
        if !overlap {
            accepted.push(c);
        }
    }
    accepted.sort_by_key(|s| s.char_start);
    accepted
}
