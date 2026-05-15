# Spec edits needed for Phase 16.6 (u16 opcode + namespace)

The code now uses a u16 opcode (bytes 5-6, BE) with `0x00xx` substrate and `0x01xx` knowledge. `flags` shrank to `u8` at byte 7. Apply the diffs below manually — `spec/` is permission-denied to me.

---

## 1. `spec/03_wire_protocol/03_frame_header.md`

### 1.1 Header diagram + table (§1)

Replace the ASCII diagram's `version (8) | opcode (8) | flags (16)` row with:

```
+---------------+-------------------------------+---------------+
|   version (8) |          opcode (16)          |   flags (8)   |
+---------------+-------------------------------+---------------+
```

In the field-by-field table replace the two rows for bytes 5 and 6–7 with:

| Bytes | Field | Type | Purpose |
|---|---|---|---|
| 5–6 | `opcode` | `u16` (big-endian) | The operation type. High byte = namespace (0x00 substrate, 0x01 knowledge); low byte = op index. See [`05_opcodes.md`](05_opcodes.md) and [`../28_knowledge_wire_protocol/00_purpose.md`](../28_knowledge_wire_protocol/00_purpose.md). |
| 7 | `flags` | `u8` | Frame-level flags (see §2). |

### 1.2 Flags section (§2)

Replace the entire §2 with:

> The 8-bit `flags` field encodes per-frame metadata:
>
> ```
> bit 7   6   5   4   3   2   1   0
>    +---+---+---+---+---+---+---+---+
>    |EOS|MPL|CMP|       reserved    |
>    +---+---+---+---+---+---+---+---+
> ```
>
> | Bit | Name | Meaning |
> |---|---|---|
> | 7 | `EOS` | End of stream — last frame of this stream. |
> | 6 | `MPL` | Multi-payload — payload spans multiple frames; concatenate to reconstruct. |
> | 5 | `CMP` | Compressed — payload is zstd-compressed. (Reserved; not used in v1.) |
> | 4-0 | reserved | Must be zero. |
>
> (The pre-v1 design used a 16-bit `flags` with bits 12-0 reserved. Phase 16.6a shrank `flags` to `u8` and reclaimed the freed byte for the upper byte of `opcode`. Since no v1.0 release has shipped, this is not a backward-compatibility break — see [`12_versioning.md`](12_versioning.md).)

### 1.3 §3.3 opcode

Replace §3.3 with:

> The operation type. See [`05_opcodes.md`](05_opcodes.md) for the full substrate table and [`../28_knowledge_wire_protocol/00_purpose.md`](../28_knowledge_wire_protocol/00_purpose.md) for the knowledge-layer table.
>
> The opcode is a big-endian `u16` split into two bytes:
>
> - **High byte — namespace.**
>   - `0x00` — substrate (cognitive primitives, connection management, admin).
>   - `0x01` — knowledge layer (schema, entities, statements, relations, queries, extractors).
>   - `0x02`–`0xFF` — reserved for future namespaces.
> - **Low byte — operation index within the namespace.** Low byte `< 0x80` is server-bound (C → S, request); low byte `≥ 0x80` is client-bound (S → C, response). The direction rule applies independently within each namespace.
>
> Examples: `0x0020` is substrate `ENCODE_REQ`; `0x00A0` is substrate `ENCODE_RESP`; `0x0130` is knowledge `ENTITY_CREATE` (request); `0x01B0` is knowledge `ENTITY_CREATE_RESP`.

### 1.4 §7 frame examples

Update each opcode in the example tables from `0xNN` to `0x00NN` and each `flags` from `0xNNNN` to `0xNN`:

- §7.1 PING: `opcode 0x0010 (PING)`, `flags 0x00`.
- §7.2 RECALL request: `opcode 0x0021 (RECALL_REQ)`, `flags 0x00`.
- §7.3 RECALL response intermediate: `opcode 0x00A1 (RECALL_RESP)`, `flags 0x00 (not EOS yet)`.
- §7.4 RECALL response final: `opcode 0x00A1 (RECALL_RESP)`, `flags 0x80 (EOS)`.

### 1.5 §8 endianness summary

Replace the `opcode` and `flags` rows with:

| Field | Endianness |
|---|---|
| `opcode` | big-endian `u16` |
| `flags` | single byte |

---

## 2. `spec/03_wire_protocol/05_opcodes.md`

### 2.1 Opening paragraph

Replace:

> The opcode is a single byte in the frame header. Server-bound opcodes (client → server) occupy 0x00–0x7F; client-bound opcodes (server → client) occupy 0x80–0xFF.

with:

> The opcode is a big-endian `u16` in the frame header (bytes 5–6). The high byte is a **namespace** (`0x00` substrate — this section; `0x01` knowledge layer — see [`../28_knowledge_wire_protocol/00_purpose.md`](../28_knowledge_wire_protocol/00_purpose.md); `0x02–0xFF` reserved). Within a namespace the low byte's high bit selects direction: low byte `< 0x80` is server-bound (C → S); low byte `≥ 0x80` is client-bound (S → C).

