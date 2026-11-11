//! Tenant LLM provider credential store.
//!
//! A tenant registers their own OpenAI or Anthropic key and Brain's
//! extraction and HyPE stages bill to it. The store lives in its own redb
//! file, independent of the per-shard metadata database, because every shard
//! resolves credentials for the namespaces it processes writes for and there
//! is exactly one answer per scope for all of them.
//!
//! # Writes land exactly, reads walk the ladder
//!
//! Registering a credential writes the scope it names and nothing else, so
//! putting a key on the namespace never silently replaces a team's own key.
//! Resolving walks narrowest-first — the caller's space, then the namespace
//! base — so a space without its own key uses the tenant's, and a space that
//! has one shadows the base for itself alone.
//!
//! The asymmetry is the same one schema declarations use: a write has to say
//! where it lands, because nearly every request names a space and inferring
//! the scope would turn every ordinary namespace-wide registration into a
//! per-space override.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use brain_crypto::{Binding, CryptoError, KeyRing};
use parking_lot::RwLock;
use redb::{Database, ReadTransaction, ReadableDatabase, ReadableTable, WriteTransaction};
use zeroize::Zeroizing;

use crate::tables::llm_credential::{LlmCredentialRow, LLM_CREDENTIALS_TABLE};
use crate::tables::scope::{space_chain, DeclScope, SPACE_BASE};

/// Errors returned by the credential store.
#[derive(thiserror::Error, Debug)]
pub enum LlmCredentialError {
    #[error("opening llm-credential store at {path}: {source}")]
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

    /// The stored key could not be opened. A tenant cannot act on this; it
    /// means the key-encryption key that sealed the row is no longer loaded,
    /// which is an operator problem.
    #[error("stored credential could not be opened: {0}")]
    Sealed(#[from] CryptoError),
}

/// Standalone wrapper for the credential redb file.
///
/// Mirrors [`crate::api_keys::ApiKeyDb`]'s writer discipline: `&mut self` on
/// the write path enforces single-writer at compile time, and the auth and
/// derivation paths hold the store behind a lock for concurrent reads.
pub struct LlmCredentialDb {
    db: Database,
    path: PathBuf,
}

/// Prints where the store lives. The rows it holds are tenant key material,
/// so they are never part of its rendering.
impl std::fmt::Debug for LlmCredentialDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmCredentialDb")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl LlmCredentialDb {
    /// Open (or create) the credential file, materializing its table so every
    /// read path can open it without a missing-table fallback.
    ///
    /// # Errors
    /// [`LlmCredentialError`] when the file cannot be opened or initialized.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LlmCredentialError> {
        let path = path.as_ref().to_path_buf();
        let db = Database::create(&path).map_err(|source| LlmCredentialError::Open {
            path: path.clone(),
            source,
        })?;
        let wtxn = db.begin_write()?;
        {
            let _ = wtxn.open_table(LLM_CREDENTIALS_TABLE)?;
        }
        wtxn.commit()?;
        Ok(Self { db, path })
    }

    /// Begin a read transaction. Many concurrent readers allowed.
    ///
    /// # Errors
    /// [`LlmCredentialError::Transaction`] when redb cannot start it.
    pub fn read_txn(&self) -> Result<ReadTransaction, LlmCredentialError> {
        Ok(self.db.begin_read()?)
    }

    /// Begin a write transaction.
    ///
    /// # Errors
    /// [`LlmCredentialError::Transaction`] when redb cannot start it.
    pub fn write_txn(&mut self) -> Result<WriteTransaction, LlmCredentialError> {
        Ok(self.db.begin_write()?)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A resolved credential together with its opened provider key.
///
/// The key is in a wrapper that clears itself on drop, so it lives only as
/// long as the call that signs a provider request with it.
pub type OpenedCredential = (CredentialStatus, Zeroizing<Vec<u8>>);

/// What a tenant sees about their installed credential: enough to tell which
/// key is in place, never enough to use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialStatus {
    pub provider: String,
    pub model: String,
    pub fingerprint: String,
    pub created_at_unix_nanos: u64,
    /// The space this answer came from. [`SPACE_BASE`] means the credential is
    /// the namespace's, which is what a caller needs to know before deciding
    /// whether removing it would affect other spaces.
    pub space: [u8; 16],
}

