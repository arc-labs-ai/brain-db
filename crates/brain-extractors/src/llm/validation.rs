//! Schema-validation helper for the LLM extractor tier.
//!
//! On a schema-validation failure the extractor re-prompts the model
//! once with the validator's first error embedded in the system
//! block (per the extractor design); the second failure marks the
//! extraction as a dropped result with the error preserved in the
//! audit row.

use jsonschema::JSONSchema;
use serde_json::Value;

/// The operator-declared response schema, in the two forms the extractor
/// needs: the raw document sent to the provider as the tool schema, and the
/// compiled validator the reply is checked against. Built from a single
/// [`Value`] so the two can never describe different schemas.
pub struct ResponseSchema {
    raw: Value,
    validator: JSONSchema,
}

impl ResponseSchema {
    /// Compile an operator-declared schema. `Err` names why it failed.
    pub fn compile(raw: Value) -> Result<Self, String> {
        let validator = JSONSchema::options()
            .compile(&raw)
            .map_err(|e| format!("schema compile failed: {e}"))?;
        Ok(Self { raw, validator })
    }

    /// Return the raw document for the provider request body.
    pub fn raw(&self) -> &Value {
        &self.raw
    }

    /// Parse, normalise and validate one model reply.
    pub fn validate(&self, content: &str) -> Result<Value, String> {
        let mut parsed: Value =
            parse_json_payload(content).map_err(|e| format!("response is not valid JSON: {e}"))?;
        normalize_nullable_required(&self.raw, &mut parsed);
        if let Err(mut errs) = self.validator.validate(&parsed) {
            let msg = match errs.next() {
                Some(e) => e.to_string(),
                None => "unknown validation failure".into(),
            };
            return Err(msg);
        }
        Ok(parsed)
    }
}

// The compiled schema follows OpenAI's strict structured-output convention:
// every property is required, with optionality expressed by admitting null.
// Providers following ordinary JSON Schema omit those nullable keys instead.
// Normalize both response shapes before validation so the validator stays
// provider-agnostic and the shared schema can retain OpenAI's required list.
fn normalize_nullable_required(schema: &Value, response: &mut Value) {
    let Some(schema_object) = schema.as_object() else {
        return;
    };
    if schema_object
        .keys()
        .any(|key| matches!(key.as_str(), "$ref" | "anyOf" | "oneOf"))
    {
        return;
    }

    if schema_object.contains_key("properties") {
        let Some(response_object) = response.as_object_mut() else {
            return;
        };

        if let Some(required) = schema_object.get("required").and_then(Value::as_array) {
            if let Some(properties) = schema_object.get("properties").and_then(Value::as_object) {
                for name in required.iter().filter_map(Value::as_str) {
                    if !response_object.contains_key(name)
                        && properties.get(name).is_some_and(schema_admits_null)
                    {
                        response_object.insert(name.to_owned(), Value::Null);
                    }
                }
            }
        }

        if let Some(properties) = schema_object.get("properties").and_then(Value::as_object) {
            for (name, property_schema) in properties {
                if let Some(value) = response_object.get_mut(name) {
                    normalize_nullable_required(property_schema, value);
                }
            }
        }
        return;
    }

    if schema_type_includes(schema_object.get("type"), "array") {
        if let Some(items_schema) = schema_object.get("items") {
            if let Some(items) = response.as_array_mut() {
                for item in items {
                    normalize_nullable_required(items_schema, item);
                }
            }
        }
    }
}

fn schema_admits_null(schema: &Value) -> bool {
    match schema.get("type") {
        Some(Value::String(kind)) => kind == "null",
        Some(Value::Array(types)) => types.iter().any(|kind| kind.as_str() == Some("null")),
        _ => false,
    }
}

fn schema_type_includes(schema_type: Option<&Value>, expected: &str) -> bool {
    match schema_type {
        Some(Value::String(kind)) => kind == expected,
        Some(Value::Array(types)) => types.iter().any(|kind| kind.as_str() == Some(expected)),
        _ => false,
    }
}

