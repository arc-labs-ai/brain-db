# Plan: Phase 1 — Task 1.11, brain-core Companion Types

**Status:** approved (implemented; option A from §9)
**Date:** 2026-05-10
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Bring `brain-core` into spec-faithful alignment. Three concerns:

1. **Fix existing drift** between `brain-core::ids` and `spec/02_data_model/03_identifiers.md`. Three layout drifts found.
2. **Add the missing types** the protocol uses (`TxnId`).
3. **Update `Edge`** to match `spec/02_data_model/06_edges.md` §1 (add `weight`, `origin`, rename to `source/target`).
4. **Add `From`/`Into` between `brain-core` domain types and `brain-protocol` wire-domain types** so the wire/domain split is explicit and conversion happens at one place per type.

**Out of scope:**

- Rewriting the `Memory` struct to match spec §02/02's full shape. The current struct is sufficient for what the protocol surfaces; Phase 2 (Storage) owns the fully-hydrated `Memory` because that's where vectors, salience subcomponents, and timestamps actually live.
- New tests for `Salience` semantics (decay, access boost — those are background-worker concerns in Phase 8).
- Removing `brain-protocol`'s wire-domain types (`WireMemoryId`, `WireUuid`, `MemoryKindWire`, `EdgeKindWire`). Those exist by design (Task 1.7 plan §1) so brain-core stays free of rkyv. Wire types stay; conversions are added.

## 2. Spec references

- `spec/02_data_model/03_identifiers.md`
  - §2.1: `MemoryId` layout — **shard 16 bits + slot 48 bits + version 32 bits + reserved 32 bits**.
  - §3: `AgentId` — UUIDv7.
  - §4: `ContextId` — **u64**, agent-scoped.
  - §5: `RequestId` — UUIDv7.
  - §6.2: runtime `ShardId` — **u16**.
- `spec/02_data_model/02_memory_entity.md` §2 — Memory's logical shape (text, vector, kind, salience, timestamps, edges). Reference only; Phase 2 implements.
- `spec/02_data_model/05_salience.md` §1–2 — `Salience` is `f32` clamped to `[0, 1]`. Already matches.
- `spec/02_data_model/06_edges.md` §1 — `Edge { source, target, kind, weight, origin, created_at }`.

Binding constraints (the drift list):

| Type | Spec | Current `brain-core` | Action |
|---|---|---|---|
| `MemoryId` bit layout | shard 16 + slot 48 + version 32 + reserved 32 | shard 8 + version 16 + slot 56 + reserved 48 | rewrite `pack`, accessors, tests |
| `ContextId` | u64 (agent-scoped) | UUID (Uuid) | replace with newtype around u64 |
| runtime `ShardId` | u16 | u8 | widen to u16 |
| `SlotIndex` | implied 48-bit (from MemoryId) | u64 | keep u64; mask to 48 bits in pack |
| `SlotVersion` | implied 32-bit | u16 | widen to u32 |
| `TxnId` | implied UUIDv7 (used in protocol §07/9-11) | absent | add as UUIDv7 newtype |
| `Edge` fields | source/target/kind/weight/origin/created_at | from/to/kind/created_at_unix_ms | add weight + origin; rename from/to → source/target |
| `EdgeOrigin` enum | Explicit / AutoDerived | absent | add |

Spec wins on every drift per AUTONOMY §2.1.

## 3. External validation

**Not applicable** — pure data-model work. No new framework, no new library. UUIDv7 already in use via the existing `uuid` workspace dep with the `v7` feature.

## 4. Architecture sketch

### 4.1 brain-core/src/ids.rs

```rust
// Sizes per spec §02/03
pub type ShardId    = u16;        // was u8
pub type SlotIndex  = u64;        // 48-bit value space; type stays u64
pub type SlotVersion = u32;       // was u16

#[derive(...)]
pub struct ContextId(pub u64);    // was Uuid

#[derive(...)]
pub struct TxnId(pub Uuid);       // new

impl MemoryId {
    /// Spec §02/03 §2.1:
    ///   bytes 0..2  : shard_id (u16, BE on the wire)
    ///   bytes 2..8  : slot_id  (u48, BE)
    ///   bytes 8..12 : version  (u32, BE)
    ///   bytes 12..16: reserved (must be 0)
    ///
    /// Internally stored as `u128` in the spec's bit ordering so
    /// `to_be_bytes` / `from_be_bytes` match the wire layout.
    pub const fn pack(shard: ShardId, slot: SlotIndex, version: SlotVersion) -> Self;
    pub const fn shard(self)   -> ShardId;
    pub const fn slot(self)    -> SlotIndex;
    pub const fn version(self) -> SlotVersion;
}
```

The `u128` storage stays — only the bit *layout* changes. The on-the-wire bytes (which `WireMemoryId = u128` carries in `brain-protocol`) inherit the new layout automatically since `WireMemoryId` is just the `u128`.

