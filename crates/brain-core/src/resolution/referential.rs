//! Non-referential surface detection — a shared backstop both the
//! classifier tier and the extractor apply path use to refuse minting an
//! entity for a surface that names no concrete referent (a bare pronoun,
//! determiner, or relative).
//!
//! **This is an English-only best-effort backstop, not the primary signal.**
//! The real defenses against junk entities are upstream and language-neutral:
//! the LLM/classifier decline to surface most pronouns as entities, and
//! coreference resolves a pronoun subject to its antecedent before minting.
//! A miss here only produces a low-harm junk node (the apply path counts it via
//! `apply_dropped_total`), never lost signal. The genuinely language-agnostic
//! fix — closed-class detection via the tokenizer, or having the LLM flag
//! non-referential subjects — is tracked as follow-up; until then this single
//! list is the one place the heuristic lives (previously duplicated, and
//! drifted, across two crates).

/// Closed-class English function words that name no referent: personal /
/// object / reflexive / possessive pronouns, bare determiners, relatives, and
/// indefinite pronouns. Matched as the whole (trimmed, lowercased) surface, so
/// real names that merely start with one of these ("Ian", "Al", "An Inspector
/// Calls") are unaffected.
const NON_REFERENTIAL_SURFACES: &[&str] = &[
    // personal (subject)
    "i",
    "you",
    "we",
    "they",
    "he",
    "she",
    "it",
    // personal (object)
    "me",
    "us",
    "him",
    "her",
    "them",
    // reflexive
    "myself",
    "yourself",
    "himself",
    "herself",
    "itself",
    "ourselves",
    "yourselves",
    "themselves",
    // possessive
    "my",
    "your",
    "our",
    "their",
    "his",
    "its",
    // determiners
    "this",
    "that",
    "these",
    "those",
    "the",
    "a",
    "an",
    // relative / interrogative
    "who",
    "whom",
    "whose",
    // indefinite
    "someone",
    "something",
    "anyone",
    "everyone",
    "nobody",
];

/// True if `surface` is a closed-class English function word that names no
/// concrete referent and must not be minted as an entity. Whole-surface,
/// case-insensitive match (trims surrounding whitespace).
#[must_use]
pub fn is_non_referential_surface(surface: &str) -> bool {
    let s = surface.trim();
    if s.is_empty() {
        return true;
    }
    NON_REFERENTIAL_SURFACES.contains(&s.to_lowercase().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_closed_class_words_case_insensitively() {
        for w in [
            "i", "They", "  she ", "THE", "my", "whom", "someone", "itself",
        ] {
            assert!(
                is_non_referential_surface(w),
                "{w:?} should be non-referential"
            );
        }
    }

    #[test]
    fn empty_or_whitespace_is_non_referential() {
        assert!(is_non_referential_surface(""));
        assert!(is_non_referential_surface("   "));
    }

    #[test]
    fn real_names_starting_with_a_stopword_pass() {
        // Only an exact whole-surface match is rejected — names that merely
        // begin with a function word are real referents.
        for w in ["Ian", "Al", "An Inspector Calls", "Theodore", "Wells Fargo"] {
            assert!(
                !is_non_referential_surface(w),
                "{w:?} should be referential"
            );
        }
    }

    #[test]
    fn non_english_pronouns_are_not_caught() {
        // Documents the known English-only limitation: the backstop misses
        // non-English pronouns (the LLM/coref + drop-counting cover these).
        for w in ["ellos", "il", "sie", "他"] {
            assert!(
                !is_non_referential_surface(w),
                "{w:?} not in the English list"
            );
        }
    }
}