/// Parse the JSON document a model reply carries.
///
/// A chat model asked for JSON by prompt alone (which is every
/// Anthropic call — the client sends no structured-output field) often
/// replies with the document plus packaging: a prose preamble ("Here is
/// the extracted graph:"), a Markdown fence, a note after the closing
/// fence, or an opening fence whose language tag shares the line with
/// the body. Every one of those puts a non-JSON byte at offset 0, which
/// serde reports as `expected value at line 1 column 1` — an error about
/// the packaging, not about the document inside it. Re-prompting does
/// not help, because at temperature 0 the same input produces the same
/// packaging, so the whole extraction is lost deterministically.
///
/// So a reply is read strictest-first: as sent, then with one Markdown
/// fence unwrapped, then narrowed to its first balanced `{…}` / `[…]`
/// span. The first reading that parses wins. When none parses, the error
/// from the reply AS SENT is returned, so a reply that was never JSON
/// still reports what it actually was rather than what a rescue attempt
/// made of it.
///
/// One shape is deliberately NOT rescued: a preamble carrying its own
/// balanced `{…}` aside ahead of the document, where the first span is
/// the aside and not the answer. Scanning on to later openers would
/// start returning a fragment of a malformed document (its first
/// complete inner object) as if it were the whole reply, which is a
/// worse failure than a parse error — it would validate as a document
/// missing its required sections, and misname the fault. Such a reply
/// fails, with its bytes in the extractor's warning.
fn parse_json_payload(content: &str) -> Result<Value, serde_json::Error> {
    let trimmed = content.trim();
    let error = match parse_unfenced(trimmed) {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    // Last resort: the document with prose around it. Tried only after
    // both whole-reply readings failed, so a well-formed reply is never
    // narrowed and can never lose a trailing section to a stray brace.
    for candidate in [
        balanced_json_span(unfence(trimmed)),
        balanced_json_span(trimmed),
    ]
    .into_iter()
    .flatten()
    {
        if let Ok(value) = serde_json::from_str::<Value>(candidate) {
            return Ok(value);
        }
    }
    Err(error)
}

/// Parse a reply as sent, then with one Markdown fence unwrapped. The
/// error is always the one from the reply AS SENT.
fn parse_unfenced(trimmed: &str) -> Result<Value, serde_json::Error> {
    let error = match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    let unfenced = unfence(trimmed);
    if unfenced != trimmed {
        if let Ok(value) = serde_json::from_str::<Value>(unfenced) {
            return Ok(value);
        }
    }
    Err(error)
}

/// Unwrap a Markdown code fence from a model reply.
///
/// Covers the shapes chat models actually emit: a fence wrapping the
/// whole reply, a fence opened after a prose preamble, text trailing the
/// closing fence, a MISSING closing fence (the reply was cut short), and
/// a language tag sharing its line with the body (```` ```json {…} ````).
/// Returns the input unchanged when it carries no fence, so a plain JSON
/// body is untouched.
fn unfence(trimmed: &str) -> &str {
    let Some(open) = trimmed.find("```") else {
        return trimmed;
    };
    // `find` lands on a char boundary and the fence is ASCII, so the
    // slice below is always valid UTF-8.
    let after = &trimmed[open + 3..];
    let body = match after.find('\n') {
        // Ordinary fence: an optional language tag, then the body on the
        // following lines.
        Some(newline) if !after[..newline].contains(['{', '[']) => &after[newline + 1..],
        // Tag and body share the opening fence line — drop the bare tag
        // word (`json`, `JSON5`, nothing at all).
        _ => after.trim_start_matches(|c: char| c.is_ascii_alphanumeric()),
    };
    match body.find("```") {
        Some(close) => body[..close].trim(),
        None => body.trim(),
    }
}

/// The first balanced `{…}` or `[…]` span in `text`, or `None` when the
/// text opens no JSON container or never closes the one it opens.
///
/// String-aware: a brace or bracket inside a JSON string, and an escaped
/// quote inside one, never move the nesting depth — otherwise a document
/// containing `"{"` as a value would be cut at the wrong byte.
fn balanced_json_span(text: &str) -> Option<&str> {
    let start = text.find(['{', '['])?;
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            // `start` is an opener, so depth is ≥ 1 at every closer; the
            // saturating form keeps a mismatched reply from wrapping.
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&text[start..start + offset + ch.len_utf8()]);
                }
            }
            _ => {}
        }
    }
    None
}

/// True when a model response is evidently an ATTEMPT at JSON (it opens
/// an object or array, possibly inside a code fence) — as opposed to a
/// deliberate plain-text answer. Such a response that fails to parse is
/// malformed output, never an entity name.
///
/// Deliberately blind to [`balanced_json_span`]: a plain-text reply that
/// happens to contain a bracketed aside is an entity name, not malformed
/// JSON, and must keep the bare-string projection.
fn looks_like_json(content: &str) -> bool {
    let trimmed = content.trim();
    [trimmed, unfence(trimmed)]
        .iter()
        .any(|candidate| candidate.starts_with('{') || candidate.starts_with('['))
}

/// The reason a free-form model response could not be parsed as JSON.
pub enum FreeformError {
    /// The response evidently attempted JSON but was malformed.
    MalformedJson(String),
    /// The response was deliberate plain text rather than JSON.
    PlainText,
}

