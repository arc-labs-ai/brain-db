# 27 — FAQ

Common questions, grouped by topic. Each answer is short
and links to the chapter that explains the topic
properly.

If you want the alphabetical-vocabulary view instead,
[chapter 26](26-glossary.md) is the glossary.

---

## Getting started

### What is Brain?

A database whose primary operations are cognitive verbs
(encode, recall, plan, reason, forget) instead of CRUD.
It stores text + its meaning (as embeddings) + optionally
a typed graph of who-said-what-about-whom. Built for AI
agents that need persistent memory beyond a context
window. See [chapter 01](01-what-brain-is.md).

### Should I use Brain or a vector database?

If you only need "find similar vectors at scale and
you'll write the rest yourself," a vector DB
(Pinecone, Qdrant, Milvus) is the smaller, simpler
choice. If you want the full cognitive substrate —
substrate-managed embedding, forget semantics,
background workers, optional typed knowledge — Brain
fits better. See [chapter 04](04-vs-other-systems.md).

### Should I use Brain or Postgres?

Different layers. Postgres is for tabular business data
(users, orders, line items); Brain is for the agent's
memory (conversations, extracted knowledge). The common
production shape is "Postgres for app data + Brain for
agent memory." See [chapter 04](04-vs-other-systems.md).

### How do I get started?

Run Brain in Docker:
[`tutorials/01-quickstart-docker.md`](../tutorials/01-quickstart-docker.md).
You'll have a working server in a few minutes. Then
read [chapter 03](03-guided-tour.md) for a walkthrough
of encode/recall/forget.

### Do I have to declare a schema?

No. Brain runs as a pure vector substrate without any
schema. You only declare a schema if you want the
knowledge layer (entities, statements, relations). See
[chapter 02](02-two-layer-model.md) for the two-layer
model and [chapter 15](15-schemas.md) for what
declaring a schema does.

---

## How it works

### Why are the API verbs `encode/recall/forget` and not `put/get/delete`?

Because the operations *aren't* CRUD. `encode` runs an
embedding model and a WAL fsync. `recall` runs vector
search and (with a schema) fuses three retrievers.
`forget` tombstones with a grace period and cascades
through derived knowledge. Using CRUD names would lie
about what's happening. See [chapter 16](16-cognitive-operations.md).

### What's actually stored when I encode a memory?

Three pieces: the original text (verbatim), a 384-float
vector representing its meaning, and metadata (agent_id,
kind, salience, timestamps, ...). The text lives in a
B-tree (redb); the vector lives in a memory-mapped flat
file (the arena); both pieces of one memory are tied
together by a `MemoryId`. See [chapter 05](05-memories.md).

### Where do the 384 numbers in an embedding come from?

A small ML model (BGE-small-en-v1.5) running on the
server reads the text and produces 384 numbers
representing its meaning. Similar texts produce
similar numbers. The model has been trained on
millions of sentence pairs to make this work. See
[chapter 08](08-embeddings.md).

### How does Brain decide two memories are similar?

