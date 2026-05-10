# Plan: Phase 1 — Task 1.10, Wire Up the Fuzz Target

**Status:** approved (implemented)
**Date:** 2026-05-10
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Replace the Phase 0 placeholder fuzz target with real harnesses for the wire codec:

- `fuzz/fuzz_targets/protocol_frame.rs` — `Frame::decode` on arbitrary bytes; if decode succeeds, re-encoding must reproduce the consumed prefix.
- `fuzz/fuzz_targets/protocol_request.rs` — `RequestBody::decode` cycled across every server-bound opcode.
- `fuzz/fuzz_targets/protocol_response.rs` — `ResponseBody::decode` cycled across every client-bound opcode.

Update `fuzz/Cargo.toml` to register the new bin targets. Verify each builds and runs cleanly for 60 seconds (`cargo +nightly fuzz run <target> -- -max_total_time=60`).

**Out of scope:**

- **Structure-aware fuzzing** via `arbitrary::Arbitrary` derives on internal types. Raw-bytes fuzzing is the right shape for parsers that accept `&[u8]`; structured fuzzing belongs at higher layers (e.g., operation-level invariants in Phase 7+).
- **CI integration of the nightly fuzz job.** Phase 11 owns CI extensions; for now we ship the harnesses and the documented `cargo +nightly fuzz run` command. A note goes in `fuzz/README.md`.
- **Persistent corpora / regression seeds.** Not needed yet; cargo-fuzz auto-creates a corpus per target.
- **Sanitizer profile beyond default.** libfuzzer-sys defaults to AddressSanitizer + libFuzzer; that's enough for v1.

## 2. Spec references

- `spec/03_wire_protocol/11_validation.md` — particularly §1 (layered validation) and §7 (validation determinism). Quote:
  > **Validation MUST be deterministic for a given input.** The same payload, against the same configuration, MUST always produce the same accept/reject decision.
  Fuzz harnesses pin this property — same bytes always yield the same result; no panics.
- Existing test coverage:
  - `frame::tests::decode_arbitrary_bytes_is_total` — proptest at 1024 cases. Fuzz extends this to libFuzzer's coverage-guided exploration.
  - `request::tests::decode_garbage_returns_malformed` and `response::tests::decode_garbage_returns_malformed` — single-shot garbage tests.

## 3. External validation

Web-searched (May 2026):

