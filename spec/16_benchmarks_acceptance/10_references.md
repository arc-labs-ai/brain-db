# 16.10 References

References for benchmarking and acceptance.

## 1. Foundational works

- **Jain, "The Art of Computer Systems Performance Analysis" (1991).** Comprehensive textbook on systems benchmarking.

- **Gregg, "Systems Performance" (2nd ed., 2020).** Modern systems performance.

- **Bondi, "Foundations of Software and System Performance Engineering" (2014).**

## 2. Latency measurement

- **Tene, "How NOT to Measure Latency" (talk).** [youtube.com/watch?v=lJ8ydIuPFeU](https://www.youtube.com/watch?v=lJ8ydIuPFeU). Required watching.

- **HdrHistogram** — [hdrhistogram.org](http://hdrhistogram.org/). The right way to measure latency distributions.

- **Coordinated Omission** — Discussed in Tene's talk; the most common benchmarking error.

## 3. Vector database benchmarks

- **ANN-Benchmarks** — [github.com/erikbern/ann-benchmarks](https://github.com/erikbern/ann-benchmarks). Standard ANN evaluation framework.

- **VectorDBBench** — [github.com/zilliztech/VectorDBBench](https://github.com/zilliztech/VectorDBBench). Vector DB comparisons.

- **MTEB** — [github.com/embeddings-benchmark/mteb](https://github.com/embeddings-benchmark/mteb). Massive Text Embedding Benchmark (focuses on embedding models, not stores).

## 4. Database benchmarks

- **TPC benchmarks** — [tpc.org](https://www.tpc.org/). Standard relational DB benchmarks.

- **YCSB** — [github.com/brianfrankcooper/YCSB](https://github.com/brianfrankcooper/YCSB). Yahoo! Cloud Serving Benchmark.

- **HammerDB** — [hammerdb.com](https://www.hammerdb.com/).

## 5. SRE / SLO references

- **Beyer et al., "Site Reliability Engineering" (Google SRE Book, 2016).** Chapter 4: Service Level Objectives. [sre.google/sre-book/service-level-objectives](https://sre.google/sre-book/service-level-objectives/).

- **The SLO checklist** — [sre.google/workbook/implementing-slos](https://sre.google/workbook/implementing-slos/).

## 6. Reproducibility

- **ACM Reproducibility** — [acm.org/publications/policies/artifact-review-and-badging-current](https://www.acm.org/publications/policies/artifact-review-and-badging-current).

## 7. HNSW and recall measurement

- **Malkov & Yashunin, "Efficient and robust approximate nearest neighbor search using Hierarchical Navigable Small World graphs" (2018).** [arxiv.org/abs/1603.09320](https://arxiv.org/abs/1603.09320).

- **Aumüller, Bernhardsson, Faithfull, "ANN-Benchmarks: A Benchmarking Tool for Approximate Nearest Neighbor Algorithms" (2018).** [arxiv.org/abs/1807.05614](https://arxiv.org/abs/1807.05614).

## 8. Acceptance testing

- **Beck, "Test-Driven Development: By Example" (2002).**

- **Pollice et al., "Software Architecture: Foundations, Theory, and Practice" (2009).** Chapter on architectural acceptance criteria.

## 9. Statistical analysis of benchmarks

- **Georges, Buytaert, Eeckhout, "Statistically Rigorous Java Performance Evaluation" (2007).** Methodology for proper performance comparison.

## 10. Brain-internal references

- See [01. System Architecture](../01_system_architecture/) for what's being benchmarked.
- See [09. Cognitive Operations](../09_cognitive_operations/) for the operation contracts.
- See [14. Observability + Operations](../14_observability_ops/) for in-production monitoring.
- See [15. Failure Modes + Recovery](../15_failure_recovery/) for chaos test scenarios.