/// Parse a free-form model response, preserving deliberate plain text.
///
/// Unlike [`ResponseSchema::validate`] this reads only the whole reply
/// (bare, then unfenced): a free-form extractor's reply may legitimately
/// BE prose, and narrowing prose to a bracketed fragment inside it would
/// turn an entity name into a parse error.
pub fn parse_freeform(content: &str) -> Result<Value, FreeformError> {
    match parse_unfenced(content.trim()) {
        Ok(value) => Ok(value),
        Err(error) if looks_like_json(content) => {
            Err(FreeformError::MalformedJson(error.to_string()))
        }
        Err(_) => Err(FreeformError::PlainText),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfence_unwraps_fenced_json() {
        assert_eq!(unfence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(unfence("```\n[1,2]\n```"), "[1,2]");
        assert_eq!(unfence("{\"a\":1}"), "{\"a\":1}");
        // No closing fence (the reply was cut short): the body is still
        // recovered rather than handed to serde with a backtick at
        // offset 0.
        assert_eq!(unfence("```json\n{\"a\":1}"), "{\"a\":1}");
        // Tag and body on one line.
        assert_eq!(unfence("```json {\"a\":1}```"), "{\"a\":1}");
        // Prose before the fence, a note after it.
        assert_eq!(
            unfence("Here you go:\n```json\n{\"a\":1}\n```\nLet me know."),
            "{\"a\":1}"
        );
        assert_eq!(unfence("Acme Corp"), "Acme Corp");
    }

    #[test]
    fn balanced_json_span_finds_the_document_in_prose() {
        assert_eq!(
            balanced_json_span("Here is the graph: {\"a\":[1,2]} — done."),
            Some("{\"a\":[1,2]}")
        );
        // Braces inside a string never move the depth.
        assert_eq!(
            balanced_json_span(r#"note {"a":"} {","b":"\""} tail"#),
            Some(r#"{"a":"} {","b":"\""}"#)
        );
        // Opened and never closed.
        assert_eq!(balanced_json_span("prose {\"a\":1"), None);
        // No container at all.
        assert_eq!(balanced_json_span("Acme Corp"), None);
    }

    #[test]
    fn looks_like_json_detects_attempted_structure() {
        assert!(looks_like_json(
            "{\"statements\": [{\"subject\": \"Mirror\"},{"
        ));
        assert!(looks_like_json("```json\n[{\"x\":1}\n```"));
        assert!(!looks_like_json("Acme Corp"));
        assert!(!looks_like_json("Mirror},{"));
        // A plain-text reply carrying a bracketed aside stays plain text.
        assert!(!looks_like_json("Acme Corp [the holding company]"));
    }

    #[test]
    fn validate_accepts_clean_json() {
        let schema = ResponseSchema::compile(statement_schema()).unwrap();
        let value = schema
            .validate(
                r#"{"statements":[{"subject":"A","predicate":"knows","object":"B","object_is_entity":true}]}"#,
            )
            .unwrap();
        assert_eq!(value["statements"][0]["subject"], "A");
    }

    #[test]
    fn validate_accepts_fenced_json() {
        let schema = ResponseSchema::compile(statement_schema()).unwrap();
        let body = r#"{"statements":[{"subject":"A","predicate":"knows","object":"B","object_is_entity":true}]}"#;
        for content in [
            format!("```json\n{body}\n```"),
            format!("```\n{body}\n```"),
            // Cut off before the closing fence.
            format!("```json\n{body}"),
            // Tag and body share the opening line.
            format!("```json {body}```"),
        ] {
            let value = schema.validate(&content).unwrap_or_else(|e| {
                panic!("fenced reply rejected: {e} (content: {content})");
            });
            assert_eq!(value["statements"][0]["object"], "B");
        }
    }

    #[test]
    fn validate_accepts_json_wrapped_in_prose() {
        let schema = ResponseSchema::compile(statement_schema()).unwrap();
        let body = r#"{"statements":[{"subject":"A","predicate":"knows","object":"B","object_is_entity":true}]}"#;
        for content in [
            format!("Here is the extracted graph:\n{body}"),
            format!("Sure! Here you go:\n```json\n{body}\n```\nLet me know if you need more."),
            format!("{body}\n\nNote: I skipped the greeting."),
        ] {
            let value = schema.validate(&content).unwrap_or_else(|e| {
                panic!("prose-wrapped reply rejected: {e} (content: {content})");
            });
            assert_eq!(value["statements"][0]["predicate"], "knows");
        }
    }

    #[test]
    fn validate_still_rejects_a_reply_that_is_not_json() {
        let schema = ResponseSchema::compile(statement_schema()).unwrap();
        for content in [
            "I cannot extract anything from this text.",
            "```json\n{\"statements\": [{\"subject\":",
            "",
            "{{",
        ] {
            let error = schema
                .validate(content)
                .expect_err("non-JSON reply must fail");
            assert!(
                error.starts_with("response is not valid JSON") || error.contains("is not of type"),
                "unexpected error for {content:?}: {error}"
            );
        }
    }

    #[test]
    fn validate_reports_the_reply_as_sent_not_a_rescue_attempt() {
        let schema = ResponseSchema::compile(statement_schema()).unwrap();
        let error = schema.validate("I cannot help with that.").unwrap_err();
        // serde's verdict on offset 0 of the reply itself.
        assert!(
            error.contains("expected value at line 1 column 1"),
            "{error}"
        );
    }

    #[test]
    fn freeform_prose_with_brackets_stays_plain_text() {
        match parse_freeform("Acme Corp [the holding company]") {
            Err(FreeformError::PlainText) => {}
            Ok(v) => panic!("prose parsed as JSON: {v}"),
            Err(FreeformError::MalformedJson(e)) => panic!("prose read as malformed JSON: {e}"),
        }
    }

    #[test]
    fn freeform_recovers_a_fence_without_its_close() {
        let value = match parse_freeform("```json\n[{\"name\":\"Acme Corp\"}]") {
            Ok(value) => value,
            Err(FreeformError::MalformedJson(e)) => panic!("unterminated fence rejected: {e}"),
            Err(FreeformError::PlainText) => panic!("unterminated fence read as plain text"),
        };
        assert_eq!(value[0]["name"], "Acme Corp");
    }

    #[test]
    fn freeform_truncated_json_is_still_malformed() {
        match parse_freeform("{\"statements\": [{\"subject\": \"Mirror\"},{") {
            Err(FreeformError::MalformedJson(_)) => {}
            Ok(v) => panic!("truncated JSON parsed: {v}"),
            Err(FreeformError::PlainText) => panic!("truncated JSON read as plain text"),
        }
    }

    fn statement_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["statements"],
            "properties": {
                "statements": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": [
                            "subject", "predicate", "object", "object_is_entity", "kind",
                            "confidence", "event_at", "subject_is_self", "retract", "is_stateful"
                        ],
                        "properties": {
                            "subject": {"type": "string"},
                            "predicate": {"type": "string"},
                            "object": {"type": "string"},
                            "object_is_entity": {"type": "boolean"},
                            "kind": {"type": ["string", "null"]},
                            "confidence": {"type": ["number", "null"]},
                            "event_at": {"type": ["string", "null"]},
                            "subject_is_self": {"type": ["boolean", "null"]},
                            "retract": {"type": ["boolean", "null"]},
                            "is_stateful": {"type": ["boolean", "null"]}
                        }
                    }
                }
            }
        })
    }

    #[test]
    fn nullable_required_fields_are_filled_through_nested_arrays() {
        let raw = statement_schema();
        let schema = ResponseSchema::compile(raw).unwrap();
        let value = schema
            .validate(
                r#"{"statements":[{"subject":"A","predicate":"knows","object":"B","object_is_entity":true}]}"#,
            )
            .unwrap();
        let statement = &value["statements"][0];
        for name in [
            "event_at",
            "kind",
            "confidence",
            "subject_is_self",
            "retract",
            "is_stateful",
        ] {
            assert_eq!(statement[name], Value::Null, "{name}");
        }
    }

    #[test]
    fn explicitly_null_nullable_required_fields_are_preserved() {
        let raw = statement_schema();
        let schema = ResponseSchema::compile(raw).unwrap();
        let content = r#"{"statements":[{"subject":"A","predicate":"knows","object":"B","object_is_entity":true,"kind":null,"confidence":null,"event_at":null,"subject_is_self":null,"retract":null,"is_stateful":null}]}"#;
        let value = schema.validate(content).unwrap();
        assert_eq!(value, serde_json::from_str::<Value>(content).unwrap());
    }

    #[test]
    fn non_nullable_required_fields_are_not_filled() {
        let raw = statement_schema();
        let schema = ResponseSchema::compile(raw).unwrap();
        let error = schema
            .validate(
                r#"{"statements":[{"predicate":"knows","object":"B","object_is_entity":true}]}"#,
            )
            .unwrap_err();
        assert!(error.contains("subject"), "{error}");
    }

    #[test]
    fn supplied_nullable_values_are_never_overwritten() {
        let raw = statement_schema();
        let schema = ResponseSchema::compile(raw).unwrap();
        let value = schema
            .validate(
                r#"{"statements":[{"subject":"A","predicate":"knows","object":"B","object_is_entity":true,"event_at":"2026-11-19T00:00:00Z"}]}"#,
            )
            .unwrap();
        assert_eq!(value["statements"][0]["event_at"], "2026-11-19T00:00:00Z");
    }

    #[test]
    fn raw_returns_the_document_compiled_from() {
        let raw = statement_schema();
        let schema = ResponseSchema::compile(raw.clone()).unwrap();
        assert_eq!(schema.raw(), &raw);
    }
}