impl CredentialStatus {
    fn from_row(row: &LlmCredentialRow, space: [u8; 16]) -> Self {
        Self {
            provider: row.provider.clone(),
            model: row.model.clone(),
            fingerprint: row.fingerprint.clone(),
            created_at_unix_nanos: row.created_at_unix_nanos,
            space,
        }
    }
}

/// Register (or replace) the credential at exactly `scope`.
///
/// The key is sealed against the scope's namespace and the provider, so the
/// stored row cannot be moved to another tenant or another provider and still
/// open.
///
/// # Errors
/// [`LlmCredentialError`] on a storage failure or if sealing fails.
pub fn llm_credential_put(
    wtxn: &WriteTransaction,
    ring: &KeyRing,
    scope: DeclScope<'_>,
    provider: &str,
    model: &str,
    api_key: &str,
    now_unix_nanos: u64,
) -> Result<CredentialStatus, LlmCredentialError> {
    let sealed = ring.seal(
        api_key.as_bytes(),
        Binding {
            namespace: scope.namespace(),
            provider,
        },
    )?;

    let row = LlmCredentialRow {
        provider: provider.to_string(),
        model: model.to_string(),
        sealed,
        fingerprint: brain_crypto::fingerprint(api_key),
        created_at_unix_nanos: now_unix_nanos,
    };

    {
        let mut t = wtxn.open_table(LLM_CREDENTIALS_TABLE)?;
        t.insert(&(scope.namespace(), scope.space_id()), &row)?;
    }

    Ok(CredentialStatus::from_row(&row, scope.space_id()))
}

/// What is installed at exactly `scope`, without walking the ladder.
///
/// This is the shape a "remove my key" or "which scope owns this" question
/// needs: a space must be able to tell its own credential from the one it
/// merely inherits.
///
/// # Errors
/// [`LlmCredentialError`] on a storage failure.
pub fn llm_credential_get_exact(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
) -> Result<Option<CredentialStatus>, LlmCredentialError> {
    let t = rtxn.open_table(LLM_CREDENTIALS_TABLE)?;
    Ok(t.get(&(scope.namespace(), scope.space_id()))?
        .map(|v| CredentialStatus::from_row(&v.value(), scope.space_id())))
}

/// The credential a caller in `space` resolves, walking narrowest-first.
///
/// # Errors
/// [`LlmCredentialError`] on a storage failure.
pub fn llm_credential_resolve(
    rtxn: &ReadTransaction,
    namespace: &str,
    space: [u8; 16],
) -> Result<Option<CredentialStatus>, LlmCredentialError> {
    let t = rtxn.open_table(LLM_CREDENTIALS_TABLE)?;
    for sp in space_chain(space) {
        if let Some(v) = t.get(&(namespace, sp))? {
            return Ok(Some(CredentialStatus::from_row(&v.value(), sp)));
        }
    }
    Ok(None)
}

/// Resolve and open the credential a caller in `space` should use.
///
/// The plaintext comes back in a wrapper that clears itself on drop, so the
/// key lives only as long as the call that signs a provider request with it.
///
/// # Errors
/// [`LlmCredentialError::Sealed`] when the sealing key is no longer loaded;
/// [`LlmCredentialError`] on a storage failure.
pub fn llm_credential_open(
    rtxn: &ReadTransaction,
    ring: &KeyRing,
    namespace: &str,
    space: [u8; 16],
) -> Result<Option<OpenedCredential>, LlmCredentialError> {
    let t = rtxn.open_table(LLM_CREDENTIALS_TABLE)?;
    for sp in space_chain(space) {
        if let Some(v) = t.get(&(namespace, sp))? {
            let row = v.value();
            let key = ring.open(
                &row.sealed,
                Binding {
                    namespace,
                    provider: &row.provider,
                },
            )?;
            return Ok(Some((CredentialStatus::from_row(&row, sp), key)));
        }
    }
    Ok(None)
}

