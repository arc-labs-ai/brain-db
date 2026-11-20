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
        let mut parsed: Value = serde_json::from_str(strip_code_fences(content))
            .map_err(|e| format!("response is not valid JSON: {e}"))?;
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

/// Strip one surrounding Markdown code fence (```` ```json … ``` ```` or
/// ```` ``` … ``` ````) that chat models commonly wrap JSON in. Anything
/// that is not a complete fenced block is returned trimmed but otherwise
/// untouched, so a plain JSON body parses exactly as before.
fn strip_code_fences(content: &str) -> &str {
    let t = content.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let Some(body) = rest.strip_suffix("```") else {
        return t;
    };
    // Drop an optional language tag on the opening fence line.
    match body.split_once('\n') {
        Some((tag, inner)) if !tag.trim().contains(['{', '[']) => inner.trim(),
        _ => body.trim(),
    }
}

/// True when a model response is evidently an ATTEMPT at JSON (it opens
/// an object or array) — as opposed to a deliberate plain-text answer.
/// Such a response that fails to parse is malformed output, never an
/// entity name.
fn looks_like_json(content: &str) -> bool {
    let t = strip_code_fences(content);
    t.starts_with('{') || t.starts_with('[')
}

/// The reason a free-form model response could not be parsed as JSON.
pub enum FreeformError {
    /// The response evidently attempted JSON but was malformed.
    MalformedJson(String),
    /// The response was deliberate plain text rather than JSON.
    PlainText,
}

/// Parse a free-form model response, preserving deliberate plain text.
pub fn parse_freeform(content: &str) -> Result<Value, FreeformError> {
    match serde_json::from_str::<Value>(strip_code_fences(content)) {
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
    fn strip_code_fences_unwraps_fenced_json() {
        assert_eq!(strip_code_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fences("```\n[1,2]\n```"), "[1,2]");
        assert_eq!(strip_code_fences("  {\"a\":1}  "), "{\"a\":1}");
        // Not a complete fence: returned trimmed, untouched.
        assert_eq!(
            strip_code_fences("```json\n{\"a\":1}"),
            "```json\n{\"a\":1}"
        );
        assert_eq!(strip_code_fences("Acme Corp"), "Acme Corp");
    }

    #[test]
    fn looks_like_json_detects_attempted_structure() {
        assert!(looks_like_json(
            "{\"statements\": [{\"subject\": \"Mirror\"},{"
        ));
        assert!(looks_like_json("```json\n[{\"x\":1}\n```"));
        assert!(!looks_like_json("Acme Corp"));
        assert!(!looks_like_json("Mirror},{"));
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
