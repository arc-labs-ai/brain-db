# Fuzz targets

Coverage-guided fuzzing of the wire-protocol decoders via [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) and libFuzzer. Targets live outside the main workspace so they can use nightly-only sanitizer instrumentation without affecting the rest of the build.

## Prerequisites

- **Nightly Rust** — libFuzzer's coverage instrumentation requires nightly. Install once:
  ```
  rustup toolchain install nightly
  ```
- **cargo-fuzz** — installed once:
  ```
  cargo install cargo-fuzz
  ```

## Targets

| Target | Surface | Invariants |
|---|---|---|
| `protocol_frame` | `Frame::decode` | No panic on arbitrary input; on success, the consumed prefix re-encodes to itself. |
| `protocol_request` | `RequestBody::decode` × 27 server-bound opcodes | No panic on arbitrary `(opcode, payload)`. |
| `protocol_response` | `ResponseBody::decode` × 27 client-bound opcodes | No panic on arbitrary `(opcode, payload)`. |

For `protocol_request` / `protocol_response`, the harness uses the first input byte (mod len) to pick the opcode and feeds the remainder as the rkyv payload — this exercises every variant's validation path under libFuzzer's coverage guidance.

## Running

From the project root:

```
cargo +nightly fuzz run protocol_frame    -- -max_total_time=60
cargo +nightly fuzz run protocol_request  -- -max_total_time=60
cargo +nightly fuzz run protocol_response -- -max_total_time=60
```

A 60-second smoke is the per-target acceptance gate (spec §16/06 acceptance criteria; spec §03/11 §7 determinism). For a longer overnight run, drop the `-max_total_time` flag.

## Corpora and artifacts

cargo-fuzz auto-creates `fuzz/corpus/<target>/` and `fuzz/artifacts/<target>/` on first run; both are git-ignored. A regression corpus (curated seeds for known interesting inputs) belongs in Phase 11 alongside CI integration.

## Reproducing a crash

When libFuzzer finds a crash, it writes the offending input to `fuzz/artifacts/<target>/crash-<hash>`. Reproduce locally with:

```
cargo +nightly fuzz run <target> fuzz/artifacts/<target>/crash-<hash>
```

## Spec references

- `spec/03_wire_protocol/11_validation.md` — frame/payload validation rules; §7 pins determinism.
- `spec/16_benchmarks_acceptance/08_acceptance_test_suite.md` — overall fuzzing strategy.

## Companion testing

- Proptests in `crates/brain-protocol/src/frame.rs` cover round-trip and decode-totality on 1024 generated cases per test. The fuzz harness adds coverage-guided exploration of the *failure* paths the proptest doesn't reach.
- See the `brain-fuzz-target` skill (`.claude/skills/brain-fuzz-target/SKILL.md`) for the convention used when adding new targets.
