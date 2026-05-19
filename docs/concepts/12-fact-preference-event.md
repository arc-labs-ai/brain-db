# 12 — Fact, Preference, Event

A statement (chapter 11) is a typed claim with subject,
predicate, object, evidence, and confidence. Statements also
have a **kind**: one of `Fact`, `Preference`, or `Event`.

This chapter is about why three kinds — and what difference
the kind makes when claims arrive that contradict, supersede,
or correct existing ones.

---

## At a glance

| Kind | What it captures | Mutation policy | Time field |
|---|---|---|---|
| **Fact** | Stable claims about the world | Append-only. Contradicting Facts are stored side-by-side. | `valid_from` (extraction time), no `valid_to` until contradicted |
| **Preference** | Revisable beliefs / choices | Versioned via supersession. New Preference replaces old. | `valid_from`, `valid_to = superseded_by.extracted_at` |
| **Event** | Discrete occurrences at a moment | Immutable. Corrections add a new Event, never modify. | `event_at` (the moment); no validity range |

The same storage row holds all three; what differs is the
*contracts* the substrate honours when a new statement arrives
that matches an existing one.

---

## Fact

A Fact is a stable claim about the world. Examples:

- "Priya is the engineering manager for Atlas."
- "Atlas is the codename for the authentication migration."
- "Acme Corp's domain is acme.com."
- "The team's standup is at 09:00 PST."

Facts are about *how things are*. They might change — but
when they do, the change is gradual, attributable, and worth
preserving the history of.

### The mutation rule: append, don't replace

When a new Fact arrives that matches an existing Fact's
subject + predicate:

- **If the object is the same**, the existing Fact's
  evidence list grows and its confidence rises. Same row,
  augmented.
- **If the object is *different***, the new Fact is stored
  too — both Facts now exist. This is a *contradiction*,
  not a supersession.

That second case is the important one. Suppose:

```
# Existing fact (created last month):
Statement {
    statement_id = stmt_001
    kind         = Fact
    subject      = ent_priya
    predicate    = pred_role
    object       = TextLiteral("engineering manager")
    confidence   = 0.92
}

# New encode arrives today: "Priya is now VP Engineering."
# Extractor produces:
Statement {
    statement_id = stmt_002
    kind         = Fact
    subject      = ent_priya
    predicate    = pred_role
    object       = TextLiteral("VP engineering")
    confidence   = 0.84
}
```

Both Facts now exist. Neither replaces the other. A query
for "Priya's role" gets back **both**, with the higher-
confidence one ranked first and a *contradiction marker* in
the response metadata:

```
{
    statements: [
        { id: stmt_001, object: "engineering manager", confidence: 0.92, evidence: […] },
        { id: stmt_002, object: "VP engineering",      confidence: 0.84, evidence: […] },
    ],
    contradiction: {
        highest_confidence: stmt_001,
        recommendation: "by_confidence | by_recency | unresolved",
    },
}
```

The agent — or you, or an upstream UI — picks how to
resolve. Brain is a substrate; it surfaces the conflict
rather than picking a winner silently.

### Why surface contradictions instead of resolving them

Because the agent might know things Brain doesn't. Maybe
the more recent fact is wrong (extractor misread the text);
maybe the more confident one is wrong (extractor was
biased); maybe both are right and the role legitimately
changed.

A substrate that silently picked one would hide that nuance.
The cognitive design prefers surfacing the contradiction —
the calling system gets to see "Priya's role" is in flux and
can ask a follow-up, prompt the user, or escalate.

If you want auto-resolution, you write that logic *above*
Brain. The substrate's contract is "I'll tell you what I
have; you decide what to believe."

---

## Preference

A Preference is a revisable belief or choice. Examples:

- "Priya prefers async sprint planning."
- "The user prefers JSON output over YAML."
- "Devi likes one-on-ones on Tuesdays."

Preferences are *settable*: someone (the user, the agent,
the entity itself) holds them, and they change over time
without contradicting their past values. Yesterday's
preference isn't *wrong* — it's just no longer current.

### The mutation rule: supersede

When a new Preference arrives that matches an existing one's
subject + predicate:

- The old Preference gets `superseded_by = new_id`.
- The new Preference gets `version = old.version + 1` and
  `chain_root = old.chain_root` (or, if old.version was 1,
  `chain_root = old.statement_id`).
- The two are now linked in a **supersession chain**.

A query for "Priya's current preferences" returns only the
*head* of the chain (where `superseded_by = None`). A query
with `include_superseded = true` returns the whole chain in
order.

