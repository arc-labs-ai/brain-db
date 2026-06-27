//! Scope-bound authentication for incoming connections.
//!
//! The AUTH frame carries either:
//!
//! - `AuthMethod::Token` — the bytes of a previously-minted API key.
//!   The server hashes them, looks up the scope row, and stamps the
//!   resolved scope on the connection. Every subsequent request reads
//!   identity / namespace / permissions from this scope, never from
//!   the wire request.
//! - `AuthMethod::None` — dev / trusted-network mode. Whether this is
//!   acceptable depends on `BRAIN_REQUIRE_SCOPED_API_KEYS`:
//!   - unset / "false" / "0" (v1.0 default): scope is permissive over
//!     the agent_id the client claimed.
//!   - "true" / "1" (v1.1 default): the AUTH is rejected.
//!
//! The store lives in its own redb file (`api_keys.redb`) so the
//! connection layer can resolve credentials before pinning a shard.
//! Mint and revoke are implemented as plain functions; the HTTP admin
//! surface wires them up.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::sync::Arc;

use brain_core::AgentId;
use brain_metadata::api_keys::{bits, hash_secret, ResolvedScope};
use brain_metadata::{
    api_key_create, api_key_list_for_agent, api_key_lookup_by_secret, api_key_revoke, ApiKeyDb,
    ApiKeyError,
};
use brain_protocol::connection::handshake::{
    AgentPermissions, AuthCredentials, AuthMethod, AuthPayload,
};
use parking_lot::RwLock;
use tracing::{debug, warn};

/// Environment variable that flips strict scope enforcement on.
pub const STRICT_ENV_VAR: &str = "BRAIN_REQUIRE_SCOPED_API_KEYS";

/// True when strict mode is requested via the environment.
#[must_use]
pub fn require_scoped_keys_from_env() -> bool {
    matches!(
        std::env::var(STRICT_ENV_VAR).as_deref(),
        Ok("true") | Ok("1") | Ok("TRUE") | Ok("True")
    )
}

/// Resolved scope a connection inherits from its AUTH credential.
/// Wraps [`ResolvedScope`] alongside a typed [`AgentId`] for fast
/// shard-routing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestScope {
    pub agent_id: AgentId,
    pub org_id: [u8; 16],
    pub user_id: [u8; 16],
    pub namespace: String,
    pub permissions: u32,
    pub scope_enforced: bool,
    /// BLAKE3 of the secret used to authenticate; all-zero in
    /// permissive mode. Useful for `last_used_at` background touches.
    pub key_hash: [u8; 32],
}

impl RequestScope {
    /// Build a permissive scope carrying the agent the client claimed and
    /// the operator-configured default namespace (empty = none, resolves to
    /// the system namespace). Scope binding stays off — the namespace is
    /// carried only so dispatch stamps writes into that tenant.
    #[must_use]
    pub fn permissive(agent_id: AgentId, namespace: String) -> Self {
        Self {
            agent_id,
            org_id: [0u8; 16],
            user_id: [0u8; 16],
            namespace,
            permissions: bits::FULL,
            scope_enforced: false,
            key_hash: [0u8; 32],
        }
    }

    /// Project a resolved row onto the connection scope.
    #[must_use]
    pub fn from_resolved(resolved: ResolvedScope) -> Self {
        Self {
            agent_id: AgentId(uuid::Uuid::from_bytes(resolved.agent_id)),
            org_id: resolved.org_id,
            user_id: resolved.user_id,
            namespace: resolved.namespace,
            permissions: resolved.permissions,
            scope_enforced: true,
            key_hash: resolved.key_hash,
        }
    }

    /// Project these scope claims onto the `AgentPermissions` wire
    /// shape carried by `AUTH_OK`.
    #[must_use]
    pub fn to_agent_permissions(&self) -> AgentPermissions {
        AgentPermissions {
            can_encode: self.permissions & bits::ENCODE != 0,
            can_recall: self.permissions & bits::RECALL != 0,
            can_plan: self.permissions & bits::RECALL != 0,
            can_reason: self.permissions & bits::RECALL != 0,
            can_forget: self.permissions & bits::FORGET != 0,
            can_admin: self.permissions & bits::ADMIN != 0,
        }
    }

