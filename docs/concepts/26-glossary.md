# 26 — Glossary

Alphabetical reference for every term that's load-bearing
in the rest of the docs. Each entry has a one-to-three
sentence definition and a link to the chapter that
explains it properly.

If you're reading the docs and hit a word you don't
recognise, this chapter is the first place to look.

---

## A

**Agent.** A client of Brain — an AI agent, a user-facing
application, or any caller of the wire protocol. Each
agent has an `agent_id`; the substrate isolates memories
per agent.

**`agent_id`.** A 16-byte UUID-shaped identifier for an
agent. Every memory is tagged with its `agent_id`;
queries are implicitly filtered by it. See
[chapter 23](23-sharding-and-isolation.md).

**ANN (Approximate Nearest Neighbour).** A class of
algorithms that find the *approximately* closest
vectors in a high-dimensional space. Trades exactness
for speed. Brain uses HNSW. See
[chapter 20](20-indexes-exact-vs-approximate.md).

**Arena.** A flat, memory-mapped file (`arena.bin`) that
holds the vector portion of every memory in fixed-size
slots. The substrate's primary storage for vectors. See
[chapter 19](19-mmap-and-arenas.md).

**`ArcSwap`.** A lock-free atomic pointer swap primitive.
Brain uses it for the schema gate so the hot recall
path can check schema status without locking.

**Async runtime.** The engine that drives Rust's
`async`/`.await` code. Brain uses Tokio at the edge and
Glommio per shard. See [chapter 22](22-concurrency-and-async.md).

**Audit log.** A table of records describing every
extractor invocation — its inputs, outputs, status,
and (for LLM extractors) cost. See
[chapter 14](14-extractors.md).

---

## B

**Backfill.** A background worker that re-runs extractors
over historical memories — typically triggered by an
admin RPC after a schema upload. See
[chapter 14](14-extractors.md) and
[chapter 15](15-schemas.md).

