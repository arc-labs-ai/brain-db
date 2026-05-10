# 09.13 References

References for the cognitive operations layer.

## 1. Cognitive primitives in retrieval-augmented systems

- **Lewis et al., "Retrieval-Augmented Generation for Knowledge-Intensive NLP Tasks" (2020).** [arXiv:2005.11401](https://arxiv.org/abs/2005.11401). RAG framework; informs the RECALL design.

- **Borgeaud et al., "Improving language models by retrieving from trillions of tokens" (RETRO, 2022).** [arXiv:2112.04426](https://arxiv.org/abs/2112.04426). At-scale retrieval-augmentation.

## 2. Memory in cognitive science

- **Tulving, "Episodic and Semantic Memory" (1972).** A foundational distinction; informs Brain's MemoryKind enum.

- **Squire, "Memory systems of the brain: a brief history and current perspective" (2004).** Overview of memory taxonomies; not directly applicable but useful framing.

## 3. Graph traversal algorithms

- **Pohl, "Bi-directional and heuristic search in path problems" (1969).** Bidirectional BFS used in PLAN.

- **Russell & Norvig, "Artificial Intelligence: A Modern Approach" (4th ed., 2020).** Standard reference for search algorithms.

## 4. Argumentation and reasoning

- **Walton, "Argumentation Schemes" (2008).** A framework for structured argument analysis. Brain's REASON is a much simpler version of evidence aggregation.

- **Dung, "On the acceptability of arguments and its fundamental role in nonmonotonic reasoning, logic programming and n-person games" (1995).** Argumentation frameworks; far more sophisticated than what Brain does.

## 5. Eventual consistency and isolation models

- **Vogels, "Eventually Consistent" (2009).** [queue.acm.org/detail.cfm?id=1466448](https://queue.acm.org/detail.cfm?id=1466448). Survey of consistency models.

- **Berenson et al., "A Critique of ANSI SQL Isolation Levels" (1995).** Classic on isolation level definitions.

## 6. Idempotency in distributed systems

- **Helland, "Idempotence is Not a Medical Condition" (2012).** [queue.acm.org/detail.cfm?id=2187821](https://queue.acm.org/detail.cfm?id=2187821).

- **Stripe's idempotency keys** — [stripe.com/docs/api/idempotent_requests](https://stripe.com/docs/api/idempotent_requests).

## 7. Subscription and change-data-capture systems

- **Debezium documentation** — [debezium.io](https://debezium.io/). CDC patterns relevant to SUBSCRIBE.

- **Apache Kafka** — [kafka.apache.org](https://kafka.apache.org/). The streaming reference. Brain's SUBSCRIBE is much more constrained but borrows ideas.

## 8. Vector similarity in retrieval

- **Mikolov et al., "Distributed Representations of Words and Phrases and their Compositionality" (2013).** [arXiv:1310.4546](https://arxiv.org/abs/1310.4546). Word embeddings; cosine similarity.

- **Reimers & Gurevych, "Sentence-BERT" (2019).** [arXiv:1908.10084](https://arxiv.org/abs/1908.10084). Sentence embeddings.

## 9. Saga pattern

- **Garcia-Molina & Salem, "Sagas" (1987).** [dl.acm.org/doi/10.1145/38713.38742](https://dl.acm.org/doi/10.1145/38713.38742). The original sagas paper, relevant for cross-shard alternatives.

- **Microservices Patterns: Saga** — [microservices.io/patterns/data/saga](https://microservices.io/patterns/data/saga.html). Modern usage.

## 10. Adjacent reading

- **Kleppmann, "Designing Data-Intensive Applications" (2017).** O'Reilly. Foundational for the consistency, transaction, and streaming concepts.

- **Brewer, "CAP Twelve Years Later: How the 'Rules' Have Changed" (2012).** [computer.org/csdl/magazine/co/2012/02/mco2012020023](https://www.computer.org/csdl/magazine/co/2012/02/mco2012020023). On consistency trade-offs.

## 11. Brain-internal references

- See [02. Data Model](../02_data_model/) for data structures referenced.
- See [05. Storage](../05_storage_arena_wal/) for the durability primitives.
- See [08. Query Planner](../08_query_planner/) for execution details.
- See [13. SDK Design](../13_sdk_design/) for the client-side abstractions of these primitives.
