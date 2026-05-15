//! Trigram extraction + Jaccard similarity — pure functions used by
//! both the resolver (brain-core) and the redb integration
//! (brain-metadata::trigram_ops).
//!
//! pg_trgm convention: split a normalized string into whitespace-
//! separated words, pad each as `"  WORD "` (two leading spaces, one
//! trailing), extract every 3-byte window. Operates on **bytes**, not
//! Unicode code points — multi-byte sequences may be sliced; that's
//! pg_trgm's standard behavior, opaque-bucket trigrams. Acceptable
//! as long as both indexing and query paths extract the same way.

use std::collections::HashSet;

/// Extract the trigram set of a normalized string. Caller is
/// responsible for pre-normalizing (lowercase + whitespace collapse).
/// Empty input → empty set.
#[must_use]
pub fn extract_trigrams(normalized: &str) -> HashSet<[u8; 3]> {
    let mut out = HashSet::new();
    for word in normalized.split_whitespace() {
        let mut padded = Vec::with_capacity(word.len() + 3);
        padded.extend_from_slice(b"  ");
        padded.extend_from_slice(word.as_bytes());
        padded.push(b' ');
        for window in padded.windows(3) {
            if let Ok(arr) = <[u8; 3]>::try_from(window) {
                out.insert(arr);
            }
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
        let t = extract_trigrams("priya");
        let expected: HashSet<[u8; 3]> =
            [*b"  p", *b" pr", *b"pri", *b"riy", *b"iya", *b"ya "]
                .into_iter()
                .collect();
        assert_eq!(t, expected);
    }

    #[test]
    fn extract_two_words_unions_and_dedupes() {
        let t = extract_trigrams("priya patel");
        // "  p" appears in both "priya" and "patel" — dedup keeps one.
        assert!(t.contains(b"  p"));
        assert!(t.contains(b"pri"));
        assert!(t.contains(b"pat"));
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
