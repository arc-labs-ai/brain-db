# 01.00 Purpose

This document defines the architecture of **Brain**, a system that provides persistent, queryable, structured memory and cognitive operations to AI agents. It is the foundational specification — every other document in this series builds on the abstractions, terminology, and component boundaries defined here.

## What this document covers

This document covers the *conceptual whole* of the system at a level of detail sufficient to guide all the detail specs that follow.

- The motivating problem: why agents need a substrate dedicated to cognition rather than reusing existing storage systems. ([`01_problem.md`](01_problem.md))
- Background and prerequisite concepts the architecture depends on: LLMs and context windows, vectors and embeddings, approximate nearest neighbor search, the vector database landscape, agent memory frameworks, async runtime designs, and Linux I/O primitives. ([`02_background.md`](02_background.md))
- The conceptual framework: the five cognitive primitives (`ENCODE`, `RECALL`, `PLAN`, `REASON`, `FORGET`) that an agent uses to interact with the substrate. ([`03_primitives.md`](03_primitives.md))
- The layered architecture: the seven components that implement those primitives, their boundaries, their dependencies, and the design constraints that shape them. ([`04_layers.md`](04_layers.md))
- The hardware envelope: what we assume about the deployment target and what we promise in return. ([`05_hardware.md`](05_hardware.md), [`06_targets.md`](06_targets.md))
- Non-goals: an explicit list of what Brain will not do, so that scope creep is detectable rather than gradual. ([`07_non_goals.md`](07_non_goals.md))
- Comparison with adjacent systems: where Brain sits in the landscape of databases, vector stores, graph databases, and agent memory frameworks. ([`08_comparison.md`](08_comparison.md))
- Cross-references to the 16 detail specs that follow.

## What this document does not cover

The architecture sets the stage for the detail specs but does not duplicate them.

- **Wire-format byte layouts.** Defined in [03. Wire Protocol](../03_wire_protocol/).
- **On-disk storage formats.** Defined in [05. Storage: Arena & WAL](../05_storage_arena_wal/) and [07. Metadata + Graph Store](../07_metadata_graph/).
- **Cognitive operation semantics in depth.** Defined in [09. Cognitive Operations](../09_cognitive_operations/).
- **Concurrency and epoch model.** Defined in [10. Concurrency + Epoch Model](../10_concurrency_epochs/).
- **Failure-mode procedures.** Defined in [15. Failure Modes + Recovery](../15_failure_recovery/).

## Audience

The reader is assumed to be a senior systems engineer or an ML engineer with systems background. We define terms when they first appear and provide further-reading links throughout.

A reader who has built distributed systems and used embedding models will find no entirely new ideas here — just an unusual *combination* of them, applied to a problem that has historically been solved with brittle ad-hoc scaffolding. The contribution is the synthesis, not any individual piece.

## Voice and conventions

This document mixes two voices:

- **First-person plural** ("we chose...", "we accept...") for rationale, design discussion, and trade-off analysis.
- **Third-person normative** ("the server MUST...", "implementations SHOULD...") for requirements that bind implementations.

Where requirements appear, they follow [RFC 2119 conventions](https://www.rfc-editor.org/rfc/rfc2119): MUST, MUST NOT, SHOULD, SHOULD NOT, MAY.

Code identifiers are in `monospace`. Concept names are in **bold** when first introduced. Cross-references to other documents in the spec series use the form *NN. Document Title*. Cross-references within this document use file names like [`02_background.md`](02_background.md).

## Document version

This is format version 1 of the architecture spec. See [`../00_master_overview/03_versioning.md`](../00_master_overview/) for how spec documents are versioned over time.

---

*Continue to [`01_problem.md`](01_problem.md) for the motivating problem.*