    /// Materialize a `brain_ops::RequestCaller` for dispatch.
    ///
    /// The caller is stamped with the wire-level `session_id` minted
    /// at HELLO/WELCOME so the txn store can link buffered work back
    /// to the originating connection — disconnect-time cleanup
    /// fans out on session_id, not on agent_id, because a
    /// single agent may hold many concurrent sessions.
    #[must_use]
    pub fn to_caller(&self, session_id: [u8; 16]) -> brain_ops::RequestCaller {
        let base = if self.scope_enforced {
            brain_ops::RequestCaller::from_scope(
                self.agent_id,
                self.org_id,
                self.user_id,
                self.namespace.clone(),
                self.permissions,
            )
        } else {
            // Permissive: carry the configured default namespace (if any)
            // so dispatch stamps writes into that tenant, but leave scope
            // enforcement off.
            let caller = brain_ops::RequestCaller::new(self.agent_id);
            if self.namespace.is_empty() {
                caller
            } else {
                caller.with_namespace(self.namespace.clone())
            }
        };
        base.with_session_id(session_id)
    }
}

/// Auth-time failure modes. Each maps to a specific wire error.
#[derive(thiserror::Error, Debug)]
pub enum AuthError {
    /// Strict mode is on but the AUTH frame carries no token.
    #[error("API key required (BRAIN_REQUIRE_SCOPED_API_KEYS=true)")]
    Missing,
    /// The presented secret hashed to an unknown key.
    #[error("unknown API key")]
    Unknown,
    /// The key exists but has been revoked.
    #[error("API key has been revoked")]
    Revoked,
    /// Strict mode is on and the client picked `AuthMethod::None`.
    #[error("anonymous authentication is disabled")]
    PolicyForbidsAnonymous,
    /// Backend redb error during lookup.
    #[error("api-key store: {0}")]
    Storage(#[from] ApiKeyError),
}

/// Shared, thread-safe wrapper around the [`ApiKeyDb`].
///
/// The connection layer reads concurrently from many Tokio tasks and
/// occasionally writes (mint / revoke / touch); a `parking_lot::RwLock`
/// gives reader concurrency without an async hop. `ApiKeyDb` itself
/// enforces single-writer via `&mut self` on `write_txn`.
pub struct AuthStore {
    db: RwLock<ApiKeyDb>,
    strict: bool,
    /// Namespace permissive (`auth=none`) connections are scoped to.
    /// `None` keeps legacy behavior (permissive callers resolve to the
    /// system namespace). Ignored in strict mode, where each key carries
    /// its own namespace.
    default_namespace: Option<String>,
}

impl AuthStore {
    /// Open the store at `path`. `strict` is captured at construction
    /// (typically from [`require_scoped_keys_from_env`]) so all
    /// concurrent readers see a single coherent flag.
    pub fn open(path: impl AsRef<Path>, strict: bool) -> Result<Self, ApiKeyError> {
        let db = ApiKeyDb::open(path)?;
        Ok(Self {
            db: RwLock::new(db),
            strict,
            default_namespace: None,
        })
    }

    /// Set the permissive default namespace (from `[auth] default_namespace`).
    #[must_use]
    pub fn with_default_namespace(mut self, namespace: Option<String>) -> Self {
        self.default_namespace = namespace.filter(|s| !s.is_empty());
        self
    }

    /// The permissive default namespace, if configured.
    #[must_use]
    pub fn default_namespace(&self) -> Option<&str> {
        self.default_namespace.as_deref()
    }

    /// True iff scope binding is enforced for new connections.
    #[must_use]
    pub fn strict(&self) -> bool {
        self.strict
    }

    /// Convenience for tests that need to pre-seed keys.
    #[allow(dead_code)]
    pub fn write<F, R>(&self, f: F) -> Result<R, ApiKeyError>
    where
        F: FnOnce(&mut ApiKeyDb) -> Result<R, ApiKeyError>,
    {
        let mut guard = self.db.write();
        f(&mut guard)
    }

    /// Convenience for tests / read paths.
    #[allow(dead_code)]
    pub fn read<F, R>(&self, f: F) -> Result<R, ApiKeyError>
    where
        F: FnOnce(&ApiKeyDb) -> Result<R, ApiKeyError>,
    {
        let guard = self.db.read();
        f(&guard)
    }

