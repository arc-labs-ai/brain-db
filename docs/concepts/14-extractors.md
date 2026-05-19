# 14 — Extractors

An **extractor** is the piece that turns unstructured memory
text into the structured items the knowledge layer holds —
entities, statements, and relations. Without extractors, the
knowledge tables stay empty no matter how much you encode.

Brain ships three *tiers* of extractor, in increasing order
of capability and cost:

| Tier | Speed | Cost | Determinism | Recall | Precision |
|---|---|---|---|---|---|
| **Pattern** | 10–100 µs | ~$0 | yes | low (narrow) | very high |
| **Classifier** | 1–10 ms | small (CPU) | yes (pinned) | medium | high |
| **LLM** | 100 ms – 10 s | dollars per call | cached → yes | high | high (validated) |

The substrate composes them like a ladder: cheap and certain
tiers run first; expensive and broad tiers fill in what the
cheap tiers miss. Operators pick which tiers a deployment
uses by declaring them in the schema.

---

## What "extracting" means concretely

Suppose a memory arrives:

> "Priya talked to Devi in standup about moving sprint
> planning to async."

For a knowledge layer that cares about Persons and
Preferences, an extractor's job is to produce something like:

```
EntityMention { type = Person, text = "Priya",  start = 0,  end = 5  }
EntityMention { type = Person, text = "Devi",   start = 18, end = 22 }
StatementMention {
    kind       = Preference,
    subject    = "Priya",
    predicate  = "prefers",
    object     = "async sprint planning",
    confidence = 0.84,
}
```

These are *candidates* — typed mentions, not yet
materialised. The substrate then:

1. Resolves each `EntityMention` against the entity tables
   (chapter 10: exact / alias / trigram / vector
   similarity).
2. Writes resulting statements / entities / relations into
   the knowledge-layer redb tables.
3. Records an audit row per extractor invocation, with the
   outcome.

The extractor doesn't touch the database itself. It just
returns a typed list. The substrate's *materialiser* takes
that list and turns it into rows.

---

## Pattern extractors

The simplest tier. A pattern extractor declares one or more
regular expressions and a target type:

```
extractor person_mentions {
    kind     = pattern
    target   = entity Person
    patterns = [
        /\b[A-Z][a-z]+\b/,                  # "Priya"
        /\b[A-Z][a-z]+ [A-Z][a-z]+\b/,      # "Priya Ramesh"
    ]
    confidence = 0.7
}
```

On every encode, the substrate runs each pattern over the
memory's text. Each regex match produces an entity-mention
candidate of the declared type, at the matched span, with
the declared confidence.

### What patterns are good at

- **IDs, codes, URLs, code identifiers.** "ATLAS-1247",
  "https://acme.com/api", "auth_service".
- **Well-formatted dates and times.** "2024-09-12", "14:31".
- **Names with consistent capitalisation.** Cheap to match,
  high precision.
- **Triggers for higher tiers.** A regex-matched pattern can
  *gate* whether an expensive extractor runs at all.

### What patterns are bad at

- Paraphrase ("she," "the engineering manager").
- Declension and morphology ("Priya's" doesn't match
  "Priya").
- Semantic intent ("any complaint about meetings").
- Anything where the surface form varies.

So patterns are deliberately *narrow*: they're meant to
catch the high-precision stuff cheaply, and let the higher
tiers handle the rest.

### Safety: the regex size limit

Regex engines can be tricked into catastrophic backtracking
or DFA explosion with adversarial patterns — a "regex denial
of service" via a 30-line nested-quantifier pattern. Brain
caps compiled regex size at 1 MiB (NFA + DFA combined) and
rejects patterns above that:

```
extractor evil_pattern {
    kind     = pattern
    patterns = [ "(a+)+b" ]   # exponential on input "aaaaaa…"
    …
}
# → schema validation fails: regex too complex
```

The cap is a hard limit at extractor compilation. A
misconfigured schema can't take the shard down at runtime
through a poorly-chosen pattern.

