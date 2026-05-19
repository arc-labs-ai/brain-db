# SDK

**Audience:** application developers connecting their Rust
service to Brain.

**Goal:** *integration patterns*. Not full API reference (see
[`../../reference/sdk-rust.md`](../../reference/sdk-rust.md)); not
"why does Brain exist" (see [`../../concepts/overview.md`](../../concepts/overview.md)).

## Pages

| Page | Read when |
|---|---|
| [`rust-quickstart.md`](rust-quickstart.md) | First time using `brain-sdk-rust` in your project |
| [`connection-pooling.md`](connection-pooling.md) | Going beyond a single connection per process |
| [`typed-knowledge.md`](typed-knowledge.md) | Mapping your domain types onto schemas with the derive macros |

## SDK shape

The Brain SDK exposes **domain verbs**, not engine names:

```rust
client.encode(...)?;     // store a memory or typed statement
client.recall(...)?;     // hybrid retrieval; substrate or knowledge depending on schema
client.plan(...)?;
client.reason(...)?;
client.forget(...)?;
```

There is no `recall_hybrid` or `recall_substrate` — the SDK
routes based on whether a schema is declared. See
[`../../concepts/substrate-vs-knowledge.md`](../../concepts/substrate-vs-knowledge.md).

## See also

- [`../../reference/sdk-rust.md`](../../reference/sdk-rust.md) —
  full public surface, every type and verb.
- [`../../reference/wire-protocol/`](../../reference/wire-protocol/)
  — what the SDK is talking over the wire, if you need to debug.
- Non-Rust SDKs are not part of v1.0. The wire protocol is
  documented if you need to build one.
