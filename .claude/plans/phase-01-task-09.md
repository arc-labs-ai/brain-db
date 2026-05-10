# Plan: Phase 1 — Task 1.9, Handshake

**Status:** approved (implemented)
**Date:** 2026-05-10
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Add `brain_protocol::handshake` covering all four handshake messages from spec §03/06: `HELLO`, `WELCOME`, `AUTH`, `AUTH_OK`. Implement rkyv codecs for each, version/capability negotiation logic with a `negotiate` function, and round-trip + negotiation tests. Wire the four payloads into `RequestBody` (HELLO, AUTH — server-bound) and `ResponseBody` (WELCOME, AUTH_OK — client-bound) so frame dispatch is uniform.

**Out of scope:**

- Actual auth backend (token verification, mTLS validation, agent lookup) — owned by Phase 9 / brain-server.
- Session resumption (`client_session_token` reserved per §06 §2.4 / §8 — explicitly v2).
- Multi-AUTH (§06 §9 — explicitly v2).
- Handshake timing / timeout enforcement (§06 §6.3) — that's the connection layer in Phase 9.

**Naming reconciliation:** the phase doc says "`ClientHello` / `ServerHello`," but the spec uses the names `HELLO` / `WELCOME` / `AUTH` / `AUTH_OK`. The spec wins; payload structs will be `HelloPayload`, `WelcomePayload`, `AuthPayload`, `AuthOkPayload`. Phase doc gets an inline correction note.

## 2. Spec references

- `spec/03_wire_protocol/06_handshake.md` — the four-message handshake, payload schemas, version/capability negotiation, failure paths.
  - §1 — sequence (HELLO → WELCOME → AUTH → AUTH_OK).
  - §2 `HelloPayload` + `HelloCapabilities` (client_id ≤ 256 bytes, supported_versions, capabilities, optional client_session_token).
  - §3 `WelcomePayload` + `ServerFeatures` (server picks highest mutual version per §3.1; otherwise `VersionNotSupported`).
  - §4 `AuthPayload` + `AuthCredentials` + `MtlsClaim` (method must be one of `WELCOME.auth_methods`).
  - §5 `AuthOkPayload` + `AgentPermissions` (binds connection to agent_id and shard_id).
  - §6 failure paths (server emits ERROR + close on `VersionNotSupported`, `Unauthenticated`).
- `spec/03_wire_protocol/05_opcodes.md` §1.1 — opcodes 0x01 HELLO, 0x81 WELCOME, 0x02 AUTH, 0x82 AUTH_OK already in `Opcode` enum (Task 1.3).
- `spec/03_wire_protocol/04_payload_encoding.md` — rkyv structured encoding (we already have the pipeline from Task 1.7).

Binding constraints:

- §06 §3.1: server picks the **highest mutually-supported** version. Negotiation is deterministic.
- §06 §4.1: `AUTH.method` must appear in `WELCOME.auth_methods`. `negotiate` only handles version+capabilities; auth-method intersection happens at the auth-frame layer (later phase) since it requires the server-side decision.
- §06 §1: handshake frames all carry `stream_id = 0` and `flags = EOS`. That's enforced by the Frame layer (whoever writes the frame); the body codec doesn't know.
- `client_id` and `server_id` ≤ 256 bytes — validation rule, not a hard codec invariant. We document in rustdoc and add an assertion at the validation layer in Phase 9.

## 3. External validation

Not applicable — reuses the rkyv 0.7 plumbing established in Tasks 1.7 / 1.8 and the shared `rkyv_codec` helpers. No new dependency, no new framework. The handshake protocol is entirely defined by the spec; no need to cross-reference industry standards.

## 4. Architecture sketch

```text
brain-protocol/src/handshake.rs

// §06 §2 — HELLO body
pub struct HelloPayload {
    pub client_id: String,
    pub supported_versions: Vec<u8>,
    pub capabilities: HelloCapabilities,
    pub client_session_token: Option<[u8; 32]>,
}
pub struct HelloCapabilities { streaming, compression_zstd, server_push: bool }

// §06 §3 — WELCOME body
pub struct WelcomePayload {
    pub server_id: String,
    pub chosen_version: u8,
    pub session_id: [u8; 16],
    pub capabilities: HelloCapabilities,    // intersected
    pub server_features: ServerFeatures,
}
pub struct ServerFeatures { max_payload_size, max_concurrent_streams,
                            idle_timeout_seconds: u32, auth_methods: Vec<AuthMethod> }
pub enum AuthMethod { Token, Mtls, None }

// §06 §4 — AUTH body
pub struct AuthPayload {
    pub method: AuthMethod,
    pub agent_id: WireUuid,
    pub credentials: AuthCredentials,
}
pub enum AuthCredentials { Token(Vec<u8>), Mtls(MtlsClaim), None }
pub struct MtlsClaim { cert_fingerprint: [u8; 32], asserted_subject: String }

// §06 §5 — AUTH_OK body
pub struct AuthOkPayload {
    pub agent_id: WireUuid,
    pub bound_shard_id: u16,
    pub permissions: AgentPermissions,
    pub server_time_unix_nanos: u64,
}
pub struct AgentPermissions { can_encode, can_recall, can_plan, can_reason,
                              can_forget, can_admin: bool }

// Negotiation

/// Server's local view of what it supports — input to `negotiate`.
pub struct ServerCapabilities {
    pub supported_versions: Vec<u8>,
    pub capabilities: HelloCapabilities,
    pub server_features: ServerFeatures,
    pub server_id: String,
}

/// Result of a successful version+capabilities handshake.
pub struct NegotiatedSession {
    pub chosen_version: u8,
    pub capabilities: HelloCapabilities,    // intersection
}

/// Pick the highest mutually-supported version and intersect
/// capabilities. Returns `ProtocolError::BadVersion` (mapped to
/// `VersionNotSupported` in §10 §3.2) if no overlap exists.
pub fn negotiate(client: &HelloPayload, server: &ServerCapabilities)
    -> Result<NegotiatedSession, ProtocolError>;
```

