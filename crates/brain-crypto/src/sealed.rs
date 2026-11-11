//! Sealing and opening a tenant's LLM provider key.
//!
//! A tenant's OpenAI or Anthropic key bills to their account and is useful to
//! an attacker on its own, so Brain stores it sealed and holds the plaintext
//! only for the moment it takes to sign an outbound provider call. Nothing
//! that leaves this module can reconstruct a key: the credential store holds
//! ciphertext, and the read path returns a provider, a model and a display
//! fingerprint.
//!
//! # The binding is load-bearing
//!
//! Each secret is sealed against the namespace and provider it belongs to,
//! and that pair is authenticated alongside the ciphertext. Moving one
//! tenant's stored row into another tenant's slot therefore fails to open
//! rather than yielding a usable key, which is what keeps write access to the
//! credential table from becoming a way to spend someone else's provider
//! budget.
//!
//! # Rotation
//!
//! The id of the key-encryption key used for a seal travels in the clear
//! beside the ciphertext. Several key-encryption keys can be live at once, so
//! an operator can introduce a new one, have new seals use it, and leave
//! existing rows readable until they are re-sealed.

use std::collections::HashMap;
use std::fmt;

use aws_lc_rs::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305, NONCE_LEN};
use aws_lc_rs::rand::fill;
use zeroize::Zeroizing;

/// Length of a raw key-encryption key, in bytes.
pub const KEK_LEN: usize = 32;

/// Identifies which key-encryption key sealed a given secret, so that more
/// than one can be live during a rotation.
pub type KekId = u32;

/// What can go wrong sealing or opening a secret.
///
/// Deliberately coarse on the open path: a caller cannot tell a wrong key
/// from a tampered ciphertext from a secret that belongs to another tenant,
/// because that distinction would answer questions about data the caller is
/// not entitled to. Operators get the detail from logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    /// The supplied key material was not a valid key-encryption key.
    #[error("invalid key-encryption key: must be {KEK_LEN} raw bytes")]
    InvalidKek,
    /// No key-encryption key is loaded for the id this ciphertext names.
    #[error("no key-encryption key loaded for id {0}")]
    UnknownKekId(KekId),
    /// Sealing failed.
    #[error("sealing failed")]
    SealFailed,
    /// Opening failed: wrong key, tampered ciphertext, or a secret sealed for
    /// a different namespace or provider.
    #[error("opening failed")]
    OpenFailed,
}

/// What a secret is bound to. Both halves are authenticated with the
/// ciphertext, so a sealed secret only ever opens in the place it was sealed
/// for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding<'a> {
    /// The tenant namespace that owns the credential.
    pub namespace: &'a str,
    /// The provider the key authenticates against (`"openai"`, `"anthropic"`).
    pub provider: &'a str,
}

impl Binding<'_> {
    /// The authenticated byte string. The separator cannot appear in a
    /// namespace or a provider name, so two different bindings can never
    /// produce the same bytes.
    fn as_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.namespace.len() + self.provider.len() + 1);
        out.extend_from_slice(self.namespace.as_bytes());
        out.push(0);
        out.extend_from_slice(self.provider.as_bytes());
        out
    }
}

/// A sealed secret, exactly as it is persisted.
///
/// Carries no plaintext and no key material. `kek_id` is in the clear on
/// purpose — it selects the key needed to open the row and reveals nothing
/// about its contents.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone, PartialEq, Eq)]
pub struct SealedSecret {
    /// Which key-encryption key sealed this.
    pub kek_id: KekId,
    /// Per-seal nonce.
    pub nonce: [u8; NONCE_LEN],
    /// Ciphertext with its authentication tag appended.
    pub ciphertext: Vec<u8>,
}

/// Prints shape only. A `Debug` that rendered the ciphertext would put tenant
/// key material into any log line that formats a credential row.
impl fmt::Debug for SealedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SealedSecret")
            .field("kek_id", &self.kek_id)
            .field("ciphertext_len", &self.ciphertext.len())
            .finish()
    }
}

/// The key-encryption keys this process can seal and open with.
///
/// Sealing always uses the active key; opening selects by the id stored on
/// the row, which is what lets a rotation proceed without rewriting every
/// credential first.
pub struct KeyRing {
    keys: HashMap<KekId, Zeroizing<[u8; KEK_LEN]>>,
    active: KekId,
}

/// Prints the loaded ids and nothing else.
impl fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut ids: Vec<_> = self.keys.keys().copied().collect();
        ids.sort_unstable();
        f.debug_struct("KeyRing")
            .field("loaded", &ids)
            .field("active", &self.active)
            .finish()
    }
}

impl KeyRing {
    /// Build a ring holding a single key, which becomes the active one.
    ///
    /// # Errors
    /// [`CryptoError::InvalidKek`] when `key` is not [`KEK_LEN`] bytes.
    pub fn new(id: KekId, key: &[u8]) -> Result<Self, CryptoError> {
        let mut ring = Self {
            keys: HashMap::new(),
            active: id,
        };
        ring.add(id, key)?;
        Ok(ring)
    }