> **What's regex denial-of-service?**
>
> Some regex implementations evaluate certain pattern shapes
> in exponential time. A pattern like `(a+)+b` on input
> `aaaaaaaaaaaaaaaaaaaaaaaaaac` (no 'b' at the end) can
> take seconds. Modern engines (and the Rust `regex` crate
> Brain uses) have explicit guards; the size limit is the
> belt-and-braces version.
>
> See [Wikipedia: ReDoS](https://en.wikipedia.org/wiki/ReDoS).

---

## Classifier extractors

The middle tier. A classifier extractor wraps a small,
pinned ML model — typically a fine-tuned BERT-derivative or
a logistic regression — that takes the memory text and
outputs structured predictions.

```
extractor reporting_lines {
    kind                = classifier
    target              = relation reports_to
    model               = "brain-reporting-line-classifier-v3"
    confidence_threshold = 0.8
    trigger             = on encode where memory.text matches ".*manager.*|.*reports.*"
}
```

The substrate runs the model on every memory whose `trigger`
fires, takes the prediction's confidence, and emits a
relation candidate if the confidence exceeds the threshold.

### Why deterministic

Classifier outputs are bit-exact across runs, given:

- **Pinned weights** — same `.safetensors` file every time.
- **Pinned tokenizer** — same `tokenizer.json`.
- **Greedy decoding** — no sampling, no temperature.

Same input → same output, byte-for-byte. That's what makes
classifier extractors fit the idempotency model with no
cache (chapter 25).

### Where classifiers fit

A classifier is the right tier when:

- The pattern would be too brittle (paraphrase, morphology
  variants).
- The LLM would be overkill (you don't need general
  reasoning).
- You have training data and the budget to train (or you
  can use a pre-trained model from the community).

Brain doesn't train classifiers for you. You bring the
model; the crate provides the runtime. The model file
lives in your deployment's `models/` directory; the schema
references it by path.

### Where classifiers fall short

- **Adversarial inputs** — text the model wasn't trained
  for produces low-confidence outputs that may be wrong.
- **Long-tail entities** — a model trained on common
  English names won't reliably tag domain-specific names
  (codenames, internal jargon).
- **Multi-step reasoning** — "the meeting on Tuesday's
  agenda included…" requires the model to understand the
  date *and* the relationship to the agenda. Classifiers
  do single-shot prediction; chains are LLM territory.

### Degraded mode

If the operator hasn't configured the model path (env var
unset, file missing), the materialiser wires the
classifier extractor in **degraded mode**: it stays
registered but every invocation immediately returns
`Failure("model not loaded")`. The audit log gets a row
explaining why; the shard still spawns.

This pattern shows up in the LLM tier too — Brain prefers
"loudly fail every call" over "refuse to start" so
deployment misconfiguration is recoverable without
restarting.

---

## LLM extractors

The third tier. An LLM extractor wraps a call to a large
language model (Anthropic Claude, OpenAI GPT, or a
self-hosted equivalent) via the JSON-output mode that
modern models support.

```
extractor preference_extraction {
    kind          = llm
    target        = statement Preference
    model         = "claude-haiku-4-5"
    prompt        = """
        Extract user preferences from this memory.
        Output JSON matching the schema below.
        Memory: {memory.text}
    """
    schema = { /* JSON Schema for the output */ }
    cache_ttl     = 90d
    cost_budget   = "$0.001 per memory"
    trigger       = on encode where memory.kind = episodic
}
```

The substrate's LLM tier:

1. Looks up the memory's text in the **LLM cache** (next
   section). Hit → return the cached output, no API call.
2. Miss → estimate the cost of the call against the
   declared `cost_budget`. Over budget → audit
   `SkippedBudget`, no call.
3. Within budget → call the LLM provider (chapter 22 of the
   architecture tier covers the HTTP transport).
4. **Validate the response against the declared schema.**
   If invalid, retry once with the validation error fed
   back into the prompt; if still invalid, drop and log.
5. Project the validated JSON to entity / statement /
   relation mentions per the extractor's `target`.
6. Write the response to the cache.

