//! # brain-crypto
//!
//! Sealing for the secrets Brain holds on a tenant's behalf — today, the LLM
//! provider keys that pay for extraction and HyPE.
//!
//! The crate exists as its own unit so that every use of a cipher in Brain is
//! in one place to review, and so that crates which never touch a secret do
//! not link a cryptographic implementation.

#![forbid(unsafe_code)]

pub mod kek_file;
pub mod sealed;

pub use kek_file::{from_supplied, from_supplied_hex, load_or_create, KekFileError, KEK_FILE_NAME};
pub use sealed::{
    fingerprint, generate_kek, Binding, CryptoError, KekId, KeyRing, SealedSecret, KEK_LEN,
};
