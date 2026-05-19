//! Thin wrapper around [`brain_sdk_rust::Client`] that applies the
//! shell's defaults (timeout, single-connection pool).

use std::net::SocketAddr;
use std::time::Duration;

use brain_core::AgentId;
use brain_sdk_rust::{Client, ClientConfig, ClientError, PoolConfig};

/// Open a `Client` to `addr` configured with shell defaults.
///
/// - per-op `timeout`
/// - single-connection pool (the REPL serialises ops so one socket
///   is sufficient; the one-shot path uses one and exits)
pub async fn connect(
    addr: SocketAddr,
    agent_id: AgentId,
    timeout: Duration,
) -> Result<Client, ClientError> {
    let config = ClientConfig::default()
        .with_timeout(timeout)
        .with_pool(PoolConfig::single());
    Client::connect_with(addr, agent_id, config).await
}
