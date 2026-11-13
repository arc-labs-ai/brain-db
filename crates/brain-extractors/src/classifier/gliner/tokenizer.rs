//! Word splitter + prompt builder for GLiNER v2.1 inputs.
//!
//! At training time, GLiNER v2.1 builds inputs as a flat list of
//! whitespace-separated "words" and hands them to flair, which calls
//! the HuggingFace fast tokenizer with `is_split_into_words=True`:
//!
//! ```text
//! ["<<ENT>>", "label_1", "<<ENT>>", "label_2", "<<SEP>>",
//!  "word_1", "word_2", ..., "word_N"]
//! ```
//!
//! The DeBERTa-v3-small fast tokenizer then prepends `[CLS]`, expands
//! each word into one-or-more SentencePiece subtokens (`▁word`,
//! `▁word_piece`, ...) and appends a trailing `[SEP]`. `<<ENT>>` and
//! `<<SEP>>` are *regular* added tokens (not "special"), so the
//! Metaspace pre-tokenizer skips them and each one occupies exactly
//! one slot in `input_ids`.
//!
//! Pooling at inference (matches `compute_score_eval` in upstream
//! v2.1 `gliner/model.py`):
//!
//! - Label embeddings: hidden states at the positions of the
//!   `<<ENT>>` markers in the prompt section (one per label).
//! - Word embeddings: hidden state at the *first* subtoken of every
//!   input word in the words section.
//!
//! The structural `[CLS]` (id 1) and trailing `[SEP]` (id 2) are
//! DeBERTa's actual sequence-boundary tokens. They are *not* the
//! same as `<<ENT>>` (128001) / `<<SEP>>` (128002), which carry the
//! GLiNER-specific marker semantics.

use std::sync::OnceLock;

use regex::Regex;

use super::GlinerError;

/// One word identified by [`split_words`], with its byte-exact
/// character offsets in the original UTF-8 input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordOffset {
    /// The word's text, slice-equal to `&text[char_start..char_end]`.
    pub text: String,
    /// Inclusive char offset (byte index) of the word in the source.
    pub char_start: usize,
    /// Exclusive char offset (byte index) of the word end.
    pub char_end: usize,
}

/// Tokenised input ready to be fed to the backbone.
#[derive(Debug, Clone)]
pub struct TokenizedInput {
    /// Full token sequence: `[CLS] <<ENT>> label_1_subtokens <<ENT>>
    /// label_2_subtokens ... <<SEP>> word_1_subtokens word_2_subtokens
    /// ... [SEP]`.
    pub input_ids: Vec<u32>,
    /// `1` everywhere — GLiNER does not use padding at inference
    /// (single-sequence batches).
    pub attention_mask: Vec<u32>,
    /// Position of each `<<ENT>>` marker in `input_ids`. Length equals
    /// the number of labels; preserves label order.
    pub ent_positions: Vec<u32>,
    /// For every word in the original split, the absolute index in
    /// `input_ids` of its first subtoken (post-`<<SEP>>`).
    pub word_first_subtoken: Vec<u32>,
    /// Per-word character offsets in the source string, parallel
    /// to `word_first_subtoken`.
    pub word_offsets: Vec<(usize, usize)>,
}

/// Compile the upstream gline-rs splitter regex on first use.
/// `\w+` matches contiguous word characters (Unicode letters /
/// digits / underscore); `[^\s\w]` matches any single non-whitespace
/// non-word character (every punctuation mark is its own token).
fn splitter() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\w+|[^\s\w]").expect("invariant: literal regex is valid"))
}

/// Split `text` into words, preserving the original byte offsets.
pub fn split_words(text: &str) -> Vec<WordOffset> {
    splitter()
        .find_iter(text)
        .map(|m| WordOffset {
            text: m.as_str().to_owned(),
            char_start: m.start(),
            char_end: m.end(),
        })
        .collect()
}

/// IDs the tokenizer must resolve at load time; pass-through here so
/// the builder stays a pure function of inputs.
#[derive(Debug, Clone, Copy)]
pub struct TokenizerIds {
    /// DeBERTa-v3 `[CLS]` (id 1 on the standard tokenizer).
    pub cls: u32,
    /// DeBERTa-v3 `[SEP]` (id 2 on the standard tokenizer).
    pub sep: u32,
    /// GLiNER `<<ENT>>` marker (id 128001 in the v2.1 trained vocab).
    pub ent: u32,
    /// GLiNER `<<SEP>>` prompt-terminator (id 128002).
    pub prompt_sep: u32,
}

