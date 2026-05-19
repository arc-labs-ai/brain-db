# Your first substrate app

A 15-minute walk from a blank Brain deployment to a working
hybrid query. Picks up where the
[Docker quickstart](01-quickstart-docker.md) left off — you have
Brain running and want to actually *use* it.

You'll:

1. Install and start `brain-server`.
2. Encode three memories.
3. Run a substrate recall.
4. Declare a schema.
5. Run a query against the hybrid engine.

---

## 1. Install

Brain ships as a workspace of Rust crates. From a checkout:

```bash
cargo install --path crates/brain-server   # the daemon
cargo install --path crates/brain-shell    # `brain` — interactive shell
cargo install --path crates/brain-cli      # admin HTTP CLI
```

The `brain` shell is the fastest way to poke a running substrate
end-to-end — it's the `psql` / `redis-cli` equivalent. The SDK
example walked through at the end of this page is the same thing
written in Rust.

Or, on Linux (recommended — Glommio + io_uring), use a
cross-compiled binary from your `target/x86_64-unknown-linux-gnu`
directory after `cargo zigbuild --release`.

## 2. Start the server

```bash
brain-server --data-dir /tmp/brain-tutorial --listen 127.0.0.1:7332
```

Leave it running in one terminal. The data directory is created
on first start; it's empty until you encode.

## 3. Encode three memories

In another terminal, use the `brain` shell:

```bash
brain --server 127.0.0.1:7332 encode \
    "ticket ACME-1247 reproduces under heavy load"

brain --server 127.0.0.1:7332 encode \
    "Priya merged the budget pushback decision on Friday"

brain --server 127.0.0.1:7332 encode \
    "strawberry rhubarb cobbler recipe — bake at 375F"
```

Each call returns a `MemoryId`. The substrate stores the text
in the WAL, indexes the vector in HNSW, and indexes the text
in tantivy.

> **Or, interactively:** just `brain --server 127.0.0.1:7332` drops
> you into a REPL prompt where every line above runs without the
> `brain --server …` prefix. See [`../reference/brain-shell.md`](../reference/brain-shell.md).

## 4. Substrate recall (no schema yet)

```bash
brain --server 127.0.0.1:7332 recall "budget pushback" --include-text
```

The response is a list of `MemoryResult` rows with their
similarity scores. With no schema declared, `contributing_retrievers`
is empty and `fused_score` is `0.0` — this is the substrate's
pure-semantic recall path.

## 5. Declare a schema

Schema upload (`SCHEMA_UPLOAD_REQ`, spec §21/08) doesn't yet have a
shell or CLI surface — for now, upload it through the SDK:

```rust
let req = SchemaUploadRequest {
    schema_document: std::fs::read_to_string("/tmp/my-schema.brain")?,
    dry_run: false,
};
client.schema_upload(req).await?;
```

The server's per-shard `SchemaGate` flips from `false` to
`true`. Substrate `RECALL_REQ` now routes through the hybrid
pipeline transparently (spec §28/08 §5).

## 6. Hybrid query

Once a schema is declared, the existing `recall` verb routes through
the hybrid path automatically. Re-run:

```bash
brain --server 127.0.0.1:7332 recall "budget pushback" --include-text --output json | jq
```

The response now includes per-hit `contributing_retrievers`
(`Semantic` for text-only queries by the auto router) and
non-zero `fused_score`. Same data, hybrid path.

You can also call `recall` and see the same hybrid metadata
on `MemoryResult` — the runbook in
[`docs/runbooks/schema-toggle.md`](../runbooks/schema-toggle.md)
walks through this and the backfill flow.

## What's next

- [Operator runbook: schema toggle](../runbooks/schema-toggle.md)
- [Spec overview](../../spec/00_master_overview/02_doc_map.md)
- [SDK reference (`brain-sdk-rust`)](../../crates/brain-sdk-rust)
- [Hybrid query design](../../spec/24_hybrid_query/00_purpose.md)

For a tour of the cognitive primitives (ENCODE / RECALL / PLAN /
REASON / FORGET) see
[`spec/09_cognitive_operations/`](../../spec/09_cognitive_operations/).
