//! Model fingerprint computation.
//!
//! Every memory's stored fingerprint
//! depends on this; changing the algorithm orphans every stored vector.
//! See `crates/brain-metadata/src/tables/model_fingerprint.rs` for the
//! storage side.
//!
//! ## Algorithm
//!
//! ```text
//! BLAKE3(  b"config.json:"
//!        + config_bytes
//!        + b"tokenizer.json:"
//!        + tokenizer_bytes
//!        + b"weights:"
//!        + BLAKE3(weights_file)   // 32 bytes, the full BLAKE3
//!        + b"vector_dim:"
//!        + vector_dim.to_le_bytes()
//!        + b"normalize:"
//!        + [normalize as u8]
//!     )[..16]
//! ```
//!
//! Note: `weights_file` is hashed *separately* into 32 bytes (the full
//! BLAKE3 output), then those 32 bytes are appended to the outer hasher.

use std::io::Read;
use std::path::Path;

/// Compute the model fingerprint from its component bytes.
///
/// `weights_blake3` is the BLAKE3 hash of the weights file (32 raw
/// bytes — the full BLAKE3 output, NOT truncated). Caller computes it
/// via [`blake3_hash_file`] or equivalent.
///
/// `vector_dim` is the model's output dim (= 384 for BGE-small).
/// `normalize` is `true` for Brain (we always L2-normalise).
///
/// Returns the 16-byte truncated BLAKE3.
#[must_use]
pub fn compute_fingerprint(
    config_bytes: &[u8],
    tokenizer_bytes: &[u8],
    weights_blake3: &[u8; 32],
    vector_dim: u32,
    normalize: bool,
) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"config.json:");
    hasher.update(config_bytes);
    hasher.update(b"tokenizer.json:");
    hasher.update(tokenizer_bytes);
    hasher.update(b"weights:");
    hasher.update(weights_blake3);
    hasher.update(b"vector_dim:");
    hasher.update(&vector_dim.to_le_bytes());
    hasher.update(b"normalize:");
    hasher.update(&[u8::from(normalize)]);
    let full = hasher.finalize();
    full.as_bytes()[..16]
        .try_into()
        .expect("invariant: BLAKE3 digest is 32 bytes, so the 16-byte prefix exists")
}

/// Compute the BLAKE3-truncated-16 of a text string. The cache in
/// `crate::dispatcher::cache` uses this as its key — 16 bytes are enough
/// that collision probability at 10⁶ entries is ≈ 10⁻¹⁹.
#[must_use]
pub fn blake3_hash_text(text: &str) -> [u8; 16] {
    let full = blake3::hash(text.as_bytes());
    full.as_bytes()[..16]
        .try_into()
        .expect("invariant: BLAKE3 digest is 32 bytes, so the 16-byte prefix exists")
}

/// Compute the BLAKE3 of a file by streaming 64 KiB chunks. Avoids
/// loading the full ~130 MiB weights file into memory.
pub fn blake3_hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_round_trip_deterministic() {
        let a = compute_fingerprint(b"config", b"tokenizer", &[0x11; 32], 384, true);
        let b = compute_fingerprint(b"config", b"tokenizer", &[0x11; 32], 384, true);
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_differs_on_config_change() {
        let a = compute_fingerprint(b"config", b"tokenizer", &[0x11; 32], 384, true);
        let b = compute_fingerprint(b"CONFIG", b"tokenizer", &[0x11; 32], 384, true);
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_differs_on_weights_change() {
        let a = compute_fingerprint(b"config", b"tokenizer", &[0x11; 32], 384, true);
        let mut weights = [0x11u8; 32];
        weights[0] = 0x12;
        let b = compute_fingerprint(b"config", b"tokenizer", &weights, 384, true);
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_differs_on_dim_change() {
        let a = compute_fingerprint(b"config", b"tokenizer", &[0x11; 32], 384, true);
        let b = compute_fingerprint(b"config", b"tokenizer", &[0x11; 32], 768, true);
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_differs_on_normalize_change() {
        let a = compute_fingerprint(b"config", b"tokenizer", &[0x11; 32], 384, true);
        let b = compute_fingerprint(b"config", b"tokenizer", &[0x11; 32], 384, false);
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_known_vector() {
        // Pins 's byte ordering. Inputs were chosen to be
        // short and recognisable. The expected value below is what
        // the algorithm produces today; any change to the algorithm
        // (field order, separators, dim encoding, etc.) flips this.
        //
        // Recompute via:
        //   blake3 of:
        //     "config.json:" + "alpha" + "tokenizer.json:" + "beta"
        //     + "weights:" + 0x42 × 32 + "vector_dim:" + 384u32_le
        //     + "normalize:" + 0x01
        //   truncated to first 16 bytes.
        let fp = compute_fingerprint(b"alpha", b"beta", &[0x42; 32], 384, true);
        // Hex of expected:
        let expected: [u8; 16] = [
            0x3d, 0x94, 0x4d, 0xec, 0x35, 0x64, 0x3f, 0x80, 0xee, 0x3d, 0x43, 0xc1, 0xf0, 0xe7,
            0x01, 0x51,
        ];
        assert_eq!(
            fp, expected,
            "fingerprint algorithm changed; recompute the expected value and ensure no stored \
             fingerprints rely on the old algorithm"
        );
    }

    #[test]
    fn blake3_hash_text_deterministic_and_16_bytes() {
        let a = blake3_hash_text("hello");
        let b = blake3_hash_text("hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        let c = blake3_hash_text("hellO"); // case sensitive
        assert_ne!(a, c);
        let d = blake3_hash_text("");
        assert_ne!(a, d);
    }

}
