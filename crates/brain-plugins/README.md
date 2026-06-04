# brain-plugins

> Plugin surface (enricher + connector) for the Brain knowledge pipeline.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

Defines the plugin surface for the Brain write pipeline. Two plugin kinds share
the `RecallPlugin` lifecycle trait: `EnricherPlugin` mutates candidate
`ExtractedItem`s between extraction and persistence (add, mutate, or drop), and
`ConnectorPlugin` pulls raw memories from external sources (the trait only — no
connectors ship in v1). Plugins are registered at compile time through
`PluginRegistry`; dynamic loading is intentionally unsupported. Every plugin call
runs on the writer's executor, must be synchronous and fast, and is wrapped in
`catch_unwind` so failures and panics are logged and the pipeline continues
fail-open with un-enriched items.

## Key modules

- `recall` — the shared `RecallPlugin` lifecycle trait.
- `enricher` — `EnricherPlugin`, `EnricherInput`, `EnricherOutput`.
- `connector` — `ConnectorPlugin` and its request/response/item types.
- `registry` — `PluginRegistry` (compile-time registration) + `EnricherOutcome`.
- `errors` — `PluginError` / `PluginResult` (failure-isolation taxonomy).

## Where it fits

Depends on `brain-core` and `brain-extractors` (for `ExtractedItem`). The write
pipeline in `brain-ops` drives registered enrichers via the `enricher_hook` seam
that lives in `brain-extractors` to avoid a circular dependency.

## Spec

- [`../../spec/11_extractors/07_plugins.md`](../../spec/11_extractors/07_plugins.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
