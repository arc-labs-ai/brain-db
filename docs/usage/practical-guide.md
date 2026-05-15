# Brain — Practical Usage Guide

> How end users actually use Brain through the SDK. A complete walkthrough from "I just got the binary" to "I'm running this in production."

This guide is task-oriented. The spec tells you *what* Brain is. This tells you *what you do with it*.

---

## The scenario

You work at a 30-person startup called **Acme**. The engineering team has tribal knowledge scattered across Slack threads, Notion docs, meeting notes, and people's heads. Onboarding takes weeks because new hires can't find anything. Decisions get re-litigated because nobody remembers why we picked Postgres over Mongo two years ago.

You want to build an AI assistant — call it **Mind** — that ingests everything the team writes (notes, meeting transcripts, Slack messages they opt to share, Linear tickets, design docs) and lets anyone ask:

- "Why did we pick Postgres?"
- "What's Priya working on right now?"
- "How does Bob prefer to do code reviews?"
- "Who's been involved with the billing rewrite?"
- "What did we decide about the K8s migration last quarter?"

You picked Brain because:
- Vector search alone doesn't cut it (need keyword matching for ticket IDs like `ACME-1247`, need to track entities as they get renamed, need graph queries like "everyone on Priya's team").
- You don't want to glue together 5 services (Elasticsearch + Postgres + Neo4j + an LLM extraction pipeline + an embedding service).
- You want this on one box, runnable in a coffee shop on a laptop for development.

Let's build it.

---

## Day 1: Get Brain running

```bash
# Install the binary
curl -sSf https://brain.example/install.sh | sh
# (or: cargo install brain-server)

# Start a server
brain-server start \
  --data-dir ~/acme-mind/data \
  --listen 127.0.0.1:7860 \
  --shards 4

# Server logs:
# [INFO] Opened 4 shards at ~/acme-mind/data
# [INFO] Brain 1.0.0 listening on 127.0.0.1:7860
# [INFO] No schema declared; running substrate-only
```

Add the SDK to your Rust project:

```toml
# Cargo.toml
[dependencies]
brain-sdk-rust = "2.0"
tokio = { version = "1", features = ["full"] }
```

A "hello world" — write a memory, recall it:

```rust
use brain_sdk::Client;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let brain = Client::connect("127.0.0.1:7860", "mind-agent-1").await?;
    
    // Write
    let mem = brain.encode("Priya kicked off the billing rewrite project today.").await?;
    println!("Stored memory {}", mem.id);
    
    // Read
    let hits = brain.recall("billing project").limit(5).await?;
    for hit in hits {
        println!("- {} (score {:.2})", hit.text(), hit.score);
    }
    Ok(())
}
```

Output:

```
Stored memory MEM-01HW8T3P9...
- Priya kicked off the billing rewrite project today. (score 0.87)
```

Thats it. You used the substrate-only surface and it works without any schema. Brain is now a vector memory store.

But you havent touched the knowledge layer yet. Vector recall on a single memory is not impressive. Let's go further.

---

## Day 2: Declare your schema

This is the moment Brain becomes more than vector search. You tell Brain what kinds of things exist in your world and how to recognize them.

Create `acme-schema.brain`:

```
# Acme engineering mind — schema version 1
namespace acme

# ─── Entity types ────────────────────────────────────────────

define entity_type Person {
    attributes {
        email:    text optional unique
        role:     text optional
        team:     text optional
        timezone: text optional
    }
}

define entity_type Project {
    attributes {
        slug:     text required unique
        status:   enum[planning, active, paused, done] default planning
        repo_url: text optional
    }
}

define entity_type Ticket {
    attributes {
        key:      text required unique     # e.g. "ACME-1247"
        status:   enum[open, in_progress, blocked, closed] optional
    }
}

define entity_type Decision {
    attributes {
        slug:     text required unique
        area:     text optional             # "infrastructure", "billing", ...
    }
}

# ─── Predicates ──────────────────────────────────────────────

define predicate role {
    kind: Fact
    object: Value<text>
}

define predicate prefers {
    kind: Preference
    object: Value<text>
}

define predicate said {
    kind: Event
    object: Value<text>
}

define predicate decided {
    kind: Fact
    object: Value<text>
}

# ─── Relations ───────────────────────────────────────────────

define relation_type works_on {
    from: Person
    to: Project
    cardinality: many-to-many
    properties {
        since: date optional
    }
}

define relation_type reports_to {
    from: Person
    to: Person
    cardinality: many-to-one
}

define relation_type assigned_to {
    from: Ticket
    to: Person
    cardinality: many-to-one
}

define relation_type owns {
    from: Person
    to: Project
    cardinality: many-to-many
}

# ─── Extractors ──────────────────────────────────────────────

use brain.entity_mentions       # built-in pattern + NER

define extractor ticket_ids {
    kind: pattern
    target: entity Ticket
    patterns [
        /\b(ACME-\d+)\b/
    ]
    confidence: 0.99
}

define extractor preferences {
    kind: llm
    target: statement Preference
    model: "claude-haiku-4-5"
    prompt: """
        From the memory below, extract any preferences a person has expressed
        about how they work. Return JSON array. Empty array if none.
        
        Each item: {"subject": "<person name>", "object": "<preference>", "confidence": 0-1}
        
        Memory: {{memory.text}}
    """
    schema: {
        type: array
        items: {
            type: object
            required: [subject, object, confidence]
            properties: {
                subject:    { type: string }
                object:     { type: string }
                confidence: { type: number, minimum: 0, maximum: 1 }
            }
        }
    }
    cache: enabled
    cost_budget: "$0.001 per memory"
    confidence_threshold: 0.7
    trigger: on encode where memory.kind = episodic
}

define extractor decisions {
    kind: llm
    target: statement Fact
    model: "claude-haiku-4-5"
    prompt: """
        Did this memory record a team decision? If yes, extract:
        {"subject": "<decision slug, e.g. database_choice>",
         "predicate": "decided",
         "object": "<what was decided, one sentence>",
         "confidence": 0-1}
        
        Return JSON array (one item if decision found, empty if not).
        
        Memory: {{memory.text}}
    """
    schema: { /* ... */ }
    cache: enabled
    confidence_threshold: 0.75
    trigger: on encode where memory.text matches ".*(decided|chose|picked|going with).*"
}
```

