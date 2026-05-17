# Plan: Phase 22 — Task 02, Custom tantivy tokenizer

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Materialise the §23/02 §3 tokenizer pipeline as a single
`tantivy::TextAnalyzer` and register it on both per-shard
indexes from 22.1. After this sub-task, `TEXT` fields
(`memory_text.text`, `statements.subject_name`,
`statements.object_text`) use the brain-side analyzer instead of
tantivy's default.

Pipeline steps (binding §23/02 §3):

1. NFC normalisation.
2. Unicode-aware lowercase.
3. Sublanguage preservation — emit URL tokens (`\bhttps?://\S+`)
   and code-ID tokens (`[A-Z][A-Z0-9]+-\d+` plus dot/underscore
   identifiers) verbatim, NOT stemmed.
4. Generic tokenization (whitespace + punctuation) over the
   residue.
5. NO stop-word removal in v1.
6. Porter English stemming applied ONLY to the
   generic-tokenization output.

Not in scope:
- Field-specific analyzer overrides (deferred post-v1).
- Snippet generation (22.5 plan owns the call site).
- Stop-word filter (explicit v1 cut).
- Multi-language stemmers (post-v1).

## 2. Spec references

- `spec/23_retrievers/02_lexical_retriever.md` §3 — the
  pipeline. Binding.
- `spec/26_knowledge_storage/01_tantivy_layout.md` §2 — schema
  fields the analyzer is registered against.
- `spec/27_knowledge_workers/02_text_indexer_workers.md` §1 —
  text indexers consume the analyzer-aware schema (not
  registration; that's 22.2's responsibility).

## 3. External validation

| Item | Source | Confirmed |
|---|---|---|
| `tantivy::tokenizer::TextAnalyzer` builder API | docs.rs/tantivy/0.26.1/tantivy/tokenizer | `TextAnalyzer::builder(tokenizer).filter(filter).build()` chains tokenizers + filters. |
| Porter stemmer availability | `cargo info tantivy` features | `stemmer` feature (enabled in 22.1) pulls `rust-stemmers`; `tantivy::tokenizer::Stemmer::new(Language::English)` is the wrapper. |
| `Index::tokenizers().register("name", analyzer)` | docs.rs | Registers a named analyzer on the index. Schema fields with `TEXT` use the field's tokenizer name (default `"default"`); to use ours, schema fields must reference our name. |

**Decision point** flagged in §6: should the schema reference
the named analyzer (`TextOptions::set_indexing_options(TextFieldIndexing::default().set_tokenizer("brain_text"))`)? That
edits the schema bytes, which means the schema-version stamp
also has to bump. Cleaner alternative: register our analyzer
under the literal name `"default"` so the existing
`TEXT`-as-default schema fields pick it up automatically.

Going with the **`"default"` registration** approach in this
plan. Rationale: keeps schemas in 22.1 byte-identical, avoids a
22.0 retrofit, and tantivy supports overriding `"default"`.

## 4. Architecture sketch

```rust
// crates/brain-index/src/tantivy_shard/tokenizer.rs

use regex::Regex;
use tantivy::tokenizer::{
    BoxTokenStream, Language, LowerCaser, Stemmer, TextAnalyzer, Token, Tokenizer,
};

/// Brain tokenizer name (overrides tantivy's `"default"`).
pub const BRAIN_TOKENIZER: &str = "default";

pub fn build_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(BrainSplitTokenizer::new())
        .filter(LowerCaser)
        .filter(Stemmer::new(Language::English))
        .build()
}

/// Custom Tokenizer impl:
///
/// 1. Walk the input once with `regex::RegexSet` matching URL +
///    code-ID patterns. Each match is emitted as a token with
///    `WordIdent { protected: true }` (a token attribute we set
///    via `TokenStream::set_attr`).
/// 2. Residue between matches is fed to the default
///    whitespace+punctuation splitter (lifted from tantivy's
///    `SimpleTokenizer`).
/// 3. The `Stemmer` filter respects `protected` and short-
///    circuits on those tokens.
pub struct BrainSplitTokenizer { /* regex set + spans */ }

impl Tokenizer for BrainSplitTokenizer {
    type TokenStream<'a> = BoxTokenStream<'a>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> BoxTokenStream<'a> { ... }
}
```

**Sublanguage preservation** in the Stemmer step needs a
mechanism — tantivy's `Stemmer` filter doesn't have a "skip
protected tokens" hook. Two options:

