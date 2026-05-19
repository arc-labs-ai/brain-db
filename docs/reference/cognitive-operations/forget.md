# FORGET

Soft- or hard-delete memories. Soft forgets tombstone immediately
and zero physical data after a grace period (default 7 days);
hard forgets zero immediately for compliance use cases.

**Opcode:** `ForgetReq = 0x0024` / `ForgetResp = 0x00A4`.
**Spec:** §09/06. **Source:** `crates/brain-ops/src/ops/forget.rs`.

## Request fields

| Field | Type | Required | Notes |
|---|---|---|---|
| `target` | `ForgetTarget` | yes | One of `Memory(MemoryId)`, `Memories(Vec<MemoryId>)`, `Filter(ForgetFilter)`. |
| `mode` | `ForgetMode` | no | `Soft` (default) or `Hard`. |
| `agent_id` | `AgentId` | yes | Auth check. |
| `request_id` | `RequestId` | yes | Idempotency key. |

### `ForgetMode`

| Mode | Effect |
|---|---|
| `Soft` | Memory becomes invisible to RECALL/PLAN/REASON. Physical data (vector, text, metadata) is retained for the grace period (default 7 d, configurable via `[workers] slot_reclamation_interval_sec` and a separate grace knob). Reversible during grace. |
| `Hard` | All of the above + vector + text are zeroed immediately. Irreversible. Use for compliance / right-to-be-forgotten. |

### `ForgetFilter`

| Field | Type | Notes |
|---|---|---|
| `contexts` | `Option<Vec<ContextId>>` | Limit to specific contexts. |
| `kind` | `Option<MemoryKind>` | Limit by Episodic/Semantic/Consolidated. |
| `max_age` | `Option<Duration>` | Memories older than this. |
| `max_salience` | `Option<f32>` | Low-salience memories. |
| `tags` | `Option<Vec<String>>` | AND-combined metadata tags. |

Filter-based FORGET is capped at 100 K memories per call.

## Response fields

| Field | Type | Notes |
|---|---|---|
| `forgotten` | `Vec<MemoryId>` | Successfully forgotten. |
| `not_found` | `Vec<MemoryId>` | IDs that didn't exist (already-gone). Not an error. |
| `failed` | `Vec<(MemoryId, Error)>` | Per-ID errors (e.g. `NotOwned`). |
| `grace_until` | `Option<u64>` | Unix timestamp when reclamation runs (Soft only). |

## Side effects

Soft FORGET:
- One WAL record per memory (`Forgotten`). Fsynced before ack.
- Per-memory tombstone bit set in the arena slot header.
- Index entries flagged tombstoned; HNSW maintenance cleans them
  on its next tick.
- Outgoing/incoming edges to the forgotten memory are tombstoned.

Hard FORGET:
- All of the above + vector bytes zeroed in the arena +
  text + metadata zeroed in redb.
- Disk-level recovery still possible. For paranoia, encrypt disk
  + rotate keys after a hard FORGET.

After the grace period, the slot-reclamation worker reclaims
the slot (slot version bumps; the old `MemoryId` is now invalid
and returns `MemoryNotFound`).

## Errors

| Code | When |
|---|---|
| `MemoryNotFound` *(per-id, in `not_found`)* | ID never existed or already reclaimed. **Not** a top-level error. |
| `PermissionDenied` | Memory belongs to a different agent. Returned in `failed`. |
| `BudgetTooLarge` | `Memories` target had > 1 K ids, or filter would match > 100 K memories. |
| `Conflict` | Memory held in an active transaction by another client. |
| `IdempotencyConflict` | Same `request_id` reused with different params. |

## Idempotency

Same `request_id` returns the original response. Forgetting an
already-forgotten memory is a no-op.

## Performance target

Spec §16/02 §7:

| Workload | p50 | p99 |
|---|---|---|
| Single ID | 1 ms | 5 ms |
| Batch of 100 IDs | 5–10 ms | 20 ms |
| Filter matching 10 K memories | 100–500 ms | 1 s |

Caps:
- `Memory` / `Memories`: 1 000 IDs per call.
- `Filter`: 100 000 memories matched per call.
- Per-agent rate: 100 FORGETs / sec.

## Substrate vs knowledge

FORGET operates on the substrate. When a schema is declared,
**tombstoning a memory does not cascade to typed statements
extracted from it** by default. Use the knowledge-layer
`StatementRetractReq` (`0x0144`) for typed-statement deletion.

## See also

- [`encode.md`](encode.md) — the write side.
- [`../../runbooks/mass-forget.md`](../../runbooks/mass-forget.md) — operating runbook for bulk forgets.
- [`../../architecture/03-arena-and-wal.md`](../../architecture/03-arena-and-wal.md) — slot lifecycle.

**Spec:** §09/06.
