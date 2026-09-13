//! # brain-plugins
//!
//! Plugin surface for the Brain write pipeline.
//!
//! Three plugin kinds, each anchored on the shared lifecycle trait
//! [`RecallPlugin`]:
//!
//! - [`EnricherPlugin`] — mutate candidate items between extraction
//!   and persistence. Adds, mutates, or drops [`brain_extractors::framework::item::ExtractedItem`]s.
//! - [`ConnectorPlugin`] — pull raw memories from external sources
//!   (Drive / GitHub / Slack — none shipped in v1, just the trait).
//!
//! Plugins run on the writer's executor only and must be sync — they
//! MUST NOT spawn threads, await on tokio futures, or perform
//! synchronous IO that exceeds a few microseconds. Doing so will stall
//! the shard's Glommio executor. Use plugin-side caches and pre-built
//! indexes for any non-trivial work.
//!
//! Plugins are registered at compile time via
//! [`PluginRegistry::register_enricher`] / [`PluginRegistry::register_connector`].
//! Dynamic loading is intentionally not supported in v1.
//!
//! # Failure isolation
//!
//! Every plugin call is wrapped in `std::panic::catch_unwind`. Errors
//! and panics are logged via `tracing::warn!` and translated into
//! [`PluginError::Failed`] / [`PluginError::Panicked`]; the host
//! pipeline continues with the un-enriched items (fail-open).
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//! use brain_plugins::{
//!     EnricherInput, EnricherOutput, EnricherPlugin, PluginRegistry,
//!     PluginResult, RecallPlugin,
//! };
//!
//! struct Tagger;
//! impl RecallPlugin for Tagger {
//!     fn plugin_id(&self) -> &'static str { "example:tagger" }
//!     fn plugin_name(&self) -> &'static str { "Tagger" }
//!     fn initialize(&self, _: &serde_json::Value) -> PluginResult<()> { Ok(()) }
//! }
//! impl EnricherPlugin for Tagger {
//!     fn enrich(&self, _input: EnricherInput<'_>) -> PluginResult<EnricherOutput> {
//!         Ok(EnricherOutput::zero())
//!     }
//! }
//!
//! let mut reg = PluginRegistry::new();
//! reg.register_enricher(Arc::new(Tagger), &serde_json::Value::Null).unwrap();
//! assert_eq!(reg.enricher_ids(), vec!["example:tagger"]);
//! ```

#![forbid(unsafe_code)]

pub mod connector;
pub mod enricher;
pub mod errors;
pub mod recall;
pub mod registry;

pub use connector::{ConnectorItem, ConnectorPlugin, ConnectorRequest, ConnectorResponse};
pub use enricher::{EnricherInput, EnricherOutput, EnricherPlugin};
pub use errors::{PluginError, PluginResult};
pub use recall::RecallPlugin;
pub use registry::{EnricherOutcome, PluginRegistry};
