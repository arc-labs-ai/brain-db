//! Render a [`Schema`] back to its DSL source.
//!
//! `SCHEMA_GET` hands back a document clients re-upload, and since
//! `SCHEMA_UPLOAD` became additive the authoritative schema is the merged AST
//! — no single uploaded document describes it. Echoing the last document would
//! return a fragment, so the response is rendered from the AST instead.
//!
//! The parser is the specification: the only thing that makes a rendering
//! correct is that it parses back to the schema it came from. Rather than
//! trust this module to be exhaustive, [`render_schema_verified`] re-parses
//! its own output and compares, so a construct the renderer gets wrong (or
//! that the grammar simply cannot express — a `Null` attribute default, a
//! regex containing a newline) yields `None` and lets the caller fall back
//! rather than hand out a document that does not reproduce the schema.

use super::ast::{
    AttrType, AttributeDecl, CacheConfig, CardinalityAst, ConditionExpr, ConditionOp,
    ConditionValue, CostExpr, CostUnit, DurationAst, DurationUnit, EntityTypeDef, ExtractorDef,
    ExtractorField, ExtractorKindAst, ExtractorTarget, KindCardinalityAst, KindDef, LiteralValue,
    ObjectKindAst, ObjectTypeDecl, PredicateDef, RelationTypeDef, Schema, SchemaItem,
    StatementKindAst, SubjectTypeDecl, TemporalModelAst,
};

/// Render `schema` to DSL, then prove the result parses back to it.
///
/// `None` means the schema holds something this renderer or the grammar
/// cannot express; the caller should fall back to whatever source text it
/// has rather than publish a lossy document.
#[must_use]
pub fn render_schema_verified(schema: &Schema) -> Option<String> {
    let text = render_schema(schema);
    let reparsed = super::parser::parse_schema(&text).ok()?;
    // `source` is the document text itself, which naturally differs; the
    // declarations are what must survive the round trip.
    (reparsed.namespace == schema.namespace && reparsed.items == schema.items).then_some(text)
}

