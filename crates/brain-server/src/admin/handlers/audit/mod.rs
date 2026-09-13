//! Admin HTTP handlers for the historical audit log.
//!
//! `GET /v1/audit` reads one paginated page of the extraction- /
//! resolution-audit tables; `GET /v1/audit/export` is the bulk convenience
//! over the same selection. Both are **deployment-wide operator surfaces**
//! gated by the `/v1` admin token — there is no per-tenant scoping (the
//! admin token owns the deployment, exactly like `/metrics` and the
//! snapshot routes).
//!
//! **Scope.** With `?shard=N` omitted the query is **deployment-wide**: it
//! fans out to every shard and merges their pages into the single global
//! order the selected index implies. Passing `?shard=N` slices a single
//! shard (back-compat). `by=memory` is always single-shard — a `MemoryId`
//! encodes its owning shard in its high bits, so the handler routes
//! straight to that shard and never fans out.
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
//! that ceiling is hit).
//!
//! **Cursors.** A single-shard query (`?shard=N` or `by=memory`) uses the
//! simple `base64url(ts_be[8] ++ audit_id[16])` token encoding that shard's
//! last index key. A deployment-wide query uses a **compound cursor**: a
//! versioned, per-shard vector of positions (each shard is `Start`,
//! `Resume(index-key)`, or `Exhausted`). Resuming re-queries each
//! non-exhausted shard from its recorded position and re-merges; the token
//! goes `null` once every shard is exhausted. The compound cursor is bound
//! to the shard count it was minted under — replaying it against a
//! different topology is a `400`.
//!
//! **Merge order.** `by=time` merges by `(started_at, audit_id)` — the
//! `BY_TIME` index is timestamp-ordered per shard, so a global time order
//! is well-defined. `by=extractor` and `by=resolution` merge by `audit_id`
//! alone, matching each shard's native scan order (the by-extractor index
//! and the resolution primary key both scan in `audit_id` order; `audit_id`
//! is a UUIDv7, so this is a total, roughly-chronological global order).

mod export;
mod query;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use brain_core::MemoryId;
use brain_metadata::tables::audit::{ExtractionAudit, ResolutionAudit};

use crate::shard::{AuditCursor, AuditPage, AuditSelector, ShardHandle};

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

/// Everything an audit request resolves to after parsing. The `cursor`
/// stays raw here because its interpretation depends on scope: a
/// single-shard slice decodes the simple 24-byte token, a deployment-wide
/// query decodes the compound per-shard cursor. `shard` is `Some(N)` only
/// when `?shard=N` is explicit; `None` selects the deployment-wide fan-out.
struct AuditParams {
    selector: AuditSelector,
    shard: Option<usize>,
    limit: usize,
    cursor: Option<String>,
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

    let shard = crate::admin::query::shard_optional(query)?;

