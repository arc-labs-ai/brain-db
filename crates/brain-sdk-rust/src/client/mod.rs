//! `Client` — the SDK's user-facing entry point.
//!
//! Backed by an [`Arc<Pool>`].
//!
//! 10.1 shipped this as a single-TCP wrapper; 10.2 reshapes it
//! around the pool while preserving the public surface (
//! `connect`, `bye`, `agent_id`, `session`, `config`).
//!
//! Op methods (encode / recall / plan / reason / forget / link /
//! txn / subscribe) land in 10.5+ and will dispatch through
//! `pool.acquire().await?` once per call.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use brain_core::{AgentId, MemoryId, RequestId};
use brain_protocol::envelope::request::{EdgeKindWire, ObservationInput, PlanState};
use brain_protocol::envelope::response::{TxnAbortResponse, TxnBeginResponse, TxnCommitResponse};

use crate::config::ClientConfig;
use crate::error::ClientError;
use crate::observability::{MetricsSnapshot, MetricsState};
use crate::ops::{
    admin::AdminClient,
    txn::{txn_abort, txn_begin, txn_commit, DEFAULT_TXN_TIMEOUT_SECONDS},
    EncodeBuilder, ForgetBuilder, LinkBuilder, MaterializeProceduralBuilder, PlanBuilder,
    ReasonBuilder, RecallBuilder, SubscribeBuilder, UnlinkBuilder,
};
use crate::pool::{Pool, PoolConfig, PoolGuard};
use crate::proto::handshake::NegotiatedSession;
use crate::request_id::{DefaultRequestIdSource, RequestIdSource};
use crate::retry::{retry_with_backoff, DefaultJitter, JitterSource};

/// User-facing async client. Cheap to clone (it's just a handful
/// of `Arc`s under the hood).
#[derive(Clone)]
pub struct Client {
    pool: Arc<Pool>,
    /// Cached for [`Client::agent_id`]. Always equals the agent
    /// id stamped on every checked-out connection.
    agent_id: AgentId,
    /// Cached for [`Client::config`]. Equals the `ClientConfig`
    /// the pool was built with.
    config: ClientConfig,
    /// Shared jitter source. One LCG per client; cloning the
    /// `Client` shares it (Arc).
    jitter: Arc<dyn JitterSource>,
    /// Shared `RequestId` generator. Cloned `Client`s share the
    /// same source so concurrent op calls still see distinct ids.
    req_id_source: Arc<dyn RequestIdSource>,
    /// Shared metrics state. Snapshots from any cloned client
    /// reflect every op anywhere in the process.
    pub(crate) metrics: Arc<MetricsState>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("pool", &self.pool)
            .field("agent_id", &self.agent_id)
            .field("config", &self.config)
            .finish()
    }
}

impl Client {
    /// Open a single connection to `addr` with spec defaults
    /// (auth = `None`, pool = 1/1). Eagerly completes the
    /// handshake — equivalent to 10.1's `Client::connect` and
    /// keeps that contract.
    pub async fn connect(addr: SocketAddr) -> Result<Self, ClientError> {
        let config = ClientConfig::default().with_pool(PoolConfig::single());
        Self::connect_with(addr, AgentId::new(), config).await
    }

    /// Open with an explicit agent id and config. Eagerly opens
    /// `config.pool.min_connections` connections via
    /// [`Pool::warm_up`], so the first op call doesn't pay the
    /// handshake latency. If `min_connections`
    /// is 0, this is equivalent to [`Client::new_lazy`].
    pub async fn connect_with(
        addr: SocketAddr,
        agent_id: AgentId,
        config: ClientConfig,
    ) -> Result<Self, ClientError> {
        let pool = Pool::new(addr, agent_id, config.clone());
        if config.pool.min_connections > 0 {
            pool.warm_up().await?;
        } else {
            // Honor 10.1's "connection ready on return" contract
            // by opening exactly one connection. Lazy callers
            // should use `new_lazy`.
            pool.acquire().await?;
        }
        Ok(Self {
            pool,
            agent_id,
            config,
            jitter: Arc::new(DefaultJitter::default()),
            req_id_source: Arc::new(DefaultRequestIdSource),
            metrics: Arc::new(MetricsState::default()),
        })
    }