By computing the cosine similarity between their
384-dim embeddings — geometrically, the cosine of the
angle between the two vectors in the 384-dimensional
embedding space. For L2-normalised vectors (which
Brain's embeddings always are), cosine similarity
equals the dot product. See [chapter 09](09-vector-similarity.md).

### Why is search "approximate"? Doesn't it miss things?

Exact nearest-neighbour search in 384 dimensions is
`O(N)` brute force. For 1M memories per shard, that's
too slow. The approximate algorithm (HNSW) finds the
top-K in `O(log N)` time at ~96% recall — you'd miss
about 4% of true top-10 results. The trade-off is
usually worth ~50× speed-up. See [chapter 20](20-indexes-exact-vs-approximate.md).

### Why does Brain run two async runtimes?

Tokio (work-stealing) is the right fit for the edge —
TCP accept, TLS, HTTP, broad ecosystem. Glommio
(thread-per-core, io_uring) is the right fit for
storage — single-writer-per-shard, low-jitter,
io_uring for fast writes. Different requirements, two
runtimes, one channel between them. See [chapter 22](22-concurrency-and-async.md).

### How does forgetting actually work?

`forget` immediately tombstones the memory (invisible to
queries) and writes a WAL record. The slot's bytes stay
on disk through a configurable grace period (default 7
days), after which a background worker reclaims the
slot. Hard-forget zeros the bytes immediately for
privacy. See [chapter 16](16-cognitive-operations.md)
and [chapter 18](18-storage-and-durability.md).

---

## Durability and trust

### What survives a crash?

Any memory whose `encode` returned successfully. Brain
writes a record to the write-ahead log and fsyncs it
*before* acknowledging the encode. Power loss in the
next millisecond does not lose the memory. See
[chapter 18](18-storage-and-durability.md).

### What survives disk loss?

Whatever you've backed up off-server. Brain doesn't
replicate or fail-over in v1; you need to run the
snapshot worker and copy snapshots elsewhere. The
operational pattern is documented in
[`../guides/deployment/backup-restore.md`](../guides/deployment/backup-restore.md).

### Is Brain ACID?

It honours **A**tomicity (each operation either fully
commits or fully aborts), **C**onsistency (the invariants
in [chapter 24](24-invariants-and-trust.md) hold), and
**D**urability (WAL-before-ack). **I**solation is
single-writer-per-shard — for everything in one shard,
writes are naturally serialised. Cross-shard isolation
is per-agent: agents can't see each other's memories.
See [chapter 24](24-invariants-and-trust.md).

### Can I retry an operation safely?

Yes, with the same `request_id`. Brain uses it to
deduplicate retries. Same `request_id` + same params →
the original response. Same `request_id` + different
params → an `IdempotencyConflict` error (client bug).
See [chapter 25](25-determinism-idempotency-replay.md).

### What if I encode the same text twice?

You get two memories. Brain doesn't auto-dedup by
content. If you want that behaviour, generate the
client-side `request_id` deterministically from the text
hash, and a retry with the same id replays. See
[chapter 25](25-determinism-idempotency-replay.md).

### What's a "fail-stop" system?

A system that stops serving rather than serving
potentially-wrong data. If Brain detects internal
inconsistency (corrupt file, wrong CRC, version
mismatch), it refuses to operate instead of pressing on.
The opposite of "best-effort degraded mode." See
[chapter 24](24-invariants-and-trust.md).

---

## Operations

### How many shards should I configure?

Roughly `num_cores - 4` for a single-server deployment,
reserving cores for the edge (Tokio), embedding,
background workers, and OS. The shard count is fixed at
deployment time; changing it later requires a data
migration. See [chapter 23](23-sharding-and-isolation.md).

### Can I add shards at runtime?

No. v1 doesn't support runtime resharding. The shard
count is fixed at server start. To scale beyond the
initial count, redeploy with more shards and migrate
data (export + import). See [chapter 23](23-sharding-and-isolation.md).

### What hardware should I run Brain on?

A modern Linux server with enterprise-grade NVMe SSDs
(for the WAL fsync story) and enough RAM for the
working set. Tens of GB of RAM per shard is typical;
the arena and embedding cache benefit from being hot
in the kernel page cache. See [chapter 18](18-storage-and-durability.md)
and the deployment guides.

### How big can a shard get?

In v1, ~10M memories is a comfortable upper bound per
shard. The HNSW index lives in RAM; at 10M × 1.5KB
arena slots plus index overhead, a shard is using a few
GB of RAM. Beyond ~10M, you'd want more shards rather
than scaling one. See [chapter 23](23-sharding-and-isolation.md).

### Do I need snapshots?

In production, yes. Without them, a process restart
rebuilds the HNSW index from scratch (~30 seconds for
1M memories), and recovery has to replay the entire
WAL. With them, restart is sub-second and recovery
replays only post-snapshot WAL records. The snapshot
worker is opt-in; turn it on. See [chapter 18](18-storage-and-durability.md).

### How do I back up Brain?

Run the snapshot worker on a cadence (hourly is fine).
Copy the resulting snapshot directory to off-server
storage (S3, GCS, equivalent). See
[`../guides/deployment/backup-restore.md`](../guides/deployment/backup-restore.md).

### Does Brain support multi-tenant isolation?

Soft isolation per `agent_id` works out of the box —
agents can't see each other's memories. For *strong*
isolation (compliance, regulated tenants), run one
Brain instance per tenant. See [chapter 23](23-sharding-and-isolation.md).

---

## The knowledge layer

### What's the difference between the substrate and the knowledge layer?

The substrate stores memories (text + vector +
metadata). The knowledge layer derives entities,
statements, and relations from memories using
extractors. The substrate is always on; the knowledge
layer activates when you declare a schema. See
[chapter 02](02-two-layer-model.md).

### When should I declare a schema?

When your domain has well-defined types (Person,
Project, ...) and you want structured queries against
them, plus provenance back to the source memories. If
all you need is "remember and recall," skip the
schema. See [chapter 15](15-schemas.md).

### Can I change a schema later?

Yes, additively. New entity types, predicates, relation
types, and extractors can all be added. You can also
modify existing extractors (new prompt, new model);
they get a version bump and a backfill option. What
you *can't* easily do is remove things that existing
data references. See [chapter 15](15-schemas.md).

### Do I have to write extractors myself?

For pattern extractors: yes — declare the regexes in
your schema. For classifier extractors: you provide
the model file; the substrate runs it. For LLM
extractors: you write the prompt and JSON schema; the
substrate calls the model. Brain doesn't train
extractors; you bring the brains. See [chapter 14](14-extractors.md).

### What does it cost to run LLM extractors?

Depends on the model, the prompt length, the rate of
new encodes, and how many cache hits you get. With a
per-call budget of `$0.001` and a Claude Haiku model,
you can extract from thousands of memories per dollar.
The cache makes retries free. Operators set per-call
budgets to bound spend. See [chapter 14](14-extractors.md).

### Why three statement kinds (Fact / Preference / Event)?

Each has a different mutation contract. Facts are
append-only (contradicting Facts both stored).
Preferences are versioned (new supersedes old). Events
are immutable. Three kinds match how people think about
claims; merging them into one type would lose the
distinction. See [chapter 12](12-fact-preference-event.md).

### What happens to derived statements when I forget the source memory?

The `forget_cascade` background worker re-evaluates each
statement that cited the memory as evidence. If other
memories still support it, confidence is recomputed.
If the memory was the only evidence, the statement is
superseded with `superseded_by = null` (effectively
retracted). See [chapter 16](16-cognitive-operations.md).

### How does Brain decide if two mentions are the same entity?

Through a four-tier entity resolver: exact canonical
name → alias → trigram fuzzy match → vector similarity.
If none of the tiers match, a new entity is created.
See [chapter 10](10-entities.md).

---

## Limits and edge cases

### What's the maximum text length per memory?

The substrate stores the full text verbatim, no
length cap. But the embedder truncates inputs above 512
tokens before computing the vector. So very long
memories have an embedding computed from only the first
~512 tokens' worth. For long content, pre-segment into
paragraph-sized chunks client-side. See [chapter 08](08-embeddings.md).

### Can I use a different embedding model?

In principle, yes. Anything producing 384-dim
L2-normalised float vectors via `BertModel::forward`
works. v1 ships with BGE-small. Swapping models means
re-embedding all stored memories — the substrate can do
this via a background worker after a model upgrade.

### Does Brain support GPU embedding?

CPU only in v1. GPU support is on the roadmap. For
deployments where embedding throughput is the
bottleneck, this matters; for most workloads, the LRU
cache + CPU inference is sufficient.

### Does Brain support languages other than English?

The default embedder (BGE-small-en) is English-only.
Swap to a multilingual model (e.g., BGE-M3) if you
need other languages. The substrate is
language-agnostic — only the embedder cares.

### Can I run Brain on Mac or Windows?

For development, yes (with the storage layer stubbed
out). Production: no. Brain depends on `io_uring`, a
Linux-specific kernel API, for its concurrency and
durability primitives. macOS and Windows targets exist
in the build matrix but aren't supported runtimes.

### What about sub-millisecond recall latency?

For warm cache hits with the embedder cache populated,
recall latency can drop into the low single-digit
milliseconds. Cold cache misses include the embedder's
forward pass (5–10 ms on CPU) which sets a floor. If
you need sub-millisecond p99, you'd need to wire a
client-side embedding cache *and* accept the
operational complexity.

### What's the largest deployment Brain has been tested on?

Internal testing has covered up to several million
memories across multiple shards on a single server.
External production deployments will inform real-world
scale; the design targets are documented in the
architecture tier.

---

## Where to ask more

### Where do I find detailed configuration reference?

[`../reference/configuration.md`](../reference/configuration.md)
— every TOML field with its default and acceptable
range.

### Where do I find the wire protocol spec?

[`../reference/wire-protocol/`](../reference/wire-protocol/)
covers frame format, opcodes, error codes, and the
handshake.

### Where do I find the architecture-tier detail?

[`../architecture/`](../architecture/) — twelve
chapters covering implementation detail with code
citations. Read this when you want to read the source.

### What if my question isn't here?

The glossary ([chapter 26](26-glossary.md)) covers
vocabulary. The architecture tier covers internals. The
guides cover operations. If you're still stuck, open an
issue or check the spec's `*_open_questions.md` files
for what's deferred or under discussion.

---

## Recap

This FAQ collects the most common questions across the
27-chapter concepts tier. For depth on any answer, follow
the linked chapter. For implementation-level detail,
follow the architecture tier links.
