# Handshake

Every Brain connection opens with a four-frame dance:

```
Client                                  Server
  │  TCP (+ optional TLS)                  │
  │                                        │
  │  HELLO (0x0001) ─────────────────────► │
  │  ◄───────────────────  WELCOME (0x0081)│
  │  AUTH  (0x0002) ─────────────────────► │
  │  ◄───────────────────  AUTH_OK (0x0082)│
  │                                        │
  │  (connection ready; data plane opens)  │
```

All four frames carry `stream_id = 0` and the `EOS` flag.
Payloads are `rkyv`-encoded.

**Source:** `crates/brain-protocol/src/handshake.rs`. **Spec:**
§03/06.

---

## HELLO (`0x0001`, C → S)

```rust
struct HelloPayload {
    client_id: String,                      // ≤ 256 bytes; "brain-rust-sdk/0.5.0"
    supported_versions: Vec<u8>,            // e.g. [1]
    capabilities: HelloCapabilities,
    client_session_token: Option<[u8; 32]>, // reserved (session-resumption, v2)
}

struct HelloCapabilities {
    streaming: bool,         // always true in v1
    compression_zstd: bool,  // reserved
    server_push: bool,       // reserved
}
```

`client_id` is observability-only — it surfaces in traces and
the admin `agents` table.

## WELCOME (`0x0081`, S → C)

```rust
struct WelcomePayload {
    server_id: String,                  // ≤ 256 bytes; "brain-server/0.5.0"
    chosen_version: u8,                 // highest mutual version
    session_id: [u8; 16],               // 16 random bytes; per-connection
    capabilities: HelloCapabilities,    // AND-intersection of advertised caps
    server_features: ServerFeatures,
}

struct ServerFeatures {
    max_payload_size: u32,        // default 16 MiB
    max_concurrent_streams: u32,  // default 1024
    idle_timeout_seconds: u32,    // default 300; server emits SERVER_PING after
    auth_methods: Vec<AuthMethod>,
}

enum AuthMethod {
    Token = 0,
    Mtls  = 1,
    None  = 2,
}
```

### Version negotiation

The server picks the highest version that satisfies:

1. Server supports it.
2. Client lists it in `supported_versions`.

No mutual version → `Error(VersionNotSupported)` and the
connection closes (`handshake.rs:262–299`).

Per spec §03/12, the v1 wire protocol is **unstable until Brain
v1.0.0 tags**. Compatibility commitments begin after the tag.

### Capability negotiation

Capabilities are AND-intersected (bitwise AND of booleans).
`streaming` must be `true` on both sides in v1.

## AUTH (`0x0002`, C → S)

```rust
struct AuthPayload {
    method: AuthMethod,           // must appear in WELCOME's auth_methods
    agent_id: WireUuid,           // 16-byte UUID
    credentials: AuthCredentials,
}

enum AuthCredentials {
    Token(Vec<u8>),               // opaque bearer token
    Mtls(MtlsClaim),              // claim about the TLS cert
    None,
}

struct MtlsClaim {
    cert_fingerprint: [u8; 32],   // SHA-256 of the client cert
    asserted_subject: String,     // SAN / CN the client claims
}
```

### Auth state today

- `AuthMethod::None` is the only fully-wired path in v1.0 — see
  [`../../guides/security/auth-modes.md`](../../guides/security/auth-modes.md).
- `Token` and `Mtls` payload shapes are stable; the backend
  verification logic lands in Phase 14+.
- The server will accept and decode all three variants but
  reject `Token` / `Mtls` with `AuthBackendUnavailable` until
  the verifier is wired.

## AUTH_OK (`0x0082`, S → C)

```rust
struct AuthOkPayload {
    agent_id: WireUuid,             // echoed (server may canonicalise)
    bound_shard_id: u16,             // shard this connection will be routed to
    permissions: AgentPermissions,
    server_time_unix_nanos: u64,    // for clock-skew detection
}

struct AgentPermissions {
    can_encode: bool,
    can_recall: bool,
    can_plan:   bool,
    can_reason: bool,
    can_forget: bool,
    can_admin:  bool,   // required for ADMIN_* opcodes
}
```

After `AUTH_OK`, the connection is bound to:

- One agent (`agent_id`).
- One shard (`bound_shard_id`).
- A fixed permission set for the connection's lifetime.

All subsequent frames carrying a different agent's data → `WrongShard`
or `PermissionDenied`.

## Failure modes

| Failure | Reaction |
|---|---|
| Bad magic / CRC during HELLO | Server emits `Error(BadMagic/BadHeaderCrc)` and closes the TCP socket. |
| `VersionNotSupported` | Server emits the error frame and closes. Client should not retry with the same version list. |
| `Unauthenticated` in AUTH | Server emits the error frame, closes the connection. Client may reconnect with different credentials. |
| Idle timeout | After `idle_timeout_seconds`, server emits `SERVER_PING`. Two unanswered pings → connection close. |
| Client sends an op-frame before `AUTH_OK` | Server emits `Error(NotAuthenticated)`, closes. |

## See also

- [`frame-format.md`](frame-format.md) — frame-level mechanics.
- [`opcodes.md`](opcodes.md) — every opcode, including the four handshake frames.
- [`error-codes.md`](error-codes.md) — handshake failure codes.

**Spec:** §03/06 (handshake), §03/12 (versioning). **Source:** `crates/brain-protocol/src/handshake.rs`.
