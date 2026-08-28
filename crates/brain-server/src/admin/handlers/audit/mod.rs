//! Admin HTTP handlers for the historical audit log.
//!
//! `GET /v1/audit` reads one paginated page of the extraction- /
//! resolution-audit tables; `GET /v1/audit/export` is the bulk convenience
//! over the same selection. Both are **deployment-wide operator surfaces**
//! gated by the `/v1` admin token — there is no per-tenant scoping (the
//! admin token owns the deployment, exactly like `/metrics` and the
//! snapshot routes). Shard selection follows the existing `?shard=N`
//! convention (default `0`).
//!
//! Query shape:
//! `GET /v1/audit?by=<memory|extractor|time|resolution>&<selector>&limit=<n>&cursor=<opaque>[&shard=N]`
//! - `by=memory`     → `memory=<hex MemoryId>`      (extractor-audit rows)
//! - `by=extractor`  → `extractor=<u32>`            (extractor-audit rows)
//! - `by=time`       → `since=<unix_ns>&until=<unix_ns>` (extractor-audit)
//! - `by=resolution` → `since=<unix_ns>&until=<unix_ns>` (resolution-audit)
//!
//! `since` defaults to `0`, `until` to `u64::MAX`. For `GET /v1/audit`,
//! `limit` defaults to 100 and is capped at 1000. `GET /v1/audit/export`
//! ignores `limit`, walking fixed 1000-row pages internally and returning
//! up to 100 000 rows in one array (a `next_cursor` is returned only if
//! that ceiling is hit). The opaque `cursor` is a base64url token encoding
//! the last index key returned; resuming after it yields the next page in
//! deterministic index order.

mod export;
mod query;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;

use crate::shard::{AuditCursor, AuditPage, AuditSelector};

pub use export::export;
pub use query::query;

/// Default page size for `GET /v1/audit` when `limit=` is absent.
const DEFAULT_QUERY_LIMIT: usize = 100;
/// Hard cap on a single `GET /v1/audit` page.
const MAX_QUERY_LIMIT: usize = 1000;
/// Per-page fetch size the export handler walks with internally.
const EXPORT_PAGE_LIMIT: usize = 1000;
/// Safety ceiling on the number of rows a single export response returns
/// before it stops and hands back a `next_cursor` to continue from.
const EXPORT_MAX_ROWS: usize = 100_000;

/// Everything an audit request resolves to after parsing.
struct AuditParams {
    selector: AuditSelector,
    shard_id: usize,
    limit: usize,
    cursor: Option<AuditCursor>,
}

/// Parse the shared audit query string. `default_limit` / `max_limit`
/// differ between the query and export routes. Returns a client-safe
/// error string on any malformed / missing selector.
fn parse_params(
    query: &str,
    default_limit: usize,
    max_limit: usize,
) -> Result<AuditParams, String> {
    let by = param(query, "by").ok_or_else(|| "missing required ?by= parameter".to_string())?;
    let selector = match by {
        "memory" => {
            let raw = param(query, "memory")
                .ok_or_else(|| "by=memory requires ?memory=<hex MemoryId>".to_string())?;
            let bytes = hex_decode_16(raw)
                .ok_or_else(|| "invalid memory: expected 32 hex chars".to_string())?;
            AuditSelector::Memory(bytes)
        }
        "extractor" => {
            let raw = param(query, "extractor")
                .ok_or_else(|| "by=extractor requires ?extractor=<u32>".to_string())?;
            let id = raw
                .parse::<u32>()
                .map_err(|e| format!("invalid extractor: {e}"))?;
            AuditSelector::Extractor(id)
        }
        "time" => {
            let (since, until) = parse_window(query)?;
            AuditSelector::Time { since, until }
        }
        "resolution" => {
            let (since, until) = parse_window(query)?;
            AuditSelector::Resolution { since, until }
        }
        other => {
            return Err(format!(
                "unknown by={other}: expected one of memory, extractor, time, resolution"
            ))
        }
    };

    let shard_id = crate::admin::query::shard_required(query)?;

    let limit = match param(query, "limit") {
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|e| format!("invalid limit: {e}"))?
            .clamp(1, max_limit),
        None => default_limit,
    };

    let cursor = match param(query, "cursor") {
        Some(raw) => Some(decode_cursor(raw)?),
        None => None,
    };

    Ok(AuditParams {
        selector,
        shard_id,
        limit,
        cursor,
    })
}

