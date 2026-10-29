//! Projecting schema parse and validation failures onto the wire.
//!
//! `SCHEMA_UPLOAD`, `SCHEMA_REPLACE` and `SCHEMA_DROP` all answer a rejected
//! document the same way — a success frame whose `validation_errors` carry
//! the reasons — so the mapping from an internal error to
//! [`SchemaValidationErrorWire`] belongs to all three rather than to any one
//! of them.
//!
//! It lived in `schema.rs` and was hand-copied into `schema_replace.rs`, which
//! is how the two drifted: the copy spells `severity` the same by luck, not by
//! construction. `schema_drop.rs` then needed it too, and reaching into a
//! sibling handler for it would have made a third way to say one thing.

use brain_protocol::schema::{ParseError, ValidationError};
use brain_protocol::SchemaValidationErrorWire;

/// Every finding here is an error rather than a warning. The wire reserves
/// lower values for advisories, which the schema path does not yet produce.
const SEVERITY_ERROR: u8 = 2;

/// A parse failure, located at the token that caused it.
#[must_use]
pub fn parse_error(e: &ParseError) -> SchemaValidationErrorWire {
    let (code, line, col) = match e {
        ParseError::Syntax { line, col, .. } => ("Syntax", *line, *col),
        ParseError::InvalidNumber { line, col, .. } => ("InvalidNumber", *line, *col),
        ParseError::InvalidJson { line, col, .. } => ("InvalidJson", *line, *col),
        ParseError::InvalidDuration { line, col, .. } => ("InvalidDuration", *line, *col),
        ParseError::InvalidCost { line, col, .. } => ("InvalidCost", *line, *col),
        ParseError::MissingField { line, col, .. } => ("MissingField", *line, *col),
    };
    SchemaValidationErrorWire {
        code: code.to_string(),
        message: e.to_string(),
        line: line as u32,
        column: col as u32,
        // A parse error has no span: it is a point, and the caller's own
        // document is what gives it width.
        length: 0,
        severity: SEVERITY_ERROR,
    }
}

/// A validation failure. Carries its span when the document had one — a
/// document narrowed by `SCHEMA_DROP` has no source text to point into, so
/// the zeroes are "unlocated", not "at the start of the file".
#[must_use]
pub fn validation_error(e: &ValidationError) -> SchemaValidationErrorWire {
    let (line, column, length) = e
        .source_span
        .map_or((0, 0, 0), |s| (s.line, s.column, s.length));
    SchemaValidationErrorWire {
        code: format!("{:?}", e.code),
        message: e.message.clone(),
        line,
        column,
        length,
        severity: SEVERITY_ERROR,
    }
}

/// Project a whole batch, which is how every caller uses these.
#[must_use]
pub fn validation_errors<'a>(
    errs: impl IntoIterator<Item = &'a ValidationError>,
) -> Vec<SchemaValidationErrorWire> {
    errs.into_iter().map(validation_error).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_protocol::schema::{SourceSpan, ValidationErrorCode};

    #[test]
    fn a_parse_error_carries_the_position_it_failed_at() {
        let wire = parse_error(&ParseError::Syntax {
            line: 7,
            col: 3,
            message: "boom".into(),
        });
        assert_eq!(wire.code, "Syntax");
        assert_eq!((wire.line, wire.column), (7, 3));
        assert_eq!(wire.severity, SEVERITY_ERROR);
    }

    #[test]
    fn a_validation_error_carries_its_span_when_the_document_had_one() {
        let wire = validation_error(&ValidationError {
            code: ValidationErrorCode::DuplicateDefinition,
            message: "dup".into(),
            source_span: Some(SourceSpan {
                line: 4,
                column: 5,
                length: 6,
            }),
        });
        assert_eq!(wire.code, "DuplicateDefinition");
        assert_eq!((wire.line, wire.column, wire.length), (4, 5, 6));
    }

    #[test]
    fn an_unlocated_validation_error_reports_zeroes_not_a_guess() {
        // A document narrowed by SCHEMA_DROP has no source text to point
        // into, so this is "unlocated" rather than "at the start of the file"
        // — the zeroes are the honest answer, not a default.
        let wire = validation_error(&ValidationError {
            code: ValidationErrorCode::NamespaceMissing,
            message: "missing".into(),
            source_span: None,
        });
        assert_eq!((wire.line, wire.column, wire.length), (0, 0, 0));
    }

    #[test]
    fn a_batch_projects_every_error() {
        let errs = [
            ValidationError {
                code: ValidationErrorCode::NamespaceMissing,
                message: "a".into(),
                source_span: None,
            },
            ValidationError {
                code: ValidationErrorCode::DuplicateDefinition,
                message: "b".into(),
                source_span: None,
            },
        ];
        let wire = validation_errors(errs.iter());
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[1].message, "b");
    }
}
