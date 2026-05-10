---
name: rust-perf
description: Performance discipline for Brain hot paths — measure first, allocate-on-startup, zero-copy reads, SIMD where it matters. No premature optimization.
when-to-use: |
  Triggers:
    - User says "make this faster" / "optimize" / "benchmark"
    - Diff touches a hot path: `crates/brain-storage`, `crates/brain-index`,
      `crates/brain-ops/{encode,recall}.rs`, `brain-protocol::frame`/`request`/`response`
    - Code allocates inside a per-request hot loop
    - Adding `criterion` benches or interpreting their output
    - User mentions flamegraph, perf, profile, regression
trigger-files:
  - crates/brain-storage/**/*.rs
  - crates/brain-index/**/*.rs
  - crates/brain-ops/**/*.rs
  - crates/brain-protocol/src/{frame,request,response}.rs
spec-refs:
  - spec/16_benchmarks_acceptance/02_latency_targets.md
  - spec/01_system_architecture/05_hardware.md
license: MIT
source: https://github.com/actionbook/rust-skills/tree/1f4becdcb88d1cbccc1880594479f28891102843/skills/m10-performance
---

# Performance Optimization

## When to use

Hot-path or budget-bound work. Don't apply this skill to setup code, tests, CLI parsing, or anywhere `cargo bench` doesn't run.

Brain has spec'd latency targets (spec §16/02): single-shard ENCODE p99 ≤ 1 ms, RECALL p99 ≤ 5 ms (top-k=10). Optimization is judged against those numbers — not against feelings.

## Core question

**What's the bottleneck, and is optimization worth it?** Before touching the hot path:

1. **Have you measured?** Profile (`flamegraph`, `perf`, `dhat`) and benchmark (`criterion`). Don't guess.
2. **What's the SLA?** Spec §16/02 has the targets. If we're already inside, stop.
3. **What's the trade-off?** Complexity vs. speed; memory vs. CPU; latency vs. throughput.

## Optimization priority (typical wins)

```
1. Algorithm choice              10x – 1000x
2. Data structure                2x – 10x
3. Allocation reduction          2x – 5x
4. Cache locality                1.5x – 3x
5. SIMD / parallelism            2x – 8x
```

Always start at the top. A `Vec` instead of a `LinkedList` beats hand-rolled SIMD on the wrong shape.

## Brain hot-path rules (from CLAUDE.md §9)

- **Don't allocate in the hot path.** Pre-allocate buffers; use object pools (`Vec::with_capacity`, slot pools, scratch arenas).
- **Don't introduce a thread pool** for parallel work. Sharding is the parallelism.
- **Don't add Tokio inside a shard.** Glommio + thread-per-core; `tokio::*` belongs only at the connection layer.
- **Don't hold a lock across `.await`.**
- **Don't add `Send + Sync`** to per-shard types.
- **Use rkyv + bytemuck for zero copy** on the read path (spec §03/04).

## Workflow

1. **Measure first.** Run `just bench <crate>` or `cargo flamegraph -p <crate> --bench <bench>`. Capture the baseline.
2. **Identify the actual hot spot.** Not the function you suspect — the one the profile shows.
3. **Apply the highest-priority fix that addresses it** (see priority table). Often this is a data-structure change, not a micro-opt.
4. **Re-measure.** If the win is marginal (<5%), revert. Complexity has to pay for itself.
5. **Pin the win.** Add the benchmark as a regression test (spec §16's acceptance criteria).

## Tooling reference

| Tool | Purpose |
|------|---------|
| `cargo bench` / `criterion` | Statistical benchmarks |
| `flamegraph` | CPU profile, sample-based |
| `perf` | Hardware-counter profile (cache misses, branch mispredicts) |
| `heaptrack` / `dhat` | Allocation tracking |
| `cachegrind` | Cache analysis |
| `loom` | Concurrency exploration (correctness, not perf) |

## Common techniques

| Technique | When | How |
|-----------|------|-----|
| Pre-allocation | Known size | `Vec::with_capacity(n)` |
| Object pool | Per-request scratch | Per-shard pool; reset on borrow return |
| Avoid cloning | Hot paths | References or `Cow<T>` |
| Zero-copy read | rkyv structured payloads | `check_archived_root` + deref |
| Zero-copy vector | f32 blobs | `bytemuck::cast_slice<u8, f32>` |
| Batch operations | Many small ops | Collect then process |
| SmallVec | Usually small | `smallvec::SmallVec<[T; N]>` (only if measured win) |
| SIMD | Tight numeric loops | `wide` crate, `matrixmultiply` |

## Common mistakes

| Mistake | Why wrong | Better |
|---------|-----------|--------|
| Optimize without profiling | Wrong target | Profile first |
| Benchmark in debug mode | Meaningless | Always `--release` |
| Use `LinkedList` | Cache-unfriendly | `Vec` or `VecDeque` |
| Hidden `.clone()` | Unnecessary allocs | References |
| Premature optimization | Wasted effort | Make it correct first |
| Optimize the cold path | Wasted effort | Measure first |

## Anti-patterns

| Anti-Pattern | Why bad | Better |
|--------------|---------|--------|
| Clone to avoid lifetimes | Allocation cost | Proper ownership |
| Box everything | Indirection cost | Stack when possible |
| HashMap for tiny sets | Overhead | Vec with linear search (≤16 items) |
| String concat in loop | O(n²) | `String::with_capacity` |
| Async in CPU-bound shard work | Executor overhead | Plain function or rayon (NOT inside shard) |

## Source / Adaptations

- **Source:** [`actionbook/rust-skills@1f4becd`](https://github.com/actionbook/rust-skills/tree/1f4becdcb88d1cbccc1880594479f28891102843/skills/m10-performance)
- **License:** MIT
- **Adaptations:**
  - Renamed `m10-performance` → `rust-perf`.
  - Layered Brain hot-path rules (CLAUDE.md §9) on top of the generic technique list.
  - Linked the spec's latency targets (`16/02`) so optimization is bounded by spec, not vibes.
  - Removed the upstream "Trace Up / Trace Down" graph references.
  - Added the "Re-measure; revert if marginal" step to the workflow.