Upload it:

```rust
let schema_text = std::fs::read_to_string("acme-schema.brain")?;
let result = brain.schema().upload_text(&schema_text).await?;
println!("Schema v{} accepted", result.version);
```

Output:

```
Schema version 1 accepted
```

What just happened on the server side:

```
[INFO] Schema upload: 4 entity types, 4 predicates, 4 relation types, 3 extractors
[INFO] Schema validation passed
[INFO] Schema version 1 active
[INFO] Activating extractors: ticket_ids, preferences, decisions
[INFO] Built-in extractor brain.entity_mentions activated
[INFO] Extraction now active on new memories
```

You can also build the schema programmatically with the SDK — useful when your schema is computed from config:

```rust
use brain_sdk::knowledge::SchemaBuilder;

#[derive(BrainEntity)]
#[brain(entity_type = "Person", namespace = "acme")]
struct Person {
    #[brain(optional, unique)]
    email: Option<String>,
    role: Option<String>,
    team: Option<String>,
}

#[derive(BrainEntity)]
#[brain(entity_type = "Project", namespace = "acme")]
struct Project {
    #[brain(required, unique)]
    slug: String,
    status: Option<String>,
}

#[derive(BrainRelation)]
#[brain(from = "Person", to = "Project", cardinality = "many-to-many")]
struct WorksOn;

let schema = SchemaBuilder::new("acme")
    .entity_type::<Person>()
    .entity_type::<Project>()
    .relation_type::<WorksOn>()
    // ... predicates and extractors
    .build()?;

brain.schema().upload(&schema).await?;
```

Same result; just authored in Rust instead of the DSL.

---

## Day 3: Start ingesting

Now you wire up your ingestion. Slack messages, meeting notes, Linear comments — whatever the team produces. Each becomes a memory.

```rust
async fn ingest_slack_message(brain: &Client, msg: SlackMessage) -> anyhow::Result<()> {
    brain.encode(&msg.text)
        .kind(MemoryKind::Episodic)
        .source(&format!("slack:{}", msg.channel))
        .author(&msg.user)
        .at(msg.timestamp)
        .commit()
        .await?;
    Ok(())
}
```

Watch what happens when you ingest one:

```rust
ingest_slack_message(&brain, SlackMessage {
    text: "Priya: Let's just go with Postgres for billing. Bob prefers Mongo \
           but we already have ops experience with PG. ACME-1247 captures the decision.",
    channel: "eng-billing",
    user: "U-PRIYA",
    timestamp: now(),
}).await?;
```

You see in the server logs:

```
[INFO] ENCODE memory MEM-01J... (132 bytes, agent mind-agent-1)
[INFO] pattern_extractor brain.entity_mentions: 2 candidates (Priya, Bob)
[INFO] resolver tier1: Priya -> ENT-PERSON-PRIYA (existing)
[INFO] resolver tier1: Bob -> ENT-PERSON-BOB (existing)
[INFO] pattern_extractor ticket_ids: 1 candidate (ACME-1247)
[INFO] resolver tier5: ACME-1247 -> new Ticket entity ENT-TICKET-ACME-1247
[INFO] LLM extractor preferences queued (background)
[INFO] LLM extractor decisions queued (matched trigger)
```

A few seconds later (LLM extractors are async background):

```
[INFO] preferences extracted 1 statement:
       (ENT-PERSON-BOB, prefers, "Mongo", conf=0.83)
[INFO] decisions extracted 1 statement:
       (database_choice_billing, decided, "Postgres for billing", conf=0.91)
[INFO] decisions resolved subject database_choice_billing -> new Decision entity
```

Brain just did, automatically:

1. Wrote the raw memory (substrate behavior).
2. Found two known people (Priya, Bob) and resolved them via exact match.
3. Found a ticket ID via regex pattern, created a new Ticket entity.
4. Called the LLM extractor for preferences — found Bob's Mongo preference.
5. Called the LLM extractor for decisions — found the Postgres decision.
6. Linked statements back to the source memory as evidence.

You didn't write any extraction code. You declared the schema; Brain did the rest.

### What if Priya and Bob didn't exist yet?

First time the team's name comes up, Brain creates them. You can also seed entities up front from your HR database:

```rust
let priya = brain.entity::<Person>()
    .canonical_name("Priya Patel")
    .alias("Priya")
    .alias("priya@acme.com")
    .with(|p| {
        p.email = Some("priya@acme.com".into());
        p.role = Some("Engineering Manager".into());
        p.team = Some("Platform".into());
    })
    .create()
    .await?;
```

Now when memories mention "Priya" or "priya@acme.com" or even "Priya Patel," they resolve to the same entity.

### What about typos and nicknames?

