# 12.10 References

References for sharding and clustering.

## 1. Distributed-database literature

- **Dynamo: Amazon's Highly Available Key-value Store (SOSP 2007).** [allthingsdistributed.com/files/amazon-dynamo-sosp2007.pdf](https://www.allthingsdistributed.com/files/amazon-dynamo-sosp2007.pdf). Foundational paper on sharded distributed stores.

- **Bigtable: A Distributed Storage System for Structured Data (OSDI 2006).** [research.google/pubs/bigtable](https://research.google/pubs/bigtable-a-distributed-storage-system-for-structured-data/). Sharding via tablet model.

- **Spanner: Google's Globally-Distributed Database (OSDI 2012).** [research.google/pubs/spanner](https://research.google/pubs/spanner-googles-globally-distributed-database-2/). For the geo-distributed transaction story.

## 2. Consensus and membership

- **Ongaro & Ousterhout, "In Search of an Understandable Consensus Algorithm" (Raft, USENIX 2014).** [raft.github.io/raft.pdf](https://raft.github.io/raft.pdf). The Raft paper.

- **`raft-rs`** — Rust Raft implementation. [GitHub: tikv/raft-rs](https://github.com/tikv/raft-rs).

- **`openraft`** — Another Rust Raft library. [GitHub: datafuselabs/openraft](https://github.com/datafuselabs/openraft).

## 3. Consistent hashing

- **Karger et al., "Consistent Hashing and Random Trees" (1997).** [doi.org/10.1145/258533.258660](https://doi.org/10.1145/258533.258660). The original consistent-hashing paper.

- **Lamping & Veach, "A Fast, Minimal Memory, Consistent Hash Algorithm" (Jump Hash, 2014).** [arxiv.org/abs/1406.2294](https://arxiv.org/abs/1406.2294). A simple alternative.

- **Rendezvous hashing** — Wikipedia: [en.wikipedia.org/wiki/Rendezvous_hashing](https://en.wikipedia.org/wiki/Rendezvous_hashing).

## 4. Replication protocols

- **Chain Replication for Supporting High Throughput and Availability (van Renesse, Schneider; OSDI 2004).** [cs.cornell.edu/home/rvr/papers/OSDI04.pdf](https://www.cs.cornell.edu/home/rvr/papers/OSDI04.pdf).

- **Paxos Made Simple (Lamport, 2001).** [lamport.azurewebsites.net/pubs/paxos-simple.pdf](https://lamport.azurewebsites.net/pubs/paxos-simple.pdf). For comparison with Raft.

## 5. Cluster systems in production

- **Cassandra documentation** — [cassandra.apache.org/doc/latest](https://cassandra.apache.org/doc/latest/). Authoritative reference for sharded eventual-consistent stores.

- **CockroachDB engineering blog** — [cockroachlabs.com/blog](https://www.cockroachlabs.com/blog/). For range-based sharding ideas.

- **TiDB / TiKV documentation** — [tikv.org/docs](https://tikv.org/docs/). Multi-Raft architecture.

- **ScyllaDB documentation** — [scylladb.com/docs](https://docs.scylladb.com/). Thread-per-core distributed database.

## 6. Hashing functions

- **BLAKE3** — [github.com/BLAKE3-team/BLAKE3](https://github.com/BLAKE3-team/BLAKE3). Brain's choice for routing.

- **xxHash** — [github.com/Cyan4973/xxHash](https://github.com/Cyan4973/xxHash). Fast non-cryptographic alternative.

## 7. CAP / PACELC

- **Brewer, "CAP twelve years later: How the 'rules' have changed" (2012).** [computer.org/csdl/magazine/co/2012/02/mco2012020023](https://www.computer.org/csdl/magazine/co/2012/02/mco2012020023/13rRUxYIN3z). Brewer's reflection on CAP.

- **Abadi, "Consistency Tradeoffs in Modern Distributed Database System Design" (2012, Computer).** Introduced PACELC.

## 8. Adjacent reading

- **Kleppmann, "Designing Data-Intensive Applications" (2017).** Chapters 5-9 cover replication, partitioning, transactions, consistency.

- **Petrov, "Database Internals" (2019).** Chapters on distributed systems fundamentals.

## 9. Brain-internal references

- See [01.04 Sharding](../01_system_architecture/04_sharding.md) for the overview.
- See [02.03 Identifiers](../02_data_model/03_identifiers.md) for the MemoryId encoding.
- See [03. Wire Protocol](../03_wire_protocol/) for the cross-node call mechanism.
- See [05.10 Snapshots](../05_storage_arena_wal/10_snapshots.md) for the per-shard snapshot story.