/// Render `schema` to DSL, best-effort. Prefer [`render_schema_verified`].
#[must_use]
pub fn render_schema(schema: &Schema) -> String {
    let mut out = String::new();
    out.push_str("namespace ");
    out.push_str(&schema.namespace);
    out.push('\n');
    for item in &schema.items {
        out.push('\n');
        match item {
            SchemaItem::EntityType(e) => entity_type(&mut out, e),
            SchemaItem::Predicate(p) => predicate(&mut out, p),
            SchemaItem::RelationType(r) => relation_type(&mut out, r),
            SchemaItem::Extractor(x) => extractor(&mut out, x),
            SchemaItem::Kind(k) => kind(&mut out, k),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Items.
// ---------------------------------------------------------------------------

fn entity_type(out: &mut String, e: &EntityTypeDef) {
    out.push_str(&format!("define entity_type {} {{\n", e.name));
    out.push_str("    attributes {\n");
    for a in &e.attributes {
        out.push_str("        ");
        attribute(out, a);
        out.push('\n');
    }
    out.push_str("    }\n}\n");
}

fn predicate(out: &mut String, p: &PredicateDef) {
    out.push_str(&format!("define predicate {} {{\n", p.name));
    out.push_str(&format!("    kind: {}\n", statement_kind(p.kind)));
    if let Some(s) = &p.subject {
        out.push_str(&format!("    subject: {}\n", subject_type(s)));
    }
    out.push_str(&format!("    object: {}\n", object_type(&p.object)));
    if let Some(v) = p.stateful {
        out.push_str(&format!("    stateful: {v}\n"));
    }
    if let Some(d) = &p.description {
        out.push_str(&format!("    description: {}\n", string_lit(d)));
    }
    if let Some(r) = &p.retention {
        out.push_str(&format!("    retention: {}\n", duration(*r)));
    }
    out.push_str("}\n");
}

fn relation_type(out: &mut String, r: &RelationTypeDef) {
    out.push_str(&format!("define relation_type {} {{\n", r.name));
    out.push_str(&format!("    from: {}\n", r.from_type));
    out.push_str(&format!("    to: {}\n", r.to_type));
    out.push_str(&format!(
        "    cardinality: {}\n",
        cardinality(r.cardinality)
    ));
    if r.symmetric {
        out.push_str("    symmetric: true\n");
    }
    if !r.properties.is_empty() {
        out.push_str("    properties {\n");
        for a in &r.properties {
            out.push_str("        ");
            attribute(out, a);
            out.push('\n');
        }
        out.push_str("    }\n");
    }
    if let Some(d) = &r.description {
        out.push_str(&format!("    description: {}\n", string_lit(d)));
    }
    out.push_str("}\n");
}

fn kind(out: &mut String, k: &KindDef) {
    out.push_str(&format!("define kind {} {{\n", k.name));
    let card = match k.cardinality {
        KindCardinalityAst::Single => "single",
        KindCardinalityAst::Set => "set",
    };
    out.push_str(&format!("    cardinality: {card}\n"));
    let temporal = match k.temporal {
        TemporalModelAst::State => "state",
        TemporalModelAst::Event => "event",
        TemporalModelAst::None => "none",
    };
    out.push_str(&format!("    temporal: {temporal}\n"));
    if !k.object.is_empty() {
        let kinds: Vec<&str> = k
            .object
            .iter()
            .map(|o| match o {
                ObjectKindAst::Entity => "entity",
                ObjectKindAst::Value => "value",
                ObjectKindAst::Time => "time",
                ObjectKindAst::Quantity => "quantity",
                ObjectKindAst::List => "list",
            })
            .collect();
        out.push_str(&format!("    object: [{}]\n", kinds.join(", ")));
    }
    if k.polarity {
        out.push_str("    polarity: true\n");
    }
    if let Some(h) = &k.hint {
        out.push_str(&format!("    hint: {}\n", string_lit(h)));
    }
    out.push_str("}\n");
}

fn extractor(out: &mut String, x: &ExtractorDef) {
    out.push_str(&format!("define extractor {} {{\n", x.name));
    let k = match x.kind {
        ExtractorKindAst::Pattern => "pattern",
        ExtractorKindAst::Classifier => "classifier",
        ExtractorKindAst::Llm => "llm",
    };
    out.push_str(&format!("    kind: {k}\n"));
    out.push_str(&format!("    target: {}\n", target(&x.target)));
    for f in &x.fields {
        extractor_field(out, f);
    }
    out.push_str("}\n");
}

fn extractor_field(out: &mut String, f: &ExtractorField) {
    match f {
        ExtractorField::Patterns(ps) => {
            let body: Vec<String> = ps.iter().map(|p| regex_lit(p)).collect();
            out.push_str(&format!("    patterns [{}]\n", body.join(", ")));
        }
        ExtractorField::Model(m) => out.push_str(&format!("    model: {}\n", string_lit(m))),
        ExtractorField::FeatureExtraction(s) => {
            out.push_str(&format!("    feature_extraction: {s}\n"));
        }
        ExtractorField::Prompt(p) => {
            out.push_str(&format!("    prompt: {}\n", prompt_lit(p)));
        }
        ExtractorField::Examples(v) => {
            out.push_str(&format!("    examples: {v}\n"));
        }
        ExtractorField::Schema(v) => {
            out.push_str(&format!("    schema: {v}\n"));
        }
        ExtractorField::Cache(c) => {
            let v = match c {
                CacheConfig::Enabled => "enabled",
                CacheConfig::Disabled => "disabled",
            };
            out.push_str(&format!("    cache: {v}\n"));
        }
        ExtractorField::CacheTtl(d) => {
            out.push_str(&format!("    cache_ttl: {}\n", duration(*d)));
        }
        ExtractorField::Confidence(c) => {
            out.push_str(&format!("    confidence: {}\n", number_f32(*c)));
        }
        ExtractorField::ConfidenceThreshold(c) => {
            out.push_str(&format!("    confidence_threshold: {}\n", number_f32(*c)));
        }
        ExtractorField::Trigger(t) => {
            out.push_str(&format!("    trigger: {}\n", trigger(t)));
        }
        ExtractorField::CostBudget(c) => {
            out.push_str(&format!("    cost_budget: {}\n", cost(*c)));
        }
        ExtractorField::DependsOn(ds) => {
            out.push_str(&format!("    depends_on: [{}]\n", ds.join(", ")));
        }
        ExtractorField::Resolver(_) => out.push_str("    resolver { }\n"),
    }
}

// ---------------------------------------------------------------------------
// Fragments.
// ---------------------------------------------------------------------------

fn attribute(out: &mut String, a: &AttributeDecl) {
    out.push_str(&format!("{}: {}", a.name, attr_type(&a.attr_type)));
    // The parser defaults every modifier off, so only the set ones are
    // emitted; `optional` is the absence of `required`.
    if a.required {
        out.push_str(" required");
    }
    if a.unique {
        out.push_str(" unique");
    }
    if a.indexed {
        out.push_str(" indexed");
    }
    if let Some(d) = &a.default {
        out.push_str(&format!(" default {}", literal(d)));
    }
}

fn attr_type(t: &AttrType) -> String {
    match t {
        AttrType::Text => "text".into(),
        AttrType::Number => "number".into(),
        AttrType::Bool => "bool".into(),
        AttrType::Date => "date".into(),
        AttrType::Timestamp => "timestamp".into(),
        AttrType::Enum { variants } => format!("enum [{}]", variants.join(", ")),
        AttrType::Ref { target } => format!("ref<{target}>"),
    }
}

fn statement_kind(k: StatementKindAst) -> &'static str {
    match k {
        StatementKindAst::Fact => "Fact",
        StatementKindAst::Preference => "Preference",
        StatementKindAst::Event => "Event",
        StatementKindAst::Attribute => "Attribute",
        StatementKindAst::Relation => "Relation",
        StatementKindAst::Directive => "Directive",
        StatementKindAst::Any => "Any",
    }
}

fn subject_type(s: &SubjectTypeDecl) -> String {
    match s {
        SubjectTypeDecl::Entity { entity_type } => format!("Entity<{entity_type}>"),
        SubjectTypeDecl::Any => "Any".into(),
    }
}

fn object_type(o: &ObjectTypeDecl) -> String {
    match o {
        ObjectTypeDecl::Value { value_type } => format!("Value<{}>", attr_type(value_type)),
        ObjectTypeDecl::Entity { entity_type } => format!("Entity<{entity_type}>"),
        ObjectTypeDecl::Memory => "Memory".into(),
        ObjectTypeDecl::Statement => "Statement".into(),
        ObjectTypeDecl::Any => "Any".into(),
    }
}

fn cardinality(c: CardinalityAst) -> &'static str {
    match c {
        CardinalityAst::OneToOne => "one-to-one",
        CardinalityAst::OneToMany => "one-to-many",
        CardinalityAst::ManyToOne => "many-to-one",
        CardinalityAst::ManyToMany => "many-to-many",
    }
}

fn target(t: &ExtractorTarget) -> String {
    match t {
        ExtractorTarget::Entity { entity_type } => format!("entity {entity_type}"),
        ExtractorTarget::Statement { kind } => format!("statement {}", statement_kind(*kind)),
        ExtractorTarget::Relation { relation_type } => format!("relation {relation_type}"),
        ExtractorTarget::EntityOrStatement => "entity_or_statement".into(),
    }
}

fn trigger(t: &super::ast::TriggerExpr) -> String {
    use super::ast::TriggerExpr as T;
    match t {
        T::OnEncode => "on encode".into(),
        T::OnEncodeWhere(c) => format!("on encode where {}", condition(c)),
        T::OnDemand => "on demand".into(),
        T::OnSchemaChange => "on schema_change".into(),
        T::Periodic { cron } => format!("periodic at {}", string_lit(cron)),
    }
}

fn condition(c: &ConditionExpr) -> String {
    match c {
        ConditionExpr::Atom { field, op, value } => {
            format!("{} {} {}", field.join("."), cond_op(*op), cond_value(value))
        }
        ConditionExpr::Matches { field, regex } => {
            format!("{} matches {}", field.join("."), regex_lit(regex))
        }
        // The grammar is a flat `atom (binop atom)*`, so nesting is carried by
        // parentheses; wrap any compound operand to preserve the tree.
        ConditionExpr::And(l, r) => format!("{} and {}", operand(l), operand(r)),
        ConditionExpr::Or(l, r) => format!("{} or {}", operand(l), operand(r)),
    }
}

fn operand(c: &ConditionExpr) -> String {
    match c {
        ConditionExpr::And(..) | ConditionExpr::Or(..) => format!("({})", condition(c)),
        _ => condition(c),
    }
}

fn cond_op(op: ConditionOp) -> &'static str {
    match op {
        ConditionOp::Eq => "=",
        ConditionOp::Neq => "!=",
        ConditionOp::Lt => "<",
        ConditionOp::Lte => "<=",
        ConditionOp::Gt => ">",
        ConditionOp::Gte => ">=",
        ConditionOp::In => "in",
    }
}