/// Remove the credential at exactly `scope`.
///
/// Reports whether a row existed rather than always succeeding, so a caller
/// revoking a key during an incident can tell "revoked" from "there was
/// nothing there".
///
/// # Errors
/// [`LlmCredentialError`] on a storage failure.
pub fn llm_credential_delete(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
) -> Result<bool, LlmCredentialError> {
    let mut t = wtxn.open_table(LLM_CREDENTIALS_TABLE)?;
    let existed = t.remove(&(scope.namespace(), scope.space_id()))?.is_some();
    Ok(existed)
}

/// Every scope in `namespace` that has a credential of its own, narrowest
/// scopes included. The namespace base is reported as [`SPACE_BASE`].
///
/// # Errors
/// [`LlmCredentialError`] on a storage failure.
pub fn llm_credential_list(
    rtxn: &ReadTransaction,
    namespace: &str,
) -> Result<Vec<CredentialStatus>, LlmCredentialError> {
    let t = rtxn.open_table(LLM_CREDENTIALS_TABLE)?;
    let mut out = Vec::new();
    for entry in t.range((namespace, SPACE_BASE)..=(namespace, [0xffu8; 16]))? {
        let (k, v) = entry?;
        let (ns, sp) = k.value();
        if ns != namespace {
            break;
        }
        out.push(CredentialStatus::from_row(&v.value(), sp));
    }
    Ok(out)
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use brain_crypto::generate_kek;

    const NS: &str = "acme";
    const OTHER_NS: &str = "globex";
    const TEAM: [u8; 16] = [7u8; 16];
    const OTHER_TEAM: [u8; 16] = [9u8; 16];
    const NOW: u64 = 1_700_000_000_000_000_000;

    struct Fixture {
        db: LlmCredentialDb,
        ring: KeyRing,
        _dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let db = LlmCredentialDb::open(dir.path().join("llm_credentials.redb")).unwrap();
            let ring = KeyRing::new(1, generate_kek().unwrap().as_ref()).unwrap();
            Self {
                db,
                ring,
                _dir: dir,
            }
        }

        fn put(&mut self, scope: DeclScope<'_>, provider: &str, model: &str, key: &str) {
            let wtxn = self.db.write_txn().unwrap();
            llm_credential_put(&wtxn, &self.ring, scope, provider, model, key, NOW).unwrap();
            wtxn.commit().unwrap();
        }

        fn resolve(&self, namespace: &str, space: [u8; 16]) -> Option<CredentialStatus> {
            let rtxn = self.db.read_txn().unwrap();
            llm_credential_resolve(&rtxn, namespace, space).unwrap()
        }

        fn exact(&self, scope: DeclScope<'_>) -> Option<CredentialStatus> {
            let rtxn = self.db.read_txn().unwrap();
            llm_credential_get_exact(&rtxn, scope).unwrap()
        }

        fn open(&self, namespace: &str, space: [u8; 16]) -> Option<String> {
            let rtxn = self.db.read_txn().unwrap();
            llm_credential_open(&rtxn, &self.ring, namespace, space)
                .unwrap()
                .map(|(_, key)| String::from_utf8(key.to_vec()).unwrap())
        }

        fn delete(&mut self, scope: DeclScope<'_>) -> bool {
            let wtxn = self.db.write_txn().unwrap();
            let removed = llm_credential_delete(&wtxn, scope).unwrap();
            wtxn.commit().unwrap();
            removed
        }
    }

    #[test]
    fn a_namespace_with_no_credential_resolves_to_nothing() {
        let f = Fixture::new();
        assert!(f.resolve(NS, SPACE_BASE).is_none());
        assert!(f.resolve(NS, TEAM).is_none());
    }

    #[test]
    fn a_registered_key_comes_back_usable() {
        let mut f = Fixture::new();
        f.put(
            DeclScope::base(NS),
            "openai",
            "gpt-4.1-mini",
            "sk-live-acme",
        );

        let status = f.resolve(NS, SPACE_BASE).unwrap();
        assert_eq!(status.provider, "openai");
        assert_eq!(status.model, "gpt-4.1-mini");
        assert_eq!(status.space, SPACE_BASE);
        assert_eq!(f.open(NS, SPACE_BASE).unwrap(), "sk-live-acme");
    }

    #[test]
    fn a_space_with_no_key_of_its_own_uses_the_namespaces() {
        let mut f = Fixture::new();
        f.put(DeclScope::base(NS), "openai", "gpt-4.1-mini", "sk-tenant");

        let status = f.resolve(NS, TEAM).unwrap();
        assert_eq!(
            status.space, SPACE_BASE,
            "the answer must name the scope it came from"
        );
        assert_eq!(f.open(NS, TEAM).unwrap(), "sk-tenant");
    }

    #[test]
    fn a_spaces_own_key_shadows_the_namespaces_for_that_space_alone() {
        let mut f = Fixture::new();
        f.put(DeclScope::base(NS), "openai", "gpt-4.1-mini", "sk-tenant");
        f.put(
            DeclScope::space(NS, TEAM),
            "anthropic",
            "claude-haiku-4-5",
            "sk-team",
        );

        let team = f.resolve(NS, TEAM).unwrap();
        assert_eq!(team.provider, "anthropic");
        assert_eq!(team.space, TEAM);
        assert_eq!(f.open(NS, TEAM).unwrap(), "sk-team");

        let sibling = f.resolve(NS, OTHER_TEAM).unwrap();
        assert_eq!(
            sibling.provider, "openai",
            "a sibling space must not see another space's key"
        );
        assert_eq!(f.open(NS, OTHER_TEAM).unwrap(), "sk-tenant");
        assert_eq!(f.open(NS, SPACE_BASE).unwrap(), "sk-tenant");
    }

    #[test]
    fn a_write_lands_only_where_it_names() {
        let mut f = Fixture::new();
        f.put(DeclScope::base(NS), "openai", "gpt-4.1-mini", "sk-tenant");
        f.put(DeclScope::space(NS, TEAM), "openai", "gpt-4.1", "sk-team");

        assert_eq!(
            f.exact(DeclScope::base(NS)).unwrap().model,
            "gpt-4.1-mini",
            "registering a space key must not overwrite the namespace's"
        );
        assert_eq!(
            f.exact(DeclScope::space(NS, TEAM)).unwrap().model,
            "gpt-4.1"
        );
    }

    #[test]
    fn an_exact_read_tells_an_owned_key_from_an_inherited_one() {
        let mut f = Fixture::new();
        f.put(DeclScope::base(NS), "openai", "gpt-4.1-mini", "sk-tenant");

        assert!(
            f.exact(DeclScope::space(NS, TEAM)).is_none(),
            "a space that only inherits owns nothing to remove"
        );
        assert!(f.resolve(NS, TEAM).is_some());
    }

    #[test]
    fn registering_again_replaces_the_key_in_place() {
        let mut f = Fixture::new();
        f.put(DeclScope::base(NS), "openai", "gpt-4.1-mini", "sk-first");
        f.put(DeclScope::base(NS), "openai", "gpt-4.1", "sk-second");

        assert_eq!(f.open(NS, SPACE_BASE).unwrap(), "sk-second");
        assert_eq!(f.exact(DeclScope::base(NS)).unwrap().model, "gpt-4.1");
    }

    #[test]
    fn removing_a_spaces_key_leaves_the_namespace_standing() {
        let mut f = Fixture::new();
        f.put(DeclScope::base(NS), "openai", "gpt-4.1-mini", "sk-tenant");
        f.put(DeclScope::space(NS, TEAM), "openai", "gpt-4.1", "sk-team");

        assert!(f.delete(DeclScope::space(NS, TEAM)));
        assert_eq!(
            f.open(NS, TEAM).unwrap(),
            "sk-tenant",
            "the space falls back to the tenant key"
        );
    }

    #[test]
    fn removing_the_namespaces_key_leaves_a_spaces_own_standing() {
        let mut f = Fixture::new();
        f.put(DeclScope::base(NS), "openai", "gpt-4.1-mini", "sk-tenant");
        f.put(DeclScope::space(NS, TEAM), "openai", "gpt-4.1", "sk-team");

        assert!(f.delete(DeclScope::base(NS)));
        assert_eq!(f.open(NS, TEAM).unwrap(), "sk-team");
        assert!(
            f.resolve(NS, OTHER_TEAM).is_none(),
            "a space that only inherited now resolves nothing"
        );
    }

    #[test]
    fn a_delete_reports_whether_anything_was_revoked() {
        let mut f = Fixture::new();
        assert!(
            !f.delete(DeclScope::base(NS)),
            "nothing was there to revoke"
        );
        f.put(DeclScope::base(NS), "openai", "gpt-4.1-mini", "sk");
        assert!(f.delete(DeclScope::base(NS)));
        assert!(!f.delete(DeclScope::base(NS)));
    }

    #[test]
    fn one_tenants_key_is_invisible_to_another() {
        let mut f = Fixture::new();
        f.put(DeclScope::base(NS), "openai", "gpt-4.1-mini", "sk-acme");

        assert!(
            f.resolve(OTHER_NS, SPACE_BASE).is_none(),
            "a namespace resolves only its own credential"
        );
    }

    #[test]
    fn a_row_moved_between_tenants_fails_to_open_rather_than_leaking() {
        let mut f = Fixture::new();
        f.put(DeclScope::base(NS), "openai", "gpt-4.1-mini", "sk-acme");

        // Simulate row-level write access to the store: copy one tenant's
        // stored row verbatim into another tenant's slot.
        let stolen = {
            let rtxn = f.db.read_txn().unwrap();
            let t = rtxn.open_table(LLM_CREDENTIALS_TABLE).unwrap();
            t.get(&(NS, SPACE_BASE)).unwrap().unwrap().value()
        };
        {
            let wtxn = f.db.write_txn().unwrap();
            {
                let mut t = wtxn.open_table(LLM_CREDENTIALS_TABLE).unwrap();
                t.insert(&(OTHER_NS, SPACE_BASE), &stolen).unwrap();
            }
            wtxn.commit().unwrap();
        }

        let rtxn = f.db.read_txn().unwrap();
        assert!(
            llm_credential_open(&rtxn, &f.ring, OTHER_NS, SPACE_BASE).is_err(),
            "the sealing binding must reject a replayed row"
        );
    }

    #[test]
    fn a_credential_survives_reopening_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llm_credentials.redb");
        let ring = KeyRing::new(1, generate_kek().unwrap().as_ref()).unwrap();

        {
            let mut db = LlmCredentialDb::open(&path).unwrap();
            let wtxn = db.write_txn().unwrap();
            llm_credential_put(
                &wtxn,
                &ring,
                DeclScope::base(NS),
                "openai",
                "gpt-4.1-mini",
                "sk-durable",
                NOW,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }

        let db = LlmCredentialDb::open(&path).unwrap();
        let rtxn = db.read_txn().unwrap();
        let (_, key) = llm_credential_open(&rtxn, &ring, NS, SPACE_BASE)
            .unwrap()
            .unwrap();
        assert_eq!(key.as_slice(), b"sk-durable");
    }

    #[test]
    fn a_listing_shows_every_scope_that_owns_a_key_and_no_others() {
        let mut f = Fixture::new();
        f.put(DeclScope::base(NS), "openai", "gpt-4.1-mini", "sk-tenant");
        f.put(DeclScope::space(NS, TEAM), "openai", "gpt-4.1", "sk-team");
        f.put(DeclScope::base(OTHER_NS), "openai", "gpt-4.1", "sk-other");

        let rtxn = f.db.read_txn().unwrap();
        let rows = llm_credential_list(&rtxn, NS).unwrap();
        let mut spaces: Vec<_> = rows.iter().map(|r| r.space).collect();
        spaces.sort_unstable();
        assert_eq!(
            spaces,
            vec![SPACE_BASE, TEAM],
            "a listing is scoped to its own namespace"
        );
    }

    #[test]
    fn a_status_never_carries_the_key() {
        let mut f = Fixture::new();
        f.put(
            DeclScope::base(NS),
            "openai",
            "gpt-4.1-mini",
            "sk-super-secret-value",
        );
        let status = f.resolve(NS, SPACE_BASE).unwrap();
        let rendered = format!("{status:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert_eq!(status.fingerprint, "…alue");
    }

    /// An access facade over a fresh store and a fresh ring.
    fn fresh_access(dir: &tempfile::TempDir) -> LlmCredentialAccess {
        LlmCredentialAccess::new(
            LlmCredentialDb::open(dir.path().join("llm_credentials.redb")).unwrap(),
            Arc::new(KeyRing::new(1, generate_kek().unwrap().as_ref()).unwrap()),
        )
    }

    #[test]
    fn an_empty_store_audits_clean() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(fresh_access(&dir).audit_openable().unwrap(), (0, 0));
    }

    #[test]
    fn a_store_restored_without_its_sealing_key_audits_as_wholly_unopenable() {
        // The backup case: `llm_credentials.redb` came back, the key file did
        // not, so Brain generated a fresh one. Every row is now ciphertext
        // nothing can open — and resolution reports that as "unavailable",
        // which admits writes. Without this audit the deployment serves every
        // write and fails derivation on all of them, forever, in silence.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llm_credentials.redb");
        {
            let original = KeyRing::new(1, generate_kek().unwrap().as_ref()).unwrap();
            let mut db = LlmCredentialDb::open(&path).unwrap();
            let wtxn = db.write_txn().unwrap();
            llm_credential_put(
                &wtxn,
                &original,
                DeclScope::base(NS),
                "openai",
                "gpt-4.1-mini",
                "sk-live",
                NOW,
            )
            .unwrap();
            llm_credential_put(
                &wtxn,
                &original,
                DeclScope::base(OTHER_NS),
                "openai",
                "gpt-4.1-mini",
                "sk-live-2",
                NOW,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }

        let regenerated = KeyRing::new(1, generate_kek().unwrap().as_ref()).unwrap();
        let access =
            LlmCredentialAccess::new(LlmCredentialDb::open(&path).unwrap(), Arc::new(regenerated));
        assert_eq!(
            access.audit_openable().unwrap(),
            (2, 0),
            "two rows present, none openable — the signal a boot must refuse on"
        );
    }

    #[test]
    fn a_store_with_its_own_key_audits_fully_openable() {
        let dir = tempfile::tempdir().unwrap();
        let access = fresh_access(&dir);
        access
            .put(DeclScope::base(NS), "openai", "gpt-4.1-mini", "sk-a", NOW)
            .unwrap();
        access
            .put(
                DeclScope::base(OTHER_NS),
                "openai",
                "gpt-4.1-mini",
                "sk-b",
                NOW,
            )
            .unwrap();
        assert_eq!(access.audit_openable().unwrap(), (2, 2));
    }

    #[test]
    fn a_credential_cannot_be_opened_once_its_sealing_key_is_gone() {
        let mut f = Fixture::new();
        f.put(DeclScope::base(NS), "openai", "gpt-4.1-mini", "sk-acme");

        let other_ring = KeyRing::new(1, generate_kek().unwrap().as_ref()).unwrap();
        let rtxn = f.db.read_txn().unwrap();
        assert!(
            llm_credential_open(&rtxn, &other_ring, NS, SPACE_BASE).is_err(),
            "a different key-encryption key must not open the row"
        );
    }
}

