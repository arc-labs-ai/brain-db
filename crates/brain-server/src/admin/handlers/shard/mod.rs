//! Admin HTTP handlers for `shard`.
//!
//! Routes:
//! - `GET /v1/shards` → 200 + `{"shards":[{"index":N,"shard_id":N}]}`
//! - `POST /v1/shards` / `DELETE /v1/shards/{idx}` → 501 (cluster
//!   expansion / decommission not yet supported).

mod create;
mod delete;
mod list;

pub use create::create;
pub use delete::delete;
pub use list::list;
