# The Models Brain Uses

> **What this is:** a catalog of the machine-learning models inside Brain — what each one is, what it does, and the problem it solves. Written at the conceptual level: it names the models and explains their role in the system, not the implementation. For how to run them at scale (CPU → GPU), see [`../guides/deployment/gpu-scaling.md`](../guides/deployment/gpu-scaling.md).

---

## Why a memory database owns models at all

Brain's clients send **text** — "Alice merged the auth-rewrite branch", "Stripe is headquartered in San Francisco". Text on its own is opaque: you can't search it by meaning, you can't connect the people and places it mentions, and you can't tell a durable fact from a passing event. Brain's job is to turn that text into two things:

1. **Something searchable by meaning** — a numeric vector, so "who merged auth?" finds "Alice merged the auth-rewrite branch" even though they share no keywords.
2. **Something structured** — typed entities (Person *Alice*), facts, and relations, so memories become a connected knowledge graph, not a pile of strings.

Neither is possible with string matching. Both require models. So **Brain owns its models** — clients never send vectors or embeddings, only text; Brain controls the models, their versions, and their guarantees end to end. Four models do this work, each solving a distinct problem.

---

## The four models at a glance

| # | Model | Family | Size | Job (one line) | When it runs |
|---|---|---|---|---|---|
| 1 | **BGE-small-en-v1.5** | bi-encoder (sentence embedding) | 384-dim · ~33 M params | text → meaning-vector | every write **and** every query |
| 2 | **bge-reranker-base** | cross-encoder | ~278 M params | score how well a candidate answers a query | every query (always-on) |
| 3 | **GLiNER (small v2.1)** | zero-shot NER tagger | ~140 M params | pull typed entities out of text | every write (background) |
| 4 | **LLM extractor** | large generative model | 7 B–70 B (external/self-hosted) | extract facts & relations that need reasoning | a subset of writes (gated) |

All four are permissively licensed (Apache/MIT-class), which is why they can ship inside the product.

### Where each sits in the pipeline

```
WRITE PATH                                READ PATH
─────────                                 ─────────
text in                                   query text in
  │                                         │
  ├─▶ (1) embedding ──▶ store vector,       ├─▶ (1) embedding ──▶ vector search
  │                     index it                                    + keyword search
  │                                                                 + graph traversal
  └─▶ extraction pipeline (background):              │  (candidate set)
        (pattern rules) ─▶ (2/3) NER ─▶ (4) LLM       ▼
        └─▶ typed entities / facts / relations  (2) rerank ─▶ final ranked answer
```

Models 1 and 2 are on the **latency path** (a user waits for them). Models 3 and 4 run **asynchronously after the write is acknowledged**, enriching the memory into the knowledge graph in the background.

---

## Model 1 — Embedding: **BGE-small-en-v1.5**

**Family:** bi-encoder / sentence-embedding model (a transformer that maps a whole text to one dense vector).
**Shape:** 384-dimensional output vector, ~33 M parameters, English, retrieval-tuned, permissively licensed.

### What it does
It reads a piece of text and emits a single **384-number vector** that encodes the text's *meaning*. Two texts that mean similar things land close together in this 384-dimensional space; unrelated texts land far apart. Closeness is measured by cosine similarity. This is the foundation of semantic search.

### The problem it solves
**Find by meaning, not by keyword.** Keyword search fails the moment the words differ: "who merged auth?" shares no words with "Alice merged the auth-rewrite branch", yet they're about the same thing. The embedding places both near each other in vector space, so the query retrieves the memory. Every memory is embedded once at write time and stored as its vector; every query cue is embedded the same way, and retrieval finds the nearest stored vectors.

### Why this model
- **Small and fast** — at ~33 M parameters it runs on CPU at interactive latency, so Brain has *no hard GPU dependency* to get started.
- **Compact output** — 384 dimensions keep per-vector storage small (the whole index and arena are sized around this), which matters at millions of memories.
- **Strong retrieval quality per megabyte** — among the best small open models for the query-find-passage task it's used for.
- **It is the single most-used model** — it runs on every write and every read, so its speed sets the system's baseline throughput.

### Key property: the vector is bound to the model
A stored vector only means anything *relative to the model that produced it*. So every memory carries a stamp of which embedding model made its vector. Swapping the embedding model invalidates every existing vector and requires re-embedding the whole corpus — embeddings are not portable across models. (This is why an embedding upgrade is a planned migration, not a config change — see the scaling guide.)

