# 04.04 Handshake

The connection handshake establishes the protocol version, the session, and the authenticated agent. This file specifies the four frames involved: `HELLO`, `WELCOME`, `AUTH`, `AUTH_OK`.

## 1. The handshake sequence

```
Client                                    Server
  │                                         │
  │ TCP connect (and TLS, if enabled)       │
  │                                         │
  │ HELLO ────────────────────────────────► │
  │                                         │ (validate magic, version range,
  │                                         │  client_id, etc.)
  │ ◄──────────────────────────── WELCOME   │
  │                                         │
  │ AUTH ─────────────────────────────────► │
  │                                         │ (validate credentials, look up agent)
  │ ◄────────────────────────── AUTH_OK     │
  │                                         │
  │     [authenticated; operations flow]    │
```

After `AUTH_OK`, the connection is in the "established" state and operations can flow.

If the handshake fails at any step, the server sends an `ERROR` frame and closes the connection.

## 2. HELLO

The first frame the client sends after TCP/TLS establishment. Contains:

```rust
struct HelloPayload {
    client_id: String,              // human-readable; e.g., "my-app/1.0"
    supported_versions: Vec<u8>,    // protocol versions the client supports, e.g., [1]
    capabilities: HelloCapabilities,
    client_session_token: Option<[u8; 32]>,  // for resumption (not currently used)
}

struct HelloCapabilities {
    streaming: bool,                 // always true
    compression_zstd: bool,          // not currently used; reserved
    server_push: bool,               // not currently used; reserved
}
```

Frame layout:

- `opcode = 0x01` (HELLO)
- `stream_id = 0`
- `flags = EOS`
- `payload`: CBOR-encoded `HelloPayload`

### 2.1 client_id

A free-form string identifying the client. Used for logging and observability. Format suggestion: `"<client_name>/<version>"`, e.g., `"my-app/1.0"`.

Maximum length: 256 bytes.

### 2.2 supported_versions

The list of protocol versions the client can speak. The server picks one to use; see §3 below.

Currently this is `[1]`. As newer protocol versions are introduced, clients list both old and new.

### 2.3 capabilities

A struct of feature flags the client supports. The server's `WELCOME` confirms which the server also supports; the connection uses only mutually-supported features.

Currently the only required capability is `streaming = true`.

### 2.4 client_session_token

Reserved for future session-resumption (post-failover or after brief disconnects). Not currently used; clients send `None`.

## 3. WELCOME

The server's response to `HELLO`. Contains:

```rust
struct WelcomePayload {
    server_id: String,              // human-readable; e.g., "brain-server/0.5.0"
    chosen_version: u8,             // protocol version for this connection
    session_id: [u8; 16],           // server-allocated, unique to this session
    capabilities: HelloCapabilities, // mutually-supported
    server_features: ServerFeatures,
}

struct ServerFeatures {
    max_payload_size: u32,           // server's max accepted payload (default 16 MiB)
    max_concurrent_streams: u32,    // per-connection
    idle_timeout_seconds: u32,      // before server sends SERVER_PING
    auth_methods: Vec<AuthMethod>,  // which AUTH credentials the server accepts
}

enum AuthMethod {
    Token,
    Mtls,
    None,                            // for test/dev only
}
```

Frame layout:

- `opcode = 0x81` (WELCOME)
- `stream_id = 0`
- `flags = EOS`
- `payload`: CBOR-encoded `WelcomePayload`

### 3.1 chosen_version

The server picks the highest protocol version that:

- Appears in the client's `supported_versions` list.
- Is supported by the server.

If no mutual version exists, the server sends `ERROR(VersionNotSupported)` and closes.

### 3.2 session_id

