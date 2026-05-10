# 04.02 Tokenization

Before text can be embedded, it must be tokenized. This file specifies the tokenization pipeline.

## 1. The tokenizer

`bge-small-en-v1.5` uses the BERT WordPiece tokenizer with the English vocabulary.

- **Vocabulary size:** 30,522 tokens (the standard BERT-base-uncased vocab).
- **Special tokens:** `[CLS]`, `[SEP]`, `[UNK]`, `[PAD]`, `[MASK]`.
- **Casing:** uncased (the model was trained on lowercased text).
- **Implementation:** [HuggingFace `tokenizers`](https://github.com/huggingface/tokenizers), Rust crate.

## 2. The pipeline

Text → tokens:

1. **Normalize** — Unicode NFC normalization, strip accents, lowercase.
2. **Pre-tokenize** — split on whitespace and punctuation.
3. **WordPiece encode** — break each pre-token into vocabulary subwords. `"unaffordable"` → `["un", "##afford", "##able"]`.
4. **Add special tokens** — prepend `[CLS]`, append `[SEP]`.
5. **Truncate** — if longer than max length, truncate to `max_length - 1` tokens then append `[SEP]`.
6. **Pad** — pad to a uniform length within a batch using `[PAD]`.
7. **Output** — token_ids, attention_mask, token_type_ids (always zeros for our single-segment inputs).

Steps 1–4 produce a logical sequence; steps 5–6 prepare it for the model.

## 3. The maximum length

The model's training maximum is **512 tokens**. Inputs longer than this are truncated.

For a typical English text:

- 1 token ≈ 0.75 words (rough average).
- 512 tokens ≈ 380 words ≈ 2,000–3,000 characters.

Longer inputs are truncated to 512 tokens. The truncation is right-side (later tokens dropped), which preserves the beginning of the text. For agent memories, this is usually correct — the most-relevant content tends to be near the start (a topic statement, a name, a key fact).

The substrate exposes the truncation behavior:

- The cap is a hard limit; longer inputs lose tail content.
- The original text is stored in full; only the embedded vector is computed from the truncated portion.
- A warning may be logged or returned in metadata when truncation happens, so the agent can adjust if it notices.

## 4. Long-text strategies

For texts longer than 512 tokens, the agent has options:

### 4.1 Truncation (default)

The substrate truncates and embeds. The vector represents the first ~2,000 characters; the stored text is intact.

### 4.2 Application-level chunking

The agent splits the long text into chunks before encoding. Each chunk becomes a separate memory. Edges (`PART_OF`) link them to a parent memory representing the whole.

This is recommended for content the agent expects to query later — chunking gives finer-grained recall.

### 4.3 Application-level summarization

The agent generates a summary (perhaps via the LLM) and encodes the summary as a memory; the full text is stored elsewhere or as an attached resource.

This is recommended for content that's primarily about its gist rather than its details.

The substrate doesn't auto-chunk or auto-summarize; that's the application's responsibility. Auto-chunking would impose semantic decisions (where to split) that we're not in a position to make correctly.

## 5. Handling unknown characters

WordPiece's `[UNK]` token represents anything the vocabulary can't break down. This typically means:

- Characters outside the model's training distribution (uncommon scripts, emojis).
- Compound rare words.

The model has been trained to handle some `[UNK]` tokens; performance degrades gracefully. For typical agent text (English with occasional unusual content), unknown tokens are rare and not consequential.

If a deployment regularly encodes content with many `[UNK]`s, that's a signal to consider a different model (multilingual BGE, or a model with a richer vocabulary).

## 6. Performance

Tokenization is fast — < 0.1 ms per text in normal cases. The HuggingFace `tokenizers` library is heavily optimized:

- Compiled in Rust (no Python overhead).
- Uses fast string algorithms.
- Releases the GIL when called from Python.

In the embedding layer's latency budget (5–10 ms total CPU), tokenization is < 2% of the cost. Worth getting right but not worth optimizing further.

## 7. Tokenizer configuration

The tokenizer is loaded from a tokenizer.json file in the model directory:

```
/var/brain/models/bge-small-en-v1.5/
├── tokenizer.json       # tokenizer config + vocab
├── config.json          # model config
├── pytorch_model.bin    # or model.safetensors — the weights
```

The substrate loads `tokenizer.json` at startup and holds it for the process lifetime. Tokenizer state is read-only and shared across all embedding calls.

## 8. Threading

The HuggingFace tokenizer is thread-safe for encoding. Multiple Glommio executors on different cores can call it concurrently without coordination. There's no per-tokenizer mutex; the tokenizer's internal state is purely read-only at encode time.

## 9. Pre-tokenization considerations

Some preprocessing decisions matter for retrieval quality:

### 9.1 Lowercasing

The model is uncased; the tokenizer lowercases automatically. The substrate doesn't preserve case in the vector.

The original text retains case (the substrate stores text verbatim). If the agent later wants to display the memory, it sees the original case. Search, however, is case-insensitive at the model level — `"Brain"` and `"brain"` produce identical vectors.

### 9.2 Whitespace

Multiple consecutive whitespace characters are collapsed to single spaces by the pre-tokenizer. Leading and trailing whitespace is stripped.

### 9.3 Punctuation

Standard ASCII punctuation is split as separate tokens. Unicode punctuation may be `[UNK]` depending on the character.

### 9.4 Numbers

Numbers are tokenized as digit sequences. Long numbers may be split into pieces (`"1234567"` → `["1234", "##567"]`).

## 10. The role of [CLS]

BERT-family models output a representation per input token. For embeddings, the convention is to use the `[CLS]` token's output as the sentence representation, possibly with mean-pooling over all tokens.

For `bge-small-en-v1.5`, the official approach is to use the `[CLS]` representation (after a final linear projection). The substrate follows this convention.

## 11. Token-level introspection

The substrate doesn't expose tokenization details to clients. The wire protocol carries text, not tokens. Clients can't ask "how was this tokenized?" via Brain.

For debugging, operators can use the `ADMIN_TOKENIZE` opcode (admin-only) to test how a specific text tokenizes. This is operationally useful for understanding why a query did or didn't match expected content.

## 12. Vocabulary versioning

The tokenizer's vocabulary is part of the model. Different model versions may have different vocabularies. When the operator changes the model, the tokenizer changes too — and the model fingerprint covers this (the fingerprint hashes the tokenizer config along with the model weights).

A mid-flight tokenizer change (without a model change) is not supported. The tokenizer is loaded at startup and immutable.

---

*Continue to [`03_inference.md`](03_inference.md) for the inference path.*
