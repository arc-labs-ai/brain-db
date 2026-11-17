//! Shared in-process server harness for integration tests.
//!
//! Brings up N shards + the data-plane connection listener + the
//! admin HTTP server on ephemeral 127.0.0.1 ports, returns a
//! [`Server`] with the bound addresses. The `e2e.rs` tests and
//! the wire integration tests share this scaffold.
//!
//! ## How this reaches the server's internals
//!
//! Through the `brain_server` library, like any other dependant. A crate with
//! no library target leaves only one way in: `#[path]`-mounting every source
//! module at each test file's own crate root so that `crate::config` and
//! friends resolve. That mount makes the mounted modules look unused and
//! forces a blanket dead-code suppression in each test crate.

#![cfg(target_os = "linux")]
// This module is compiled into EVERY integration-test binary, and no single
// binary uses every helper in it. That is inherent to a shared test scaffold
// rather than code nobody needs, so the suppression is a statement about the
// scaffold and is scoped to it alone.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_protocol::connection::handshake::{AuthMethod, ServerCapabilities};
use tempfile::TempDir;

use crate::config;
use brain_server::admin::{AdminServer, AdminState};
use brain_server::network::connection::{
    ConnectionLimits, ConnectionListener, ConnectionMetrics, ShutdownSignal, ShutdownTrigger,
    Topology,
};
use brain_server::network::routing::RoutingTable;
use brain_server::shard::{
    spawn_shard, LlmSpawnConfig, RerankSpawnConfig, ShardHandle, ShardJoiner, ShardSpawnConfig,
};

/// Integration-test stub dispatcher. The harness never exercises
/// embedding quality and we don't want to load a real BGE model per
/// test binary.
struct TestStubDispatcher;
impl Dispatcher for TestStubDispatcher {
    fn embed(&self, _: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok([0.0; VECTOR_DIM])
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0; 16]
    }
}

pub fn stub_dispatcher() -> Arc<dyn Dispatcher> {
    Arc::new(TestStubDispatcher)
}

pub struct Server {
    pub data_plane_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub trigger: ShutdownTrigger,
    pub listener_handle: tokio::task::JoinHandle<std::io::Result<SocketAddr>>,
    pub admin_handle: tokio::task::JoinHandle<std::io::Result<SocketAddr>>,
    pub handles: Vec<ShardHandle>,
    pub joiners: Vec<Option<ShardJoiner>>,
    /// Mandatory-auth handle for tests: the API-key store the data plane
    /// resolves credentials against. Tests mint keys via [`Server::mint`].
    pub auth_store: Arc<brain_server::network::auth::AuthStore>,
    /// A pre-minted FULL-permission token (raw secret bytes) for a default
    /// `(namespace="test", space=default_space)`. Most tests just present
    /// this; multi-space tests call [`Server::mint`] for more.
    pub token: Vec<u8>,
    /// The space_id bound to [`Server::token`].
    pub default_space: [u8; 16],
    /// `Some` when [`start`] owns the data dir (auto-cleanup on `stop`);
    /// `None` when [`start_in`] was used and the caller holds the
    /// `TempDir` (so the data dir survives `stop` for inspection).
    pub _data_dir: Option<TempDir>,
}

impl Server {
    /// Mint an API key for `(namespace, space)` with the given permission
    /// bitfield and return the raw secret bytes to present in AUTH.
    pub fn mint(&self, namespace: &str, space: [u8; 16], permissions: u32) -> Vec<u8> {
        self.mint_with_may_act(namespace, space, permissions, Vec::new())
    }

    /// Like [`Server::mint`] but attaches a `may_act` allowlist — the set of
    /// namespaces an `ACT_AS`-bearing service principal is permitted to run
    /// on behalf of. Used by the `act_as` isolation test to mint a trusted
    /// gateway/edge principal.
    pub fn mint_with_may_act(
        &self,
        namespace: &str,
        space: [u8; 16],
        permissions: u32,
        may_act: Vec<String>,
    ) -> Vec<u8> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        self.auth_store
            .mint(
                [0u8; 16],
                [0u8; 16],
                namespace.to_string(),
                space,
                permissions,
                may_act,
                now,
            )
            .expect("mint test key")
            .secret_bytes
    }
}

impl Server {
    pub async fn stop(mut self) {
        self.trigger.signal();
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut self.listener_handle).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut self.admin_handle).await;
        drop(self.handles);
        for joiner in self.joiners.iter_mut().filter_map(|j| j.take()) {
            let _ = tokio::task::spawn_blocking(move || joiner.join())
                .await
                .map_err(|_| ());
        }
    }
}

pub async fn start(n_shards: usize) -> Server {
    let data_dir = TempDir::new().expect("tmp");
    let mut server = start_in(data_dir.path(), n_shards).await;
    server._data_dir = Some(data_dir);
    server
}

/// Same as [`start`] but uses a caller-supplied data directory. The
/// caller owns the directory's lifetime — useful when a test wants to
/// inspect on-disk state after `Server::stop()` returns.
pub async fn start_in(data_dir: &Path, n_shards: usize) -> Server {
    // ShardSpawnConfig::new defaults the model/key-dependent capabilities
    // (classifier, llm, rerank) to OFF so a shard spawns without GLiNER /
    // cross-encoder models or an LLM key — see their Default impls. The tests
    // on this harness exercise the wire / dispatch / storage / recall paths,
    // not extraction quality, so the model-free pattern tier is enough.
    start_in_with(data_dir, n_shards, |dd| {
        ShardSpawnConfig::new(dd, stub_dispatcher())
    })
    .await
}

