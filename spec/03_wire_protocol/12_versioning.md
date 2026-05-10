# 03.12 Wire Protocol Versioning

How wire-protocol versions are negotiated and what the compatibility commitments are. This file is the wire-protocol side of the versioning story; for the system-wide picture see [00.03 Versioning](../00_master_overview/03_versioning.md).

## 1. Versions covered here

The wire protocol carries a version field in every frame and a richer version exchange in the handshake. This file covers:

- The single `wire_version: u8` field in the frame header.
- The richer version negotiation in HELLO/WELCOME.
- The compatibility commitment between server versions.
- How feature flags interact with versions.

## 2. The frame-level version field

Every frame carries a `wire_version: u8` byte. Allowed values:

- **0** — reserved (never sent).
- **1** — the v1 wire format defined in this spec.
- **2..255** — reserved for future versions.

After handshake completes, both sides know the negotiated version and every frame uses that version. Frames carrying a version different from the negotiated one are protocol violations and close the connection.

The version byte is in the frame header so that pre-handshake frames (HELLO, WELCOME) carry their own version. This handles the bootstrap problem: the client doesn't yet know what the server speaks; it sends a HELLO with its preferred version and a list of supported versions; the server picks one and responds with WELCOME at the chosen version.

## 3. Handshake version negotiation

The HELLO frame's payload includes:

```
struct HelloPayload {
    preferred_version: u8,              // What the client prefers
    supported_versions: Vec<u8>,        // All versions the client can speak
    sdk_name: String,
    sdk_version: String,
    feature_flags_requested: Vec<String>,
}
```

The WELCOME frame's payload includes:

```
struct WelcomePayload {
    chosen_version: u8,                 // Picked by the server
    server_version: String,             // Brain server build version
    feature_flags_enabled: Vec<String>,
    session_id: SessionId,
}
```

### 3.1 Negotiation rule

The server picks the highest version that:

- The server supports.
- Appears in the client's `supported_versions`.

If no overlap exists, the server returns `WireVersionNegotiationFailed` and closes the connection. The error includes the server's supported versions so the client can report a useful diagnostic.

### 3.2 The HELLO frame's own version

The HELLO frame's `wire_version` field carries the client's `preferred_version`. The server processes the HELLO at that version (or an earlier compatible version) for the purposes of decoding the payload. After WELCOME, the negotiated version is used.

This means the HELLO payload schema must be backward-compatible across versions — otherwise the server couldn't decode a HELLO whose version it doesn't accept (only to send back a clean refusal). In practice, we keep the HELLO payload schema additive only.

## 4. Compatibility commitment

The server's commitment: **a server at wire version N MUST support clients at wire versions N and N-1.**

This gives a one-version compatibility window. It enables rolling upgrades: deploy a new server, old clients keep working; deploy new clients, old servers keep working (until the old servers are upgraded).

### 4.1 What "support" means

For a server at version N receiving a client at version N-1:

- All N-1 opcodes work as defined in the N-1 spec.
- Server's responses use N-1 frame layouts.
- New N opcodes (added since N-1) return `UnknownOpcode` if the client tries to use them.
- New N error codes are mapped to closest N-1 equivalents.
- Bug fixes between N-1 and N apply (clients at N-1 benefit from server-side fixes).

### 4.2 What "support" does not mean

It does not mean:

- N-2 or older clients are supported. They aren't.
- Forward compatibility: an N-1 server accepting an N client. Refused with `WireVersionNegotiationFailed`.
- Cross-major-version migration without WELCOME negotiation. Pre-WELCOME frames at unknown versions are refused.

### 4.3 The contractual implication

A wire version's published spec is forever. Once we publish version N, the byte layouts in version N are immutable. We can add a version N+1 with different layouts; we can't change version N's layouts after publication.

Bug fixes to a version's spec are corrections of unstated invariants, not changes to the wire format. If we ever discover the spec is wrong about some bytes, we follow the existing format and document the spec error in an erratum.