### Worked example

```
# Day 1
Statement {
    statement_id   = stmt_p1
    kind           = Preference
    subject        = ent_priya
    predicate      = pred_prefers
    object         = TextLiteral("async sprint planning")
    confidence     = 0.81
    version        = 1
    superseded_by  = None
    chain_root     = stmt_p1
}

# Day 12 — new memory: "Priya thinks weekly syncs would help."
Statement {
    statement_id   = stmt_p2
    kind           = Preference
    subject        = ent_priya
    predicate      = pred_prefers
    object         = TextLiteral("async with weekly syncs")
    confidence     = 0.78
    version        = 2
    superseded_by  = None
    chain_root     = stmt_p1
}
# And stmt_p1 is updated in-place:
#   stmt_p1.superseded_by = stmt_p2
```

Now Brain knows Priya *used to prefer* pure async, and *now
prefers* async-with-syncs. The history is queryable for
audit; the current state is queryable for action.

### "Deleting" a preference

You don't delete a Preference. You *supersede* it with a
sentinel value:

```
encode("Priya no longer cares about meeting style.")
# → extractor produces:
Statement {
    statement_id   = stmt_p3
    kind           = Preference
    subject        = ent_priya
    predicate      = pred_prefers
    object         = TextLiteral("(no preference)")   ← sentinel
    confidence     = 0.71
    version        = 3
    superseded_by  = None
    chain_root     = stmt_p1
}
```

The chain still exists; the current head is the sentinel.
History queries can recover the past preferences. If you
need the Preference genuinely *gone* (privacy, encoded by
mistake), there's a hard-tombstone admin RPC.

### Why supersede instead of overwriting

Two reasons:

1. **Audit trail.** Knowing what the user *used to* prefer
   is information. An agent that always replied "Priya
   prefers Async" when she switched her preference three
   times in two months is making the team look bad. The
   chain preserves that something changed.
2. **Recovery.** If a new Preference turns out to come from
   a misread memory, you can supersede *it* (with a
   correction) and the prior Preference is still there
   intact in the chain.

The cost is one extra row per change. Storage is cheap;
audit is valuable.

---

## Event

An Event is a discrete occurrence at a moment in time.
Examples:

- "Priya scheduled the planning session for Tuesday."
- "The deploy of `auth-service` failed at 14:31."
- "The user clicked 'export'."
- "Atlas v3 shipped on 2024-09-15."

Events are about *something that happened*. They have a
time-stamp — `event_at` — and they don't change after
being recorded.

### The mutation rule: immutable

When a new Event arrives that "matches" an existing Event
(same subject + predicate + similar time), it's stored as a
*second, independent* Event. Events don't supersede each
other.

```
# Existing event:
Statement {
    statement_id  = stmt_e1
    kind          = Event
    subject       = ent_priya
    predicate     = pred_scheduled
    object        = ent_planning_session_42
    event_at      = 2024-09-10T14:00Z
    evidence      = [ mem_018a… ]
}

# Later memory: "Priya scheduled another planning session, this time on Thursday."
Statement {
    statement_id  = stmt_e2          ← new, independent
    kind          = Event
    subject       = ent_priya
    predicate     = pred_scheduled
    object        = ent_planning_session_43
    event_at      = 2024-09-12T15:30Z
    evidence      = [ mem_019b… ]
}
```

Both events exist independently. The query "did Priya
schedule planning sessions?" returns both, sorted by
`event_at`.

### Correcting an Event

You don't. Events are immutable. If the first memory got the
time wrong, the correction is *another statement*:

- A new Fact: `stmt_e1 actually_occurred_at 2024-09-10T13:00Z`
  — that's a Fact, of kind Fact (yes, Brain has Facts
  *about* Events), supported by the corrective memory.
- Or a new Event entirely if the first one was wholly
  fabricated, with the original tombstoned via admin.

The point is: the historical record stays straight. The
substrate doesn't rewrite Events to match later updates; it
adds new statements with their own provenance.

### Why immutability

Because Events are *what happened*. The agent's record of
what happened shouldn't drift as the agent's understanding
evolves. If the substrate let you "fix" an Event, six months
later you'd have no way to distinguish "the agent observed
this directly" from "the agent corrected this based on
later evidence."

Audit trail. Provenance. Same theme as Facts and
Preferences, applied to a different mutation pattern.

---

## Side by side

Same three pieces of memory text:

