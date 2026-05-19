# Schema DSL — worked examples

Self-contained schemas you can upload to a running Brain via
`SchemaUploadReq (0x0120)` (or the SDK equivalent). Use these as
starting points; the
[grammar reference](grammar.md) is the authoritative shape.

## 1. Minimal — one entity, one predicate

The smallest schema that flips a deployment from substrate-only
mode into the knowledge layer.

```text
namespace minimal

define entity_type Person {
  attributes {
    name: text required
    email: text optional unique
  }
}

define predicate likes {
  kind: Preference
  object: Value<text>
}
```

After upload:
- ENCODE runs the (currently empty) extractor pipeline.
- RECALL routes through the hybrid query engine.
- The semantic / lexical / graph retrievers all consult the same
  empty knowledge layer until the first typed statement is
  stored.

A schema with **zero extractors** is valid — typed entities and
statements can still be created via the knowledge-layer opcodes
(`EntityCreateReq (0x0130)`, `StatementCreateReq (0x0140)`).

## 2. Multi-entity with a typed relation

```text
namespace company

define entity_type Person {
  attributes {
    full_name: text required
    email: text optional unique
    role: text optional
  }
}

define entity_type Project {
  attributes {
    code: text required unique
    title: text required
    started_at: date optional
  }
}

define relation_type contributes_to {
  from: Person
  to: Project
  cardinality: many-to-many
  properties {
    role_on_project: text optional
    since: date optional
  }
}

define relation_type reports_to {
  from: Person
  to: Person
  cardinality: many-to-one
}
```

Cardinality is enforced at write time. `contributes_to` allows
the same `(Person, Project)` pair multiple times only if a
property differs (e.g. role change over time). `reports_to`
allows at most one outgoing edge per `Person`.

## 3. Fact / Preference / Event

The three statement kinds, demonstrated side by side.

```text
namespace knowledge

define entity_type Person {
  attributes { name: text required }
}

define entity_type Topic {
  attributes { name: text required }
}

// FACT — objective, supersedeable
define predicate works_on {
  kind: Fact
  object: Entity<Topic>
}

// PREFERENCE — subjective; cannot reference entities or statements
define predicate prefers_topic {
  kind: Preference
  object: Value<text>
}

// EVENT — temporal; cannot reference statements
define predicate met_person {
  kind: Event
  object: Entity<Person>
}
```

Behavioural differences (spec §17/02):

| Kind | Auto-supersession | Allowed objects | Decay |
|---|---|---|---|
| `Fact` | Latest assertion wins; previous → `superseded` | `Value<…>`, `Entity<…>`, `Memory`, `Statement`, `Any` | Slow |
| `Preference` | Latest wins; never reference entities or statements | `Value<…>`, `Memory`, `Any` | Slow |
| `Event` | All asserted instances are retained | `Value<…>`, `Entity<…>`, `Memory`, `Any` | Time-decayed |

## 4. Pattern extractor — regex over memory text

```text
namespace newsroom

define entity_type Reporter {
  attributes { name: text required }
}

define entity_type Outlet {
  attributes { name: text required }
}

define extractor reporter_names {
  kind: pattern
  target: entity Reporter
  patterns [
    /(?:^|\s)([A-Z][a-z]+ [A-Z][a-z]+) reports?/
    /by ([A-Z][a-z]+ [A-Z][a-z]+)/
  ]
  confidence: 0.7
  trigger: on encode where memory.kind = episodic
}
```

`confidence: 0.7` is the **fixed** confidence the pattern
extractor will stamp on every entity it creates. The classifier
and LLM extractors use `confidence_threshold:` instead — they
compute confidence and drop below it.

## 5. Classifier extractor

```text
namespace classifier

define entity_type Topic {
  attributes { name: text required }
}

define extractor topic_classifier {
  kind: classifier
  target: entity Topic
  model: "bge-classifier-small-v1"
  feature_extraction: "mean_pool"
  confidence_threshold: 0.75
  trigger: on encode
  depends_on: []
}
```

Classifier extractors run after pattern extractors and can use
their output via `depends_on:`.

## 6. LLM extractor — full set of knobs

