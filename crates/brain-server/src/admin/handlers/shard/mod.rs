//! Admin HTTP handlers for `shard` (spec §14/06 §11; sub-task 10.11).
//!
//! Routes:
//! - `GET /v1/shards` → 200 + `{"shards":[{"index":N,"shard_id":N}]}`
//! - `POST /v1/shards` / `DELETE /v1/shards/{idx}` → 501 (cluster
//!   expansion / decommission is Phase-12 territory).

mod create;
mod delete;
mod list;

pub use create::create;
pub use delete::delete;
pub use list::list;
