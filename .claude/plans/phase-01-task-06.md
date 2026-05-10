# Plan: Phase 1 — Task 1.6, ProtocolError Taxonomy (retrospective)

**Status:** implemented (commit `61e81b6`)
**Date drafted:** 2026-05-10 (retrospective)
**Author:** Claude (autonomous)

---

## 1. Scope

Build out the full error type hierarchy in `brain_protocol::error`:

- `ErrorCategory` — the 9 broad classes from spec §10 §2 that drive client retry behavior.
- `ErrorCode` — every named code in spec §10 §3.1–§3.9, one variant per row (~55 variants).
- Expand `ProtocolError` with `BadFrame(String)`, `BadFlagCombination(String)`, `MalformedPayload(String)`, plus `code()` and `category()` accessors.
- `From<ProtocolError> for brain_core::Error` so codec failures propagate up the stack.

**Out of scope:**

- ERROR-frame on-wire encoding (numeric codes / category bytes) — Task 1.8 introduces `ErrorCodeWire` / `ErrorCategoryWire` mirrors with stable u16 / u8 values.
- Server-side error message formatting / `ErrorDetails` policy — Phase 9.

## 2. Spec references

- `spec/03_wire_protocol/10_errors.md` — entire file, every section.
  - §2 — 9 categories with retryability rules.
  - §3.1–§3.9 — full code tables (Protocol, Authentication, Authorization, Validation, NotFound, Conflict, ResourceExhausted, Internal, Unavailable).
  - §6 — SDK retry-policy table (drives `is_retryable`).
  - §10 — error message verbosity limits (informational; not enforced in codec).

Binding constraints:

- §3.2 — `VersionNotSupported` and `NoSuchAuthMethod` are Protocol-category; `Unauthenticated`, `NotAuthenticated`, `AuthBackendUnavailable`, `SessionExpired` are Authentication-category. Implementation faithfully splits the §3.2 table.
- §6 — retryable categories are `ResourceExhausted`, `Internal`, `Unavailable`. Encoded in `ErrorCategory::is_retryable`.

## 3. External validation

Not applicable — pure type-system work driven by spec.

## 4. Architecture sketch

```text
brain-protocol/src/error.rs

pub enum ErrorCategory { Protocol, Authentication, Authorization, Validation,
                          NotFound, Conflict, ResourceExhausted, Internal, Unavailable }
impl ErrorCategory {
    pub fn is_retryable(self) -> bool;   // §6
}

#[non_exhaustive]
pub enum ErrorCode {
    // §3.1 Protocol (11)
    BadMagic, BadHeaderCrc, BadPayloadCrc, BadOpcode, BadVersion,
    BadFrame, OversizePayload, ReservedFieldNonZero, BadFlagCombination,
    MalformedRkyv, MalformedVector,
    // §3.2 Connection / handshake (6)
    VersionNotSupported, NoSuchAuthMethod, Unauthenticated, NotAuthenticated,
    AuthBackendUnavailable, SessionExpired,
    // §3.3 Authorization (3) ...
    // §3.4 Validation (11) ...
    // §3.5 NotFound (5) ...
    // §3.6 Conflict (5) ...
    // §3.7 ResourceExhausted (7) ...
    // §3.8 Internal (5) ...
    // §3.9 Unavailable (4)
    ShardUnavailable, Overloaded, Restarting, Maintenance,
}
impl ErrorCode {
    pub fn category(self) -> ErrorCategory;   // exhaustive match
}

pub enum ProtocolError {
    BadMagic, BadVersion { got, expected }, BadHeaderCrc, BadPayloadCrc,
    OversizePayload { len, max }, ReservedFieldNonZero,
    UnknownOpcode(u8), Truncated { have, need },
    BadFrame(String), BadFlagCombination(String), MalformedPayload(String),
}
impl ProtocolError {
    pub fn code(&self) -> ErrorCode;
    pub fn category(&self) -> ErrorCategory;
}

impl From<ProtocolError> for brain_core::Error {
    fn from(e: ProtocolError) -> Self {
        match e.category() {
            ErrorCategory::Internal => brain_core::Error::Internal(e.to_string()),
            _ => brain_core::Error::InvalidArgument(e.to_string()),
        }
    }
}
```

