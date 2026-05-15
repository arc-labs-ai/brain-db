# Why Three Statement Kinds

## Fact, Preference, Event

the knowledge layer distinguishes three statement kinds in its API. Internally they share a common storage schema; the distinction lives in mutation rules, default validity semantics, and query intent.

| Kind | What it captures | Mutation policy | Time default |
|---|---|---|---|
| **Fact** | Stable claims about the world. "Priya is the engineering manager." | Append-only, contradictable by higher-confidence newer Facts. | Valid-from extraction time, no valid-to until contradicted. |
| **Preference** | Revisable beliefs/choices. "Priya prefers async meetings." | Versioned via supersession. New preference supersedes old. | Valid-from extraction time, valid-to = `superseded_by.extracted_at`. |
| **Event** | Discrete occurrences at a moment. "Priya scheduled a planning session on Tuesday." | Immutable. Corrections add a new Event, never modify. | `event_at` is the moment; no valid range. |

## Why distinguish them

The three kinds are not three storage schemas. They are three *contracts* on top of one storage schema.

### Mutation contracts differ

When new evidence arrives, what happens to existing data depends on the kind:

- **Fact + new contradicting Fact**: both stored. The planner returns the higher-confidence (or more recent at equal confidence) one. The contradicted Fact stays in the audit trail.
- **Preference + new Preference (same subject, predicate)**: the new one supersedes. The old one's `superseded_by` points to the new one. Queries for "current preference" return only the new one. History queries return the chain.
- **Event + similar Event**: stored as a second, independent Event. Events do not supersede each other — that's their whole point. If the user got the date wrong on the first Event, they record a corrective Fact ("the meeting was Wednesday, not Tuesday"), or insert a new Event with provenance noting the correction.

### Query intent differs

Users ask different questions about each kind:

| Query pattern | Kind |
|---|---|
| "Who is X?" / "What is the role of X?" | Fact |
| "What does X prefer?" / "How does X like things done?" | Preference |
| "What did X do?" / "What happened on date D?" / "Show me the timeline." | Event |

If the user asks "what does Priya prefer," they almost certainly don't want a Fact about Priya's job title even if it has higher confidence. The kind filter resolves this without requiring elaborate query syntax.

### Storage is unified

Internally, all three are rows in a single `statements` table:

```rust
struct StatementRow {
    id: StatementId,
    kind: StatementKind,      // Fact | Preference | Event
    subject: EntityId,
    predicate: PredicateId,
    object: StatementObject,
    confidence: f32,
    evidence: Vec<MemoryId>,
    extractor_id: ExtractorId,
    extracted_at: u64,
    
    // Time fields (kind-dependent meaning)
    valid_from: Option<u64>,      // Fact, Preference: when this becomes true
    valid_to: Option<u64>,        // Fact, Preference: when it stops being true
    event_at: Option<u64>,        // Event: when it occurred
    
    // Versioning (Preference only typically)
    version: u32,
    superseded_by: Option<StatementId>,
}
```

`kind` is the first column of compound indexes: queries that filter by kind don't pay for scanning other kinds' rows. Cross-kind queries still work; per-kind queries are fast.

## Why not just one statement type with a "mutability" flag?

We considered this. The argument for one type: simpler schema, fewer concepts to teach.

The argument against (and why we went with three):

1. **Users mentally categorize**. "Fact vs preference vs event" maps to how people think about knowledge. A single type with flags doesn't help them at the schema level.

2. **Extractor schemas differ**. A pattern extractor producing Events ("met with X on T") has a different output shape from one producing Preferences ("X likes Y"). Strong typing prevents extractor misuse.

3. **Default query behavior differs**. By default, "Preference" queries return current versions only; "Event" queries return all events in a range; "Fact" queries return highest-confidence non-superseded. Defaults need a type to attach to.

4. **Validation differs**. An Event must have an `event_at`. A Preference can be superseded; a Fact's `superseded_by` field is unused. The validator's life is easier with three kinds.

Three kinds, one storage. The API surface carries the distinction; the implementation shares everything.

## Why not more kinds (e.g., Observation, Goal, Rule)?

Each kind adds:
- API surface (a new constructor, a new query filter)
- Validation logic
- Documentation burden
- Test surface

The benefit of a new kind is that users would otherwise abuse one of the existing three to express it. So the test is: can existing kinds express it cleanly with minor schema work?

| Candidate | Can existing kinds express it? |
|---|---|
| **Observation** ("I observed X") | Yes — an Event with predicate=observed. |
| **Goal** ("I want X") | Yes — a Preference with predicate=wants, or a Fact with predicate=goal. |
| **Rule** ("If X then Y") | No — but Rules are not facts about entities; they're programs. Belongs in the extractor / planner layer, not as statements. |
| **Hypothesis** ("X might be true") | Yes — a Fact with low confidence. |
| **Contradiction-marker** ("X and Y disagree") | Yes — a Fact with predicate=contradicts, subject=X, object=Y. |

Three kinds are enough. We resist the temptation to add more. If a user has a genuine sixth-kind use case, they encode it in `predicate` and tag with `kind=Fact` or whichever has the right mutation contract.

## Special case: contradicting Facts

Two Facts with the same `(subject, predicate)` but different `object` are *contradictions*, not supersessions. Both are stored. The planner exposes the conflict:

```rust
struct ContradictionView {
    statements: Vec<Statement>,    // all conflicting statements
    highest_confidence: StatementId,
    recommendation: ConflictResolution,  // by_confidence | by_recency | unresolved
}
```

This is one of the things a cognitive substrate *should* surface, not hide. The user (or an upstream agent) decides how to resolve it. Surfacing contradictions is a feature, not a bug.

## Special case: deleting a Preference

You don't. You record a new Preference that *supersedes* it, with the new object being a sentinel (`null` or `"none"` or whatever the schema permits). The supersession chain stays intact. If someone wants to know what Priya used to prefer and stopped preferring, the history is there.

If the user genuinely wants the Preference gone forever (e.g., they encoded it by accident, or for privacy reasons), they invoke `FORGET_STATEMENT` (hard, with the same grace-period semantics as the substrate's hard FORGET on memories). This is rare and audited.
