# 02.10 Data Model Failure Modes

This file lists the things that can go wrong at the data-model level — corruption, inconsistency, invalid records — and Brain's response to each. The system-level failure-mode catalog is in [18. Failure Recovery](../18_failure_recovery/00_purpose.md); this file covers the entity-level cases.

## 1. Stale MemoryId references

**Failure mode.** A client holds a `MemoryId` that no longer corresponds to any active memory. The memory was forgotten, and possibly reclaimed afterwards.

**Detection.** Brain looks up the row the `MemoryId` names. If:

- The row is tombstoned: the memory was forgotten but not yet reclaimed. Return `MemoryNotFound`.
- There is no row: the memory was reclaimed, and nothing replaces it. Return `MemoryNotFound`.
- The row's owner scope differs from the caller's: return `MemoryNotFound` as well. This case is deliberately indistinguishable from the previous two — see §3.
- The `MemoryId`'s version does not match the row's: return `MemoryNotFound`.

**Response.** Return an explicit error rather than silently returning a different memory. Two independent properties make silent re-targeting impossible: slot ids are minted from a monotonic counter and never recycled, so nothing can occupy a retired handle; and the version field in the `MemoryId` is checked on lookup regardless.

**Implication for clients.** Clients should treat `MemoryId` as a value that may go stale. Operations like `RECALL` always return current `MemoryId`s; clients caching `MemoryId`s need a strategy for re-validating them periodically.

## 2. Invalid request_id (idempotency)

**Failure mode.** A client retries an operation with a `RequestId` that has been seen before, but with different parameters than the original.

**Detection.** The idempotency table records the `RequestId`'s original parameters. On a retry, Brain compares the new parameters to the stored ones. If they match, return the original response. If they differ, that's a client bug.

**Response.** Return `IdempotencyConflict` with details of the mismatch. Brain refuses to overwrite the original response with new data.

**Implication for clients.** Use a fresh `RequestId` for each logically-distinct operation. Reusing `RequestId`s only for actual retries (same parameters, same intent).

## 3. Cross-agent reference

**Failure mode.** An operation in agent A's session references a `MemoryId` belonging to agent B.

**Detection.** A `MemoryId` is an enumerable packed integer, so routing alone is not a wall: the owner check is made authoritatively against the stored row's own owner scope, inside the same transaction as the operation. The space id is the complete tenant key — it folds the namespace, so a request from a foreign namespace always resolves to a disjoint space id.

**Response.** A row owned by someone else is treated exactly as a missing one. Reads answer `MemoryNotFound`, and the lenient writes answer as they would for an absent memory — `FORGET` reports a no-op success rather than an error.

This is not an approximation of an authorization error; it is the intended answer. Any response that distinguished "exists but is not yours" from "does not exist" would let a caller enumerate another tenant's memories by probing ids. There is no `Unauthorized` code on this path, and nothing to add one to.

**Implication for clients.** Don't share `MemoryId`s across spaces. They are space-scoped, and a foreign id reads as absent rather than as refused.

## 4. Session references are not validated

**There is no "invalid session reference" failure mode.** A `SessionId` is an unsigned 64-bit integer on the wire, so every value a client can send is structurally valid, and the id space is **client-owned**: `SESSION_CREATE` takes the id from the request rather than issuing one, and `ENCODE` creates a registry row for whatever id the write carries. A session comes into existence *by being written to*.

That is what rules the check out, not an omission. Because writing to a session creates it, "this id has no registry row" is indistinguishable from "nothing has been written there yet" — and the second is a legitimate race against a concurrent write by the same client. Refusing the id would mean refusing a correct program. The server therefore has no authority to call a session id invalid, and refusing one would only tell a caller that nobody has written to that id, which is not Brain's to disclose.

**Consequences.** A read filtered to a session nobody has written to matches nothing and returns an empty result, which is the honest answer. `SESSION_DELETE` on an id with no row reports `existed: false` rather than failing — the same permissive posture.

**The one refusal that does exist** is a size bound, not a validity check: a `session_filter` list longer than its cap is rejected as a malformed request. The list is rescanned for every index node the vector lane visits and expands into one term query per id in the lexical lane, so its length multiplies the cost of the whole read — the cap bounds caller-controlled work. `RECALL`, `QUERY` and `MATERIALIZE_PROCEDURAL` each cap it at 1024 entries; `SUBSCRIBE` applies its own bound of the same size to the filter it carries.

