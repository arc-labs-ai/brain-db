---
name: brain-loom-design
description: Identify concurrency-critical paths needing loom tests; scaffold the loom test. Use for new lock-free code, ArcSwap+epoch usage, or anywhere ordering matters.
when-to-use: |
  Triggers:
    - User says "loom test" / "memory ordering" / "race condition"
    - New lock-free code lands (ArcSwap, AtomicT, crossbeam-epoch usage)
    - Investigating a TSAN-style bug
    - Phase exit checklist for crates that have lock-free code
spec-refs:
  - spec/01_system_architecture/04_layers.md
---

# Loom Design

## When to use

Any code that relies on memory ordering or non-trivial concurrency primitives — not pre-existing patterns from std (`Mutex`, `RwLock`) but the lock-free machinery Brain uses for cross-shard reads:

- `ArcSwap<T>` for atomic pointer swap.
- `crossbeam-epoch` for safe-memory reclamation.
- Per-shard `AtomicUsize` counters where the orderings matter.
- Any custom CAS loop.

`loom` is a CDCL-style model checker: it explores all interleavings of memory operations and surfaces races / deadlocks / orderings violations. It's slow; use it on the *contract*, not the full code path.

## What this enforces

- New lock-free abstractions ship with at least one loom test.
- The loom test models the realistic two-thread scenario (one writer, one reader; or two readers + one writer).
- `loom::sync::Arc`, `loom::sync::atomic::*`, `loom::thread::spawn` replace their `std` / `crossbeam` counterparts inside the test (gated by `cfg(loom)`).

## Workflow

1. **Decide if loom applies.** If the code uses only `Mutex`/`RwLock` (no lock-free primitives), loom adds little. If it uses `Atomic*`, `Arc`, `crossbeam-epoch`, or `ArcSwap` with non-default orderings — loom applies.
2. **Stub the abstraction.** Move the concurrency-critical code into a small struct with a clear API. Loom tests model that struct.
3. **Gate primitives:**

```rust
#[cfg(loom)]
use loom::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
#[cfg(not(loom))]
use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
```

4. **Write the test:**

```rust
#[cfg(loom)]
#[test]
fn ordering_is_correct() {
    loom::model(|| {
        let state = Arc::new(MyAbstraction::new());
        let s2 = state.clone();
        let writer = loom::thread::spawn(move || s2.publish(42));
        let read = state.observe();
        writer.join().unwrap();
        // Assert the post-condition that holds on every interleaving.
        assert!(read == None || read == Some(42));
    });
}
```

5. **Run:** `RUSTFLAGS="--cfg loom" cargo test --test <test> -- --test-threads=1`. Loom is single-threaded by design.
6. **Iterate.** A failing loom test gives a precise interleaving trace; fix the ordering or add a barrier.

## Common patterns to test

| Pattern | What loom catches |
|---|---|
| `ArcSwap` reader sees torn pointer | Acquire/release mismatch |
| Epoch GC reclaims live memory | Pinned-handle violations |
| Single-writer claim | Writer concurrency despite the design |
| Counter under-/over-shoots | Ordering on counter vs. data |

## Anti-patterns

- **Loom test imports std primitives.** The whole point is to substitute loom's. Use the cfg switch.
- **Loom test runs in parallel.** Single-threaded only; loom controls scheduling.
- **Loom test on the full code path.** Too many states; loom OOMs. Stub the abstraction.

## Cross-references

- CLAUDE.md §10 (testing strategy).
- `rust-concurrency` — Send/Sync background.
- `brain-glommio-rules` — shard-side rules; lock-free is for cross-shard.

## Source / Adaptations

Project-local.
