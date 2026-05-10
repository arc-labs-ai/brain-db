---
name: brain-protocol-version-bump
description: Catch wire-breaking changes in brain-protocol needing a VERSION bump (new opcodes, reordered fields, type changes). Fires on edits to header/opcode/frame/request/response/error.
when-to-use: |
  Triggers:
    - Diff touches crates/brain-protocol/src/{header,opcode,frame,request,response,error}.rs
    - User says "wire-breaking change" / "do we need a version bump?"
    - Adding/renaming/reordering an Opcode variant
    - Adding/renaming/reordering an ErrorCode variant
    - Changing a field type, name, or order in a Request/Response struct
    - Adding a new field to an existing rkyv-archived struct
trigger-files:
  - crates/brain-protocol/src/header.rs
  - crates/brain-protocol/src/opcode.rs
  - crates/brain-protocol/src/frame.rs
  - crates/brain-protocol/src/request.rs
  - crates/brain-protocol/src/response.rs
  - crates/brain-protocol/src/error.rs
spec-refs:
  - spec/03_wire_protocol/05_opcodes.md
  - spec/03_wire_protocol/10_errors.md
---

# Wire-Version Bump Detector

## When to use

Any change to `brain-protocol`'s wire surface (frame header, opcode table, request/response payloads, error codes). The wire version (`VERSION = 1`) is bound by spec §03/03 §3.2; bumping it is a coordinated event across SDKs and the server.

## Core rule

**The wire format is contractual.** Per spec §03/05 §7 ("Opcode evolution"), and §03/12 (versioning):

> Adding new opcodes is a wire-protocol-version bump.
> Existing opcodes are stable; their semantics don't change within a version.

Same applies to:

- Frame header layout (any field reorder, size, or semantic change).
- Error codes (adding new ones is a bump; existing codes' meaning is stable).
- Request/response payload structure within an opcode.

## Wire-breaking change → required action

| Change | Impact | Required action |
|---|---|---|
| **New opcode** | Old clients/servers can't decode | Spec change + `VERSION` bump |
| **Renumber existing opcode** | Catastrophic interop break | NEVER do this (spec forbids) |
| **Add `Opcode` variant in a reserved range (0x70-0x7F, 0xF0-0xFE)** | Old code returns `UnknownOpcode` | Spec change + `VERSION` bump |
| **Rename `Opcode` variant** | Source-compat break for SDKs; wire bytes unchanged | Source-only — *no* wire bump, but SDK callers see the new name |
| **Reorder rkyv struct fields** | Old archives can't validate | Spec change + `VERSION` bump |
| **Add field to existing rkyv struct** | Old archives won't deserialize as new type | Spec change + `VERSION` bump |
| **Remove field from existing rkyv struct** | New archives can't be read by old clients | Spec change + `VERSION` bump |
| **Change `MAGIC` bytes** | Catastrophic — first-frame magic check fails | NEVER do this |
| **Change `VERSION` value** | The bump itself | Always paired with spec change |
| **Add new `ErrorCode` variant** | Old clients see `UnknownCode` | Spec change to §10 (`ErrorCode` is `#[non_exhaustive]` so source-compat survives) |
| **Add new `ErrorCategory` variant** | Same as above | Spec change to §10 §2 |
| **Add a new opcode to `RequestBody` / `ResponseBody`** | New variants OK on the same `VERSION` only if the opcode existed already | Verify Opcode exists |
| **`Header` field reorder / type change** | Catastrophic — every frame mis-decodes | Spec change + `VERSION` bump |
| **Constant change (`HEADER_SIZE`, `MAX_PAYLOAD_BYTES`)** | Misframing | Spec change + `VERSION` bump |
| **rustdoc / comment change** | None | Source-only |
| **New private helper / refactor with same external API** | None | Source-only |

## Workflow

1. **Identify the touched files.** `git diff --name-only HEAD <files>` against the trigger globs.
2. **Classify each change** against the table above.
3. **If any row demands a `VERSION` bump:**
   - Confirm the corresponding spec section has been updated (it should be — Brain spec is read-only to autonomous Claude).
   - If spec didn't change, STOP and surface — this is a drift event (AUTONOMY §19).
4. **Always check:**
   - The pinned CRC test vector in `crc::tests::header_crc_known_vector_for_minimal_header` — does it still match? If not, the header layout drifted.
   - The `Opcode::from_u8` proptest — does it still pass for every byte?
   - The per-variant round-trip tests in `request.rs` / `response.rs` — does adding/changing the type break any?
5. **Report:**
   - For source-only changes: "no wire bump required; SDK code change only."
   - For wire bumps: confirm the spec change reference and call out which SDKs need updating (currently only `brain-sdk-rust`; future SDKs in Phase 10).

## Examples

### Golden — adding a new admin opcode

User adds `Opcode::AdminCompactReq = 0x6A` and `AdminCompactResp = 0xEA`.

Workflow:
- Opcode table is appended (not reordered) — wire-bump territory.
- Confirm spec §03/05 §1.6 includes the new entries. If yes: this *is* a wire bump; the spec said so; the new `VERSION = 2` change must accompany.
- Round-trip tests pass; pinned CRC vector unaffected (header layout unchanged).
- `RequestBody::AdminCompact(...)` and `ResponseBody::AdminCompact(...)` variants added; tests added.

Report: wire bump required (per spec §03/05 §1.6 v2). All checks pass.

### Counter — silent renumbering

```diff
-    EncodeReq = 0x20,
+    EncodeReq = 0x21,         // ← NO. Catastrophic interop break.
```

This is forbidden by spec §03/05 §1. STOP and surface; do not commit.

### Counter — silent field reorder

```diff
 pub struct EncodeRequest {
+    pub deduplicate: bool,    // ← moved earlier
     pub text: String,
-    pub deduplicate: bool,
 }
```

rkyv archives are positional. Reorder breaks every existing serialized payload. Wire bump required, AND the spec §07/1 has to be updated, AND we need to confirm existing on-disk records (Phase 2+) aren't affected.

## Cross-references

- `brain-invariants` — for the seven CLAUDE.md §5 invariants.
- `brain-spec-invariant` — for verifying a specific MUST in §03.
- `audit-spec` (built-in) — whole-crate audit.

## Source / Adaptations

Project-local.
