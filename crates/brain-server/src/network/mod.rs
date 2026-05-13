//! Tokio connection layer: the per-listener accept loop
//! (`connection`), the Tokio↔Glommio frame dispatcher (`dispatch`),
//! the agent→shard routing table (`routing`), and the SUBSCRIBE bridge
//! (`subscribe`).

#[cfg(target_os = "linux")]
pub mod connection;
#[cfg(target_os = "linux")]
pub mod dispatch;
#[cfg(target_os = "linux")]
pub mod routing;
#[cfg(target_os = "linux")]
pub mod subscribe;
