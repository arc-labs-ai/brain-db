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

redb itself versions its format. Brain layers a logical schema version on top: each `redb` database carries a singleton private table holding one `schema_version` row, written when the file is first created. The current value is **1**.

One global row covers the whole metadata file rather than one row per table. The tables ship from the same crate and co-evolve, so bumping one means bumping the file, and per-table versioning would multiply the open-time checks without changing any outcome.

### Server-vs-file behaviour

When the server starts, it inspects each file's format version:

- **Version absent** — a fresh file. The server writes the current version and proceeds.
- **Same version** — normal startup.
- **Older version** — refused. The error names both versions and states that this binary requires the current one.
- **Newer version** — refused. The binary is older than the file; the error says to upgrade the server or restore from a compatible backup.

The refusal is symmetric, and it is deliberate rather than merely cautious. Several readers resolve a stored row without surfacing a decode failure: one parses its bundle with a fallible decode that falls back to a default, so a row written against another layout comes back **empty** rather than wrong-looking, silently blanking every stored memory's inspection view; others resolve rows in a way that aborts the reading thread. Neither failure announces itself as a version problem. Refusing the file at open is the honest failure, and it is the only one a reader can act on.

### What triggers an on-disk version bump

- Changing the arena slot layout.
- Changing the WAL record framing.
- Changing the metadata table schemas.

### What does not trigger a bump

- Adding new tables to the metadata store (existing readers don't open them).

---

## 4. What a bump costs

**There is no migration tool and no migration registry.** A format bump means a fresh data directory: the old files are refused at open and nothing converts them.

This is a scope decision, not a gap awaiting a tool. Brain is pre-release with no external users, so there is no deployed data a converter would serve, and writing one now would mean committing to read every format Brain has ever written. A bump is therefore a deliberate, infrequent act, and the on-disk version is at **1** — no format has yet been superseded.

The practical consequence for an operator: treat a format bump as a re-ingest, not an upgrade step, and keep the source of truth for stored content outside Brain until that changes.

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
- **On-disk format version:** 1 across all three formats, with no superseded version to convert from