    /// Mint a fresh scope-bound API key. Returns the raw secret bytes
    /// to surface once to the operator (never stored).
    pub fn mint(
        &self,
        org_id: [u8; 16],
        user_id: [u8; 16],
        namespace: String,
        agent_id: [u8; 16],
        permissions: u32,
        now_unix_nanos: u64,
    ) -> Result<MintedKey, ApiKeyError> {
        // 32 bytes of CSPRNG output. Concatenate two v7 UUIDs and run
        // them through BLAKE3 to bleach the embedded timestamp and
        // version bits out of the secret.
        let mut buf = [0u8; 32];
        buf[..16].copy_from_slice(uuid::Uuid::now_v7().as_bytes());
        buf[16..].copy_from_slice(uuid::Uuid::now_v7().as_bytes());
        let secret = *blake3::hash(&buf).as_bytes();
        let mut guard = self.db.write();
        let wtxn = guard.write_txn()?;
        let row = api_key_create(
            &wtxn,
            &secret,
            org_id,
            user_id,
            namespace,
            agent_id,
            permissions,
            now_unix_nanos,
        )?;
        wtxn.commit()?;
        Ok(MintedKey {
            secret_bytes: secret.to_vec(),
            key_hash: row.key_hash,
        })
    }

    /// Revoke a key by its hashed identifier. Idempotent.
    pub fn revoke(&self, key_hash: &[u8; 32]) -> Result<bool, ApiKeyError> {
        let mut guard = self.db.write();
        let wtxn = guard.write_txn()?;
        let found = api_key_revoke(&wtxn, key_hash)?;
        wtxn.commit()?;
        Ok(found)
    }

    /// List every key issued to `agent_id`. Returns the rows verbatim
    /// — admin views should redact `key_hash` if surfacing publicly.
    pub fn list_for_agent(
        &self,
        agent_id: &[u8; 16],
    ) -> Result<Vec<brain_metadata::tables::api_keys::ApiKeyRow>, ApiKeyError> {
        let guard = self.db.read();
        let rtxn = guard.read_txn()?;
        api_key_list_for_agent(&rtxn, agent_id)
    }

    /// Look up a secret. Returns `None` when the secret hashes to a row
    /// that doesn't exist.
    pub fn lookup(
        &self,
        secret: &[u8],
    ) -> Result<Option<brain_metadata::tables::api_keys::ApiKeyRow>, ApiKeyError> {
        let guard = self.db.read();
        let rtxn = guard.read_txn()?;
        api_key_lookup_by_secret(&rtxn, secret)
    }
}

/// Result of a successful [`AuthStore::mint`] call.
#[derive(Debug)]
pub struct MintedKey {
    /// Raw 32-byte secret. Surface once to the operator; the server
    /// only retains the hash.
    pub secret_bytes: Vec<u8>,
    /// BLAKE3 of `secret_bytes`, also the primary-key identifier.
    pub key_hash: [u8; 32],
}

impl MintedKey {
    /// `brain_<base64url(secret)>` — the canonical display form.
    #[must_use]
    pub fn formatted(&self) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        format!("brain_{}", URL_SAFE_NO_PAD.encode(&self.secret_bytes))
    }
}

/// Parse the canonical `brain_<base64url(secret)>` display form back to
/// the raw 32-byte secret. Returns `None` on a malformed string.
#[allow(dead_code)]
#[must_use]
pub fn parse_formatted_key(s: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    let body = s.strip_prefix("brain_")?;
    URL_SAFE_NO_PAD.decode(body).ok()
}

