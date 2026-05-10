---
name: brain-spec-invariant
description: Verify a single spec MUST clause against the impl with file:line evidence. Tighter than audit-spec — one MUST at a time. Use when investigating "does the impl honor §X.Y?"
when-to-use: |
  Triggers:
    - User says "verify spec MUST 03/03 §3.6" / "does this honor §X.Y?"
    - Spec section was updated and we need to confirm code follows
    - Drafting a CONTEXT.md after detecting drift
    - Pre-phase-exit: walking the spec section by section
spec-refs:
  - spec/00_master_overview/02_doc_map.md
---

# Spec MUST Verification

## When to use

A specific spec clause needs to be checked against the implementation — not a whole-crate audit (`audit-spec` does that), and not the seven CLAUDE.md invariants (`brain-invariants` does that). One MUST at a time, with file:line evidence.

## Workflow

1. **Locate the MUST.** Open the spec file. Quote the exact clause verbatim — including the surrounding sentence so the context is preserved.
2. **Identify the responsible code.** Use the doc map (`spec/00_master_overview/02_doc_map.md`) or grep for symbol names mentioned in the clause. If a clause names a function, type, or constant, that's the entry point.
3. **Walk the implementation.** Read the actual code path that should honor the MUST. Don't trust comments — trust the executed code.
4. **Verify the test evidence.** A MUST without a test is a latent regression risk. Find the test that proves the clause; if missing, flag it.
5. **Report.** One of:
   - **honored** — file:line where the rule is enforced + the test that proves it.
   - **violated** — file:line + a one-paragraph description of how the impl differs from the spec; STOP and surface.
   - **not yet implemented** — the clause is for a future phase; note which phase.

## Output format

```
MUST: spec/<path>.md §<section>
Quote: "<verbatim spec text>"

Status:    honored | violated | not-yet-implemented
Evidence:  <crate>/<file>:<line>  -- <one-line summary>
Test:      <crate>/<file>:<test>  -- <what it proves>
Notes:     <optional: subtleties, caveats>
```

If `violated`:

```
Drift: <one paragraph>
Impact: <who's affected; what's the worst-case>
Fix shape: <code change OR spec change OR both>
```

…then STOP and surface per AUTONOMY §3 / §19. Do not autofix spec drift.

## Examples

### Golden

```
MUST: spec/03_wire_protocol/03_frame_header.md §3.6
Quote: "Computed over bytes 0–7 followed by bytes 12–31 — i.e., the entire
       header minus the `header_crc32c` field itself."

Status:   honored
Evidence: crates/brain-protocol/src/header.rs:175  -- compute_header_crc splices [0..8] ++ [12..32]
Test:     crates/brain-protocol/src/crc.rs:tests::header_crc_excludes_self
Notes:    Header::seal recomputes on mutation; the splice is in one helper.
```

### Counter — drift detected

```
MUST: spec/03_wire_protocol/03_frame_header.md §8 (endianness summary)
Quote: "All multi-byte integers are big-endian."

Status:   violated
Evidence: crates/brain-protocol/src/header.rs:84    -- payload_len uses u24::to_le_bytes
Test:     none — round-trip tests succeed because both sides flipped.

Drift:    The header writer and reader both use little-endian, so the
          internal round-trip passes, but a third-party reader following
          the spec would mis-decode payload_len.
Impact:   Cross-implementation interop; SDKs in non-Rust would fail at v1.
Fix shape: Code change — switch to to_be_bytes / from_be_bytes; add a
           pinned hex test vector for one PING-shaped frame.
```

…then write `CONTEXT.md` with this summary and stop.

## Cross-references

- `brain-invariants` — for the seven CLAUDE.md §5 invariants (broader; this skill is one-MUST-at-a-time).
- `audit-spec` (built-in) — for whole-crate spec walks.
- `spec` (built-in) — for navigating to a section.

## Source / Adaptations

Project-local.
