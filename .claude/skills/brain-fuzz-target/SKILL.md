---
name: brain-fuzz-target
description: Design and scaffold a cargo-fuzz target for a new wire surface (Frame, RequestBody, ResponseBody, handshake). Adds fuzz/fuzz_targets/<name>.rs and a 60s smoke run.
when-to-use: |
  Triggers:
    - User says "add a fuzz target" / "fuzz this"
    - New parser or wire surface lands in brain-protocol
    - Investigating a panic-on-input bug
    - Phase exit checklist for brain-protocol
spec-refs:
  - spec/03_wire_protocol/11_validation.md
---

# Fuzz Target Scaffold

## When to use

Adding a fuzz target for a new wire-protocol parser. Brain has a `fuzz/` workspace from Phase 0; this skill drops a new target into `fuzz/fuzz_targets/<name>.rs` and registers it in `fuzz/Cargo.toml`.

Fuzzing requires **nightly Rust**; CI gates the fuzz job behind a nightly-only matrix entry.

## What this enforces

- Every parser exposed by `brain-protocol` has a fuzz target.
- Targets accept arbitrary `&[u8]` and exercise the parser in panic-free mode.
- Targets verify the round-trip property where applicable: `decode → encode → decode` produces identical output (no panics, errors are structured `ProtocolError`).
- A 60-second `cargo +nightly fuzz run <target> -- -max_total_time=60` finds no panics on a fresh checkout.

## Workflow

1. **Pick the target.** One per public parser:
   - `protocol_frame` — `Frame::decode`
   - `protocol_request` — `RequestBody::decode` (cycles through opcodes)
   - `protocol_response` — `ResponseBody::decode` (cycles through opcodes)
   - `protocol_handshake` — `HelloPayload`, `WelcomePayload`, `AuthPayload`, `AuthOkPayload`
2. **Write `fuzz/fuzz_targets/<name>.rs`:**

```rust
#![no_main]

use libfuzzer_sys::fuzz_target;
use brain_protocol::{Frame, RequestBody, ResponseBody, Opcode};

fuzz_target!(|data: &[u8]| {
    // Parse may fail; that's fine. We're proving it never panics.
    if let Ok((frame, _rest)) = Frame::decode(data) {
        // If parse succeeds, re-encode must yield bytes that re-parse.
        let _ = Frame::decode(&frame.encode());
    }
});
```

3. **Register in `fuzz/Cargo.toml`:**

```toml
[[bin]]
name = "<name>"
path = "fuzz_targets/<name>.rs"
test = false
doc = false
```

4. **60-second smoke:**

```bash
cargo +nightly fuzz run <name> -- -max_total_time=60
```

5. **CI integration.** Phase 11 wires nightly fuzz; for now the target's existence + smoke is enough.

## Anti-patterns

- **Target panics on first input.** The parser was supposed to return `Result`. Find the panic; add the missing bounds check.
- **Target only feeds well-formed input.** Defeats the point. Pass through `Arbitrary` for random construction.
- **Target ignores round-trip.** A fuzzer that only checks "no panic" misses half the bugs. If decode succeeds, re-encoding should match.

## Cross-references

- `brain-loom-design` — for concurrency-bug exploration (different tool).
- spec §03/11 — frame validation rules.

## Source / Adaptations

Project-local. Operationalizes Task 1.10.
