# 01. System Architecture

> **Brain — A Cognitive Substrate for AI Agents**
> Specification document, format version 1.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Senior systems engineers, ML engineers building agent systems |
| Voice | Hybrid (rationale + normative MUST/SHOULD where applicable) |
| Depends on | (none — this is the foundational document) |
| Referenced by | All other specs |

## What this spec defines

The architecture of **Brain**, a system that provides persistent, queryable, structured memory and cognitive operations to AI agents. This is the foundational specification — every other document in this series builds on the abstractions, terminology, and component boundaries defined here.

## Reading order

The files in this directory are numbered. Read them in order for a top-to-bottom understanding of the architecture, or jump directly to the section you need.

| File | Topic |
|---|---|
| [`00_purpose.md`](00_purpose.md) | What this document covers and doesn't |
| [`01_problem.md`](01_problem.md) | Why agents need a dedicated cognitive substrate |
| [`02_background.md`](02_background.md) | Prerequisite concepts: LLMs, vectors, ANN, async runtimes, Linux I/O |
| [`03_primitives.md`](03_primitives.md) | The five cognitive primitives plus supporting operations |
| [`04_layers.md`](04_layers.md) | The seven architectural layers and their boundaries |
| [`05_hardware.md`](05_hardware.md) | Hardware assumptions: OS, CPU, memory, storage, network |
| [`06_targets.md`](06_targets.md) | Capacity targets and scaling envelope |
| [`07_non_goals.md`](07_non_goals.md) | Explicit non-goals — what Brain will not do |
| [`08_comparison.md`](08_comparison.md) | Comparison with adjacent systems |
| [`09_glossary.md`](09_glossary.md) | Vocabulary used throughout the spec series |
| [`10_open_questions.md`](10_open_questions.md) | Unresolved architectural questions |
| [`11_references.md`](11_references.md) | References and further reading |

## How this spec relates to others

This spec defines the *what* — the entities, layers, and boundaries. Detail specs (02 through 16) define the *how*: byte layouts, algorithms, protocols, operational procedures.

If a detail spec contradicts this one, the detail spec is wrong unless explicitly amending this one.

## Audience expectations

The reader is assumed to know:

- Rust at production-engineering level.
- Async I/O concepts (futures, executors, backpressure).
- Basic distributed-systems vocabulary (replication, consistency models, sharding).
- Basic ML vocabulary (embeddings, similarity, transformers).
- Linux as a deployment target.

We do not assume the reader has built an ANN index, a wire protocol, or an agent system before. Where these are needed for the architecture to make sense, [`02_background.md`](02_background.md) provides the missing context.

---

*See [`00_purpose.md`](00_purpose.md) to begin.*
