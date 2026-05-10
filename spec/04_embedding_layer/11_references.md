# 04.11 References

References for the embedding layer.

## 1. The model

- **`bge-small-en-v1.5`** — model card on HuggingFace. [huggingface.co/BAAI/bge-small-en-v1.5](https://huggingface.co/BAAI/bge-small-en-v1.5).

- **FlagEmbedding** — BAAI's model family and training repository. [GitHub: FlagOpen/FlagEmbedding](https://github.com/FlagOpen/FlagEmbedding).

- **MTEB benchmark** — Massive Text Embedding Benchmark, used to evaluate embedding models. [GitHub: embeddings-benchmark/mteb](https://github.com/embeddings-benchmark/mteb).

## 2. Inference framework

- **candle** — the Rust ML framework. [GitHub: huggingface/candle](https://github.com/huggingface/candle).

- **safetensors** — the weight file format. [GitHub: huggingface/safetensors](https://github.com/huggingface/safetensors).

## 3. Tokenization

- **HuggingFace `tokenizers`** — the tokenization library. [GitHub: huggingface/tokenizers](https://github.com/huggingface/tokenizers).

- **WordPiece** — the tokenization algorithm. Wu et al., 2016. ["Google's Neural Machine Translation System: Bridging the Gap between Human and Machine Translation"](https://arxiv.org/abs/1609.08144).

- **BERT** — the model architecture lineage `bge-small-en-v1.5` derives from. Devlin et al., 2018. ["BERT: Pre-training of Deep Bidirectional Transformers for Language Understanding"](https://arxiv.org/abs/1810.04805).

## 4. Hashing

- **BLAKE3** — fingerprint hashing. [GitHub: BLAKE3-team/BLAKE3](https://github.com/BLAKE3-team/BLAKE3).

## 5. Caching

- **`lru` crate** — Rust LRU cache. [GitHub: jeromefroe/lru-rs](https://github.com/jeromefroe/lru-rs).

## 6. Linear algebra

- **`matrixmultiply`** — fast matrix-matrix multiplication. [GitHub: bluss/matrixmultiply](https://github.com/bluss/matrixmultiply).

- **`wide`** — portable SIMD wrappers. [GitHub: Lokathor/wide](https://github.com/Lokathor/wide).

- **Intel MKL** — high-performance BLAS for x86. [intel.com/content/www/us/en/developer/tools/oneapi/onemkl.html](https://www.intel.com/content/www/us/en/developer/tools/oneapi/onemkl.html).

## 7. Background reading

- **Reimers & Gurevych, "Sentence-BERT: Sentence Embeddings using Siamese BERT-Networks" (2019)** — foundational paper for BERT-style sentence embeddings. [arXiv:1908.10084](https://arxiv.org/abs/1908.10084).

- **The MTEB paper** — Muennighoff et al., 2023. ["MTEB: Massive Text Embedding Benchmark"](https://arxiv.org/abs/2210.07316). Methodology for evaluating embedding models.

- **The "Lost in the Middle" paper** — Liu et al., 2023. [arXiv:2307.03172](https://arxiv.org/abs/2307.03172). Background on why retrieval matters even with long-context models.

- **Continuous Hopfield Networks** — Ramsauer et al., 2020. ["Hopfield Networks Is All You Need"](https://arxiv.org/abs/2008.02217). Background for the attractor dynamics that use these embeddings.
