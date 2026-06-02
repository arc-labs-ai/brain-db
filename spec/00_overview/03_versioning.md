# 00.03 Versioning

How the spec series, the wire protocol, and on-disk formats are versioned.

## Three things get versioned

1. **The spec series** — this collection of documents.
2. **The wire protocol** — the binary format described in [04. Wire Protocol](../04_wire_protocol/00_purpose.md).
3. **The on-disk formats** — arena, WAL, redb tables.

The spec series tracks the documentation. The wire protocol and on-disk formats are tied to a single Brain release: each release ships one wire version and one storage format, and a server only speaks to clients at the same wire version.

---

## 1. Spec series versioning

The spec series uses a single integer **format version**, starting at 1. The current version is **format version 1**.

A new format version is published when the cumulative changes since the last version are large enough that a reader benefits from the diff being marked. Format-version bumps are deliberate, infrequent, and announced.

Within a format version, individual spec documents are revised freely. Revisions are tracked in a changelog at the top of each spec's `00_purpose.md`.

### What triggers a format-version bump

- A change to the cognitive primitives (adding, removing, or renaming one).
- A change to the architectural layer structure.
- A vocabulary change in the glossary.
- An accumulation of smaller revisions reaching a meaningful threshold.

### What does not trigger a bump

- Adding examples, fixing typos, expanding rationale.
- Filling in previously-deferred details (e.g., resolving an open question).
- Adding new specs.

---

## 2. Wire protocol versioning

The wire protocol carries a single version field in every frame. The version is **1**. Clients that send any other version are rejected at handshake with `WireVersionMismatch` and the connection is closed.

There is no version negotiation. There is no support for prior wire versions. The wire format ships in lockstep with the server: each Brain release defines exactly one wire version. Because Brain ships no first-party client, third-party clients target a specific wire version and are rejected at handshake if it does not match the server's.

The full wire framing rules live in [`../04_wire_protocol/03_opcodes.md`](../04_wire_protocol/03_opcodes.md).

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

The WAL has its own format version in each segment's header. Detailed in [05. Storage: Arena & WAL](../08_storage/00_purpose.md) §3.

### Metadata format

redb itself versions its format. Brain layers a logical schema version on top: each `redb` database carries a metadata table with a `schema_version` row.

### Server-vs-file behaviour

When the server starts, it inspects each file's format version. Files at the server's version proceed to normal startup. Files at a different version cause the server to refuse to start and emit a migration instruction. Migration to the current version is the only path forward; the server does not operate against off-version files.

### What triggers an on-disk version bump

- Changing the arena slot layout.
- Changing the WAL record framing.
- Changing the metadata table schemas.

### What does not trigger a bump

- Adding new tables to the metadata store (existing readers don't open them).

---

## 4. Migration tools

For format changes that require explicit migration:

- **`brainctl migrate <data_dir>`** reads the existing storage, performs the migration, and writes back. Idempotent — safe to run multiple times.
- Migration is offline: the server is stopped during migration.

---

## 5. Coordination across versions

Wire and storage are tied to a single release. A new Brain release pairs a wire version, a server build, and one set of on-disk format versions. The release notes name all three:

- Spec format version (e.g., "Format version 1").
- Wire protocol version (e.g., "Wire version 1").
- On-disk format versions (arena / WAL / metadata).

---

## 6. The current state

As of this document:

- **Spec format version:** 0.1 (working draft toward 1.0)
- **Wire protocol version:** 1
- **On-disk format version:** 1 across all three formats