## 5. Adding a new wire version

The process for introducing version N+1:

1. **Spec the new version.** Define the differences from N. Common categories: new opcodes, new payload fields, refined frame headers (rare).
2. **Implement on both sides.** The new version is added; the old is retained.
3. **Test rolling upgrade.** Verify N+1 servers accept N clients and vice versa.
4. **Release.** The N+1 server speaks both N and N+1; the N+1 client prefers N+1.
5. **After enough time, deprecate N.** The N+2 server supports only N+1 and N+2.

The "after enough time" is operationally driven. We don't pre-commit to a deprecation timeline; we observe deployment churn and announce deprecations with notice.

## 6. Feature flags vs versions

The HELLO/WELCOME exchange carries feature flags alongside the version. This is a deliberate split:

- **Versions** govern the byte-level format.
- **Feature flags** govern semantic capabilities that are optional.

Examples of feature flags:

- `gpu_inference` — server supports GPU embedding (relevant only for performance, not correctness).
- `subscribe_v2_filters` — extended filter expressions on SUBSCRIBE.
- `txn_isolation_serializable` — transactions support serializable isolation (otherwise read-committed only).

Feature flags can be added without bumping the wire version, as long as their addition doesn't change byte layouts of existing frames. A feature flag's behavior is documented alongside its definition.

### 6.1 Negotiation

The client requests a set of feature flags in HELLO. The server responds in WELCOME with the subset it actually enables (which may be smaller — the server doesn't enable a flag the client requested but the server doesn't support).

After WELCOME, both sides know the enabled set. Operations that depend on a flag check it; if disabled, they fail with `FeatureNotEnabled`.

### 6.2 Why not just bump the version

Versions multiply test cases. A feature flag is cheaper to maintain than a version: it's an opt-in switch, not a divergent code path through the entire stack.

For changes that are byte-format-level (new field in a frame), versioning is the right tool. For changes that are semantic (new behavior on existing frames), feature flags are.

## 7. The current state

As of this document:

- **Wire version:** unstable. Will be 1 at first stable release.
- **Compatibility:** We are pre-1.0. No compatibility commitments yet.
- **Feature flags:** A nascent set; will be detailed in [14. Observability + Operations](../14_observability_ops/) once it stabilizes.

Once 1.0 ships, the compatibility commitments above apply.

## 8. Diagnostic surfaces

To help operators debug version mismatches:

- The `ADMIN_STATS` opcode reports the server's wire version and supported versions.
- Per-connection statistics include the negotiated version.
- Logs record connections that fail handshake with version-mismatch errors.

These surfaces let operators detect "version skew" — clients lagging server upgrades or vice versa — before the skew exceeds the compatibility window.

## 9. SDK responsibilities

A conforming SDK MUST:

- Send a HELLO with at least the version it was built against in `supported_versions`.
- Accept any negotiated version the server picks, as long as the SDK supports it.
- Refuse to operate if the server picks a version the SDK doesn't support (this shouldn't happen — server picks from the client's list — but defensive code matters).
- Surface version-negotiation errors clearly to the application.

A conforming SDK SHOULD:

- Include a few versions in `supported_versions`, not just the latest, to maintain compatibility with older servers.
- Log the negotiated version and feature flags at connection setup.
- Provide a way for the application to query the negotiated version (useful for application logic that depends on server capabilities).

## 10. The frame-version-mismatch close

If, after handshake, the server receives a frame whose `wire_version` differs from the negotiated version, this is treated as protocol corruption (not a client mistake — there's no path through the SDK that would do this). The connection is closed with no error frame; an out-of-band log entry records the issue.

This is asymmetric with handshake-time mismatch (which gets `WireVersionNegotiationFailed`). The reason: a post-handshake mismatch indicates something is so wrong that further communication is meaningless.

---

*Continue to [`13_open_questions.md`](13_open_questions.md) for unresolved protocol-level questions.*