| Option | Trade-off |
|---|---|
| A. Implement a custom `TokenFilter` that wraps `Stemmer` and skips tokens matching the URL / code-ID pattern. | One pass over each token; regex match per token is ~50 ns. |
| B. Emit protected tokens with an out-of-band marker (e.g. prefix `\u{FFFE}`) that the custom filter strips before stemming. | More fragile; markers leak if filtering is bypassed. |

**Chosen: Option A.** A `ProtectStemFilter { inner: Stemmer, protect_re: Regex }` that inspects each token before delegating. Regex compiled once at filter construction.

Registration site (extending 22.1's `mod.rs`):

```rust
// in TantivyShard::open, after both indexes are built:
self.memory_text.index.tokenizers().register(BRAIN_TOKENIZER, build_analyzer());
self.statements.index.tokenizers().register(BRAIN_TOKENIZER, build_analyzer());
```

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| Override `"default"` name (this plan) | Schemas from 22.1 unchanged; payload bump avoided | Less explicit which fields use which analyzer | ✓ |
| Add named analyzer + edit schemas | Field-level intent visible | Forces schema-version bump now; coupling with 22.1 | rejected |
| Skip Porter, use Snowball | Slightly better quality on edge cases | rust-stemmers' Snowball English is Porter2; tantivy's `Stemmer::English` is already Porter; minimal gain | rejected |
| Drop sublanguage preservation, rely on stop-word disable | One filter fewer | Loses exact-ID match (`ACME-1247` → `acm 1247`) | rejected — §23/02 §3 binds preservation |
| Add stop-word filter despite spec | Smaller index | Breaks exact-ID; explicitly cut in §23/02 §3 + 22.0 plan | rejected |

## 6. Risks / open questions

- **Risk:** tantivy's `"default"` analyzer name might be reserved or hard-coded somewhere. **Mitigation:** docs.rs shows `register("default", ...)` is the documented way to override. Tested in 22.2's unit tests.
- **Risk:** regex compilation cost on every `Tokenizer::token_stream` call. **Mitigation:** compile once in `BrainSplitTokenizer::new()` and clone via `Arc<Regex>`.
- **Open question:** should the analyzer be a single shared `TextAnalyzer` between scopes, or one per scope? **Resolution:** one per scope (cheap clone), registered separately, in case 22.5 wants per-scope tuning later. Same regex sources.

## 7. Test plan

Unit tests in `crates/brain-index/src/tantivy_shard/tokenizer_tests.rs`:

- `lowercase_and_stem` — input `"running quickly"` → tokens `["run", "quick"]` (Porter stems both).
- `preserves_url` — input `"see https://example.com/foo for details"` → `https://example.com/foo` survives verbatim alongside stemmed neighbours.
- `preserves_code_id` — input `"ticket ACME-1247 broke"` → `acme-1247` survives (lowercased) verbatim alongside `tick`, `broke`.
- `preserves_dotted_ident` — input `"call brain_storage::arena::Arena"` → `brain_storage::arena::arena` survives.
- `no_stopword_removal` — input `"the quick brown fox"` → all four tokens present (including `"the"`, `"a"`, `"of"`).
- `nfc_normalised` — input combining-form `"cafe\u{0301}"` (e + ◌́) yields the precomposed-form token `"café"`.
- `register_overrides_default` — register the analyzer under `"default"` on a fresh Index; build an `IndexWriter`, add a doc, query for the protected token; verify it matches.

The last test is an integration smoke (writes through an
IndexWriter); it doubles as a sanity check that 22.3's writer
will see the right tokens.

## 8. Commit shape

Single commit:

```
feat(index): 22.2 — custom tantivy tokenizer (URL/ID/Porter)

- crates/brain-index/src/tantivy_shard/tokenizer.rs (new):
  BrainSplitTokenizer + ProtectStemFilter + build_analyzer().
  Registered under `"default"` so 22.1's schemas pick it up
  without a schema-version bump.
- crates/brain-index/src/tantivy_shard/mod.rs: register the
  analyzer on each Index after open_or_create.
- 7 unit tests covering lowercase, stem, URL / code-ID / dotted
  identifier preservation, no stop-word removal, NFC, and the
  register-overrides-default sanity check.
```

## 9. Confirmation

Please confirm:

1. **Tokenizer registered under `"default"`** (vs. named analyzer requiring a 22.0 schema retrofit + version bump).
2. **`ProtectStemFilter` wraps `Stemmer`** to skip URL / code-ID tokens, rather than the marker-prefix approach.
3. **One analyzer instance per index** (cheap clone; future per-scope tuning easier).
4. **No stop-word filter** — explicit v1 cut, matching §23/02 §3.

After approval: write the module + tests, run `cargo zigbuild ... -p brain-index --tests` + `just docker cargo test -p brain-index tokenizer`, commit.
