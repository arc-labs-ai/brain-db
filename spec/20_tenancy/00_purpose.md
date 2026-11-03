# §20 — Tenancy & Isolation

*Normative home for Brain's multi-tenant memory model — the contract the other sections
reference. Design rationale: `.claude/plans/tenancy_industry_scale.md`.*

> **Decision status.** Settled. The vocabulary is `namespace → space → session`, and the
> connection-lifetime concept is named **connection** so that `session` is free for the memory
> tier. Nothing below is provisional.

## 1. Purpose — the two isolation axes

An application built on Brain has **two independent axes of isolation**, and conflating them is
the dominant failure mode of memory systems:

| Axis | Cardinality | Lifetime | Mechanism |
|---|---|---|---|
| *Who is calling* (org / app) | small (10²–10³) | long-lived | a **credential** (API key) |
| *Whose memory this is* (end-user) | **large (up to 10⁶ per org)** | dynamic, churny | a **data-partition key passed per request** |

**MUST:** Per-end-user isolation MUST be enforced in the index as a storage-key prefix, **not**
by minting a credential per end-user.

## 2. The scope model — `namespace → space → session`

Three nested tiers, plus the records:

| Tier | Meaning | Set by | Isolation |
|---|---|---|---|
| `namespace` | organization / product / environment | resolved from the API key at AUTH | **hard wall** |
| `space` | the isolation unit — one end-user, or one purpose | a per-request selector (the `act_as` slot) | **hard wall** |
| `session` | a conversation / run within a space | optional per-request grouping | **soft grouping, not a wall** |

- **MUST:** Every stored row (memory, entity, statement, relation) carries an owner scope
  `RowScope = (namespace_id: u32, space_id: [u8;16])`, immutable for the row's life.
- **MUST:** `space` is the sole isolation-unit noun; it is the same name across wire, storage,
  and SDK, with no alias and no second name for the concept.
- **MUST:** `session` is an **optional** grouping. It MUST NOT be a security boundary and MUST
  NOT appear in any secondary-index scope *prefix*. Isolation is `space`; session is ergonomics.

### 2.1 `space_id` representation

- **MUST:** The wire/SDK carries `space_id` as a **structured opaque string** (e.g.
  `support-bot:user123`). The server MUST NOT parse sub-scopes in v1 — it is opaque.
- **MUST:** The server derives the 16-byte storage key `SpaceId = UUIDv5(namespace, space_string)`
  at request ingress, deterministically. The `uuid5` seed derivation MUST fold the namespace (so
  equal strings under different namespaces diverge at the id level too) and MUST be pinned by a
  golden test — changing it re-keys every space.
- **Rationale:** a fixed-width 16-byte storage key keeps every secondary-index prefix
  `(u32, [u8;16], …)` compact and range-scannable, and lets sharding and scope-prefix range-delete
  operate on it directly.

## 3. Isolation guarantees (the normative core)

- **MUST:** The `namespace` filter is **unconditional**. No request flag MAY widen a read across
  namespaces.
- **MUST:** Every secondary index carries a leading `(namespace_id, space_id)` prefix, so a range
  scan for one scope can physically never traverse another's rows. (Existing exception: the shared
  directional edge table, isolated by a post-scan sidecar-scope re-check.)
- **MUST:** Every id-keyed primary read re-checks the row's own `(namespace_id, space_id)` before
  returning it (defense-in-depth against a corrupt index key).
- **MUST:** Two distinct spaces with an identical `space_id` string under different namespaces
  resolve to disjoint data: `(nsA, "u1") ≠ (nsB, "u1")`.
- **MUST:** A per-request `space` selector naming a namespace outside the principal's `may_act`
  allowlist is hard-rejected (`ActAsDenied`), never silently downgraded.

### 3.1 Credential isolation

Isolation covers what a tenant's data costs as well as where it lives. The two fail together: a
system that keeps one tenant's rows apart but pays for their LLM calls out of another tenant's key
has not isolated them.