Six months in, someone writes "Priyaa kicked off the migration." The pattern extractor finds "Priyaa" as a person candidate. The resolver runs:

1. Tier 1 (exact): no match for "Priyaa."
2. Tier 2 (trigram fuzzy): "Priyaa" is 89% similar to "Priya Patel." Above threshold (0.85). 
3. Resolved to ENT-PERSON-PRIYA with confidence 0.89.

If the resolver weren't confident enough, it would have:
- Run tier 3: embedded "Priyaa" + context, found nearest entity.
- Run tier 4 (if you enabled LLM resolution): asked an LLM to disambiguate.
- Otherwise: created a new entity, then later a sweeper or operator could merge them.

### What if two people are both named Sarah?

Now the resolver might find multiple candidates above threshold. Instead of guessing, Brain writes the statement with a pending subject and audits the ambiguity:

```rust
// You can inspect:
let pending = brain.admin().list_pending_resolutions().await?;
for p in pending {
    println!("Candidate '{}' in context '{}': {} matches",
             p.candidate, p.context_snippet, p.candidates.len());
    for (entity_id, score) in p.candidates {
        let e = brain.entity_get(entity_id).await?;
        println!("  - {} ({}): score {:.2}", e.canonical_name, e.id, score);
    }
}

// Resolve manually:
brain.admin().resolve_ambiguity(p.audit_id, chosen_entity_id).await?;
```

---

## Day 4: Query the substrate

Now the team starts asking questions through Mind.

### Question 1: "Why did we pick Postgres?"

```rust
let answer = brain.query()
    .text("why did we pick Postgres")
    .limit(5)
    .execute()
    .await?;

for item in answer.items {
    println!("{}", render(item));
}
```

The query router classifies this:
- Has text → semantic + lexical retrievers.
- "Postgres" is a proper noun → lexical weighted up.
- No entity anchor → graph retriever not invoked.

Behind the scenes:
- **SemanticRetriever** finds memories and statements semantically close to "pick Postgres" / "choose database" / "database decision."
- **LexicalRetriever** finds occurrences of exact term "Postgres" via tantivy BM25.
- **RRF Fusion** combines the two ranked lists.

Result:

```
1. Statement(Fact): decision database_choice_billing decided "Postgres for billing"
   Evidence: MEM-01J... (the Slack message from Priya)
   Confidence: 0.91
   Contributing retrievers: semantic (rank 3), lexical (rank 1)

2. Memory: "Priya: Let's just go with Postgres for billing. Bob prefers Mongo
   but we already have ops experience with PG. ACME-1247 captures the decision."
   Contributing retrievers: semantic (rank 1), lexical (rank 2)

3. Memory: "Discussed Mongo vs Postgres in eng review — leaning Postgres
   given our existing tooling."
   Contributing retrievers: semantic (rank 2)
```

Notice: Mind didn't just return the raw memory — it returned the **derived Fact** at rank 1, because the decision extractor surfaced it as a first-class statement. The user gets the structured answer plus the evidence.

### Question 2: "What's Priya working on?"

```rust
let priya = brain.entity::<Person>()
    .resolve("Priya")
    .await?
    .expect_resolved()?;

let projects = brain.relation::<WorksOn>()
    .traverse_from(priya.entity_id)
    .depth(1)
    .current_only()
    .execute()
    .await?;

for edge in projects {
    let proj = brain.entity_get::<Project>(edge.to).await?;
    println!("- {} ({})", proj.canonical_name, edge.properties.get("since"));
}
```

Output:

```
- Billing rewrite (since 2026-03-15)
- Platform telemetry (since 2026-01-10)
```

This is the graph lane. The query router would have done it automatically if you'd asked in natural language:

```rust
let answer = brain.query()
    .text("what is Priya working on")
    .execute()
    .await?;
```

The router sees "Priya" → NER resolves to ENT-PERSON-PRIYA → graph retriever invoked with anchor=priya, edges=works_on, depth=1.

### Question 3: "How does Bob prefer to do code reviews?"

```rust
let bob = brain.entity::<Person>().resolve("Bob").await?.expect_resolved()?;

let prefs = brain.statements()
    .where_subject(bob.entity_id)
    .of_kind(StatementKind::Preference)
    .current_only()
    .with_min_confidence(0.6)
    .list()
    .await?;

for p in prefs {
    println!("Bob prefers {}: {} (conf {:.2})",
             p.predicate, p.object_text(), p.confidence);
    if let Some(mem_id) = p.evidence.first() {
        let m = brain.memory_get(*mem_id).await?;
        println!("  Source: \"{}\"", m.text);
    }
}
```

Output:

```
Bob prefers reviews: "small PRs over big ones" (conf 0.88)
  Source: "Bob: please keep PRs under 400 lines, my reviews suffer past that"
Bob prefers reviews: "async over synchronous" (conf 0.81)
  Source: "I'd rather get review comments async than do live walkthroughs - Bob"
Bob prefers stack: "Mongo" (conf 0.83)
  Source: "Priya: Let's just go with Postgres for billing. Bob prefers Mongo..."
```

Notice the last one is from the *same* memory as the Postgres decision. Same memory, multiple extracted statements, each independently queryable.

### Question 4: "What did Priya say last week about the billing project?"

This combines: entity anchor + temporal filter + project context.

```rust
let priya = brain.entity::<Person>().resolve("Priya").await?.expect_resolved()?;
let billing = brain.entity::<Project>().resolve_by_slug("billing-rewrite").await?;

let answer = brain.query()
    .text("billing project")
    .with_entity(priya.entity_id)
    .where_time(TimeRange::last(Duration::days(7)))
    .limit(20)
    .execute()
    .await?;
```

