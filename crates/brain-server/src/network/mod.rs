//! Tokio connection layer: the per-listener accept loop
//! (`connection`), the Tokio↔Glommio frame dispatcher (`dispatch`),
//! the agent→shard routing table (`routing`), and the SUBSCRIBE bridge
//! (`subscribe`). The connection admission gate (`gate`) is a child of
//! `connection` so it travels with the accept loop across the binary and
//! integration-test `#[path]` mounts.

#[cfg(target_os = "linux")]
pub mod auth;
#[cfg(target_os = "linux")]
pub mod connection;
#[cfg(target_os = "linux")]
pub mod dispatch;
#[cfg(target_os = "linux")]
pub mod routing;
#[cfg(target_os = "linux")]
pub mod subscribe;