The cache is the most important reliability feature here.
The next two encodes of the same text don't call the LLM at
all — they read from the cache and get bit-identical
output. That's how the LLM tier achieves idempotency
despite the underlying API being non-deterministic.

### Where LLMs fit

LLM extractors are the right tier when:

- The pattern would be over-specific.
- The classifier would need training data you don't have.
- The extraction needs *language understanding* —
  paraphrase, intent, multi-clause reasoning.

The cost is dollars per call and tens-of-milliseconds-to-
seconds of latency. The substrate runs LLM extractors in
the background, *after* the encode has acknowledged. The
client doesn't wait for them.

### JSON schema validation

The LLM tier doesn't trust the model's output verbatim.
Every response must validate against the schema declared
in the extractor:

```
schema = {
    type     = "object"
    required = ["subject", "predicate", "object", "confidence"]
    properties = {
        subject    = { type = "string", minLength = 1 }
        predicate  = { type = "string", enum = ["prefers", "wants", "dislikes"] }
        object     = { type = "string" }
        confidence = { type = "number", minimum = 0.0, maximum = 1.0 }
    }
}
```

The first response gets validated. If validation fails, the
extractor retries once, passing the validation error back
into the prompt ("your previous response missed the
`confidence` field; please include it"). If the retry still
fails, the extraction is logged as `Failure` and dropped.

This validation is what keeps malformed LLM output from
landing in the knowledge tables.

### Cost controls

`cost_budget` is a hard per-call ceiling. The substrate
estimates the call's cost (input tokens × input price +
output tokens × output price) against your declared
ceiling. If the estimate exceeds, the call is skipped —
audited, but not made.

A typical extractor:

```
cost_budget = "$0.001 per memory"
```

…means: "don't spend more than 0.001 USD on extracting
from a single memory." For a Claude Haiku call on a
~200-token memory, that's roughly the actual cost.
Long memories get budget-skipped automatically; short
memories proceed.

The pricing table is operator-configurable per deployment.
Defaults ship for common models (Claude Haiku, Claude
Sonnet, GPT-4o mini); unknown models fall back to a
conservative default.

---

## The LLM cache

The LLM cache is a separate redb file per shard —
`llm_cache.redb`, alongside the main `metadata.redb`. Why
separate?

- **Size.** The cache caps at 10 GiB by default. Co-locating
  it with the hot metadata file would bloat metadata writes.
- **Workload differences.** Cache writes are large and
  infrequent (one per LLM miss); metadata writes are small
  and constant. Different files let them tune
  independently.
- **Cheap snapshots.** A snapshot can omit the cache file;
  rebuilding it from LLM calls is expensive but possible.
  Snapshot the cache *too* if you want the warm-cache state
  preserved.

The cache key is:

```
key = (
    BLAKE3(memory.text)[..16],
    extractor_id,
    extractor_version,
    model_id,
)
```

So:

- **Same memory + same extractor + same model → cache hit.**
  Bit-identical output across runs.
- **Bump `extractor_version`** (prompt changed, schema
  changed) → old cache rows become misses; they age out
  naturally over the TTL.
- **Change `model`** → similar, full miss path until the
  cache repopulates.

Default TTL is 7 days per entry. The `llm_cache_sweeper`
worker (background) prunes expired entries on an hourly
cadence.

---

## Triggers: when an extractor runs

Triggers are *filters* that gate whether an extractor runs
on a given memory. Five forms:

```
trigger = on encode                                  # every memory
trigger = on encode where memory.kind = episodic     # filtered
trigger = on demand                                  # admin-invoked only
trigger = on schema_change                           # migration runs
trigger = periodic at "0 0 * * *"                    # cron
```

The most common is `on encode where ...`. Brain evaluates
the `where` clause against the memory's metadata before
deciding to invoke the extractor. A miss doesn't fail the
encode — it just produces a `SkippedFilter` audit row and
the extractor isn't called.

`on demand` is for backfill — running an extractor over a
batch of historical memories triggered by an admin RPC.
`on schema_change` is for migrations: when a schema upload
bumps an extractor's version, this trigger re-runs it over
the affected memories.

