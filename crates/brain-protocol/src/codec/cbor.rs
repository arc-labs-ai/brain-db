//! CBOR encode/decode helpers for request and response bodies.
//!
//! The wire payload is split into two sections: a self-describing CBOR
//! map carrying the structured fields, followed by an optional trailing
//! raw little-endian `f32` section carrying embedding vectors. Vectors
//! never enter the CBOR — keeping per-float tagging out of the encoding
//! and the bytes contiguous for bulk copy. CBOR is RFC 8949; any
//! language reads a payload with a stock decoder, which is what lets
//! Brain ship no client library.
//!
//! Encoding is reproducible-deterministic: a given value always encodes
//! to the same bytes (fixed struct field order, shortest-form integers
//! from `ciborium`), which is what the conformance corpus pins.

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::ProtocolError;

/// Serialize a value into a freshly allocated CBOR byte vector. The only
/// failure mode for an owned in-memory value is allocation, so the
/// unreachable error path is unwrapped with a descriptive message.
pub(crate) fn to_cbor_bytes<T: Serialize>(value: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf)
        .expect("invariant: CBOR encode of an owned value is infallible");
    buf
}

/// Decode a CBOR `T` from `bytes`, requiring that the whole buffer is
/// consumed. Trailing bytes after a complete CBOR item mean a malformed
/// frame (non-vector payloads have no trailing section). Any decode
/// error maps to [`ProtocolError::MalformedPayload`].
pub(crate) fn from_cbor_bytes<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ProtocolError> {
    let mut cursor = std::io::Cursor::new(bytes);
    let value: T = ciborium::from_reader(&mut cursor)
        .map_err(|e| ProtocolError::MalformedPayload(format!("CBOR decode failed: {e}")))?;
    if (cursor.position() as usize) != bytes.len() {
        return Err(ProtocolError::MalformedPayload(
            "trailing bytes after CBOR payload".into(),
        ));
    }
    Ok(value)
}

/// Decode a CBOR `T` from the front of `bytes` and return how many bytes
/// the CBOR section consumed, so the caller can read a trailing raw
/// section (e.g. an embedding vector) from `bytes[consumed..]`.
pub(crate) fn from_cbor_prefix<T: DeserializeOwned>(
    bytes: &[u8],
) -> Result<(T, usize), ProtocolError> {
    let mut cursor = std::io::Cursor::new(bytes);
    let value: T = ciborium::from_reader(&mut cursor)
        .map_err(|e| ProtocolError::MalformedPayload(format!("CBOR decode failed: {e}")))?;
    Ok((value, cursor.position() as usize))
}

/// Pack an `f32` slice into a contiguous little-endian byte vector for
/// the trailing raw-vector section.
pub(crate) fn f32_slice_to_le_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Read a little-endian `f32` vector out of the trailing raw section.
/// The length must be a whole number of 4-byte floats. Alignment-safe:
/// reads 4-byte chunks rather than casting the socket buffer.
pub(crate) fn le_bytes_to_f32_vec(bytes: &[u8]) -> Result<Vec<f32>, ProtocolError> {
    if !bytes.len().is_multiple_of(4) {
        return Err(ProtocolError::MalformedPayload(format!(
            "trailing vector section is {} bytes, not a multiple of 4",
            bytes.len()
        )));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Serde adapter for a `Vec<[u8; 16]>` of UUID-shaped wire ids, encoding
/// each element as a CBOR byte string (major type 2) inside a CBOR array.
///
/// `serde_bytes` only flattens a single contiguous buffer; a vector of
/// distinct 16-byte ids must stay an array-of-byte-strings so each id
/// keeps its own framing. Use via `#[serde(with = "vec_byte_array16")]`.
pub(crate) mod vec_byte_array16 {
    use serde::de::{Deserializer, SeqAccess, Visitor};
    use serde::ser::SerializeSeq;
    use std::fmt;

    pub(crate) fn serialize<S>(v: &[[u8; 16]], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(v.len()))?;
        for id in v {
            seq.serialize_element(serde_bytes::Bytes::new(id))?;
        }
        seq.end()
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<[u8; 16]>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Vec<[u8; 16]>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a sequence of 16-byte byte strings")
            }
            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                // Clamp the pre-allocation: `size_hint` echoes the
                // CBOR array length the *client* declared, so a small
                // frame can claim a huge element count and force an
                // oversized up-front allocation (memory amplification).
                // Reserve a sane floor and let `push` grow to the real
                // size as elements actually decode.
                const PREALLOC_CAP: usize = 64;
                let hint = seq.size_hint().unwrap_or(0).min(PREALLOC_CAP);
                let mut out = Vec::with_capacity(hint);
                while let Some(b) = seq.next_element::<serde_bytes::ByteBuf>()? {
                    let arr: [u8; 16] = b.as_ref().try_into().map_err(|_| {
                        serde::de::Error::invalid_length(b.len(), &"exactly 16 bytes")
                    })?;
                    out.push(arr);
                }
                Ok(out)
            }
        }
        deserializer.deserialize_seq(V)
    }
}

/// Serde adapter for `Option<Vec<[u8; 16]>>`, deferring to
/// [`vec_byte_array16`] for the `Some` case. Use via
/// `#[serde(with = "opt_vec_byte_array16")]`.
pub(crate) mod opt_vec_byte_array16 {
    use serde::de::Deserializer;

