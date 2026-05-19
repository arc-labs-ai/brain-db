//! Integration tests for the shared clap tree + tokenizer.

use brain_shell::parser::{
    parse_txn_id, tokenize_line, Cli, Command, KindArg, OutputFormatArg, TxnCommand,
};
use clap::Parser;

fn parse_argv(argv: &[&str]) -> Cli {
    let mut full = vec!["brain".to_string()];
    full.extend(argv.iter().map(|s| (*s).to_string()));
    Cli::try_parse_from(full).expect("parse should succeed")
}

#[test]
fn one_shot_encode_with_all_flags() {
    let cli = parse_argv(&[
        "encode",
        "hello world",
        "--context",
        "7",
        "--kind",
        "semantic",
        "--salience",
        "0.8",
        "--deduplicate",
    ]);
    match cli.subcommand {
        Some(Command::Encode(a)) => {
            assert_eq!(a.text, "hello world");
            assert_eq!(a.context, Some(7));
            assert_eq!(a.kind, Some(KindArg::Semantic));
            assert_eq!(a.salience, Some(0.8));
            assert!(a.deduplicate);
        }
        other => panic!("expected Encode, got {other:?}"),
    }
}

#[test]
fn one_shot_recall_with_filters() {
    let cli = parse_argv(&[
        "recall",
        "auth rewrite",
        "--top-k",
        "5",
        "--filter-context",
        "1",
        "--filter-context",
        "2",
        "--filter-kind",
        "episodic",
    ]);
    match cli.subcommand {
        Some(Command::Recall(a)) => {
            assert_eq!(a.query, "auth rewrite");
            assert_eq!(a.top_k, 5);
            assert_eq!(a.filter_context, vec![1u64, 2]);
            assert_eq!(a.filter_kind, vec![KindArg::Episodic]);
            assert!(!a.include_text);
        }
        other => panic!("expected Recall, got {other:?}"),
    }
}

#[test]
fn one_shot_recall_include_text_flag() {
    let cli = parse_argv(&["recall", "build breakage", "--top-k", "3", "--include-text"]);
    match cli.subcommand {
        Some(Command::Recall(a)) => {
            assert_eq!(a.query, "build breakage");
            assert_eq!(a.top_k, 3);
            assert!(a.include_text);
        }
        other => panic!("expected Recall, got {other:?}"),
    }
}

#[test]
fn one_shot_link_with_edge_kind() {
    let cli = parse_argv(&[
        "link",
        "0xdeadbeef",
        "supports",
        "0xcafef00d",
        "--weight",
        "0.5",
    ]);
    match cli.subcommand {
        Some(Command::Link(a)) => {
            assert_eq!(a.src.0.raw(), 0xdeadbeef_u128);
            assert_eq!(a.tgt.0.raw(), 0xcafef00d_u128);
            assert!((a.weight - 0.5).abs() < 1e-6);
        }
        other => panic!("expected Link, got {other:?}"),
    }
}

#[test]
fn one_shot_txn_subcommands() {
    let begin = parse_argv(&["txn", "begin"]);
    assert!(matches!(
        begin.subcommand,
        Some(Command::Txn(TxnCommand::Begin))
    ));

    let id = "00112233445566778899aabbccddeeff";
    let commit = parse_argv(&["txn", "commit", id]);
    match commit.subcommand {
        Some(Command::Txn(TxnCommand::Commit { id: got })) => assert_eq!(got, id),
        other => panic!("expected txn commit, got {other:?}"),
    }
}

#[test]
fn one_shot_forget_decimal_id() {
    let cli = parse_argv(&["forget", "999", "--mode", "hard"]);
    match cli.subcommand {
        Some(Command::Forget(a)) => {
            assert_eq!(a.id.0.raw(), 999u128);
        }
        other => panic!("expected Forget, got {other:?}"),
    }
}

#[test]
fn one_shot_subscribe_with_collect() {
    let cli = parse_argv(&["subscribe", "--collect", "5", "--context", "1"]);
    match cli.subcommand {
        Some(Command::Subscribe(a)) => {
            assert_eq!(a.collect, Some(5));
            assert_eq!(a.context, vec![1u64]);
        }
        other => panic!("expected Subscribe, got {other:?}"),
    }
}

#[test]
fn one_shot_subscribe_without_collect_is_streaming_mode() {
    // Bare `subscribe` must parse — collect is optional now (live
    // streaming is the default).
    let cli = parse_argv(&["subscribe"]);
    match cli.subcommand {
        Some(Command::Subscribe(a)) => {
            assert_eq!(a.collect, None);
            assert!(a.context.is_empty());
            assert!(a.kind.is_empty());
            assert_eq!(a.start_lsn, None);
        }
        other => panic!("expected Subscribe, got {other:?}"),
    }
}

#[test]
fn one_shot_subscribe_streaming_with_filter() {
    let cli = parse_argv(&["subscribe", "--context", "7", "--kind", "semantic"]);
    match cli.subcommand {
        Some(Command::Subscribe(a)) => {
            assert_eq!(a.collect, None);
            assert_eq!(a.context, vec![7u64]);
            assert_eq!(a.kind.len(), 1);
        }
        other => panic!("expected Subscribe, got {other:?}"),
    }
}

#[test]
fn global_output_flag_recognised() {
    let cli = parse_argv(&["--output", "json", "encode", "hi"]);
    assert_eq!(cli.global.output, Some(OutputFormatArg::Json));
}

#[test]
fn repl_line_tokenises_and_parses() {
    // `encode "hello world" --context 7` typed at the REPL.
    let toks = tokenize_line(r#"encode "hello world" --context 7"#).expect("tokenize");
    let mut argv = vec!["brain".to_string()];
    argv.extend(toks);
    let cli = Cli::try_parse_from(argv).expect("parse");
    match cli.subcommand {
        Some(Command::Encode(a)) => {
            assert_eq!(a.text, "hello world");
            assert_eq!(a.context, Some(7));
        }
        other => panic!("expected Encode, got {other:?}"),
    }
}

#[test]
fn round_trip_one_shot_and_repl_identical() {
    // Same conceptual command via two paths produces equivalent Cli values.
    let one_shot = parse_argv(&["recall", "x", "--top-k", "3"]);
    let toks = tokenize_line(r#"recall "x" --top-k 3"#).unwrap();
    let mut argv = vec!["brain".to_string()];
    argv.extend(toks);
    let repl_side = Cli::try_parse_from(argv).expect("parse");
    let (a, b) = match (one_shot.subcommand, repl_side.subcommand) {
        (Some(Command::Recall(a)), Some(Command::Recall(b))) => (a, b),
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(a.query, b.query);
    assert_eq!(a.top_k, b.top_k);
}

#[test]
fn tokenise_handles_quoted_with_spaces() {
    let toks = tokenize_line(r#"recall "two words" --top-k 5"#).unwrap();
    assert_eq!(toks, vec!["recall", "two words", "--top-k", "5"]);
}

#[test]
fn tokenise_escape_sequences() {
    let toks = tokenize_line(r#"encode "tab\there" --salience 0.5"#).unwrap();
    assert_eq!(toks, vec!["encode", "tab\there", "--salience", "0.5"]);
}

#[test]
fn parse_txn_id_round_trip() {
    let s = "0xdeadbeefcafef00ddeadbeefcafef00d";
    let bytes = parse_txn_id(s).expect("parse");
    assert_eq!(bytes[0], 0xde);
    assert_eq!(bytes[15], 0x0d);
}