The router engages:
- Entity anchor (Priya) → graph retriever.
- Time filter → temporal filter pushed down into retrievers.
- Text → semantic + lexical.

Three retrievers, one temporal filter, RRF fusion. Result is memories and statements about Priya in the last 7 days, sorted by relevance to "billing project."

### Question 5: "Who's involved in the billing rewrite?"

Pure graph query, no text:

```rust
let billing = brain.entity::<Project>().resolve_by_slug("billing-rewrite").await?;

let people = brain.relation::<WorksOn>()
    .traverse_to(billing.entity_id)        // reverse direction
    .depth(1)
    .current_only()
    .execute()
    .await?;

for edge in people {
    let p = brain.entity_get::<Person>(edge.from).await?;
    println!("- {} ({})", p.canonical_name, p.attributes.get("role"));
}
```

Two-hop variant — "everyone on the team of anyone working on billing":

```rust
let answer = brain.query()
    .with_entity(billing.entity_id)
    .traverse(TraversalSpec {
        edges: vec!["works_on".into(), "reports_to".into()],
        depth: 2,
        direction: Direction::BothWays,
    })
    .execute()
    .await?;
```

### Question 6: "Show me the timeline for the K8s migration"

Event queries are time-ordered:

```rust
let k8s = brain.entity::<Project>().resolve_by_slug("k8s-migration").await?;

let events = brain.statements()
    .where_subject(k8s.entity_id)
    .of_kind(StatementKind::Event)
    .order_by_event_time(Order::Asc)
    .list()
    .await?;

for e in events {
    println!("{}: {}", format_date(e.event_at), e.object_text());
}
```

Output:

```
2026-01-15: kickoff meeting scheduled
2026-02-03: pilot cluster provisioned
2026-02-20: staging migration completed
2026-03-10: prod migration paused due to ACME-1402
2026-04-02: prod migration resumed
2026-04-18: migration completed
```

### Debugging a query

When the result is surprising, use `.trace()`:

```rust
let traced = brain.query()
    .text("Priya leadership preferences")
    .trace()
    .await?;

println!("{}", traced.plan_summary);
for r in traced.retriever_traces {
    println!("\n{} ({:.1} ms, {} results):", r.name, r.latency_ms, r.total);
    for (rank, item) in r.top_3.iter().enumerate() {
        println!("  rank {}: {} (score {:.2})", rank + 1, item.summary, item.score);
    }
}
println!("\nFinal top 3:");
for (rank, item) in traced.items.iter().take(3).enumerate() {
    println!("  rank {}: {}", rank + 1, item.summary);
    for c in &item.contributing_retrievers {
        println!("    via {} (rank {}, raw score {:.2})", c.retriever, c.rank, c.raw_score);
    }
}
```

Output:

```
PLAN: entity-anchored (Priya), text-bearing
  RETRIEVERS: Semantic(w=1.0), Lexical(w=0.5), Graph(w=2.0, anchor=Priya)
  FUSION: RRF(k=60)

semantic (4.2 ms, 87 results):
  rank 1: Preference: Priya prefers "1:1s in the morning" (score 0.81)
  rank 2: Preference: Priya prefers "written design docs" (score 0.78)
  rank 3: Memory: "Priya emphasized async leadership in standups..." (score 0.75)

lexical (2.1 ms, 12 results):
  rank 1: Memory: "leadership offsite agenda from Priya" (score 8.4)
  rank 2: Memory: "Priya's leadership style was discussed..." (score 6.9)
  rank 3: Preference: Priya prefers "written design docs" (score 4.1)

graph (1.8 ms, 23 results):
  rank 1: Preference: Priya prefers "1:1s in the morning" (score 1.0)
  rank 2: Preference: Priya prefers "written design docs" (score 1.0)
  rank 3: Preference: Priya prefers "async feedback over live" (score 1.0)

Final top 3:
  rank 1: Preference: Priya prefers "written design docs"
    via semantic (rank 2, raw score 0.78)
    via lexical (rank 3, raw score 4.10)
    via graph (rank 2, raw score 1.00)
  rank 2: Preference: Priya prefers "1:1s in the morning"
    via semantic (rank 1, raw score 0.81)
    via graph (rank 1, raw score 1.00)
  rank 3: Memory: "leadership offsite agenda from Priya"
    via lexical (rank 1, raw score 8.40)
```

You see which retrievers contributed to each result and at what rank. When something's off, you know whether to tune retriever weights, fix the schema, or add more evidence.

---

## Day 5: Knowledge evolves

Real knowledge isn't static. People change roles, preferences shift, facts get corrected. Brain handles this.

### Preferences update

Three months in, the team rebuilt their CI and Bob says: "Actually I changed my mind on PR size. With the new fast CI, bigger batches are fine."

You ingest the message; the extractor fires; a new Preference is created. Because it's a Preference with same `(subject="Bob", predicate="prefers")` as before, the old one is **superseded**, not replaced:

```rust
let prefs = brain.statements()
    .where_subject(bob.entity_id)
    .of_kind(StatementKind::Preference)
    .current_only()
    .list()
    .await?;
// Returns the new preference only.

// Want the history?
let all = brain.statements()
    .where_subject(bob.entity_id)
    .of_kind(StatementKind::Preference)
    .include_superseded(true)
    .list()
    .await?;

for p in all {
    if p.is_current() {
        println!("Now: {} (since {})", p.object_text(), format_date(p.valid_from));
    } else {
        println!("Was: {} ({} - {})",
                 p.object_text(),
                 format_date(p.valid_from),
                 format_date(p.valid_to.unwrap()));
    }
}
```

