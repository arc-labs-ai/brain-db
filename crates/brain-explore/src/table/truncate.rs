//! Grapheme-aware middle truncation.
//!
//! Memory text and entity names often need to fit into a fixed-width cell
//! while keeping the human-recognizable head and the discriminating tail
//! visible. Slicing on byte or `char` boundaries breaks emoji ZWJ
//! sequences and any character whose grapheme spans more than one code
//! point; counting graphemes via `unicode-segmentation` keeps "🦀" as one
//! visible unit.

use unicode_segmentation::UnicodeSegmentation;

/// Middle-truncate `s` so its grapheme width is at most `max`.
///
/// Pattern: `<head>…<tail>`. The head is biased one grapheme larger than
/// the tail when the budget is odd, because the first few characters of a
/// memory text usually carry the "what kind of thing am I" signal.
///
/// When `max == 0` the result is empty; when `max == 1` the ellipsis itself
/// is returned (rather than truncating to a partial grapheme).
#[must_use]
pub fn middle_truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    // Collect once: we need the count and we'll index from both ends.
    let graphemes: Vec<&str> = s.graphemes(true).collect();
    let count = graphemes.len();
    if count <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    // 1 grapheme for the ellipsis; split the remainder between head and tail.
    let budget = max - 1;
    let head_len = budget.div_ceil(2);
    let tail_len = budget - head_len;
    let mut out = String::with_capacity(s.len());
    for g in &graphemes[..head_len] {
        out.push_str(g);
    }
    out.push('…');
    for g in &graphemes[count - tail_len..] {
        out.push_str(g);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn middle_truncate_no_change_when_within_width() {
        assert_eq!(middle_truncate("hello", 10), "hello");
        assert_eq!(middle_truncate("hello", 5), "hello");
    }

    #[test]
    fn middle_truncate_long_keeps_head_and_tail() {
        let s = middle_truncate("the quick brown fox jumps over the lazy dog", 11);
        assert!(s.contains('…'), "missing ellipsis: {s}");
        assert!(s.starts_with("the"), "lost head: {s}");
        assert!(s.ends_with("dog"), "lost tail: {s}");
    }

    #[test]
    fn middle_truncate_tiny_returns_ellipsis() {
        assert_eq!(middle_truncate("hello", 1), "…");
    }

    #[test]
    fn middle_truncate_zero_returns_empty() {
        assert_eq!(middle_truncate("hello", 0), "");
    }

    #[test]
    fn middle_truncate_preserves_unicode_graphemes() {
        // Crab emoji — one grapheme, multi-byte. Inside the budget: passes
        // through. Over the budget: split must not bisect the codepoint.
        assert_eq!(middle_truncate("🦀🦀🦀", 5), "🦀🦀🦀");
        let truncated = middle_truncate("🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀", 5);
        // 5 grapheme budget: 4 graphemes + 1 ellipsis.
        assert_eq!(truncated.graphemes(true).count(), 5);
        assert!(truncated.contains('…'));

        // Japanese — each character is one grapheme but multiple bytes in
        // UTF-8. Byte-slicing would panic / corrupt; grapheme-slicing is fine.
        let jp = "日本語のテキストです";
        let t = middle_truncate(jp, 6);
        assert_eq!(t.graphemes(true).count(), 6);
        assert!(t.starts_with("日本"), "lost JP head: {t}");
        assert!(t.ends_with("です"), "lost JP tail: {t}");

        // ZWJ family sequence — one user-perceived grapheme.
        let family = "a👨‍👩‍👧‍👦b";
        // 3 graphemes total ("a", family-zwj, "b"); within max=3, identity.
        assert_eq!(middle_truncate(family, 3), family);
    }

    #[test]
    fn middle_truncate_head_bias_on_odd_budget() {
        // budget = max - 1 = 3, head_len = ceil(3/2) = 2, tail_len = 1.
        let t = middle_truncate("abcdefghij", 4);
        assert_eq!(t, "ab…j");
    }
}