---

## Model 2 — Reranker: **bge-reranker-base**

**Family:** cross-encoder (a transformer that reads a *query and a candidate together* and scores their relevance jointly).
**Shape:** ~278 M parameters, multilingual backbone, outputs a single relevance score per pair.

### What it does
Given a query and one candidate memory, it reads **both at once** — attending across the boundary between them — and emits **one number: how well this candidate answers this query**. Brain runs it over the top handful of candidates a query produced and re-sorts them by that score.

### The problem it solves
**Precision at the very top of the results.** The embedding model is fast because it encodes query and candidate *independently* and compares vectors — but that independence costs accuracy: it can't notice subtle query-specific relevance. The result is a good candidate *set* with imperfect *ordering* — the exact answer can land at position 4 instead of 1. The cross-encoder fixes the ordering: by reading query and candidate jointly it judges true relevance and floats the best answer to the top.

### Bi-encoder vs cross-encoder — why both
This is a deliberate two-stage design, standard in modern retrieval:

| | Bi-encoder (Model 1) | Cross-encoder (Model 2) |
|---|---|---|
| Reads | query and candidate **separately** | query and candidate **together** |
| Cost | embed once, compare cheaply against millions | one full pass **per candidate** |
| Strength | fast recall over the whole corpus | precise relevance on a few |
| Used for | **retrieve** a candidate set | **reorder** the top of that set |

You can't afford the cross-encoder over millions of memories (a full transformer pass each). You can't trust the bi-encoder's ordering at the very top. So: bi-encoder retrieves cheaply and widely; cross-encoder reranks precisely and narrowly. Best of both.