    /// Add a key that opens existing rows without becoming the active one.
    ///
    /// # Errors
    /// [`CryptoError::InvalidKek`] when `key` is not [`KEK_LEN`] bytes.
    pub fn add(&mut self, id: KekId, key: &[u8]) -> Result<(), CryptoError> {
        let bytes: [u8; KEK_LEN] = key.try_into().map_err(|_| CryptoError::InvalidKek)?;
        self.keys.insert(id, Zeroizing::new(bytes));
        Ok(())
    }

    /// Make `id` the key new seals use. Callers switch to a key the ring
    /// already holds, so a rotation cannot leave the ring sealing under a key
    /// it is unable to open.
    ///
    /// # Errors
    /// [`CryptoError::UnknownKekId`] when `id` is not loaded.
    pub fn set_active(&mut self, id: KekId) -> Result<(), CryptoError> {
        if !self.keys.contains_key(&id) {
            return Err(CryptoError::UnknownKekId(id));
        }
        self.active = id;
        Ok(())
    }

    /// The id new seals are written under.
    #[must_use]
    pub const fn active(&self) -> KekId {
        self.active
    }

    /// Seal `plaintext` for `binding` under the active key.
    ///
    /// # Errors
    /// [`CryptoError::SealFailed`] if the system random source or the AEAD
    /// fails; [`CryptoError::UnknownKekId`] if the active key is somehow
    /// absent.
    pub fn seal(
        &self,
        plaintext: &[u8],
        binding: Binding<'_>,
    ) -> Result<SealedSecret, CryptoError> {
        let kek = self
            .keys
            .get(&self.active)
            .ok_or(CryptoError::UnknownKekId(self.active))?;

        let mut nonce = [0u8; NONCE_LEN];
        fill(&mut nonce).map_err(|_| CryptoError::SealFailed)?;

        let key = LessSafeKey::new(
            UnboundKey::new(&CHACHA20_POLY1305, kek.as_ref())
                .map_err(|_| CryptoError::InvalidKek)?,
        );

        let mut ciphertext = plaintext.to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(binding.as_bytes()),
            &mut ciphertext,
        )
        .map_err(|_| CryptoError::SealFailed)?;

        Ok(SealedSecret {
            kek_id: self.active,
            nonce,
            ciphertext,
        })
    }

    /// Open `sealed`, which must have been sealed for the same `binding`.
    ///
    /// The plaintext comes back in a wrapper that clears itself when dropped,
    /// so a key does not outlive the call that needed it.
    ///
    /// # Errors
    /// [`CryptoError::UnknownKekId`] when the sealing key is not loaded;
    /// [`CryptoError::OpenFailed`] for a wrong key, a tampered row, or a
    /// binding that does not match.
    pub fn open(
        &self,
        sealed: &SealedSecret,
        binding: Binding<'_>,
    ) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
        let kek = self
            .keys
            .get(&sealed.kek_id)
            .ok_or(CryptoError::UnknownKekId(sealed.kek_id))?;

        let key = LessSafeKey::new(
            UnboundKey::new(&CHACHA20_POLY1305, kek.as_ref())
                .map_err(|_| CryptoError::InvalidKek)?,
        );

        let mut buf = Zeroizing::new(sealed.ciphertext.clone());
        let len = key
            .open_in_place(
                Nonce::assume_unique_for_key(sealed.nonce),
                Aad::from(binding.as_bytes()),
                &mut buf,
            )
            .map_err(|_| CryptoError::OpenFailed)?
            .len();

        let mut plaintext = Zeroizing::new(buf.to_vec());
        plaintext.truncate(len);
        Ok(plaintext)
    }
}

/// Generate a fresh key-encryption key from the system random source.
///
/// # Errors
/// [`CryptoError::SealFailed`] if the system random source fails.
pub fn generate_kek() -> Result<Zeroizing<[u8; KEK_LEN]>, CryptoError> {
    let mut key = Zeroizing::new([0u8; KEK_LEN]);
    fill(key.as_mut()).map_err(|_| CryptoError::SealFailed)?;
    Ok(key)
}