Output:

```
Now: "smaller PRs no longer a strict requirement after CI rebuild" (since 2026-08-12)
Was: "small PRs over big ones" (2026-01-10 - 2026-08-12)
```

The history is intact. The default view shows current. You explicitly opt in to see the past.

### Contradicting Facts

Mind ingests two memories:

```
"Priya is the engineering manager of the Platform team."
"Priya is now leading Infrastructure, not Platform anymore."
```

Both produce Facts with same `(subject=Priya, predicate=role)` but different objects. Brain stores both — they're contradictions, not supersessions (Facts don't auto-supersede). The query surfaces them:

```rust
let role = brain.statements()
    .where_subject(priya.entity_id)
    .where_predicate("role")
    .of_kind(StatementKind::Fact)
    .current_only()
    .list()
    .await?;

if role.len() > 1 {
    println!("⚠️  {} contradicting Facts:", role.len());
    for f in &role {
        println!("  - \"{}\" (conf {:.2}, evidence: {} memories)",
                 f.object_text(), f.confidence, f.evidence.len());
    }
}
```

Output:

```
⚠️  2 contradicting Facts:
  - "engineering manager of Platform" (conf 0.91, evidence: 3 memories)
  - "leading Infrastructure" (conf 0.94, evidence: 1 memory)
```

What does Mind do? Up to you. You can:
- Show both to the user and let them pick.
- Pick the higher-confidence one and disclose.
- Pick the more recent one (and disclose).
- Resolve by writing a Fact explicitly retracting one.

Brain refuses to silently pick. That's intentional — the second a cognitive substrate hides contradictions, you can't trust it.

To resolve explicitly:

```rust
brain.fact()
    .subject(priya.entity_id)
    .predicate("role")
    .object_value("VP of Infrastructure")          // the correct current role
    .evidence(vec![latest_memory_id])
    .confidence(0.98)
    .supersedes_facts(&[old_fact_id_1, old_fact_id_2])
    .create()
    .await?;
```

Now the previous Facts are explicitly marked superseded by this one. Current-only queries return the new Fact alone.

### Forgetting a memory cascades

Someone realizes a memory contains private information that shouldn't be in the substrate:

```rust
brain.forget(memory_id).hard().reason("PII").await?;
```

In the background, the **FORGET cascade worker** runs:

```
[INFO] FORGET memory MEM-01J... (hard)
[INFO] Cascade: 3 statements have this memory in evidence
       - STMT-XX confidence: 0.91 -> 0.82 (recomputed, 2 evidence remain)
       - STMT-YY confidence: 0.87 -> 0.81 (recomputed, 1 evidence remain)
       - STMT-ZZ confidence: 0.74 -> orphan, tombstoned (reason: SourceMemoryForgotten)
[INFO] Cascade: 1 relation depends on this memory
       - REL-AA confidence: 0.83 (1 evidence remain)
[INFO] Cascade complete
```

The memory is gone. Statements derived from it have their confidence recomputed; one lost its only evidence and got tombstoned. The audit log records the cascade.

You don't write any of this code. Brain owns the data integrity.

### Renaming an entity

Priya gets married, becomes Priya Singh:

```rust
brain.entity(priya.entity_id)
    .rename("Priya Singh")
    .keep_old_name_as_alias()
    .await?;
```

Aftermath:
- EntityId unchanged.
- All Statements and Relations still point to the same EntityId — they automatically reflect the new name.
- "Priya" remains in aliases, so old text mentioning "Priya" still resolves correctly.
- Future ingestion mentioning "Priya Singh" resolves to the same entity.

Compare to systems where renaming means migrating thousands of records. Here, you change one row.

### Merging duplicate entities

Six months in, you discover the team has two entities for the same person — "Bob Chen" and "Bob C." — because they were created from different memories before aliases were set up.

```rust
let dups = brain.admin()
    .find_potential_duplicates::<Person>()
    .min_confidence(0.85)
    .await?;

for d in dups {
    println!("Possible duplicates ({:.2}): {} <-> {}",
             d.confidence, d.entity_a.canonical_name, d.entity_b.canonical_name);
}

// Or merge directly when you're confident:
brain.entity_merge()
    .survivor(bob_chen_id)
    .merged(bob_c_id)
    .confidence(0.95)
    .commit()
    .await?;
```

After merge:
- All Statements and Relations pointing to bob_c_id now point to bob_chen_id.
- bob_c_id's row stays as a redirect.
- Within a 7-day grace period, you can unmerge if you made a mistake.

```rust
// "Wait, those weren't actually the same person."
brain.entity_unmerge(bob_c_id).await?;  // works within grace; rejected after
```

---

## Day 6: Schema evolves

Two quarters in, you realize you've been missing something. The team's been talking about "incidents" — outages, post-mortems, blameless retros — but you don't have an Incident entity type. Decisions about incidents are being awkwardly stuffed into Decision entities.

You evolve the schema. Edit `acme-schema.brain`:

