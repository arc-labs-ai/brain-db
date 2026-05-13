//! Retry policy + runner. Spec §13/04.
//!
//! - [`config`] — [`RetryConfig`] knobs and presets.
//! - [`runner`] — [`retry_with_backoff`] generic loop +
//!   [`compute_delay`] helper + jitter sources.

pub mod config;
pub mod runner;

pub use config::RetryConfig;
pub use runner::{compute_delay, retry_with_backoff, DefaultJitter, JitterSource};