**Implication for clients.** Choose session ids deliberately and reuse them; nothing on the server will catch a typo in one. An empty result from a session-filtered read means that session holds nothing — it is not a signal that the id was wrong.

## 5. Vector corruption

**Failure mode.** A memory's stored vector has been corrupted (cosmic ray, disk error, software bug) and no longer represents the memory's text. The vector has two at-rest copies — the authoritative redb row and the arena image rebuilt from the log at startup — and either can be the corrupted one.

**Detection.** Brain doesn't actively verify vectors against text on every read; that would be too expensive. Detection is passive:

- **Norm check on read** — `RECALL` candidates have their norm checked. A vector with norm far from 1.0 (epsilon = 1e-3) is suspicious. The candidate is excluded from results, and the memory is flagged for repair.
- **Periodic background scrub** — the integrity-check worker scans memories periodically, recomputing norms and checking against a per-shard checksum.

**Response.**

- If the text is intact and the embedding model is still available, re-embed and overwrite the corrupted vector.
- If the text is also corrupted (or unavailable), the memory is unrecoverable from the live state. Restore from snapshot if available; otherwise, the memory is lost.

**Implication for operators.** Run regular snapshots. Monitor the integrity-check worker's output.

## 6. Text corruption

**Failure mode.** A memory's text has been corrupted in the metadata store.

**Detection.** The metadata store (redb) checksums its own pages. Page-level corruption is detected by redb. A specific memory's text being corrupted while the page checksum is intact requires both the data and the checksum to be corrupted in a consistent way — extremely rare.

