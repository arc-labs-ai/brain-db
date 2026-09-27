//! Source positions for validator errors.
//!
//! The AST carries no spans (it is persisted as JSON in the schema blob, so
//! adding them would change stored bytes), which left every semantic error at
//! `0:0` — a client could say *what* was wrong but not *where*. When the DSL
//! text is at hand (every SCHEMA_UPLOAD / VALIDATE / REPLACE), this module
//! resolves each span-less error to the offending definition and, where the
//! message names one, the offending field.
//!
//! It reads the messages [`super::validator`] itself produces — an internal,
//! tested contract, never user text — so every error code has a test below: a
//! reworded message fails loudly here instead of silently losing its span.

use super::ast::Schema;
use super::validator::{validate, SourceSpan, ValidatedSchema, ValidationErrors};

/// [`validate`], with each error's `source_span` resolved against `source` (the
/// text `schema` was parsed from) when the validator left it empty.
///
/// # Errors
/// The same errors as [`validate`], now positioned.
pub fn validate_located(
    source: &str,
    schema: &Schema,
) -> Result<ValidatedSchema, ValidationErrors> {
    validate(schema).map_err(|mut errors| {
        locate_spans(source, &mut errors);
        errors
    })
}

/// Fill in `source_span` for every error that lacks one and can be placed.
pub fn locate_spans(source: &str, errors: &mut ValidationErrors) {
    let lines: Vec<&str> = source.lines().collect();
    for e in errors.iter_mut().filter(|e| e.source_span.is_none()) {
        e.source_span = locate(&lines, &e.message);
    }
}

const DEF_KINDS: [&str; 5] = [
    "entity_type",
    "predicate",
    "relation_type",
    "extractor",
    "kind",
];

fn locate(lines: &[&str], message: &str) -> Option<SourceSpan> {
    if message.starts_with("schema must declare a `namespace`") {
        return Some(SourceSpan {
            line: 1,
            column: 1,
            length: 0,
        });
    }
    if let Some(rest) = message.strip_prefix("namespace ") {
        let name = first_quoted(rest)?;
        return find_token_in_line_starting(lines, 0, lines.len(), "namespace", &name);
    }
    if let Some(rest) = message.strip_prefix("kind name ") {
        let name = first_quoted(rest)?;
        return def_span(lines, "kind", &name, 1);
    }

    // `duplicate <kind> "<name>"` → the SECOND definition (the first is valid).
    if let Some(rest) = message.strip_prefix("duplicate ") {
        let (kind, rest) = split_kind(rest)?;
        let name = first_quoted(rest)?;
        return def_span(lines, kind, &name, 2);
    }

    // `<kind> "<name>": …` / `<kind> name "<name>" exceeds …`.
    if let Some((kind, rest)) = split_kind(message) {
        let rest = rest.strip_prefix("name ").unwrap_or(rest);
        let name = first_quoted(rest)?;
        let (start, end) = def_block(lines, Some(kind), &name, 1)?;
        let detail = rest.split_once(": ").map_or("", |(_, d)| d);
        return field_span(lines, start, end, detail).or_else(|| token_span(lines, start, &name));
    }

    // Attribute errors: `<Owner>.<attr>: …` (owner is an entity or relation).
    let (head, _) = message.split_once(": ")?;
    let (owner, attr) = head.split_once('.')?;
    let (start, end) = def_block(lines, None, owner, 1)?;
    find_token_in_line_starting(lines, start + 1, end, attr, attr)
        .or_else(|| token_span(lines, start, owner))
}