/// Resolve the AUTH frame into a [`RequestScope`].
///
/// In permissive mode the scope is whatever the client claimed; in
/// strict mode the AUTH must carry a valid `Token` whose hash maps to a
/// non-revoked row in the store.
pub fn derive_scope_from_handshake(
    auth: &AuthPayload,
    store: &Arc<AuthStore>,
) -> Result<RequestScope, AuthError> {
    if !store.strict() {
        // Permissive: token (if any) is opaque, we just trust the
        // client-supplied agent. No store hit on the hot path. The caller
        // is scoped to the operator-configured default namespace (if any).
        return Ok(RequestScope::permissive(
            AgentId(uuid::Uuid::from_bytes(auth.agent_id)),
            store.default_namespace().unwrap_or_default().to_string(),
        ));
    }

    let secret = match (&auth.method, &auth.credentials) {
        (AuthMethod::Token, AuthCredentials::Token(bytes)) => bytes.as_slice(),
        (AuthMethod::None, _) | (_, AuthCredentials::None) => {
            return Err(AuthError::PolicyForbidsAnonymous);
        }
        _ => return Err(AuthError::Missing),
    };
    if secret.is_empty() {
        return Err(AuthError::Missing);
    }

    let row = store.lookup(secret)?.ok_or(AuthError::Unknown)?;
    if row.revoked {
        warn!(key_hash = %hex32(&row.key_hash), "revoked key presented at AUTH");
        return Err(AuthError::Revoked);
    }
    debug!(
        key_hash = %hex32(&row.key_hash),
        agent_id = ?row.agent_id,
        namespace = %row.namespace,
        "AUTH resolved scope from API key",
    );
    let _ = hash_secret(secret); // exercise the import in release builds
    Ok(RequestScope::from_resolved(ResolvedScope::from_row(&row)))
}