// ---------------------------------------------------------------------------
// Access facade
// ---------------------------------------------------------------------------

/// The credential store together with the keys that open it.
///
/// These two travel as one value because neither is useful alone: a store
/// without its key ring can hold credentials it cannot serve, and a key ring
/// without a store has nothing to open. Handing them around separately made
/// that pairing a convention; here it is the type.
///
/// The facade also owns transaction handling, so a caller registering or
/// revoking a credential does not open one and cannot forget to commit.
pub struct LlmCredentialAccess {
    store: RwLock<LlmCredentialDb>,
    ring: Arc<KeyRing>,
}

impl std::fmt::Debug for LlmCredentialAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmCredentialAccess")
            .finish_non_exhaustive()
    }
}

impl LlmCredentialAccess {
    #[must_use]
    pub fn new(store: LlmCredentialDb, ring: Arc<KeyRing>) -> Self {
        Self {
            store: RwLock::new(store),
            ring,
        }
    }

    /// Register or replace the credential at exactly `scope`.
    ///
    /// Reports whether a credential was already registered at that scope, so
    /// a caller can tell a first registration from a rotation.
    ///
    /// # Errors
    /// [`LlmCredentialError`] on a storage failure or if sealing fails.
    pub fn put(
        &self,
        scope: DeclScope<'_>,
        provider: &str,
        model: &str,
        api_key: &str,
        now_unix_nanos: u64,
    ) -> Result<(CredentialStatus, bool), LlmCredentialError> {
        let mut guard = self.store.write();
        let replaced = {
            let rtxn = guard.read_txn()?;
            llm_credential_get_exact(&rtxn, scope)?.is_some()
        };
        let wtxn = guard.write_txn()?;
        let status = llm_credential_put(
            &wtxn,
            &self.ring,
            scope,
            provider,
            model,
            api_key,
            now_unix_nanos,
        )?;
        wtxn.commit()?;
        Ok((status, replaced))
    }

