# RB-11: Schema toggle (declare / migrate / revert)

**Severity:** **operator-triggered**, not alerted.
Treat as P3 if planned; P2 if unplanned (a schema
upload that broke things).
**Alert:** none.
**SLO impact:** depends on the operation; backfills
consume shard time and can degrade latency.
**Estimated duration:** 15 minutes (small schema, no
backfill) to days (large backfill with LLM
extractors).
**Skill level:** comfortable with Brain's schema DSL,
the extractor tiers, and the LLM-tier cost story.

When to use this runbook:

- You're running Brain in **substrate-only mode** (no
  schema declared) and want to enable the knowledge
  layer.
- You want to **migrate an existing schema** to a new
  version.
- You want to **revert** a schema-declared deployment
  to substrate-only behaviour.

For an incident *caused by* a schema upload (extractor
gone wrong, costs spiking), this runbook covers the
rollback path; see also
[RB-14](rb-14-llm-cost-spike.md) for cost-side issues.

---

## Am I in the right runbook?

Use this if you're about to (or just did):

- Run `brain-cli schema upload` for the first time on
  a deployment.
- Run `brain-cli schema upload` to bump an existing
  schema's version.
- Run a backfill against historical memories.
- Reverse one of the above.

This is an **operational** runbook (planned work),
not an **incident** runbook (alert-driven). The
"Stop the bleeding" section is "pre-flight checks"
instead.

---

## Pre-flight checklist

Before the first command:

- [ ] **Snapshot the current state.** A full backup of
      the data directory:
      ```bash
      brain-cli admin snapshot take --label "pre-schema-$(date +%Y%m%d)"
      ```
- [ ] **Run the substrate's health checks.** It should
      be green before you start.
      ```bash
      brain-cli admin shards | jq '.[].status'
      brain-cli admin workers
      ```
- [ ] **Review the schema** for correctness:
      ```bash
      brain-cli schema validate --file my-schema.brain
      ```
- [ ] **Estimate LLM cost** if extractors include the
      LLM tier:
      - How many memories × LLM extractor × cost-per-call?
      - For a 100K-memory shard with one LLM extractor
        at $0.001/call, that's ~$100 if no cache
        hits. Cache eventually mitigates.
- [ ] **Schedule a maintenance window** if backfill is
      expected. Backfill consumes shard time.
- [ ] **Notify stakeholders.** Schema changes can be
      noticed by clients (recall returns
      additional types, latency profile shifts).
- [ ] **Verify LLM API keys** are set in env if
      relevant.

---

## Step 1 — Validate the schema (dry-run)

```bash
brain-cli schema validate --file my-schema.brain
```

Expected output: `0 errors`. Address every reported
error before continuing. Common errors:

- **Unresolved type references** — the schema mentions
  `Person` but doesn't declare it.
- **Conflicting predicate kinds** — two declarations
  of the same predicate with different signatures.
- **Missing extractor definitions** — an extractor
  declared but its body is empty.
- **Regex too complex** — pattern extractor's regex
  exceeds the 1 MB compiled-size cap.
- **Invalid attribute type** — schema DSL only
  supports a small set (`String`, `Integer`, etc.).

For each error, fix the schema file and re-validate.
Don't skip this step — the upload is a write
transaction; rolling back is harder than fixing
upfront.

---

## Step 2 — Declare the schema

```bash
brain-cli schema upload --file my-schema.brain
```

Expected response:

```text
namespace: acme
schema_version: 1
validation_errors: []
migration_summary: { total_items: 0 }
```

What happened server-side:

- The schema document was parsed and validated again.
- Schema definitions were committed to redb.
- The per-shard `SchemaGate` flipped from `false` to
  `true`.
- Substrate RECALL now routes through the hybrid
  pipeline (semantic + lexical + graph).
- Knowledge-layer opcodes (entity / statement /
  relation / query) start accepting requests.

The flip is **immediate** but the actual extractor
activity takes effect on the next encode. Existing
memories are not auto-processed; see Step 3.

