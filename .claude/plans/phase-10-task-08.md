# Sub-task 10.8 — `brain-cli stats` and `health`

**Reads:**
- `spec/14_observability_ops/06_admin_ops.md` §3 (status ops).
- `crates/brain-server/src/admin/mod.rs` — already serves
  `/healthz` and `/metrics` HTTP endpoints (shipped in 9.13).
- Skim §14 (scriptable output) — JSON vs table modes.

**Phase doc:** `docs/phases/phase-10-sdk-cli.md` §10.8.

**Done when:** `brain-cli health` and `brain-cli stats` connect
to a running brain-server's admin HTTP endpoint, fetch the
spec'd info, and print either JSON or a human-readable table.
Tests cover the success path + a failing-connect case.

---

## 1. Scope decision: HTTP, not admin-op wire frames

Two ways to implement these commands:

| Option | Path |
|---|---|
| A — HTTP | `brain-cli` GETs `/healthz` + `/metrics` on the admin port. |
| B — Wire | `brain-cli` opens a TCP+handshake connection like the SDK does, sends `ADMIN_STATS_REQ`, decodes `AdminStatsResponse`. |

Option A wins for 10.8: the server already serves `/healthz` and
`/metrics` (sub-task 9.13). Option B would require wiring
admin-op dispatch server-side (currently stubbed as
`NotYetImplemented` in `dispatch.rs`) — that's its own
sub-task. We do A here; the bigger admin-wire surface lands
alongside 10.9 / 10.10 / 10.11 when each command actually needs
it.

10.8 deliberately keeps the CLI scope narrow: two commands +
the supporting plumbing (arg parsing, HTTP client, output
formatter). The remaining commands (10.9-10.12) reuse the
plumbing.

---

## 2. Module layout (folder-per-concern per project rules)

```
crates/brain-cli/src/
├── main.rs                  thin entry — parses args, dispatches
├── cli/
│   ├── mod.rs               argv parsing (hand-rolled, no clap dep
│   │                        in workspace yet)
│   └── args.rs              Args struct + flags (--server,
│                            --output, --token, --help, --version)
├── commands/
│   ├── mod.rs               dispatch table + Command enum
│   ├── stats.rs             GET /metrics + parse + render
│   └── health.rs            GET /healthz + render
├── output/
│   ├── mod.rs
│   ├── json.rs              serde_json renderer
│   └── table.rs             borrowless table renderer (no deps)
└── http/
    └── mod.rs               minimal reqwest blocking client
```