### Always-on
Reranking runs on **every** query — there is no opt-in flag. Whenever the model is loaded, the top candidates are reranked; the only control is a deployment-time switch that decides whether the model loads at all (when it doesn't, results fall back to the fused ranking, no error). This is why rerank throughput is the primary driver of inference-tier sizing (see the scaling guide).

---

## Model 3 — Entity extraction: **GLiNER (small v2.1)**

**Family:** zero-shot Named Entity Recognition (NER) tagger, built on a compact DeBERTa-class backbone.
**Shape:** ~140 M parameters, permissively licensed.

### What it does
Given a piece of text **and a list of labels**, it finds the spans of text that are entities of those labels: in "Priya Sharma joined Stripe", with labels *Person* and *Organization*, it tags "Priya Sharma" → Person and "Stripe" → Organization. Brain runs it on every memory after the write is acknowledged, and the entities it finds become nodes in the knowledge graph.

### The problem it solves
**Turn prose into a typed knowledge graph.** A memory is just a sentence until you know *who* and *what* it's about. NER extracts the people, organizations, places, and concepts so memories can be linked — "all memories mentioning Stripe", "everything about Priya Sharma" — and so facts and relations can be attached to real entities rather than raw strings.

### Why *zero-shot* matters most
A conventional NER model is trained on a **fixed** label set (Person, Org, Location…) and can't recognize anything else without retraining. GLiNER is **zero-shot**: the labels are supplied *at inference time*. Brain feeds it **the active schema's entity types** as the label set. The consequence is the whole point:

- Brain ships with a productive default vocabulary (Person, Organization, Place, Event, Concept, …), so extraction works out of the box.
- When a user **expands the schema** with their own entity types, extraction immediately recognizes them — **no retraining, no redeploy.** The model adapts to the schema dynamically.

This is what lets the schema be a first-class, ever-expanding thing while the extractor stays fixed.

### Where it runs
Asynchronously, in the background, after the write is durable. It is *off* the latency path — a slow extraction never slows down a write or a read; it just means the entity appears in the graph a moment later.

---

## Model 4 — Deep extraction: the **LLM extractor**

**Family:** a large general-purpose generative language model. Accessed via an external API by default; self-hostable on a GPU pool (see the scaling guide).

### What it does
It reads a memory and extracts **structured knowledge that requires reasoning**: factual statements ("X works at Y"), preferences ("the user prefers dark mode"), events, and **relations between entities** — the connections NER alone can't infer. Its output, like NER's, lands in the typed knowledge graph.

### The problem it solves
**The structure that needs understanding, not just pattern-matching.** Recognizing that "Stripe" is an Organization is tagging (NER's job). Recognizing that "Priya **joined** Stripe **as a Senior Engineer**" asserts an *employment relation with a role* requires reading comprehension. The LLM extracts these higher-order facts and relationships that rules and taggers miss.

### Why it's gated and cached
It is by far the most expensive and slowest of the four, and the only one that can incur an external cost. So it is **best-effort and budget-gated** — it runs on a subset of writes, not all — and its results are **cached** so the same text is never extracted twice. It sits at the top of an escalation ladder (next section): the cheap tiers handle what they can, and only what's left escalates to the LLM.

---

## How the extraction models compose: the three-tier pipeline

Models 3 and 4 don't run in isolation. Extraction is a **three-tier escalation**, cheapest first:

| Tier | Mechanism | Cost | Catches |
|---|---|---|---|
| **1 — Pattern** | deterministic rules / patterns | ~free | structured, predictable mentions |
| **2 — Classifier** | GLiNER zero-shot NER (Model 3) | cheap, local | typed entities from open text |
| **3 — LLM** | the generative model (Model 4) | expensive, gated | facts & relations needing reasoning |

The design principle is **escalate only when needed**: run the cheap, deterministic tier first; fall to the local NER model for entities; reach for the expensive LLM only for what the lower tiers can't get. Each tier can be enabled or disabled independently at deployment. Everything they extract is **schema-gated** — only entities and relations whose types exist in an active schema namespace are persisted; the rest is dropped (best-effort). This keeps the graph clean and bounds what the models are allowed to write.

---

## How retrieval composes the models: recall then precision

On the read side, Models 1 and 2 bracket a fan-out:

1. **Recall (wide, cheap).** The query is embedded (Model 1) and used for vector similarity search; in parallel, keyword search and graph traversal run. Their ranked results are **fused** into one candidate set. This stage is tuned to *not miss* the right answer — high recall.
2. **Precision (narrow, exact).** The cross-encoder (Model 2) rescores the top of that fused set by true query-relevance and reorders it. This stage is tuned to *get the order right* at the very top — high precision.

Recall casts a wide net cheaply; precision sharpens the top exactly. Neither model could do the whole job alone.

---

## Why these specific models (the selection criteria)

The four were chosen against a consistent set of constraints:

- **Permissive licensing.** All are Apache/MIT-class, so they can be embedded and shipped without legal encumbrance.
- **CPU-feasible to start.** The two on-path models (embedding, rerank) and the NER model are small enough to run on CPU at usable latency, so a deployment needs **no GPU to begin** — GPU is an optimization, not a prerequisite.
- **Quality per parameter.** Each is near the top of its class for its size, maximizing accuracy per unit of compute and memory.
- **The right tool per job.** A fast bi-encoder for recall, a precise cross-encoder for reranking, a *zero-shot* tagger so the schema can grow without retraining, and an LLM reserved for the reasoning-heavy extraction — each model is matched to a problem the others can't solve.
- **Compact representations.** The 384-dimensional embedding keeps storage and index footprints small at scale.

---

## What each model does *not* do (boundaries)

Knowing the edges prevents misuse:

- **The embedding model does not rank precisely.** It gives a good candidate set, not the final order — that's the reranker's job.
- **The reranker does not retrieve.** It only reorders an existing candidate set; it never scans the corpus (far too expensive).
- **The NER model does not reason about relationships.** It tags entity spans; inferring "who did what to whom" is the LLM's job.
- **The LLM extractor is best-effort, not guaranteed.** It is gated and may not run on a given write; the graph is enriched opportunistically, and the lower tiers provide the floor.
- **None of them store or rank by importance over time** — salience, decay, and consolidation are separate, non-model mechanisms.

---

## Upgrading the models

All four can be swapped for larger, higher-quality variants when a GPU tier is available — a bigger embedding model (with the storage caveat that vector dimensionality is tied to the on-disk record layout), a larger or multilingual reranker, a bigger NER tagger, and a self-hosted LLM in place of the external API. The selection above is the **CPU-first default**; the upgrade paths, their hardware needs, and the migration steps (especially the re-embed required by any embedding-model change) are detailed in [`../guides/deployment/gpu-scaling.md`](../guides/deployment/gpu-scaling.md).