/// Boot a single shard with the FULL extraction pipeline live: a real
/// embedding dispatcher plus the pattern + classifier (GLiNER) + LLM extractor
/// tiers enabled, with the OpenAI key ferried into the LLM tier. Rerank is left
/// off (read-path only; skips loading the cross-encoder). The GLiNER model is
/// auto-discovered from the XDG model dir at shard spawn. Used by the
/// write-extraction-accuracy corpus test, which needs every tier running to
/// exercise real entity/statement/relation extraction.
/// Boot the full pipeline, optionally with a provider credential registered
/// for one namespace.
///
/// A deployment holds no provider key of its own, so a test that wants the
/// model-backed stages to run registers one the way a tenant would:
/// `provider_key` is `(namespace, provider, model, api_key)`.
pub async fn start_full_pipeline_in(
    data_dir: &Path,
    dispatcher: Arc<dyn Dispatcher>,
    provider_key: Option<(String, String, String, String)>,
) -> Server {
    let credentials = provider_key.map(|(namespace, provider, model, api_key)| {
        let ring = brain_crypto::load_or_create(data_dir).expect("sealing key");
        let store = brain_metadata::llm_credential::LlmCredentialDb::open(
            data_dir.join("llm_credentials.redb"),
        )
        .expect("credential store");
        let access = Arc::new(brain_metadata::llm_credential::LlmCredentialAccess::new(
            store,
            Arc::new(ring),
        ));
        access
            .put(
                brain_metadata::tables::scope::DeclScope::base(&namespace),
                &provider,
                &model,
                &api_key,
                0,
            )
            .expect("register credential");
        access
    });
    start_in_with(data_dir, 1, move |dd| {
        let mut cfg = ShardSpawnConfig::new(dd, dispatcher.clone());
        cfg.llm = LlmSpawnConfig {
            extractor_max_tokens: None,
            model: None,
            credentials: credentials.clone(),
        };
        cfg.rerank = RerankSpawnConfig { enabled: false };
        cfg
    })
    .await
}

async fn start_in_with<F>(data_dir: &Path, n_shards: usize, mk_cfg: F) -> Server
where
    F: Fn(&Path) -> ShardSpawnConfig,
{
    let mut handles = Vec::with_capacity(n_shards);
    let mut joiners = Vec::with_capacity(n_shards);
    for shard_id in 0..n_shards {
        let cfg = mk_cfg(data_dir);
        let (h, j) = spawn_shard(shard_id as u16, cfg).expect("spawn shard");
        handles.push(h);
        joiners.push(Some(j));
    }
    let shards: Arc<Vec<ShardHandle>> = Arc::new(handles.clone());
    let routing = Arc::new(arc_swap::ArcSwap::from_pointee(
        RoutingTable::new(n_shards as u16, std::collections::HashMap::new()).unwrap(),
    ));
    let request_metrics = Arc::new(brain_server::metrics::request::RequestMetrics::new());

    let auth_store_path = data_dir.join("api_keys.redb");
    let auth_store = Arc::new(
        brain_server::network::auth::AuthStore::open(&auth_store_path).expect("open auth store"),
    );
    // Mandatory auth: mint a default FULL-permission key so tests can
    // connect. The default key is bound to a real namespace ("test") — an
    // authenticated caller with no namespace is now rejected fail-closed at
    // dispatch (no SYSTEM fallback for user data), so the harness key must
    // carry one. Namespace-scoped tests mint their own keys via
    // `Server::mint` for other tenants.
    let default_space = *uuid::Uuid::now_v7().as_bytes();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let token = auth_store
        .mint(
            [0u8; 16],
            [0u8; 16],
            "test".to_string(),
            default_space,
            brain_metadata::api_keys::bits::FULL,
            Vec::new(),
            now,
        )
        .expect("mint default test key")
        .secret_bytes;
    let topology = Topology {
        shards: shards.clone(),
        routing,
        server_caps: Arc::new(ServerCapabilities::v1_default(
            "brain-server/e2e",
            vec![AuthMethod::Token],
        )),
        request_metrics: request_metrics.clone(),
        auth_store: auth_store.clone(),
    };

    let connections = Arc::new(ConnectionMetrics::default());
    let (trigger, signal) = ShutdownSignal::channel();

    let listener = ConnectionListener::new(
        "127.0.0.1:0".parse().unwrap(),
        None,
        topology,
        connections.clone(),
        ConnectionLimits::default(),
        signal.clone(),
    );
    let bound = listener.bind().expect("bind listener");
    let data_plane_addr = bound.local_addr();
    let listener_handle = tokio::spawn(async move { bound.serve().await });

    let admin_state = Arc::new(AdminState::new(
        shards,
        connections,
        Arc::new(config::Config::for_tests()),
        request_metrics,
        auth_store.clone(),
    ));
    let admin = AdminServer::new("127.0.0.1:0".parse().unwrap(), admin_state, signal);
    let bound_admin = admin.bind().await.expect("bind admin");
    let admin_addr = bound_admin.local_addr();
    let admin_handle = tokio::spawn(async move { bound_admin.serve().await });

    Server {
        data_plane_addr,
        admin_addr,
        trigger,
        listener_handle,
        admin_handle,
        handles,
        joiners,
        auth_store,
        token,
        default_space,
        _data_dir: None,
    }
}
