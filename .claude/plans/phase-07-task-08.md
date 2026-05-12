# Sub-task 7.8 — LINK / UNLINK handlers — SPEC GAP

This sub-task hits a spec gap I want to surface before implementing.

## 0. The spec gap

- **Spec §09/07** describes LINK and UNLINK as cognitive primitives:
  "LINK creates an edge between two memories. UNLINK removes one."
- **Spec §03/05** (the authoritative wire-opcode table) does **not**
  list LINK or UNLINK. The cognitive-ops opcode range (0x20–0x2A) is
  fully filled with ENCODE, RECALL, PLAN, REASON, FORGET, and
  ENCODE_VECTOR_DIRECT — no LINK/UNLINK opcodes exist.
- **Spec §08/01** lists `Link(LinkRequest)` and `Unlink(UnlinkRequest)`
  as planner-input variants but no spec doc defines `LinkRequest` or
  `UnlinkRequest` shapes.
- Edges flow today via **inline encode-edges**: `EncodeRequest.edges:
  Vec<EdgeRequest>` (max 64 per encode) — already wired in 7.3.

The original 7.x checklist line "Wire variant addition needed"
implies we were expected to extend the wire here. That requires a
spec change, which CLAUDE.md §2 forbids: "The spec is read-only.
Don't edit it. Spec changes go through the user."

## 1. Options

### Option A — Add wire opcodes (spec change)

- Add `LinkReq = 0x25` / `LinkResp = 0xA5` / `UnlinkReq = 0x26` /
  `UnlinkResp = 0xA6` to `brain-protocol::Opcode`.
- Add `LinkRequest` / `LinkResponse` / `UnlinkRequest` /
  `UnlinkResponse` types (mirroring §09/07 §3 + §5).
- Add `RequestBody::Link(_)` / `RequestBody::Unlink(_)` + matching
  `ResponseBody::Link(_)` / `ResponseBody::Unlink(_)` variants.
- Add brain-ops handlers + dispatcher arms.
- Update `spec/03_wire_protocol/05_opcodes.md` to list the new
  opcodes. **This is a spec edit.**
- 0x25 / 0x26 / 0xA5 / 0xA6 are in unassigned (not reserved) regions:
  spec §03/05 §2 marks 0x70–0x7F and 0xF0–0xFE as reserved, so
  filling 0x25–0x2A is a backwards-compatible extension. But the
  spec-table change still needs your sign-off.

### Option B — Library-only handlers, no wire (no spec change)

- Add `brain-ops::link::handle_link` / `handle_unlink` taking
  `LinkRequest` / `UnlinkRequest` Rust structs (lib-only types — in
  `brain-ops`, not `brain-protocol`).
- The writer's idempotency machinery + redb edge tables already do
  the actual work via `tables::edge::link` / `unlink`.
- No dispatcher arm because there's no `RequestBody::Link` variant.
- When the wire spec ships, we re-export the types from
  `brain-protocol` and wire the dispatcher (one-line change).
- Agents needing LINK on the wire today use inline encode-edges.

### Option C — Defer entirely (close 7.8 as "spec-gap-blocked")

- Mark 7.8 as blocked on a spec revision; ship 7.9–7.11 instead.
- LINK/UNLINK live only via inline encode-edges.

## 2. Recommendation

**Option B**, on this reasoning:

- Keeps the spec untouched per CLAUDE.md.
- Makes progress (the redb plumbing is identical to wire LINK; only
  the dispatch/decode layer changes when the wire spec catches up).
- Closes the 7.8 sub-task with concrete shippable code, not just a
  TODO.
- Cheap to upgrade to Option A later: copy the lib-only structs into
  `brain-protocol`, add 4 enum variants + 4 opcode bytes, wire the
  dispatcher.

If you want Option A instead, I can do that and update the spec
table — but that needs explicit approval per the autonomy contract.

## 3. Scope under Option B

**In scope:**

- New module `crates/brain-ops/src/link.rs` (replaces the 7.1 stub
  that took a fictitious `LinkRequest`). Exposes:
  - `LinkRequest` / `UnlinkRequest` structs (plain Rust, no rkyv).
  - `LinkResponse` / `UnlinkResponse` structs.
  - `handle_link(req, ctx) -> Result<LinkResponse, OpError>`.
  - `handle_unlink(req, ctx) -> Result<UnlinkResponse, OpError>`.
