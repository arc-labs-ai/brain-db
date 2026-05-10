---
name: rust-error-handling
description: Error handling discipline — thiserror in libs, anyhow in bins, no unwrap outside tests, expect("invariant:...") for unreachable, Result vs Option vs panic.
when-to-use: |
  Triggers:
    - User says "how should I handle this error?" / "Result or Option?"
    - Adding a new error variant or error type
    - Diff contains `.unwrap()` outside a test, or `Box<dyn Error>` in a library crate
    - User mentions `?`, `panic!`, `anyhow`, `thiserror`, "lost context", or context propagation
    - Designing the error surface for a new public API
spec-refs:
  - spec/03_wire_protocol/10_errors.md
license: MIT
source: https://github.com/actionbook/rust-skills/tree/1f4becdcb88d1cbccc1880594479f28891102843/skills/m06-error-handling
---

# Error Handling

## When to use

Designing or reviewing how a Rust function reports failure. Brain has a strict policy (CLAUDE.md §7):

- **Libraries** (`brain-core`, `brain-protocol`, `brain-storage`, `brain-metadata`, `brain-index`, `brain-embed`, `brain-planner`, `brain-ops`, `brain-workers`, `brain-sdk-rust`): **`thiserror`**, no `anyhow`.
- **Binaries** (`brain-server`, `brain-cli`): **`anyhow`** for ergonomic top-level handling.
- **No `.unwrap()` outside tests.** Use `expect("invariant: <reason>")` only when reaching that line is genuinely impossible.
- The wire-protocol error taxonomy is fixed — see `brain_protocol::error::{ProtocolError, ErrorCode, ErrorCategory}` and spec §03/10.

## Core question

**Is this failure expected, or a bug?**

- Expected → `Result<T, E>`
- Absence is normal → `Option<T>`
- Bug or invariant violation → `panic!` / `assert!` / `unreachable!`
- Unrecoverable → `panic!`

## Workflow

1. Identify what fails. Pick the type from the question above.
2. If `Result`: choose the error type.
   - Library crate → define a `thiserror` enum at the crate boundary.
   - Internal helpers within a library → return the same enum (or a sub-enum) — no `anyhow`.
   - Binary crate → `anyhow::Result` is fine for orchestration code; specific errors at boundaries.
3. Propagate with `?`. Add `.context("...")` (anyhow) or a `From` impl (thiserror) when context is needed.
4. Map to wire if relevant. `ProtocolError → brain_core::Error` exists; if you're adding a new boundary, mirror that pattern.
5. Reject any new `.unwrap()` outside tests. Replace with `?` or `expect("invariant: ...")`.

## Pattern → use

| Pattern | When | Example |
|---------|------|---------|
| `Result<T, E>` | Recoverable error | `fn read() -> Result<String, io::Error>` |
| `Option<T>` | Absence is normal | `fn find() -> Option<&Item>` |
| `?` | Propagate error | `let data = file.read()?;` |
| `expect("invariant: ...")` | Unreachable in this code | `iter.next().expect("invariant: non-empty")` |
| `panic!` | Genuine bug | `panic!("detected drift, see CONTEXT.md")` |
| `unwrap()` | **Tests only** | `result.unwrap()` in `#[test]` |

## Library vs application

| Context | Crate | Why |
|---------|-------|-----|
| Library | `thiserror` | Typed errors for consumers; no `anyhow` |
| Application | `anyhow` | Ergonomic top-level handling |
| Boundary | `thiserror` exposed, `anyhow` internal | Stable error surface for callers |

Brain libraries already follow this — see `brain_core::Error`, `brain_protocol::ProtocolError`. New libraries copy the pattern.

## Decision flowchart

```
Is failure expected?
├─ Yes → Is absence the only "failure"?
│        ├─ Yes → Option<T>
│        └─ No  → Result<T, E>
│                 ├─ Library  → thiserror
│                 └─ Binary   → anyhow
└─ No  → Is it a bug?
        ├─ Yes → panic!, assert!, unreachable!
        └─ No  → Reconsider; rarely truly "no"

Use ? → Need context?
├─ Yes → .context("...") (anyhow) or From impl (thiserror)
└─ No  → bare ?
```

## Common errors

| Error | Cause | Fix |
|-------|-------|-----|
| `unwrap()` panics in CI | Unhandled `None`/`Err` | `?`, `expect("invariant: ...")`, or match |
| Type mismatch on `?` | Different error types | thiserror `From` impl, or anyhow |
| Lost error context | `?` without context | `.context("what was happening")` |
| `cannot use ?` | Missing `Result` return | Return `Result<(), E>` or restructure |

## Anti-patterns

| Anti-Pattern | Why bad | Better |
|--------------|---------|--------|
| `.unwrap()` in library code | Production panic; CLAUDE.md §7 forbids | `?` or `expect("invariant: ...")` |
| Ignore errors silently (`let _ = ...`) | Hides bugs | Handle or propagate |
| `panic!` for expected errors | Bad UX, no recovery | `Result<T, E>` |
| `Box<dyn Error>` in libraries | Lost type info | `thiserror` enum |
| `.expect("")` empty message | Useless on panic | Always `expect("invariant: <why>")` |

## Source / Adaptations

- **Source:** [`actionbook/rust-skills@1f4becd`](https://github.com/actionbook/rust-skills/tree/1f4becdcb88d1cbccc1880594479f28891102843/skills/m06-error-handling)
- **License:** MIT
- **Adaptations:**
  - Renamed `m06-error-handling` → `rust-error-handling`.
  - Specialized the library/application table to Brain's crate split.
  - Pinned the `expect("invariant: ...")` convention from CLAUDE.md §7.
  - Linked spec §03/10 for the wire-protocol error taxonomy.
  - Removed upstream m-* cross-references.