    let limit = match param(query, "limit") {
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|e| format!("invalid limit: {e}"))?
            .clamp(1, max_limit),
        None => default_limit,
    };

    let cursor = param(query, "cursor").map(str::to_owned);

    Ok(AuditParams {
        selector,
        shard,
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

/// The owning shard of a `by=memory` audit query. A `MemoryId` encodes its
/// shard in its high 16 bits, so an audit-by-memory read is inherently
/// single-shard: route straight to the owner, never fan out.
fn owning_shard(memory_bytes: [u8; 16]) -> usize {
    MemoryId::from_be_bytes(memory_bytes).shard() as usize
}

/// One shard's pagination position within a deployment-wide query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShardPos {
    /// Not yet read from — resume from the beginning of the selection.
    Start,
    /// Resume strictly after this index key.
    Resume(AuditCursor),
    /// No further rows on this shard; skip it on subsequent pages.
    Exhausted,
}

impl ShardPos {
    /// The per-shard cursor to hand [`ShardHandle::audit_query`], or `None`
    /// to read from the start. `Exhausted` shards are never queried.
    fn cursor(self) -> Option<AuditCursor> {
        match self {
            ShardPos::Start | ShardPos::Exhausted => None,
            ShardPos::Resume(c) => Some(c),
        }
    }
}

/// Version tag on the compound cursor wire form. Bumped if the layout
/// changes so a stale token is rejected rather than misread.
const COMPOUND_CURSOR_VERSION: u8 = 1;
const FLAG_EXHAUSTED: u8 = 0;
const FLAG_RESUME: u8 = 1;
const FLAG_START: u8 = 2;

/// Encode a deployment-wide position vector as
/// `base64url(ver[1] ++ n[2 BE] ++ (flag[1] ++ [key[24] if RESUME])*)`.
/// The embedded shard count `n` binds the token to the topology it was
/// minted under; [`decode_compound_cursor`] rejects a mismatch.
fn encode_compound_cursor(positions: &[ShardPos]) -> String {
    let mut buf = Vec::with_capacity(3 + positions.len() * 25);
    buf.push(COMPOUND_CURSOR_VERSION);
    let n = positions.len() as u16;
    buf.extend_from_slice(&n.to_be_bytes());
    for pos in positions {
        match pos {
            ShardPos::Exhausted => buf.push(FLAG_EXHAUSTED),
            ShardPos::Start => buf.push(FLAG_START),
            ShardPos::Resume(c) => {
                buf.push(FLAG_RESUME);
                buf.extend_from_slice(&c.ts.to_be_bytes());
                buf.extend_from_slice(&c.audit_id);
            }
        }
    }
    URL_SAFE_NO_PAD.encode(buf)
}

/// Decode a compound `cursor=` token for a deployment-wide query. Rejects
/// any token whose version, embedded shard count (`expected_shards`), or
/// framing is malformed.
fn decode_compound_cursor(raw: &str, expected_shards: usize) -> Result<Vec<ShardPos>, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| "invalid cursor: not base64url".to_string())?;
    if bytes.len() < 3 {
        return Err("invalid cursor: truncated header".to_string());
    }
    if bytes[0] != COMPOUND_CURSOR_VERSION {
        return Err("invalid cursor: unknown version".to_string());
    }
    let n = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
    if n != expected_shards {
        return Err("invalid cursor: shard count mismatch".to_string());
    }
    let mut positions = Vec::with_capacity(n);
    let mut i = 3;
    for _ in 0..n {
        let flag = *bytes.get(i).ok_or("invalid cursor: truncated body")?;
        i += 1;
        match flag {
            FLAG_EXHAUSTED => positions.push(ShardPos::Exhausted),
            FLAG_START => positions.push(ShardPos::Start),
            FLAG_RESUME => {
                let end = i + 24;
                let key = bytes.get(i..end).ok_or("invalid cursor: truncated key")?;
                let mut ts = [0u8; 8];
                ts.copy_from_slice(&key[..8]);
                let mut audit_id = [0u8; 16];
                audit_id.copy_from_slice(&key[8..]);
                positions.push(ShardPos::Resume(AuditCursor {
                    ts: u64::from_be_bytes(ts),
                    audit_id,
                }));
                i = end;
            }
            _ => return Err("invalid cursor: bad flag".to_string()),
        }
    }
    if i != bytes.len() {
        return Err("invalid cursor: trailing bytes".to_string());
    }
    Ok(positions)
}

/// One shard's fetched slice during a deployment-wide merge round.
struct ShardBuf<R> {
    rows: Vec<R>,
    /// Index of the next unconsumed row in `rows`.
    pos: usize,
    /// Whether the shard reported further rows beyond this fetched slice.
    more: bool,
    /// Whether this shard was queried this round (`false` = it was already
    /// `Exhausted`).
    active: bool,
}