/// Refine to the field a detail message names.
fn field_span(lines: &[&str], start: usize, end: usize, detail: &str) -> Option<SourceSpan> {
    let quoted = first_quoted(detail);
    let at = |key: &str| match &quoted {
        Some(tok) => find_token_in_line_starting(lines, start + 1, end, key, tok),
        None => find_token_in_line_starting(lines, start + 1, end, key, key),
    };
    if detail.starts_with("from_type") {
        return at("from:");
    }
    if detail.starts_with("to_type") {
        return at("to:");
    }
    if detail.starts_with("object Entity<") || detail.starts_with("kind ") {
        // Both forms quote the entity type when the object is `Entity<…>`;
        // a `Value<…>` object has nothing quoted, so point at `object`.
        let tok = quoted.clone().unwrap_or_else(|| "object".to_owned());
        return find_token_in_line_starting(lines, start + 1, end, "object:", &tok);
    }
    if detail.starts_with("`symmetric") {
        return find_token_in_line_starting(lines, start + 1, end, "symmetric:", "symmetric");
    }
    if detail.starts_with("target ") {
        return at("target:");
    }
    if let Some(r) = detail.strip_prefix("field ") {
        // A repeated field: point at its second occurrence.
        let field = r.split_whitespace().next()?;
        let key = format!("{field}:");
        let first = find_line_starting(lines, start + 1, end, &key)?;
        return find_token_in_line_starting(lines, first + 1, end, &key, field);
    }
    for key in ["confidence_threshold", "confidence"] {
        if detail.starts_with(key) {
            return find_token_in_line_starting(lines, start + 1, end, &format!("{key}:"), key);
        }
    }
    None
}

/// Split a leading definition keyword off `s` (`"predicate \"x\": …"`).
fn split_kind(s: &str) -> Option<(&'static str, &str)> {
    DEF_KINDS.iter().find_map(|k| {
        s.strip_prefix(k)
            .and_then(|r| r.strip_prefix(' '))
            .map(|r| (*k, r))
    })
}

/// The first `"…"` in `s` (validator messages quote names with `{:?}`).
fn first_quoted(s: &str) -> Option<String> {
    let start = s.find('"')? + 1;
    let len = s[start..].find('"')?;
    Some(s[start..start + len].to_owned())
}

/// `[start, end)` line range of the `nth` definition of `name` (of `kind`, or
/// of any kind), ending before the next `define`.
fn def_block(lines: &[&str], kind: Option<&str>, name: &str, nth: usize) -> Option<(usize, usize)> {
    let mut seen = 0;
    let start = lines.iter().position(|l| {
        let mut words = l.split_whitespace();
        let hit = words.next() == Some("define")
            && kind.is_none_or(|k| words.next() == Some(k))
            && l.split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|w| w == name)
            && (kind.is_some() || DEF_KINDS.iter().any(|k| l.contains(k)));
        if hit {
            seen += 1;
        }
        hit && seen == nth
    })?;
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with("define "))
        .map_or(lines.len(), |p| start + 1 + p);
    Some((start, end))
}

fn def_span(lines: &[&str], kind: &str, name: &str, nth: usize) -> Option<SourceSpan> {
    let (start, _) = def_block(lines, Some(kind), name, nth)?;
    token_span(lines, start, name)
}

fn find_line_starting(lines: &[&str], from: usize, to: usize, prefix: &str) -> Option<usize> {
    (from..to.min(lines.len())).find(|&i| lines[i].trim_start().starts_with(prefix))
}

fn find_token_in_line_starting(
    lines: &[&str],
    from: usize,
    to: usize,
    prefix: &str,
    token: &str,
) -> Option<SourceSpan> {
    let i = find_line_starting(lines, from, to, prefix)?;
    token_span(lines, i, token)
}