`HelloCapabilities` is shared between HELLO and WELCOME. The intersection rule for negotiation: `streaming` is always true (spec mandates it in v1); `compression_zstd` and `server_push` are AND-ed across client and server.

The four payload types each get their own rkyv derive (`Archive`, `Serialize`, `Deserialize`, `check_bytes`). The handshake module re-uses `WireUuid` (from `request`) for `agent_id`.

`RequestBody` adds two variants:
- `Hello(HelloPayload)` for opcode 0x01.
- `Auth(AuthPayload)` for opcode 0x02.

`ResponseBody` adds two variants:
- `Welcome(WelcomePayload)` for opcode 0x81.
- `AuthOk(AuthOkPayload)` for opcode 0x82.

Existing `decode` arms for those opcodes (which currently return `UnknownOpcode`) start succeeding once the variants exist.

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| **Chosen:** four payload types in `handshake.rs`, surfaced through `RequestBody` / `ResponseBody`. | Uniform frame dispatch (every C→S body is a `RequestBody`); spec-faithful naming; tests live with the codec. | Some cross-module coupling — `request.rs` and `response.rs` must import handshake types. | ✓ |
| Keep handshake completely separate; have a top-level `Body` enum that's `Request | Response | Handshake`. | Highlights the connection-level vs stream-level distinction. | Burns a layer of indirection for marginal taxonomic clarity; the EOS / stream_id rules already encode the connection-vs-stream split at the Frame layer. | rejected |
| Put `HelloCapabilities` inline (duplicate in both Hello and Welcome). | No shared type, no risk of accidental coupling. | Drift risk: spec says they share a shape; duplication invites divergence on a future spec bump. | rejected |
| Have `negotiate` return `WelcomePayload` directly. | One-shot helper. | Conflates pure logic (version+capability intersection) with frame-payload construction (server chooses session_id, server_features). The server decides those at runtime; tests want to exercise pure negotiation in isolation. | rejected |

## 6. Risks / open questions

- **Auth method intersection:** §06 §4.1 says `AUTH.method` must be in `WELCOME.auth_methods`. That check happens when the server validates the AUTH frame, not during pre-AUTH negotiation. Out of scope for `negotiate`; surfaced as an explicit doc-comment.
- **Session resumption:** `client_session_token` and `session_id` exist in payloads but aren't used in v1 (§06 §8). Codec round-trips them as-is; we don't gate on them.
- **`AuthMethod::None` in production:** spec §06 §4.5 allows it for trusted-network deployments only. Codec accepts it; policy enforcement lives in the auth backend.
- **Wire byte values for `AuthMethod`:** the spec doesn't pin numeric values for the enum variants. We assign `Token = 0`, `Mtls = 1`, `None = 2` and document them as stable wire values; future spec changes require a wire-version bump per §07 of the opcodes spec.
- **Naming drift in phase doc:** "ClientHello / ServerHello" is imprecise. Plan adopts spec names; commit message and phase-doc footnote call out the correction so future readers don't get confused.

## 7. Test plan

Per phase-doc Done-when:

- **Hello messages round-trip.** Maps to:
  - `hello_payload_round_trips`
  - `welcome_payload_round_trips`
  - `auth_payload_round_trips_each_method` (Token, Mtls, None)
  - `auth_ok_payload_round_trips`
- **Negotiation logic matches the spec's compatibility matrix.** Maps to:
  - `negotiate_picks_highest_mutual_version` — client `[1, 2, 3]`, server `[1, 2]` → chosen `2`.
  - `negotiate_one_overlap` — client `[1]`, server `[1, 2]` → chosen `1`.
  - `negotiate_no_overlap_fails` — client `[3]`, server `[1, 2]` → `BadVersion { got: 3, expected: 2 }` (or a structured error capturing both lists).
  - `negotiate_intersects_capabilities` — server lacks `compression_zstd`, client has it → result `compression_zstd = false`.
  - `negotiate_streaming_always_true_in_v1` — both sides set `streaming = true`; result preserves it.

Plus integration with body enums:

- `request_body_hello_round_trips` — encode+decode through `RequestBody::Hello`.
- `request_body_auth_round_trips` — encode+decode through `RequestBody::Auth` for each method.
- `response_body_welcome_round_trips`.
- `response_body_auth_ok_round_trips`.

## 8. Commit shape

One commit:

> `1.9: implement handshake codec (HELLO, WELCOME, AUTH, AUTH_OK) and negotiation`

Includes:

1. `crates/brain-protocol/src/handshake.rs` with payload structs, helper enums, `ServerCapabilities`, `NegotiatedSession`, `negotiate`.
2. Update `request.rs` — add `Hello`, `Auth` variants and dispatch arms.
3. Update `response.rs` — add `Welcome`, `AuthOk` variants and dispatch arms.
4. Update `lib.rs` — register module, re-exports.
5. Tests (per §7).
6. Phase-doc 1.9 marked `[x]`; brief inline note correcting `ClientHello/ServerHello` → `HelloPayload/WelcomePayload`.

Estimated diff: ~600–800 lines (handshake.rs ~400, modest deltas to request/response).

## 9. Confirmation

Awaiting "go" / "approved" / specific revisions.
