//! Tab completion — subcommand names, flag stems, and recent ids.
//!
//! Recent ids come from the live `Session.recent_ids` ring (cap 100).
//! The ring is shared via `Arc<Mutex<...>>` because rustyline owns the
//! helper for the lifetime of the editor; the REPL loop mutates the
//! same buffer on every op result.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use brain_core::MemoryId;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};

const SUBCOMMANDS: &[&str] = &[
    "encode",
    "recall",
    "plan",
    "reason",
    "forget",
    "link",
    "unlink",
    "txn",
    "subscribe",
    "shell",
    "generate-completion",
    "help",
    "quit",
    "exit",
    "entity",
    "statement",
    "relation",
    "mention",
    "extract",
];

const FLAGS: &[&str] = &[
    "--server",
    "--agent",
    "--agent-id",
    "--output",
    "--timeout",
    "--color",
    "--hyperlinks",
    "--context",
    "--kind",
    "--salience",
    "--deduplicate",
    "--no-dedup",
    "--edge",
    "--request-id",
    "--from-file",
    "--from-stdin",
    "--wait-for-extraction",
    "--vector",
    "--txn",
    "--top-k",
    "--confidence",
    "--filter-context",
    "--filter-kind",
    "--include-text",
    "--include-graph",
    "--strategy",
    "--max-steps",
    "--max-wall-time-ms",
    "--depth",
    "--max-inferences",
    "--mode",
    "--weight",
    "--start-lsn",
    "--collect",
    "--type",
    "--limit",
    "--prefix",
    "--subject",
    "--predicate",
    "--object",
    "--from",
    "--to",
    "--memory",
    "--entity",
    "--since",
    "--all",
];

/// rustyline helper that completes subcommand + flag names + recent ids.
///
/// `recent` is shared with the REPL loop's session so completions stay
/// current as the user runs commands.
#[derive(Default, Clone)]
pub struct ShellHelper {
    pub recent: Arc<Mutex<VecDeque<MemoryId>>>,
}

impl ShellHelper {
    /// Build a helper bound to the given shared id ring.
    #[must_use]
    pub fn with_recent(recent: Arc<Mutex<VecDeque<MemoryId>>>) -> Self {
        Self { recent }
    }
}

impl Helper for ShellHelper {}

impl Hinter for ShellHelper {
    type Hint = String;
}

impl Highlighter for ShellHelper {}

impl Validator for ShellHelper {}

impl Completer for ShellHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let prefix_bytes = &line.as_bytes()[..pos];
        let word_start = prefix_bytes
            .iter()
            .rposition(|b| *b == b' ' || *b == b'\t')
            .map(|i| i + 1)
            .unwrap_or(0);
        let word = &line[word_start..pos];

        if word.starts_with("--") || word.starts_with('-') {
            let cands: Vec<Pair> = FLAGS
                .iter()
                .filter(|name| name.starts_with(word))
                .map(|name| Pair {
                    display: (*name).to_string(),
                    replacement: (*name).to_string(),
                })
                .collect();
            return Ok((word_start, cands));
        }
        if word_start == 0 {
            let cands: Vec<Pair> = SUBCOMMANDS
                .iter()
                .filter(|name| name.starts_with(word))
                .map(|name| Pair {
                    display: (*name).to_string(),
                    replacement: (*name).to_string(),
                })
                .collect();
            return Ok((word_start, cands));
        }

        // Positional word in the middle of the line — offer recent ids
        // if the prefix looks id-shaped (`s`, `0x`, or a digit). Avoids
        // dumping ids into the middle of free text like the `encode`
        // body or recall queries.
        if looks_like_id_prefix(word) {
            let ring = self.recent.lock().unwrap_or_else(|e| e.into_inner());
            let cands: Vec<Pair> = ring
                .iter()
                .map(|id| short_form(*id))
                .filter(|s| s.starts_with(word))
                .map(|s| Pair {
                    display: s.clone(),
                    replacement: s,
                })
                .collect();
            return Ok((word_start, cands));
        }

        Ok((word_start, vec![]))
    }
}

fn looks_like_id_prefix(s: &str) -> bool {
    s.starts_with('s')
        || s.starts_with('S')
        || s.starts_with("0x")
        || s.starts_with("0X")
        || s.chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
}

fn short_form(id: MemoryId) -> String {
    format!("s{}/m{}/v{}", id.shard(), id.slot(), id.version())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_prefix_detection() {
        assert!(looks_like_id_prefix("s2/m"));
        assert!(looks_like_id_prefix("S"));
        assert!(looks_like_id_prefix("0xab"));
        assert!(looks_like_id_prefix("42"));
        assert!(!looks_like_id_prefix("hello"));
        assert!(!looks_like_id_prefix(""));
    }

    #[test]
    fn completer_offers_recent_ids_for_id_prefix() {
        let ring = Arc::new(Mutex::new(VecDeque::from([
            MemoryId::pack(2, 17, 1),
            MemoryId::pack(2, 18, 1),
            MemoryId::pack(3, 1, 1),
        ])));
        let helper = ShellHelper::with_recent(ring);
        let mut hist = rustyline::history::DefaultHistory::new();
        let ctx = Context::new(&mut hist);
        // "forget s2/m1" — partial id prefix should match the two s2 ids.
        let (start, cands) = helper.complete("forget s2/m1", 12, &ctx).unwrap();
        assert_eq!(start, 7);
        let strings: Vec<&str> = cands.iter().map(|c| c.replacement.as_str()).collect();
        assert!(
            strings.contains(&"s2/m17/v1"),
            "expected s2/m17/v1 in {strings:?}"
        );
        assert!(
            strings.contains(&"s2/m18/v1"),
            "expected s2/m18/v1 in {strings:?}"
        );
    }

    #[test]
    fn completer_returns_empty_for_free_text() {
        let helper = ShellHelper::default();
        let mut hist = rustyline::history::DefaultHistory::new();
        let ctx = Context::new(&mut hist);
        let (_, cands) = helper.complete("encode hello world", 18, &ctx).unwrap();
        assert!(cands.is_empty());
    }

    #[test]
    fn completer_offers_subcommands_at_line_start() {
        let helper = ShellHelper::default();
        let mut hist = rustyline::history::DefaultHistory::new();
        let ctx = Context::new(&mut hist);
        let (_, cands) = helper.complete("ent", 3, &ctx).unwrap();
        let strings: Vec<&str> = cands.iter().map(|c| c.replacement.as_str()).collect();
        assert!(strings.contains(&"entity"));
    }
}