A server-allocated unique identifier for this connection session. Format: 16 random bytes (cryptographically random, but doesn't need to be UUID-formatted).

The `session_id` is used in:

- Logs (correlating client and server logs for the same connection).
- Tracing (as a span attribute).
- Future session resumption (not currently used).

### 3.3 server_id

Free-form string identifying the server. Format: `"<server_name>/<version>"`. Maximum 256 bytes.

### 3.4 server_features

Server-declared parameters that affect client behavior:

- `max_payload_size` — clients MUST NOT send frames with payload exceeding this size. Default 16 MiB.
- `max_concurrent_streams` — clients SHOULD limit concurrent stream count to this. Default 1024.
- `idle_timeout_seconds` — after this much idle time, server sends `SERVER_PING`. Default 300 (5 min).
- `auth_methods` — which methods the server accepts in `AUTH`.

## 4. AUTH

The client's authentication credentials. Sent immediately after `WELCOME`.

```rust
struct AuthPayload {
    method: AuthMethod,
    agent_id: AgentId,                       // 16 bytes; the agent claiming identity
    credentials: AuthCredentials,
}

enum AuthCredentials {
    Token(Vec<u8>),                          // bearer token
    Mtls(MtlsClaim),                         // mTLS-presented certificate
    None,                                    // for test/dev only
}

struct MtlsClaim {
    cert_fingerprint: [u8; 32],              // SHA-256 of the client's cert
    asserted_subject: String,                // the subject the client claims
}
```

Frame layout:

- `opcode = 0x02` (AUTH)
- `stream_id = 0`
- `flags = EOS`
- `payload`: CBOR-encoded `AuthPayload`

### 4.1 method

The auth method the client is using. Must be one of those declared in `WELCOME.auth_methods`.

### 4.2 agent_id

The agent the client is identifying as. The server's authentication backend confirms (or denies) that the credentials authorize this `agent_id`.

This is the field that binds the connection to an agent. From this point on, every operation is authorized as this agent.

### 4.3 Token authentication

For `method = Token`:

- The token is opaque to the protocol — it's whatever the operator's auth system issued.
- Token validation is delegated to a configurable backend (JWT verification, OAuth introspection, or static-token tables).
- The server verifies the token and binds the agent_id.

### 4.4 mTLS authentication

For `method = Mtls`:

- The server already received the client's certificate during the TLS handshake.
- The `MtlsClaim` confirms the client's expected cert fingerprint and asserted subject.
- The server matches the asserted subject against its agent_id mapping (typically Subject Alternative Name or Common Name).

### 4.5 None

For `method = None`:

- No credentials. Allowed only when the server's policy permits.
- Used for test, dev, and trusted-network deployments.
- Server still binds the connection to the requested `agent_id`; trust is transitive from the network.

## 5. AUTH_OK

The server's acknowledgment of successful authentication.

```rust
struct AuthOkPayload {
    agent_id: AgentId,                       // confirmed agent_id
    bound_shard_id: u16,                     // runtime shard ID for this agent
    permissions: AgentPermissions,
    server_time_unix_nanos: u64,             // server's current time, for clock-skew check
}

struct AgentPermissions {
    can_encode: bool,
    can_recall: bool,
    can_plan: bool,
    can_reason: bool,
    can_forget: bool,
    can_admin: bool,                         // typically false for normal agents
}
```

Frame layout:

- `opcode = 0x82` (AUTH_OK)
- `stream_id = 0`
- `flags = EOS`
- `payload`: CBOR-encoded `AuthOkPayload`

### 5.1 bound_shard_id

The shard ID this agent is bound to. The client uses it for routing optimizations (knowing which shard owner serves the connection).

### 5.2 permissions

The agent's permitted operations. Typically all operations are allowed; admin operations require elevated permissions.

If the agent attempts an operation outside its permissions (e.g., calling `ADMIN_SNAPSHOT` without `can_admin`), the server returns `ERROR(PermissionDenied)`.

### 5.3 server_time_unix_nanos

The server's current time, for the client to check clock skew. If the client's clock differs from the server's by more than a configurable threshold (default: 1 second), the client SHOULD warn (some operations are time-sensitive).

## 6. Failure paths in the handshake

### 6.1 Bad HELLO

If the server rejects the HELLO (invalid magic, no mutually-supported version, oversize payload), it sends an `ERROR` frame and closes:

```
S → C: ERROR(stream_id=0, EOS)
       payload: {code: VersionNotSupported, message: "client supports [3,4]; server supports [1,2]"}
S: closes connection
```

### 6.2 Bad AUTH

If authentication fails, the server sends `ERROR` and closes:

```
S → C: ERROR(stream_id=0, EOS)
       payload: {code: Unauthenticated, message: "token rejected"}
S: closes connection
```

The error message MAY include detail about why auth failed (token expired, no matching agent, etc.) but SHOULD NOT include sensitive information.

### 6.3 Timeout

If the client doesn't send AUTH within a reasonable time after WELCOME (default: 30 seconds), the server times out and closes the connection.

If the server doesn't send WELCOME or AUTH_OK within a reasonable time (default: 30 seconds for each), the client times out and closes.

## 7. After the handshake

Once `AUTH_OK` is received, the client may send any operation. The server processes operations subject to the connection's permissions.

If the client tries to send operations before AUTH_OK (e.g., right after WELCOME, before sending AUTH), the server responds with `ERROR(NotAuthenticated)` and the operation is rejected.

## 8. Resumption (deferred)

The protocol reserves a `client_session_token` field in HELLO and a session_id mechanism. These are intended for future session resumption — re-establishing a connection without re-authenticating, useful for transient network failures.

Currently, resumption is not implemented. Disconnections require full re-handshake on reconnect. This is reserved for a future major version.

## 9. Multi-AUTH

Some applications would benefit from changing identity within a connection — e.g., a proxy serving many users wants to multiplex their identities over one underlying connection. Brain does not support this; one AUTH per connection.

For multi-tenant proxies, the recommended pattern is one connection per identity (subject to connection-pool limits). Multi-AUTH is an open question for a future major version.

## 10. Identity is bound to the API key, not carried in requests

Brain derives the caller's identity from the authenticated API key, not from a per-request `agent_id` field. The AUTH step issues a per-tenant scope binding: the key carries `(user, agent, namespace)` claims and the server treats those as authoritative for every operation on the connection. Clients never construct or send a scope object; the server fills it from the key.

This closes a class of impersonation bugs at the wire boundary. With identity carried in the request, a client that constructs the wrong `agent_id` can write into another agent's namespace; with identity bound to the key, the same request is rejected at the handshake. Operations that legitimately act across agents (admin migration, snapshot scope) require an admin key with explicit cross-agent permissions, not a client key with a different `agent_id` substituted in.

`agent_id` is not carried on client-facing requests; it is derived server-side from the authenticated connection key. The server rejects any request that carries a redundant `agent_id` field.

## 11. Handshake summary

The full handshake exchange:

```
[TCP, then optionally TLS]

C → S: HELLO              (32-byte header + ~100-200 byte payload)
S → C: WELCOME            (32-byte header + ~150-300 byte payload)
C → S: AUTH               (32-byte header + ~200-2000 byte payload, depending on method)
S → C: AUTH_OK            (32-byte header + ~50 byte payload)

[connection established; operations flow]
```

Total handshake bytes (typical): ~700-2000 bytes. Latency: 2 RTT after TCP/TLS, plus auth backend latency.

---

*Continue to [`05_frame_layouts.md`](05_frame_layouts.md) for per-opcode request layouts.*