Three layers of generality. `ProtocolError` is what the codec emits; `ErrorCode` is the wire-level enumeration; `ErrorCategory` is the retry-policy bucket.

## 5. Trade-offs considered

| Alternative | Verdict |
|---|---|
| **Chosen:** three layers — `ProtocolError` (codec) + `ErrorCode` (full §10 mirror) + `ErrorCategory` (retry policy). | ✓ Codec stays focused; full taxonomy is reachable via `code()`. |
| `ProtocolError` carries every §10 variant as its own variant. | rejected — ~60 variants; codec only emits ~11 of them; the rest arrive over the wire from the server, not as Rust-side errors. |
| Drop `ErrorCategory` and just expose `is_retryable` on `ErrorCode`. | rejected — category is referenced by SDKs, dashboards, and logs (spec §12). Worth its own type. |
| Make `ErrorCode` exhaustive (no `#[non_exhaustive]`). | rejected — spec §03/05 §7 reserves opcode ranges for future expansion; mirroring that disposition for codes is the same forward-compat principle. |
| `From<ProtocolError> for brain_core::Error` always uses `Internal`. | rejected — most codec failures are caller-bug, not server-bug; `InvalidArgument` better captures "you fed me bad bytes." |

## 6. Risks / open questions

- **`#[non_exhaustive]` ⇄ rkyv** — adding rkyv derives to a non-exhaustive enum is awkward. Resolved in Task 1.8 by introducing an `ErrorCodeWire` mirror that's closed and rkyv-archivable, with `From` impls bridging to/from the canonical type.
- **`MalformedPayload` is a single variant** that maps to `MalformedRkyv` — the spec distinguishes `MalformedRkyv` and `MalformedVector` (§3.1). For now the codec doesn't differentiate; if a vector-blob validator surfaces, we'd add `MalformedVector(String)` and refine the mapping.
- **`Truncated` maps to `BadFrame`** — spec doesn't have a dedicated "truncated" code, just the umbrella `BadFrame`. Acceptable but the Display message is informative.

## 7. Test plan

Mapped to phase-doc Done-when:

- **Every variant in spec §10 is represented.** ← `error_code_categories_match_spec` spot-checks one code per category; 55 variants are present in the enum and validated by exhaustive `match` in `ErrorCode::category`.
- **`From<ProtocolError>` for `brain_core::Error`.** ← `protocol_error_converts_to_brain_core_invalid_argument` exercises both branches and pins the Display-message round-trip.

Additional:
- `retryability_matches_spec_table` — confirms §6 mapping.
- `protocol_error_codes_are_in_protocol_category` — every codec-emitted variant is Protocol-category.
- `protocol_error_code_mapping_is_stable` — pins the ProtocolError → ErrorCode lookup table.

5 tests, all pass. Workspace-total at this point: 64.

## 8. Commit shape

Single commit:

> `61e81b6  1.6: complete ProtocolError taxonomy with ErrorCode/Category`

Total diff: ~447 insertions, 16 deletions.

## 9. Lessons / handoff

- The "open canonical type + closed wire mirror" pattern was new here — used again for `ErrorCodeWire` / `ErrorCategoryWire` in Task 1.8 to pair with rkyv's closed-world derive. Worth recognizing as a reusable shape: when a type needs to be both forward-compatible (Rust API) and rkyv-encoded (closed wire), introduce a `*Wire` mirror.
- `is_retryable` is on `ErrorCategory`, not `ErrorCode`, because retry policy is per-category in §6. The SDK layer will use `e.category().is_retryable()`.
- `From<ProtocolError> for brain_core::Error` is intentionally lossy — the `ProtocolError` variant data is flattened to a Display string. Higher layers shouldn't need to introspect codec errors past the category.
