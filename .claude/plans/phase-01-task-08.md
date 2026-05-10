# Plan: Phase 1 — Task 1.8, Response Body Codecs

**Status:** approved (implemented)
**Date:** 2026-05-10
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Add `brain_protocol::response` mirroring the request module: one variant of `ResponseBody` per server-bound opcode in spec §03/08, rkyv codecs for the structured fields, and a streaming-sequence test that encodes a multi-frame stream (`[Next, Next, Complete]` shape) and confirms ordering survives a round-trip.

**Out of scope:**

- Multi-frame transport (Phase 9 owns the network read/write loop and the stream demultiplexer).
- ERROR-frame on-wire numeric encoding for `ErrorCode` / `ErrorCategory` — those enums exist (Task 1.6) but their wire byte values are fixed in the ERROR-payload codec here, and a fuller interop test will appear when we wire the response side of the server.

## 2. Spec references

- `spec/03_wire_protocol/08_response_frames.md` — the 25 response payload schemas (§1–§25).
- `spec/03_wire_protocol/09_streaming.md` — stream lifecycle, EOS semantics, frame interleaving. Particularly:
  - §3 EOS contract: streaming responses set EOS only on the final frame.
  - §3.2 RECALL/PLAN/REASON/ADMIN_MIGRATE_EMBEDDINGS/ADMIN_LIST_TOMBSTONED stream multiple response frames.
  - §11.1 illustrates frame ordering within and across streams.
- `spec/03_wire_protocol/10_errors.md` — already mapped to `ErrorCode` / `ErrorCategory` in Task 1.6; ERROR-frame body just embeds those.

Binding constraints:

- §08 §3: each `RecallResponseFrame` carries `is_final: bool` *redundantly* with the header's EOS flag. We must preserve it so decoders can validate consistency at a higher layer.
- §09 §3.4: "EOS commits the sender to no more frames on this stream." Body codec can't enforce this — it's a Frame-layer / dispatcher invariant — but tests must respect it.
- §08 §25: ERROR body uses `ErrorCode` + `ErrorCategory` from §10, which already exist in `brain_protocol::error`.

## 3. External validation

Not applicable for this sub-task — no new framework. We reuse the rkyv 0.7 plumbing established in Task 1.7 (`AllocSerializer<256>`, `check_archived_root`, the HRTB workaround for `DefaultValidator<'a>`). No web search needed.

## 4. Architecture sketch

```text
brain-protocol/src/response.rs
├── per-opcode response structs (25)
│   ├── EncodeResponse                   (§1, single-frame)
│   ├── (EncodeVectorDirectResponse same shape — alias OR own struct)
│   ├── RecallResponseFrame              (§3, streaming, has is_final)
│   ├── PlanResponseFrame + PlanStep + TransitionKind + PlanStatus
│   ├── ReasonResponseFrame + InferenceStep + InferenceKind + ReasonStatus
│   ├── ForgetResponse                   (§6)
│   ├── SubscriptionEvent + EventType    (§7)
│   ├── UnsubscribeResponse              (§8)
│   ├── TxnBeginResponse / Commit / Abort (§9–§11)
│   ├── CancelStreamAck                  (§12)
│   ├── PongResponse                     (§13)
│   ├── ServerPingRequest                (§14, server→client despite name)
│   ├── AdminStatsResponse + StatsSummary + ShardStats + ContextStats + SalienceHistogram (§15)
│   ├── AdminSnapshot/Restore/IntegrityCheck/MigrateEmbeddings (§16–§19, last is streaming)
│   ├── AdminCreateContext / RenameContext / MoveMemory / Reclassify (§20–§23)
│   ├── AdminListTombstonedResponseFrame + TombstonedMemoryInfo (§24, streaming)
│   └── ErrorResponse + ErrorDetails     (§25, embeds ErrorCode/Category)
│
├── enum ResponseBody { ... }            // dispatch enum, ~25 variants
│
└── impl ResponseBody {
        fn opcode(&self) -> Opcode;
        fn is_final(&self) -> Option<bool>;     // None for non-streaming responses
        fn encode(&self) -> Vec<u8>;
        fn decode(opcode: Opcode, &[u8]) -> Result<Self, ProtocolError>;
    }
```

Shape mirrors `RequestBody` (Task 1.7). Reuse the `to_rkyv_bytes` / `from_rkyv_bytes` helpers — promote them to a small `crate::rkyv_codec` module so request and response share the implementation.

