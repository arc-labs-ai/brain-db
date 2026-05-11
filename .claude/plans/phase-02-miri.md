# Phase 2 follow-up: Miri on `brain-storage`

**Classification:** light. The infrastructure (nightly + miri component) is already in the dev container. The work is: identify which tests miri can actually run, gate the rest, run it, document.

**Spec / phase doc reference:** Phase 2 exit checklist item: "Miri passes on `brain-storage` (requires nightly): `cargo +nightly miri test -p brain-storage`."

## 1. What miri actually catches in `brain-storage`

Miri interprets MIR and detects UB: out-of-bounds reads, use-after-free, data races, alignment violations, uninitialized reads, strict-provenance violations, and (with `-Zmiri-tree-borrows`) aliasing violations. Most of our `unsafe` blocks fall into categories miri *can't simulate*:

| `unsafe` site | Why miri can't run it |
|---|---|
| `libc::mmap` / `mremap` / `munmap` / `fallocate` / `msync` / `madvise` / `pwritev2` (`arena/file.rs`, `wal/segment.rs`) | Miri doesn't shim these syscalls. The interpreter aborts with "unsupported foreign item". |
| Hand-rolled mmap pointer arithmetic that follows an `mmap` call (`ArenaFile::slot` / `slot_mut`) | Reachable only after `mmap`, so transitively unreachable under miri. |

What miri *can* exercise:

| `unsafe` site | Coverage value |
|---|---|
| `bytemuck::Pod` derive for `Slot` / `SlotMeta` / `HeaderRaw` / `WalSegmentHeaderRaw` | Catches implicit padding (would be UB to read as bytes). High value — we rely on `const _: () = { assert_eq!(size_of::<...>(), ...) };` blocks, but those don't catch *padding inside* a same-sized struct. |
| `WalRecord::encode_into` / `decode_one` byte arithmetic | Pure Rust; mostly bounds-checked. Miri sanity-checks the few `expect` / `try_into` paths. |
| `WalPayload::encode_to_bytes` / `decode` | Same as above. |

Verdict: miri's coverage of `brain-storage` is narrow (~30 tests of 155) but the parts it does cover are exactly the ones where a silent UB bug would be hardest to catch (Pod-derived structs with implicit padding leaking uninit bytes).

## 2. Approach

1. **Gate syscall-bound tests with `#[cfg_attr(miri, ignore)]`.** Miri runs them but skips with a note in the output. Doesn't require splitting the test files.
2. **Run `cargo +nightly miri test -p brain-storage --lib`.** `--lib` confines miri to the lib's `#[cfg(test)]` tests; the integration test (`tests/random_kill.rs`) is entirely syscall-bound and adds no coverage under miri. (We `--ignore` the whole binary instead of decorating every test.)
3. **Verify miri-safe tests pass.** Document the count.
4. **Add a `just miri` recipe** so the command is one-word reproducible.
5. **Update `docs/spec-deviations.md`** (no — this isn't a deviation, it's a coverage scope decision) — instead add a short section in the phase-2 doc explaining what's miri-covered vs not.
6. **Phase exit checklist**: mark the Miri item satisfied with a footnote linking to the scope document.

## 3. Gating strategy

For each `#[test]` that opens a file, calls `Wal::create`, opens an `ArenaFile`, or spawns the `GroupCommitter` thread, add `#[cfg_attr(miri, ignore)]`. Inventory:

| Module | Total tests | Miri-safe | Miri-gated |
|---|---|---|---|
| `wal/kinds.rs` | 4 | 4 | 0 |
| `wal/record.rs` | 13 | 13 | 0 |
| `wal/payload.rs` | 11 | 11 | 0 |
| `arena/slot.rs` | 13 | 13 | 0 |
| `arena/file.rs` | 19 | 0 | 19 |
| `arena/allocator.rs` | 18 | 0 | 18 |
| `wal/segment.rs` | 8 | 0 | 8 |
| `wal/reader.rs` | 15 | 0 | 15 |
| `wal/group_commit.rs` | 10 | 0 | 10 |
| `wal/wal.rs` | 12 | 0 | 12 |
| `wal/checkpoint.rs` | 10 | 0 | 10 |
| `recovery.rs` | 13 | 0 | 13 |
| **lib total** | **146** | **41** | **105** |
| `tests/random_kill.rs` (integration) | 4 (1 ignored) | 0 | 4 (whole binary skipped) |

Miri runs **41 tests** end-to-end. (Plus 9 `lib.rs` top-level tests like `slot_size_is_1600` if any — checking inventory below.)

Counts above are approximate from grepping `#[test]`. The actual run will report the real numbers.

### Bulk-gating approach

Rather than annotating every individual test, I'll add `#[cfg_attr(miri, ignore)]` at the **test-module level** in syscall-heavy files. Rust applies module-level attributes to all contained `#[test]` functions.

Actually — `#[cfg_attr(miri, ignore)]` on a `mod tests` block doesn't propagate to inner `#[test]`s; the `ignore` attribute is per-test. So the choice is:

- (A) Annotate each test individually (verbose; ~105 attributes added).
- (B) Use a helper: at the top of every miri-gated test module, do `#![cfg_attr(miri, allow(dead_code))]` and have each `#[test]` start with `if cfg!(miri) { return; }`.
- (C) Move syscall-bound tests into a separate `#[cfg(not(miri))] mod ...` block.