/// A display-safe tail of a provider key, so an operator can tell *which* key
/// is installed without the key being readable again.
///
/// Shows the last four characters at most, which is too little to narrow a
/// search for the rest.
#[must_use]
pub fn fingerprint(secret: &str) -> String {
    let tail: String = secret
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = "acme";

    fn ring() -> KeyRing {
        KeyRing::new(1, generate_kek().unwrap().as_ref()).unwrap()
    }

    fn binding<'a>(namespace: &'a str, provider: &'a str) -> Binding<'a> {
        Binding {
            namespace,
            provider,
        }
    }

    #[test]
    fn a_sealed_key_opens_to_exactly_what_went_in() {
        let ring = ring();
        let b = binding(NS, "openai");
        let sealed = ring.seal(b"sk-live-secret", b).unwrap();
        assert_eq!(ring.open(&sealed, b).unwrap().as_slice(), b"sk-live-secret");
    }

    #[test]
    fn another_tenants_row_does_not_open() {
        let ring = ring();
        let sealed = ring.seal(b"sk-acme", binding("acme", "openai")).unwrap();
        assert_eq!(
            ring.open(&sealed, binding("globex", "openai")),
            Err(CryptoError::OpenFailed),
            "a row moved between tenants must fail rather than yield a key"
        );
    }

    #[test]
    fn a_row_does_not_open_under_a_different_provider() {
        let ring = ring();
        let sealed = ring.seal(b"sk-acme", binding(NS, "openai")).unwrap();
        assert_eq!(
            ring.open(&sealed, binding(NS, "anthropic")),
            Err(CryptoError::OpenFailed)
        );
    }

    #[test]
    fn a_tampered_row_does_not_open() {
        let ring = ring();
        let b = binding(NS, "openai");
        let mut sealed = ring.seal(b"sk-acme", b).unwrap();
        sealed.ciphertext[0] ^= 0xff;
        assert_eq!(ring.open(&sealed, b), Err(CryptoError::OpenFailed));
    }

    #[test]
    fn a_namespace_boundary_cannot_be_forged_by_shifting_the_separator() {
        // Without an unambiguous separator `("a", "bc")` and `("ab", "c")`
        // would authenticate the same bytes, and one provider slot would open
        // another's row.
        let ring = ring();
        let sealed = ring.seal(b"sk", binding("a", "bc")).unwrap();
        assert_eq!(
            ring.open(&sealed, binding("ab", "c")),
            Err(CryptoError::OpenFailed)
        );
    }

    #[test]
    fn rotation_keeps_existing_rows_readable() {
        let old = generate_kek().unwrap();
        let new = generate_kek().unwrap();
        let b = binding(NS, "openai");

        let mut ring = KeyRing::new(1, old.as_ref()).unwrap();
        let sealed_under_old = ring.seal(b"sk-old", b).unwrap();

        ring.add(2, new.as_ref()).unwrap();
        ring.set_active(2).unwrap();
        let sealed_under_new = ring.seal(b"sk-new", b).unwrap();

        assert_eq!(sealed_under_old.kek_id, 1);
        assert_eq!(sealed_under_new.kek_id, 2);
        assert_eq!(
            ring.open(&sealed_under_old, b).unwrap().as_slice(),
            b"sk-old"
        );
        assert_eq!(
            ring.open(&sealed_under_new, b).unwrap().as_slice(),
            b"sk-new"
        );
    }

    #[test]
    fn a_row_sealed_under_an_unloaded_key_names_the_id_it_needs() {
        let ring = ring();
        let b = binding(NS, "openai");
        let mut sealed = ring.seal(b"sk", b).unwrap();
        sealed.kek_id = 99;
        assert_eq!(ring.open(&sealed, b), Err(CryptoError::UnknownKekId(99)));
    }

    #[test]
    fn the_ring_cannot_be_made_to_seal_under_a_key_it_cannot_open() {
        let mut ring = ring();
        assert_eq!(ring.set_active(7), Err(CryptoError::UnknownKekId(7)));
        assert_eq!(ring.active(), 1);
    }

    #[test]
    fn a_key_of_the_wrong_length_is_rejected() {
        assert_eq!(
            KeyRing::new(1, &[0u8; 16]).err(),
            Some(CryptoError::InvalidKek)
        );
    }

    #[test]
    fn two_seals_of_one_secret_differ() {
        let ring = ring();
        let b = binding(NS, "openai");
        let a = ring.seal(b"sk-same", b).unwrap();
        let c = ring.seal(b"sk-same", b).unwrap();
        assert_ne!(a.nonce, c.nonce, "each seal takes a fresh nonce");
        assert_ne!(a.ciphertext, c.ciphertext);
    }

    #[test]
    fn debug_output_carries_no_ciphertext() {
        let ring = ring();
        let sealed = ring
            .seal(b"sk-super-secret", binding(NS, "openai"))
            .unwrap();
        let rendered = format!("{sealed:?}");
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(rendered.contains("ciphertext_len"), "{rendered}");
    }

    #[test]
    fn the_key_ring_debug_lists_ids_not_keys() {
        let ring = ring();
        let rendered = format!("{ring:?}");
        assert!(rendered.contains("active: 1"), "{rendered}");
        assert!(!rendered.contains("00"), "{rendered}");
    }

    #[test]
    fn a_fingerprint_shows_only_the_tail() {
        assert_eq!(fingerprint("sk-proj-abcdef1234"), "…1234");
        assert_eq!(fingerprint("ab"), "…ab");
        assert_eq!(fingerprint(""), "…");
    }
}
