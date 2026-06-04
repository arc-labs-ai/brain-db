# brain-llm

> LLM transport clients (Anthropic, OpenAI) for Brain's LLM extractor tier.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

Provides the provider-agnostic LLM transport used by Brain's LLM extractor tier
and supersession judge. It defines a single `LlmClient` trait over
`LlmRequest` / `LlmResponse`, a `ModelRouter` that maps a model id to its
`Provider`, and concrete reqwest-based implementations for Anthropic and OpenAI.
This crate is transport only — it carries no extraction logic, prompts, or
caching; those live in `brain-extractors`.

## Key modules

- `types` — provider-agnostic `LlmMessage` / `LlmRequest` / `LlmResponse` /
  `LlmRole` / `SystemBlock`.
- `client` — the `LlmClient` trait and `LlmFuture`.
- `error` — the `LlmError` taxonomy.
- `router` — `ModelRouter` + `Provider` (model id → provider routing).
- `providers` — concrete `AnthropicClient` and `OpenAIClient` impls.

## Where it fits

Depends on `reqwest` (HTTP), `tokio` (timeouts), `serde`/`serde_json`, and
`blake3`. It has no `brain-*` dependencies — it sits at the leaf of the
extractor stack and is consumed by `brain-extractors` (LLM tier) and the
per-shard LLM setup in `brain-server`.

## Spec

- [`../../spec/11_extractors/01_extractor_tiers.md`](../../spec/11_extractors/01_extractor_tiers.md)
- [`../../spec/11_extractors/06_prompt_caching.md`](../../spec/11_extractors/06_prompt_caching.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
