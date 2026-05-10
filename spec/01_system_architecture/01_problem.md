# 01.01 The Problem We're Solving

Before defining a system, we should be clear about what's broken in the absence of it. This file makes the case for why a cognitive substrate is the right shape of solution, distinct from existing storage systems.

## 1. Agents are stateless; cognition is not

Modern AI agents are built around large language models (LLMs). The model itself is stateless: each invocation is a pure function from input tokens to output tokens. Whatever the agent "remembers" is whatever the application chooses to put back into the context window on the next call.

This works for short tasks. For anything that spans hours, days, or longer — a coding assistant tracking a project, a customer-support agent maintaining a relationship, a research assistant accumulating findings — the application needs persistent state. The state has to support several different kinds of access:

- **Recall by similarity** — "what did the user say earlier that's relevant to this question?"
- **Recall by reference** — "what did we decide on Tuesday?"
- **Planning over remembered structure** — "given what I know, what's the path from here to the goal?"
- **Reasoning over relations** — "why did this happen? what depends on what?"
- **Forgetting** — graceful decay of unimportant memories, hard erasure of sensitive ones.

No single existing storage system handles all of these natively. Today's agent stacks duct-tape three or four together: a vector database for similarity search, a relational database for structured facts, a graph database for relationships, a document store for raw events, plus application code that orchestrates them. Each integration is custom, each consistency boundary is hand-managed, each performance budget is a guess.

## 2. The integration tax

The cost of this duct tape is mostly invisible until production. The actually-encountered problems include:

### 2.1 Latency stacking

A single agent turn might query the vector store, then the SQL store, then a graph store, then the LLM. Each round trip is 1–10 ms in the best case; the latency budget for "feels responsive" is 100–300 ms total. Half of that is consumed before the LLM even starts. An agent that needs to consult its memory five times per turn (each a separate vector + metadata join) will spend more time waiting for stores than thinking.

### 2.2 Consistency across stores

A new memory needs to be written to the vector index, the metadata table, and the graph edges atomically. Without distributed transactions across heterogeneous stores, you get races: a reader sees the metadata but not the vector, or vice versa, and the agent behaves inconsistently. The application code grows compensating logic — quorum reads, retry loops, idempotency keys, soft-deletion patterns — that should be the substrate's responsibility.

### 2.3 Embedding lifecycle

Vectors are only valid relative to the embedding model that produced them. When the model is upgraded, every vector becomes incomparable to new ones. Most systems handle this by ignoring it: the application keeps using mismatched vectors and tolerates the quality drop, or freezes on an old model long past its sell-by date. A correct migration requires re-embedding all stored content, which the substrate is in the best position to manage but typically doesn't.

### 2.4 Working-set fragmentation

Hot memories belong in RAM; cold memories belong on disk. Each store has its own caching and tiering logic, and they don't coordinate. The system as a whole has worse cache utilization than any single component would alone — pages that should evict together stay resident in one store and cold in another, depending on whose access patterns dominate.

### 2.5 Operational complexity

Each store has its own backup, monitoring, replication, scaling, and failure-mode story. An agent application now requires expertise in four different operational disciplines. Recovery from a partial failure (one store up, another down) is an unsolved problem in most deployments — the application either tolerates inconsistency or refuses requests entirely, neither of which is satisfying.

### 2.6 Cognitive operations don't compose from CRUD

`PLAN` is not a function of CRUD operations on a vector store. Neither is `REASON`. These cognitive operations want native support: they need access to the index structure, the graph, the salience scores, and the working set, all together, in tight loops without round-trips. Implementing them as application code on top of vector-search-and-graph-walk APIs results in slow, brittle approximations.

## 3. The pitch: "SQL for cognition"

The right analogy is the one Codd made in 1970 when he proposed the relational model. Applications were each implementing their own ad-hoc record management, and they were all making the same mistakes in slightly different ways. The relational model gave them a uniform, declarative language ("describe what you want, not how to fetch it") and a uniform storage substrate.

