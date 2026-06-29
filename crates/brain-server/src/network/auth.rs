//! Mandatory key authentication for incoming connections.
//!
//! Every data-plane connection MUST present a valid, resolvable,
//! non-revoked API key. The AUTH frame carries `AuthMethod::Token` — the
//! bytes of a previously-minted key. The server hashes them, looks up the
//! scope row, and stamps the resolved `(namespace, agent, permissions)` on
//! the connection. Every subsequent request reads identity from this scope,
//! never from the wire request. There is no anonymous / permissive mode and
//! no default namespace: a missing / unknown / revoked key — or an mTLS
//! method (not yet supported) — is rejected at AUTH.
//!
//! The store lives in its own redb file (`api_keys.redb`) so the
//! connection layer can resolve credentials before pinning a shard. Keys
//! are minted via the HTTP admin listener (gated by the operator admin
//! secret); mint and revoke are implemented as plain functions here.

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
    /// BLAKE3 of the secret used to authenticate. Useful for
    /// `last_used_at` background touches.
    pub key_hash: [u8; 32],
}

impl RequestScope {
    /// Project a resolved row onto the connection scope.
    #[must_use]
    pub fn from_resolved(resolved: ResolvedScope) -> Self {
        Self {
            agent_id: AgentId(uuid::Uuid::from_bytes(resolved.agent_id)),
            org_id: resolved.org_id,
            user_id: resolved.user_id,
            namespace: resolved.namespace,
            permissions: resolved.permissions,
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
        brain_ops::RequestCaller::from_scope(
            self.agent_id,
            self.org_id,
            self.user_id,
            self.namespace.clone(),
            self.permissions,
        )
        .with_session_id(session_id)
    }
}

/// Auth-time failure modes. Each maps to a specific wire error.
#[derive(thiserror::Error, Debug)]
pub enum AuthError {
    /// The AUTH frame carries no token (empty or wrong credential shape).
    #[error("API key required")]
    Missing,
    /// The presented secret hashed to an unknown key.
    #[error("unknown API key")]
    Unknown,
    /// The key exists but has been revoked.
    #[error("API key has been revoked")]
    Revoked,
    /// The client picked an auth method the server does not support
    /// (currently mTLS).
    #[error("unsupported auth method")]
    UnsupportedMethod,
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
}

impl AuthStore {
    /// Open the store at `path`. Auth is always mandatory; there is no
    /// permissive mode to configure.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ApiKeyError> {
        let db = ApiKeyDb::open(path)?;
        Ok(Self {
            db: RwLock::new(db),
        })
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
#[must_use]
pub fn parse_formatted_key(s: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    let body = s.strip_prefix("brain_")?;
    URL_SAFE_NO_PAD.decode(body).ok()
}

/// Resolve the AUTH frame into a [`RequestScope`].
///
/// The AUTH MUST carry a valid `Token` whose hash maps to a non-revoked
/// row in the store. Any other method (mTLS), a missing/empty token, an
/// unknown key, or a revoked key is rejected — identity is the credential.
pub fn derive_scope_from_handshake(
    auth: &AuthPayload,
    store: &Arc<AuthStore>,
) -> Result<RequestScope, AuthError> {
    let token = match (&auth.method, &auth.credentials) {
        (AuthMethod::Token, AuthCredentials::Token(bytes)) => bytes.as_slice(),
        (AuthMethod::Mtls, _) | (_, AuthCredentials::Mtls(_)) => {
            return Err(AuthError::UnsupportedMethod);
        }
    };
    if token.is_empty() {
        return Err(AuthError::Missing);
    }

    // The credential clients present is the canonical `brain_<base64url>` form
    // the admin mint hands out, sent verbatim. Decode it to the raw secret
    // before hashing — the store keys on BLAKE3(secret_bytes), not the display
    // string. Fall back to the raw bytes for a caller that presents the
    // pre-decoded secret directly.
    let decoded = std::str::from_utf8(token)
        .ok()
        .and_then(parse_formatted_key);
    let secret: &[u8] = decoded.as_deref().unwrap_or(token);

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

    fn store() -> (tempfile::TempDir, Arc<AuthStore>) {
        let dir = tempfile::tempdir().unwrap();
        let s = AuthStore::open(dir.path().join("api_keys.redb")).unwrap();
        (dir, Arc::new(s))
    }

    fn agent(byte: u8) -> [u8; 16] {
        let mut a = [0u8; 16];
        a[15] = byte;
        a
    }

    fn auth_token(secret: Vec<u8>) -> AuthPayload {
        AuthPayload {
            method: AuthMethod::Token,
            credentials: AuthCredentials::Token(secret),
        }
    }

    fn auth_mtls() -> AuthPayload {
        AuthPayload {
            method: AuthMethod::Mtls,
            credentials: AuthCredentials::Mtls(brain_protocol::connection::handshake::MtlsClaim {
                cert_fingerprint: [0u8; 32],
                asserted_subject: "CN=x".into(),
            }),
        }
    }

    #[test]
    fn rejects_missing_token() {
        let (_dir, store) = store();
        let err = derive_scope_from_handshake(&auth_token(Vec::new()), &store).unwrap_err();
        assert!(matches!(err, AuthError::Missing));
    }

    #[test]
    fn rejects_mtls_method() {
        let (_dir, store) = store();
        let err = derive_scope_from_handshake(&auth_mtls(), &store).unwrap_err();
        assert!(matches!(err, AuthError::UnsupportedMethod));
    }

    #[test]
    fn accepts_valid_api_key() {
        let (_dir, store) = store();
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
        let payload = auth_token(minted.secret_bytes.clone());
        let scope = derive_scope_from_handshake(&payload, &store).expect("accepts");
        assert_eq!(scope.namespace, "acme");
        assert_eq!(scope.agent_id, AgentId(uuid::Uuid::from_bytes(agent(7))));
        assert!(scope.permissions & bits::ENCODE != 0);
        assert!(scope.permissions & bits::ADMIN == 0);
        // The caller inherits the key's namespace; identity is the credential.
        assert_eq!(scope.to_caller([0u8; 16]).namespace, "acme");
    }

    #[test]
    fn accepts_formatted_api_key() {
        // The canonical credential clients present is the `brain_<base64url>`
        // display string the admin mint returns — verbatim, not pre-decoded.
        // The wire path must decode it before hashing, or every minted key is
        // rejected as unknown (the bug the docker e2e caught).
        let (_dir, store) = store();
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
        let payload = auth_token(minted.formatted().into_bytes());
        let scope = derive_scope_from_handshake(&payload, &store)
            .expect("the formatted brain_ token must resolve");
        assert_eq!(scope.namespace, "acme");
        assert_eq!(scope.agent_id, AgentId(uuid::Uuid::from_bytes(agent(7))));
    }

    #[test]
    fn rejects_unknown_key() {
        let (_dir, store) = store();
        let payload = auth_token(b"never-minted".to_vec());
        let err = derive_scope_from_handshake(&payload, &store).unwrap_err();
        assert!(matches!(err, AuthError::Unknown));
    }

    #[test]
    fn rejects_revoked_key() {
        let (_dir, store) = store();
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
        let payload = auth_token(minted.secret_bytes);
        let err = derive_scope_from_handshake(&payload, &store).unwrap_err();
        assert!(matches!(err, AuthError::Revoked));
    }

    #[test]
    fn resolves_agent_from_key() {
        // The resolved scope's agent comes entirely from the API key row.
        let (_dir, store) = store();
        let key_agent = agent(7);
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
        let payload = auth_token(minted.secret_bytes);
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
        let (_dir, store) = store();
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
