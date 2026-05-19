//! Tab completion — subcommand names + flag stems.
//!
//! v1 keeps this static: dynamic id completion from the session's
//! recent-ids list is a future polish.

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
];

const FLAGS: &[&str] = &[
    "--server",
    "--agent-id",
    "--output",
    "--timeout",
    "--context",
    "--kind",
    "--salience",
    "--deduplicate",
    "--txn",
    "--top-k",
    "--confidence",
    "--filter-context",
    "--filter-kind",
    "--max-steps",
    "--max-wall-time-ms",
    "--depth",
    "--max-inferences",
    "--mode",
    "--weight",
    "--start-lsn",
    "--collect",
];

/// rustyline helper that completes subcommand + flag names.
#[derive(Default)]
pub struct ShellHelper;

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
        // Find the start of the word under the cursor.
        let prefix_bytes = &line.as_bytes()[..pos];
        let word_start = prefix_bytes
            .iter()
            .rposition(|b| *b == b' ' || *b == b'\t')
            .map(|i| i + 1)
            .unwrap_or(0);
        let word = &line[word_start..pos];

        let pool: &[&str] = if word.starts_with("--") || word.starts_with('-') {
            FLAGS
        } else if word_start == 0 {
            SUBCOMMANDS
        } else {
            // No useful static suggestion for positional args mid-line.
            return Ok((word_start, vec![]));
        };

        let cands: Vec<Pair> = pool
            .iter()
            .filter(|name| name.starts_with(word))
            .map(|name| Pair {
                display: (*name).to_string(),
                replacement: (*name).to_string(),
            })
            .collect();
        Ok((word_start, cands))
    }
}
