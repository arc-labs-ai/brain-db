# 13.01 SDK Design Principles

The principles guiding Brain SDK design.

## 1. Idiomatic > Uniform

Each SDK should feel native to its language, not a transliteration of the Rust API.

- Python: snake_case, keyword args, optional types.
- TypeScript: camelCase, interfaces, Promises.
- Go: PascalCase exported, returning errors.
- Rust: snake_case, builder pattern, Result types.

A Python developer reading the Python SDK shouldn't think "this is a Rust API in Python clothing".

## 2. Reveal the Substrate's Shape

The SDK shouldn't hide what Brain is doing:

- Operations correspond to wire-protocol calls.
- A `recall()` call results in one (or for multi-shard, a few) network round-trips.
- The agent should be able to reason about latency.

Hiding the substrate's behavior leads to surprising performance. We prefer transparency.

## 3. No Magic

The SDK doesn't:

- Auto-cache (the application should know what's cached).
- Auto-retry destructive operations (only idempotent ones, with explicit RequestId).
- Auto-derive or guess parameters not specified.

Magic is convenient until it's wrong. We avoid it.

## 4. Errors Are Data

Error handling uses the language's native error mechanism (exceptions, Result, errors.New, throw).

Errors carry:

- Code (stable identifier).
- Message (human-readable).
- Retryable flag.
- Optional details (which field was bad, etc.).

Applications can match on codes for typed error handling.

## 5. Defaults Should Just Work

For first-time users:

- One line to create a client.
- One line per operation.
- No mandatory configuration beyond the substrate address.

Sensible defaults for all parameters: K=10, consistency=eventual, timeouts=30s, retries=3.

## 6. Power Users Get Levers

For advanced users:

- All parameters configurable.
- Custom retry policies pluggable.
- Custom connection pooling.
- Hooks for tracing, metrics, etc.

The SDK exposes the full wire protocol; nothing is hidden.

## 7. Versioning Predictability

SDK versions follow semantic versioning:

- MAJOR: breaking API changes.
- MINOR: additions, backward-compatible.
- PATCH: bug fixes.

Within a major version, code that worked at v1.0 should still work at v1.5. Deprecations are signaled clearly.

## 8. Wire-Protocol Awareness, Not Wire-Protocol Replication

The SDK isn't a 1-to-1 mapping of the wire protocol. It's a typed, ergonomic layer.

For example:
- The wire protocol has separate `ENCODE` and `ENCODE_BATCH` opcodes; the SDK offers `encode()` (single) and `encode_batch()` (batch) — same methods but the SDK might internally batch a few `encode()` calls if the user uses async parallel calls.
- The wire protocol's stream IDs are internal; the SDK uses request handles instead.

The SDK is a layer, not a passthrough.

## 9. Async-First, Sync Available

All SDKs are async by default. Sync wrappers are provided for languages where mixing is hard (Python, especially).

Async benefits:

- High concurrency without thread overhead.
- Natural for I/O-bound workloads.
- Composable with the language's async ecosystem.

For sync use cases (scripts, simple agents), the sync wrapper is one method call away.

## 10. Cancellation Aware

Operations should respect cancellation:

- Rust: futures should cancel cleanly.
- Python: asyncio.CancelledError should propagate.
- TypeScript: AbortController support.
- Go: context.Context propagation.

Cancellation matters for long-running operations (PLAN, REASON, large RECALL).

## 11. Backpressure Communicated

When the substrate is overloaded:

- The SDK surfaces `Overloaded` errors clearly.
- Doesn't auto-retry indefinitely (would amplify load).
- Provides metrics for the application to track and shed.

The SDK is honest: if the substrate is overwhelmed, the application should know.

## 12. Testable

SDKs ship with test utilities:

- A mock client (returns canned responses).
- A fake substrate (in-memory, for integration tests).
- Recording / replay for debugging.

Application authors should be able to test without a real substrate.

## 13. Observable

SDKs emit:

- Per-request logs (debug-level by default; structured).
- Metrics (latency histograms, error counters).
- Tracing spans (compatible with OpenTelemetry).

These integrate with the application's observability infrastructure.

## 14. No Hidden State

The SDK doesn't carry hidden state across calls:

- Each operation is self-contained.
- The client is the only stateful object (connection pool).
- No "last result" implicitly available.

This makes the SDK predictable in async/concurrent contexts.

## 15. Compatibility Across Versions

The SDK should work across substrate versions within a major:

- SDK v1.5 works with substrate v1.0 through v1.x.
- New SDK methods that need newer substrate features should error gracefully.

This decouples SDK upgrades from substrate upgrades.

## 16. The "small surface area" preference

The SDK has a small public surface:

- One `Client` type.
- ~10 operation methods.
- A handful of error types.
- A handful of result types.

Internal helpers are private; users shouldn't need to learn them.

A small surface is easier to learn, document, and evolve.

## 17. Documentation as Part of the SDK

Every public method has:

- A docstring describing what it does.
- An example of typical usage.
- Notes on errors and edge cases.

Inline documentation is a first-class part of the SDK. Users should rarely need external docs.

## 18. Examples Over Reference

Brain's SDK docs lead with examples, not API reference:

```
"How to encode and recall a memory" → step-by-step example
"Client.encode method reference" → comes after.
```

Examples accelerate learning. References are for after the basics.

## 19. The "Pit of Success"

Configurations and defaults should encourage correct usage:

- Default timeouts that prevent infinite hangs.
- Default retry counts that handle transient errors but don't overload.
- Default request IDs that ensure idempotency.

Users should fall into correct usage by default.

## 20. The "Pit of Pit"

For advanced misuse, the SDK doesn't pretend everything's fine:

- Setting K=1000000 → warning logged.
- Setting timeout=0 → error.
- Disabling retries → warning that says "are you sure?"

The SDK helps users avoid foot-guns.

---

*Continue to [`02_core_api.md`](02_core_api.md) for the core API.*