**BERT.** A 2018 transformer-based language model
architecture from Google. BGE-small is BERT-style. See
[chapter 08](08-embeddings.md) and
[Wikipedia: BERT](https://en.wikipedia.org/wiki/BERT_(language_model)).

**BGE / BGE-small-en-v1.5.** The embedding model Brain
ships with by default. A 33M-parameter BERT-style model
from BAAI that produces 384-dim L2-normalised vectors.
See [chapter 08](08-embeddings.md).

**BLAKE3.** A modern cryptographic hash function — fast,
parallelisable. Used by Brain for routing (shard
selection), content addressing, and several integrity
checks. See [BLAKE3 official site](https://github.com/BLAKE3-team/BLAKE3).

**BM25.** The classical text-search ranking function used
by Brain's lexical retriever. Weights document matches
by term frequency × inverse document frequency. See
[chapter 21](21-lexical-and-fusion.md).

**Brain.** The product. A cognitive substrate for AI
agents — a database whose primary operations are
cognitive verbs (encode, recall, plan, reason, forget)
plus optional typed-knowledge support.

---

## C

**Cache (LLM extractor cache).** A per-shard redb file
(`llm_cache.redb`) that stores LLM extraction responses
keyed by `(text_hash, extractor_id, extractor_version,
model_id)`. Makes LLM extractions effectively
deterministic and avoids redundant API calls. See
[chapter 14](14-extractors.md).

**Cache (embedder cache).** An LRU cache inside the
embedder that maps text → vector. Skips inference for
repeated text. See [chapter 08](08-embeddings.md).

**Classifier extractor.** An extractor that uses a pinned
ML model (BERT-style or similar) to extract structured
data from text. Faster than LLM, broader than pattern.
See [chapter 14](14-extractors.md).

**Cognitive operations.** The five verbs Brain exposes:
encode, recall, plan, reason, forget. See
[chapter 16](16-cognitive-operations.md).

**Cognitive substrate.** The label Brain uses to
distinguish itself from "vector database." A substrate
owns the embedder, the cognitive verbs, the memory
lifecycle, and the typed knowledge layer. See
[chapter 01](01-what-brain-is.md).

**Confidence.** A `[0, 1]` score on statements and
relations indicating how strongly the substrate trusts
the claim. Set by the extractor that produced it. See
[chapter 11](11-statements.md).

**Consolidation.** A background process where the
substrate clusters similar episodic memories and
summarises them into a single Consolidated memory. The
"sleep" analogue from cognitive science. See
[chapter 07](07-salience-decay-consolidation.md).

**Consolidated memory.** A memory of `kind = Consolidated`
— produced by the consolidation worker as a summary of
multiple related episodic memories. Decays slower than
Episodic; faster than Semantic. See
[chapter 06](06-memory-kinds.md).

**Context window.** The maximum text a language model can
read in one call. Anything outside is invisible to the
model. See [chapter 01](01-what-brain-is.md).

**Cosine similarity.** A measure of similarity between
two vectors, computed as the cosine of the angle between
them. In `[-1, 1]`; 1 is identical, 0 is orthogonal,
-1 is opposite. For L2-normalised vectors, equal to
the dot product. See [chapter 09](09-vector-similarity.md).

**CRC / CRC32C.** Cyclic Redundancy Check — a checksum
for detecting accidental data corruption. CRC32C is the
Castagnoli variant, accelerated by modern CPUs and
used throughout Brain's storage layer. See
[chapter 18](18-storage-and-durability.md).

---

## D

**Decay.** The process by which a memory's salience
decreases over time without recall. Brain models it as
exponential with a per-kind half-life. See
[chapter 07](07-salience-decay-consolidation.md).

**Determinism.** The property that same inputs always
produce the same outputs. Pattern and classifier
extractors are deterministic by construction; LLM
extractors become effectively deterministic through
caching. See [chapter 25](25-determinism-idempotency-replay.md).

**Domain (relation type).** In a relation type
declaration, the type of the `from` endpoint. See
[chapter 13](13-relations.md).

**Durable.** Bytes on stable storage that would survive
a power loss. Stronger than "written" — requires
explicit fsync (or equivalent) plus cooperating
hardware. See [chapter 18](18-storage-and-durability.md).

---

## E

**Ebbinghaus forgetting curve.** Hermann Ebbinghaus's
1885 model of human memory: without rehearsal,
retention decays roughly exponentially. Brain models
salience decay along the same shape. See
[chapter 07](07-salience-decay-consolidation.md) and
[Wikipedia](https://en.wikipedia.org/wiki/Forgetting_curve).

**Edge / EdgeKind.** A typed connection between two
*memories* (not entities — those are relations).
Substrate-level. See [chapter 05](05-memories.md).

**Embedding.** A 384-element vector representing the
meaning of a piece of text. Produced by the embedder
model (BGE-small in Brain). See
[chapter 08](08-embeddings.md).

**Encode.** The verb for storing a new memory. Brain
embeds the text on the server, allocates a slot, writes
the WAL, and acknowledges only after fsync. See
[chapter 16](16-cognitive-operations.md).

**Entity.** A typed identity anchor in the knowledge
layer — a Person, Project, Place, etc. Has an
`EntityId`, a canonical name, aliases, and typed
attributes. See [chapter 10](10-entities.md).

**Entity HNSW.** A separate HNSW index used by the entity
resolver for vector-similarity-based deduplication of
entity mentions. See [chapter 10](10-entities.md).

**Entity resolver.** The process that decides whether a
new entity mention matches an existing entity. Runs
through four tiers: exact match, alias, trigram, vector
similarity. See [chapter 10](10-entities.md).

**Entity type.** A schema-declared type for entities
(e.g., `Person`, `Project`). Carries typed attributes.
See [chapter 10](10-entities.md) and
[chapter 15](15-schemas.md).

**Episodic memory.** A memory of `kind = Episodic` —
capturing a specific moment or observation. Default
kind. Decays with a 30-day half-life. See
[chapter 06](06-memory-kinds.md).

**Event (statement kind).** A statement of
`kind = Event` — captures a discrete occurrence at a
moment. Immutable; new events don't supersede old. See
[chapter 12](12-fact-preference-event.md).

**Evidence.** The list of memories that support a
statement or relation. Provides provenance — the agent
can drill into a statement and see why Brain believes
it. See [chapter 11](11-statements.md).

**Extractor.** A pipeline that derives structured items
(entities, statements, relations) from memory text.
Three tiers: pattern, classifier, LLM. See
[chapter 14](14-extractors.md).

---

## F

**Fact (statement kind).** A statement of `kind = Fact`
— a stable claim about the world. Append-only;
contradicting Facts are both stored, with the conflict
surfaced. See [chapter 12](12-fact-preference-event.md).

**Fail-stop.** A failure model where a system stops
serving rather than serving potentially-wrong data. One
of Brain's seven invariants. See
[chapter 24](24-invariants-and-trust.md).

**`fdatasync`.** A Unix system call similar to `fsync`
but skipping metadata sync. Equivalent durability for
append-only data; one less I/O. See
[chapter 18](18-storage-and-durability.md).

**Filter pushdown.** Moving a query filter from
post-fusion (applied after retrieval) into the
retriever's index lookup (applied during retrieval).
The biggest hybrid-recall performance win. See
[chapter 17](17-hybrid-retrieval.md).

**Forget.** The verb for removing a memory. Tombstones
the memory immediately; reclaims its slot after a
grace period (default 7 days). Knowledge-active mode:
cascades to statements/relations citing the memory. See
[chapter 16](16-cognitive-operations.md).

**Forget cascade.** A background worker that re-evaluates
statements and relations whose evidence included a
forgotten memory. May reduce confidence or supersede
the claim. See [chapter 16](16-cognitive-operations.md).

**`fsync`.** A Unix system call that forces a file's
buffered writes to stable storage. The primitive Brain
uses to make WAL records durable. See
[chapter 18](18-storage-and-durability.md).

**FUA (Force Unit Access).** A flag on SCSI/NVMe write
commands that ensures the bytes reach non-volatile
media (not just the device's RAM cache). Enterprise
SSDs implement it correctly; consumer SSDs vary. See
[chapter 18](18-storage-and-durability.md).

---

## G

**Glommio.** A Rust async runtime from DataDog —
thread-per-core, single-threaded executors, io_uring-
based. Brain uses one Glommio executor per shard. See
[chapter 22](22-concurrency-and-async.md) and
[github.com/DataDog/glommio](https://github.com/DataDog/glommio).

**Grace period.** The configurable interval (default 7
days) between a memory's `forget` and the actual
reclamation of its storage slot. Ensures stale
`MemoryId`s safely return `NotFound` rather than
accidentally hitting a reused slot. See
[chapter 05](05-memories.md).

**Graph retriever.** One of three hybrid-retrieval
retrievers; walks the typed relations starting from an
entity anchor. See [chapter 17](17-hybrid-retrieval.md).

**Group commit.** A WAL technique where multiple records
share one fsync. Brain batches up to ~60 KB or 100 µs
worth of records per group commit. See
[chapter 18](18-storage-and-durability.md).

---

## H

**Hard forget.** A variant of forget that zeros the
memory's vector and text bytes immediately, before the
grace period. Used for compliance / privacy. See
[chapter 16](16-cognitive-operations.md).

**HNSW (Hierarchical Navigable Small World).** The
approximate-nearest-neighbour algorithm Brain uses for
vector search. Multi-layer graph, navigated greedily.
`O(log N)` query time. See [chapter 20](20-indexes-exact-vs-approximate.md)
and [the original paper](https://arxiv.org/abs/1603.09320).

**Hybrid retrieval.** The recall path used when a schema
is declared: three retrievers (semantic, lexical, graph)
run in parallel and their outputs are fused with RRF.
See [chapter 17](17-hybrid-retrieval.md).

---

## I

**Idempotency.** The property that doing an operation
twice has the same effect as once. Brain's state-
mutating ops carry a `request_id`; retries with the
same id return the cached response. See
[chapter 25](25-determinism-idempotency-replay.md).

**`IdempotencyConflict`.** The error returned when a
client reuses a `request_id` with *different*
parameters. Indicates a client bug; the substrate
refuses to silently overwrite. See
[chapter 25](25-determinism-idempotency-replay.md).

**Inverted index.** An index keyed by *term* rather than
by document. The data structure that makes "find
documents containing term X" cheap. Used by tantivy for
Brain's lexical search. See [chapter 21](21-lexical-and-fusion.md).

**io_uring.** A Linux kernel interface for async I/O.
Programs submit operations through shared-memory rings
with near-zero syscall overhead. Brain's storage relies
on it via Glommio. See [chapter 22](22-concurrency-and-async.md)
and [Wikipedia](https://en.wikipedia.org/wiki/Io_uring).

---

## K

**Knowledge layer.** The optional, opt-in layer of Brain
that derives typed entities, statements, and relations
from memories. Activates when a schema is declared. See
[chapter 02](02-two-layer-model.md).

---

## L

**L2 normalisation.** Rescaling a vector so its
Euclidean magnitude equals 1. After normalisation,
cosine similarity equals the dot product. Brain
L2-normalises every embedding. See
[chapter 09](09-vector-similarity.md).

**Lexical retriever.** One of three hybrid-retrieval
retrievers; runs BM25 text search over the tantivy
indexes. See [chapter 17](17-hybrid-retrieval.md) and
[chapter 21](21-lexical-and-fusion.md).

**Link / unlink.** Verbs for adding or removing a typed
edge between two memories (substrate-level). See
[chapter 16](16-cognitive-operations.md).

**LLM.** Large Language Model. Brain uses LLMs only in
the LLM extractor tier; it doesn't generate text
itself. See [chapter 14](14-extractors.md).

**LLM extractor.** The third extractor tier — calls an
LLM (Claude, GPT, …) to extract structured data with
JSON schema validation. Cached per-shard for replay
safety. See [chapter 14](14-extractors.md).

**LSN (Log Sequence Number).** A monotonically-
increasing counter on WAL records. Identifies a unique
position in the log; used by recovery to know what's
been applied. See [chapter 18](18-storage-and-durability.md).

---

## M

**Memory.** The substrate's unit of storage: text +
384-dim vector + metadata. See [chapter 05](05-memories.md).

**`MemoryId`.** A 16-byte handle to a specific memory,
encoding shard, slot, slot version. Safe across
forget+reclamation thanks to the slot-version check.
See [chapter 05](05-memories.md).

**mmap.** A Unix system call that maps a file directly
into a process's address space. Reads look like array
accesses; the OS handles disk I/O on demand. Brain's
arena uses it. See [chapter 19](19-mmap-and-arenas.md).

**Model fingerprint.** A 16-byte BLAKE3 of the
embedder's config + tokenizer + weights + dim +
normalize-flag. Marks each memory's embedding with the
specific model that produced it. See
[chapter 08](08-embeddings.md).

---

## N

**NFKC.** Normalization Form Compatibility Composition —
a Unicode normalisation that rewrites text into a
canonical form. Brain uses it to normalise entity
names so visually-identical strings compare equal. See
[chapter 10](10-entities.md).

---

## P

**Page cache.** The kernel's pool of in-RAM copies of
recently-accessed disk pages. mmap'd files participate
in the page cache like any other file. See
[chapter 19](19-mmap-and-arenas.md).

**Pattern extractor.** The first extractor tier — uses
regex to extract structured data from text. Fast,
deterministic, high precision, narrow recall. See
[chapter 14](14-extractors.md).

**Per-agent isolation.** The substrate's soft multi-
tenancy model: each agent's memories are filtered out
of other agents' queries. Enforced by the substrate,
not by hardware. See [chapter 23](23-sharding-and-isolation.md).

**Plan (verb).** A cognitive operation that returns a
sequence of memories the agent could traverse to get
to a goal. See [chapter 16](16-cognitive-operations.md).

**Predicate.** The verb part of a statement
(`subject predicate object`). Schema-declared; interned
to a `u32` ID. See [chapter 11](11-statements.md).

**Preference (statement kind).** A statement of
`kind = Preference` — captures a revisable belief or
choice. Versioned via supersession. See
[chapter 12](12-fact-preference-event.md).

**Provenance.** The chain of evidence backing a
knowledge-layer claim. Every statement carries the
memories it derived from; forgetting a memory triggers
a cascade. See [chapter 11](11-statements.md).

---

## R

**Range (relation type).** In a relation type
declaration, the type of the `to` endpoint. See
[chapter 13](13-relations.md).

**Recall.** The verb for finding memories similar to a
cue. Substrate-only: vector search. Knowledge-active:
hybrid retrieval. See [chapter 16](16-cognitive-operations.md).

**Reason (verb).** A cognitive operation that combines
existing memories into a new derived one. The most
experimental of the five verbs. See
[chapter 16](16-cognitive-operations.md).

**Reciprocal Rank Fusion (RRF).** The fusion algorithm
that combines multiple ranked retrieval lists into one
ranking. Score-scale invariant; rank-only. `k = 60` is
the default smoothing constant. See
[chapter 17](17-hybrid-retrieval.md) and
[chapter 21](21-lexical-and-fusion.md).

**Recovery.** The substrate's process for reconstructing
state after a crash: replay the WAL from the last
durable LSN, rebuild indexes. See [chapter 18](18-storage-and-durability.md).

**redb.** The B-tree key-value store Brain uses for
metadata. A Rust-native embedded ACID database with
copy-on-write semantics. See [github.com/cberner/redb](https://github.com/cberner/redb)
and the architecture-tier [chapter 05](../architecture/05-redb-metadata.md).

**Relation.** A typed edge between two entities. See
[chapter 13](13-relations.md).

**Relation type.** A schema-declared type for relations
(e.g., `reports_to`, `works_on`). See
[chapter 13](13-relations.md) and [chapter 15](15-schemas.md).

**Replay.** Re-running a previous operation and getting
the same result. Applies to WAL recovery, extractor
backfill, LLM cache hits, and client retries. See
[chapter 25](25-determinism-idempotency-replay.md).

**`request_id`.** A 16-byte client-generated UUID that
identifies a logical operation. Used by Brain to
deduplicate retries. See
[chapter 25](25-determinism-idempotency-replay.md).

---

## S

**Salience.** A `[0, 1]` per-memory score representing
how "alive" the memory feels. Decays with time; boosts
on recall. Used in ranking. See [chapter 07](07-salience-decay-consolidation.md).

**Schema.** A declaration of what types your knowledge
layer cares about: entity types, predicates, relation
types, and extractors. Not a SQL schema. See
[chapter 15](15-schemas.md).

**Schema gate.** A per-shard boolean (`ArcSwap<bool>`)
that flips from off to on when the first schema is
declared. Recall checks the gate to decide between
pure-substrate and hybrid paths. See [chapter 02](02-two-layer-model.md).

**Semantic memory.** A memory of `kind = Semantic` —
capturing generalised knowledge. Decays slowly
(365-day half-life). See [chapter 06](06-memory-kinds.md).

**Semantic retriever.** One of three hybrid-retrieval
retrievers; runs vector search over the memory HNSW
(and optionally statement HNSW). See [chapter 17](17-hybrid-retrieval.md).

**Shard.** An independent partition of the substrate.
Each shard runs on one CPU, owns its own arena, WAL,
metadata, and indexes. See [chapter 23](23-sharding-and-isolation.md).

**Single-writer-per-shard.** The discipline that, within
a shard, only one task writes to the shard's data at
any moment. Enforced by Glommio's single-threaded
executor, not by locks. See [chapter 22](22-concurrency-and-async.md).

**Slot.** A fixed-size (1600-byte) cell in the arena
that holds one memory's vector + slot-local metadata.
See [chapter 19](19-mmap-and-arenas.md).

**Slot version.** A counter that bumps every time a slot
is reclaimed. Encoded in the `MemoryId` so stale
references safely return `NotFound`. See
[chapter 19](19-mmap-and-arenas.md).

**Snapshot.** A point-in-time copy of a shard's data
files. Used for backup, faster cold start, and
disaster recovery. See [chapter 18](18-storage-and-durability.md).

**Sparse file.** A file whose logical size exceeds its
physical disk usage — unwritten regions cost no disk.
The arena uses sparse storage. See [chapter 19](19-mmap-and-arenas.md).

**Statement.** A typed claim with subject, predicate,
object, evidence, and confidence. Has a kind: Fact,
Preference, or Event. See [chapter 11](11-statements.md).

**Stemming.** Mapping morphological variants of a word
to a single form (`running` → `run`). Brain's lexical
tokenisation uses the Porter stemmer. See
[chapter 21](21-lexical-and-fusion.md).

**Subject.** The entity a statement is about. Always an
`EntityId`. See [chapter 11](11-statements.md).

**Substrate.** Brain's vector-memory layer; always on.
The opposite of the knowledge layer (which is opt-in).
See [chapter 02](02-two-layer-model.md).

**Supersession.** The mechanism by which a new
Preference replaces an old one — old's `superseded_by`
points to new; new's `version` is `old.version + 1`.
History stays queryable. See [chapter 12](12-fact-preference-event.md).

---

## T

**Tantivy.** A Rust full-text search library, conceptual
analog of Apache Lucene. Brain uses it for BM25 indexes
over memory and statement text. See
[chapter 21](21-lexical-and-fusion.md).

**TLS.** Transport Layer Security — the modern protocol
for encrypted network communication. Brain supports it
on the wire protocol. See [chapter 23](23-sharding-and-isolation.md).

**Thread-per-core.** A concurrency model where each CPU
runs one dedicated thread; threads don't share mutable
state. Brain's per-shard executors use this. See
[chapter 22](22-concurrency-and-async.md).

**Tokio.** A Rust async runtime with work-stealing
scheduling. Brain uses it at the edge (network, TLS,
HTTP). See [chapter 22](22-concurrency-and-async.md) and
[tokio.rs](https://tokio.rs/).

**Tokenizer.** A component that splits text into
subword tokens for the embedder or classifier. BGE
uses a WordPiece-style tokenizer. See [chapter 08](08-embeddings.md).

**Tombstone.** A flag on a memory's slot indicating
"this memory is forgotten, hide it from queries." The
slot's bytes are still on disk during the grace period;
hard-forget zeros them earlier. See [chapter 05](05-memories.md).

**`top_k`.** The maximum number of results a recall or
query should return. Configurable per request. See
[chapter 16](16-cognitive-operations.md).

**`top_n` (per retriever).** In hybrid retrieval, the
cap on how many candidates each retriever contributes
to RRF fusion. Default ~100. See [chapter 21](21-lexical-and-fusion.md).

**Transaction (`txn_*`).** A wire-protocol verb trio
(`txn_begin / txn_commit / txn_abort`) for grouping
multiple state mutations into one atomic unit. See
[chapter 16](16-cognitive-operations.md).

**Trigram.** A 3-character window over text. Brain's
entity resolver uses trigram overlap for fuzzy name
matching. See [chapter 10](10-entities.md).

**Two-layer model.** Brain's substrate + knowledge-layer
architecture, with the substrate always on and the
knowledge layer opt-in via schema declaration. See
[chapter 02](02-two-layer-model.md).

---

## U

**UUIDv7.** A version of UUID that embeds a timestamp in
its first bytes — time-ordered, sortable, still
collision-resistant. Brain uses UUIDv7 for many
internal IDs (`MemoryId`, `EntityId`, `StatementId`,
`request_id`). See [Wikipedia: UUID](https://en.wikipedia.org/wiki/Universally_unique_identifier).

---

## V

**Vector.** An ordered list of numbers. In Brain, every
memory carries a 384-element vector (its embedding).
See [chapter 08](08-embeddings.md).

**Vector similarity.** A geometric measure of how close
two vectors are. Brain uses cosine similarity (which
equals dot product for L2-normalised vectors). See
[chapter 09](09-vector-similarity.md).

---

## W

**WAL (Write-Ahead Log).** An append-only file that
records every state-changing operation *before* the
state is changed. The foundation of Brain's durability.
See [chapter 18](18-storage-and-durability.md) and
[Wikipedia: WAL](https://en.wikipedia.org/wiki/Write-ahead_logging).

**Work stealing.** A scheduling pattern where idle
worker threads steal tasks from busy threads' queues.
Tokio uses it; Glommio doesn't. See
[chapter 22](22-concurrency-and-async.md).

---

## See also

- [Chapter 27 — FAQ](27-faq.md) for common questions.
- [Chapter 04 — Brain vs other systems](04-vs-other-systems.md)
  for comparisons.
- [The architecture tier](../architecture/README.md) for
  the deeper implementation reference.