fn cond_value(v: &ConditionValue) -> String {
    match v {
        ConditionValue::Text(s) => string_lit(s),
        ConditionValue::Number(n) => number(*n),
        ConditionValue::Bool(b) => b.to_string(),
        ConditionValue::List(vs) => {
            let body: Vec<String> = vs.iter().map(cond_value).collect();
            format!("[{}]", body.join(", "))
        }
    }
}

fn literal(l: &LiteralValue) -> String {
    match l {
        LiteralValue::Text(s) => string_lit(s),
        LiteralValue::Number(n) => number(*n),
        LiteralValue::Bool(b) => b.to_string(),
        // The grammar's `literal` is string / number / bool only, so these
        // three cannot round-trip. Rendered anyway; the verifier rejects them.
        LiteralValue::Date(s) => string_lit(s),
        LiteralValue::Timestamp(t) => t.to_string(),
        LiteralValue::Null => "null".into(),
    }
}

fn duration(d: DurationAst) -> String {
    let unit = match d.unit {
        DurationUnit::Seconds => "s",
        DurationUnit::Minutes => "m",
        DurationUnit::Hours => "h",
        DurationUnit::Days => "d",
    };
    format!("{}{unit}", d.amount)
}

fn cost(c: CostExpr) -> String {
    let unit = match c.unit {
        CostUnit::PerMemory => "memory",
        CostUnit::PerRequest => "request",
        CostUnit::PerDay => "day",
    };
    format!("${} per {unit}", number(c.amount))
}

