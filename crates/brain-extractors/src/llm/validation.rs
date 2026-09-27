//! Schema-validation helper for the LLM extractor tier.
//!
//! On a schema-validation failure the extractor re-prompts the model
//! once with the validator's first error embedded in the system
//! block (per the extractor design); the second failure marks the
//! extraction as a dropped result with the error preserved in the
//! audit row.

use jsonschema::JSONSchema;
use serde_json::Value;

pub(super) fn validate_against(schema: &JSONSchema, content: &str) -> Result<Value, String> {
    let parsed: Value = serde_json::from_str(strip_code_fences(content))
        .map_err(|e| format!("response is not valid JSON: {e}"))?;
    if let Err(mut errs) = schema.validate(&parsed) {
        let msg = match errs.next() {
            Some(e) => e.to_string(),
            None => "unknown validation failure".into(),
        };
        return Err(msg);
    }
    Ok(parsed)
}

/// Strip one surrounding Markdown code fence (```` ```json … ``` ```` or
/// ```` ``` … ``` ````) that chat models commonly wrap JSON in. Anything
/// that is not a complete fenced block is returned trimmed but otherwise
/// untouched, so a plain JSON body parses exactly as before.
pub(super) fn strip_code_fences(content: &str) -> &str {
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
pub(super) fn looks_like_json(content: &str) -> bool {
    let t = strip_code_fences(content);
    t.starts_with('{') || t.starts_with('[')
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
}