/// Lowercase-hex of a 32-byte hash; used for log fields.
#[must_use]
pub fn hex32(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn store(strict: bool) -> (tempfile::TempDir, Arc<AuthStore>) {
        let dir = tempfile::tempdir().unwrap();
        let s = AuthStore::open(dir.path().join("api_keys.redb"), strict).unwrap();
        (dir, Arc::new(s))
    }

    fn agent(byte: u8) -> [u8; 16] {
        let mut a = [0u8; 16];
        a[15] = byte;
        a
    }

    fn auth_token(agent_id: [u8; 16], secret: Vec<u8>) -> AuthPayload {
        AuthPayload {
            method: AuthMethod::Token,
            agent_id,
            credentials: AuthCredentials::Token(secret),
        }
    }

    fn auth_none(agent_id: [u8; 16]) -> AuthPayload {
        AuthPayload {
            method: AuthMethod::None,
            agent_id,
            credentials: AuthCredentials::None,
        }
    }

    #[test]
    fn permissive_mode_allows_unscoped_handshake() {
        let (_dir, store) = store(false);
        let scope =
            derive_scope_from_handshake(&auth_none(agent(7)), &store).expect("permissive accepts");
        assert!(!scope.scope_enforced);
        assert_eq!(scope.permissions, bits::FULL);
        assert_eq!(scope.agent_id, AgentId(uuid::Uuid::from_bytes(agent(7))));
        // No configured default namespace → empty (resolves to the system
        // namespace at dispatch). The caller carries no namespace.
        assert_eq!(scope.namespace, "");
        assert_eq!(scope.to_caller([0u8; 16]).namespace, "");
    }

    #[test]
    fn permissive_default_namespace_flows_to_scope_and_caller() {
        let dir = tempfile::tempdir().unwrap();
        // Permissive store configured with an operator default namespace.
        let store = Arc::new(
            AuthStore::open(dir.path().join("api_keys.redb"), false)
                .unwrap()
                .with_default_namespace(Some("acme".to_string())),
        );
        let scope =
            derive_scope_from_handshake(&auth_none(agent(3)), &store).expect("permissive accepts");
        // Scope binding stays OFF, but the configured namespace is carried
        // so dispatch stamps writes into that tenant.
        assert!(!scope.scope_enforced);
        assert_eq!(scope.namespace, "acme");
        let caller = scope.to_caller([0u8; 16]);
        assert_eq!(caller.namespace, "acme");
        assert!(!caller.scope_enforced);
    }

    #[test]
    fn permissive_empty_default_namespace_is_ignored() {
        // An empty string is normalized to "no default" so it can't be
        // confused with a real namespace named "".
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::open(dir.path().join("api_keys.redb"), false)
            .unwrap()
            .with_default_namespace(Some(String::new()));
        assert_eq!(store.default_namespace(), None);
    }

    #[test]
    fn permissive_mode_ignores_missing_token() {
        let (_dir, store) = store(false);
        // Even an empty-token AUTH succeeds in permissive mode.
        let payload = auth_token(agent(7), Vec::new());
        let scope = derive_scope_from_handshake(&payload, &store).unwrap();
        assert!(!scope.scope_enforced);
    }

    #[test]
    fn strict_mode_rejects_missing_api_key() {
        let (_dir, store) = store(true);
        let err = derive_scope_from_handshake(&auth_none(agent(1)), &store).unwrap_err();
        assert!(matches!(err, AuthError::PolicyForbidsAnonymous));

        let err =
            derive_scope_from_handshake(&auth_token(agent(1), Vec::new()), &store).unwrap_err();
        assert!(matches!(err, AuthError::Missing));
    }

    #[test]
    fn strict_mode_accepts_valid_api_key() {
        let (_dir, store) = store(true);
        let minted = store
            .mint(
                agent(2),
                [0u8; 16],
                "acme".into(),
                agent(7),
                bits::STANDARD_AGENT,
                1_700_000_000_000_000_000,
            )
            .unwrap();
        let payload = auth_token(agent(7), minted.secret_bytes.clone());
        let scope = derive_scope_from_handshake(&payload, &store).expect("accepts");
        assert!(scope.scope_enforced);
        assert_eq!(scope.namespace, "acme");
        assert_eq!(scope.agent_id, AgentId(uuid::Uuid::from_bytes(agent(7))));
        assert!(scope.permissions & bits::ENCODE != 0);
        assert!(scope.permissions & bits::ADMIN == 0);
    }

    #[test]
    fn strict_mode_rejects_unknown_key() {
        let (_dir, store) = store(true);
        let payload = auth_token(agent(1), b"never-minted".to_vec());
        let err = derive_scope_from_handshake(&payload, &store).unwrap_err();
        assert!(matches!(err, AuthError::Unknown));
    }

    #[test]
    fn strict_mode_rejects_revoked_key() {
        let (_dir, store) = store(true);
        let minted = store
            .mint(
                agent(2),
                [0u8; 16],
                "acme".into(),
                agent(7),
                bits::STANDARD_AGENT,
                1,
            )
            .unwrap();
        assert!(store.revoke(&minted.key_hash).unwrap());
        let payload = auth_token(agent(7), minted.secret_bytes);
        let err = derive_scope_from_handshake(&payload, &store).unwrap_err();
        assert!(matches!(err, AuthError::Revoked));
    }

    #[test]
    fn strict_mode_resolves_agent_from_key_not_client() {
        // Even if the client claims agent X in the AUTH frame, the
        // resolved scope's agent comes from the API key row.
        let (_dir, store) = store(true);
        let key_agent = agent(7);
        let claimed_agent = agent(9);
        let minted = store
            .mint(
                agent(2),
                [0u8; 16],
                "acme".into(),
                key_agent,
                bits::STANDARD_AGENT,
                1,
            )
            .unwrap();
        let payload = auth_token(claimed_agent, minted.secret_bytes);
        let scope = derive_scope_from_handshake(&payload, &store).unwrap();
        assert_eq!(scope.agent_id, AgentId(uuid::Uuid::from_bytes(key_agent)));
    }

    #[test]
    fn permissions_project_onto_wire_shape() {
        let scope = RequestScope::from_resolved(ResolvedScope {
            key_hash: [0u8; 32],
            org_id: [0u8; 16],
            user_id: [0u8; 16],
            namespace: "n".into(),
            agent_id: agent(1),
            permissions: bits::ENCODE | bits::RECALL,
        });
        let p = scope.to_agent_permissions();
        assert!(p.can_encode);
        assert!(p.can_recall);
        assert!(p.can_plan);
        assert!(p.can_reason);
        assert!(!p.can_forget);
        assert!(!p.can_admin);
    }

    #[test]
    fn minted_key_round_trips_through_formatted() {
        let (_dir, store) = store(true);
        let minted = store
            .mint(
                agent(1),
                [0u8; 16],
                "n".into(),
                agent(1),
                bits::STANDARD_AGENT,
                1,
            )
            .unwrap();
        let formatted = minted.formatted();
        assert!(formatted.starts_with("brain_"));
        let parsed = parse_formatted_key(&formatted).unwrap();
        assert_eq!(parsed, minted.secret_bytes);
    }
}
