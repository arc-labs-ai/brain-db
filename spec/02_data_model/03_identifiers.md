# 02.03 Identifiers

Brain uses several identifier types, each with specific format and stability properties. This file specifies them all.

## 1. The identifier types

| Identifier | Size | Format | Scope | Stability |
|---|---|---|---|---|
| `MemoryId` | 16 bytes | Encoded shard + slot + version + reserved | Cluster | Stable until forgotten + reclaimed |
| `AgentId` | 16 bytes | UUIDv7 | Cluster | Permanent |
| `ContextId` | 8 bytes | Server-assigned u64 | Per-agent | Permanent within agent |
| `RequestId` | 16 bytes | Client-supplied UUIDv7 | Per-agent within idempotency horizon | Bounded TTL |
| `ShardId` (storage) | 16 bytes | UUIDv7 | Cluster | Permanent |
| `ShardId` (runtime) | 2 bytes | u16, mapping table → storage UUID | Cluster | Subject to remapping during cluster ops |

These are documented individually below.

## 2. MemoryId

The public identifier of a memory.

### 2.1 Format

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|         shard_id (16)         |        slot_id (high 16)      |
+---------------------------------------------------------------+
|                  slot_id (low 32)                             |
+---------------------------------------------------------------+
|                       version (32)                            |
+---------------------------------------------------------------+
|                       reserved (32)                           |
+---------------------------------------------------------------+
```

Total: 128 bits = 16 bytes.

- `shard_id` (16 bits) — the runtime shard identifier (see §6 below).
- `slot_id` (48 bits) — the slot within the shard's arena.
- `version` (32 bits) — incremented on each slot reuse.
- `reserved` (32 bits) — must be zero in v1; reserved for future use.

### 2.2 Properties

- **Opaque to clients.** Clients treat `MemoryId` as an opaque 16-byte handle. They MUST NOT attempt to extract or interpret subfields.
- **Endianness:** big-endian for the on-the-wire and on-disk representations. The memory representation in Rust is `[u8; 16]`.
- **Equality:** byte-for-byte equality.
- **Ordering:** byte lexicographic ordering. (Note: this is *not* a meaningful ordering for memories — sorting by `MemoryId` produces an arbitrary order, not a temporal one.)

### 2.3 Stability

A `MemoryId` is stable from creation until the memory is forgotten *and* its slot is reclaimed. After reclamation, the slot's version increments, so the same `MemoryId` would map to a different memory only if a subsequent reclamation cycle restored the old version — which we forbid.

**INVARIANT:** A `MemoryId` that previously identified memory M never identifies a different memory. If M is forgotten and the slot is reused for memory M', M' has a `MemoryId` with the new (incremented) version. Old `MemoryId`s referencing the previous version detect the mismatch via the version field.

This is what the version field is *for*. Without versions, slot reuse would silently re-target stale references.

### 2.4 The zero MemoryId

The all-zero `MemoryId` (16 bytes of zero) is reserved as the "null" value. It MUST NOT be returned by any operation; clients MAY use it as a sentinel for "no memory".

## 3. AgentId

The identifier for an agent.

### 3.1 Format

[UUIDv7](https://datatracker.ietf.org/doc/rfc9562/), 16 bytes.

UUIDv7 encodes a 48-bit Unix-millisecond timestamp followed by random bits, with version and variant fields per the spec. This gives:

- Time-ordered: agents created later have lexicographically-greater UUIDs.
- Unique: collision probability vanishingly small.
- Sortable: indexes by `AgentId` cluster the most recently created agents at the end.

### 3.2 Properties

- **Permanent.** An agent's `AgentId` never changes.
- **Cluster-scoped.** Unique across the entire cluster.
- **Client-generated.** The agent generates its own UUIDv7 at creation. The substrate doesn't issue agent ids; an external identity system or the client SDK does.

### 3.3 The zero AgentId

The all-zero `AgentId` is reserved. Operations referencing the zero `AgentId` MUST be refused with `INVALID_ARGUMENT`.

## 4. ContextId

A logical scope within an agent's memory.

### 4.1 Format

A 64-bit unsigned integer.

### 4.2 Properties

- **Agent-scoped.** Two different agents can both have `context_id = 1`; they are unrelated.
- **Server-assigned.** When an agent first references a context name (a string), the server assigns a `context_id` and persists the name → id mapping.
- **Permanent within an agent.** Once a `context_id` is assigned, it never changes; if a context is "deleted" (out of scope for v1), the id is retired, not reused.

### 4.3 The default context

`context_id = 0` is reserved for the default context. Every agent automatically has a context with id 0 named "default". Memories encoded without an explicit context land in the default.

## 5. RequestId

The client-supplied idempotency token for `ENCODE` and `FORGET`.

### 5.1 Format

[UUIDv7](https://datatracker.ietf.org/doc/rfc9562/), 16 bytes. Recommended but not strictly enforced; any 16 bytes are accepted.

### 5.2 Properties

- **Agent-scoped.** Idempotency is checked within a single agent's namespace.
- **Bounded TTL.** Stored in the idempotency table for a configurable window (default: 5 minutes). After the TTL, the same `RequestId` may be treated as a new operation.
- **Single-use semantically.** Within the TTL, a duplicate `RequestId` results in the original operation's response being replayed. Different operations submitted with the same `RequestId` is an error.

### 5.3 Why UUIDv7

UUIDv7's time-ordering helps the idempotency table — old entries are toward the front of the time-ordered keyspace and easy to expire in batch.

Other UUID versions or random tokens work but lose this property. The protocol accepts any 16 bytes; UUIDv7 is the recommendation.

## 6. ShardId — two senses

The shard identifier has two senses depending on context. Confusing them is a frequent source of bugs.

### 6.1 Storage shard ID

The persistent identifier of a shard, used in:

- Storage filesystem paths: `data/<storage_shard_uuid>/arena.bin`.
- Backup and snapshot metadata.
- Cluster control plane records.

Format: UUIDv7, 16 bytes.

**Permanent.** A shard's storage UUID is set when the shard is created and never changes. Even when the shard is moved between nodes, its storage UUID stays the same.

### 6.2 Runtime shard ID

The compact identifier of a shard, used in:

- The high 16 bits of every `MemoryId`.
- Routing tables.
- Wire-format frames.

Format: 16-bit unsigned integer.

**Subject to remapping.** Up to 65,535 shards per cluster (id 0 reserved). The mapping from runtime id → storage UUID is maintained by the cluster control plane.

### 6.3 The mapping

A control-plane table:

```
runtime_shard_id | storage_shard_uuid                        | epoch
-----------------+-------------------------------------------+-------
1                | 0190a8e1-0001-7000-8000-000000000001      | 5
2                | 0190a8e1-0001-7000-8000-000000000002      | 5
...
```

The `epoch` field tracks generation; a node sees the table as of some epoch and refuses to operate on later versions until it refreshes. Specified in [12. Sharding + Clustering](../12_sharding_clustering/).

### 6.4 Why two senses

The runtime id is small (16 bits) so MemoryIds fit in 16 bytes. The storage UUID is permanent so backups and disaster recovery aren't broken by cluster reorganization.

Mapping between them is a control-plane concern. Clients see only `MemoryId` (which embeds the runtime id at the time the memory was created) and `AgentId` (from which routing computes the runtime id). They never see the storage UUID directly.

### 6.5 Implication for stale MemoryIds

A `MemoryId` was created when its memory's shard had runtime id S. If the shard is later renumbered (due to cluster reorganization), the `MemoryId` still encodes S in its high 16 bits — but S now maps to a different storage UUID.

Resolution: the cluster control plane records the *historical* mapping. A `MemoryId` created at epoch E uses the runtime → storage mapping in effect at epoch E. The router resolves it using the historical mapping, not just the current one.

This is rare in practice (cluster reorganization is infrequent), but the mechanism exists to avoid breaking client-cached `MemoryId`s.

## 7. Identifier collisions

We need to argue that none of these identifiers collide in practice.

### 7.1 MemoryId collision

By construction, no two memories share a `MemoryId`. The combination of (shard_id, slot_id, version) is unique within the shard, and shard_id is unique within the cluster.

### 7.2 AgentId collision

UUIDv7 collision probability: ~1 in 2^62 within a millisecond, vanishingly small in practice. Even an organization creating a billion agents per day has effectively zero collision probability.

### 7.3 RequestId collision

`RequestId` collisions matter only within an agent's namespace and within the idempotency horizon. UUIDv7 collisions in this scope are essentially zero.

If a client uses non-UUIDv7 random tokens, collision probability rises but is still small for any reasonable token. (16 bytes of random gives 2^128 possibilities.)

### 7.4 Storage UUID collision

UUIDv7 across shard creation; same argument as `AgentId`.

### 7.5 Runtime shard id collision

Bounded at 65,535 shards per cluster. This is a soft cap; if you need more, add a separate cluster.

## 8. Wire and storage representations

| Identifier | Wire (bytes, big-endian) | Storage (bytes, native) |
|---|---|---|
| `MemoryId` | 16, fixed | 16, fixed |
| `AgentId` | 16, fixed | 16, fixed |
| `ContextId` | 8, fixed | 8, fixed (host endianness) |
| `RequestId` | 16, fixed | 16, fixed |
| `ShardId` (storage) | 16, fixed | 16, fixed |
| `ShardId` (runtime) | 2, fixed | 2, fixed (host endianness) |

The wire formats use big-endian for portability. Storage formats use host endianness for performance; cross-architecture migration requires byte-swapping (out of scope for v1; same-architecture restore is the supported path).

---

*Continue to [`04_context.md`](04_context.md) for contexts.*
