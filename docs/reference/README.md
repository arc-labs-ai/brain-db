# Reference

**Audience:** anyone who needs the *exact* answer to a narrow
question. What's the default for `[hnsw] ef_search`? What's the
wire opcode for ENCODE? What error code does Brain return when an
idempotency conflict is detected?

**Goal:** *information*. Tables, field lists, grammar. Not
"how do I use X" (see [`../guides/`](../guides/)), not "why is X
this way" (see [`../concepts/`](../concepts/) and
[`../architecture/`](../architecture/)).

Reference pages are short and skimmable. They cite the spec
section that owns the contract. If a reference page contradicts
the spec, **the spec wins** and the reference page is stale.

## Single pages

| Page | Covers |
|---|---|
| [`configuration.md`](configuration.md) | Every TOML field, every default, every env-override pattern |
| [`brain-shell.md`](brain-shell.md) | Overview of the `brain` interactive shell (deep ref in [`shell/`](shell/)) |
| [`cli.md`](cli.md) | `brain-cli` admin subcommand reference |
| [`http-api.md`](http-api.md) | HTTP routes on the metrics port — `/healthz`, `/metrics`, `/v1/*` |
| [`sdk-rust.md`](sdk-rust.md) | Public surface of `brain-sdk-rust` |
| [`metrics.md`](metrics.md) | Catalogue of Prometheus metrics Brain emits |
| [`performance.md`](performance.md) | Latency + throughput targets per operation |

## Subtrees

| Subtree | Covers |
|---|---|
| [`shell/`](shell/) | `brain` shell deep reference: commands, REPL meta, output formats, configuration, errors |
| [`wire-protocol/`](wire-protocol/) | Frame format, opcodes, error codes, handshake |
| [`cognitive-operations/`](cognitive-operations/) | Exact semantics of ENCODE / RECALL / PLAN / REASON / FORGET |
| [`schema-dsl/`](schema-dsl/) | Grammar + worked examples of the schema language |

## What's not here

- The Rust API by signature — that's `cargo doc` / rustdoc. Run
  `cargo doc --workspace --no-deps --open` in the workspace root for the rendered version.
- The authoritative spec — that's [`../../spec/`](../../spec/).
  Reference pages here are derived from the spec; the spec is
  the source of truth.
- "How do I use this?" — that's [`../guides/`](../guides/).
- "Why is this designed this way?" — that's [`../concepts/`](../concepts/)
  (high-level) and [`../architecture/`](../architecture/) (deep).

## Citation discipline

Every reference page ends with `**Spec:** §NN/MM` pointing at the
authoritative section. If a code change makes a reference page
wrong, both the page and the cited spec section need re-checking
— the spec might be wrong too, but that requires the user to
approve a spec edit. Open an issue rather than fixing it silently.