    /// Construct a `Client` lazily — no eager handshake. The
    /// first [`Client::acquire`] / op call drives the handshake.
    /// Useful for tests that want to assert connection counts.
    #[must_use]
    pub fn new_lazy(addr: SocketAddr, agent_id: AgentId, config: ClientConfig) -> Self {
        let pool = Pool::new(addr, agent_id, config.clone());
        Self {
            pool,
            agent_id,
            config,
            jitter: Arc::new(DefaultJitter::default()),
            req_id_source: Arc::new(DefaultRequestIdSource),
            metrics: Arc::new(MetricsState::default()),
        }
    }

    /// Pre-establish `min_connections` connections in parallel.
    /// Returns once all are ready.
    pub async fn warm_up(&self) -> Result<(), ClientError> {
        self.pool.warm_up().await
    }

    /// The agent id stamped on every connection in this pool.
    #[must_use]
    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    /// The configuration the client was built with.
    #[must_use]
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Acquire one connection from the pool. The returned guard
    /// dereferences to `&mut Connection`; drop releases it back.
    ///
    /// 10.5+ uses this from each op method.
    #[allow(dead_code)] // Consumed by op methods in 10.5.
    pub async fn acquire(&self) -> Result<PoolGuard, ClientError> {
        self.pool.acquire().await
    }

    /// Mint a fresh [`RequestId`] (UUIDv7 by default). Spec
    /// §13/04 §3: state-mutating ops (ENCODE / FORGET / LINK /
    /// UNLINK / TXN_COMMIT) need one; 10.5's op-method builders
    /// call this when the caller didn't supply one. Retries
    /// **reuse the same id** so the server's 24-hour
    /// idempotency cache deduplicates.
    #[must_use]
    pub fn next_request_id(&self) -> RequestId {
        self.req_id_source.next()
    }

    /// Point-in-time snapshot of the SDK's internal counters
    /// (request totals, retry totals, in-flight gauge, per-op
    /// breakdown).
    ///
    /// All counters are monotonically increasing across the
    /// process lifetime; callers compute deltas between
    /// snapshots.
    #[must_use]
    pub fn metrics_snapshot(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }

    // ---- Cognitive operations (-§11) ----