- **MUST:** Every LLM call resolves its authority **per namespace**, from the broker, as a
  namespace-scoped grant. There is no shard-wide, deployment-wide or fallback credential, and no
  privileged consumer exempt from this — HyPE, the extractor's LLM tier, the entity disambiguator
  and the summarizer all resolve the same way.
- **MUST NOT:** Brain accept or construct a client from a provider API key, from any source.
  There is no configuration that supplies one.
- **MUST NOT:** anything credential-shaped be persisted — not to the WAL, the metadata store, a
  snapshot, a backup, the response cache, a log line or a diagnostic. This holds **structurally**:
  a provider key cannot leak from data at rest because one never arrives.
- **MUST:** The LLM response cache carry a credential scope in its key, so a completion one
  tenant paid for is never served to another. The scope is derived from the grant or tenant id,
  never from key material.
- **MUST NOT:** a namespace with no registered credential be served out of any other namespace's.
  The write lands, the stages that need an LLM report that they could not run, and the caller is
  told why (`LlmCredentialMissing`, or a `Failed` stage outcome with a reason).

Brain's own minted client keys are a separate object and are unaffected: they authenticate a
connection to Brain, they cannot spend anything upstream, and the database stores only
`BLAKE3(secret)`. See [`../11_extractors/00_purpose.md`](../11_extractors/00_purpose.md)
§"The LLM tier's credential".

## 4. Space registry & CRUD

- Registry table `spaces`: `(namespace_id, space_id) → { space_string, created_at, last_active,
  memory_count, session_count, metadata }`. The leading `namespace_id` makes each namespace's
  spaces a contiguous keyspace.
- **MUST:** A space is creatable **two ways** — implicitly on first write (zero-ceremony) and
  explicitly via `SPACE_CREATE` (pre-provision + metadata/quota).
