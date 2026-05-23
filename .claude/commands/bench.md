---
description: Run benchmarks for a specific crate or operation
argument-hint: [crate-name | benchmark-name]
allowed-tools: Bash(cargo bench:*), Read
---

Run benchmarks. Argument: `$ARGUMENTS`

If `$ARGUMENTS` is empty, list all benchmark targets in the workspace and ask the user which one(s) to run.

If `$ARGUMENTS` is a crate name (e.g. `brain-storage`):
```
cargo bench -p $ARGUMENTS
```

If `$ARGUMENTS` is a benchmark name (e.g. `wal_append`):
```
cargo bench --bench $ARGUMENTS
```

Report:

1. **Summary table** of each benchmark's median time and any change vs the previous baseline (criterion handles the comparison automatically).
2. **Regressions** (>5% slower than baseline) — flag prominently.
3. **Improvements** (>5% faster) — note these, since they often indicate a real change worth understanding.

Compare results against the targets in `spec/20_benchmarks/02_latency_targets.md` and `spec/20_benchmarks/03_throughput_targets.md`. If a benchmark is below target, flag it.

After running:
- Save the results to `bench-results/$(date +%Y-%m-%d)-$ARGUMENTS.txt` if requested.
- Don't auto-commit benchmark results.