LOC: cli/args ~80, commands/* ~80 each, output/* ~60 each,
http ~50, main ~40. Total ~450 LOC + tests.

---

## 3. CLI surface (10.8)

```
brain-cli [--server <host:port>] [--output json|table] <COMMAND>

Commands:
  health                  Probe the admin /healthz endpoint
  stats                   Snapshot /metrics counters

Options:
  --server <host:port>    Admin endpoint (default 127.0.0.1:9091)
  --output <fmt>          json | table (default table)
  --token <value>         Admin token (currently unused; spec §14/06 §12)
  --version, -V
  --help, -h
```

The token flag is parsed-but-ignored in 10.8 because the
server's `/healthz` and `/metrics` are unauthenticated per spec
§14/01 §19. We accept the flag for forward compatibility.

---

## 4. Output formats

### 4.1 `health --output json`
```json
{"status":"healthy","admin_endpoint":"127.0.0.1:9091","probe":"/healthz"}
```

### 4.2 `health --output table` (default)
```
status            healthy
admin_endpoint    127.0.0.1:9091
probe             /healthz
```

### 4.3 `stats --output json`
A parsed Prometheus text-format → JSON object keyed by metric
name, with labels as nested object and a single numeric value:
```json
{
  "brain_connections_total": [{"labels":{},"value":12}],
  "brain_admin_requests_total": [{"labels":{"endpoint":"/metrics"},"value":3}],
  ...
}
```

### 4.4 `stats --output table`
Two-column "metric" → "value", labels appended in `{...}`.

---

## 5. Plumbing details

### 5.1 HTTP client
brain-server's `llm/` adapters already pull `reqwest` (with
`rustls-tls`, feature-gated on `summarizer-openai` or
`summarizer-ollama`). For brain-cli, we make `reqwest` a hard
dep — operators always want HTTP. Hand-rolling HTTP/1.1 GET
in 50 LOC is also an option but the parse-headers / chunked-
body details add up; reqwest is the right call.

Use `reqwest::blocking::Client` so the CLI doesn't need a
Tokio runtime. Spec §14/06 says admin ops are operator-facing
and rarely scripted heavily; sync blocking is fine.

### 5.2 Argument parsing
Hand-rolled. The workspace doesn't have `clap` yet; adding it
is a real dep. For 10.8's surface (1 main command + 4 global
flags), 80 LOC of hand-rolled parsing beats pulling clap. If
10.9-10.12 require nested subcommands, we add clap then.

### 5.3 Output formatter
serde_json for JSON. Table format is hand-rolled — render a
`Vec<(String, String)>` as two columns with padding.

### 5.4 Prom-format parser
Server emits Prometheus text format. We don't need a full
parser — line-by-line:
- Lines starting with `#` are comments (HELP, TYPE).
- Other lines: `metric_name[{labels}] value` (optional timestamp ignored).
A 40-LOC parser handles 95% of real-world cases.

---

## 6. Tests

### 6.1 Unit (`cli/args.rs::tests`)
- Default flags.
- `--server foo:7` overrides.
- `--output json|table` valid; unknown errors.
- Unknown subcommand errors.

### 6.2 Unit (`output/json.rs` / `output/table.rs`)
- Stats render: known input → known output.

### 6.3 Unit (`commands/stats.rs::tests::parse_metrics_*`)
- Parse a known Prom-format snippet; assert structure.

### 6.4 Integration (`tests/cli.rs`)
- Spawn a mock HTTP server (tokio::net::TcpListener + hand-rolled
  HTTP/1.1 response — same shape as the SDK's mock).
- Run `health` / `stats` via the command function (not the
  binary) against `http://127.0.0.1:<port>`.
- Assert JSON-mode output matches expectation.

---

## 7. Deferred (later sub-tasks)

- `info` command — server-side endpoint TBD; defer.
- `--token` authentication — server's `/healthz` is unauth.
- TLS — admin endpoint is plain HTTP in dev; 11.x adds TLS.
- Pretty colored output, YAML mode, JSONL streaming — defer.
- All other sub-commands (snapshot, rebuild-ann, worker, …) —
  10.9 / 10.10 / 10.11 / 10.12.
- Subprocess CLI integration test (binary spawn) — 10.13.

---

## 8. Risks

| Risk | Mitigation |
| ---- | ---------- |
| Adding `reqwest` is a new dep that pulls `hyper` etc. | reqwest is already in the workspace (via brain-server's optional summarizer features). For brain-cli we make it a hard dep with `rustls-tls`. |
| Hand-rolled arg parser is brittle | The surface is small. We keep the parser pure and unit-test every flag. 10.9+ may switch to `clap` if the surface grows. |
| Prom-format edge cases (escape sequences in labels, NaN, +Inf, histogram buckets) | 10.8's `stats` outputs raw scalars only; histogram-bucket reconstruction is out of scope. Document. |
| `reqwest::blocking` inside a Tokio test confuses runtimes | Tests use sync paths only; we don't mix. |

---

## 9. Done criteria

- [ ] `src/cli/`, `src/commands/`, `src/output/`, `src/http/`
  folders (no flat files at src/ root besides main.rs).
- [ ] `brain-cli health` works against a live server.
- [ ] `brain-cli stats` works against a live server.
- [ ] `--output json|table` toggles output shape.
- [ ] 4+ unit tests + 1+ integration test (mock HTTP server).
- [ ] All 50 pre-10.8 tests still pass.
- [ ] `just docker-verify` green.
- [ ] Sub-task 10.8 marked `[x]` in the phase doc.

---

*Implement on approval.*