    /// The credential a caller in `space` resolves, walking narrowest-first.
    ///
    /// # Errors
    /// [`LlmCredentialError`] on a storage failure.
    pub fn resolve(
        &self,
        namespace: &str,
        space: [u8; 16],
    ) -> Result<Option<CredentialStatus>, LlmCredentialError> {
        let guard = self.store.read();
        let rtxn = guard.read_txn()?;
        llm_credential_resolve(&rtxn, namespace, space)
    }

    /// What is registered at exactly `scope`, without walking the ladder.
    ///
    /// # Errors
    /// [`LlmCredentialError`] on a storage failure.
    pub fn get_exact(
        &self,
        scope: DeclScope<'_>,
    ) -> Result<Option<CredentialStatus>, LlmCredentialError> {
        let guard = self.store.read();
        let rtxn = guard.read_txn()?;
        llm_credential_get_exact(&rtxn, scope)
    }

    /// Resolve and open the credential a caller in `space` should use.
    ///
    /// # Errors
    /// [`LlmCredentialError`] on a storage failure, or when the sealing key
    /// is no longer loaded.
    pub fn open(
        &self,
        namespace: &str,
        space: [u8; 16],
    ) -> Result<Option<OpenedCredential>, LlmCredentialError> {
        let guard = self.store.read();
        let rtxn = guard.read_txn()?;
        llm_credential_open(&rtxn, &self.ring, namespace, space)
    }