```
# Add an entity type
define entity_type Incident {
    attributes {
        slug:        text required unique
        severity:    enum[sev1, sev2, sev3, sev4] optional
        started_at:  timestamp optional
        resolved_at: timestamp optional
    }
}

# Add predicates
define predicate caused_by {
    kind: Fact
    object: Entity<Person>      # or Project, or anything
}

# Add an extractor
define extractor incidents {
    kind: llm
    target: entity Incident
    model: "claude-haiku-4-5"
    prompt: """
        Did this memory describe an incident or outage? If yes, extract:
        {"slug": "<short_slug>", "severity": "sev1"|"sev2"|"sev3"|"sev4",
         "summary": "<one sentence>"}
        Empty array if no incident.
        
        Memory: {{memory.text}}
    """
    schema: { /* ... */ }
    cache: enabled
    confidence_threshold: 0.8
    trigger: on encode where memory.text matches ".*(incident|outage|down|broke|sev[1-4]).*"
}
```

Upload:

```rust
let new_schema = std::fs::read_to_string("acme-schema.brain")?;
let result = brain.schema()
    .upload_text(&new_schema)
    .migration_policy(MigrationPolicy::ReExtractChanged)
    .await?;

println!("Schema v{} accepted", result.version);
println!("Migration plan:");
for action in &result.migration_plan {
    println!("  - {:?}", action);
}
```

Output:

```
Schema version 2 accepted
Migration plan:
  - AddEntityType(Incident)
  - AddPredicate(caused_by)
  - AddExtractor(incidents)
```

This is **non-breaking** — pure additions. Existing entities, statements, relations untouched. New writes use the new schema.

But you also want the new `incidents` extractor to run over *existing* memories. That's a **backfill**:

```rust
let job = brain.admin().backfill()
    .extractor("incidents")
    .memory_range(MemoryRange::All)
    .priority(Priority::Background)
    .start()
    .await?;

println!("Backfill job {} started", job.id);

// Check status:
loop {
    let status = brain.admin().job_status(job.id).await?;
    println!("Progress: {}/{} memories ({:.1}%)",
             status.completed, status.total,
             100.0 * status.completed as f64 / status.total as f64);
    if status.is_done() { break; }
    tokio::time::sleep(Duration::from_secs(30)).await;
}
```

The backfill runs in background, respects priority budget, resumable on restart. New Incident entities and decisions get created.

### Breaking changes

What if you wanted to *remove* the Decision type? That's breaking:

```rust
let result = brain.schema()
    .upload_text(&schema_without_decision)
    .migration_policy(MigrationPolicy::CascadeTombstone)  // explicit opt-in
    .await?;
```

Brain refuses without the explicit flag. With it, all existing Decision entities and statements get tombstoned with reason `SchemaInvalidation` and a 30-day grace before hard deletion. You can roll back the schema within the grace.

### Improving an extractor

Three months in, you tune the `preferences` extractor — better prompt, better few-shot examples. You bump its version implicitly by editing the schema:

```rust
brain.schema().upload_text(&improved_schema).await?;
```

What happens to existing Preferences?
- They're flagged **stale** (`extractor_version` older than current).
- They remain queryable.
- A worker periodically lists stale statements:

```rust
let stale = brain.admin().list_stale_statements(StalenessFilter::All).await?;
println!("{} stale statements", stale.len());
```

You decide: re-extract them all (costs LLM calls but improves quality), or let them age out, or hard-delete and re-extract on demand.

```rust
brain.admin().backfill()
    .extractor("preferences")
    .stale_only(true)
    .start()
    .await?;
```

---

## Day 7: Operating in production

Mind is live. Engineers use it daily. You need observability, cost control, backups.

### Metrics

Brain exports Prometheus metrics on `:7860/metrics`:

```
brain_encode_total{shard="0"} 1284932
brain_query_latency_seconds{quantile="0.5"} 0.008
brain_query_latency_seconds{quantile="0.99"} 0.041

brain_extractor_extractions_total{extractor="preferences",status="success"} 41281
brain_extractor_extractions_total{extractor="preferences",status="skipped_budget"} 12
brain_extractor_extractions_total{extractor="preferences",status="failure"} 47
brain_extractor_cost_usd_total{extractor="preferences"} 27.41
brain_extractor_cache_hit_rate{extractor="preferences"} 0.62

brain_worker_queue_depth{worker="llm_extractor"} 8
brain_worker_queue_overflow_total{worker="llm_extractor"} 0

brain_retriever_contribution_top10{retriever="semantic"} 0.51
brain_retriever_contribution_top10{retriever="lexical"} 0.27
brain_retriever_contribution_top10{retriever="graph"} 0.22

brain_entity_resolution_total{tier="exact"} 982341
brain_entity_resolution_total{tier="fuzzy"} 14829
brain_entity_resolution_total{tier="embedding"} 3104
brain_entity_resolution_total{tier="llm"} 0       # not enabled
brain_entity_resolution_total{tier="created"} 8421
brain_entity_resolution_ambiguous_total 39
```

You watch:
- **Cost**: `brain_extractor_cost_usd_total` — daily LLM spend.
- **Health**: `brain_worker_queue_overflow_total` — if non-zero, you're losing extractions.
- **Quality**: `brain_retriever_contribution_top10` — if one retriever stops contributing, something's off.
- **Ambiguity**: `brain_entity_resolution_ambiguous_total` — count of pending resolutions; if growing, review needed.

### Cost control

If LLM extraction is getting expensive:

```rust
// Reduce per-call budget on a hot extractor
brain.admin()
    .update_extractor_config("preferences")
    .cost_budget("$0.0005 per memory")
    .await?;

// Or disable it temporarily
brain.admin().disable_extractor("preferences").await?;

// Or change the trigger to fire less often
// (edit schema, re-upload — preferences only on memories that mention "prefer")
```

