//! Trigram extraction + Jaccard similarity — pure functions used by
//! both the resolver (brain-core) and the redb integration
//! (brain-metadata::trigram_ops).
//!
//! pg_trgm convention: split a normalized string into whitespace-
//! separated words, pad each as `"  WORD "` (two leading spaces, one
//! trailing), extract every 3-element window. Windows are taken over
//! **Unicode code points**, not raw bytes, so a multibyte name ("李明",
//! "São Paulo") yields meaningful trigrams instead of arbitrary
//! mid-codepoint byte cuts. Each code-point trigram is folded to a
//! stable 24-bit `[u8; 3]` bucket via FNV-1a so the on-disk key type is
//! unchanged; both the index and the query path fold identically, so
//! Jaccard over the buckets approximates Jaccard over the code-point
//! trigrams (a rare bucket collision only adds a candidate, which the
//! exact-set Jaccard scoring then filters). Opaque-bucket trigrams.

use std::collections::HashSet;

/// Fold a 3-code-point window to a stable 24-bit bucket. FNV-1a over the
/// code points' little-endian bytes; deterministic across index and query.
#[must_use]
fn bucket(a: char, b: char, c: char) -> [u8; 3] {
    let mut h: u32 = 0x811c_9dc5;
    for ch in [a, b, c] {
        for byte in (ch as u32).to_le_bytes() {
            h ^= u32::from(byte);
            h = h.wrapping_mul(0x0100_0193);
        }
    }
    [
        (h & 0xff) as u8,
        ((h >> 8) & 0xff) as u8,
        ((h >> 16) & 0xff) as u8,
    ]
}

/// Extract the trigram set of a normalized string. Caller is
/// responsible for pre-normalizing (lowercase + whitespace collapse).
/// Empty input → empty set.
#[must_use]
pub fn extract_trigrams(normalized: &str) -> HashSet<[u8; 3]> {
    let mut out = HashSet::new();
    for word in normalized.split_whitespace() {
        // Pad with two leading + one trailing space (as code points), then
        // window over code points.
        let mut chars: Vec<char> = Vec::with_capacity(word.chars().count() + 3);
        chars.push(' ');
        chars.push(' ');
        chars.extend(word.chars());
        chars.push(' ');
        for window in chars.windows(3) {
            out.insert(bucket(window[0], window[1], window[2]));
        }
    }
    out
}

/// Jaccard similarity: `|A ∩ B| / |A ∪ B|`. Both sets empty → `0.0`
/// (avoids 0/0).
#[must_use]
pub fn jaccard(a: &HashSet<[u8; 3]>, b: &HashSet<[u8; 3]>) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        0.0
    } else {
        (intersection as f32) / (union as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_pg_trgm_style_single_word() {
        // "  priya " padded = 8 code points → 6 windows; trigrams are opaque
        // buckets now, so assert the count and determinism rather than bytes.
        let t = extract_trigrams("priya");
        assert_eq!(t.len(), 6);
        assert_eq!(t, extract_trigrams("priya"));
    }

    #[test]
    fn extract_two_words_unions_and_dedupes() {
        let t = extract_trigrams("priya patel");
        // The shared leading "  p" window dedupes: the union is strictly
        // smaller than the sum of the per-word trigram counts.
        let p = extract_trigrams("priya");
        let q = extract_trigrams("patel");
        assert!(!p.is_disjoint(&q), "both words share the '  p' bucket");
        assert!(t.len() < p.len() + q.len(), "shared trigram deduped");
        assert!(p.is_subset(&t) && q.is_subset(&t));
    }

    #[test]
    fn extract_handles_non_ascii_code_points() {
        // The whole point of windowing over code points: a CJK / accented
        // name yields real trigrams (no mid-codepoint byte slicing) and is
        // deterministic, so the same name resolves to itself.
        let a = extract_trigrams("李明");
        assert!(!a.is_empty());
        assert_eq!(a, extract_trigrams("李明"));
        let sao = extract_trigrams("são paulo");
        assert!(!sao.is_empty());
        assert_eq!(sao, extract_trigrams("são paulo"));
    }

    #[test]
    fn extract_empty_is_empty() {
        assert!(extract_trigrams("").is_empty());
        assert!(extract_trigrams("   ").is_empty());
    }

    #[test]
    fn jaccard_identical_is_one() {
        let a = extract_trigrams("priya patel");
        assert!((jaccard(&a, &a) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn jaccard_disjoint_is_zero() {
        let a: HashSet<[u8; 3]> = [*b"abc"].into_iter().collect();
        let b: HashSet<[u8; 3]> = [*b"xyz"].into_iter().collect();
        assert_eq!(jaccard(&a, &b), 0.0);
    }

    #[test]
    fn jaccard_empty_empty_is_zero() {
        let a: HashSet<[u8; 3]> = HashSet::new();
        let b: HashSet<[u8; 3]> = HashSet::new();
        assert_eq!(jaccard(&a, &b), 0.0);
    }

    #[test]
    fn jaccard_partial_overlap() {
        let a: HashSet<[u8; 3]> = [*b"abc", *b"def", *b"ghi"].into_iter().collect();
        let b: HashSet<[u8; 3]> = [*b"def", *b"ghi", *b"jkl"].into_iter().collect();
        assert!((jaccard(&a, &b) - 0.5).abs() < f32::EPSILON);
    }
}
