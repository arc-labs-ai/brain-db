//! # brain-workers
//!
//! Background-worker infrastructure plus the 12 concrete workers
//! (sub-tasks 8.2 – 8.13). v1 runs on the default tokio runtime;
//! Phase 9 swaps in the Glommio shard executor without changing the
//! trait surface.
//!
//! Sub-task 8.1 ships the infrastructure only:
//! - [`Worker`] trait + [`WorkerKind`].
//! - [`WorkerConfig`] with spec §11/01 §11 defaults.
//! - [`WorkerContext`] (handle bag + shutdown signal).
//! - [`WorkerMetrics`] (spec §11/01 §15).
//! - [`WorkerScheduler`] + [`WorkerHandle`].
//! - [`drive_batch`] helper for spec §11/01 §5 / §6 cycle structure.
//!
//! See `spec/11_background_workers/` for the authoritative design.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod config;
pub mod context;
pub mod error;
pub mod metrics;
pub mod scheduler;
pub mod worker;

pub use config::{WorkerConfig, WorkerKind};
pub use context::WorkerContext;
pub use error::WorkerError;
pub use metrics::{Snapshot as MetricsSnapshot, WorkerMetrics};
pub use scheduler::{WorkerHandle, WorkerScheduler};
pub use worker::{drive_batch, Worker};
