# brain-extractors

> Extractor framework (pattern / classifier / llm) for the Brain knowledge layer.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

Runs the three-tier extractor pipeline (pattern → GLiNER classifier → LLM) that
turns memory text into typed-graph candidates: entity, statement, and relation
mentions. Tier 1 is regex-driven, Tier 2 is zero-shot NER plus a statement-kind
pattern matcher loaded via candle, and Tier 3 calls Anthropic/OpenAI through
`brain-llm` with cost budgeting, JSON-schema validation, retries, and an
idempotency cache. It also owns the entity-resolution gauntlet (exact → alias →
fuzzy trigram → embedding HNSW → create) and the materialization bridge that
builds an `ExtractorRegistry` from persisted schema definitions.

## Key modules

- `framework` — the `Extractor` trait, registry, output items (`EntityMention` /
  `StatementMention` / `RelationMention`), run options, and tier gating.
- `pattern` — Tier 1 regex extraction (`PatternExtractor`, `CompiledRegex`).
- `classifier` — Tier 2 GLiNER NER + statement-kind pattern matcher.
- `llm` — Tier 3 LLM extraction with `CostBudget`, schema validation, retries.
- `resolver` / `resolver_llm` — entity-resolution gauntlet + LLM disambiguator.
- `materialize` — builds the dispatch registry from `ExtractorDefinition` rows.
- `idempotency` — text-hash keys for extractor caching.
- `enricher_hook` — `EnricherPlugin` dispatch seam (here to avoid a circular dep
  with `brain-plugins`).
- `supersede_source` — exposes the statement HNSW to the supersession judge.

## Where it fits

Depends on `brain-core`, `brain-protocol`, `brain-metadata`, `brain-index`,
`brain-embed`, and `brain-llm`. The extractor worker in `brain-workers` /
`brain-ops` dispatches through the registry this crate materializes, resolving
and persisting candidates inside the caller's redb write transaction.

## Spec

- [`../../spec/11_extractors/00_purpose.md`](../../spec/11_extractors/00_purpose.md)
- [`../../spec/11_extractors/01_extractor_tiers.md`](../../spec/11_extractors/01_extractor_tiers.md)
- [`../../spec/11_extractors/03_resolver.md`](../../spec/11_extractors/03_resolver.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
