//! Where a deployment's key-encryption keys come from.
//!
//! Brain generates a key-encryption key the first time it needs one and keeps
//! it in the data directory, so a fresh install stores tenant provider keys
//! sealed without the operator configuring anything. An operator who wants
//! the sealing key held somewhere Brain's data is not — a secret manager, an
//! injected environment variable, a mounted file — supplies it instead and
//! the generated path is never taken.
//!
//! # What this protects, stated plainly
//!
//! A generated key sits in the same directory as the rows it seals. That
//! defends against the ways credential ciphertext ordinarily escapes — a
//! database file copied out, a backup archive, a snapshot restored somewhere
//! else, anything that greps the metadata store — because none of those carry
//! the key file with them. It does **not** defend against someone who has the
//! whole data directory; for that the key has to live somewhere Brain's disk
//! is not, which is what supplying one is for.
//!
//! # Rotation
//!
//! Keys are listed one per line as `<id> <hex>`, and the highest id is the
//! one new seals use. Appending a line rotates: new credentials seal under
//! the new key while existing rows stay readable under the old one, with no
//! coordinated rewrite.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

use crate::sealed::{generate_kek, KekId, KeyRing, KEK_LEN};

/// Name of the key file inside the data directory.
pub const KEK_FILE_NAME: &str = "llm_credential_kek";

/// What can go wrong loading or creating the key file.
#[derive(Debug, thiserror::Error)]
pub enum KekFileError {
    #[error("reading key-encryption key file at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("writing key-encryption key file at {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("key-encryption key file at {path} is malformed on line {line}: {reason}")]
    Malformed {
        path: PathBuf,
        line: usize,
        reason: &'static str,
    },

    #[error("key-encryption key file at {path} holds no keys")]
    Empty { path: PathBuf },

    #[error("generating a key-encryption key: {0}")]
    Generate(#[from] crate::sealed::CryptoError),
}

/// Load the key ring for `data_dir`, generating and persisting a key on the
/// first call for a deployment.
///
/// # Errors
/// [`KekFileError`] when the file cannot be read, is malformed, or a new key
/// cannot be written.
pub fn load_or_create(data_dir: &Path) -> Result<KeyRing, KekFileError> {
    let path = data_dir.join(KEK_FILE_NAME);
    if path.exists() {
        return load(&path);
    }
    create(&path)
}

/// Build a key ring from an operator-supplied key, bypassing the file
/// entirely. The id is fixed because a single supplied key has nothing to
/// rotate against; an operator rotating externally re-supplies the value.
///
/// # Errors
/// [`KekFileError::Generate`] when the bytes are not a valid key.
pub fn from_supplied(key: &[u8]) -> Result<KeyRing, KekFileError> {
    Ok(KeyRing::new(1, key)?)
}

/// Parse a hex-encoded key as an operator would supply it.
///
/// # Errors
/// [`KekFileError::Generate`] when the text is not [`KEK_LEN`] bytes of hex.
pub fn from_supplied_hex(hex: &str) -> Result<KeyRing, KekFileError> {
    let bytes = decode_hex(hex.trim()).ok_or(crate::sealed::CryptoError::InvalidKek)?;
    from_supplied(bytes.as_ref())
}

fn load(path: &Path) -> Result<KeyRing, KekFileError> {
    let text = fs::read_to_string(path).map_err(|source| KekFileError::Read {
        path: path.to_path_buf(),
        source,
    })?;

    let mut entries: Vec<(KekId, Zeroizing<[u8; KEK_LEN]>)> = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(id), Some(hex), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(KekFileError::Malformed {
                path: path.to_path_buf(),
                line: idx + 1,
                reason: "expected `<id> <hex>`",
            });
        };
        let id: KekId = id.parse().map_err(|_| KekFileError::Malformed {
            path: path.to_path_buf(),
            line: idx + 1,
            reason: "key id is not a number",
        })?;
        let bytes = decode_hex(hex).ok_or(KekFileError::Malformed {
            path: path.to_path_buf(),
            line: idx + 1,
            reason: "key is not 32 bytes of hex",
        })?;
        entries.push((id, bytes));
    }

    let Some(active) = entries.iter().map(|(id, _)| *id).max() else {
        return Err(KekFileError::Empty {
            path: path.to_path_buf(),
        });
    };

    let mut ring = KeyRing::new(active, entries[0].1.as_ref())?;
    for (id, bytes) in &entries {
        ring.add(*id, bytes.as_ref())?;
    }
    ring.set_active(active)?;
    Ok(ring)
}

