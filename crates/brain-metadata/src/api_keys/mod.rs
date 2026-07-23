//! Scope-bound API key store.
//!
//! API keys bind `(org_id, user_id, namespace, space_id, permissions)`
//! at issuance — the server derives every request's scope from the
//! AUTH-time key, so client-supplied space / namespace fields can never
//! escalate beyond what the key permits.
//!
//! The store lives in its own redb file (`api_keys.redb`) — independent
//! of the per-shard `MetadataDb` so the connection layer can resolve
//! credentials before pinning a shard.

use std::path::{Path, PathBuf};

use redb::{Database, ReadTransaction, ReadableDatabase, ReadableTable, WriteTransaction};

use crate::tables::api_keys::{permissions, ApiKeyRow, API_KEYS_BY_SPACE_TABLE, API_KEYS_TABLE};

pub use crate::tables::api_keys::permissions as bits;

/// Errors returned by the API-key store.
#[derive(thiserror::Error, Debug)]
pub enum ApiKeyError {
    #[error("opening api-key store at {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: redb::DatabaseError,
    },

    #[error("transaction: {0}")]
    Transaction(#[from] redb::TransactionError),

    #[error("table: {0}")]
    Table(#[from] redb::TableError),

    #[error("storage: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("commit: {0}")]
    Commit(#[from] redb::CommitError),

    #[error("duplicate key hash on insert")]
    Duplicate,

    #[error("namespace registry: {0}")]
    Namespace(#[from] crate::namespace::NamespaceOpError),
}

/// Hash a raw secret into the lookup key (BLAKE3-256).
#[must_use]
pub fn hash_secret(secret: &[u8]) -> [u8; 32] {
    *blake3::hash(secret).as_bytes()
}

/// Standalone wrapper for the API-key redb file.
///
/// Mirrors [`crate::llm_cache::LlmCacheDb`]'s `&mut self` writer
/// discipline. The auth path holds the store behind a mutex / once-cell
/// since multiple connection tasks need read access concurrently.
pub struct ApiKeyDb {
    db: Database,
    path: PathBuf,
}

impl ApiKeyDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ApiKeyError> {
        let path = path.as_ref().to_path_buf();
        let db = Database::create(&path).map_err(|source| ApiKeyError::Open {
            path: path.clone(),
            source,
        })?;
        // Ensure both tables exist (creating empty ones on a fresh file).
        let wtxn = db.begin_write()?;
        {
            let _ = wtxn.open_table(API_KEYS_TABLE)?;
            let _ = wtxn.open_table(API_KEYS_BY_SPACE_TABLE)?;
        }
        wtxn.commit()?;
        Ok(Self { db, path })
    }

    /// Begin a read transaction. Many concurrent readers allowed.
    pub fn read_txn(&self) -> Result<ReadTransaction, redb::TransactionError> {
        self.db.begin_read()
    }

    /// Begin a write transaction. `&mut self` enforces single-writer
    /// at compile time.
    pub fn write_txn(&mut self) -> Result<WriteTransaction, redb::TransactionError> {
        self.db.begin_write()
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ---------------------------------------------------------------------------
// Ops — work directly on `redb` transactions so the caller controls
// batching with other tables if needed.
// ---------------------------------------------------------------------------

/// Insert a fresh key row, populating both the primary table and the
/// per-space index. Fails with [`ApiKeyError::Duplicate`] if the key
/// hash is already present (collision means the same secret was minted
/// twice, which the caller should treat as a server bug).
#[allow(clippy::too_many_arguments)]
pub fn api_key_create(
    wtxn: &WriteTransaction,
    secret: &[u8],
    org_id: [u8; 16],
    user_id: [u8; 16],
    namespace: String,
    space_id: [u8; 16],
    permissions: u32,
    may_act: Vec<String>,
    now_unix_nanos: u64,
) -> Result<ApiKeyRow, ApiKeyError> {
    let key_hash = hash_secret(secret);
    let mut row = ApiKeyRow::new(
        key_hash,
        org_id,
        user_id,
        namespace,
        space_id,
        permissions,
        now_unix_nanos,
    );
    row.may_act = may_act;

    // Provision the tenant: interning the key's namespace makes it a
    // real, resolvable owner so the dispatch path resolves it to a
    // NamespaceId (rather than falling back to the system namespace).
    // Idempotent — re-minting a key for an existing namespace is a no-op.
    // An empty namespace ("no lock") is left un-interned. `brain` always
    // resolves to NamespaceId::SYSTEM and is never created here.
    if !row.namespace.is_empty() {
        crate::namespace::namespace_intern_or_get(wtxn, &row.namespace, now_unix_nanos)?;
    }

    let mut primary = wtxn.open_table(API_KEYS_TABLE)?;
    if primary.get(&key_hash)?.is_some() {
        return Err(ApiKeyError::Duplicate);
    }
    primary.insert(&key_hash, &row)?;

    let mut by_space = wtxn.open_table(API_KEYS_BY_SPACE_TABLE)?;
    by_space.insert(&(space_id, key_hash), &())?;

    Ok(row)
}

/// Resolve a presented secret to its row. `None` when no matching key
/// exists; the caller decides whether to surface that as `Unknown` or
/// `Missing`. The returned row may still be `revoked = true` — the
/// caller must check.
pub fn api_key_lookup_by_secret(
    rtxn: &ReadTransaction,
    secret: &[u8],
) -> Result<Option<ApiKeyRow>, ApiKeyError> {
    let key_hash = hash_secret(secret);
    api_key_lookup_by_hash(rtxn, &key_hash)
}

/// Resolve a hashed secret to its row.
pub fn api_key_lookup_by_hash(
    rtxn: &ReadTransaction,
    key_hash: &[u8; 32],
) -> Result<Option<ApiKeyRow>, ApiKeyError> {
    let table = rtxn.open_table(API_KEYS_TABLE)?;
    Ok(table.get(key_hash)?.map(|g| g.value()))
}

/// Mark a key revoked. Idempotent — calling twice is a no-op. Returns
/// `false` if the key wasn't found.
pub fn api_key_revoke(wtxn: &WriteTransaction, key_hash: &[u8; 32]) -> Result<bool, ApiKeyError> {
    let mut table = wtxn.open_table(API_KEYS_TABLE)?;
    let Some(existing) = table.get(key_hash)? else {
        return Ok(false);
    };
    let mut row = existing.value();
    drop(existing);
    if !row.revoked {
        row.revoked = true;
        table.insert(key_hash, &row)?;
    }
    Ok(true)
}

/// Update the `last_used_at_unix_nanos` watermark. Called from a
/// background touch task off the AUTH hot path.
pub fn api_key_touch_last_used(
    wtxn: &WriteTransaction,
    key_hash: &[u8; 32],
    now_unix_nanos: u64,
) -> Result<bool, ApiKeyError> {
    let mut table = wtxn.open_table(API_KEYS_TABLE)?;
    let Some(existing) = table.get(key_hash)? else {
        return Ok(false);
    };
    let mut row = existing.value();
    drop(existing);
    if row.last_used_at_unix_nanos < now_unix_nanos {
        row.last_used_at_unix_nanos = now_unix_nanos;
        table.insert(key_hash, &row)?;
    }
    Ok(true)
}

/// List every (non-tombstoned) key issued to `space_id`. Used by admin
/// `list keys for space X`.
pub fn api_key_list_for_space(
    rtxn: &ReadTransaction,
    space_id: &[u8; 16],
) -> Result<Vec<ApiKeyRow>, ApiKeyError> {
    let index = rtxn.open_table(API_KEYS_BY_SPACE_TABLE)?;
    let primary = rtxn.open_table(API_KEYS_TABLE)?;

    let lo = (*space_id, [0u8; 32]);
    let hi = (*space_id, [0xFFu8; 32]);
    let mut out = Vec::new();
    for entry in index.range(lo..=hi)? {
        let (k, _) = entry?;
        let (_space, key_hash) = k.value();
        if let Some(row) = primary.get(&key_hash)? {
            out.push(row.value());
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Scope projection helpers
// ---------------------------------------------------------------------------

/// Resolved scope a connection inherits from its AUTH credential. The
/// connection-layer's auth path constructs one of these and stores it
/// on `ConnPhase::Established`; every subsequent request reads from
/// here instead of trusting client-supplied fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedScope {
    pub key_hash: [u8; 32],
    pub org_id: [u8; 16],
    pub user_id: [u8; 16],
    pub namespace: String,
    pub space_id: [u8; 16],
    pub permissions: u32,
    /// Allowlist of namespaces this scope may act *for* under the
    /// [`permissions::ACT_AS`] grant. Carried verbatim from the key row so
    /// the dispatch path can validate a request's `act_as` target against
    /// it. Empty for every non-`ACT_AS` key.
    pub may_act: Vec<String>,
}

impl ResolvedScope {
    /// Build a scope from a key row.
    #[must_use]
    pub fn from_row(row: &ApiKeyRow) -> Self {
        Self {
            key_hash: row.key_hash,
            org_id: row.org_id,
            user_id: row.user_id,
            namespace: row.namespace.clone(),
            space_id: row.space_id,
            permissions: row.permissions,
            may_act: row.may_act.clone(),
        }
    }

    /// Permissive scope used when scope-binding is disabled (the v1.0
    /// default). Carries the space the client claimed, full
    /// permissions, no namespace lock, zero org/user.
    #[must_use]
    pub fn permissive(space_id: [u8; 16]) -> Self {
        Self {
            key_hash: [0u8; 32],
            org_id: [0u8; 16],
            user_id: [0u8; 16],
            namespace: String::new(),
            space_id,
            permissions: permissions::FULL,
            may_act: Vec::new(),
        }
    }

    /// True iff every bit in `op` is set on this scope.
    #[must_use]
    pub fn allows(&self, op: u32) -> bool {
        self.permissions & op == op
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    fn db(dir: &tempfile::TempDir) -> ApiKeyDb {
        ApiKeyDb::open(dir.path().join("api_keys.redb")).expect("open")
    }

    fn space(byte: u8) -> [u8; 16] {
        let mut a = [0u8; 16];
        a[15] = byte;
        a
    }

    fn org(byte: u8) -> [u8; 16] {
        let mut o = [0u8; 16];
        o[15] = byte;
        o
    }

    #[test]
    fn api_key_create_then_lookup_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = db(&dir);
        let secret = b"raw-secret-bytes";

        let row_created = {
            let wtxn = store.write_txn().unwrap();
            let row = api_key_create(
                &wtxn,
                secret,
                org(1),
                [0u8; 16],
                "acme".into(),
                space(7),
                bits::STANDARD_SPACE,
                Vec::new(),
                1_700_000_000_000_000_000,
            )
            .unwrap();
            wtxn.commit().unwrap();
            row
        };

        let rtxn = store.read_txn().unwrap();
        let row_found = api_key_lookup_by_secret(&rtxn, secret).unwrap().unwrap();
        assert_eq!(row_found, row_created);
        assert!(row_found.allows(bits::ENCODE));
        assert!(!row_found.allows(bits::ADMIN));
    }

    #[test]
    fn api_key_lookup_by_unknown_secret_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = db(&dir);
        let rtxn = store.read_txn().unwrap();
        assert!(api_key_lookup_by_secret(&rtxn, b"never-minted")
            .unwrap()
            .is_none());
    }

    #[test]
    fn api_key_revoked_still_lookups_but_is_marked() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = db(&dir);
        let secret = b"to-be-revoked";

        let key_hash = {
            let wtxn = store.write_txn().unwrap();
            let row = api_key_create(
                &wtxn,
                secret,
                org(2),
                [0u8; 16],
                "acme".into(),
                space(2),
                bits::READ_ONLY,
                Vec::new(),
                1_700_000_000_000_000_000,
            )
            .unwrap();
            wtxn.commit().unwrap();
            row.key_hash
        };

        {
            let wtxn = store.write_txn().unwrap();
            assert!(api_key_revoke(&wtxn, &key_hash).unwrap());
            wtxn.commit().unwrap();
        }

        let rtxn = store.read_txn().unwrap();
        let found = api_key_lookup_by_secret(&rtxn, secret).unwrap().unwrap();
        assert!(found.revoked);

        // Idempotent.
        {
            let wtxn = store.write_txn().unwrap();
            assert!(api_key_revoke(&wtxn, &key_hash).unwrap());
            wtxn.commit().unwrap();
        }
    }

    #[test]
    fn api_key_list_for_space_returns_only_that_spaces_keys() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = db(&dir);

        for (n, who) in [(b"k-a-1".as_ref(), 1u8), (b"k-a-2", 1), (b"k-b-1", 2)] {
            let wtxn = store.write_txn().unwrap();
            api_key_create(
                &wtxn,
                n,
                org(who),
                [0u8; 16],
                "acme".into(),
                space(who),
                bits::STANDARD_SPACE,
                Vec::new(),
                1_700_000_000_000_000_000,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = store.read_txn().unwrap();
        let space_1 = api_key_list_for_space(&rtxn, &space(1)).unwrap();
        let space_2 = api_key_list_for_space(&rtxn, &space(2)).unwrap();
        assert_eq!(space_1.len(), 2);
        assert_eq!(space_2.len(), 1);
        for row in &space_1 {
            assert_eq!(row.space_id, space(1));
        }
    }

    #[test]
    fn duplicate_create_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = db(&dir);
        let secret = b"once-only";

        let wtxn = store.write_txn().unwrap();
        api_key_create(
            &wtxn,
            secret,
            org(1),
            [0u8; 16],
            "n".into(),
            space(1),
            bits::STANDARD_SPACE,
            Vec::new(),
            1,
        )
        .unwrap();
        wtxn.commit().unwrap();

        let wtxn = store.write_txn().unwrap();
        let err = api_key_create(
            &wtxn,
            secret,
            org(1),
            [0u8; 16],
            "n".into(),
            space(1),
            bits::STANDARD_SPACE,
            Vec::new(),
            2,
        );
        assert!(matches!(err, Err(ApiKeyError::Duplicate)));
    }

    #[test]
    fn touch_last_used_advances_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = db(&dir);
        let secret = b"touch-me";
        let key_hash = {
            let wtxn = store.write_txn().unwrap();
            let row = api_key_create(
                &wtxn,
                secret,
                org(1),
                [0u8; 16],
                "n".into(),
                space(1),
                bits::STANDARD_SPACE,
                Vec::new(),
                1_000,
            )
            .unwrap();
            wtxn.commit().unwrap();
            row.key_hash
        };

        {
            let wtxn = store.write_txn().unwrap();
            api_key_touch_last_used(&wtxn, &key_hash, 5_000).unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = store.read_txn().unwrap();
        let row = api_key_lookup_by_hash(&rtxn, &key_hash).unwrap().unwrap();
        assert_eq!(row.last_used_at_unix_nanos, 5_000);
    }

    #[test]
    fn resolved_scope_permissive_grants_full() {
        let scope = ResolvedScope::permissive(space(9));
        assert!(scope.allows(bits::ENCODE));
        assert!(scope.allows(bits::ADMIN));
        assert!(scope.allows(bits::FULL));
        assert_eq!(scope.space_id, space(9));
    }
}