### 4.2 brain-core/src/edge.rs

```rust
pub struct Edge {
    pub source: MemoryId,         // was `from`
    pub target: MemoryId,         // was `to`
    pub kind: EdgeKind,
    pub weight: f32,              // new — spec §02/06 §1 ([0.0, 1.0])
    pub origin: EdgeOrigin,       // new
    pub created_at_unix_nanos: u64,  // was created_at_unix_ms; spec uses nanos
}

pub enum EdgeOrigin {
    Explicit,
    AutoDerived,
}
```

`EdgeKind` already matches spec (8 variants). `is_symmetric` predicate stays.

### 4.3 brain-protocol — From/Into bridges

Add (in `brain-protocol/src/request.rs` or a new tiny `convert.rs`):

```rust
impl From<MemoryKind>          for MemoryKindWire   { ... }
impl From<MemoryKindWire>      for MemoryKind       { ... }
impl From<EdgeKind>            for EdgeKindWire     { ... }
impl From<EdgeKindWire>        for EdgeKind         { ... }
impl From<MemoryId>            for WireMemoryId     { fn from(id: MemoryId) -> u128 { id.raw() } }
impl From<WireMemoryId>        for MemoryId         { fn from(raw: u128)    -> MemoryId { MemoryId::from_raw(raw) } }
impl From<RequestId>           for WireUuid         { fn from(r: RequestId) -> [u8; 16] { *r.0.as_bytes() } }
impl From<AgentId>             for WireUuid         { fn from(a: AgentId)   -> [u8; 16] { *a.0.as_bytes() } }
impl From<TxnId>               for WireUuid         { fn from(t: TxnId)     -> [u8; 16] { *t.0.as_bytes() } }
impl TryFrom<WireUuid>         for RequestId, AgentId, TxnId { ... }
```

These conversions live in `brain-protocol` (which already depends on `brain-core`) so brain-core stays unaware of wire formats.

### 4.4 What does NOT change

- `Memory` struct stays as-is; Phase 2 reshapes it.
- `Salience` stays; spec already matches.
- `MemoryKind` variant order stays; matches `MemoryKindWire`.
- `EdgeKind` variant order stays; matches `EdgeKindWire`.
- `RequestId`, `AgentId` representations stay (UUIDv7).
- `Error` taxonomy stays.

## 5. Trade-offs considered

| Alternative | Verdict |
|---|---|
| **Chosen:** fix drifts in-place; widen sizes; add From/Into bridges in brain-protocol; keep wire-domain types. | ✓ Spec-faithful; minimal blast radius; preserves the wire/domain split. |
| Replace wire-domain types with brain-core types via rkyv-derive on brain-core. | rejected (Task 1.7 plan §5 already considered) — pulls rkyv into brain-core; couples value types to wire format. Same reasoning still applies. |
| Defer the `MemoryId` layout fix to Phase 2 (storage). | rejected — the layout is encoded in `MemoryId::pack` and consumed by every test that builds a sample memory id. Touching it now is small; touching it after storage code lands would require rewriting storage assumptions too. |
| Rewrite `Memory` to match spec §02/02 fully here. | rejected — Memory's vector/timestamp/salience subcomponents are storage-layer concerns. Doing it here creates types that brain-storage will inevitably reshape. Defer. |
| Add wire-format `Conv*` traits with explicit `to_wire`/`from_wire`. | rejected — `From`/`Into` is the standard Rust idiom; bespoke traits don't add value. |
| Move From/Into into brain-core. | rejected — would force brain-core to depend on brain-protocol (cycle risk) or import wire-domain primitives back in. Keep them in brain-protocol where the wire-domain types live. |

## 6. Risks / open questions

- **Bit-layout fix is breaking** for any code that constructs a `MemoryId` from a hex literal or a hand-packed `u128`. Today: only `brain-core::ids::tests` and `brain-protocol`'s `sample_memory_id()` helpers. Both are mine to update. No persisted data to migrate (Phase 1 is wire only).
- **`ContextId: Uuid → u64` is breaking** for any callers. Today: only `brain-protocol`'s `WireUuid`-typed `context_id` fields and the response sample helpers. The wire format keeps `WireUuid` (16 bytes) — but spec §02/03 §4 says wire is 8 bytes. **Drift between protocol §07 (WireUuid for context_id) and §02/03 (8 bytes for ContextId).** This is a legitimate spec ambiguity. Mitigation:
  - Plan A: keep `context_id` in the wire as `[u8; 8]` and call it `WireContextId`; bridge to `ContextId(u64)` via `u64::from_be_bytes`.
  - Plan B: surface to user — spec §02/03 vs spec §03/07 disagree; ask which is authoritative.
  - **I will surface this and stop on it before changing wire-format fields.** It's a cross-spec inconsistency, not a brain-core fix.