/// Parse the optional `since` / `until` unix-nanosecond window. `since`
/// defaults to `0`, `until` to `u64::MAX` (open-ended).
fn parse_window(query: &str) -> Result<(u64, u64), String> {
    let since = match param(query, "since") {
        Some(raw) => raw
            .parse::<u64>()
            .map_err(|e| format!("invalid since: {e}"))?,
        None => 0,
    };
    let until = match param(query, "until") {
        Some(raw) => raw
            .parse::<u64>()
            .map_err(|e| format!("invalid until: {e}"))?,
        None => u64::MAX,
    };
    if since > until {
        return Err("since must be <= until".to_string());
    }
    Ok((since, until))
}

/// First `key=value` match in a `&`-joined query string. Returns `None`
/// when absent or empty.
fn param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    for kv in query.split('&') {
        if let Some(rest) = kv.strip_prefix(key) {
            if let Some(v) = rest.strip_prefix('=') {
                if v.is_empty() {
                    return None;
                }
                return Some(v);
            }
        }
    }
    None
}

/// Encode a cursor as `base64url(ts_be[8] ++ audit_id[16])` — an opaque,
/// URL-safe token the client echoes back verbatim.
fn encode_cursor(c: AuditCursor) -> String {
    let mut buf = [0u8; 24];
    buf[..8].copy_from_slice(&c.ts.to_be_bytes());
    buf[8..].copy_from_slice(&c.audit_id);
    URL_SAFE_NO_PAD.encode(buf)
}

/// Decode a `cursor=` token back into an [`AuditCursor`]. Rejects any
/// token that isn't exactly 24 bytes of base64url.
fn decode_cursor(raw: &str) -> Result<AuditCursor, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| "invalid cursor: not base64url".to_string())?;
    if bytes.len() != 24 {
        return Err("invalid cursor: wrong length".to_string());
    }
    let mut ts = [0u8; 8];
    ts.copy_from_slice(&bytes[..8]);
    let mut audit_id = [0u8; 16];
    audit_id.copy_from_slice(&bytes[8..]);
    Ok(AuditCursor {
        ts: u64::from_be_bytes(ts),
        audit_id,
    })
}

/// Serialize an [`AuditPage`] into the wire JSON envelope:
/// `{"kind":<str>,"rows":[...],"next_cursor":<str|null>}`.
fn page_to_json(page: &AuditPage, next_cursor: Option<&str>) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(1024);
    match page {
        AuditPage::Extraction { rows, .. } => {
            out.push_str("{\"kind\":\"extraction\",\"rows\":[");
            for (i, r) in rows.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write!(
                    &mut out,
                    "{{\"audit_id\":\"{aid}\",\"memory_id\":\"{mid}\",\"extractor_id\":{eid},\
                     \"extractor_version\":{ev},\"schema_version\":{sv},\
                     \"started_at_unix_nanos\":{sa},\"completed_at_unix_nanos\":{ca},\
                     \"status\":{st},\"status_reason\":{sr},\"outputs\":[",
                    aid = hex_encode(&r.audit_id_bytes),
                    mid = hex_encode(&r.memory_id_bytes),
                    eid = r.extractor_id,
                    ev = r.extractor_version,
                    sv = r.schema_version,
                    sa = r.started_at_unix_nanos,
                    ca = r.completed_at_unix_nanos,
                    st = r.status,
                    sr = json_string(&r.status_reason),
                )
                .expect("string write");
                for (j, o) in r.outputs.iter().enumerate() {
                    if j > 0 {
                        out.push(',');
                    }
                    write!(
                        &mut out,
                        "{{\"kind\":{k},\"id\":\"{id}\"}}",
                        k = o.kind,
                        id = hex_encode(&o.id),
                    )
                    .expect("string write");
                }
                write!(
                    &mut out,
                    "],\"cost_micro_usd\":{cost},\"model_metadata\":\"{mm}\",\"input_hash\":\"{ih}\"}}",
                    cost = r.cost_micro_usd,
                    mm = URL_SAFE_NO_PAD.encode(&r.model_metadata),
                    ih = hex_encode(&r.input_hash),
                )
                .expect("string write");
            }
        }
        AuditPage::Resolution { rows, .. } => {
            out.push_str("{\"kind\":\"resolution\",\"rows\":[");
            for (i, r) in rows.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let resolved = match &r.resolved_entity_bytes {
                    Some(b) => format!("\"{}\"", hex_encode(b)),
                    None => "null".to_string(),
                };
                write!(
                    &mut out,
                    "{{\"audit_id\":\"{aid}\",\"candidate_name\":{cn},\"entity_type_id\":{et},\
                     \"resolved_entity\":{re},\"outcome\":{oc},\"confidence\":{cf},\
                     \"created_at_unix_nanos\":{ct},\"candidates_blob\":\"{cb}\"}}",
                    aid = hex_encode(&r.audit_id_bytes),
                    cn = json_string(&r.candidate_name),
                    et = r.entity_type_id,
                    re = resolved,
                    oc = r.outcome,
                    cf = r.confidence,
                    ct = r.created_at_unix_nanos,
                    cb = URL_SAFE_NO_PAD.encode(&r.candidates_blob),
                )
                .expect("string write");
            }
        }
    }
    out.push_str("],\"next_cursor\":");
    match next_cursor {
        Some(c) => {
            out.push('"');
            out.push_str(c);
            out.push('"');
        }
        None => out.push_str("null"),
    }
    out.push_str("}\n");
    out
}

