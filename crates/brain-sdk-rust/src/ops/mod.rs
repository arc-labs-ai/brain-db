//! Op builders for every spec §07 cognitive operation.
//!
//! Each op has its own file; the `Client` exposes a method that
//! returns the builder. Spec §13/02 §3-§11.
//!
//! Streaming ops (RECALL / PLAN / REASON / SUBSCRIBE) ship with
//! a Vec-collecting `send()` (or `collect()`) in 10.5. The
//! async-iterator surface lands in 10.6.

pub mod common;
pub mod encode;
pub mod forget;
pub mod link;
pub mod plan;
pub mod reason;
pub mod recall;
pub mod stream;
pub mod subscribe;
pub mod txn;
pub mod unlink;

pub use stream::FrameStream;

pub use encode::EncodeBuilder;
pub use forget::ForgetBuilder;
pub use link::LinkBuilder;
pub use plan::PlanBuilder;
pub use reason::ReasonBuilder;
pub use recall::RecallBuilder;
pub use subscribe::SubscribeBuilder;
pub use unlink::UnlinkBuilder;
