# 14.02 Logging

How Brain emits structured logs.

## 1. The format

JSON-structured logs:

```json
{
  "ts": "2026-05-07T12:00:00.123Z",
  "level": "info",
  "logger": "brain.executor",
  "shard": "<uuid>",
  "operation": "encode",
  "agent_id": "agent-001",
  "request_id": "...",
  "duration_ms": 8,
  "msg": "encode completed"
}
```

One JSON object per line. Readable with `jq`, ingestible by Loki / Elastic / Splunk / etc.

## 2. The log levels

- **TRACE**: per-frame protocol details. Used during deep debugging.
- **DEBUG**: per-request details. Used during development.
- **INFO**: normal lifecycle events (startup, shutdown, worker cycles).
- **WARN**: unusual conditions (retries, slow queries, capacity warnings).
- **ERROR**: errors that need attention (failed operations, etc.).

Default level: INFO. Production deployments may use WARN to reduce volume.

## 3. The destination

By default, logs go to stdout. Operators redirect to:

- A file (`brain > /var/log/brain.log`).
- A log aggregator (via stdout capture in containers).
- syslog.

Config:

```toml
[logging]
output = "stdout"            # or "file", "syslog"
file_path = "/var/log/brain/brain.log"
rotation = "daily"           # for file
```

## 4. The fields

Common fields in all entries:

- `ts`: ISO 8601 timestamp.
- `level`: severity.
- `logger`: which subsystem (brain.executor, brain.worker.decay, etc.).
- `msg`: human-readable message.

Operation-specific fields:

- `operation`: encode, recall, etc.
- `shard`: shard UUID.
- `agent_id`: agent (when applicable).
- `request_id`: request UUID.
- `duration_ms`: latency.

Error-specific fields:

- `error_code`: stable error identifier.
- `error_message`: human-readable.
- `stack`: Rust backtrace (DEBUG/TRACE only).

## 5. The "logger" hierarchy

The logger is a dotted path:

- `brain` — top level.
- `brain.executor` — request handlers.
- `brain.worker.<name>` — workers.
- `brain.storage.arena` — arena layer.
- `brain.storage.wal` — WAL layer.
- `brain.hnsw` — index.
- `brain.embedder` — embedding service.
- `brain.network` — connection layer.

Operators can filter by logger to focus:

```
| jq 'select(.logger | startswith("brain.worker"))'
```

## 6. The per-level guidance

What logs at each level:

```
TRACE:
  - Per-frame send/receive.
  - Per-iteration of a search.

DEBUG:
  - Per-request begin/end.
  - Per-cycle of a worker.
  - Per-checkpoint.

INFO:
  - Substrate startup / shutdown.
  - Each worker's hourly summary.
  - Each major event (rebuild started, snapshot created).

WARN:
  - Retries.
  - Slow operations (> p99 expectation).
  - Approaching capacity.
  - Recovered from transient error.

ERROR:
  - Failed operations after retries.
  - Storage errors.
  - Crashes.
```

## 7. The "no PII" rule

Logs should not contain user data by default:

- ✗ Memory text.
- ✗ Cue text.
- ✓ Memory IDs (opaque).
- ✓ Counts and durations.

For debugging that requires user data, use TRACE level (which can be enabled selectively, with auth).

## 8. The audit log

Separate from regular logs, an audit stream:

- Every state-mutating operation.
- Every admin action.
- Hash-chained for tamper evidence.

```
brain-audit.log:
{
  "ts": "...",
  "actor": "agent-001",
  "operation": "encode",
  "memory_id": "...",
  "agent_id": "...",
  "context": "...",
  "auth_method": "token",
  "hash": "sha256:...",
  "prev_hash": "sha256:..."
}
```

The hash chain lets auditors verify the log hasn't been tampered with.

## 9. Audit log retention

Audit logs typically have stricter retention:

- Production: 1-7 years (regulatory).
- Internal: 90 days.

Configurable via `[audit]` section. Logs can be exported to external storage (S3, etc.).

## 10. Log sampling

For very high-volume operations, the substrate may sample:

- 100% of errors.
- 10% of warnings.
- 1% of info.

Default is no sampling (full fidelity). Sampling is opt-in for cost-conscious deployments.

## 11. Per-request logging

Each request, by default, emits one INFO log at completion:

```json
{"ts":"...","level":"info","operation":"encode","shard":"<uuid>","agent_id":"...","request_id":"...","duration_ms":8,"status":"success","msg":"encode completed"}
```

For DEBUG, additional logs at start, mid-points, end.

## 12. Log-rate adaptive

Under load, log volume can become a problem. The substrate has:

- Rate-limited error logging (don't log "same error" thousands of times).
- Backpressure on the log pipeline (drop with warning if backed up).

These prevent observability from becoming a performance issue.

## 13. The "message" field guidance

Messages should be:

- Short (< 80 chars typical).
- Action-focused ("encode completed", "cache miss", "rebuild started").
- Avoid raw IDs/values in the message — those are in dedicated fields.

Bad:
```
"msg": "Encoded memory abc-123 in context xyz at 12:34:56 with salience 0.8"
```

Good:
```
"msg": "encode completed",
"memory_id": "abc-123",
"context_id": "xyz",
"salience": 0.8
```

## 14. The "trace ID" propagation

If the SDK provides a trace ID (per OpenTelemetry):

```
"trace_id": "abc...",
"span_id": "def..."
```

These propagate through to substrate logs. Operators can join logs and traces by trace ID.

## 15. The "structured exception" handling

Errors include structured fields:

```json
{
  "ts": "...",
  "level": "error",
  "msg": "encode failed",
  "operation": "encode",
  "agent_id": "...",
  "error": {
    "code": "QuotaExceeded",
    "message": "Agent has reached its memory quota",
    "details": {
      "current_count": 1000000,
      "limit": 1000000
    }
  }
}
```

Code, message, and details are separate fields. Operators can alert on specific codes.

## 16. The "logger configuration" surface

Per-logger level overrides:

```toml
[logging.loggers]
"brain.network" = "debug"      # See network details
"brain.hnsw" = "warn"          # Reduce HNSW noise
```

For focused debugging.

## 17. The log retention default

Default retention (file rotation):

- Daily rotation.
- Keep 7 days.
- Compress after 1 day.

Configurable. For aggregated logs (Loki, etc.), retention is in the aggregator.

## 18. The "graceful logging shutdown"

On shutdown, the substrate flushes pending log buffers:

```
1. Stop accepting new operations.
2. Existing ones complete and emit logs.
3. Logger flushes to disk / network.
4. Process exits.
```

This avoids losing logs at shutdown.

---

*Continue to [`03_tracing.md`](03_tracing.md) for distributed tracing.*
