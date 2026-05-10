# 06.12 References

References for the ANN index.

## 1. The HNSW algorithm

- **Malkov & Yashunin, "Efficient and robust approximate nearest neighbor search using Hierarchical Navigable Small World graphs" (2016, IEEE TPAMI 2018).** [arXiv:1603.09320](https://arxiv.org/abs/1603.09320). The original HNSW paper and authoritative reference.

## 2. The Rust crate

- **`hnsw_rs`** (also published as `hnswlib-rs`). [GitHub: jean-pierreBoth/hnswlib-rs](https://github.com/jean-pierreBoth/hnswlib-rs). The Rust HNSW library Brain uses.

  - Documentation in the repo's README and on [docs.rs/hnsw_rs](https://docs.rs/hnsw_rs).

## 3. Reference C++ implementation

- **`hnswlib`** — the original C++ HNSW library by the paper's authors. [GitHub: nmslib/hnswlib](https://github.com/nmslib/hnswlib). The Rust crate is loosely modeled on it.

## 4. Concurrency primitives

- **`crossbeam-epoch`** — epoch-based reclamation. [GitHub: crossbeam-rs/crossbeam](https://github.com/crossbeam-rs/crossbeam) (specifically the `crossbeam-epoch` crate).

- **`arc-swap`** — atomic swap of `Arc<T>`. [GitHub: vorner/arc-swap](https://github.com/vorner/arc-swap).

## 5. Comparison with alternatives

- **FAISS** (Facebook AI Similarity Search). [GitHub: facebookresearch/faiss](https://github.com/facebookresearch/faiss). The reference for ANN libraries; HNSW is one of many index types FAISS supports.

- **Annoy** — Spotify's index. [GitHub: spotify/annoy](https://github.com/spotify/annoy). Random forest of trees; simpler than HNSW.

- **DiskANN** — Microsoft's disk-resident ANN. [GitHub: microsoft/DiskANN](https://github.com/microsoft/DiskANN). For indexes that don't fit in RAM.

- **ScaNN** — Google's ANN. [GitHub: google-research/google-research/scann](https://github.com/google-research/google-research/tree/master/scann). High-quality, complex.

## 6. Ann-benchmarks

- **ann-benchmarks** — standardized benchmarking for ANN libraries. [GitHub: erikbern/ann-benchmarks](https://github.com/erikbern/ann-benchmarks). Brain's HNSW parameters are calibrated against the recall/latency curves visible there.

## 7. Theoretical background

- **Small-world networks.** Watts & Strogatz, 1998. ["Collective dynamics of 'small-world' networks"](https://www.nature.com/articles/30918) (Nature). The original notion of "small world" graphs.

- **Skip lists.** Pugh, 1990. ["Skip Lists: A Probabilistic Alternative to Balanced Trees"](https://dl.acm.org/doi/10.1145/78973.78977). The hierarchical structure HNSW echoes.

## 8. Linear algebra acceleration

- **`matrixmultiply`** — fast matrix-matrix multiplication. [GitHub: bluss/matrixmultiply](https://github.com/bluss/matrixmultiply). Used for batched distance computations.

- **`wide`** — portable SIMD wrappers. [GitHub: Lokathor/wide](https://github.com/Lokathor/wide). Used for fast dot products in the cosine distance kernel.

- **AVX2 / AVX-512** — Intel's SIMD ISAs. [Intel Intrinsics Guide](https://www.intel.com/content/www/us/en/docs/intrinsics-guide/index.html).

- **NEON** — ARM's SIMD ISA. [Arm Architecture Reference Manual](https://developer.arm.com/architectures/cpu-architecture/a-profile/docs).

## 9. Adjacent reading

- **Wang et al., "A Comprehensive Survey and Experimental Comparison of Graph-Based Approximate Nearest Neighbor Search" (2021)**. [arXiv:2101.12631](https://arxiv.org/abs/2101.12631). Compares HNSW with newer graph-based approaches.

- **Bernhardsson, "Annoy benchmarks"** — practical performance comparisons for ANN libraries. [erikbern.com/2018/06/17/new-benchmarks-for-approximate-nearest-neighbors](https://erikbern.com/2018/06/17/new-benchmarks-for-approximate-nearest-neighbors).
