//! `api_keys` table — scope-bound credentials.
//!
//! Each row is keyed by `BLAKE3(secret) → ApiKeyRow`. The plaintext
//! secret never lives on disk; clients send the secret on the wire and
//! the server hashes it to look up the row.
//!
//! A secondary index `api_keys_by_space` lets admin ops list every key
//! issued to a given space without scanning the primary table.

use crate::impl_redb_rkyv_value;
use redb::TableDefinition;

/// Primary table: `BLAKE3(secret_bytes)` → [`ApiKeyRow`].
pub const API_KEYS_TABLE: TableDefinition<'static, [u8; 32], ApiKeyRow> =
    TableDefinition::new("api_keys");

/// Secondary index: `(space_id, key_hash)` → `()`. Lets the admin
/// surface enumerate keys per space in O(log N) without scanning the
/// primary table.
pub const API_KEYS_BY_SPACE_TABLE: TableDefinition<'static, ([u8; 16], [u8; 32]), ()> =
    TableDefinition::new("api_keys_by_space");

/// Permission bitfield. The on-wire value is a `u32` so future
/// capabilities can land without a schema bump.
pub mod permissions {
    /// May ENCODE memories.
    pub const ENCODE: u32 = 1 << 0;
    /// May RECALL / QUERY / PLAN / REASON.
    pub const RECALL: u32 = 1 << 1;
    /// May FORGET memories.
    pub const FORGET: u32 = 1 << 2;
    /// May LINK / UNLINK edges.
    pub const LINK: u32 = 1 << 3;
    /// May upload schemas.
    pub const SCHEMA_UPLOAD: u32 = 1 << 4;
    /// May call ADMIN_* ops (including key minting and revocation).
    pub const ADMIN: u32 = 1 << 5;
    /// May run data-plane ops on behalf of another identity via the
    /// per-request `act_as` field, bounded by the key's `may_act`
    /// allowlist. Held only by a trusted service principal (edge or
    /// gateway) — never a normal space key, and never part of a default
    /// bundle; it is always explicitly granted.
    pub const ACT_AS: u32 = 1 << 6;

    /// Common bundles. A standard space gets read + write + link.
    pub const STANDARD_SPACE: u32 = ENCODE | RECALL | FORGET | LINK;
    /// Read-only observer (no mutation).
    pub const READ_ONLY: u32 = RECALL;
    /// Everything but admin.
    pub const READ_WRITE: u32 = ENCODE | RECALL | FORGET | LINK | SCHEMA_UPLOAD;
    /// Full powers including admin.
    pub const FULL: u32 = ENCODE | RECALL | FORGET | LINK | SCHEMA_UPLOAD | ADMIN;
}

/// One row in [`API_KEYS_TABLE`]. The plaintext secret never lives
/// here; `key_hash` is the BLAKE3 of the secret bytes that the holder
/// presents on AUTH.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyRow {
    /// BLAKE3 of the raw secret. Also the primary key — duplicated in
    /// the value so range scans don't need a join.
    pub key_hash: [u8; 32],
    /// Tenant identifier (organization-level scope).
    pub org_id: [u8; 16],
    /// Optional human/service identity. All-zero when the key is not
    /// bound to a specific user (machine credential).
    pub user_id: [u8; 16],
    /// Schema namespace this key is permitted to address (e.g.
    /// `"brain"` for the built-in noun set, `"acme"` for a tenant's
    /// custom schema). Empty string means "no namespace lock" (legacy
    /// keys; rejected when strict mode is on and the request requires
    /// a namespace).
    pub namespace: String,
    /// The space identity this key acts as. Every operation issued
    /// while authenticated with this key is stamped with this space.
    pub space_id: [u8; 16],
    /// Permission bitfield (see [`permissions`]).
    pub permissions: u32,
    /// Allowlist of namespaces this key may act *for* when it carries the
    /// [`permissions::ACT_AS`] bit and a request supplies an `act_as`
    /// target. Brain honors an `act_as` request only when the target
    /// namespace is in this set; a principal can never act as a namespace
    /// outside it. Empty for every non-`ACT_AS` key. A single `"*"` entry
    /// is the wildcard grant: the key may act as any namespace (the trusted
    /// gateway/edge front-door case, where the tenant set grows at runtime
    /// and can't be enumerated at mint time).
    pub may_act: Vec<String>,
    pub created_at_unix_nanos: u64,
    /// Updated by the auth path; eventually consistent (single
    /// background touch). Zero on a never-used key.
    pub last_used_at_unix_nanos: u64,
    /// Once flipped, the row stays in the table (for audit) but auth
    /// rejects on lookup.
    pub revoked: bool,
}

impl ApiKeyRow {
    #[must_use]
    pub fn new(
        key_hash: [u8; 32],
        org_id: [u8; 16],
        user_id: [u8; 16],
        namespace: String,
        space_id: [u8; 16],
        permissions: u32,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            key_hash,
            org_id,
            user_id,
            namespace,
            space_id,
            permissions,
            may_act: Vec::new(),
            created_at_unix_nanos,
            last_used_at_unix_nanos: 0,
            revoked: false,
        }
    }

    /// True iff `op` is permitted under this key.
    #[must_use]
    pub fn allows(&self, op: u32) -> bool {
        self.permissions & op == op
    }
}

impl_redb_rkyv_value!(ApiKeyRow, "brain_metadata::ApiKeyRow");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    fn sample(byte: u8) -> ApiKeyRow {
        let mut hash = [0u8; 32];
        hash[31] = byte;
        let mut org = [0u8; 16];
        org[15] = byte;
        let mut user = [0u8; 16];
        user[14] = byte;
        let mut space = [0u8; 16];
        space[13] = byte;
        ApiKeyRow::new(
            hash,
            org,
            user,
            "acme".into(),
            space,
            permissions::STANDARD_SPACE,
            1_700_000_000_000_000_000,
        )
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let row = sample(7);
        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(API_KEYS_TABLE).unwrap();
            t.insert(&row.key_hash, &row).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(API_KEYS_TABLE).unwrap();
        assert_eq!(t.get(&row.key_hash).unwrap().unwrap().value(), row);
    }

    #[test]
    fn allows_checks_bitfield() {
        let mut row = sample(1);
        row.permissions = permissions::ENCODE | permissions::RECALL;
        assert!(row.allows(permissions::ENCODE));
        assert!(row.allows(permissions::RECALL));
        assert!(!row.allows(permissions::FORGET));
        assert!(!row.allows(permissions::ADMIN));
        // Compound: must have all requested bits.
        assert!(row.allows(permissions::ENCODE | permissions::RECALL));
        assert!(!row.allows(permissions::ENCODE | permissions::ADMIN));
    }
}