`periodic` is supported syntactically; the current
scheduler treats it as a hint. Production cadence is
better done with external cron + admin RPCs.

---

## Idempotency, end-to-end

Each extractor invocation is uniquely keyed:

```
IdempotencyKey {
    memory_id,
    text_hash       = BLAKE3(memory.text),
    extractor_id,
    extractor_version,
    schema_version,
}
```

Three replay scenarios:

- **Re-extracting an unchanged memory.** Pattern and
  classifier recompute identical output (deterministic by
  construction). LLM hits the cache and returns the same
  bytes.
- **Bumping an extractor's version.** All three tiers
  recompute. LLM gets a cache miss because the key
  changed. Old statements remain but get flagged stale
  (the `stale_extraction_detector` worker handles this);
  newer ones supersede or augment them.
- **Adding a new extractor in a schema upload.** Only the
  new extractor runs; existing extractors' outputs aren't
  touched. The backfill worker can sweep old memories
  through the new extractor on demand.

The `extractor_audit` log records every invocation with
the key and outcome. Six outcome statuses are
wire-stable bytes (won't be renumbered):

| Status | Byte | Means |
|---|---|---|
| Success | 1 | Items produced (may be empty list). |
| Failure | 2 | Extractor errored. |
| SkippedBudget | 3 | LLM tier exceeded `cost_budget`. |
| SkippedFilter | 4 | Trigger `where` clause didn't fire. |
| SkippedDuplicate | 5 | Idempotency cache hit. |
| SkippedDisabled | 6 | Extractor was disabled at dispatch time. |

A failed extraction isn't a request failure. The encode
returns successfully; the audit row records the
extraction's outcome. Operators query the audit log to
see what went wrong and why.

---

## Putting it together: the promotion ladder

The composition isn't accidental. Pattern → classifier →
LLM, in increasing cost order, with each tier handling
what the cheaper ones miss:

```
encode(text):
    write to substrate (durable)
    │
    │  synchronous (microseconds):
    ├──▶ pattern extractors run
    │
    │  synchronous (milliseconds):
    ├──▶ classifier extractors run (if their triggers fire)
    │
    │  background (seconds, optional):
    └──▶ LLM extractors queued for batch processing
```

Synchronous tiers complete before `encode` returns. The LLM
tier runs in the background; its outputs appear in the
knowledge tables seconds to minutes later, but the encode
call itself doesn't pay for it.

The right design pattern for a domain:

1. **Start with patterns** for the high-precision stuff (IDs,
   well-known names).
2. **Add a classifier** when you need broader recall on
   something the pattern can't capture, and you have either
   training data or a pre-trained model.
3. **Add an LLM extractor** when the classifier isn't enough
   — for paraphrase-heavy types like Preferences, or for
   types the operator simply can't be bothered to train a
   classifier for.

Most production deployments end up with a handful of
pattern extractors, one or two classifier extractors, and
one or two LLM extractors. The bulk of the volume runs on
the cheap tiers; the LLM tier handles the long tail.

---

## Recap

- An extractor turns text into structured items: entity
  mentions, statement mentions, relation mentions.
- Three tiers: **pattern** (regex, free), **classifier**
  (small ML, cheap), **LLM** (large model, expensive).
- Patterns and classifiers are deterministic; LLM tier
  uses a per-shard cache to make repeats deterministic.
- The LLM tier has a per-call cost budget. JSON schema
  validation rejects malformed output with one retry.
- Missing model paths or API keys put extractors into
  degraded mode — the shard still boots; the audit log
  records loud failures.
- Every invocation writes an audit row with a wire-stable
  status byte. Operators query the audit log to see what
  happened.

---

## Where to go next

- **Where extracted items go:** [chapter 10](10-entities.md),
  [chapter 11](11-statements.md), [chapter 13](13-relations.md).
- **How types and extractors are declared:**
  [chapter 15](15-schemas.md) — schemas.
- **How idempotency and replay work at the system level:**
  [chapter 25](25-determinism-idempotency-replay.md).
- **The architecture-tier deep dive:**
  [`../architecture/10-extractors.md`](../architecture/10-extractors.md).
