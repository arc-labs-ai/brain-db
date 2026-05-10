# 08.12 References

References for the query planner and execution engine.

## 1. Database query planning

- **Selinger et al., "Access Path Selection in a Relational Database Management System" (1979).** [dl.acm.org/doi/10.1145/582095.582099](https://dl.acm.org/doi/10.1145/582095.582099). The seminal cost-based query planning paper.

- **Hellerstein, Stonebraker, Hamilton, "Architecture of a Database System" (2007).** [db.cs.berkeley.edu/papers/fntdb07-architecture.pdf](http://db.cs.berkeley.edu/papers/fntdb07-architecture.pdf). Comprehensive survey including query optimization.

## 2. Async runtimes for high-throughput servers

- **Glommio** — thread-per-core async runtime. [GitHub: DataDog/glommio](https://github.com/DataDog/glommio). Brain's runtime.

- **Tokio** — general-purpose async runtime. [GitHub: tokio-rs/tokio](https://github.com/tokio-rs/tokio). Considered as alternative.

- **The thread-per-core architecture: a primer** — Aleksey Charapko's blog. [charap.co/the-cost-of-context-switching](https://charap.co/the-cost-of-context-switching/).

## 3. Cooperative scheduling

- **Glommio's scheduler design** — covered in [glommio.io](https://www.datadoghq.com/blog/engineering/introducing-glommio/) and the project README.

- **The "thread-per-core" pattern** — in databases like ScyllaDB and Seastar. [seastar.io](http://seastar.io/).

## 4. Backpressure and flow control

- **Reactive Streams specification** — [reactive-streams.org](https://www.reactive-streams.org/). The async backpressure standard.

- **Tokio's backpressure docs** — [tokio.rs/tokio/topics/bridging](https://tokio.rs/tokio/topics/bridging). Concepts also relevant to Glommio.

## 5. Bidirectional BFS

- **Russell & Norvig, "Artificial Intelligence: A Modern Approach" (4th ed., 2020).** Chapter on uninformed search. The standard textbook reference for bidirectional BFS.

- **Pohl, "Bi-directional and heuristic search in path problems" (1969).** The original bidirectional-search paper.

## 6. Approximate-nearest-neighbor cost models

- **Wang et al., "A Comprehensive Survey and Experimental Comparison of Graph-Based Approximate Nearest Neighbor Search" (2021).** [arXiv:2101.12631](https://arxiv.org/abs/2101.12631). Cost-vs-recall trade-offs.

- **ann-benchmarks** — empirical measurements. [GitHub: erikbern/ann-benchmarks](https://github.com/erikbern/ann-benchmarks).

## 7. Cross-shard query patterns

- **Ports & Lin, "Designing distributed key-value stores: chain replication" (2014).** [research.cs.cornell.edu/chain-replication](https://www.cs.cornell.edu/home/rvr/papers/OSDI04.pdf).

- **Dynamo: Amazon's Highly Available Key-value Store (2007).** [allthingsdistributed.com/files/amazon-dynamo-sosp2007.pdf](https://www.allthingsdistributed.com/files/amazon-dynamo-sosp2007.pdf). Discusses cross-shard concerns.

## 8. Idempotency in API design

- **Stripe's idempotency keys** — [stripe.com/docs/api/idempotent_requests](https://stripe.com/docs/api/idempotent_requests). Reference implementation.

- **AWS API design — idempotency** — [aws.amazon.com/builders-library](https://aws.amazon.com/builders-library/).

## 9. Adjacent reading

- **Kleppmann, "Designing Data-Intensive Applications" (2017).** O'Reilly.

- **CMU's intro database course** — [15445.courses.cs.cmu.edu](https://15445.courses.cs.cmu.edu/). Lectures cover query execution and planning.

## 10. Brain-internal references

- See [03. Wire Protocol](../03_wire_protocol/) for request/response framing.
- See [04. Embedding Layer](../04_embedding_layer/) for embedding.
- See [05. Storage: Arena & WAL](../05_storage_arena_wal/) for the durability path.
- See [06. ANN Index](../06_ann_index/) for HNSW.
- See [07. Metadata + Graph Store](../07_metadata_graph/) for the metadata side.
