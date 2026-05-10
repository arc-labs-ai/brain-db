# 10.10 References

References for the concurrency model.

## 1. Core Rust async runtimes

- **Glommio** — thread-per-core async runtime. [GitHub: DataDog/glommio](https://github.com/DataDog/glommio).

- **Tokio** — general-purpose async runtime. [GitHub: tokio-rs/tokio](https://github.com/tokio-rs/tokio). For comparison.

## 2. Lock-free programming

- **`crossbeam-epoch`** — epoch-based reclamation. [GitHub: crossbeam-rs/crossbeam](https://github.com/crossbeam-rs/crossbeam).

- **`arc-swap`** — atomic Arc swap. [GitHub: vorner/arc-swap](https://github.com/vorner/arc-swap).

- **Herlihy, Shavit, "The Art of Multiprocessor Programming" (2nd ed., 2020).** Morgan Kaufmann. The standard textbook on concurrent algorithms.

## 3. Epoch-based reclamation

- **Fraser, "Practical lock freedom" (PhD thesis, Cambridge, 2004).** Introduced epoch-based memory management.

- **McKenney, "Is Parallel Programming Hard, And, If So, What Can You Do About It?" (book).** [kernel.org/pub/linux/kernel/people/paulmck/perfbook/perfbook.html](https://www.kernel.org/pub/linux/kernel/people/paulmck/perfbook/perfbook.html). Comprehensive reference for kernel-style synchronization including RCU.

## 4. The thread-per-core architecture

- **Seastar** — C++ thread-per-core framework. [seastar.io](http://seastar.io/). The inspiration for Glommio.

- **ScyllaDB on Seastar** — engineering posts on [scylladb.com/blog](https://www.scylladb.com/blog/).

- **"The cost of context switching"** — Aleksey Charapko's blog. [charap.co/the-cost-of-context-switching](https://charap.co/the-cost-of-context-switching/).

## 5. Cooperative scheduling

- **Adya et al., "Cooperative task management without manual stack management" (USENIX 2002).** Foundational paper on cooperative scheduling for servers.

- **Glommio's task model documentation** — in the project's README and docs.

## 6. NUMA awareness

- **Lameter, "NUMA (Non-Uniform Memory Access): An Overview" (2013, ACM Queue).** [queue.acm.org/detail.cfm?id=2513149](https://queue.acm.org/detail.cfm?id=2513149).

- **Linux numactl manual** — [man7.org/linux/man-pages/man8/numactl.8.html](https://man7.org/linux/man-pages/man8/numactl.8.html).

## 7. Linearizability and consistency

- **Herlihy & Wing, "Linearizability: A Correctness Condition for Concurrent Objects" (1990).** [doi.org/10.1145/78969.78972](https://doi.org/10.1145/78969.78972). The canonical paper on linearizability.

- **Bailis & Ghodsi, "Eventual Consistency Today: Limitations, Extensions, and Beyond" (CACM 2013).** [cacm.acm.org/magazines/2013/5/163755](https://cacm.acm.org/magazines/2013/5/163755).

## 8. MVCC

- **Bernstein, Hadzilacos, Goodman, "Concurrency Control and Recovery in Database Systems" (1987).** Free PDF: [microsoft.com/en-us/research/people/philbe](https://www.microsoft.com/en-us/research/people/philbe/). The classic textbook.

## 9. Adjacent reading

- **Kleppmann, "Designing Data-Intensive Applications" (2017).** O'Reilly. Chapter 7 on transactions; chapter 9 on consistency.

- **Petrov, "Database Internals" (2019).** O'Reilly. Covers concurrency primitives in modern databases.

## 10. Brain-internal references

- See [05.11 Concurrency](../05_storage_arena_wal/11_concurrency.md) for the storage layer's concurrency.
- See [06.08 Concurrency](../06_ann_index/08_concurrency.md) for HNSW concurrency.
- See [07.09 Concurrency](../07_metadata_graph/09_concurrency.md) for metadata-store concurrency.
- See [08.09 Concurrency](../08_query_planner/09_concurrency.md) for query-planner concurrency.
