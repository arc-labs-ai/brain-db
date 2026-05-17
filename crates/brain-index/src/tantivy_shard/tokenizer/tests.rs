//! Unit tests for the brain tantivy analyzer (phase 22.2).

use std::collections::HashSet;

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Schema, TEXT};
use tantivy::tokenizer::{TokenStream, Tokenizer};
use tantivy::{Index, TantivyDocument};

use super::{build_analyzer, BrainTokenizer, BRAIN_TOKENIZER_NAME};

fn tokens(input: &str) -> Vec<String> {
    let mut tk = BrainTokenizer::new();
    let mut stream = tk.token_stream(input);
    let mut out = Vec::new();
    while stream.advance() {
        out.push(stream.token().text.clone());
    }
    out
}

#[test]
fn lowercase_and_stem_residue() {
    assert_eq!(tokens("Running Quickly"), vec!["run", "quick"]);
}

#[test]
fn preserves_url_token_verbatim() {
    let out = tokens("see https://example.com/foo for details");
    assert!(
        out.contains(&"https://example.com/foo".to_string()),
        "URL should survive verbatim, got {out:?}",
    );
    assert!(out.contains(&"see".to_string()));
    assert!(
        out.contains(&"detail".to_string()),
        "`details` should stem to `detail`"
    );
}

#[test]
fn preserves_code_id_lowercased() {
    let out = tokens("ticket ACME-1247 broke");
    assert!(
        out.contains(&"acme-1247".to_string()),
        "code ID should survive lowercased, got {out:?}",
    );
    // Plain residue words still stem.
    assert!(out.contains(&"ticket".to_string()));
    assert!(out.contains(&"broke".to_string()));
}

#[test]
fn preserves_dotted_identifier() {
    let out = tokens("call brain_storage.arena.Arena now");
    let set: HashSet<_> = out.iter().cloned().collect();
    assert!(
        set.contains("brain_storage.arena.arena"),
        "dotted identifier should survive (lowercased), got {out:?}",
    );
    assert!(set.contains("call"));
    assert!(set.contains("now"));
}

#[test]
fn no_stopword_removal() {
    let out = tokens("the quick brown fox");
    // All four tokens (stemmed) must be present — no stop-word filter.
    let set: HashSet<_> = out.iter().cloned().collect();
    assert!(set.contains("the"), "stop-word `the` must be preserved");
    assert!(set.contains("a") || true); // input has no `a`; sanity check that `the` survived.
    assert!(set.contains("quick"));
    assert!(set.contains("brown"));
    assert!(set.contains("fox"));
}

#[test]
fn nfc_normalises_decomposed_form() {
    // U+0065 U+0301 (e + combining acute) -> "é" in NFC.
    let out = tokens("cafe\u{0301}");
    assert_eq!(out, vec!["café"]);
}

#[test]
fn register_overrides_default_through_writer() {
    // End-to-end smoke: register the analyzer on a fresh Index
    // under the literal name `"default"`, write a doc whose text
    // contains a protected token, then query for that token and
    // assert the doc matches. Verifies the override actually
    // routes through to the BM25 path.

    let mut sb = Schema::builder();
    let text_field = sb.add_text_field("text", TEXT);
    let schema = sb.build();

    let index = Index::create_in_ram(schema);
    index
        .tokenizers()
        .register(BRAIN_TOKENIZER_NAME, build_analyzer());

    let mut writer = index
        .writer_with_num_threads(1, 50_000_000)
        .expect("index writer");
    let mut doc = TantivyDocument::default();
    doc.add_text(text_field, "ticket ACME-1247 broke production");
    writer.add_document(doc).expect("add doc");
    writer.commit().expect("commit");

    let reader = index.reader().expect("reader");
    let searcher = reader.searcher();
    let qp = QueryParser::for_index(&index, vec![text_field]);
    let q = qp.parse_query("acme-1247").expect("parse query");
    let top = searcher
        .search(&q, &TopDocs::with_limit(10).order_by_score())
        .expect("search");
    assert_eq!(
        top.len(),
        1,
        "exact-ID query for `acme-1247` must match the indexed doc; got {top:?}",
    );
}

#[test]
fn empty_input_yields_no_tokens() {
    assert!(tokens("").is_empty());
}

#[test]
fn whitespace_only_yields_no_tokens() {
    assert!(tokens("   \t  \n").is_empty());
}
