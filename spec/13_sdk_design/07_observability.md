# 13.07 SDK-Level Observability

What the SDK exposes for monitoring, logging, and tracing.

## 1. The three signals

The SDK supports OpenTelemetry-style observability:

- **Logs** — structured per-request entries.
- **Metrics** — counters, histograms, gauges.
- **Traces** — distributed tracing spans.

These integrate with the application's observability stack.

## 2. Logs

Each request produces log entries:

```json
{
  "ts": "2026-05-07T12:00:00Z",
  "level": "debug",
  "operation": "encode",
  "agent_id": "agent-001",
  "request_id": "...",
  "duration_ms": 8,
  "status": "success"
}
```

Log fields:

- Operation name.
- Agent ID (for correlation).
- Request ID.
- Duration.
- Status (success / error code).
- Optional: server, retry attempt, etc.

The user configures the log level (default INFO; debug shows per-request).

## 3. The logger interface

The SDK uses the language's standard logging:

- Rust: `tracing` crate.
- Python: `logging` module.
- TypeScript: pluggable; defaults to `console`.
- Go: `log/slog` (Go 1.21+).

Users can plug their own logger:

```rust
let client = Client::builder()
    .logger(MyCustomLogger::new())
    .build();
```

## 4. Metrics

The SDK exposes metrics:

```
brain_client_requests_total{operation="encode", status="success"} 12345
brain_client_request_duration_ms{operation="encode", quantile="0.99"} 0.025
brain_client_retries_total{operation="encode"} 23
brain_client_connections_active{server="host1:9090"} 4
brain_client_streams_active 2
```

Standard Prometheus naming.

The Client exports a `metrics()` accessor; users can scrape or push to their stack.

## 5. The metrics integration

```rust
use prometheus::Registry;

let registry = Registry::new();
let client = Client::builder()
    .metrics_registry(registry.clone())
    .build();

// Other parts of the app use the same registry.
```

For Python:

```python
from prometheus_client import REGISTRY

client = brain.Client(metrics_registry=REGISTRY)
```

The SDK's metrics integrate with the rest of the app's metrics.

## 6. Tracing

For distributed tracing, the SDK creates spans:

```rust
let _span = tracing::info_span!("brain.encode", agent_id, request_id).entered();
```

Each operation has a span:

- Span name: `brain.<operation>`.
- Span attributes: operation parameters (agent ID, etc.).
- Status: success / error.
- Duration: from start to response.

Spans nest in the application's tracing context. If the application is using OpenTelemetry, the Brain SDK spans appear as children of the application's spans.

## 7. The trace propagation

The SDK propagates trace context to the substrate:

- The substrate logs include the trace ID.
- The substrate's traces (if it uses OTel) become children of the SDK's spans.

End-to-end traces show the request flowing from application → SDK → substrate → response.

This integration matters for debugging in production.

## 8. The custom hooks

For arbitrary side effects, the SDK exposes hooks:

```rust
let client = Client::builder()
    .on_request(|req| log::debug!("Request: {:?}", req))
    .on_response(|resp| log::debug!("Response: {:?}", resp))
    .on_error(|err| metrics::increment("brain.errors", &[("code", err.code().as_str())]))
    .build();
```

Hooks fire at well-defined points. They're optional; default is no-op.

## 9. The "audit" mode

For compliance scenarios:

```rust
let client = Client::builder()
    .audit_log(AuditConfig {
        enabled: true,
        log_path: "/var/log/brain-audit.log",
        include_payloads: false,    // Privacy-aware
    })
    .build();
```

Audit mode logs every operation with a stable schema. Used for compliance, security review, and debugging.

## 10. The "request tracing" detail

For debugging, the SDK can trace individual requests:

```rust
client.encode("text")
    .trace(true)
    .send()
    .await?;
```

Tracing for one request includes:

- The request payload.
- Per-attempt details.
- The final response.
- Latency breakdown.

This is intentionally verbose; useful for debugging specific failures.

## 11. The "circuit breaker" metrics

If the SDK has a circuit breaker (per [13.03 Connection](03_connection.md)):

```
brain_client_circuit_state{server="host1:9090"} 0    # 0=closed, 1=open, 2=half-open
brain_client_circuit_failures_total{server="host1:9090"} 5
brain_client_circuit_opens_total{server="host1:9090"} 2
```

These help operators understand when the SDK is shedding load.

## 12. The "connection pool" metrics

```
brain_client_connections_active{server="host1:9090"} 4
brain_client_connections_idle{server="host1:9090"} 2
brain_client_connections_failed_total{server="host1:9090"} 1
brain_client_connection_age_sec{server="host1:9090", quantile="0.5"} 120
```

The pool's behavior is exposed; tuning becomes data-driven.

## 13. The "stream" metrics

For SUBSCRIBE clients:

```
brain_client_subscribe_active 2
brain_client_subscribe_events_received_total 1234567
brain_client_subscribe_buffer_size{stream_id="..."} 42
brain_client_subscribe_lag_sec{stream_id="...", quantile="0.99"} 0.005
```

Stream health is visible.

## 14. The "user-defined attributes"

The SDK allows custom tags:

```rust
let client = Client::builder()
    .default_tags([("team", "search"), ("env", "production")])
    .build();
```

These tags are added to all metrics from this client. Useful for multi-team deployments.

## 15. The "performance" measurement

For benchmarking:

```rust
client.metrics().request_count();
client.metrics().avg_latency();
client.metrics().error_rate();
```

These are also available as Prometheus metrics; the API just gives quick access.

## 16. The "dump state" debugging

For deep debugging:

```rust
let snapshot = client.debug_snapshot();
println!("{:#?}", snapshot);
```

Returns a structure describing:

- Connection state.
- Pending requests.
- Recent errors.
- Configuration.

Used during troubleshooting; not for production code.

## 17. The "logging level guidance"

What to log at each level:

- ERROR: failed requests after retries; connection failures; programmer errors.
- WARN: retries; slow requests; unusual patterns.
- INFO: client lifecycle (start, stop, reconnects).
- DEBUG: per-request details.
- TRACE: per-frame details.

Default level: INFO. Production deployments may use WARN or ERROR.

---

*Continue to [`08_testing.md`](08_testing.md) for testing.*