> "Priya is the engineering manager."
> "Priya prefers async meetings."
> "Priya scheduled the planning session for Tuesday."

Three Statements, three different kinds, three different
mutation contracts:

| Kind | Mutation when a "matching" claim arrives |
|---|---|
| Fact (the role) | A *different* role → both stored; contradiction surfaced. A *same* role → evidence appended; confidence rises. |
| Preference (async meetings) | A new preference → supersedes the old. History chain preserved. |
| Event (scheduled the session) | A new event → independent row. Even if subject + predicate match, they don't supersede each other. |

The shared storage means querying across kinds works — you
can ask "show me everything about Priya" and get all three.
The differing contracts mean the substrate models the
domain correctly: stable beliefs, revisable choices, and
discrete occurrences are three different things.

---

## Picking the right kind

Some rules of thumb:

**Use Fact when:**
- The claim is about *how things are*, not what someone
  prefers or what happened.
- Two different values for the same predicate would be a
  contradiction worth surfacing, not just an update.
- Examples: roles, capabilities, properties of things,
  relationships in the world.

**Use Preference when:**
- The claim is about *what someone wants* (or believes, in
  a non-factual sense).
- Updates over time are expected and the new value should
  *replace* the old as the "current" answer.
- Examples: user preferences, settings, working styles,
  beliefs that may evolve.

**Use Event when:**
- The claim is about *something that happened at a moment*.
- Two events with the same subject + predicate at different
  times are *both real*, not contradictions.
- Examples: actions taken, observations made, deploys,
  meetings, transactions, anything log-shaped.

Picking the right kind is the extractor's job, not the
caller's. The extractor's declaration in the schema names
the target:

```
extractor preference_extraction {
    kind   = llm
    target = statement Preference
    …
}

extractor event_detection {
    kind   = classifier
    target = statement Event
    …
}
```

The extractor produces statements of its declared kind. So
the *schema* picks the kind via the extractor; the running
substrate just honours it.

---

## Why three kinds and not more

Other plausible kinds got considered and rejected. From the
spec discussion:

- **Observation** ("I saw X"). Same as Event with
  `predicate = observed`.
- **Goal** ("I want X to happen"). Same as Preference with
  `predicate = wants`, or a Fact with `predicate = goal`.
- **Rule** ("If X then Y"). Rules aren't claims about
  entities; they're programs. Belongs in the extractor /
  planner layer, not in statements.
- **Hypothesis** ("X might be true"). Same as Fact with low
  confidence.
- **Contradiction-marker** ("X and Y disagree"). Same as
  Fact with `predicate = contradicts`, subject and object
  being the two statements.

Three kinds are enough for almost any domain. If you have a
real sixth-kind use case, you encode it in the `predicate`
and pick the kind with the right mutation contract.

---

## Why not just one "Statement" with a mutability flag

Considered. Rejected. The argument for one type: simpler
schema, fewer concepts to teach.

The argument against:

1. **Users mentally categorise.** "Fact vs preference vs
   event" maps to how people think about claims. A single
   type with flags doesn't help.
2. **Extractor outputs differ in shape.** An event extractor
   produces `event_at`; a preference extractor produces
   `valid_from / valid_to`. Strong typing prevents
   extractor misuse.
3. **Default query behaviour differs.** "Current
   preferences" should mean head-of-chain only; "Events"
   should mean all events in a range; "Facts" should
   surface contradictions. Defaults need a kind to attach
   to.
4. **Validation differs.** An Event must have `event_at`. A
   Preference can be superseded; a Fact's `superseded_by`
   is unused. Strong typing means the validator's life is
   easier.

So three kinds, one storage. The API surface carries the
distinction; the implementation shares everything.

---

## Recap

- Every statement has a `kind`: Fact, Preference, or Event.
- **Fact**: append-only. Contradicting Facts both stored;
  the conflict is surfaced.
- **Preference**: supersession chain. New replaces old;
  history is preserved.
- **Event**: immutable. New occurrences are new rows; old
  ones don't update.
- The extractor's schema declaration picks the kind. The
  substrate honours the mutation contract.
- Three kinds is enough for most domains. More predicates,
  not more kinds, is the extension point.

---

## Where to go next

- **Statements in general:** [chapter 11](11-statements.md).
- **Typed edges between entities:** [chapter 13](13-relations.md)
  — relations.
- **What produces statements:** [chapter 14](14-extractors.md)
  — extractors.
- **The verbs that query statements:** [chapter 16](16-cognitive-operations.md).