### 2.2 All tables in §1.1–§1.7

Prefix every opcode value in the tables with `0x00` (e.g. `0x01 HELLO` → `0x0001 HELLO`, `0xA0 ENCODE_RESP` → `0x00A0 ENCODE_RESP`).

### 2.3 §2 reserved ranges

Replace with:

> Within the substrate namespace (`0x00xx`), the following low-byte ranges are reserved:
>
> - 0x70–0x7F (server-bound, `0x0070–0x007F`) — reserved for future C → S substrate operations.
> - 0xF0–0xFE (client-bound, `0x00F0–0x00FE`) — reserved for future S → C substrate operations.
>
> Other namespaces (`0x01xx` knowledge, etc.) have their own reservations — see the corresponding spec sections.
>
> Receivers MUST treat unknown opcodes as protocol errors (sending `ERROR` with `BadOpcode`) — no silent discarding.

### 2.4 §3 symmetry

Update the mnemonic: `0x002N → 0x00AN` (cognitive), `0x006N → 0x00EN` (admin).

### 2.5 §4 dispatch

Replace "For client → server opcodes (0x00–0x7F)" with "For server-bound opcodes (low byte `< 0x80`)" — the namespace byte is independent of direction.

---

## 3. `spec/03_wire_protocol/12_versioning.md`

Add a new sub-section at the top (after the existing intro paragraph):

> ### Pre-v1.0 compatibility policy
>
> Prior to the `v1.0.0` tag, the wire protocol is allowed to change in incompatible ways without a `version`-field bump or handshake negotiation step. Phase 16.6 widened the opcode from `u8` to `u16` and shrank `flags` from `u16` to `u8` (see [`03_frame_header.md`](03_frame_header.md) §1) under this policy. After v1.0 ships, all changes follow the formal versioning rules below.

---

## 4. `spec/28_knowledge_wire_protocol/00_purpose.md`

### 4.1 Opcode-space section (top of file, replace the existing block)

Replace:

```
0x00–0x0F   reserved
0x10–0x1F   cognitive primitives (defined in section 03)
0x20–0x2F   schema operations
0x30–0x3F   entity operations
0x40–0x4F   statement operations
0x50–0x5F   relation operations
0x60–0x6F   query operations (hybrid retrieval)
0x70–0x7F   admin operations
0x80–0x8F   reserved future
```

with:

> ## Opcode namespace
>
> All knowledge-layer opcodes live in the **`0x01xx` namespace** of the u16 wire opcode (spec §03/03 §3.3 + §03/05). The substrate occupies `0x00xx`; the knowledge layer occupies `0x01xx`; other namespaces are reserved.
>
> Within `0x01xx`, low-byte ranges are partitioned by operation family:
>
> ```
> 0x0100–0x010F   reserved
> 0x0110–0x011F   reserved future (was: cognitive primitives — they live in 0x00xx)
> 0x0120–0x012F   schema operations
> 0x0130–0x013F   entity operations
> 0x0140–0x014F   statement operations
> 0x0150–0x015F   relation operations
> 0x0160–0x016F   query operations (hybrid retrieval)
> 0x0170–0x017F   admin operations
> 0x0180–0x018F   reserved future
> ```
>
> The low byte's high bit selects direction within the knowledge namespace, mirroring substrate convention. For example, `ENTITY_CREATE` is `0x0130` (request) and its response `ENTITY_CREATE_RESP` is `0x01B0`.

### 4.2 Per-family tables

Prefix every opcode in §28's tables with `0x01`. E.g.:

- Schema: `0x20 SCHEMA_UPLOAD` → `0x0120 SCHEMA_UPLOAD`, …
- Entities: `0x30 ENTITY_CREATE` → `0x0130 ENTITY_CREATE`, …, `0x38 ENTITY_TOMBSTONE` → `0x0138 ENTITY_TOMBSTONE`.
- Statements: `0x40` → `0x0140`, …
- Relations: `0x50` → `0x0150`, …
- Query: `0x60` → `0x0160`, …
- Admin: `0x70` → `0x0170`, …

(Each table's response opcode follows the `low-byte | 0x80` convention: e.g. `ENTITY_CREATE_RESP = 0x01B0`.)

### 4.3 Error-codes table

The error codes (0x20 SCHEMA_INVALID, 0x30 ENTITY_NOT_FOUND, etc.) are a separate namespace from opcodes — these are wire **error codes** carried in the ERROR-frame body, not opcodes. Leave them as written (no prefix needed).

---

## 5. Sanity checklist after applying

- [ ] `cargo test -p brain-protocol` still passes (it does today; the spec edits don't change code).
- [ ] No remaining single-byte opcode references in §03/05, §03/03, or §28.
- [ ] §03/03's flags layout shows u8, EOS at bit 7.
- [ ] §12 has the pre-v1.0 note.
