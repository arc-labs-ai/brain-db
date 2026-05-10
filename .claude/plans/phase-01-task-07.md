# Plan: Phase 1 — Task 1.7, Request Body Codec (retrospective)

**Status:** implemented (commits `883f16a`, `c5ca0ea`)
**Date drafted:** 2026-05-10 (retrospective)
**Author:** Claude (autonomous)

---

## 1. Scope

Add `brain_protocol::request` with one variant of `RequestBody` per server-bound opcode in spec §03/07 (25 variants). rkyv 0.7 codecs for the structured fields. Wire-domain DTO types (`WireUuid`, `WireMemoryId`, byte-tagged enums) so `brain-core` doesn't need rkyv derives. `RequestBody::encode` / `decode` round-trip for every variant.

**Out of scope:**

- Vector-blob composition (`vector_offset` / `vector_dim` are stored in the rkyv struct, but the actual `f32` byte placement happens at the `Frame` layer). Spec §03/04 §2 owns the layout; Phase 2/9 wires it.
- Conversion between wire types and `brain-core` domain types — operation handlers do that.
- Field-level validation (e.g., `text` non-empty, `top_k` ≤ 1000). Spec §03/11 §4 says that's the operation handler's job; not the codec's.

## 2. Spec references

- `spec/03_wire_protocol/07_request_frames.md` — full request payload schemas (§1–§25).
- `spec/03_wire_protocol/04_payload_encoding.md` — rkyv + bytemuck split.
  - §3 — rkyv 0.7, validation via `check_archived_root`.
  - §4 — vector-blob layout in the trailing raw section.

Binding constraints:

- Opcode → variant mapping pinned by spec §03/05 §1 (HELLO=0x01, ENCODE_REQ=0x20, … ADMIN_LIST_TOMBSTONED_REQ=0x69, ERROR=0xFF).
- The `txn_id`, `request_id`, `agent_id`, `context_id`, `model_fingerprint` fields are 16 bytes (spec §07/§02). `MemoryId` is 16 bytes packed into u128 (spec §02/03).
- Enum variants like `MemoryKind`, `EdgeKind`, `RecallStrategy`, `PlanStrategy`, `ForgetMode`, `StatsDetail`, `EventType` carry stable wire values per spec §02 / §07.

## 3. External validation