/// Confidences are stored as `f32`; widening to `f64` first would print
/// `0.9f32` as `0.8999999761581421`, which is faithful but unreadable.
fn number_f32(v: f32) -> String {
    if !v.is_finite() {
        return "0".into();
    }
    let s = format!("{v}");
    if s.contains(['e', 'E']) {
        return format!("{v:.10}");
    }
    s
}

/// `number_literal` is `-?digits(.digits)?` — no exponent, no bare `.5`.
fn number(v: f64) -> String {
    if !v.is_finite() {
        // Unrepresentable; the verifier will reject the document.
        return "0".into();
    }
    let s = format!("{v}");
    if s.contains(['e', 'E']) {
        // Force positional notation.
        return format!("{v:.10}");
    }
    s
}

fn string_lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A prompt is usually multi-line, which a quoted literal handles badly; use a
/// heredoc unless the body itself contains the heredoc terminator.
fn prompt_lit(s: &str) -> String {
    if s.contains("\"\"\"") {
        string_lit(s)
    } else {
        format!("\"\"\"{s}\"\"\"")
    }
}

/// `regex_inner` keeps escape sequences verbatim, so the parsed string
/// already contains any `\/` the author wrote. Escaping those again would
/// emit `\\/`, which the grammar reads as an escaped backslash followed by
/// the closing delimiter — truncating the pattern. So copy an existing escape
/// pair through untouched and only escape a bare `/`.
fn regex_lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('/');
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(c);
            if let Some(next) = chars.next() {
                out.push(next);
            }
            continue;
        }
        if c == '/' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('/');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::parser::parse_schema;

    /// The only correctness criterion: the rendering parses back to the
    /// schema it came from.
    fn round_trips(src: &str) {
        let original = parse_schema(src).expect("fixture parses");
        let rendered = render_schema_verified(&original).unwrap_or_else(|| {
            panic!(
                "verified render failed for:\n{src}\ngot:\n{}",
                render_schema(&original)
            )
        });
        let reparsed = parse_schema(&rendered).expect("rendered output parses");
        assert_eq!(
            reparsed.items, original.items,
            "round trip changed the declarations.\nrendered:\n{rendered}"
        );
        assert_eq!(reparsed.namespace, original.namespace);
    }

    #[test]
    fn entity_types_with_every_attribute_shape() {
        round_trips(
            r#"
            namespace acme
            define entity_type Person {
                attributes {
                    email: text required unique
                    age: number
                    active: bool indexed
                    born: date
                    seen: timestamp
                    tier: enum [free, pro, team]
                    boss: ref<Person>
                    nick: text default "none"
                    score: number default 1.5
                    flag: bool default true
                }
            }
            define entity_type Empty { attributes {} }
            "#,
        );
    }

    #[test]
    fn predicates_with_every_field() {
        round_trips(
            r#"
            namespace acme
            define entity_type Person { attributes {} }
            define entity_type Org { attributes {} }
            define predicate works_at {
                kind: Fact
                subject: Entity<Person>
                object: Entity<Org>
                stateful: true
                description: "where they work"
                retention: 30d
            }
            define predicate note { kind: Event object: Value<text> }
            define predicate anything { kind: Any subject: Any object: Any }
            define predicate mem { kind: Fact object: Memory }
            define predicate stmt { kind: Fact object: Statement }
            define predicate status { kind: Fact object: Value<enum [a, b]> }
            "#,
        );
    }

    #[test]
    fn relation_types_with_properties_and_flags() {
        round_trips(
            r#"
            namespace acme
            define entity_type A { attributes {} }
            define relation_type links {
                from: A
                to: A
                cardinality: many-to-many
                symmetric: true
                properties {
                    weight: number
                    label: text unique
                }
                description: "a link"
            }
            define relation_type plain { from: A to: A cardinality: one-to-one }
            "#,
        );
    }

    #[test]
    fn kind_definitions() {
        round_trips(
            r#"
            namespace acme
            define kind Rating {
                cardinality: set
                temporal: event
                object: [entity, value, time, quantity, list]
                polarity: true
                hint: "a rating"
            }
            define kind Terse { cardinality: single temporal: none }
            "#,
        );
    }

    #[test]
    fn extractors_including_triggers_and_conditions() {
        round_trips(
            r#"
            namespace acme
            define entity_type Person { attributes {} }
            define extractor pat {
                kind: pattern
                target: entity Person
                patterns [/a+b/, /x\/y/]
                confidence: 0.9
                trigger: on encode
            }
            define extractor cls {
                kind: classifier
                target: entity_or_statement
                model: "gliner"
                feature_extraction: builtin
                confidence_threshold: 0.5
                trigger: on demand
                depends_on: [pat]
            }
            define extractor llm1 {
                kind: llm
                target: statement Fact
                model: "gpt-4o-mini"
                prompt: """extract things
                across lines"""
                cache: enabled
                cache_ttl: 12h
                cost_budget: $0.5 per memory
                trigger: on encode where entity.type = "Person" and age >= 18
            }
            define extractor llm2 {
                kind: llm
                target: entity Person
                model: "m"
                trigger: on schema_change
            }
            define extractor llm3 {
                kind: llm
                target: entity Person
                model: "m"
                trigger: periodic at "0 * * * *"
            }
            "#,
        );
    }

    #[test]
    fn nested_conditions_keep_their_shape() {
        round_trips(
            r#"
            namespace acme
            define entity_type P { attributes {} }
            define extractor e {
                kind: llm
                target: entity P
                model: "m"
                trigger: on encode where (a = 1 or b = 2) and c matches /x/
            }
            "#,
        );
    }

    /// Strings and regexes carrying the delimiters they are quoted with.
    #[test]
    fn escaping_survives() {
        round_trips(
            r#"
            namespace acme
            define entity_type P { attributes {} }
            define predicate p {
                kind: Fact
                object: Value<text>
                description: "he said \"hi\" and a backslash \\ too"
            }
            "#,
        );
    }

    /// A real production-shaped document, not a fixture written to suit the
    /// renderer.
    #[test]
    fn a_real_schema_round_trips() {
        round_trips(include_str!(
            "../../../../config/schemas/mirror.v3-enum-status.brainschema"
        ));
    }

    /// A schema the grammar cannot express must be refused, not mangled.
    #[test]
    fn an_unrepresentable_default_is_refused_rather_than_mangled() {
        let mut schema =
            parse_schema("namespace acme\ndefine entity_type P { attributes { a: text } }\n")
                .expect("parse");
        let SchemaItem::EntityType(e) = &mut schema.items[0] else {
            panic!("expected entity type");
        };
        // `literal` is string / number / bool — Null has no DSL spelling.
        e.attributes[0].default = Some(LiteralValue::Null);
        assert!(
            render_schema_verified(&schema).is_none(),
            "a schema that cannot be written back must be refused"
        );
    }
}