/// Span of the first whole-word `token` on line `i` (1-based line and column,
/// counted in characters).
fn token_span(lines: &[&str], i: usize, token: &str) -> Option<SourceSpan> {
    let line = lines.get(i)?;
    let mut from = 0;
    while let Some(off) = line[from..].find(token) {
        let at = from + off;
        let before = line[..at].chars().next_back();
        let after = line[at + token.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if boundary(before) && boundary(after) {
            return Some(SourceSpan {
                line: u32::try_from(i + 1).ok()?,
                column: u32::try_from(line[..at].chars().count() + 1).ok()?,
                length: u32::try_from(token.chars().count()).ok()?,
            });
        }
        from = at + token.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::parser::parse_schema;
    use crate::schema::validator::ValidationErrorCode as C;

    fn validate_text(source: &str) -> Result<ValidatedSchema, ValidationErrors> {
        let schema = parse_schema(source).expect("parses");
        validate_located(source, &schema)
    }

    fn spans(src: &str) -> Vec<(C, u32, u32, u32)> {
        let errs = validate_text(src).expect_err("invalid");
        errs.iter()
            .map(|e| {
                let s = e
                    .source_span
                    .unwrap_or_else(|| panic!("unplaced: {}", e.message));
                (e.code, s.line, s.column, s.length)
            })
            .collect()
    }

    #[test]
    fn unresolved_relation_endpoint_points_at_the_bad_type() {
        let src = "namespace acme\ndefine entity_type Env { }\ndefine relation_type clones {\n    from: Env\n    to: TargetAp\n}\n";
        assert_eq!(spans(src), vec![(C::UnresolvedTypeRef, 5, 9, 8)]);
    }

    #[test]
    fn predicate_kind_object_mismatch_points_at_the_object() {
        let src = "namespace acme\ndefine entity_type Env { }\ndefine predicate best {\n    kind: Preference\n    object: Entity<Env>\n}\n";
        assert_eq!(spans(src), vec![(C::PredicateKindObjectMismatch, 5, 20, 3)]);
    }

    #[test]
    fn unresolved_predicate_object_points_at_the_type() {
        let src =
            "namespace acme\ndefine predicate p {\n    kind: Fact\n    object: Entity<Nope>\n}\n";
        assert_eq!(spans(src), vec![(C::UnresolvedTypeRef, 4, 20, 4)]);
    }

    #[test]
    fn symmetric_on_one_to_many_points_at_symmetric() {
        let src = "namespace acme\ndefine entity_type E { }\ndefine relation_type r {\n    from: E\n    to: E\n    cardinality: one-to-many\n    symmetric: true\n}\n";
        assert_eq!(
            spans(src),
            vec![(C::RelationCardinalitySymmetricInvalid, 7, 5, 9)]
        );
    }

    #[test]
    fn duplicate_points_at_the_second_definition() {
        let src = "namespace acme\ndefine entity_type E { }\ndefine entity_type E { }\n";
        assert_eq!(spans(src), vec![(C::DuplicateDefinition, 3, 20, 1)]);
    }

    #[test]
    fn attribute_errors_point_at_the_attribute() {
        let src = "namespace acme\ndefine entity_type E {\n    attributes {\n        boss: ref<E> unique\n        n: number default \"x\"\n    }\n}\n";
        assert_eq!(
            spans(src),
            vec![
                (C::AttributeUniqueOnRefType, 4, 9, 4),
                (C::DefaultIncompatibleWithType, 5, 9, 1),
            ]
        );
    }

    #[test]
    fn namespace_errors_point_at_the_namespace() {
        assert_eq!(
            spans("namespace brain\n"),
            vec![(C::NamespaceInvalidIdentifier, 1, 11, 5)]
        );
        assert_eq!(spans("# nothing\n"), vec![(C::NamespaceMissing, 1, 1, 0)]);
    }

    #[test]
    fn extractor_errors_point_at_the_definition_or_field() {
        let src =
            "namespace acme\ndefine extractor x {\n    kind: pattern\n    target: entity Nope\n}\n";
        let got = spans(src);
        assert!(got.contains(&(C::UnresolvedTypeRef, 4, 20, 4)), "{got:?}");
        assert!(
            got.contains(&(C::ExtractorMissingRequired, 2, 18, 1)),
            "{got:?}"
        );
    }

    #[test]
    fn located_is_identity_on_valid_schemas() {
        assert!(validate_text("namespace acme\ndefine entity_type E { }\n").is_ok());
    }
}