/// One word of the input, encoded once and reused by every window it
/// appears in.
#[derive(Debug, Clone)]
pub struct EncodedWord {
    /// Subtoken ids for the word.
    pub ids: Vec<u32>,
    /// Character offsets of the word in the source string.
    pub offsets: (usize, usize),
}

/// Split `text` into words and encode each one.
///
/// Words the tokenizer maps to nothing (whitespace-only or unpaired
/// surrogates) are dropped rather than given a fabricated position.
pub fn encode_words(
    tokenizer: &tokenizers::Tokenizer,
    text: &str,
) -> Result<Vec<EncodedWord>, GlinerError> {
    let mut out = Vec::new();
    for word in split_words(text) {
        let enc = tokenizer
            .encode(word.text.as_str(), false)
            .map_err(|e| GlinerError::Tokenizer(e.to_string()))?;
        let ids = enc.get_ids();
        if ids.is_empty() {
            continue;
        }
        out.push(EncodedWord {
            ids: ids.to_vec(),
            offsets: (word.char_start, word.char_end),
        });
    }
    Ok(out)
}

/// The label prompt every window carries: `[CLS] <<ENT>> label_1 ...
/// <<ENT>> label_n <<SEP>>`, with the position of each `<<ENT>>`.
pub struct Prompt {
    ids: Vec<u32>,
    ent_positions: Vec<u32>,
}

impl Prompt {
    /// Subtokens the prompt occupies in every window.
    pub fn subtokens(&self) -> usize {
        self.ids.len()
    }
}

/// Build the label prompt shared by every window of one input.
pub fn build_prompt(
    tokenizer: &tokenizers::Tokenizer,
    labels: &[&str],
    ids: &TokenizerIds,
) -> Result<Prompt, GlinerError> {
    let mut input_ids: Vec<u32> = Vec::new();
    let mut ent_positions: Vec<u32> = Vec::with_capacity(labels.len());

    // [CLS] — DeBERTa's structural sentence-start token.
    input_ids.push(ids.cls);

    // <<ENT>> + label subtokens for every label, in order.
    for label in labels {
        ent_positions.push(input_ids.len() as u32);
        input_ids.push(ids.ent);
        let enc = tokenizer
            .encode(*label, false)
            .map_err(|e| GlinerError::Tokenizer(e.to_string()))?;
        input_ids.extend_from_slice(enc.get_ids());
    }

    // <<SEP>> — GLiNER's prompt-section terminator (NOT [SEP]). This
    // is the boundary between the label prompt and the words section.
    input_ids.push(ids.prompt_sep);

    Ok(Prompt {
        ids: input_ids,
        ent_positions,
    })
}

/// Build the token sequence for one window over `words`.
///
/// Word offsets stay absolute in the original text, so spans decoded
/// from any window already refer to the whole input.
pub fn assemble_window(
    prompt: &Prompt,
    words: &[EncodedWord],
    ids: &TokenizerIds,
) -> TokenizedInput {
    let mut input_ids = prompt.ids.clone();
    let mut word_first_subtoken: Vec<u32> = Vec::with_capacity(words.len());
    let mut word_offsets: Vec<(usize, usize)> = Vec::with_capacity(words.len());

    for word in words {
        word_first_subtoken.push(input_ids.len() as u32);
        word_offsets.push(word.offsets);
        input_ids.extend_from_slice(&word.ids);
    }

    // Trailing [SEP] — DeBERTa's structural sentence-end token.
    input_ids.push(ids.sep);

    let attention_mask = vec![1u32; input_ids.len()];
    TokenizedInput {
        input_ids,
        attention_mask,
        ent_positions: prompt.ent_positions.clone(),
        word_first_subtoken,
        word_offsets,
    }
}

/// Room left for words in one window once the prompt and the trailing
/// `[SEP]` are charged against `max_len`.
///
/// `Err(InputTooLong)` means the label prompt alone fills the model's
/// sequence: no amount of windowing can make room, so the caller must
/// be told rather than handed empty results.
pub fn word_budget(prompt: &Prompt, max_len: usize) -> Result<usize, GlinerError> {
    let fixed = prompt.subtokens() + 1;
    max_len
        .checked_sub(fixed)
        .filter(|b| *b > 0)
        .ok_or(GlinerError::InputTooLong {
            got: fixed,
            max: max_len,
        })
}
