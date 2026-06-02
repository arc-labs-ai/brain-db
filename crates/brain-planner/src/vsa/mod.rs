//! Vector Symbolic Architecture — HRR algebra over real-valued vectors.
//!
//! Three ops in three lines:
//!
//! - **bind(a, b)** — circular convolution `a ⊛ b`. Commutative,
//!   associative, distributes over bundling. Used to bind a role to
//!   a filler ("subject role ⊛ Alice").
//! - **bundle(v1, …, vk)** — element-wise sum followed by L2
//!   normalize. The HRR analog of set union; the bundle is similar
//!   to every operand (cos ~ 1/sqrt(k)).
//! - **unbind(c, a)** — circular correlation `c ⊛ a⁻¹` where
//!   `a⁻¹[k] = a[(n−k) mod n]`. The inverse of bind: if
//!   `c = bind(a, b)` then `unbind(c, a) ≈ b`. The result is noisy
//!   when `c` was bundled, so the caller snaps it to a known
//!   vocabulary via [`Codebook::cleanup`].
//!
//! Together these let us represent structured data ("Alice
//! works_at Acme") as a single fixed-dim vector that supports
//! retrieval of any one component given the others. The smoke test
//! in [`analogy`] encodes two such triples and recovers "Stripe"
//! from "Bob works_at ?" with cosine > 0.5.
//!
//! v1.0 ships this module standalone — PLAN/REASON cognitive ops
//! still take the graph-traversal path; wiring HRR into them is a
//! v1.1 follow-up. The shapes here (vector type, codebook surface,
//! analogy entry point) are stable so that wiring is purely additive.

pub mod analogy;
pub mod codebook;
pub mod errors;
pub mod fft;
pub mod ops;
pub mod semantic_centroid;

pub use analogy::{
    analogy_query, encode_triple, query_role, ROLE_OBJECT, ROLE_PREDICATE, ROLE_SUBJECT,
};
pub use codebook::Codebook;
pub use errors::VsaError;
pub use ops::{bind, bundle, cosine, normalize, random_vec, unbind, VsaVec, VSA_DIM};
pub use semantic_centroid::{cosine_to_centroid, semantic_centroid};