/// Lowercase hex of a byte slice.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Decode exactly 32 hex chars into a 16-byte id. Returns `None` on any
/// wrong length or non-hex digit.
fn hex_decode_16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    let bytes = s.as_bytes();
    for i in 0..16 {
        let hi = hex_val(bytes[i * 2])?;
        let lo = hex_val(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Minimal JSON string escaping for the free-form `status_reason` /
/// `candidate_name` fields. Handles the control characters and the two
/// mandatory escapes (`"` and `\`).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                write!(&mut out, "\\u{:04x}", c as u32).expect("string write");
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips() {
        let c = AuditCursor {
            ts: 1_700_000_000_000_000_123,
            audit_id: [7u8; 16],
        };
        let encoded = encode_cursor(c);
        assert_eq!(decode_cursor(&encoded).unwrap(), c);
    }

    #[test]
    fn decode_cursor_rejects_garbage() {
        assert!(decode_cursor("!!!not-base64!!!").is_err());
        // Valid base64url but wrong length.
        assert!(decode_cursor(&URL_SAFE_NO_PAD.encode([0u8; 8])).is_err());
    }

    #[test]
    fn hex_16_round_trips() {
        let bytes = [
            0x0a, 0x1b, 0x2c, 0x3d, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 0xff,
        ];
        let s = hex_encode(&bytes);
        assert_eq!(s.len(), 32);
        assert_eq!(hex_decode_16(&s).unwrap(), bytes);
    }

    #[test]
    fn hex_decode_rejects_bad_input() {
        assert!(hex_decode_16("xyz").is_none());
        assert!(hex_decode_16(&"g".repeat(32)).is_none());
    }

    #[test]
    fn parse_params_memory() {
        let mid = hex_encode(&[1u8; 16]);
        let p = parse_params(
            &format!("by=memory&memory={mid}"),
            DEFAULT_QUERY_LIMIT,
            MAX_QUERY_LIMIT,
        )
        .unwrap();
        assert_eq!(p.selector, AuditSelector::Memory([1u8; 16]));
        assert_eq!(p.limit, DEFAULT_QUERY_LIMIT);
        assert_eq!(p.shard_id, 0);
        assert!(p.cursor.is_none());
    }

    #[test]
    fn parse_params_clamps_limit() {
        let p = parse_params(
            "by=extractor&extractor=3&limit=999999",
            100,
            MAX_QUERY_LIMIT,
        )
        .unwrap();
        assert_eq!(p.limit, MAX_QUERY_LIMIT);
        assert_eq!(p.selector, AuditSelector::Extractor(3));
    }

    #[test]
    fn parse_params_time_window_defaults() {
        let p = parse_params("by=time", 100, MAX_QUERY_LIMIT).unwrap();
        assert_eq!(
            p.selector,
            AuditSelector::Time {
                since: 0,
                until: u64::MAX
            }
        );
    }

    #[test]
    fn parse_params_rejects_unknown_by() {
        assert!(parse_params("by=bogus", 100, MAX_QUERY_LIMIT).is_err());
        assert!(parse_params("", 100, MAX_QUERY_LIMIT).is_err());
        assert!(parse_params("by=memory", 100, MAX_QUERY_LIMIT).is_err());
        assert!(parse_params("by=time&since=9&until=1", 100, MAX_QUERY_LIMIT).is_err());
    }
}