- **rkyv 0.7 docs** (https://docs.rs/rkyv/0.7) — confirmed:
  - `#[archive(check_bytes)]` enables the validator-derived `CheckBytes` impl.
  - `rkyv::check_archived_root::<T>(bytes)` validates and returns `&Archived<T>`.
  - `rkyv::Infallible` is the deserializer when no fallible deserialization is needed.
  - `AllocSerializer<N>` allocates a starting scratch of `N` bytes; rkyv grows it as needed.
- **HRTB workaround for `DefaultValidator<'a>`** — the bound `for<'a> T::Archived: CheckBytes<DefaultValidator<'a>>` is required because rkyv's validator borrows the byte slice. Verified by the rust compiler error during initial implementation; pattern documented in `rkyv_codec.rs`.

## 4. Architecture sketch

```text
brain-protocol/src/request.rs

// Wire-domain primitives (avoid rkyv-coupling brain-core)
pub type WireUuid    = [u8; 16];   // AgentId / ContextId / RequestId / TxnId
pub type WireMemoryId = u128;      // packed (shard, slot, version)

// Helper enums (rkyv-archivable, byte-tagged)
pub enum MemoryKindWire { Episodic = 0, Semantic = 1, Consolidated = 2 }
pub enum EdgeKindWire   { Caused = 0, ..., PartOf = 7 }
pub enum RecallStrategy { Auto = 0, AnnOnly = 1, Attractor = 2, GraphWalk = 3, Hybrid = 4 }
pub enum PlanStrategy   { Auto, AStar, Mcts, AttractorRollout }
pub enum PlanState      { ByMemoryId(WireMemoryId), ByText(String), ByVector { offset, dim } }
pub enum ObservationInput { ByMemoryId(WireMemoryId), ByText(String) }
pub enum ForgetMode     { Soft, Hard }
pub enum CancellationReason { ClientUnneeded, Timeout, Other(String) }
pub enum StatsDetail    { Summary, PerShard, PerContext, Full }
pub enum CheckScope     { QuickSample, PerShard(Vec<u8>), Full }

// Per-opcode request structs (25)
pub struct EncodeRequest { ... }
pub struct EncodeVectorDirectRequest { ... }
pub struct RecallRequest { ... }
... 22 more ...

// Dispatch enum
pub enum RequestBody {
    Encode(EncodeRequest),
    EncodeVectorDirect(EncodeVectorDirectRequest),
    ... 23 more ...
}

impl RequestBody {
    pub fn opcode(&self) -> Opcode;
    pub fn encode(&self) -> Vec<u8>;
    pub fn decode(opcode: Opcode, bytes: &[u8]) -> Result<Self, ProtocolError>;
}
```

rkyv plumbing (later promoted to `crate::rkyv_codec` in Task 1.8):

```rust
fn to_rkyv_bytes<T: Serialize<AllocSerializer<256>>>(v: &T) -> Vec<u8>;
fn from_rkyv_bytes<T>(bytes: &[u8]) -> Result<T, ProtocolError>
where T: Archive,
      T::Archived: for<'a> CheckBytes<DefaultValidator<'a>> + Deserialize<T, Infallible>;
```

## 5. Trade-offs considered

| Alternative | Verdict |
|---|---|
| **Chosen:** wire-domain DTOs in brain-protocol; rkyv-derive only on the wire types. | ✓ Keeps `brain-core` dep-clean; conversion at handler boundaries is explicit. |
| Add rkyv derives to `brain-core` value types (`MemoryId`, `ContextId`, `MemoryKind`, etc.). | rejected — pulls rkyv into a foundational crate; couples value types to wire format. Big blast radius for future protocol changes. |
| One big rkyv-archivable `RequestBody` enum with all 25 variants. | rejected — rkyv handles enums but generates large per-variant archived types and the dispatch ergonomics are worse than per-struct + opcode lookup. |
| Include vector blobs in `RequestBody::encode` output. | rejected — spec §03/04 puts vectors in the trailing raw section after the rkyv portion. The Frame layer composes them; the body codec emits structured bytes only. |
| Use `serde` + bincode instead of rkyv. | rejected — spec §03/04 §3 mandates rkyv for zero-copy reads. Switching codecs is a wire-protocol-version bump per §07. |

## 6. Risks / open questions

- **HRTB lifetime in `from_rkyv_bytes`** — initial implementation used `DefaultValidator<'static>` and hit `borrowed data escapes outside of function`. Fixed by `for<'a> ...` HRTB. Pattern is now in `rkyv_codec.rs`.
- **`AllocSerializer<256>` initial scratch** — too small? rkyv grows as needed (`AlignedVec` reallocs). 256 bytes is fine for the typical small request; benchmarks not run.
- **`Cargo.lock` drift** — rkyv pulled in 6 transitive deps (ahash, bytecheck, hashbrown, ptr_meta, rend, seahash). `Cargo.lock` was missed in the initial commit; backfilled in `c5ca0ea`. Lesson: stage `Cargo.lock` whenever a manifest changes.
- **Vector-blob round-trip is untested at this layer** — by design (out of scope), but a future end-to-end test in Phase 2/9 must cross-check that `vector_offset` / `vector_dim` from the rkyv struct line up with the bytes appended after.

## 7. Test plan

Mapped to phase-doc Done-when:

- **All request opcodes from §07 have a matching variant and codec.** ← `RequestBody` enum has 25 variants; `encode`/`decode` cover all of them.
- **Round-trip tests for each.** ← 15 tests grouped by family:
  - `encode_round_trips`
  - `encode_vector_direct_round_trips`
  - `recall_round_trips`
  - `plan_round_trips_with_each_state_variant` (3 PlanState variants)
  - `reason_round_trips_with_each_observation_variant` (2 ObservationInput variants)
  - `forget_round_trips` (2 modes)
  - `subscribe_round_trips`
  - `unsubscribe_round_trips`
  - `txn_lifecycle_round_trips` (begin / commit / abort)
  - `cancel_stream_round_trips` (3 reasons)
  - `keepalive_and_bye_round_trip` (Ping, ClientPong, Bye with/without reason)
  - `admin_round_trips` (10 admin opcodes)
- **Vector blobs (where present) use `bytemuck::cast_slice`, not rkyv.** ← Vector blob composition is owned by `Frame`; phase doc footnote clarifies.

Negative tests:
- `decode_with_response_opcode_returns_unknown` — feeding a response opcode to `RequestBody::decode` errors.
- `decode_garbage_returns_malformed` — random bytes for a real opcode → `MalformedPayload`.
- `opcode_matches_variant` — every variant reports its expected opcode.

15 unit + 2 negative + 1 cross-check = **18 tests**, all pass. Workspace at this point: 79.

## 8. Commit shape

Two commits:

- `883f16a  1.7: implement RequestBody codec for all 25 server-bound opcodes` (main; 965 insertions)
- `c5ca0ea  fix: stage rkyv-induced Cargo.lock changes from 1.7` (lockfile fixup; 216 lines)

## 9. Lessons / handoff

- **Always stage `Cargo.lock` when a manifest changes.** Missed it on this task; future workflow has a verify step before commit to catch this.
- **Wire-domain DTOs are worth the duplication.** Keeping `brain-core` rkyv-free pays off when other crates (storage, metadata) need different serialization conventions.
- **rkyv HRTB is finicky** but the `for<'a>` pattern is now standard. The shared `rkyv_codec.rs` (Task 1.8) makes this a one-time cost.
- **Vector-blob layering** — clear in retrospect that the Frame layer owns the raw section. The body codec only handles the rkyv portion. Document this contract in module-level rustdoc on `request` and `response`.
- **Plan size** — this was the biggest sub-task in Phase 1 (~960 lines of new code). It split naturally because the per-variant work is uniform; a smarter macro could halve the line count but would obscure the per-variant fields. Verbose-but-readable wins for spec-faithfulness.
