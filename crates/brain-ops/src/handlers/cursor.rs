//! Opaque continuation tokens for the multi-frame list read ops
//! (`ENTITY_LIST` / `STATEMENT_LIST` / `RELATION_LIST_FROM` / `_TO`).
//!
//! A cursor names the row a page ended on: the next page resumes strictly
//! after it in the same deterministic id order the single-frame path
//! already returns. The token is opaque to clients — it carries the
//! owning `(namespace, space)` scope so a cursor minted for one tenant is
//! rejected if replayed against another, and a version tag so a future
//! layout change fails closed rather than mis-decoding.
//!
//! The four handlers all follow the same shape: materialize the full
//! scope-walled candidate set (up to the 1000-row list ceiling), sort it
//! ascending by the row's 16-byte id, then hand it to [`paginate`] with
//! the decoded cursor. Ordering by the immutable UUIDv7 id is stable
//! across pages, so — as long as the underlying rows do not change
//! between requests — every row is emitted on exactly one page with no
//! overlap or gap.

use brain_metadata::RowScope;

use crate::error::OpError;

/// Cursor wire-layout version. An unrecognized tag is treated as a
/// malformed cursor rather than mis-decoded.
const CURSOR_VERSION: u8 = 1;

/// Encoded length: `version(1) + namespace_id(4) + space_id(16) + last_id(16)`.
const CURSOR_LEN: usize = 1 + 4 + 16 + 16;

/// Encode an opaque continuation token: resume strictly after `last_id`
/// within `scope`.
#[must_use]
pub fn encode(scope: RowScope, last_id: [u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(CURSOR_LEN);
    out.push(CURSOR_VERSION);
    out.extend_from_slice(&scope.namespace_id.to_le_bytes());
    out.extend_from_slice(&scope.space_id_bytes);
    out.extend_from_slice(&last_id);
    out
}

/// Decode a continuation token minted by [`encode`], verifying it names
/// the caller's own `scope`. Returns the id to resume strictly after.
///
/// Rejects a truncated, over-long, wrong-version, or out-of-tenant cursor
/// with [`OpError::InvalidRequest`] — never panics on client input.
pub fn decode(scope: RowScope, cursor: &[u8]) -> Result<[u8; 16], OpError> {
    if cursor.len() != CURSOR_LEN || cursor[0] != CURSOR_VERSION {
        return Err(OpError::InvalidRequest("malformed cursor".into()));
    }
    let mut ns = [0u8; 4];
    ns.copy_from_slice(&cursor[1..5]);
    let namespace_id = u32::from_le_bytes(ns);
    let mut space = [0u8; 16];
    space.copy_from_slice(&cursor[5..21]);
    if namespace_id != scope.namespace_id || space != scope.space_id_bytes {
        return Err(OpError::InvalidRequest(
            "cursor does not belong to the caller's tenant".into(),
        ));
    }
    let mut id = [0u8; 16];
    id.copy_from_slice(&cursor[21..37]);
    Ok(id)
}

/// Decode the optional request cursor: `None` when empty (first page),
/// otherwise the id to resume strictly after.
pub fn decode_opt(scope: RowScope, cursor: &[u8]) -> Result<Option<[u8; 16]>, OpError> {
    if cursor.is_empty() {
        Ok(None)
    } else {
        decode(scope, cursor).map(Some)
    }
}

/// Slice one page out of a fully-materialized candidate set.
///
/// `items` MUST already be sorted ascending by the key `id_of` returns.
/// Drops everything up to and including `resume_after`, keeps the first
/// `limit` of what remains, and returns the token for the next page —
/// empty when this page is the last.
pub fn paginate<T>(
    scope: RowScope,
    mut items: Vec<T>,
    resume_after: Option<[u8; 16]>,
    limit: usize,
    id_of: impl Fn(&T) -> [u8; 16],
) -> (Vec<T>, Vec<u8>) {
    if let Some(after) = resume_after {
        items.retain(|it| id_of(it) > after);
    }
    let has_more = items.len() > limit;
    items.truncate(limit);
    let next = if has_more {
        // `has_more` implies a non-empty page, so `last` is present.
        items
            .last()
            .map(|last| encode(scope, id_of(last)))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    (items, next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> RowScope {
        RowScope::from_bytes(7, [0xAB; 16])
    }

    fn id(n: u8) -> [u8; 16] {
        [n; 16]
    }

    #[test]
    fn round_trips_within_scope() {
        let s = scope();
        let token = encode(s, id(9));
        assert_eq!(decode(s, &token).unwrap(), id(9));
    }

    #[test]
    fn rejects_wrong_tenant() {
        let token = encode(scope(), id(3));
        let other = RowScope::from_bytes(8, [0xAB; 16]);
        assert!(decode(other, &token).is_err());
        let other_space = RowScope::from_bytes(7, [0xCD; 16]);
        assert!(decode(other_space, &token).is_err());
    }

    #[test]
    fn rejects_malformed() {
        let s = scope();
        assert!(decode(s, &[]).is_err());
        assert!(decode(s, &[1, 2, 3]).is_err());
        let mut token = encode(s, id(1));
        token[0] = 0xFF; // bad version
        assert!(decode(s, &token).is_err());
        let mut over = encode(s, id(1));
        over.push(0);
        assert!(decode(s, &over).is_err());
    }

    #[test]
    fn decode_opt_empty_is_first_page() {
        assert_eq!(decode_opt(scope(), &[]).unwrap(), None);
    }

    #[test]
    fn paginate_walks_without_overlap_or_gap() {
        let s = scope();
        let all: Vec<[u8; 16]> = (1u8..=7).map(id).collect();
        let mut seen = Vec::new();
        let mut cursor: Option<[u8; 16]> = None;
        loop {
            let (page, next) = paginate(s, all.clone(), cursor, 3, |x| *x);
            seen.extend(page.iter().copied());
            if next.is_empty() {
                break;
            }
            cursor = Some(decode(s, &next).unwrap());
        }
        assert_eq!(seen, all);
    }

    #[test]
    fn paginate_last_page_has_empty_cursor() {
        let s = scope();
        let all: Vec<[u8; 16]> = (1u8..=3).map(id).collect();
        let (page, next) = paginate(s, all.clone(), None, 3, |x| *x);
        assert_eq!(page, all);
        assert!(next.is_empty());
    }

    #[test]
    fn paginate_exact_multiple_final_page_empty() {
        let s = scope();
        let all: Vec<[u8; 16]> = (1u8..=6).map(id).collect();
        // First page of 3 → more remain.
        let (p1, n1) = paginate(s, all.clone(), None, 3, |x| *x);
        assert_eq!(p1.len(), 3);
        assert!(!n1.is_empty());
        let after = decode(s, &n1).unwrap();
        // Second page of 3 → exactly exhausts, so no next cursor.
        let (p2, n2) = paginate(s, all, Some(after), 3, |x| *x);
        assert_eq!(p2.len(), 3);
        assert!(n2.is_empty());
    }
}
