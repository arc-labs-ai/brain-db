//! Spec §11/03 §7 — default consolidation prompt builder.
//!
//! Both adapters share the same prompt today. Operators wanting
//! a custom prompt set `cfg.summarizer.prompt_template` (a v2
//! follow-up). v1 hard-codes the spec's example.

#![cfg(target_os = "linux")]

const SYSTEM_PROMPT: &str =
    "You are a memory consolidation system. Below are several memories from the same context. \
     Summarize them into a single, concise paragraph that captures the key information.";

/// Build the user-side prompt from a slice of memory texts. Returns
/// `(system_prompt, user_prompt)` so the OpenAI adapter (which uses
/// messages) and the Ollama adapter (which takes a single prompt
/// string) can format as needed.
pub(crate) fn build_consolidation_prompt(memories: &[&str]) -> (&'static str, String) {
    let mut user = String::with_capacity(64 + memories.iter().map(|m| m.len() + 8).sum::<usize>());
    user.push_str("Memories:\n");
    for (idx, memory) in memories.iter().enumerate() {
        // Indices match the spec's "1.", "2." formatting.
        user.push_str(&format!("{}. {}\n", idx + 1, memory.trim()));
    }
    user.push_str("\nSummary:");
    (SYSTEM_PROMPT, user)
}

/// Convenience for adapters that want a single combined string
/// (Ollama's `/api/generate` doesn't take a system/user split).
#[cfg_attr(not(feature = "summarizer-ollama"), allow(dead_code))]
pub(crate) fn combined_prompt(memories: &[&str]) -> String {
    let (system, user) = build_consolidation_prompt(memories);
    format!("{system}\n\n{user}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_consolidation_prompt_numbers_memories() {
        let (system, user) = build_consolidation_prompt(&["a", "b", "c"]);
        assert!(system.contains("memory consolidation system"));
        assert!(user.contains("1. a"));
        assert!(user.contains("2. b"));
        assert!(user.contains("3. c"));
        assert!(user.ends_with("Summary:"));
    }

    #[test]
    fn build_consolidation_prompt_trims_memory_text() {
        let (_, user) = build_consolidation_prompt(&["   hello\n"]);
        assert!(user.contains("1. hello\n"));
    }

    #[test]
    fn combined_prompt_concatenates() {
        let combined = combined_prompt(&["one"]);
        assert!(combined.contains("memory consolidation system"));
        assert!(combined.contains("1. one"));
    }
}
