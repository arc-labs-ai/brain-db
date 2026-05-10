# 07.12 References

References for the metadata + graph store.

## 1. The redb engine

- **redb** — pure-Rust embedded ACID key-value store. [GitHub: cberner/redb](https://github.com/cberner/redb).

  - Design notes: [redb design.md](https://github.com/cberner/redb/blob/master/docs/design.md).
  - Documentation: [docs.rs/redb](https://docs.rs/redb).

## 2. Alternative engines (considered, not chosen)

- **RocksDB** — LSM-tree-based embedded store. [GitHub: facebook/rocksdb](https://github.com/facebook/rocksdb).
- **sled** — pure-Rust embedded database. [GitHub: spacejam/sled](https://github.com/spacejam/sled). Maintenance-paused.
- **SQLite** — ubiquitous embedded SQL database. [sqlite.org](https://www.sqlite.org/).
- **LMDB** — mmap-based embedded KV store. [Symas LMDB](https://www.symas.com/lmdb).
- **fjall** — newer pure-Rust LSM-tree library. [GitHub: fjall-rs/fjall](https://github.com/fjall-rs/fjall).

## 3. ACID and MVCC concepts

- **Bernstein, Hadzilacos, Goodman, "Concurrency Control and Recovery in Database Systems" (1987).** Free PDF: [research.microsoft.com/en-us/people/philbe/CCAndR.aspx](https://www.microsoft.com/en-us/research/people/philbe/). The classic textbook on transaction concurrency.

- **Hellerstein, Stonebraker, Hamilton, "Architecture of a Database System" (2007).** [db.cs.berkeley.edu/papers/fntdb07-architecture.pdf](http://db.cs.berkeley.edu/papers/fntdb07-architecture.pdf). Comprehensive overview of database engine architecture.

- **Wikipedia: ACID** — [en.wikipedia.org/wiki/ACID](https://en.wikipedia.org/wiki/ACID).

- **Wikipedia: Multiversion concurrency control** — [en.wikipedia.org/wiki/Multiversion_concurrency_control](https://en.wikipedia.org/wiki/Multiversion_concurrency_control).

## 4. The B-tree

- **Bayer & McCreight, "Organization and Maintenance of Large Ordered Indices" (1972).** The original B-tree paper.

- **Comer, "The Ubiquitous B-Tree" (1979).** [doi.org/10.1145/356770.356776](https://doi.org/10.1145/356770.356776). A survey.

## 5. UUIDv7

- **RFC 9562** — "Universally Unique IDentifiers (UUIDs)". [datatracker.ietf.org/doc/html/rfc9562](https://datatracker.ietf.org/doc/html/rfc9562). Defines UUIDv7.

- **`uuid` crate** — Rust UUID library. [GitHub: uuid-rs/uuid](https://github.com/uuid-rs/uuid).

## 6. Idempotency

- **AWS API design guide on idempotency** — [aws.amazon.com/builders-library](https://aws.amazon.com/builders-library/). Discusses request idempotency in distributed systems.

- **Stripe's idempotency keys** — [stripe.com/docs/api/idempotent_requests](https://stripe.com/docs/api/idempotent_requests). A widely-cited example.

## 7. Graph-storage patterns

- **Neo4j storage architecture documentation** — [neo4j.com/docs/operations-manual](https://neo4j.com/docs/operations-manual/current/). For comparison.

- **TigerGraph engineering blog** — [tigergraph.com/blog](https://www.tigergraph.com/blog/). Engineering notes on graph storage at scale.

## 8. The `rkyv` library

- **`rkyv`** — zero-copy deserialization for Rust. [GitHub: rkyv/rkyv](https://github.com/rkyv/rkyv). Used for value encoding.

## 9. Adjacent reading

- **Pavlo, Aslett, Andersen, "What Goes Around Comes Around... And Around..." (2024).** [db.cs.cmu.edu/papers/2024/whatgoesaround-sigmodrec2024.pdf](https://db.cs.cmu.edu/papers/2024/whatgoesaround-sigmodrec2024.pdf). Survey of database trends; informs the choice of B-tree vs LSM.

- **Kleppmann, "Designing Data-Intensive Applications" (2017).** O'Reilly. The most accessible modern textbook on distributed data systems.