```text
namespace llm_extract

define entity_type Person { attributes { name: text required } }
define entity_type Project { attributes { code: text required } }

define predicate works_on {
  kind: Fact
  object: Entity<Project>
}

define extractor person_project_link {
  kind: llm
  target: statement Fact
  model: "gpt-4o-mini"
  prompt: """
    Extract any (Person, works_on, Project) triples implied by
    the text. Output strict JSON matching the provided schema.
  """
  examples: [
    {
      "text": "Alice has been leading APOLLO since March",
      "output": [{"subject": "Alice", "predicate": "works_on", "object": "APOLLO"}]
    }
  ]
  schema: {
    "type": "array",
    "items": {
      "type": "object",
      "properties": {
        "subject":   {"type": "string"},
        "predicate": {"const": "works_on"},
        "object":    {"type": "string"}
      },
      "required": ["subject", "predicate", "object"]
    }
  }
  cache: enabled
  cache_ttl: 7d
  confidence_threshold: 0.6
  cost_budget: "$0.001 per call"
  trigger: on encode where memory.kind = episodic
  depends_on: [person_mentions, project_mentions]
}
```

Notes:
- `cache: enabled` + `cache_ttl: 7d` caches by `(model, prompt_hash, text_hash)` for 7 days.
- `cost_budget:` is a soft cap — exceeding it triggers an alert; calls aren't blocked in v1.
- `depends_on:` ensures earlier extractor outputs are available before this one runs.

## 7. Schema with a complete extractor pipeline

```text
namespace meetings

define entity_type Person { attributes { name: text required } }

define predicate decided {
  kind: Fact
  object: Value<text>
}

define predicate scheduled {
  kind: Event
  object: Value<timestamp>
}

// Tier 1 — pattern
define extractor person_pattern {
  kind: pattern
  target: entity Person
  patterns [ /([A-Z][a-z]+ [A-Z][a-z]+)/ ]
  confidence: 0.6
  trigger: on encode
}

// Tier 2 — classifier disambiguates Person mentions
define extractor person_disambiguator {
  kind: classifier
  target: entity Person
  model: "person-disambig-v1"
  feature_extraction: "mean_pool"
  confidence_threshold: 0.7
  trigger: on encode
  depends_on: [person_pattern]
}

// Tier 3 — LLM extracts typed statements
define extractor meeting_decisions {
  kind: llm
  target: entity_or_statement
  model: "gpt-4o-mini"
  prompt: "Extract decisions and scheduled events from the text."
  schema: {}
  cache: enabled
  cache_ttl: 30d
  confidence_threshold: 0.6
  trigger: on encode where memory.kind = episodic
  depends_on: [person_disambiguator]
}
```

This is the canonical three-tier pattern — pattern catches the
obvious, classifier refines, LLM handles the long tail.

## Uploading

Via the wire protocol:

```rust
let req = SchemaUploadRequest {
    schema_document: include_str!("schema.brain").to_owned(),
    dry_run: false,
};
let resp = client.schema_upload(req).await?;
assert!(resp.validation_errors.is_empty());
println!("Schema {}/v{} accepted", resp.namespace, resp.schema_version);
```

Via the brain-cli (deferred — `schema upload` lands in
Phase 25+; until then, use the SDK).

## Validating before uploading

`SchemaValidateReq (0x0123)` runs the parser + validator without
persisting. Useful in CI.

```rust
let validate = client.schema_validate(req).await?;
if !validate.validation_errors.is_empty() {
    eprintln!("schema invalid:");
    for e in validate.validation_errors {
        eprintln!("  line {}: {}", e.line, e.message);
    }
    std::process::exit(1);
}
```

## See also

- [`grammar.md`](grammar.md) — formal grammar.
- [`../cognitive-operations/encode.md`](../cognitive-operations/encode.md) — what changes when a schema is active.
- [`../../concepts/three-statement-kinds.md`](../../concepts/three-statement-kinds.md) — Fact / Preference / Event semantics in depth.
- [`../../architecture/10-extractors.md`](../../architecture/10-extractors.md) — how the three tiers compose at runtime.

**Spec:** §21 (the entire section).
