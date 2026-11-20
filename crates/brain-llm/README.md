# brain-llm

> LLM transport clients (Anthropic, OpenAI) for Brain's LLM extractor tier.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

Provides the provider-agnostic LLM transport used by Brain's LLM extractor tier
and supersession judge, plus the accounting that decides whether a call may be
made and what it cost. It defines a single `LlmClient` trait over `LlmRequest` /
`LlmResponse`, a `ModelRouter` that maps a model id to its `Provider`, and
concrete reqwest-based implementations for Anthropic and OpenAI. It carries no
extraction logic and no prompts — those live in `brain-extractors`.

## Key modules

- `types` — provider-agnostic `LlmMessage` / `LlmRequest` / `LlmResponse` /
  `LlmRole` / `SystemBlock`.
- `client` — the `LlmClient` trait and `LlmFuture`.
- `error` — the `LlmError` taxonomy plus `FailureClass`.
- `router` — `ModelRouter` + `Provider` (model id → provider routing).
- `providers` — concrete `AnthropicClient` and `OpenAIClient` impls, plus
  `providers::sizing`, which sizes the one runtime their HTTP runs on against
  the cores the shard tier left.
- `resolver` — `LlmCredentialResolver` (namespace → credential) and
  `CredentialScope` for cache-key isolation. The production implementation
  lives in `brain-server`, over Brain's own credential store; the resolvers
  here serve one client to every namespace.
- `admission` — the one place the byte budget for a call is derived, drawn on
  by both provider payloads and the preparation stage that feeds them.
- `inflight` — what that budget is currently spent on, metered apart by shape.
- `prep` — `PrepWorkload` + `prep_bytes`: what a batch's preparation stage will
  hold, in counts and lengths, so the estimate needs none of the pipeline's
  types.
- `pricing` — `ModelPricing`: the published per-model provider rates, and the
  one place a billed call becomes a dollar figure.

## Where it fits

Depends on `reqwest` (HTTP), `tokio` (timeouts), `serde`/`serde_json`, and
`blake3`. It has no `brain-*` dependencies — it sits at the
leaf of the extractor stack and is consumed by `brain-extractors` (LLM tier) and
the per-shard LLM setup in `brain-server`.

## Spec

- [`../../spec/11_extractors/01_extractor_tiers.md`](../../spec/11_extractors/01_extractor_tiers.md)
- [`../../spec/11_extractors/06_prompt_caching.md`](../../spec/11_extractors/06_prompt_caching.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