You can also set a global daily cap:

```bash
brain-server start \
  ... \
  --llm-daily-budget-usd 50.00
```

When the cap is hit, LLM extractors skip with a metric until midnight. No surprise bills.

### Backups

```bash
# Hot backup (background, no downtime)
brain-admin backup --output ~/backups/$(date +%Y-%m-%d).tar.zst

# Restore (offline)
brain-admin restore --input ~/backups/2026-08-15.tar.zst --data-dir ~/acme-mind/data
```

What's in the backup:
- Substrate (memories, vectors, WAL checkpoints, redb metadata) — authoritative.
- Knowledge layer (entities, statements, relations) — authoritative.
- LLM cache — optional (set `--no-cache` to skip; restore is faster but first queries are slower).
- Tantivy indexes and HNSW — derived. Rebuilt on restore if absent.
- Schema versions — authoritative.

### Audit

When Mind says "Priya is the VP of Infrastructure," where does that come from?

```rust
let fact = /* the statement Mind returned */;
let audit = brain.admin().trace_provenance(fact.id).await?;

println!("Statement {} created at {} by {}",
         audit.statement_id,
         audit.created_at,
         audit.extractor);

println!("Evidence:");
for mem_id in &audit.evidence {
    let mem = brain.memory_get(*mem_id).await?;
    println!("  [{}] \"{}\" - {} at {}",
             mem.id, mem.text, mem.author, format_date(mem.created_at));
}

if !audit.supersedes_chain.is_empty() {
    println!("Supersedes:");
    for prev in &audit.supersedes_chain {
        println!("  - {} (v{}): \"{}\"", prev.id, prev.version, prev.object_text());
    }
}
```

Every claim Mind makes traces back to the memories that produced it. Provenance is non-optional.

### Rollback

You uploaded a bad schema and Mind started producing garbage.

```rust
// List schema versions
let versions = brain.schema().list().await?;
for v in versions {
    println!("v{}: uploaded {} ({} active extractors)",
             v.version, format_date(v.uploaded_at), v.extractor_count);
}

// Roll back
brain.schema().rollback_to(previous_version).await?;
```

The previous schema becomes active. Stale statements from the bad version are flagged. You can re-extract them under the rolled-back schema with a backfill if you want.

To revert to substrate-only mode: use `brain-admin schema disable`. Knowledge-layer tables are retained but unused; substrate serves normally.

---

## Common patterns (cookbook)

### Pattern: agent that reads then writes

A typical Mind interaction is "answer a question, then store what was discussed":

```rust
async fn handle_user_message(brain: &Client, user: &str, msg: &str) -> Result<String> {
    // 1. Retrieve context
    let context = brain.query()
        .text(msg)
        .limit(10)
        .execute()
        .await?;
    
    // 2. Generate response with an LLM, providing context
    let response = call_llm(msg, &context).await?;
    
    // 3. Record the exchange
    brain.encode(&format!("{} asked: {}", user, msg))
        .author(user)
        .source("mind-chat")
        .commit()
        .await?;
    brain.encode(&format!("Mind replied: {}", response))
        .author("mind")
        .source("mind-chat")
        .commit()
        .await?;
    
    Ok(response)
}
```

Both messages flow through extractors. If the user mentioned a person or made a decision, Brain captures it automatically.

### Pattern: structured ingestion with hints

When you know what type something is, give Brain a hint:

```rust
// Linear ticket — you know the type and key
brain.entity::<Ticket>()
    .canonical_name(&ticket.key)               // "ACME-1247"
    .alias(&ticket.title)
    .with(|t| {
        t.key = ticket.key.clone();
        t.status = Some(ticket.status.clone());
    })
    .create()
    .await?;

// Then ingest the description as a memory mentioning this entity
brain.encode(&ticket.description)
    .mention(ticket_entity_id)                  // explicit mention
    .source(&format!("linear:{}", ticket.key))
    .commit()
    .await?;
```

The `mention` call lets you skip extractor inference for entities you already know. Saves cost.

### Pattern: build-time schema check

In your CI pipeline, validate your schema before deploying:

```rust
// validate-schema.rs
let schema_text = std::fs::read_to_string("acme-schema.brain")?;
let validation = brain.schema().validate(&schema_text).await?;
if !validation.errors.is_empty() {
    for err in &validation.errors {
        eprintln!("Schema error at line {}: {}", err.line, err.message);
    }
    std::process::exit(1);
}
println!("Schema valid");
```

### Pattern: subscribe to changes

Mind has a Slack bot that announces new decisions:

```rust
let mut stream = brain.subscribe()
    .events(&[EventKind::StatementCreated])
    .filter(EventFilter::Predicate("decided".into()))
    .start()
    .await?;

while let Some(event) = stream.next().await {
    if let Event::StatementCreated { id, subject, object, .. } = event {
        let entity = brain.entity_get(subject).await?;
        slack.post(&format!(
            "📋 Decision recorded: *{}* → {}",
            entity.canonical_name, object
        )).await?;
    }
}
```

### Pattern: per-team query scope

Sometimes you want results scoped to a team. Filter via graph traversal:

```rust
let infra_team_members = brain.relation::<ReportsTo>()
    .traverse_from(infra_lead_id)
    .reverse()
    .depth(3)
    .collect_entities()
    .await?;

let answer = brain.query()
    .text(question)
    .restrict_to_subjects(&infra_team_members)
    .execute()
    .await?;
```

### Pattern: re-extraction on prompt improvement