    /// ENCODE a memory. Returns an [`EncodeBuilder`]; chain
    /// optional fields and call `.send().await`.
    #[must_use]
    pub fn encode(&self, text: impl Into<String>) -> EncodeBuilder<'_> {
        EncodeBuilder::new(self, text)
    }

    /// RECALL similar memories by cue text. Returns a
    /// [`RecallBuilder`]; chain knobs (top_k, filters) and call
    /// `.send().await` to collect results into a `Vec`.
    #[must_use]
    pub fn recall(&self, cue: impl Into<String>) -> RecallBuilder<'_> {
        RecallBuilder::new(self, cue)
    }

    /// PLAN a path from `start` to `goal`. 10.5 ships the
    /// Vec-collecting form; 10.6 adds an async-iterator surface.
    #[must_use]
    pub fn plan(&self, start: PlanState, goal: PlanState) -> PlanBuilder<'_> {
        PlanBuilder::new(self, start, goal)
    }

    /// REASON about an observation.
    #[must_use]
    pub fn reason(&self, observation: ObservationInput) -> ReasonBuilder<'_> {
        ReasonBuilder::new(self, observation)
    }

    /// FORGET a memory. Single-id mode only in 10.5; batch /
    /// filter modes are deferred.
    #[must_use]
    pub fn forget(&self, memory_id: MemoryId) -> ForgetBuilder<'_> {
        ForgetBuilder::new(self, memory_id)
    }

    /// LINK two memories with an edge of `kind` and weight 1.0
    /// by default.
    #[must_use]
    pub fn link(&self, source: MemoryId, kind: EdgeKindWire, target: MemoryId) -> LinkBuilder<'_> {
        LinkBuilder::new(self, source, kind, target)
    }

    /// UNLINK two memories identified by `(source, kind, target)`.
    #[must_use]
    pub fn unlink(
        &self,
        source: MemoryId,
        kind: EdgeKindWire,
        target: MemoryId,
    ) -> UnlinkBuilder<'_> {
        UnlinkBuilder::new(self, source, kind, target)
    }

    /// Materialize the calling agent's `brain:behavior_*` Preferences
    /// into a rendered system block ready for LLM prompt injection
    /// (W3.1, wire v2). Returns a [`MaterializeProceduralBuilder`];
    /// chain `top_k`, `min_confidence`, etc., and call `.send()`.
    #[must_use]
    pub fn materialize_procedural(&self) -> MaterializeProceduralBuilder<'_> {
        MaterializeProceduralBuilder::new(self)
    }

    /// SUBSCRIBE to change events. `collect(N)` returns a batch;
    /// `send_stream()` returns a `Stream` that yields events as
    /// they arrive.
    #[must_use]
    pub fn subscribe(&self) -> SubscribeBuilder<'_> {
        SubscribeBuilder::new(self)
    }

    /// Operator-only admin surface. Currently exposes
    /// `backfill(scope)` (re-run extractors over a memory range)
    /// and `backfill_cancel(id)`. Streaming progress is deferred;
    /// today's wire is fire-and-forget with an initial snapshot.
    #[must_use]
    pub fn admin(&self) -> AdminClient<'_> {
        AdminClient::new(self)
    }

    /// Cancel a live subscription by its target stream id (the
    /// value returned by [`FrameStream::stream_id`] on the
    /// subscriber). The server cancels the registry entry and
    /// returns the final LSN it emitted to that subscriber.
    ///
    /// Safe to call from any connection in the pool — the registry
    /// key is global per shard.
    ///
    /// [`FrameStream::stream_id`]: crate::ops::FrameStream::stream_id
    pub async fn unsubscribe(
        &self,
        target_stream_id: u32,
    ) -> Result<brain_protocol::envelope::response::UnsubscribeResponse, ClientError> {
        crate::ops::subscribe::unsubscribe(self, target_stream_id).await
    }

    /// Open a transaction. Returns the `TxnBeginResponse`
    /// (carries the `txn_id` that subsequent ops attach via
    /// `.txn(id)`).
    pub async fn txn_begin(&self) -> Result<TxnBeginResponse, ClientError> {
        txn_begin(self, DEFAULT_TXN_TIMEOUT_SECONDS).await
    }

    /// Open a transaction with a custom timeout (in seconds).
    pub async fn txn_begin_with_timeout(
        &self,
        timeout_seconds: u32,
    ) -> Result<TxnBeginResponse, ClientError> {
        txn_begin(self, timeout_seconds).await
    }

    /// Commit the transaction.
    pub async fn txn_commit(&self, txn_id: [u8; 16]) -> Result<TxnCommitResponse, ClientError> {
        txn_commit(self, txn_id).await
    }

    /// Abort the transaction.
    pub async fn txn_abort(&self, txn_id: [u8; 16]) -> Result<TxnAbortResponse, ClientError> {
        txn_abort(self, txn_id).await
    }

    /// Run `op` through the client's retry policy.
    /// Each attempt re-invokes `op`. Returns
    /// [`ClientError::RetryExhausted`] once `max_attempts` /
    /// `total_timeout` is hit; the original error for the first
    /// non-retryable failure.
    ///
    /// `op_name` is an `observability::attributes::OP_*` constant
    /// — used for the metrics breakdown and span attribute.
    ///
    /// 10.5+ wraps every op method with this helper.
    #[allow(dead_code)] // Consumed by op methods in 10.5.
    pub(crate) async fn run_op<F, Fut, T>(
        &self,
        op_name: &'static str,
        mut op: F,
    ) -> Result<T, ClientError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, ClientError>>,
    {
        let _in_flight = self.metrics.begin_request(op_name);
        let metrics = self.metrics.clone();
        let mut attempt: u32 = 0;
        let wrapped = || {
            attempt += 1;
            if attempt > 1 {
                metrics.record_retry(op_name);
                tracing::warn!(
                    target: "brain_sdk_rust",
                    op = op_name,
                    attempt,
                    "retry attempt"
                );
            }
            op()
        };
        let result = retry_with_backoff(wrapped, &self.config.retry, self.jitter.as_ref()).await;
        if let Err(e) = &result {
            self.metrics.record_error(op_name);
            tracing::error!(
                target: "brain_sdk_rust",
                op = op_name,
                error = %e,
                "op failed"
            );
        }
        result
    }

    /// Snapshot the negotiated session from one of the pool's
    /// connections. Returns `None` if the pool is empty (e.g.
    /// before `warm_up()` on a lazy client). Acquires + releases
    /// one connection.
    pub async fn session(&self) -> Result<Option<NegotiatedSession>, ClientError> {
        let guard = self.pool.acquire().await?;
        Ok(Some(guard.session().clone()))
    }

    /// Close the pool. After this returns, further `acquire` /
    /// `bye` calls fail with `ClientError::PoolClosed`.
    pub async fn close(self) -> Result<(), ClientError> {
        self.pool.close();
        Ok(())
    }

    /// 10.1 compatibility: send a BYE on one connection then
    /// close the pool. Equivalent to `acquire → bye → close`.
    pub async fn bye(self) -> Result<(), ClientError> {
        // Acquire the connection, take ownership out of the
        // pool, send BYE, then mark the pool closed so other
        // slots can't be acquired.
        let mut guard = self.pool.acquire().await?;
        // Take the connection out of the slot via std::mem::take
        // would require a Default impl. Instead, we steal a fresh
        // Connection by re-opening — wasteful but simple. 10.5
        // will refine this with proper guard-consumption.
        //
        // Simpler approach: send BYE on the guarded connection,
        // then close the pool which discards all idle slots.
        // The connection will be dropped (TCP close) when the
        // guard goes out of scope after this method.
        //
        // We can't call `Connection::bye` because that consumes
        // the connection by value. So we hand-roll the BYE here.
        use brain_protocol::codec::opcode::Opcode;
        use brain_protocol::envelope::request::ByeRequest;
        use brain_protocol::{Frame, RequestBody};

        const FLAG_EOS: u8 = 1 << 7;
        let frame = Frame::new(
            Opcode::Bye.as_u16(),
            FLAG_EOS,
            0,
            RequestBody::Bye(ByeRequest {
                reason: Some("brain-sdk-rust client shutdown".into()),
            })
            .encode(),
        );
        crate::proto::frames::write_frame(guard.stream_mut(), &frame).await?;
        // Best-effort read of the echoed BYE, with a short timeout
        // so servers that close without acking (or mocks that
        // don't reply) don't hang the client.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            crate::proto::frames::read_one_frame(guard.stream_mut()),
        )
        .await;
        drop(guard);
        self.pool.close();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn next_request_id_is_fresh_per_call() {
        // Build a Client without touching the network: skip
        // connection setup by constructing the inner state
        // directly. The pool is never used here.
        let addr: SocketAddr = "127.0.0.1:1".parse().expect("addr");
        let agent_id = AgentId::new();
        let config = ClientConfig::default().with_pool(PoolConfig::single());
        let client = Client::new_lazy(addr, agent_id, config);

        let a = client.next_request_id();
        let b = client.next_request_id();
        let c = client.next_request_id();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[tokio::test]
    async fn cloned_client_shares_request_id_source() {
        let addr: SocketAddr = "127.0.0.1:1".parse().expect("addr");
        let agent_id = AgentId::new();
        let config = ClientConfig::default().with_pool(PoolConfig::single());
        let c1 = Client::new_lazy(addr, agent_id, config);
        let c2 = c1.clone();

        // Two clones, alternating calls → all ids distinct.
        let a = c1.next_request_id();
        let b = c2.next_request_id();
        let c = c1.next_request_id();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }
}