- New `WriterHandle` method pair: `submit_link` / `submit_unlink`
  (object-safe like the existing pair). `RealWriterHandle` implements
  both atop redb's `tables::edge::link` / `unlink` with the same
  idempotency-by-RequestId protocol (spec §07/06).
- Validation: both endpoints exist + are non-tombstoned; weight in
  the kind-specific range; same agent_id.
- Tests: 10 integration tests (5 LINK, 5 UNLINK).

**NOT in scope (under Option B):**

- Anything wire-facing. No dispatcher arm. No opcode additions. No
  spec edits.
- Edge-count maintenance (spec §09/07 §11). `tables::edge::link`
  already inserts into `edges_out` + `edges_in`; the denormalized
  `edges_out_count` / `edges_in_count` on memories isn't updated by
  this sub-task — that lives in `MetadataDb::link_with_counts`
  (Phase 3.10 will land it). Documented gap.
- Transaction-bracketed LINK (spec §09/07 §17). Comes with 7.9.

## 4. Wire request / response shapes (lib-only)

```rust
// brain-ops::link::LinkRequest
pub struct LinkRequest {
    pub source: u128,
    pub target: u128,
    pub kind: brain_protocol::request::EdgeKindWire,
    pub weight: f32,           // [0, 1] (or [-1, 1] for Contradicts)
    pub request_id: [u8; 16],
    pub txn_id: Option<[u8; 16]>,
}

pub struct LinkResponse {
    pub source: u128,
    pub target: u128,
    pub kind: brain_protocol::request::EdgeKindWire,
    pub weight: f32,
    pub created_at_unix_nanos: u64,
}

pub struct UnlinkRequest {
    pub source: u128,
    pub target: u128,
    pub kind: brain_protocol::request::EdgeKindWire,
    pub request_id: [u8; 16],
    pub txn_id: Option<[u8; 16]>,
}

pub struct UnlinkResponse {
    pub source: u128,
    pub target: u128,
    pub kind: brain_protocol::request::EdgeKindWire,
    pub removed: bool,
}
```

When Option A lands later, these struct definitions move verbatim to
`brain-protocol::request` / `response` with rkyv attributes added.

## 5. Implementation decisions (Option B)

### 5.1 Validation

- Both `source` and `target` exist (active row in `memories`).
- `weight` in `[0, 1]` for standard kinds; `[-1, 1]` for `Contradicts`
  (spec §09/07 §2).