### Verify the flip

```bash
brain-cli schema list
```

Should show your schema's namespace and version.

```bash
brain-cli admin schema-gate
```

Should report `declared: true`.

---

## Step 3 — Backfill existing memories

The schema upload doesn't process historical memories.
To extract entities / statements / relations from
existing data, run a backfill:

```bash
brain-cli admin backfill --dry-run \
    --extractors all \
    --memory-range all
```

**Always start with `--dry-run`.** It returns the
plan: number of memories that would be processed,
estimated cost (LLM dollars + shard time), expected
items to be produced. Read the output carefully.

For a real backfill, drop the flag:

```bash
brain-cli admin backfill \
    --extractors all \
    --memory-range all
```

Useful flags:

- `--extractor pattern,classifier` — limit to specific
  tiers. Useful to start with the cheap tiers and
  add LLM later.
- `--memory-range start..end` — partial backfill by
  ID. Useful for staged rollout.
- `--priority background|low` — override priority.
  Default lets the substrate balance backfill
  against serving.

### Monitor backfill progress

```bash
brain-cli admin backfill status
```

Shows: total items, completed, in flight, failed,
estimated remaining time, estimated remaining cost.

If a backfill is **interrupted** (Ctrl-C, restart),
rerun the same command. It resumes from the
checkpoint, doesn't restart from scratch.

### What can go wrong

- **Per-extractor failure rate >50%**: the backfill
  worker aborts automatically and marks the affected
  range as failed. Investigate the extractor.
- **LLM rate-limit hit:** the backfill pauses; resumes
  after the rate-limit window. Or escalate to
  [RB-14](rb-14-llm-cost-spike.md) if costs are
  unexpected.
- **Backfill seems stuck:** check the
  `backfill` worker via
  [RB-05](rb-05-worker-stuck.md).

---

## Step 4 — Verify

Once the schema is uploaded (and backfill is
either done or in progress):

```bash
# Hybrid query should return contributing-retriever info.
brain-cli query "test cue"

# Statements created by extractors.
brain-cli statement list --subject <entity-id>

# Entities accumulating.
brain-cli entity list --limit 10
```

The `contributing_retrievers` field in query
responses is `[semantic, lexical, graph]` (or a
subset) when hybrid is active. It's missing on
substrate-only deployments.

---

## Step 5 — Migrating an existing schema

To upload a new version of an existing schema:

```bash
brain-cli schema validate --file my-schema-v2.brain  # always validate first
brain-cli schema upload --file my-schema-v2.brain
```

The server-side `SCHEMA_UPLOAD` handler computes a
**MigrationPlan**: a list of affected
`(memory, extractor)` pairs that need to be
re-processed under the new version.

For a typical version bump (a new extractor; a
prompt tweak), the plan is small.

For a major version (most extractors changed), the
plan can be huge — every existing memory × every
new extractor. Treat it like a fresh backfill:
dry-run first; size the cost.

Response includes `migration_summary.total_items` so
you can see the size.

If you set `--dry-run`, the plan is returned but the
migration is **not** enqueued.

---

## Step 6 — Reverting (substrate-only fallback)

There is **no `SCHEMA_DROP` opcode in v1**. To
return to substrate-only behaviour:

### Option A — stop using knowledge-layer ops

The simplest:

- Substrate primitives (ENCODE / RECALL / etc.)
  continue working unchanged.
- Knowledge-layer data stays on disk, unused.
- Clients can stop calling `query`, `entity create`,
  etc.; substrate behaviour is identical to a fresh
  install.
- The schema gate stays on, but it doesn't matter:
  the substrate keeps serving.

Use this if you decided the knowledge layer isn't
right for your deployment and you don't want to
mess with the data.

### Option B — empty the active-schema pointer

Destructive of the schema **pointer**, not the data:

1. Stop the substrate cleanly.
2. Open `metadata.redb` with redb-cli (or write a
   small tool against `brain-metadata`).
