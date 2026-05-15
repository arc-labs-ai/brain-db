# 10 — Running tests

The Brain workspace has unit tests, integration tests, e2e tests,
benchmarks, and the acceptance gate runner. This page covers each.

## 1. Full workspace tests

**Input:**

```bash
just docker-test
```

Or scoped:

```bash
just docker-test --workspace
```

**Expected output:**

```
Compiling brain-core v0.1.0
...
test result: ok. NN passed; 0 failed; 0 ignored

[many crates] ...

Finished `dev` profile in X.Xs
```

**Verify:**

Exit code 0; every per-crate `test result:` line ends in
`0 failed`.

## 2. Single crate

**Input:**

```bash
just docker-test -p brain-protocol
just docker-test -p brain-storage
just docker-test -p brain-server
```

**Verify:**

Each prints its own `test result: ok. N passed; 0 failed`.

## 3. Single integration test

The brain-server crate has named integration suites in `tests/`:

```bash
# admin HTTP server
just docker-test -p brain-server --test admin

# wire-protocol e2e (real server, framed TCP)
just docker-test -p brain-server --test e2e

# SDK round-trips
just docker-test -p brain-server --test sdk_e2e

# CLI lib-level e2e
just docker-test -p brain-server --test cli_e2e

# dispatch + subscribe
just docker-test -p brain-server --test dispatch --test subscribe

# Phase 12.4 dashboard validator
just docker-test -p brain-server --test dashboards

# Phase 12.5 alert rules validator
just docker-test -p brain-server --test alerts
```

These require Linux (io_uring) and report zero tests on macOS —
that's expected. Always run via the container.

## 4. Single test function

```bash
just docker cargo test -p brain-storage --lib -- arena::tests::crc_mismatch_halts --nocapture
```

```bash
just docker cargo test -p brain-server --test sdk_e2e -- sdk_encode_recall_roundtrip --nocapture
```

`--nocapture` shows print output suppressed by default.

## 5. Chaos tests

Phase 13.3 ships three storage-layer chaos suites:

```bash
just docker-test -p brain-storage --test random_kill
just docker-test -p brain-storage --test bit_flip
just docker-test -p brain-storage --test io_fault
```

`random_kill` has a `#[ignore]`-gated full sweep (1000 iterations,
~3 minutes); the smoke version (100 iters) runs by default:

```bash
just docker-test -p brain-storage --test random_kill -- --ignored
```

**Verify:**

All three suites exit `0 failed`. Each chaos scenario asserts
spec §15/07 "no silent corruption" — recovery must error or stop
at the bad record, never return a corrupted record as valid.

## 6. Miri

Memory-safety check on `brain-storage`'s `unsafe` blocks:

```bash
just miri
```

Syscall-bound paths (mmap, pwritev2) are excluded via
`#[cfg(miri)]` gates; the ~47 pure-data tests run.

**Verify:**

Exit 0 = no UB found. Failures here are real soundness bugs —
surface immediately, don't merge.

## 7. Benchmarks

Per-crate criterion benches:

```bash
cargo bench -p brain-index           # recall, insert
cargo bench -p brain-storage         # crc32c
cargo bench -p brain-protocol        # frame codec
cargo bench -p brain-embed           # throughput
cargo bench -p brain-http            # router, sse_encoder, end_to_end
```

**Compile-only (CI):**

```bash
cargo bench --workspace --no-run
```

**Verify:**

Each bench prints criterion's standard summary block (mean,
std-dev, min, max, throughput). Spec §16/13 expects ±10 %
run-to-run variance; ±30 % indicates instability.

Full performance baselines are run on quiet reference hardware
per spec §16/07. See [`docs/performance/README.md`](../performance/README.md).

## 8. Load generator (operator-run)

```bash
cargo run --release --example load_generator -p brain-sdk-rust -- \
    --addr 127.0.0.1:9090 \
    --rate 1000 \
    --duration 60s \
    --warmup 5s \
    --mix encode=25,recall=70,link=5
```

**Expected output (CSV):**

```
window_unix,op,count,errors,p50_ms,p95_ms,p99_ms,p999_ms,mean_ms
1747300000,encode,2500,0,7.1,12.4,21.5,38.2,8.6
1747300000,recall,7000,0,4.8,11.2,18.4,33.0,5.9
1747300000,link,500,0,1.9,4.4,8.1,16.0,2.4
```

**Verify:**

`errors` column is `0` per op; `p99_ms` values are within an
order of magnitude of spec §16/02 targets (ENCODE 25 ms,
RECALL 20 ms).

## 9. Soak rig (operator-run, 48 h)

```bash
cargo run --release --example soak -p brain-sdk-rust -- \
    --data-addr 127.0.0.1:9090 \
    --metrics-addr 127.0.0.1:9091 \
    --duration 48h \
    --warmup 5m \
    --rate 500 \
    --sample-interval 60s
```

Emits per-sample CSV and a final `SOAK_RESULT pass=... ...` line.
See [`docs/performance/README.md`](../performance/README.md) for
the methodology.

## 10. Acceptance suite

Spec §16/08's 10 release gates:

```bash
bash scripts/acceptance/run.sh
```

Selected subsets:

```bash
bash scripts/acceptance/run.sh 1 2 3 4
bash scripts/acceptance/run.sh --skip 5 7
```

**Expected output:**

```
── Gate 1: Unit tests ──
test result: ok. N passed; 0 failed
gate 1 PASS

── Gate 2: Integration tests ──
...
gate 2 PASS

...

── Summary ──
  Passed:  10
  Failed:  0
  Skipped: 0
  Report:  scripts/acceptance/last-run.jsonl
ACCEPTANCE: PASS (10/10 gates green)
```

**Verify:**

Exit 0 = release-ready (CI gates). The JSONL report carries one
record per gate for archival. Operator-side gates (5 full bench,
7 soak, 6/8 full chaos, 10 overnight fuzz) need reference
hardware; CI runs the smoke versions.

See [`scripts/acceptance/README.md`](../../scripts/acceptance/README.md)
for the per-gate table.

## Test cadence reference

| Cadence | What | Wall clock |
|---|---|---|
| Per save / dev loop | `just docker-test -p <crate>` for the crate you touched | < 30 s |
| Pre-commit | `just docker-verify` | 1–3 min |
| Per PR (CI) | gates 1, 2, 3, 4, 6 (storage), 8 (TLS path), 9, 10 (smoke) | 15–30 min |
| Per merge to main (CI) | + full e2e + light bench | 1–2 h |
| Nightly (operator) | + full chaos + soak (compile only) | 6–12 h |
| Pre-release (operator) | + 48 h soak on reference hardware | 24–48 h |

## What's next

That's the full local-development surface. For production-side
docs (systemd, scraping, runbooks, release-cut), see
[`docs/guides/`](../guides/) and [`docs/runbooks/`](../runbooks/).
