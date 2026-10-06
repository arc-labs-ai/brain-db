//! Admin HTTP handlers for `schema`.
//!
//! Routes:
//! - `GET /v1/schema/review[?shard=N]` → the predicate review queue: the
//!   coined predicate names worth promoting into a `SCHEMA_UPLOAD`.

mod review;

pub use review::review;