3. Remove the namespace row from
   `schema_active_versions`.
4. Restart. The per-shard `SchemaGate` re-seeds from
   metadata and reads empty → `false`. Hybrid path
   is disabled; substrate RECALL is plain semantic.

A redb-cli wrapper for this is tracked as a post-v1
operator-tooling task. Until then, this is a manual
operation; consider it engineering territory.

### Option C — restore from a pre-schema snapshot

If you took a snapshot before declaring the schema:

```bash
sudo systemctl stop brain-server
sudo mv /var/lib/brain/data /var/lib/brain/data.with-schema.bak
sudo /usr/local/bin/brain-snapshot-restore \
    --from <pre-schema-snapshot> \
    --to /var/lib/brain/data
sudo systemctl start brain-server
```

You lose any data written after the snapshot.

---

## Verify (post-revert)

```bash
brain-cli schema list                  # empty?
brain-cli admin schema-gate             # declared: false?
brain-cli recall "test"                 # substrate-only response shape?
```

---

## Post-operation

For a successful schema upload + backfill, post in
your team channel:

```
:white_check_mark: Schema upload complete.
Namespace: acme
Version: 1
Backfill: 142,832 memories processed, 18,247 statements + 4,127 entities + 832 relations created.
LLM cost: $87.42
Duration: 6h 12m
Follow-up: TICKET-NNNN (review extractor audit log for failures).
```

For a revert, similarly document what you did and why.

---

## Pitfalls

### LLM cost overrun

A backfill that hits the LLM tier on millions of
memories can cost hundreds-to-thousands of dollars.
**Always dry-run first.** If the projected cost
exceeds your budget, either:

- Backfill only the highest-value subset (recent
  memories, specific agents).
- Use cheaper extractors (classifier or pattern
  only).
- Sample: backfill 10% to estimate quality, then
  decide on the rest.

### Schema version bumps mid-recall

If you upload a new schema while clients are
actively querying, the query path picks up the new
version immediately. Most operations are
backward-compatible; if not, you may see a brief
spike in query errors.

Plan schema uploads during quieter periods if
possible.

### Backfill running during high traffic

Backfill consumes shard time. If your normal load
is heavy, backfill can degrade latency. Either
schedule backfill during low-traffic windows, or
use `--priority low` so the substrate prioritises
serving.

### Memory text not persisted limitation

In v1, full memory text is not always available to
the backfill worker (it lives in metadata; some
extractors may need text not retained at v1
defaults). The backfill marks each affected item
`Failed` with reason `"memory text not persisted"`.
Operators may need to re-ingest from source-of-truth
in v1; full content-aware backfill is a post-v1
enhancement.

---

## Recovery (if backfill goes wrong)

- **Backfill stuck:**
  ```bash
  brain-cli admin backfill cancel --request-id <id>
  ```
  Then re-investigate and resume.
- **Migration stuck:**
  ```bash
  brain-cli admin schema migration cancel --request-id <id>
  ```
- **SCHEMA_UPLOAD failed mid-flight:** redb is
  transactional; either the upload landed or it
  didn't. Check `brain-cli schema list`.
- **Suspicious failure rate:** a backfill that
  exceeds 50% failure across its first 100 items
  aborts automatically. Inspect operator metrics
  and worker logs.

---

## Related runbooks

- [RB-05 — Worker stuck](rb-05-worker-stuck.md) (if
  the `backfill` worker hangs)
- [RB-14 — LLM cost spike](rb-14-llm-cost-spike.md)
  (if a backfill is driving LLM costs through the
  roof)
- [OP-02 — Snapshot restore drill](op-02-snapshot-restore-drill.md)
  (you'd want this validated before a major schema
  upload)
- [Concepts: schemas](../concepts/15-schemas.md)
- [Concepts: the knowledge layer](../concepts/02-two-layer-model.md)
- [Concepts: extractors](../concepts/14-extractors.md)

---

## Last validated

*Update on first use.*
