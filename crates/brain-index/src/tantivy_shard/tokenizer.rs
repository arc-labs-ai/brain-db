//! Brain-side tantivy analyzer.
//!
//! The pipeline:
//!
//! 1. Unicode NFC normalisation.
//! 2. Lowercase (Unicode-aware).
//! 3. Sublanguage preservation — URL, ticket-style code IDs, and
//!    dot/underscore-joined identifiers survive as single tokens.
//! 4. Generic tokenisation of the residue.
//! 5. NO stop-word removal in v1.
//! 6. Porter English stemming applied only to the
//!    generic-tokenisation output; protected tokens bypass stemming.
//!
//! Registered on each per-shard [`tantivy::Index`] under the name
//! [`BRAIN_TOKENIZER_NAME`] (`"default"`), so the `TEXT` fields
//! pick it up automatically — no schema-version bump required.

use std::sync::Arc;

use regex::Regex;
use rust_stemmers::{Algorithm, Stemmer};
use tantivy::tokenizer::{TextAnalyzer, Token, TokenStream, Tokenizer};
use unicode_normalization::UnicodeNormalization;

/// Name the brain analyzer is registered under. Override of
/// tantivy's built-in `"default"` so the schemas pick
/// it up without a schema-version bump.
pub const BRAIN_TOKENIZER_NAME: &str = "default";

/// `\bhttps?://\S+`.
const URL_RE: &str = r"\bhttps?://\S+";

/// Ticket-style code IDs (uppercase prefix + dash + digits) —
/// matches `ACME-1247`. Case is preserved at match time; we
/// lowercase before emission.
const CODE_ID_RE: &str = r"\b[A-Za-z][A-Za-z0-9]+-\d+\b";

/// Identifiers with an internal `.` or `_` separator. Matches
/// `brain_storage`, `foo.bar.baz`, `module.fn`. Plain English
/// words DON'T match (no internal separator) so they fall
/// through to the residue path and get stemmed.
const DOTTED_ID_RE: &str = r"\b[a-zA-Z_][a-zA-Z0-9_]*[._][a-zA-Z0-9_.]+\b";

/// Word tokenizer for residue (between protected matches).
/// `\p{L}` = letter, `\p{N}` = number; together they form the
/// alphanumeric run we recognise as one word.
const WORD_RE: &str = r"[\p{L}\p{N}]+";

/// Single combined regex over the three protected patterns.
fn protected_regex() -> Regex {
    Regex::new(&format!("(?:{URL_RE})|(?:{CODE_ID_RE})|(?:{DOTTED_ID_RE})"))
        .expect("invariant: protected regex is a static literal")
}

fn word_regex() -> Regex {
    Regex::new(WORD_RE).expect("invariant: word regex is a static literal")
}

/// Brain analyzer.
///
/// `Clone` so tantivy can hand out one instance per
/// `token_stream` call without rebuilding regexes — the inner
/// `Arc<Regex>` makes that cheap.
#[derive(Clone)]
pub struct BrainTokenizer {
    protected: Arc<Regex>,
    word: Arc<Regex>,
    stemmer_algo: Algorithm,
}

impl BrainTokenizer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            protected: Arc::new(protected_regex()),
            word: Arc::new(word_regex()),
            stemmer_algo: Algorithm::English,
        }
    }
}

impl Default for BrainTokenizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Tokenizer for BrainTokenizer {
    type TokenStream<'a> = BrainTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        // Step 1: NFC normalisation. Allocates a new String;
        // the resulting offsets refer into the normalised buffer.
        // We use those offsets in the emitted Token so call sites
        // that read offsets get consistent positions w.r.t. the
        // analyzed text.
        let normalized: String = text.nfc().collect();

        let mut tokens: Vec<Token> = Vec::new();
        let stemmer = Stemmer::create(self.stemmer_algo);
        let mut last_end = 0usize;
        let mut position: u32 = 0;

        for m in self.protected.find_iter(&normalized) {
            if m.start() > last_end {
                emit_residue(
                    &normalized[last_end..m.start()],
                    last_end,
                    &self.word,
                    &stemmer,
                    &mut tokens,
                    &mut position,
                );
            }
            tokens.push(Token {
                position: position as usize,
                offset_from: m.start(),
                offset_to: m.end(),
                text: m.as_str().to_lowercase(),
                position_length: 1,
            });
            position = position.saturating_add(1);
            last_end = m.end();
        }
        if last_end < normalized.len() {
            emit_residue(
                &normalized[last_end..],
                last_end,
                &self.word,
                &stemmer,
                &mut tokens,
                &mut position,
            );
        }

        BrainTokenStream { tokens, cursor: 0 }
    }
}

fn emit_residue(
    text: &str,
    offset_base: usize,
    word_re: &Regex,
    stemmer: &Stemmer,
    out: &mut Vec<Token>,
    position: &mut u32,
) {
    for m in word_re.find_iter(text) {
        let lowered = m.as_str().to_lowercase();
        let stemmed = stemmer.stem(&lowered).into_owned();
        out.push(Token {
            position: *position as usize,
            offset_from: offset_base + m.start(),
            offset_to: offset_base + m.end(),
            text: stemmed,
            position_length: 1,
        });
        *position = position.saturating_add(1);
    }
}

/// Owned-vector token stream. `text` is consumed at construction
/// time (NFC normalisation makes the input non-shareable anyway),
/// so the stream carries no borrow.
pub struct BrainTokenStream {
    tokens: Vec<Token>,
    cursor: usize,
}

impl TokenStream for BrainTokenStream {
    fn advance(&mut self) -> bool {
        if self.cursor < self.tokens.len() {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn token(&self) -> &Token {
        debug_assert!(self.cursor > 0, "token() called before advance()");
        &self.tokens[self.cursor - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        debug_assert!(self.cursor > 0, "token_mut() called before advance()");
        &mut self.tokens[self.cursor - 1]
    }
}

/// Build the brain `TextAnalyzer`. No additional filters because
/// `BrainTokenizer` does its own lowercase + stem with protected-
/// token bypass.
#[must_use]
pub fn build_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(BrainTokenizer::new()).build()
}

#[cfg(test)]
mod tests;
