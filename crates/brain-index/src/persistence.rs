//! Snapshot persistence for `HnswIndex`.
//!
//! See `spec/06_ann_index/06_persistence.md` §5 and SD-4.5-1 in
//! `docs/spec-deviations.md`.
//!
//! ## File layout
//!
//! A snapshot is a **directory** containing three files at the same
//! `basename` (SD-4.5-1: hnsw_rs's `Hnsw::file_dump` writes two files,
//! so we live with three rather than concatenating into one):
//!
//! - `<basename>.hnsw.graph` — hnsw_rs's graph dump.
//! - `<basename>.hnsw.data`  — hnsw_rs's data dump.
//! - `<basename>.brain`      — our wrapper (this module). Written
//!   **last** so its presence is the marker for "snapshot complete".
//!
//! ### `.brain` layout
//!
//! ```text
//! offset  size  field
//! ------  ----  -----
//!    0    4     magic = b"BHN0"
//!    4    4     format_version: u32 LE  (= 1)
//!    8    16    shard_uuid: [u8; 16]
//!   24    8     taken_at_lsn: u64 LE
//!   32    8     graph_node_count: u64 LE  (= IdMap.len at save time)
//!   40    4     m: u32 LE
//!   44    4     ef_construction: u32 LE
//!   48    4     ef_search: u32 LE
//!   52    4     ef_search_max: u32 LE
//!   56    4     vector_dim: u32 LE  (= D)
//!   60    4     header_crc32c: u32 LE  (CRC32C over bytes [0..60])
//!   ─────────────  64-byte header ends ──────────────
//!   64    4     id_map_count: u32 LE
//!   68   N×20   id_map entries: [u8; 16] memory_id + u32 LE internal_id
//!    .    4     next_internal_id: u32 LE
//!    .    8     tombstone_word_count: u64 LE
//!    .   M×8   tombstone bitmap: u64 LE words
//!    .    4     tombstone_set_count: u32 LE (TombstoneBitmap.count)
//!    .    8     footer: BLAKE3(file[..footer]) truncated to u64 LE
//! ```

use std::io::Read;

use crate::idmap::IdMap;
use crate::params::IndexParams;
use crate::tombstones::TombstoneBitmap;

/// Magic bytes at the start of every `.brain` file.
pub const BRAIN_MAGIC: [u8; 4] = *b"BHN0";

/// On-disk format version. Bump on any incompatible layout change.
pub const FORMAT_VERSION: u32 = 1;

/// Size of the fixed-width header (bytes 0..=63).
pub const HEADER_LEN: usize = 64;

/// Size of the BLAKE3-truncated footer.
pub const FOOTER_LEN: usize = 8;

/// Parsed `.brain` header. Validated by [`Header::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub format_version: u32,
    pub shard_uuid: [u8; 16],
    pub taken_at_lsn: u64,
    pub graph_node_count: u64,
    pub m: u32,
    pub ef_construction: u32,
    pub ef_search: u32,
    pub ef_search_max: u32,
    pub vector_dim: u32,
}