/// Read one deployment-wide page: query every non-exhausted shard from its
/// recorded position, k-way merge the results into the selector's global
/// order, and emit at most `limit` rows. Returns the merged page plus the
/// advanced per-shard positions (all `Exhausted` ⇒ the caller emits a null
/// `next_cursor`).
///
/// Correctness rests on one invariant: each shard's native scan order
/// equals the merge comparator (see the module docs). Because every shard
/// is asked for up to `limit` rows and the merge emits at most `limit`
/// total, a shard's buffer can only drain while it still has more rows once
/// the global page is already full — so a resumed shard never skips a row.
async fn deployment_wide_page(
    shards: &[ShardHandle],
    selector: AuditSelector,
    limit: usize,
    positions: &[ShardPos],
) -> Result<(AuditPage, Vec<ShardPos>), String> {
    if matches!(selector, AuditSelector::Resolution { .. }) {
        let bufs = fetch_bufs(shards, selector, limit, positions, |page| match page {
            AuditPage::Resolution { rows, next } => Ok((rows, next.is_some())),
            AuditPage::Extraction { .. } => {
                Err("resolution selector returned an extraction page".to_string())
            }
        })
        .await?;
        let (rows, next_positions) = merge_round(
            bufs,
            positions,
            limit,
            |r: &ResolutionAudit| (0u64, r.audit_id_bytes),
            |r: &ResolutionAudit| AuditCursor {
                ts: r.created_at_unix_nanos,
                audit_id: r.audit_id_bytes,
            },
        );
        Ok((AuditPage::Resolution { rows, next: None }, next_positions))
    } else {
        let by_time = matches!(selector, AuditSelector::Time { .. });
        let bufs = fetch_bufs(shards, selector, limit, positions, |page| match page {
            AuditPage::Extraction { rows, next } => Ok((rows, next.is_some())),
            AuditPage::Resolution { .. } => {
                Err("extraction selector returned a resolution page".to_string())
            }
        })
        .await?;
        let (rows, next_positions) = merge_round(
            bufs,
            positions,
            limit,
            move |r: &ExtractionAudit| {
                // by=time sorts on (ts, audit_id); by=extractor sorts on
                // audit_id alone (its per-shard scan order), so hold ts flat.
                let ts = if by_time { r.started_at_unix_nanos } else { 0 };
                (ts, r.audit_id_bytes)
            },
            |r: &ExtractionAudit| AuditCursor {
                ts: r.started_at_unix_nanos,
                audit_id: r.audit_id_bytes,
            },
        );
        Ok((AuditPage::Extraction { rows, next: None }, next_positions))
    }
}

/// Query each non-exhausted shard once and wrap its rows in a [`ShardBuf`].
/// `split` pulls the concrete row vector + "has more" flag out of the page
/// variant the selector is expected to yield.
async fn fetch_bufs<R>(
    shards: &[ShardHandle],
    selector: AuditSelector,
    limit: usize,
    positions: &[ShardPos],
    split: impl Fn(AuditPage) -> Result<(Vec<R>, bool), String>,
) -> Result<Vec<ShardBuf<R>>, String> {
    let mut bufs = Vec::with_capacity(shards.len());
    for (s, pos) in positions.iter().enumerate() {
        if matches!(pos, ShardPos::Exhausted) {
            bufs.push(ShardBuf {
                rows: Vec::new(),
                pos: 0,
                more: false,
                active: false,
            });
            continue;
        }
        let page = shards[s]
            .audit_query(selector, limit, pos.cursor())
            .await
            .map_err(|e| e.to_string())?;
        let (rows, more) = split(page)?;
        bufs.push(ShardBuf {
            rows,
            pos: 0,
            more,
            active: true,
        });
    }
    Ok(bufs)
}

