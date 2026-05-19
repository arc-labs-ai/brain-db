//! Golden-ish test for the stacked entity card. Strips OSC 8 wrappers
//! so the assertion is stable regardless of whether hyperlinks were on
//! at render time.

use brain_shell::output::render::entity_card::{
    EntityCard, MemorySummary, RelationSummary, StatementSummary,
};
use brain_shell::output::Render;

fn strip_osc8(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&']') {
            for next in chars.by_ref() {
                if next == '\\' {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn sample() -> EntityCard {
    EntityCard {
        id: "ent_priya".into(),
        canonical_name: "Priya".into(),
        type_qname: "Person".into(),
        aliases: vec!["P.".into(), "Priya P.".into()],
        statements: vec![StatementSummary {
            id: "stmt_works_at".into(),
            kind: "Fact".into(),
            predicate: "works_at".into(),
            object: "Acme Corp".into(),
            confidence: 0.95,
        }],
        mentioned_in: vec![MemorySummary {
            short_id: "s2/m17/v1".into(),
            text: "Priya works at Acme Corp as a staff engineer".into(),
        }],
        relations_out: vec![RelationSummary {
            other_id: "ent_acme".into(),
            other_name: "Acme Corp".into(),
            predicate: "works_at".into(),
        }],
        relations_in: vec![],
    }
}

#[test]
fn entity_card_renders_header_and_sections() {
    let mut buf = Vec::new();
    sample().render_table(&mut buf).unwrap();
    let s = strip_osc8(&String::from_utf8(buf).unwrap());

    assert!(s.contains("Priya"), "header missing");
    assert!(s.contains("(Person)"), "type missing");
    assert!(s.contains("Aliases"), "aliases section missing");
    assert!(s.contains("P."));
    assert!(s.contains("Statements"), "statements section missing");
    assert!(s.contains("works_at"));
    assert!(s.contains("Mentioned in"), "mentions section missing");
    assert!(s.contains("s2/m17/v1"));
    assert!(s.contains("Relations (out)"), "relations section missing");
    assert!(s.contains("Acme Corp"));
}

#[test]
fn entity_card_omits_empty_sections() {
    let mut card = sample();
    card.aliases.clear();
    card.statements.clear();
    card.relations_out.clear();
    card.mentioned_in.clear();
    let mut buf = Vec::new();
    card.render_table(&mut buf).unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert!(!s.contains("Aliases"));
    assert!(!s.contains("Statements"));
    assert!(!s.contains("Mentioned in"));
    assert!(!s.contains("Relations (out)"));
}

#[test]
fn entity_card_json_carries_all_fields() {
    let v = sample().to_json_value();
    assert_eq!(v["canonical_name"], "Priya");
    assert_eq!(v["type"], "Person");
    assert_eq!(v["aliases"].as_array().unwrap().len(), 2);
    assert_eq!(v["statements"][0]["predicate"], "works_at");
    assert_eq!(v["mentioned_in"][0]["memory_id"], "s2/m17/v1");
    assert_eq!(v["relations_out"][0]["other_name"], "Acme Corp");
}