- **Opcodes** (cognitive namespace, non-admin, scoped to the caller's namespace):
  `SPACE_CREATE 0x0070` · `SPACE_LIST 0x0071` · `SPACE_DELETE 0x0072` (resp `0x00F0–F2`).
- **MUST:** `SPACE_LIST` returns only the caller's namespace's spaces.
- **MUST:** `SPACE_DELETE` is scope-prefix bounded — `O(that space's data)`, never a full scan.
  It range-scans the `(namespace, space)` memory timeline and tombstones each memory through the
  single `submit(Write)` path; each tombstone fans out post-commit to the FORGET cascade, which
  tears down the backing entity / statement / relation rows. Reusing FORGET's machinery rather
  than writing a second graph teardown is what keeps one code path correct instead of two.
- **MUST:** For GDPR erasure `SPACE_DELETE` zeroes **immediately** — it is always hard, a
  sanctioned deviation from the default 7-day tombstone grace. There is no soft mode: a deletion
  request that leaves the data recoverable for a week has not satisfied the request.
- The v1 cascade is **synchronous**, tombstoning every matching memory in one pass. A very large
  space delete should chunk behind a resumable cursor; until it does, the call's duration scales
  with the space's size.

## 5. Session registry & CRUD

- **MUST:** `session` reaches the typed graph — statement/relation rows extracted from a session's
  memories carry that `session_id`, so a listed session is coherent across memory **and** graph.
  Entities carry `session_id` as first-mention provenance only — entity identity is
  session-agnostic, and `EntityView` therefore exposes no session at all. An entity first mentioned
  in one conversation and again in another is one entity; recording otherwise would fragment
  identity along a boundary that is explicitly not a wall.
- Registry table `sessions`: `(namespace_id, space_id, session_id) → { created_at, last_active,
  title?, memory_count }`, id-addressed (`session_id` is the client's opaque `u64`), listed
  newest-first via a `last_active`-keyed scope index.
- **Opcodes:** `SESSION_CREATE 0x0073` · `SESSION_LIST 0x0074` · `SESSION_DELETE 0x0075`
  (resp `0x00F3–F5`).
- **MUST:** `SESSION_LIST` is scoped to a single `(namespace, space)` — a caller lists only one
  space's sessions.
- **MUST:** The default session (`session_id = 0`) is non-deletable; memories encoded without an
  explicit session land there.
- **MUST:** `SESSION_DELETE` defaults to soft (7-day grace) with a `hard` mode, mirroring FORGET.
  It is the same cascade as `SPACE_DELETE`, narrowed to one session.

## 6. Per-space index partitioning (requirement; mechanism in §08/§09)

- **Rationale:** the per-user world inverts the old assumption (millions of spaces, few memories
  each). A shared index with a post-hoc scope filter misses a sparse space's own results at high
  selectivity.
- **MUST:** When a query is scoped to a single space, retrieval routes to a per-space path — exact
  brute-force scan of that space's vectors for small spaces (below a configured threshold), or a
  **per-space** HNSW for large spaces — never a shared-index-with-filter path that can miss
  sparse-space results.
- **MUST:** When no space is named (namespace-wide admin/analytics), the shared/low-selectivity
  path MAY be used.
- **MUST:** A space's memory vectors have physical locality (per-space slot-list v1; contiguous
  per-space arena segment v2) so recall touches only that space's vectors.
- **SHOULD:** At extreme scale, spaces carry an activity state (active/inactive/offloaded) with
  lazy load and cold offload to object storage (v2).
- **Gate:** the high-selectivity recall probe (§19) is the go/no-go for shipping partitioning.

## 7. Auth & `may_act`

- **MUST:** Connection identity `(namespace, space, permissions)` is derived entirely from the
  authenticated key at AUTH; clients MUST NOT send `namespace`/`space_id` as a self-identity claim.
- **MUST:** `space_id` on a data op is the per-request effective-space selector inside the
  `act_as` slot (`ActAs { namespace, space_id }`), honored only when the principal holds
  `can_act_as` and `act_as.namespace ∈ may_act`.
- **MUST:** `may_act` remains a **namespace-level** allowlist. A namespace-scoped key may select
  any `space_id` within its own namespace, and no other.
- **MUST:** The managed platform mints customer keys **namespace-scoped** (`may_act` over their
  own namespace), **never** wildcard-`may_act`. Wildcard `["*"]` is reserved for the internal
  service principal. A wildcard customer key is a cross-tenant hole.
- **MUST:** The impersonated space runs with the fixed `STANDARD_AGENT` permission mask — never
  inheriting `ADMIN`/`ACT_AS` (a space is data, not a principal).

## 8. Deployment modes — OSS-direct and managed-through-edge, one SDK

- **MUST:** The entire tenancy model (space/session, registries, `SPACE_*`/`SESSION_*`, RowScope,
  the namespace wall, per-space partitioning) ships in Brain core and is fully usable with **no
  edge** — open-source self-hosters get a first-class multi-tenant DB.
- **MUST:** Isolation lives entirely in Brain core; brain-edge MUST NOT be an isolation mechanism.
- **MUST:** The SDK speaks the wire protocol in both modes; "through the edge" means pointing the
  same client at the edge's address with the **same Brain key**. brain-edge MUST be a transparent
  passthrough that **forwards** `space_id`/`session` verbatim and MUST NOT resolve identities or
  stamp `act_as`.
- **MUST:** The end-user → `space_id` mapping lives in the customer's own application; Brain
  validates the space against the key's namespace.
- **MUST:** SDK call signatures are identical in both modes — the only difference is configuration
  (endpoint + token).

## Cross-references (contracts centralized elsewhere — link, don't duplicate)

- `act_as` R1–R6 contract: §04 handshake. · `may_act` mint + wildcard rules: §17 admin ops.
- Namespace wall + owner-vs-type namespace: §03 namespaces. · RowScope + registry tables: §10.
- Per-space index mechanism: §09 filtering + §08 arena. · Recall-probe acceptance: §19.