We propose the same separation for cognition. Agents should describe what they want from their memory in cognitive terms — *recall things similar to this cue*, *plan a path from this state to that goal*, *reason about why this happened* — and the substrate should decide how to satisfy the request. Storage layout, indexing strategy, recall algorithm, working-set management, embedding lifecycle: all become the substrate's problem, not the agent's.

This is a significant abstraction shift. SQL took a decade to be widely adopted because the abstraction was unfamiliar; the same is likely true here. But once the substrate exists, agent applications become dramatically simpler — they hand text to the substrate, ask cognitive questions, and get answers.

## 4. Why a new system, not a feature added to an existing one

A reasonable objection: "vector databases are evolving toward this. Why not extend [Qdrant](https://github.com/qdrant/qdrant), [Milvus](https://github.com/milvus-io/milvus), or [Weaviate](https://github.com/weaviate/weaviate)?"

These systems are excellent at vector similarity search — and that's what they're optimized for. Adding cognitive operations on top of a vector-search-first architecture means the cognitive layer is always a guest in someone else's house. Concretely:

### 4.1 The query model is wrong

Vector databases expose `search(vector, k, filter)`. The cognitive layer needs `recall(cue, context, confidence_threshold)`, `plan(start, goal)`, `reason(observation)`. These aren't translatable to vector-search-plus-filter without losing structure. A `RECALL` with confidence calibration depends on knowing the salience distribution and the embedding model's calibration — internal facts the vector database doesn't expose.

### 4.2 The write model is wrong

Vector databases assume the application has already produced vectors. We argue elsewhere ([04. Embedding Layer](../04_embedding_layer/)) that the substrate should own embedding — text in, vectors hidden as an implementation detail. Owning embedding is what enables features like deduplication by semantic content, automatic re-embedding on model upgrade, and native handling of caching keyed on text.

### 4.3 The consistency model is wrong

Vector databases treat metadata as a sidecar. We need metadata, graph edges, and vectors to be co-managed under one consistent transaction model. A memory's vector, its salience, its context, and its outgoing edges should be written and read atomically; otherwise the agent observes incoherent intermediate states.

### 4.4 The performance model is wrong

Vector databases optimize for batch ingestion and analytical queries: build a corpus, search it, occasionally refresh. Agents do single-item writes and single-cue reads on the hot path, hundreds of times per second per agent. Different optimization target, different working-set characteristics, different durability requirements.

### 4.5 The conclusion

A vector database can absolutely be a *component* underneath a cognitive substrate. But the substrate has to be its own thing, with its own opinions, designed for its own workload. That's what Brain is.

## 5. Why not just give the LLM more context?

Another reasonable objection: "context windows are getting bigger. Why not just dump everything into context?"

We address this in detail in [`02_background.md`](02_background.md) §1. The short answer is that long-context LLMs have four limits that don't go away with more capacity:

1. **Cost** scales linearly (and quadratically in the attention mechanism) with context size.
2. **Latency** scales similarly; long prefills add seconds before any token is generated.
3. **Attention degradation at length** is a measured phenomenon — long-context models perform worse at retrieving information from the middle of long inputs.
4. **No structure** — even a perfectly-recalling long-context model can't answer "find me the memory most causally upstream of this observation," because that's a graph-traversal question, not a token-attention question.

Brain inverts the trade-off: keep the context window small (a few thousand tokens of *relevant* memories selected by the substrate) and let the substrate carry unlimited structured state outside the model.

## 6. Summary

The reasons for Brain to exist as a new system, in one paragraph: AI agents need persistent, structured, cognitively-typed memory. Existing storage systems handle one or two facets of this poorly and require the agent application to assemble the rest at high cost and risk. A purpose-built substrate can subsume the assembly, expose cognitive operations natively, own the embedding lifecycle, and run within the latency budget agents can afford — at the cost of being a new system to learn, deploy, and operate.

The remainder of this architecture document defines what that substrate looks like.

---

*Continue to [`02_background.md`](02_background.md) for prerequisite concepts.*
