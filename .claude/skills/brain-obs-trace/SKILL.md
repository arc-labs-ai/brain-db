---
name: brain-obs-trace
description: Verify tracing spans, OpenTelemetry attributes, structured logs at the right layer per spec §14. Fires on diffs that add public ops, error paths, or background workers.
when-to-use: |
  Triggers:
    - New public op or handler — needs a tracing span
    - New error path — needs a tracing event at the spec'd level
    - New background worker — needs span + metrics
    - User says "add tracing" / "observability"
spec-refs:
  - spec/14_observability_ops/01_metrics.md
  - spec/14_observability_ops/02_logs.md
  - spec/14_observability_ops/03_tracing.md
---

# Observability Trace

## When to use

Any new public op, handler, error path, or background worker. Brain emits OpenTelemetry traces, metrics, and structured logs (`tracing` + `tracing-subscriber` + `opentelemetry`).

## What this enforces

### Per spec §14/03 (tracing)

- Every public op has a `#[tracing::instrument]` span at `info` or `debug` level.
- Span names follow `<crate>.<op>` convention (e.g., `brain_ops.encode`).
- Span fields are **structured**, not formatted strings: `tracing::info!(memory_id = %id, salience = salience.raw())`, not `info!("encode {id} {salience}")`.
- Long-running ops (recall streaming, plan, reason) emit `tracing::Span::current().record(...)` at progress points so traces show partial state.

### Per spec §14/02 (logs)

| Category | Level | Reason |
|---|---|---|
| `Validation` | INFO | Normal client-side issue |
| `NotFound` | INFO | Normal client-side issue |
| `Conflict` | INFO | Semantic conflict |
| `Authentication` | WARN | Security-relevant |
| `Authorization` | WARN | Security-relevant |
| `Protocol` | WARN | Likely client bug |
| `ResourceExhausted` | WARN | Capacity issue |
| `Internal` | ERROR | Server-side problem |
| `Unavailable` | ERROR | Server-side problem |

Map the `ErrorCategory` (from `brain_protocol::error`) to the logging level in the error-emit path.

### Per spec §14/01 (metrics)

- Every public op emits a latency histogram (`<crate>_<op>_latency_seconds`).
- Every error path increments a counter (`<crate>_<op>_errors_total{category=...}`).
- Background workers emit "work done" counters.
- No PII, no sensitive data — see spec §14/02 §5.

## Workflow

1. **For each new public op:** add `#[tracing::instrument]` with structured fields.
2. **For each error return:** emit a `tracing::<level>!` matching the category table.
3. **For each handler:** verify metrics are recorded (latency histogram + error counter).
4. **For each background worker:** verify it has a span covering its tick + structured fields for "items processed", "duration", etc.
5. **Sweep for `format!` in log args** — replace with structured fields.

## Common errors → fixes

| Pattern | Why bad | Fix |
|---|---|---|
| `tracing::info!("encoded {id}")` | Unstructured; can't query | `tracing::info!(memory_id = %id, "encoded")` |
| Error logged at info level | Loss of severity | Match the category table |
| Token contents in log | PII / secret leak | Log presence, not value |
| No span on handler | Untraceable | `#[tracing::instrument(skip(state))]` |
| Span name `do_thing` | No crate scoping | `<crate>.<op>` convention |
| Worker without metric | Invisible work | Counter on each tick |

## Examples

### Golden — handler

```rust
#[tracing::instrument(
    name = "brain_ops.encode",
    skip(state, scratch),
    fields(memory_id = tracing::field::Empty, salience = tracing::field::Empty)
)]
pub fn handle_encode(state: &mut ShardState, req: EncodeRequest, scratch: &mut Scratch) -> Result<EncodeResponse, ProtocolError> {
    let start = Instant::now();
    let r = encode_inner(state, &req, scratch);
    METRICS.encode_latency.observe(start.elapsed().as_secs_f64());
    match &r {
        Ok(resp) => {
            tracing::Span::current()
                .record("memory_id", tracing::field::display(resp.memory_id))
                .record("salience", resp.salience);
            Ok(resp.clone())
        }
        Err(e) => {
            METRICS.encode_errors.with_label_values(&[e.category().as_str()]).inc();
            match e.category() {
                ErrorCategory::Internal | ErrorCategory::Unavailable => tracing::error!(error = %e, "encode failed"),
                ErrorCategory::Authentication | ErrorCategory::Authorization | ErrorCategory::Protocol | ErrorCategory::ResourceExhausted => tracing::warn!(error = %e),
                _ => tracing::info!(error = %e),
            }
            Err(e.clone())
        }
    }
}
```

### Counter — leaky logging

```rust
let token = req.credentials.token().to_owned();
tracing::info!("auth attempt token={}", token);    // ← reject, leaks token
```

Replace with:

```rust
tracing::info!(token_present = !req.credentials.is_empty(), "auth attempt");
```

## Cross-references

- `production-checklist` — gates observability before merge.
- `brain-perf-target` — latency histograms feed into the perf-target check.
- spec §14, spec §03/10 §12.

## Source / Adaptations

Project-local. Operationalizes spec §14.
