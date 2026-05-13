# Sub-task 10.4 — Auto-generated UUIDv7 `RequestId`s

**Reads:**
- `spec/13_sdk_design/04_retries.md` §3 (idempotency), §15 (side
  effects), §17 (debug-history reuse of RequestId).
- `spec/09_cognitive_operations/` §error model (idempotency
  semantics, 24-hour TTL).
- `crates/brain-core/src/ids.rs:73-87` — `RequestId(Uuid)` +
  `RequestId::new()` already generate UUIDv7. We don't need a new
  type.

**Phase doc:** `docs/phases/phase-10-sdk-cli.md` §10.4.

**Done when:** Every state-mutating op method (10.5's
`encode/forget/link/unlink/txn_commit`) gets a fresh `RequestId`
by default, and the caller can override per-call. The retry
runner reuses the same `RequestId` across attempts (spec §3) so
the server's 24-hour idempotency cache deduplicates. The
plumbing is in place; 10.5 will hang the op methods off it.

---

## 1. What 10.4 actually delivers

`RequestId` already exists in `brain-core` and generates UUIDv7
via `uuid::Uuid::now_v7()` — there's no new type. What 10.4
adds is the SDK's contract around how `RequestId`s flow:

1. **Re-export** `brain_core::RequestId` from the SDK root so
   callers don't reach across crates.

2. **`Client::next_request_id() -> RequestId`** — public method
   that returns a fresh UUIDv7. 10.5's op-method builders call
   this when the user hasn't supplied one. Each call produces a
   fresh id, so two concurrent ops on the same `Client` get
   distinct ids.

3. **`RequestIdSource` trait** + injectable test fixture.
   `DefaultRequestIdSource` returns `RequestId::new()`;
   `FixedRequestIdSource` (test-only) returns a pre-seeded
   sequence so tests can assert on the wire bytes.
   `Client` carries an `Arc<dyn RequestIdSource>` next to the
   jitter source.

4. **Documentation** in `client/mod.rs` of the
   retry-reuses-request-id rule (spec §13/04 §3). 10.5's
   `run_op` calls will follow this contract: the `RequestId` is
   computed *once* in the op method, then captured in the
   closure passed to `Client::run_op`.

5. **Tiny unit-test surface** that exercises the
   `RequestIdSource` abstraction without touching the wire.

What 10.4 does NOT deliver (10.5):

- The actual op methods (`encode().request_id(id).send()`).
- The builder pattern around per-op overrides.
- Wire-level idempotency assertions (the server has to handle a
  duplicate `RequestId` correctly — that's already verified by
  `brain-ops`'s own idempotency tests).

---

## 2. Module layout

Folder-per-concern preserved:

```
crates/brain-sdk-rust/src/
├── lib.rs                  (+ pub mod request_id; + re-exports)
├── client/mod.rs           (+ next_request_id() + req_id_source field)
├── request_id/             NEW
│   └── mod.rs              RequestIdSource trait + Default / Fixed impls
└── (everything else unchanged)
```

LOC estimates: `request_id/mod.rs` ~120, `client/mod.rs` +30.

---

## 3. Tests

### 3.1 Unit (`request_id/mod.rs`)
- `DefaultRequestIdSource::next()` returns distinct UUIDv7s on
  successive calls. Verify the UUIDv7 variant/version bits.
- `FixedRequestIdSource::new(vec![ids…])` cycles through the
  supplied ids and panics if exhausted.

### 3.2 Unit (`client/mod.rs`)
- `Client::next_request_id()` returns a fresh id each call.
- The cloned client shares the source (same Arc) so ids stay
  unique across clones.

No integration test — without 10.5's op methods there's nothing
to send over the wire. 10.5's tests will cover the
retry-reuses-id case end-to-end.

---

## 4. Risks

| Risk | Mitigation |
| ---- | ---------- |
| `RequestIdSource` adds a generic over `Client` | Avoided. We use `Arc<dyn RequestIdSource>` — same pattern as `JitterSource` in 10.3. Zero generics on `Client`. |
| Order of `RequestId` generation in retries — caller might generate per-attempt by accident | Documented in the doc-comment on `Client::run_op` and on the future op-method builders: "the request_id is computed once before the closure; the closure reuses it across attempts." Enforced by 10.5's op-method shape (closure captures `request_id` by move). |
| Re-exporting `brain_core::RequestId` could drift if brain-core renames the type | Keep the re-export `pub use brain_core::RequestId;` so any rename surfaces as an SDK-build failure. |

---

## 5. Done criteria

- [ ] `src/request_id/mod.rs` ships with
  `RequestIdSource`, `DefaultRequestIdSource`,
  `FixedRequestIdSource` (test-only).
- [ ] `Client` has `req_id_source: Arc<dyn RequestIdSource>`
  field, populated via `DefaultRequestIdSource` in both
  `connect_with` and `new_lazy`.
- [ ] `Client::next_request_id()` is `pub` and documented.
- [ ] `RequestId` re-exported from the SDK root.
- [ ] 2 unit tests pass (source-cycles, client-fresh-each-call).
- [ ] Existing 31/31 tests from 10.1/10.2/10.3 still pass.
- [ ] `just docker-verify` green.
- [ ] Sub-task 10.4 marked `[x]` in `docs/phases/phase-10-sdk-cli.md`.

---

## 6. What 10.4 explicitly defers

- Op methods (`encode/recall/etc`) — 10.5.
- The `.request_id(id)` builder method on op builders — 10.5.
- End-to-end retry-with-same-id assertions — 10.5.
- Wire-level idempotency semantics — already handled in
  `brain-ops`.

---

*Implement on approval.*
