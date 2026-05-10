# 00.03 Versioning

How the spec series evolves over time without confusing readers or implementers.

## Three things get versioned

1. **The spec series** — this collection of documents.
2. **The wire protocol** — the binary format described in [03. Wire Protocol](../03_wire_protocol/).
3. **The on-disk formats** — arena, WAL, redb tables.

These three are independent. A spec series version-bump may or may not change the wire protocol; a wire protocol change may or may not change the on-disk formats.

---

## 1. Spec series versioning

The spec series uses a single integer **format version**, starting at 1. The current version is **format version 1**.

A new format version is published when the cumulative changes since the last version are large enough that a reader benefits from the diff being marked. Format-version bumps are deliberate, infrequent, and announced.

Within a format version, individual spec documents are revised freely. Revisions are tracked in a changelog at the top of each spec's `README.md`.

### What triggers a format-version bump

- A change to the cognitive primitives (adding, removing, or renaming one).
- A change to the architectural layer structure.
- A non-backward-compatible change to vocabulary in the glossary.
- An accumulation of smaller revisions reaching a meaningful threshold.

### What does not trigger a bump

- Adding examples, fixing typos, expanding rationale.
- Filling in previously-deferred details (e.g., resolving an open question).
- Adding new specs (e.g., a future "17. Replication" spec).

---

## 2. Wire protocol versioning

The wire protocol has its own version number, also starting at 1, decoupled from the spec format version.

The `HELLO` frame (first frame from client to server) carries a list of protocol versions the client supports; the `WELCOME` frame carries the server's chosen version. The negotiation rule:

- The server picks the highest version both sides support.
- If no version is mutually supported, the server returns an error and closes the connection.

### Backward compatibility commitment

A server at protocol version N MUST support clients at protocol versions N and N-1. It is not required to support N-2 or older.

This gives a one-version compatibility window — enough for rolling upgrades. Operators upgrade clients ahead of servers (so the server still supports old clients) or vice versa (so new servers still support old clients in transit).

### What triggers a protocol-version bump

- A new opcode that requires server-side handling (additions backward-compatible if servers can ignore unknown opcodes — but Brain's protocol does not, so additions bump the version).
- A change to an existing frame's layout.
- A change to the handshake.

### Forward compatibility

The protocol does not commit to forward compatibility. A protocol version N+1 server may use frame layouts that an N client doesn't understand; the connection fails with a clean version-mismatch error.

---

## 3. On-disk format versioning

Each on-disk format (arena, WAL, metadata) carries an explicit format version in its header.

### Arena format

A 4096-byte header at the start of each arena file:

```
[0..4]    magic = "BARN"           (Brain ARena)
[4..8]    format_version: u32
[8..16]   model_fingerprint: u64 (BLAKE3-derived)
[16..20]  vector_dim: u32
[20..24]  slot_size: u32
[24..32]  reserved
... (remaining bytes for alignment)
```

The format version starts at 1.

### WAL format

The WAL has its own format version in each segment's header. Detailed in [05. Storage: Arena & WAL](../05_storage_arena_wal/) §3.

### Metadata format

redb itself versions its format. Brain layers a logical schema version on top: each `redb` database carries a metadata table with a `schema_version` row.

### Compatibility commitment

A Brain server at on-disk format version N MUST be able to read and write storage files at version N or N-1. Files at N-2 or older require an explicit migration step (a separate `brainctl migrate` command).

When the server starts, it inspects each file's format version. If the file is at the server's version, normal startup proceeds. If the file is one version behind, the server logs a warning and operates in compatibility mode (reads work; writes upgrade the file to the new version on next operation). If the file is two or more versions behind, the server refuses to start and emits a migration instruction.

### What triggers an on-disk version bump

- Changing the arena slot layout.
- Changing the WAL record framing.
- Changing the metadata table schemas.

### What does not trigger a bump

- Adding new optional fields where existing readers can ignore unknown fields.
- Adding new tables to the metadata store (existing readers don't open them).

---

## 4. Migration tools

For format changes that require explicit migration:

- **`brainctl migrate <data_dir>`** reads the existing storage, performs the migration, and writes back. Idempotent — safe to run multiple times.
- Migration is offline: the server is stopped during migration.
- Online migration (rolling) is supported across one version (i.e., from N-1 to N), as part of the compatibility window.

---

## 5. Coordination across versions

The three versions interact:

- A spec format-version bump may or may not change the protocol or storage formats. If it does, the affected version bumps too.
- A protocol-version bump may or may not change storage. Often it doesn't.
- A storage-version bump always corresponds to a code release; the server has to know how to handle the new format.

When a release is announced, the announcement notes:

- Spec format version (e.g., "Format version 1").
- Wire protocol version (e.g., "Protocol version 2").
- On-disk format version (e.g., "Storage version 1; no migration needed").

---

## 6. Long-term frozen deployments

We commit to one version of compatibility. We do not support deployments that stay frozen for many versions.

If you've been running on protocol version 2 for two years and the current is version 5, you have two paths to upgrade:

1. **Upgrade incrementally:** 2 → 3 → 4 → 5, ensuring each step is the supported one-version-at-a-time upgrade.
2. **Stop, migrate offline, restart:** stop the version-2 server, run migration tooling to bring storage to version 5 format, start the version-5 server.

Either works. The second is faster but requires downtime; the first is zero-downtime but requires multiple maintenance windows.

We don't promise long compatibility windows because they multiply test matrix and cost. The trade-off in maintenance burden isn't justified by the small fraction of users with long-frozen deployments.

---

## 7. Pre-1.0 versioning

This document and the entire spec series are pre-release. Format versions before 1.0 (e.g., 0.x) are explicitly *not* committed to compatibility. We may break anything between 0.x versions.

Once Brain reaches 1.0, the compatibility commitments above apply.

---

## 8. The current state

As of this document:

- **Spec format version:** 0.1 (working draft toward 1.0)
- **Wire protocol version:** unstable (will be 1 at first stable release)
- **On-disk format version:** unstable (will be 1 at first stable release)

These will be settled before any production deployment is recommended. Until then, treat all formats as subject to change without notice.