Best is (C): wrap each problem test module in `#[cfg(not(miri))]`. One annotation per file; clean.

Concretely:

```rust
// Before:
#[cfg(test)]
mod tests {
    // ... 19 tests using ArenaFile ...
}

// After:
#[cfg(all(test, not(miri)))]
mod tests {
    // ... unchanged ...
}
```

One line changed per file. Five files affected (`arena/file.rs`, `arena/allocator.rs`, `wal/segment.rs`, `wal/reader.rs`, `wal/group_commit.rs`, `wal/wal.rs`, `wal/checkpoint.rs`, `recovery.rs` — that's 8).

For mixed-content files (`wal/record.rs` has miri-safe tests; but its test module is purely byte-level → leave un-gated). Same for `wal/payload.rs`, `wal/kinds.rs`, `arena/slot.rs`.

### Side effect on `cargo test`

Native (non-miri) builds: `cfg(not(miri))` is true → tests run as today. **Zero regression.**

Miri builds: those modules are simply absent. Miri reports "0 tests" from each, and the 41 miri-safe tests run.

## 4. Files touched

- `crates/brain-storage/src/arena/file.rs` — change `#[cfg(test)]` → `#[cfg(all(test, not(miri)))]` on the test module.
- `crates/brain-storage/src/arena/allocator.rs` — same.
- `crates/brain-storage/src/wal/segment.rs` — same.
- `crates/brain-storage/src/wal/reader.rs` — same.
- `crates/brain-storage/src/wal/group_commit.rs` — same.
- `crates/brain-storage/src/wal/wal.rs` — same.
- `crates/brain-storage/src/wal/checkpoint.rs` — same.
- `crates/brain-storage/src/recovery.rs` — same.
- `justfile` — add `miri` recipe.
- `docs/phases/phase-02-storage.md` — note miri coverage in the phase-exit notes.

No source-logic changes. Pure scope-gating.

## 5. The `justfile` recipe

```justfile
# Run miri against brain-storage's miri-safe tests.
# Excludes syscall-bound tests (mmap/pwritev2/etc. aren't shimmed by miri).
miri:
    @docker run --rm \
        -v "$(pwd)":/workspaces/brain \
        -v brain-cargo-registry:/usr/local/cargo/registry \
        -v brain-cargo-git:/usr/local/cargo/git \
        -v brain-target-cache:/workspaces/brain/target \
        -w /workspaces/brain \
        brain-dev:latest \
        cargo +nightly miri test -p brain-storage --lib
```

`--lib` confines to the lib crate's tests; integration tests are syscall-bound (whole binary skipped — no benefit annotating).

## 6. Risks

- **`bytemuck::Pod` interactions with miri's strict provenance.** The crate is widely-used and miri-tested upstream; very unlikely to surface a real issue. If it does, that's a *real* finding — exactly what miri is for.
- **Miri runtime.** Pure-data tests are 10–100× slower under miri. 41 tests × 50ms native ≈ 2 sec native → maybe 20–60 sec miri. Acceptable.
- **`crc32c` crate may use SIMD intrinsics** that miri doesn't shim. If so, those tests fail. Mitigation: `crc32c` is well-maintained and offers a fallback `crc32c-rust` feature; we can configure via `[target.'cfg(miri)'.dependencies]` if needed. Hold the mitigation until we see the failure mode.
- **`tracing` macros under miri** — same concern; they're pure-Rust at the macro level. Should be fine.
- **First-time `miri setup` step.** Miri builds its own stdlib on first run; can take ~5 minutes. One-shot cost.

## 7. Verification plan

After the gating changes:

1. `cargo test -p brain-storage` (native, stable) — should still pass 155 lib + 4 integration tests.
2. `cargo +nightly miri test -p brain-storage --lib` — should pass ~41 miri-safe tests; ~105 reported as "filtered out" (via `#[cfg(not(miri))]` exclusion, not `ignored` — they just don't compile under miri).
3. Update `just verify` to NOT include miri (it's a separate `just miri`).

If miri turns up a real UB issue: stop and surface (per AUTONOMY §3). The most likely sites: a `bytemuck::bytes_of(&header_struct)` where the struct has unexpected padding (would manifest as an uninit-read error from miri).

## 8. Estimated commit shape

One commit on a new branch `chore/miri`:

> `chore(brain-storage): gate syscall-bound tests under miri (closes phase-2 exit item)`

Body:
- Why miri's coverage of brain-storage is narrow (syscalls aren't shimmed).
- What it does cover (Pod derivations, byte-arithmetic paths).
- The cfg-gating approach.
- `just miri` recipe.
- The miri pass output (test count).

Branch flow: `chore/miri` → `dev` → `main`. Standard process-change pattern (matches the CI / license fixes from earlier).

After the merge, the phase-exit checklist item "Miri passes on `brain-storage`" gets a ✅ with a link to `docs/phases/phase-02-storage.md`'s scope note.

Verify gate before commit: in the dev container, run both `cargo test -p brain-storage` (must pass) and `cargo +nightly miri test -p brain-storage --lib` (must pass).

---

PLAN READY: see `.claude/plans/phase-02-miri.md` — confirm to proceed.
