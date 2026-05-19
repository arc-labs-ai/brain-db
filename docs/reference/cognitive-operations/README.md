# Cognitive operations reference

**Audience:** anyone calling Brain — SDK users, integrators,
people reading wire dumps.

**Goal:** *exact semantics* of the five cognitive operations.
Request fields, response fields, error conditions, idempotency
behaviour. Not "why these verbs" (see
[`../../concepts/cognitive-operations.md`](../../concepts/cognitive-operations.md)).

## The five operations

| Op | What it does | One-liner |
|---|---|---|
| [`encode.md`](encode.md) | Persist a memory (or, with a schema, typed statements) | Write side. Returns a `MemoryId` after WAL fsync. |
| [`recall.md`](recall.md) | Retrieve by similarity / filter | Read side. Substrate or hybrid depending on whether a schema is declared. |
| [`plan.md`](plan.md) | Multi-step retrieval with reasoning | Compound op; spec §09/04. |
| [`reason.md`](reason.md) | Inferential retrieval over the knowledge layer | Knowledge-layer-only. Requires a declared schema. |
| [`forget.md`](forget.md) | Soft (tombstone) or hard delete | Soft = tombstone + 7-day grace; hard = zero-immediate. |

## Cross-cutting

Every operation honours:

- **Idempotency by RequestId** — same `request_id` within 24h
  returns the cached response. Different params with the same
  `request_id` returns `Conflict`. See [`../wire-protocol/`](../wire-protocol/).
- **WAL-before-ack** — writes do not return until their WAL record
  is fsynced.
- **Errors** — the stable taxonomy at
  [`../wire-protocol/error-codes.md`](../wire-protocol/error-codes.md).

## See also

- [`../../concepts/cognitive-operations.md`](../../concepts/cognitive-operations.md)
  — why the API looks this way.
- [`../../../spec/09_cognitive_operations/`](../../../spec/09_cognitive_operations/)
  — authoritative spec.
