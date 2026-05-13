//! TLS server configuration loader (sub-task 9.9).
//!
//! Spec §03/02 §2: TLS 1.3 only, ALPN `"brain/1"`.
//!
//! The crypto provider (`aws-lc-rs`) is installed once at process
//! startup via [`install_default_crypto_provider`]. rustls 0.23 dropped
//! the implicit provider — callers must pick one explicitly.

#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;

#[derive(Debug, thiserror::Error)]
pub enum TlsLoadError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("PEM parse error in {path}: {detail}")]
    Pem { path: PathBuf, detail: String },

    #[error("no certificates found in {path}")]
    NoCerts { path: PathBuf },

    #[error("no private key found in {path}")]
    NoKey { path: PathBuf },

    #[error("rustls config build failed: {0}")]
    Rustls(#[from] rustls::Error),
}

/// Install `aws-lc-rs` as the default rustls crypto provider for this
/// process. Idempotent: rustls returns an error if the default is
/// already set; we treat that as success.
pub fn install_default_crypto_provider() {
    // `install_default` returns Err if a provider is already installed.
    // That's fine — likely a prior call in the same process.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Load a PEM cert chain + PEM private key from disk and build a
/// rustls `ServerConfig` constrained to TLS 1.3 with ALPN `"brain/1"`.
///
/// Spec §03/02 §2.2 (TLS 1.3 only) + §2.6 (ALPN).
pub fn load_server_tls_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<Arc<ServerConfig>, TlsLoadError> {
    install_default_crypto_provider();

    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;

    let mut cfg = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    // Spec §03/02 §2.6 — `brain/1` for protocol v1.
    cfg.alpn_protocols.push(b"brain/1".to_vec());

    Ok(Arc::new(cfg))
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsLoadError> {
    let file = File::open(path).map_err(|source| TlsLoadError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();
    for entry in rustls_pemfile::certs(&mut reader) {
        let der = entry.map_err(|e| TlsLoadError::Pem {
            path: path.to_owned(),
            detail: e.to_string(),
        })?;
        out.push(der);
    }
    if out.is_empty() {
        return Err(TlsLoadError::NoCerts {
            path: path.to_owned(),
        });
    }
    Ok(out)
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsLoadError> {
    let file = File::open(path).map_err(|source| TlsLoadError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut reader = BufReader::new(file);
    // Try PKCS#8 first (the common modern format), then RSA / SEC1
    // fall-throughs. `private_key` returns the first private key found
    // regardless of its PEM tag.
    let key = rustls_pemfile::private_key(&mut reader)
        .map_err(|e| TlsLoadError::Pem {
            path: path.to_owned(),
            detail: e.to_string(),
        })?
        .ok_or_else(|| TlsLoadError::NoKey {
            path: path.to_owned(),
        })?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_self_signed(dir: &Path) -> (PathBuf, PathBuf) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .expect("rcgen self-signed");
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
        (cert_path, key_path)
    }

    #[test]
    fn loads_valid_self_signed_cert_and_key() {
        let dir = TempDir::new().unwrap();
        let (cert, key) = write_self_signed(dir.path());
        let cfg = load_server_tls_config(&cert, &key).expect("TLS config");
        assert_eq!(cfg.alpn_protocols.len(), 1);
        assert_eq!(cfg.alpn_protocols[0], b"brain/1");
    }

    #[test]
    fn missing_cert_file_returns_io_error() {
        let dir = TempDir::new().unwrap();
        let key = dir.path().join("key.pem");
        std::fs::write(&key, "dummy").unwrap();
        let err =
            load_server_tls_config(&dir.path().join("missing.pem"), &key).expect_err("should fail");
        assert!(matches!(err, TlsLoadError::Io { .. }));
    }

    #[test]
    fn empty_cert_file_returns_no_certs() {
        let dir = TempDir::new().unwrap();
        let (_, key) = write_self_signed(dir.path());
        let empty = dir.path().join("empty.pem");
        std::fs::write(&empty, "").unwrap();
        let err = load_server_tls_config(&empty, &key).expect_err("should fail");
        assert!(matches!(err, TlsLoadError::NoCerts { .. }));
    }
}