fn create(path: &Path) -> Result<KeyRing, KekFileError> {
    let key = generate_kek()?;
    let body = format!(
        "# Brain seals tenant LLM provider keys with the keys in this file.\n\
         # Treat it as a secret: it is what makes the stored credentials readable.\n\
         # One key per line as `<id> <hex>`; the highest id seals new credentials.\n\
         1 {}\n",
        encode_hex(key.as_ref())
    );

    write_private(path, &body).map_err(|source| KekFileError::Write {
        path: path.to_path_buf(),
        source,
    })?;

    Ok(KeyRing::new(1, key.as_ref())?)
}

/// Write `body` so that only the owner can read it. The file holds the value
/// that makes every stored credential readable, so it is created with
/// owner-only permissions rather than narrowed afterwards — there is no window
/// in which it is world-readable.
fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(body.as_bytes())?;
    f.sync_all()
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn decode_hex(hex: &str) -> Option<Zeroizing<[u8; KEK_LEN]>> {
    if hex.len() != KEK_LEN * 2 {
        return None;
    }
    let mut out = Zeroizing::new([0u8; KEK_LEN]);
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sealed::Binding;

    const B: Binding<'static> = Binding {
        namespace: "acme",
        provider: "openai",
    };

    #[test]
    fn a_fresh_deployment_generates_a_key_and_reuses_it_next_boot() {
        let dir = tempfile::tempdir().unwrap();

        let first = load_or_create(dir.path()).unwrap();
        let sealed = first.seal(b"sk-live", B).unwrap();
        drop(first);

        let second = load_or_create(dir.path()).unwrap();
        assert_eq!(
            second.open(&sealed, B).unwrap().as_slice(),
            b"sk-live",
            "a restart must still open credentials sealed before it"
        );
    }

    #[test]
    fn the_generated_key_file_is_readable_only_by_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        load_or_create(dir.path()).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(KEK_FILE_NAME))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
        }
    }

    #[test]
    fn appending_a_higher_id_rotates_without_orphaning_existing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEK_FILE_NAME);

        let before = load_or_create(dir.path()).unwrap();
        let sealed_under_one = before.seal(b"sk-old", B).unwrap();
        assert_eq!(sealed_under_one.kek_id, 1);
        drop(before);

        let added = generate_kek().unwrap();
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(&format!("2 {}\n", encode_hex(added.as_ref())));
        fs::write(&path, text).unwrap();

        let after = load(&path).unwrap();
        assert_eq!(after.active(), 2, "the highest id seals new credentials");
        assert_eq!(
            after.open(&sealed_under_one, B).unwrap().as_slice(),
            b"sk-old",
            "rows sealed under the old key stay readable"
        );
        assert_eq!(after.seal(b"sk-new", B).unwrap().kek_id, 2);
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEK_FILE_NAME);
        let key = generate_kek().unwrap();
        fs::write(
            &path,
            format!("# a note\n\n  \n1 {}\n", encode_hex(key.as_ref())),
        )
        .unwrap();
        assert_eq!(load(&path).unwrap().active(), 1);
    }

    #[test]
    fn a_malformed_line_is_reported_with_its_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEK_FILE_NAME);
        let key = generate_kek().unwrap();
        fs::write(
            &path,
            format!("1 {}\nnot-a-key-line\n", encode_hex(key.as_ref())),
        )
        .unwrap();

        match load(&path) {
            Err(KekFileError::Malformed { line, .. }) => assert_eq!(line, 2),
            other => panic!("expected a malformed-line error, got {other:?}"),
        }
    }

    #[test]
    fn a_truncated_key_is_rejected_rather_than_padded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEK_FILE_NAME);
        fs::write(&path, "1 abcd\n").unwrap();
        assert!(matches!(
            load(&path),
            Err(KekFileError::Malformed { line: 1, .. })
        ));
    }

    #[test]
    fn a_file_with_no_keys_is_an_error_not_an_empty_ring() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEK_FILE_NAME);
        fs::write(&path, "# only a comment\n").unwrap();
        assert!(matches!(load(&path), Err(KekFileError::Empty { .. })));
    }

    #[test]
    fn an_operator_supplied_key_never_touches_the_data_directory() {
        let dir = tempfile::tempdir().unwrap();
        let key = generate_kek().unwrap();

        let ring = from_supplied_hex(&encode_hex(key.as_ref())).unwrap();
        ring.seal(b"sk-live", B).unwrap();

        assert!(
            !dir.path().join(KEK_FILE_NAME).exists(),
            "supplying a key must not also write one to disk"
        );
    }

    #[test]
    fn a_supplied_key_of_the_wrong_length_is_rejected() {
        assert!(from_supplied_hex("abcd").is_err());
        assert!(from_supplied(&[0u8; 16]).is_err());
    }
}