impl Header {
    /// Build a header from a live `HnswIndex`'s state.
    #[must_use]
    pub fn new<const D: usize>(
        shard_uuid: [u8; 16],
        taken_at_lsn: u64,
        graph_node_count: u64,
        params: IndexParams,
    ) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            shard_uuid,
            taken_at_lsn,
            graph_node_count,
            m: u32::try_from(params.m).expect("M fits in u32"),
            ef_construction: u32::try_from(params.ef_construction)
                .expect("ef_construction fits in u32"),
            ef_search: u32::try_from(params.ef_search).expect("ef_search fits in u32"),
            ef_search_max: u32::try_from(params.ef_search_max).expect("ef_search_max fits in u32"),
            vector_dim: u32::try_from(D).expect("vector dim fits in u32"),
        }
    }

    /// Serialize the header into a 64-byte buffer including the CRC.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&BRAIN_MAGIC);
        out[4..8].copy_from_slice(&self.format_version.to_le_bytes());
        out[8..24].copy_from_slice(&self.shard_uuid);
        out[24..32].copy_from_slice(&self.taken_at_lsn.to_le_bytes());
        out[32..40].copy_from_slice(&self.graph_node_count.to_le_bytes());
        out[40..44].copy_from_slice(&self.m.to_le_bytes());
        out[44..48].copy_from_slice(&self.ef_construction.to_le_bytes());
        out[48..52].copy_from_slice(&self.ef_search.to_le_bytes());
        out[52..56].copy_from_slice(&self.ef_search_max.to_le_bytes());
        out[56..60].copy_from_slice(&self.vector_dim.to_le_bytes());
        let crc = crc32c::crc32c(&out[..60]);
        out[60..64].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Parse and validate the header from a 64-byte prefix.
    pub fn parse(bytes: &[u8]) -> Result<Self, HeaderError> {
        if bytes.len() < HEADER_LEN {
            return Err(HeaderError::Truncated {
                expected: HEADER_LEN,
                got: bytes.len(),
            });
        }
        let magic: [u8; 4] = bytes[0..4].try_into().expect("4 bytes");
        if magic != BRAIN_MAGIC {
            return Err(HeaderError::BadMagic(magic));
        }
        let format_version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if format_version != FORMAT_VERSION {
            return Err(HeaderError::UnsupportedVersion(format_version));
        }
        let stored_crc = u32::from_le_bytes(bytes[60..64].try_into().unwrap());
        let computed_crc = crc32c::crc32c(&bytes[..60]);
        if stored_crc != computed_crc {
            return Err(HeaderError::BadCrc {
                expected: computed_crc,
                got: stored_crc,
            });
        }
        Ok(Self {
            format_version,
            shard_uuid: bytes[8..24].try_into().unwrap(),
            taken_at_lsn: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
            graph_node_count: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
            m: u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            ef_construction: u32::from_le_bytes(bytes[44..48].try_into().unwrap()),
            ef_search: u32::from_le_bytes(bytes[48..52].try_into().unwrap()),
            ef_search_max: u32::from_le_bytes(bytes[52..56].try_into().unwrap()),
            vector_dim: u32::from_le_bytes(bytes[56..60].try_into().unwrap()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    Truncated { expected: usize, got: usize },
    BadMagic([u8; 4]),
    UnsupportedVersion(u32),
    BadCrc { expected: u32, got: u32 },
}

/// Encoded payload to write into the `.brain` file body (everything
/// between the 64-byte header and the 8-byte footer).
pub struct Body {
    pub bytes: Vec<u8>,
}

impl Body {
    /// Encode the id_map + tombstone bitmap state.
    #[must_use]
    pub fn encode(id_map: &IdMap, next_internal_id: u32, tombstones: &TombstoneBitmap) -> Self {
        let mut bytes = Vec::new();

        // id_map entries.
        let count = u32::try_from(id_map.len()).expect("id_map.len fits in u32");
        bytes.extend_from_slice(&count.to_le_bytes());
        for entry in id_map.iter_forward() {
            bytes.extend_from_slice(&entry.0);
            bytes.extend_from_slice(&entry.1.to_le_bytes());
        }

        // next_internal_id.
        bytes.extend_from_slice(&next_internal_id.to_le_bytes());

        // Tombstone bitmap.
        let words = tombstones.raw_words();
        let word_count = u64::try_from(words.len()).expect("bitmap word count fits in u64");
        bytes.extend_from_slice(&word_count.to_le_bytes());
        for w in words {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        let set_count = u32::try_from(tombstones.count()).expect("tombstone count fits in u32");
        bytes.extend_from_slice(&set_count.to_le_bytes());

        Self { bytes }
    }
}

/// Parsed body state, ready to be installed into a fresh `HnswIndex`.
pub struct ParsedBody {
    pub id_map_entries: Vec<([u8; 16], u32)>,
    pub next_internal_id: u32,
    pub tombstone_words: Vec<u64>,
    pub tombstone_set_count: u32,
}

impl ParsedBody {
    /// Decode the body bytes. Caller has already validated the header
    /// and the BLAKE3 footer.
    pub fn parse(mut bytes: &[u8]) -> Result<Self, BodyError> {
        let id_map_count = read_u32(&mut bytes)? as usize;
        let mut id_map_entries = Vec::with_capacity(id_map_count);
        for _ in 0..id_map_count {
            let key = read_array_16(&mut bytes)?;
            let value = read_u32(&mut bytes)?;
            id_map_entries.push((key, value));
        }
        let next_internal_id = read_u32(&mut bytes)?;
        let tombstone_word_count = read_u64(&mut bytes)? as usize;
        let mut tombstone_words = Vec::with_capacity(tombstone_word_count);
        for _ in 0..tombstone_word_count {
            tombstone_words.push(read_u64(&mut bytes)?);
        }
        let tombstone_set_count = read_u32(&mut bytes)?;
        if !bytes.is_empty() {
            return Err(BodyError::TrailingBytes(bytes.len()));
        }
        Ok(Self {
            id_map_entries,
            next_internal_id,
            tombstone_words,
            tombstone_set_count,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyError {
    Truncated,
    TrailingBytes(usize),
}

fn read_u32(bytes: &mut &[u8]) -> Result<u32, BodyError> {
    if bytes.len() < 4 {
        return Err(BodyError::Truncated);
    }
    let v = u32::from_le_bytes(bytes[..4].try_into().unwrap());
    *bytes = &bytes[4..];
    Ok(v)
}

fn read_u64(bytes: &mut &[u8]) -> Result<u64, BodyError> {
    if bytes.len() < 8 {
        return Err(BodyError::Truncated);
    }
    let v = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    *bytes = &bytes[8..];
    Ok(v)
}

fn read_array_16(bytes: &mut &[u8]) -> Result<[u8; 16], BodyError> {
    if bytes.len() < 16 {
        return Err(BodyError::Truncated);
    }
    let v: [u8; 16] = bytes[..16].try_into().unwrap();
    *bytes = &bytes[16..];
    Ok(v)
}

/// Compute the BLAKE3-truncated-to-u64 footer for the file's
/// pre-footer bytes.
#[must_use]
pub fn compute_footer(pre_footer: &[u8]) -> [u8; FOOTER_LEN] {
    let h = blake3::hash(pre_footer);
    let mut footer = [0u8; FOOTER_LEN];
    footer.copy_from_slice(&h.as_bytes()[..FOOTER_LEN]);
    footer
}

/// Verify the footer matches BLAKE3 of `file_bytes[..file_bytes.len() - FOOTER_LEN]`.
#[must_use]
pub fn verify_footer(file_bytes: &[u8]) -> bool {
    if file_bytes.len() < FOOTER_LEN {
        return false;
    }
    let split = file_bytes.len() - FOOTER_LEN;
    let expected = compute_footer(&file_bytes[..split]);
    expected == file_bytes[split..]
}

/// Read the full `.brain` file into memory.
pub fn read_brain_file(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::IndexParams;

    fn sample_header() -> Header {
        Header::new::<384>([0xAB; 16], 12345, 100, IndexParams::default_v1())
    }

    #[test]
    fn header_round_trip() {
        let h = sample_header();
        let bytes = h.encode();
        let parsed = Header::parse(&bytes).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(parsed.format_version, FORMAT_VERSION);
        assert_eq!(parsed.shard_uuid, [0xAB; 16]);
        assert_eq!(parsed.taken_at_lsn, 12345);
        assert_eq!(parsed.graph_node_count, 100);
        assert_eq!(parsed.m, 16);
        assert_eq!(parsed.vector_dim, 384);
    }

    #[test]
    fn header_rejects_bad_magic() {
        let mut bytes = sample_header().encode();
        bytes[0] = b'X';
        match Header::parse(&bytes) {
            Err(HeaderError::BadMagic(m)) => assert_eq!(m, [b'X', b'H', b'N', b'0']),
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn header_rejects_bad_crc() {
        let mut bytes = sample_header().encode();
        // Flip a byte inside the header body (the taken_at_lsn field).
        bytes[24] ^= 0xFF;
        match Header::parse(&bytes) {
            Err(HeaderError::BadCrc { .. }) => {}
            other => panic!("expected BadCrc, got {other:?}"),
        }
    }

    #[test]
    fn header_rejects_unsupported_version() {
        let mut bytes = sample_header().encode();
        // Bump format_version to 99, then recompute the CRC so we
        // exercise the version check rather than the CRC check.
        bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
        let new_crc = crc32c::crc32c(&bytes[..60]);
        bytes[60..64].copy_from_slice(&new_crc.to_le_bytes());
        match Header::parse(&bytes) {
            Err(HeaderError::UnsupportedVersion(99)) => {}
            other => panic!("expected UnsupportedVersion(99), got {other:?}"),
        }
    }

    #[test]
    fn footer_round_trip() {
        let body = b"some body bytes";
        let mut file = Vec::new();
        file.extend_from_slice(body);
        let footer = compute_footer(&file);
        file.extend_from_slice(&footer);
        assert!(verify_footer(&file));
    }

    #[test]
    fn footer_rejects_corrupted_body() {
        let body = b"some body bytes";
        let mut file = Vec::new();
        file.extend_from_slice(body);
        let footer = compute_footer(&file);
        file.extend_from_slice(&footer);
        // Flip a byte in the body — footer no longer matches.
        file[3] ^= 0xFF;
        assert!(!verify_footer(&file));
    }
}