`is_final()` returns `Some(bool)` for the streaming frame variants (Recall/Plan/Reason/AdminMigrate/AdminListTombstoned/Subscribe) and `None` for unary responses. Surfacing this lets the Frame-layer dispatcher cross-check against the header's `EOS` flag (a Phase 9 concern, but we expose the hook now).

`ErrorResponse` re-uses the `ErrorCode` and `ErrorCategory` enums from Task 1.6. We add rkyv-archivable mirror enums in `response.rs` (one variant per code/category) since `ErrorCode` is `#[non_exhaustive]` and not currently rkyv-derived. The mirrors carry the same variant set; conversion is `match` boilerplate.

Wire-domain types are the same as in `request.rs`: `WireUuid`, `WireMemoryId`, byte-tagged `MemoryKindWire` / `EdgeKindWire`. Re-export them via `request` and use them directly in `response`.

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| **Chosen:** one struct per response, plain `ResponseBody` enum, rkyv per-variant; promote codec helpers to `rkyv_codec` module. | Mirrors request side exactly; keeps each frame's data shape intact; supports streaming naturally (one struct = one frame); easy round-trip tests. | Lots of variants (~25). | ✓ |
| Combine multi-frame streaming into a single `Vec<RecallResult>` body. | One body per logical recall. | Defeats streaming — server must buffer all results before sending; loses ability to start delivering early results. Spec §08 §3 explicitly says streaming. | rejected — violates spec |
| Generic `Streamed<T> { items: Vec<T>, is_final: bool }` wrapping per-opcode payloads. | Unified streaming abstraction. | Doesn't match the spec's per-opcode struct shape (each has its own status / progress / metadata fields). Forces awkward `T = (Vec<MemoryResult>, ...)`. | rejected — spec resists this generalization |
| Re-use the rkyv `ErrorCode` derive directly on the existing `error::ErrorCode` enum. | One source of truth. | The enum is `#[non_exhaustive]` (intentional, per Task 1.6 docstring) and adding rkyv derives on a non-exhaustive enum is fragile. | rejected — keep `ErrorCode` consumer-friendly; mirror in `response` |

## 6. Risks / open questions

- **`is_final` redundancy:** spec §08 §3 says `is_final` in the body matches the header's EOS flag. There's no resolution in `13_open_questions.md` for whether mismatch is a hard error or just a "SHOULD." Mitigation: encode/decode preserves the field as-is; Phase 9's dispatcher decides the policy (we'll surface this when we get there).
- **`EncodeResponse` vs `EncodeVectorDirectResponse`:** spec §08 §2 says they have the same shape. Two options: (a) one struct, two `ResponseBody` variants both carrying it; (b) separate types. Choosing (b) for clarity at the wire layer — a third variant later may diverge.
- **Cancellation EOS:** §09 §5.1 + §13.3 PR-OQ-2 (open question) — should `CANCEL_STREAM_ACK` carry an explicit "Cancelled" reason, or is the EOS-with-empty-payload sufficient? Not relevant for body codec; we just round-trip the spec'd struct.

## 7. Test plan

Per phase-doc Done-when:

- **All response opcodes have variants and codecs.**
  Maps to: a per-variant round-trip test asserting `encode → decode == original` for every variant. ~25 unit tests grouped by family (single-frame, streaming, admin, error).
- **Streaming protocol tested (encoding/decoding shape).**
  Maps to: `streaming_sequence_round_trips` — build `[RecallResponseFrame { is_final: false }, RecallResponseFrame { is_final: false }, RecallResponseFrame { is_final: true }]`, encode each, decode each in order, assert original `Vec` equals decoded `Vec`. Pin that ordering survives.

Negative tests:

- `decode` with a request opcode returns `UnknownOpcode`.
- `decode` of garbage bytes for a real response opcode returns `MalformedPayload`.
- `is_final()` returns `Some(false)` / `Some(true)` / `None` matching variant kind.

## 8. Commit shape

One commit:

> `1.8: implement ResponseBody codec for all 25 client-bound opcodes`

Includes:

1. Promote `to_rkyv_bytes` / `from_rkyv_bytes` from `request.rs` to a new `crate::rkyv_codec` module (private). Update `request.rs` to import from there.
2. Add `crates/brain-protocol/src/response.rs` with all response structs and the `ResponseBody` enum.
3. Re-export `ResponseBody` from `lib.rs`.
4. Tests as in §7 above.
5. Update phase doc — mark sub-task 1.8 as `[x]`.

If the file ends up >1000 lines (likely; request is ~900), no need to split — pattern is uniform and the work is mechanical.

## 9. Confirmation

Awaiting "go" / "approved" / specific revisions.