/// K-way merge across per-shard buffers already sorted in `key` order.
/// Emits up to `limit` rows and computes each shard's next position:
/// `Exhausted` once its buffer is drained with no more rows, `Resume` at
/// the last row consumed from it, or its prior position unchanged when this
/// round consumed nothing from it.
fn merge_round<R, K>(
    mut bufs: Vec<ShardBuf<R>>,
    prev: &[ShardPos],
    limit: usize,
    key: impl Fn(&R) -> K,
    cursor_of: impl Fn(&R) -> AuditCursor,
) -> (Vec<R>, Vec<ShardPos>)
where
    R: Clone,
    K: Ord,
{
    let n = bufs.len();
    let mut out = Vec::new();
    let mut last: Vec<Option<AuditCursor>> = vec![None; n];

    while out.len() < limit {
        let mut best: Option<(usize, K)> = None;
        for (s, buf) in bufs.iter().enumerate() {
            if let Some(row) = buf.rows.get(buf.pos) {
                let k = key(row);
                let take = match &best {
                    None => true,
                    Some((_, bk)) => k < *bk,
                };
                if take {
                    best = Some((s, k));
                }
            }
        }
        let Some((s, _)) = best else { break };
        let row = bufs[s].rows[bufs[s].pos].clone();
        last[s] = Some(cursor_of(&row));
        bufs[s].pos += 1;
        out.push(row);
    }

    let mut positions = prev.to_vec();
    for (s, buf) in bufs.iter().enumerate() {
        if !buf.active {
            positions[s] = ShardPos::Exhausted;
            continue;
        }
        match last[s] {
            Some(c) => {
                let drained = buf.pos >= buf.rows.len();
                positions[s] = if drained && !buf.more {
                    ShardPos::Exhausted
                } else {
                    ShardPos::Resume(c)
                };
            }
            None => {
                // Consumed nothing this round. If the shard genuinely
                // returned no rows it is exhausted; otherwise its rows are
                // still pending and its prior position is preserved.
                if buf.rows.is_empty() {
                    positions[s] = ShardPos::Exhausted;
                }
            }
        }
    }

    (out, positions)
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
        assert_eq!(p.shard, None);
        assert!(p.cursor.is_none());
    }

    #[test]
    fn parse_params_explicit_shard() {
        let p = parse_params("by=time&shard=3", 100, MAX_QUERY_LIMIT).unwrap();
        assert_eq!(p.shard, Some(3));
    }

    #[test]
    fn compound_cursor_round_trips() {
        let positions = vec![
            ShardPos::Start,
            ShardPos::Resume(AuditCursor {
                ts: 42,
                audit_id: [9u8; 16],
            }),
            ShardPos::Exhausted,
        ];
        let encoded = encode_compound_cursor(&positions);
        assert_eq!(decode_compound_cursor(&encoded, 3).unwrap(), positions);
    }

    #[test]
    fn compound_cursor_rejects_topology_mismatch() {
        let positions = vec![ShardPos::Start, ShardPos::Start];
        let encoded = encode_compound_cursor(&positions);
        // Minted for 2 shards, replayed against a 3-shard deployment.
        assert!(decode_compound_cursor(&encoded, 3).is_err());
    }

    #[test]
    fn compound_cursor_rejects_garbage() {
        assert!(decode_compound_cursor("!!!", 1).is_err());
        assert!(decode_compound_cursor(&URL_SAFE_NO_PAD.encode([0u8; 1]), 1).is_err());
    }

    #[test]
    fn owning_shard_reads_high_bits() {
        let id = MemoryId::pack(5, 0, 0);
        assert_eq!(owning_shard(id.to_be_bytes()), 5);
    }

    #[test]
    fn merge_round_interleaves_by_key_and_advances() {
        // Two shards, rows keyed by a single u64. Shard 0: [0, 2]; shard 1:
        // [1, 3]. Global order must be 0,1,2,3.
        let bufs = vec![
            ShardBuf {
                rows: vec![0u64, 2],
                pos: 0,
                more: false,
                active: true,
            },
            ShardBuf {
                rows: vec![1u64, 3],
                pos: 0,
                more: false,
                active: true,
            },
        ];
        let prev = vec![ShardPos::Start, ShardPos::Start];
        let (rows, positions) = merge_round(
            bufs,
            &prev,
            3,
            |r: &u64| *r,
            |r: &u64| AuditCursor {
                ts: *r,
                audit_id: [*r as u8; 16],
            },
        );
        assert_eq!(rows, vec![0, 1, 2]);
        // Shard 0 gave 0 and 2 (both consumed → drained, no more → done);
        // shard 1 gave only 1 (3 still pending → resume at 1).
        assert_eq!(positions[0], ShardPos::Exhausted);
        assert_eq!(
            positions[1],
            ShardPos::Resume(AuditCursor {
                ts: 1,
                audit_id: [1u8; 16]
            })
        );
    }

    #[test]
    fn merge_round_preserves_untouched_shard_position() {
        // Shard 1 contributes nothing this round (its row sorts after the
        // page) and stays at its prior Resume position rather than resetting.
        let prior = AuditCursor {
            ts: 100,
            audit_id: [7u8; 16],
        };
        let bufs = vec![
            ShardBuf {
                rows: vec![0u64],
                pos: 0,
                more: false,
                active: true,
            },
            ShardBuf {
                rows: vec![9u64],
                pos: 0,
                more: true,
                active: true,
            },
        ];
        let prev = vec![ShardPos::Start, ShardPos::Resume(prior)];
        let (rows, positions) = merge_round(
            bufs,
            &prev,
            1,
            |r: &u64| *r,
            |r: &u64| AuditCursor {
                ts: *r,
                audit_id: [*r as u8; 16],
            },
        );
        assert_eq!(rows, vec![0]);
        assert_eq!(positions[0], ShardPos::Exhausted);
        assert_eq!(positions[1], ShardPos::Resume(prior));
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
