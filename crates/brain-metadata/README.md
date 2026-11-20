# brain-metadata

> Metadata store (redb) and graph storage for Brain.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

The redb-backed metadata and typed-graph store for a shard. It holds memory
metadata, the namespace/space/session registries, the idempotency table, and the
durable LSN checkpoint, alongside the full typed-graph tables — entities,
statements (with evidence overflow), relations, predicates, schema/predicate
type interning, and the audit log. It encodes rows via `rkyv`/`bytemuck`, drives
schema upload and the seeded system schema, exposes graph traversal and
FORGET/supersession cascade logic, and implements
`brain_storage::recovery::MetadataSink` so WAL recovery can rebuild every
typed-graph table on restart.

## Key modules

| Module | Purpose |
|---|---|
| `db` | `MetadataDb` — the redb wrapper, checkpoint, and `MetadataSink` host. |
| `entity` | Entity CRUD, aliases, trigram index, type interning, and merge review. |
| `statement` | Statement CRUD/history, supersession, evidence packing & overflow slots. |
| `relation` | Relation CRUD/history plus typed-graph traversal. |
| `schema` | Schema apply/store, predicate interning, active-namespace tracking. |
| `system_schema` | Seeds and reads the always-active `brain:` system namespace. |
| `audit` | Audit-log writes and queries. |
| `cascade` | FORGET / supersession cascade across the graph. |
| `recovery` | WAL recovery, one file per `WalPayload` family. |
| `api_keys` | API-key create/lookup/revoke and scope resolution. |
| `namespace` / `registry` | Namespace (tenant) interning, plus the space and session registries — derived, recomputable view state. |
| `extraction` | The extraction queue: the durable trigger for the async extractor worker, and the backlog figure operators scrape. |
| `hype` / `statement_question` | Durable question-bridge vectors the in-RAM HyPE and statement-question HNSWs are rebuilt from. |
| `extractor` / `tables` | Extractor registry and pipeline-audit tables. |
| `llm_cache` | Separate redb-backed LLM response cache. |
| `llm_credential` / `llm_spend` | Tenant provider credentials (sealed) and cumulative spend for the non-extraction model stages. |
| `page_cache` | How much memory each redb page cache is opened with. |
| `storage_version` | On-disk storage-version stamping. `CURRENT_SCHEMA_VERSION` is 1, and open-time refusal is symmetric: a newer *or* older stamp is rejected rather than converted. |

## Where it fits

Depends on `brain-core`, `brain-storage` (recovery sink), `brain-protocol`
(schema AST), and `brain-crypto` (sealing tenant provider keys), plus `redb`,
`rkyv`, `blake3`, and `parking_lot`. Consumed by `brain-ops` and the shard
runtime in `brain-server`.

## Spec

- Metadata overview: [`../../spec/10_metadata/00_purpose.md`](../../spec/10_metadata/00_purpose.md)
- Table layout: [`../../spec/10_metadata/02_table_layout.md`](../../spec/10_metadata/02_table_layout.md)
- Typed-graph (substrate) tables: [`../../spec/10_metadata/03_substrate_tables.md`](../../spec/10_metadata/03_substrate_tables.md)
- Transactions: [`../../spec/10_metadata/04_transactions.md`](../../spec/10_metadata/04_transactions.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