    pub(crate) fn serialize<S>(v: &Option<Vec<[u8; 16]>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        match v {
            None => serializer.serialize_none(),
            Some(inner) => {
                // Wrap so the inner Vec routes through `vec_byte_array16`.
                #[derive(serde::Serialize)]
                struct W<'a>(#[serde(with = "super::vec_byte_array16")] &'a [[u8; 16]]);
                serializer.serialize_some(&W(inner))
            }
        }
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<[u8; 16]>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::Deserialize as _;
        #[derive(serde::Deserialize)]
        struct W(#[serde(with = "super::vec_byte_array16")] Vec<[u8; 16]>);
        let opt: Option<W> = Option::deserialize(deserializer)?;
        Ok(opt.map(|w| w.0))
    }
}

/// Serde adapter for `Option<[u8; 16]>`, encoding the `Some` payload as a
/// CBOR byte string (major type 2) rather than an array of `u8`.
///
/// `serde_bytes` cannot reach through `Option`: applied to an
/// `Option<[u8; 16]>` field it serializes the inner array element-wise,
/// producing a CBOR array. Routing the inner array through
/// `serde_bytes::Bytes` keeps a present id a single 16-byte byte string,
/// so every wire id has one encoding regardless of how it is wrapped. Use
/// via `#[serde(with = "opt_byte_array16")]`.
pub(crate) mod opt_byte_array16 {
    use serde::de::Deserializer;

    pub(crate) fn serialize<S>(v: &Option<[u8; 16]>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        match v {
            None => serializer.serialize_none(),
            Some(id) => serializer.serialize_some(serde_bytes::Bytes::new(id)),
        }
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<[u8; 16]>, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::Deserialize as _;
        let opt: Option<serde_bytes::ByteBuf> = Option::deserialize(deserializer)?;
        match opt {
            None => Ok(None),
            Some(b) => {
                let arr: [u8; 16] = b
                    .as_ref()
                    .try_into()
                    .map_err(|_| serde::de::Error::invalid_length(b.len(), &"exactly 16 bytes"))?;
                Ok(Some(arr))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Sample {
        a: u32,
        b: String,
    }

    #[test]
    fn round_trip_whole_buffer() {
        let s = Sample {
            a: 7,
            b: "hi".into(),
        };
        let bytes = to_cbor_bytes(&s);
        let back: Sample = from_cbor_bytes(&bytes).expect("decode");
        assert_eq!(s, back);
    }

    #[test]
    fn trailing_bytes_rejected() {
        let s = Sample {
            a: 1,
            b: "x".into(),
        };
        let mut bytes = to_cbor_bytes(&s);
        bytes.push(0xFF);
        assert!(from_cbor_bytes::<Sample>(&bytes).is_err());
    }

    #[test]
    fn prefix_reports_consumed_and_vector_round_trips() {
        let s = Sample {
            a: 3,
            b: "v".into(),
        };
        let vec = vec![1.0f32, -2.5, 3.25];
        let mut payload = to_cbor_bytes(&s);
        let cbor_len = payload.len();
        payload.extend_from_slice(&f32_slice_to_le_bytes(&vec));

        let (back, consumed): (Sample, usize) = from_cbor_prefix(&payload).expect("prefix");
        assert_eq!(back, s);
        assert_eq!(consumed, cbor_len);
        let got = le_bytes_to_f32_vec(&payload[consumed..]).expect("vec");
        assert_eq!(got, vec);
    }

    #[test]
    fn ragged_vector_section_rejected() {
        assert!(le_bytes_to_f32_vec(&[0u8, 1, 2]).is_err());
    }

    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct OptIdSample {
        #[serde(with = "super::opt_byte_array16")]
        id: Option<[u8; 16]>,
    }

    #[test]
    fn opt_byte_array16_some_encodes_as_byte_string() {
        let s = OptIdSample {
            id: Some([0xAB; 16]),
        };
        let bytes = to_cbor_bytes(&s);
        // The map has one entry keyed "id"; the value must be a 16-byte
        // CBOR byte string (major type 2, count 16 = 0x50), never a CBOR
        // array of u8 (major type 4, 0x90).
        assert!(
            bytes.windows(2).any(|w| w == [0x50, 0xAB]),
            "Some(id) did not encode as a 0x50 byte string: {bytes:02x?}"
        );
        assert!(
            !bytes.contains(&0x90),
            "Some(id) leaked a CBOR array header: {bytes:02x?}"
        );
        let back: OptIdSample = from_cbor_bytes(&bytes).expect("decode");
        assert_eq!(s, back);
    }

    #[test]
    fn opt_byte_array16_none_round_trips() {
        let s = OptIdSample { id: None };
        let bytes = to_cbor_bytes(&s);
        let back: OptIdSample = from_cbor_bytes(&bytes).expect("decode");
        assert_eq!(s, back);
    }

    #[test]
    fn opt_byte_array16_wrong_length_rejected() {
        // A 4-byte byte string under the "id" key must fail the length
        // check rather than silently truncate or pad.
        #[derive(serde::Serialize)]
        struct Bad {
            id: serde_bytes::ByteBuf,
        }
        let bytes = to_cbor_bytes(&Bad {
            id: serde_bytes::ByteBuf::from(vec![1u8, 2, 3, 4]),
        });
        assert!(from_cbor_bytes::<OptIdSample>(&bytes).is_err());
    }
}