- **`SlotVersion: u16 → u32` is breaking** for any storage assumption. Today: only the type alias and `MemoryId::version` accessor. No storage yet.
- **`EdgeKindWire` numeric values** stay 0..7 to match `EdgeKind` discriminants; the From impl is straightforward.
- **`TxnId` representation choice** — UUIDv7 like `RequestId`. Spec §03/07 §9 says "16 bytes", consistent.
- **Edge created_at unit** — spec §02/06 §1 says `unix_nanoseconds`. Current is `unix_ms`. Rename + widen-precision is a small fix.

## 7. Test plan

Per phase-doc Done-when:

- **brain-protocol compiles without inline duplicates of types that belong in core.**
  Maps to: From/Into bridges added; the wire-domain types (`MemoryKindWire`, `EdgeKindWire`, etc.) stay but are now explicitly bridged. The "no inline duplicates" test is satisfied because `MemoryId`, `ContextId`, `RequestId`, `AgentId`, `TxnId`, `MemoryKind`, `EdgeKind`, `Salience`, `Edge` all live in brain-core only.

- **brain-core compiles standalone.**
  Maps to: `cargo build -p brain-core` passes; `cargo test -p brain-core` passes.

New tests:
- `MemoryId::pack` against the new layout — proptest with `shard ∈ 0..=u16::MAX`, `slot ∈ 0..(1<<48)`, `version ∈ 0..=u32::MAX`. Round-trip via accessors.
- `MemoryId` byte-layout pin: pack a known triple; assert specific `to_be_bytes()` output matches the spec's documented bytes.
- `ContextId` round-trip via `to_le_bytes` / `from_le_bytes` (spec §08 says wire is "8, fixed").
- `TxnId::new()` constructor + Display + UUID-v7 sanity.
- `Edge` constructor with weight + origin.
- From/Into bridges round-trip: `MemoryId → WireMemoryId → MemoryId` (identity); `MemoryKind ↔ MemoryKindWire`; `EdgeKind ↔ EdgeKindWire`.

Existing tests that need updating:
- `brain-core::ids::tests::pack_unpack_roundtrip` — new layout.
- `brain-core::ids::tests::distinct_components_produce_distinct_ids` — same input shape.
- `brain-core::ids::tests::pack_unpack_arbitrary` proptest — bigger ranges.
- `brain-protocol::request::tests::sample_memory_id` and any tests asserting specific u128 bit positions.

## 8. Commit shape

One commit:

> `1.11: align brain-core types with spec §02; add TxnId, Edge fields, From/Into bridges`

Includes:

1. `crates/brain-core/src/ids.rs` — new `MemoryId` bit layout, `ShardId`/`SlotVersion` size widen, `ContextId(u64)`, add `TxnId`, update tests.
2. `crates/brain-core/src/edge.rs` — add `weight`, `origin`, rename `from`/`to` → `source`/`target`, switch to nanos.
3. `crates/brain-core/src/lib.rs` — re-export `TxnId`, `EdgeOrigin`.
4. `crates/brain-protocol/src/request.rs` — update `sample_memory_id()`; ensure `WireMemoryId = u128` field uses `MemoryId::raw()` if any `WireMemoryId` literal exists (none currently — fields stay as `u128`).
5. `crates/brain-protocol/src/convert.rs` (new, private) — From/Into bridges.
6. Phase doc — mark 1.11 `[x]` and add a footnote about the surfaced spec ambiguity (see §6 risks).

If the spec ambiguity in §6 (ContextId wire format) requires a resolution before I can compile, I will **STOP and surface** before touching the wire format and split the work into two commits: drift fixes + bridges (this commit), then ContextId wire fix (after user resolves the spec).

## 9. Ambiguity to surface before implementation

**Stop point:** spec §02/03 §4 says `ContextId` is **u64 (8 bytes)** on the wire; spec §03/07 §1 (and the rest of §07) shows `context_id: ContextId` in payloads with no explicit size, but the existing protocol implementation uses `WireUuid = [u8; 16]` for it. Looking at §02/03 §8 (wire/storage representations table):

> `ContextId` | 8, fixed | 8, fixed (host endianness)

Spec is explicit: 8 bytes. The protocol is currently wrong (16 bytes). This is real drift.

Two ways to fix:

- **A.** Update brain-protocol's `context_id` fields from `WireUuid` to a new `WireContextId = [u8; 8]` (or directly `u64`). This is wire-breaking but pre-Phase-9, and we're at v1 with no deployed clients — the right call.
- **B.** Defer the ContextId wire fix to a separate task; this 1.11 only does the brain-core side. The wire stays inconsistent with §02/03 until then.

**Recommendation: do A in this commit.** It's small (find/replace `context_id: WireUuid` → `context_id: WireContextId` in ~12 sites; update tests). Aligns wire with spec; resolves drift now.

If the user disagrees, do B and surface a CONTEXT.md describing the deferred fix.

## 10. Confirmation

Awaiting "go" / "approved" / specific revisions. Note option A vs B in §9.