    /// Remove the credential at exactly `scope`, reporting whether one was
    /// there to remove.
    ///
    /// # Errors
    /// [`LlmCredentialError`] on a storage failure.
    pub fn delete(&self, scope: DeclScope<'_>) -> Result<bool, LlmCredentialError> {
        let mut guard = self.store.write();
        let wtxn = guard.write_txn()?;
        let existed = llm_credential_delete(&wtxn, scope)?;
        wtxn.commit()?;
        Ok(existed)
    }

    /// Whether every stored credential opens under the loaded key ring, and
    /// how many there are.
    ///
    /// Exists for one failure that is otherwise invisible. A deployment
    /// restored from a backup that carried this store but not its sealing key
    /// generates a fresh key on boot, and then every row is ciphertext nothing
    /// can open. Resolution reports that as "could not find out right now",
    /// which **admits** writes — correctly, since a storage fault is not
    /// evidence of a missing key — so derivation fails on every write, forever,
    /// with no signal a tenant or operator would notice.
    ///
    /// Returns `(total, openable)`. A store with rows where `openable == 0` is
    /// that restore, and the caller should refuse to start rather than serve
    /// writes it can never ground.
    ///
    /// # Errors
    /// [`LlmCredentialError`] on a storage failure.
    pub fn audit_openable(&self) -> Result<(usize, usize), LlmCredentialError> {
        let guard = self.store.read();
        let rtxn = guard.read_txn()?;
        let t = rtxn.open_table(LLM_CREDENTIALS_TABLE)?;
        let (mut total, mut openable) = (0usize, 0usize);
        for entry in t.iter()? {
            let (k, v) = entry?;
            let (namespace, _) = k.value();
            let row = v.value();
            total += 1;
            if self
                .ring
                .open(
                    &row.sealed,
                    Binding {
                        namespace,
                        provider: &row.provider,
                    },
                )
                .is_ok()
            {
                openable += 1;
            }
        }
        Ok((total, openable))
    }

    /// Every scope in `namespace` that owns a credential.
    ///
    /// # Errors
    /// [`LlmCredentialError`] on a storage failure.
    pub fn list(&self, namespace: &str) -> Result<Vec<CredentialStatus>, LlmCredentialError> {
        let guard = self.store.read();
        let rtxn = guard.read_txn()?;
        llm_credential_list(&rtxn, namespace)
    }
}