- **cargo-fuzz** — [rust-fuzz/cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz). Uses libFuzzer; requires nightly Rust because libFuzzer's instrumentation needs nightly-only flags. Already pinned in `fuzz/Cargo.toml` (libfuzzer-sys 0.4).
- **Rust Fuzz Book** — [rust-fuzz.github.io/book/cargo-fuzz.html](https://rust-fuzz.github.io/book/cargo-fuzz.html). Confirms standard `fuzz_target!(|data: &[u8]| { ... })` pattern; one harness per logical target; stateless execution between runs.
- **Trail of Bits Testing Handbook** — [appsec.guide/docs/fuzzing/rust/cargo-fuzz](https://appsec.guide/docs/fuzzing/rust/cargo-fuzz/). Recommends:
  - Stateless harnesses (no `static mut` accumulating state).
  - Verify invariants after the call (we re-encode and compare for `Frame`).
  - One harness per logical surface (we ship three: frame, request, response).
- **`arbitrary` crate** — [rust-fuzz/arbitrary](https://github.com/rust-fuzz/arbitrary). Structure-aware option; we explicitly defer (see §1 out-of-scope).

No version bumps, no API surprises — `libfuzzer-sys 0.4` (already in our `fuzz/Cargo.toml`) is current.

## 4. Architecture sketch

```text
fuzz/
├── Cargo.toml                 (extend with two new [[bin]] entries)
├── README.md                  (extend with per-target run commands)
├── fuzz_targets/
│   ├── protocol_frame.rs      replace placeholder
│   ├── protocol_request.rs    new
│   └── protocol_response.rs   new
└── corpus/, artifacts/        auto-created by cargo-fuzz; .gitignore'd
```

### Harness shapes

```rust
// fuzz/fuzz_targets/protocol_frame.rs
#![no_main]

use libfuzzer_sys::fuzz_target;
use brain_protocol::Frame;

fuzz_target!(|data: &[u8]| {
    if let Ok((frame, rest)) = Frame::decode(data) {
        // Round-trip invariant: the consumed prefix must re-encode to itself.
        let consumed = data.len() - rest.len();
        let reencoded = frame.encode();
        assert_eq!(reencoded.as_slice(), &data[..consumed]);
    }
});
```

```rust
// fuzz/fuzz_targets/protocol_request.rs
#![no_main]

use libfuzzer_sys::fuzz_target;
use brain_protocol::{Opcode, RequestBody};

const OPCODES: &[Opcode] = &[
    Opcode::Hello, Opcode::Auth,
    Opcode::EncodeReq, Opcode::EncodeVectorDirectReq,
    Opcode::RecallReq, Opcode::PlanReq, Opcode::ReasonReq, Opcode::ForgetReq,
    Opcode::SubscribeReq, Opcode::UnsubscribeReq,
    Opcode::TxnBegin, Opcode::TxnCommit, Opcode::TxnAbort,
    Opcode::CancelStream, Opcode::Ping, Opcode::ClientPong, Opcode::Bye,
    Opcode::AdminStatsReq, Opcode::AdminSnapshotReq, Opcode::AdminRestoreReq,
    Opcode::AdminIntegrityCheckReq, Opcode::AdminMigrateEmbeddingsReq,
    Opcode::AdminCreateContextReq, Opcode::AdminRenameContextReq,
    Opcode::AdminMoveMemoryReq, Opcode::AdminReclassifyReq,
    Opcode::AdminListTombstonedReq,
];

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // Use the first byte to pick an opcode (mod len); rest is the payload.
    let op = OPCODES[(data[0] as usize) % OPCODES.len()];
    let _ = RequestBody::decode(op, &data[1..]);
});
```

`protocol_response.rs` follows the same shape for `ResponseBody::decode` over the 25 client-bound opcodes plus `Opcode::Error`.

### Cargo registration

```toml
# fuzz/Cargo.toml additions
[[bin]]
name = "protocol_request"
path = "fuzz_targets/protocol_request.rs"
test = false
doc = false
bench = false

[[bin]]
name = "protocol_response"
path = "fuzz_targets/protocol_response.rs"
test = false
doc = false
bench = false
```

## 5. Trade-offs considered

| Alternative | Verdict |
|---|---|
| **Chosen:** three harnesses (frame, request, response); raw `&[u8]`; round-trip assertion in `protocol_frame` only. | ✓ Matches the three public parser surfaces; keeps per-iteration cost low; round-trip only where it's cheap to verify. |
| Single `protocol` harness that tries all three layers per input. | rejected — opaque failures (which layer panicked?), slower iteration. |
| `arbitrary::Arbitrary` derives for structured fuzzing of `RequestBody`. | rejected for v1 — needs `arbitrary` derives on every wire-domain type plus the helper enums. Adds a feature flag, surface area, and complexity. Round-tripping a generated value through encode→decode is what proptests already cover (1024 cases). LibFuzzer's coverage-guided raw-bytes fuzzing adds a different signal (catches mutations that hit cold paths in the parser). |
| Round-trip assertion in `protocol_request` / `protocol_response`. | rejected — the request/response decode rejects most random bytes; the small subset that decodes successfully is already covered by request/response unit tests. Skipping the round-trip keeps the harness simple. |
| Use `Opcode::from_u8(data[0])` instead of mod-len indexing. | rejected — most random bytes are unassigned opcodes; the harness would skip 80% of inputs. Mod-len cycles through valid opcodes uniformly. |

## 6. Risks / open questions

- **Nightly-only.** cargo-fuzz needs nightly Rust (libFuzzer instrumentation). Local devs without nightly can't run fuzz; CI similarly. Mitigation: document the nightly requirement in `fuzz/README.md`; phase-11 wires a separate nightly job.
- **Smoke-run depends on host.** A 60-second run on a fast laptop covers more inputs than a slow one. The "no panics" assertion is host-independent; the *quality* of the smoke isn't. Acceptable for v1.
- **No persistent corpus.** cargo-fuzz creates `fuzz/corpus/<target>/` automatically. Re-running the smoke benefits from prior cases. We `.gitignore` it. Phase 11 may track a curated regression corpus.
- **Coverage gaps.** Random bytes rarely produce a valid header CRC, so most inputs fail at `Header::validate`. The proptest already covers the "succeed → round-trip" path with 1024 high-quality cases; libFuzzer adds coverage-guided exploration of the *failure* paths (bad lengths, weird flag combos, oversize claims).
- **AddressSanitizer baseline.** libfuzzer-sys 0.4 enables ASan by default. Brain has `unsafe` only inside `brain-storage` (not yet wired into `brain-protocol`); no immediate UB expected, but the harness *should* surface it if introduced.

## 7. Test plan

Per phase-doc Done-when:

- **Fuzz harness builds.**
  Maps to: `cargo +nightly fuzz build` (or per-target `cargo +nightly fuzz build protocol_frame`).
- **60-second run finds no panics.**
  Maps to: `cargo +nightly fuzz run <target> -- -max_total_time=60` for each of the three targets. Smoke runs included in the commit message; not in CI yet (phase 11).

Plus: confirm the existing `cargo test --workspace --all-targets` and `cargo clippy --workspace -- -D warnings` are unaffected (the `fuzz/` workspace is independent — `[workspace]` empty in `fuzz/Cargo.toml`).

## 8. Commit shape

One commit:

> `1.10: wire up cargo-fuzz harnesses for protocol decoders`

Includes:

1. Replace `fuzz/fuzz_targets/protocol_frame.rs` placeholder with the real `Frame::decode` harness (with re-encode round-trip).
2. Add `fuzz/fuzz_targets/protocol_request.rs`.
3. Add `fuzz/fuzz_targets/protocol_response.rs`.
4. Extend `fuzz/Cargo.toml` with two new `[[bin]]` entries.
5. Update `fuzz/README.md` with per-target run commands and the nightly note.
6. Mark Task 1.10 `[x]` in the phase doc.

Estimated diff: ~150 lines (most are test scaffolding; harness bodies are small).

## 9. Verification approach

Smoke commands I'll run before commit (capturing output in the commit message):

```bash
# Per-target build (fast, validates compilation):
cargo +nightly fuzz build protocol_frame
cargo +nightly fuzz build protocol_request
cargo +nightly fuzz build protocol_response

# 60-second smoke per target:
cargo +nightly fuzz run protocol_frame    -- -max_total_time=60
cargo +nightly fuzz run protocol_request  -- -max_total_time=60
cargo +nightly fuzz run protocol_response -- -max_total_time=60
```

If any target panics: STOP and surface (this would be a real bug surfacing through the fuzzer). Don't autofix without user input.

If the host doesn't have nightly Rust available: install via `rustup toolchain install nightly` (one-time), or ship the harness without smoke and document in the commit that smoke was deferred.

## 10. Confirmation

Awaiting "go" / "approved" / specific revisions.

---

## Appendix A — Sources

- [rust-fuzz/cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz)
- [libfuzzer-sys](https://crates.io/crates/libfuzzer-sys)
- [Rust Fuzz Book — cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html)
- [Rust Fuzz Book — Structure-aware fuzzing](https://rust-fuzz.github.io/book/cargo-fuzz/structure-aware-fuzzing.html)
- [Trail of Bits Testing Handbook — cargo-fuzz](https://appsec.guide/docs/fuzzing/rust/cargo-fuzz/)
- [Trail of Bits Testing Handbook — Writing harnesses](https://appsec.guide/docs/fuzzing/rust/techniques/writing-harnesses/)
- [rust-fuzz/arbitrary](https://github.com/rust-fuzz/arbitrary)