- `source.agent_id == target.agent_id` (spec §09/07 §10's `CrossAgent`).
- `source != target` for non-self-loop kinds. (Self-edges allowed for
  `SimilarTo`? Spec is silent; we allow them universally for v1.)

### 5.2 Idempotency

Same lookup-then-act pattern as ENCODE/FORGET. The idempotency table
key is `request_id`; the payload caches the response. RequestId reuse
with different `(source, target, kind)` → `WriterError::Conflict`.

### 5.3 Edge data

`EdgeData {
    weight,
    origin: EXPLICIT,
    derived_by: CLIENT,
    created_at_unix_nanos,
    annotation: None,
}` — same shape used for inline encode-edges.

### 5.4 UNLINK no-op semantics (spec §09/07 §5)

"If the edge doesn't exist, `removed: false` and no error." We
implement that by returning `UnlinkResponse { removed: false, ... }`
when `tables::edge::unlink` returns `Ok(false)`.

## 6. Test plan (10 tests)

### LINK (5)

1. `link_inserts_edge` — link A→B Caused, weight=0.7; verify
   edges_out contains the entry.
2. `link_replays_same_request_id` — same RequestId twice → same
   response, no double-insert.
3. `link_conflict_on_request_id_reuse` — same RequestId, different
   target → `WriterError::Conflict`.
4. `link_missing_target_errors` — link to phantom MemoryId →
   error (MemoryNotFound).
5. `link_invalid_weight_errors` — weight=1.5 → InvalidParameters.

### UNLINK (5)

1. `unlink_removes_existing_edge` — link then unlink; second LINK
   for the same triple succeeds with `created_at` updated.
2. `unlink_idempotent_replay` — same RequestId twice → same response.
3. `unlink_non_existent_edge_returns_false_not_error` — unlink an
   edge that was never linked → `removed: false`, no error.
4. `unlink_after_link_then_unlink_returns_false` — link, unlink,
   unlink (new RequestId) → second unlink `removed: false`.
5. `unlink_conflict_on_request_id_reuse` — same RequestId, different
   target → `WriterError::Conflict`.

## 7. Files written / changed (Option B)

```
crates/brain-ops/src/link.rs                [new — handlers + types]
crates/brain-ops/src/lib.rs                 [edit: + pub mod link, re-exports]
crates/brain-ops/src/writer.rs              [edit: + do_link / do_unlink]
crates/brain-ops/src/idempotency.rs         [edit: + LinkOk / UnlinkOk payload kinds]
crates/brain-planner/src/executor/writer.rs [edit: + submit_link / submit_unlink on WriterHandle]
crates/brain-ops/tests/link.rs              [new — 10 integration tests]
```

No new external deps. brain-protocol untouched.

## 8. Verify checklist

- `cargo build -p brain-planner -p brain-ops` clean.
- `cargo test -p brain-planner -p brain-ops` — old + 10 new.
- `cargo clippy -p brain-planner -p brain-ops --all-targets -- -D warnings`
  clean.
- `cargo fmt -p brain-planner -p brain-ops -- --check` no diff.

## 9. Commit message (draft)

```
feat(brain-planner,brain-ops): LINK / UNLINK handlers — lib-only (sub-task 7.8)

The wire-protocol spec §03/05 doesn't yet enumerate opcodes for
LINK / UNLINK; spec §09/07 describes them as cognitive primitives but
the opcode table is silent. This commit ships them as library-only
handlers in brain-ops; the dispatcher gets no new arms because there
are no RequestBody::Link / Unlink variants to dispatch from. When
the wire spec adds the opcodes, the lib-only types move to
brain-protocol verbatim and the dispatcher wires up.

- brain-ops::link::LinkRequest / LinkResponse / UnlinkRequest /
  UnlinkResponse — plain Rust types, no rkyv.
- brain-ops::link::handle_link / handle_unlink — validate endpoints
  exist + non-tombstoned + same agent + weight in range; delegate to
  the writer.
- WriterHandle gains submit_link / submit_unlink (object-safe).
  RealWriterHandle implements both via redb's tables::edge::link /
  unlink with the same lookup-then-act idempotency protocol used by
  ENCODE / FORGET (spec §07/06).
- Idempotency payload kinds: RESPONSE_KIND_LINK = 3,
  RESPONSE_KIND_UNLINK = 4.
- UNLINK on a non-existent edge returns removed=false, no error
  (spec §09/07 §5).

Out of scope: edges_out_count / edges_in_count maintenance on the
memories table (spec §09/07 §11 — Phase 3.10's
MetadataDb::link_with_counts). Tests use the raw redb helpers; the
counts stay at 0 until then.

Tests: 5 LINK + 5 UNLINK integration tests.

No wire-protocol changes. No new external deps.
```

## 10. Risks

- **Lib-only API can drift from a future wire shape.** The struct
  fields are designed to round-trip cleanly into rkyv-archivable
  shapes (all primitive types + the existing `EdgeKindWire`), but
  there's a small risk the wire spec adds fields we didn't anticipate.
  Acceptable — the migration cost is one type-by-type port.
- **`edges_out_count` / `edges_in_count` lie.** Phase 3.10's
  `MetadataDb::link_with_counts` is the right home for these. v1
  reports the raw edge tables; the counts on memory rows stay at 0
  until that wrapper lands. Spec §09/07 §11 acknowledges
  "denormalized" + "may temporarily drift" as expected.
- **No cross-agent check yet.** We currently don't store agent_id
  on memory rows (it's set to nil in the encode handler). The
  CrossAgent check is a no-op for v1 (single-agent deployments). When
  multi-agent lands (Phase 11 or later), the check activates.

## 11. Out-of-scope flags

- No wire variants.
- No spec edits.
- No edge-count maintenance.
- No transaction-bracketed LINK (7.9 deps).
- No bulk LINK.

---

PLAN READY (Option B recommended). Awaiting your call between A / B
/ C before I implement.
