//! Caller-facing dispatcher surface.
//!
//! - [`cpu`] — the `Dispatcher` trait + `CpuDispatcher` (runs the
//!   model on the calling thread / Glommio executor).
//! - [`cache`] — `CachingDispatcher` decorator (LRU over text hash).

pub mod cache;
pub mod cpu;

pub use cache::{CacheStats, CachingDispatcher, DEFAULT_CACHE_SIZE};
pub use cpu::{CpuDispatcher, Dispatcher, BGE_QUERY_PREFIX};