If Brain stores a separate per-memory text checksum (it does, via [BLAKE3](https://github.com/BLAKE3-team/BLAKE3) on the text), it can detect corruption that bypasses the page checksum.

**Response.** Restore from snapshot. If Brain also has a `text_hash` field on the memory and the hash mismatches the text, the memory is flagged corrupted; the text is treated as missing for all operations until repaired.

## 7. Edge corruption

**Failure mode.** An edge points at a non-existent target memory.

**Detection.** Edge traversal during `RECALL`, `PLAN`, or `REASON` may dereference an edge target. If the target is tombstoned or gone, the edge is stale; Brain filters it out during traversal rather than rejecting the whole walk.

**Response.** Filter out stale edges silently during traversal. Schedule a background job to clean up the stale edges.

**Implication.** Brain is designed to tolerate stale edges. The cost is a small extra check during traversal; the alternative (eager edge cleanup on every memory's deletion) would be expensive.

## 8. Salience saturation

**Failure mode.** A memory is accessed so many times that its salience saturates at 1.0. After saturation, the access boost has no effect.

**Detection.** This isn't really a failure — it's expected behavior. The salience formula's clamping at 1.0 means a memory's salience tops out.

**Response.** Brain operates correctly. Saturated salience just means "this is highly important"; further accesses don't move the score.

**Implication.** Salience is a relative ranking signal; absolute values matter less than rankings. If many memories saturate at 1.0, the salience signal is weak among them, and other ranking factors (recency, similarity) dominate.

## 9. Salience floor: a memory that won't decay enough

**Failure mode.** A memory's salience is at the floor (default 0.05) and stays there. The agent wants the memory truly forgotten but it lingers.

**Detection.** This is again expected behavior. The floor exists to prevent automatic forgetting via decay alone; the agent must explicitly `FORGET` memories it wants gone.

**Response.** Use `FORGET`. If the agent observes lots of memories at the floor, they're candidates for explicit forgetting based on the agent's policy.

## 10. Embedding model mismatch

**Failure mode.** A query is issued; Brain has memories from a different embedding model. Cross-model results would be noise; Brain filters them out.

**Detection.** The fingerprint comparison happens during `RECALL` (and similarly for `PLAN`, `REASON`).

**Response.** Memories with mismatched fingerprints are excluded from results. A deployment whose model changed therefore reads as empty over its pre-change memories, rather than returning results scored against an incomparable vector space.

**Implication.** There is no re-embedding operation to run: changing the embedding model strands every memory encoded under the old one. Returning nothing is the designed outcome, not a transient state to wait out. The practical consequence is that the embedding model is effectively fixed for the life of a data directory, and changing it is a re-ingest.

## 11. Concurrent access creating ordering anomalies

**Failure mode.** Two `RECALL`s for the same memory race; both salience updates are applied. Or: a `RECALL` and a `FORGET` race; the read sees the memory but the write committed afterward.

**Detection.** These are not bugs; they are expected concurrency. Brain provides per-shard linearizability ([01.06 Targets](../01_architecture/05_hardware_and_targets.md) §4.4): the order is well-defined, even if it's not the order the client might expect.

**Response.** Brain operates as specified. Anomalies are timing artifacts, not correctness errors.

**Implication.** Clients should not assume read-your-own-write across operations unless they're sequenced through the same connection.

## 12. Schema version mismatch

**Failure mode.** A client (or storage file) is at a different format version than the server. Cannot understand the data.

**Detection.** Format-version checks at load time (storage) and handshake (wire).

**Response.** Refuse to load (storage) or refuse the connection (wire). The storage error names the version found and the version required, and the refusal is symmetric: a file older than the binary and a file newer than it are both refused. The wire handshake likewise rejects any version but its own, with no negotiation.

**Implication.** Operators run clients and servers at the same Brain release. There is no converter for an off-version data directory — a storage-format bump is a re-ingest, not an upgrade step. See [`../00_overview/03_versioning.md`](../00_overview/03_versioning.md) §4.

## 13. Out-of-space conditions

**Failure mode.** The WAL fails to extend, or a store cannot grow: no disk space.

**Detection.** The underlying allocation fails — `fallocate` returns `ENOSPC`, or a store write fails for want of space.

Slot exhaustion is a separate matter and is not a practical failure mode. Slot ids come from a 48-bit monotonic counter, so the id space is not what a deployment runs out of; disk is.

**Response.** The write fails and the error reaches the client. It is not retried automatically — the client decides, because a retry against a full disk fails the same way.

**Implication.** Operators must monitor disk usage and capacity. This is the failure to alert on, well before it arrives: a shard that cannot extend its log cannot accept writes at all.

## 14. Encoding model mismatch with stored vectors

This is the operational edge of §10. Every memory carries the fingerprint of the model that embedded it, so a data directory written across a model change holds a mix of fingerprints, and a query matches only the memories whose fingerprint equals the running model's.

There is no re-embedding path, so the mix does not resolve itself: whichever model is running, the memories encoded under the other are unreachable. The one recovery that works is to put the original model back, which restores access to everything encoded under it and strands whatever was written under the other.

The honest reading is that the fingerprint is a correctness guard, not a migration mechanism. It guarantees a query is never answered from an incomparable vector space; it does not make a model change survivable. Pin the embedding model for the life of a data directory.

## 15. Validation failures at ingest

**Failure mode.** A client submits an `ENCODE` with invalid parameters: empty text, invalid session, malformed `RequestId`, oversize text, etc.

**Detection.** Validation at the protocol layer ([04. Wire Protocol](../04_wire_protocol/00_purpose.md) §11).

**Response.** Return a specific error: `InvalidArgument` with details indicating which parameter was wrong.

**Implication.** Clients should validate before submitting. Clients should validate before submitting; the server also validates server-side.

## 16. ENCODE refused for a missing LLM credential

**Failure mode.** A namespace with no registered LLM provider credential submits an `ENCODE`.

**Detection.** The ENCODE validation step probes the local credential store for the caller's namespace. The store is local, so the probe is one point read and the answer is definitive: a namespace with no registered key has **every** ENCODE refused, not an unlucky fraction of them.

**Response.** `LlmCredentialMissing` (`0x0066`, category `Conflict`). **Terminal** — retrying cannot conjure a key; the tenant registers one with `LLM_CREDENTIAL_PUT`. It is `Conflict` rather than a validation error because the request itself is well-formed: it is the namespace's configuration state that cannot satisfy it.

**Why refuse rather than degrade.** HyPE and the extraction tier both need a provider key. A write admitted without one lands as text with no questions, no entities and no statements — it looks stored, and recall quietly gets worse.

**The distinct non-refusal case.** A credential that merely cannot be *resolved* right now — a storage fault, or a sealing key that is no longer loaded — **admits** the write, because neither is evidence that the tenant has no key. The derivation stages re-resolve and retry when they run; if they still cannot, the encode trace reports `StageOutcome::Failed` for that stage. A caller diagnosing an empty graph reads the stage statuses to tell "could not run" from "this text warranted no questions".

**Implication.** Register a credential per namespace before the first write.
