# Schema DSL reference

**Audience:** anyone declaring a schema to switch Brain from pure
substrate mode into the knowledge layer.

**Goal:** *exact grammar* of the schema DSL. Not "should I declare
a schema" (see [`../../concepts/substrate-vs-knowledge.md`](../../concepts/substrate-vs-knowledge.md));
not "how do I extract typed statements" (see
[`../../architecture/10-extractors.md`](../../architecture/10-extractors.md)).

## Pages

| Page | Covers |
|---|---|
| [`grammar.md`](grammar.md) | Formal grammar, every token, every production |
| [`examples.md`](examples.md) | Worked schemas — minimal, multi-entity, with relations |

## In one paragraph

A schema declares **entity types** (e.g. `Person`, `Project`),
**predicates** (typed relations between entities), and the
**statement kinds** Brain should expect (Fact, Preference, Event).
The schema is uploaded via the `SCHEMA_UPLOAD` opcode once at
deployment time; from that point on, ENCODE runs through the
three-tier extractor pipeline and writes typed statements alongside
the vector memory.

A deployment that never uploads a schema runs in **substrate-only
mode** — a first-class deployment posture, not a degraded one.

## See also

- [`../../../spec/21_schema_dsl/`](../../../spec/21_schema_dsl/)
  — authoritative grammar.
- [`../../guides/sdk/typed-knowledge.md`](../../guides/sdk/typed-knowledge.md)
  — how to map your domain types via the Rust derive macros.