You improved the `decisions` extractor's prompt. Re-run on the last 30 days:

```rust
brain.admin().backfill()
    .extractor("decisions")
    .memory_range(MemoryRange::Window {
        since: now() - Duration::days(30),
        until: now(),
    })
    .priority(Priority::Background)
    .start()
    .await?;
```

### Pattern: explainable answers

When Mind shows a result, surface the provenance:

```rust
fn render_for_user(item: &ResultItem) -> String {
    let mut s = match &item.item {
        ItemRef::Statement(stmt) => format!("{}", render_statement(stmt)),
        ItemRef::Memory(mem) => format!("{}", mem.text),
        // ...
    };
    
    if let Some(evidence) = item.evidence() {
        s.push_str("\n_Based on:_");
        for mem in evidence.iter().take(3) {
            s.push_str(&format!("\n  - {} ({})", mem.text_excerpt(), format_date(mem.created_at)));
        }
    }
    
    s
}
```

Users see what Mind based its answer on. Trust comes from transparency, not certainty.

---

## What you don't have to do

Brain handles silently:

- **Embedding generation** — when you ENCODE, Brain embeds with the configured model. You never call an embedding API.
- **HNSW maintenance** — vectors get indexed, tombstoned, rebuilt on schedule. No tuning required.
- **Tantivy commits and merges** — BM25 index stays fresh. Segment merges happen during low-traffic windows.
- **WAL durability** — every write is durable before ACK. Crashes don't lose committed data.
- **Decay and salience** — the substrates automatic decay of episodic memories continues whether or not the knowledge layer is active.
- **Confidence aggregation** — when multiple memories support a statement, Brain combines confidences using the documented formula.
- **Supersession chains** — new Preference automatically supersedes the matching old one. New contradicting Fact doesn't (different rules); Brain knows the difference.
- **Cache management** — LLM cache evicts LRU; expired entries swept.
- **Idempotency** — re-running an extractor over the same memory is a no-op (modulo cache TTL).
- **Per-shard scheduling** — workers respect their priority budgets so foreground latency stays low even under heavy background extraction.
- **FORGET cascade** — soft FORGET cascades softly, hard cascades hard. You don't write cleanup code.
- **Index recovery** — if tantivy or HNSW gets corrupted, Brain rebuilds from authoritative redb tables. WAL covers everything else.

What you do:

- Declare the schema.
- Wire up ingestion.
- Query.

Everything else is the substrate's job.

---

## A second scenario: personal memory assistant

A briefer walkthrough showing a different feature emphasis. You're building Mnemo, a personal AI that remembers everything you tell it about your life.

Schema:

```
namespace mnemo

define entity_type Person {
    attributes {
        relationship: text optional         # "spouse", "colleague", "friend"
        birthday:     date optional
    }
}

define entity_type Place {
    attributes {
        kind: enum[restaurant, city, venue, home] optional
    }
}

define predicate likes      { kind: Preference, object: Value<text> }
define predicate dislikes   { kind: Preference, object: Value<text> }
define predicate visited    { kind: Event,      object: Entity<Place> }
define predicate said       { kind: Event,      object: Value<text> }

define relation_type knows {
    from: Person
    to: Person
    cardinality: many-to-many
    symmetric: true
}

use brain.entity_mentions
use brain.temporal_expressions

define extractor personal_preferences {
    kind: llm
    model: "claude-haiku-4-5"
    prompt: """
        Extract personal preferences (likes/dislikes) and visits/events from this memory.
        ...
    """
    # ... schema, cache, etc.
}
```

Usage:

```rust
// Ingest a daily journal entry
mnemo.encode("Had dinner at Kismet with Sarah. She loved the lamb but hated \
              the noise. Met her colleague Aaron who turned out to know my \
              brother from college.").await?;
```

After extraction:
- Entities: Sarah, Aaron, Kismet (Place, kind=restaurant)
- Statements: 
  - Event: Sarah visited Kismet on 2026-05-12
  - Preference: Sarah likes "lamb at Kismet"
  - Preference: Sarah dislikes "noise at Kismet"
- Relations: Sarah knows Aaron; Aaron knows [my brother — if "my brother" resolves to an entity]

A month later: "Where should I take Sarah for her birthday?"

```rust
let sarah = mnemo.entity::<Person>().resolve("Sarah").await?.expect_resolved()?;

let prefs = mnemo.statements()
    .where_subject(sarah.entity_id)
    .of_kinds(&[StatementKind::Preference])
    .with_min_confidence(0.7)
    .list()
    .await?;

let visits = mnemo.statements()
    .where_subject(sarah.entity_id)
    .where_predicate("visited")
    .order_by_event_time(Order::Desc)
    .limit(20)
    .list()
    .await?;

// Feed to an LLM with: "Given these prefs and visits, suggest restaurants for a birthday."
```

Mnemo has the structured signal an LLM needs to be genuinely helpful, not just lucky.

---

## Final thoughts

The thing to internalize: **with Brain, you stop building three things**.

- You stop building an entity extraction pipeline.
- You stop building a hybrid retrieval layer over Elasticsearch + a vector store.
- You stop building a provenance tracking system.

You declare the schema, ingest memories, and query. Brain owns the middle.

The cost is single-node (no horizontal scale), one schema namespace per deployment (no multi-tenant), and some discipline about confidence and contradictions. For the cognitive-substrate use case — building agents and assistants that need to *understand*, not just *retrieve* — that's a good trade.

Build something with it. The fastest way to learn what Brain can do is to give it your real data and ask real questions.
