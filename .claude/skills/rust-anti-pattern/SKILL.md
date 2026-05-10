---
name: rust-anti-pattern
description: Catch and refactor common Rust anti-patterns — unwrap-in-prod, clone-everywhere, fighting the borrow checker, locks across .await, Send+Sync on per-shard types.
when-to-use: |
  Triggers:
    - User says "review this code" / "is this idiomatic?" / "anti-pattern?"
    - Code contains repeated `.clone()`, `.unwrap()` outside tests, `Rc<T>` in async,
      `Box<dyn Error>` everywhere, or holding a lock across `.await`
    - Adding `Send + Sync` to a type that lives inside a single shard
    - User asks "should I refactor?" or "fighting the borrow checker"
spec-refs:
  - spec/01_system_architecture/04_layers.md
license: MIT
source: https://github.com/actionbook/rust-skills/tree/1f4becdcb88d1cbccc1880594479f28891102843/skills/m15-anti-pattern
---

# Anti-Patterns

## When to use

Reviewing diffs or fresh code for hidden design problems — not just "make clippy happy", but "is this solving the symptom or the cause?"

## Core question

**Is this pattern hiding a design problem?** Before pushing a fix, ask:

- Is this addressing the symptom or the cause?
- Is there a more idiomatic approach?
- Does this fight or flow with Rust?

## Anti-Pattern → better

| Anti-Pattern | Why bad | Better |
|--------------|---------|--------|
| `.clone()` everywhere | Hides ownership issues | Proper references or ownership |
| `.unwrap()` in production | Runtime panics — banned per CLAUDE.md §7 | `?`, `expect("invariant: ...")`, or handling |
| `Rc<T>` when single owner | Unnecessary overhead | Simple ownership |
| `unsafe` for convenience | UB risk; banned outside `brain-storage` | Find safe pattern (or STOP and surface) |
| OOP via `Deref` | Misleading API | Composition, traits |
| Giant match arms | Unmaintainable | Extract to methods |
| `String` everywhere | Allocation waste in hot paths | `&str`, `Cow<str>` |
| Ignoring `#[must_use]` | Lost errors | Handle or `let _ =` |
| `Send + Sync` on per-shard types | Defeats single-writer model | Keep them `!Send` (CLAUDE.md §9) |
| Holding `Mutex` guard across `.await` | Blocks the executor task | Scope the lock; drop before await |

## Workflow

1. Run `grep -nE '\.clone\(\)|\.unwrap\(\)|Rc::|Arc<Mutex' <files>`.
2. For each hit, ask the "symptom or cause" question above.
3. If cause: propose the structural fix (ownership change, error strategy, type redesign).
4. If symptom is genuinely best (e.g., `unwrap()` in a test): leave a comment explaining why.

## Top 5 mistakes (Brain-specific)

| Rank | Mistake | Fix |
|------|---------|-----|
| 1 | `.clone()` to escape borrow checker | Use references; restructure data flow |
| 2 | `.unwrap()` in library code | `?` for propagation, `expect("invariant: ...")` for unreachable |
| 3 | `Send + Sync` on a type that lives inside a shard | Keep `!Send`; use crossbeam/`ArcSwap` for cross-shard |
| 4 | Mutex across `.await` in connection-layer Tokio code | Scope the lock; drop before any `.await` |
| 5 | `Box<dyn Error>` in `brain-protocol` | Use `ProtocolError` or `brain_core::Error` (CLAUDE.md §7) |

## Code smell → refactoring

| Smell | Indicates | Refactoring |
|-------|-----------|-------------|
| Many `.clone()` | Ownership unclear | Clarify data flow |
| Many `.unwrap()` | Error handling missing | Add proper handling |
| Many `pub` fields | Encapsulation broken | Private + accessors |
| Deep nesting | Complex logic | Extract methods |
| Long functions | Multiple responsibilities | Split |
| Giant enums | Missing abstraction | Trait + types |

## Quick review checklist

- [ ] No `.clone()` without justification
- [ ] No `.unwrap()` in library code (tests OK)
- [ ] No `pub` fields with invariants
- [ ] No index loops when iterator works
- [ ] No `String` where `&str` suffices in a hot path
- [ ] No ignored `#[must_use]` warnings
- [ ] No `unsafe` outside `brain-storage`
- [ ] No giant functions (>50 lines without justification)
- [ ] No locks held across `.await`

## Source / Adaptations

- **Source:** [`actionbook/rust-skills@1f4becd`](https://github.com/actionbook/rust-skills/tree/1f4becdcb88d1cbccc1880594479f28891102843/skills/m15-anti-pattern)
- **License:** MIT
- **Adaptations:**
  - Renamed `m15-anti-pattern` → `rust-anti-pattern` (project naming).
  - Added Brain-specific top-5 anti-patterns (single-writer, Tokio/Glommio split, locks-across-await, ProtocolError boundary).
  - Cross-linked CLAUDE.md §7 and §9.
  - Removed the upstream "Trace Up / Trace Down" cross-references to other m-* skills we didn't vendor.
  - Added an explicit Workflow section.
