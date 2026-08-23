//! Pest-driven schema DSL parser.
//!
//! Entry point: [`parse_schema`]. The parser consumes a single
//! schema document and produces the value-typed [`Schema`] from
//! [`super::ast`]. The original input text is preserved in
//! `Schema.source`.
//!
//! Pest 2.7. Grammar lives in `grammar.pest`.

use pest::iterators::Pair;
use pest::Parser as _;

use crate::schema::ast::{
    AttrType, AttributeDecl, CacheConfig, CardinalityAst, ConditionExpr, ConditionOp,
    ConditionValue, CostExpr, CostUnit, DurationAst, DurationUnit, EntityTypeDef, ExtractorDef,
    ExtractorField, ExtractorKindAst, ExtractorTarget, KindCardinalityAst, KindDef, LiteralValue,
    ObjectKindAst, ObjectTypeDecl, PredicateDef, RelationTypeDef, ResolverConfig, Schema,
    SchemaItem, StatementKindAst, TemporalModelAst, TriggerExpr,
};
use crate::schema::parse_error::ParseError;

#[derive(pest_derive::Parser)]
#[grammar = "schema/grammar.pest"]
struct SchemaParser;

/// Maximum bracket-nesting depth accepted in a schema document.
///
/// The grammar has mutually-recursive rules with no built-in bound —
/// `condition_paren` (`( ... )`) and the JSON capture rules
/// (`json_nested_object` / `json_nested_array`) each descend once per
/// bracket. pest parses by recursive descent, so a document with deeply
/// nested brackets recurses once per level and overflows the native
/// stack, aborting the whole process (all shards/tenants) — an
/// availability failure reachable from any low-privilege caller via
/// `SCHEMA_VALIDATE` / `SCHEMA_UPLOAD`.
///
/// A stack overflow cannot be caught, so it must be prevented before the
/// document reaches pest. 64 is far above anything a legitimate schema
/// needs — real `where` conditions nest a handful of parens deep and JSON
/// `schema:` bodies a dozen or so, while the enclosing `define … { }`
/// blocks never accumulate across items — yet far below the tens of
/// thousands of frames that would overflow the 8 MiB stack.
const MAX_NESTING_DEPTH: usize = 64;

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

/// Reject documents whose bracket nesting exceeds [`MAX_NESTING_DEPTH`]
/// before they reach the recursive-descent pest parser.
///
/// A cheap single-pass byte scan that tracks the current `(`/`[`/`{`
/// nesting depth. Brackets inside string literals, triple-quoted
/// heredocs, and line comments do not count — they are opaque to the
/// recursive grammar rules, so counting them would falsely reject
/// legitimate documents (e.g. a prompt heredoc containing many unbalanced
/// parens). Brackets inside a *closed* regex literal (`/.../` on a single
/// line) are likewise skipped, because pest parses the literal as one
/// atomic token. A lone `/` that does not close before the line ends is
/// treated as an ordinary byte so that JSON-body brackets following it
/// are still counted — see the `b'/'` arm. Byte scanning is sound because
/// every relevant delimiter is ASCII and UTF-8 continuation bytes never
/// collide with ASCII.
fn check_nesting_depth(input: &str) -> Result<(), ParseError> {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut line = 1usize;
    let mut col = 1usize;
    let mut depth = 0usize;

    while i < len {
        match bytes[i] {
            b'\n' => {
                line += 1;
                col = 1;
                i += 1;
            }
            b'#' => {
                // Line comment: skip to (but not past) the newline.
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' => {
                if i + 2 < len && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                    // Triple-quoted heredoc: opaque until the next `"""`.
                    i += 3;
                    col += 3;
                    while i < len {
                        if bytes[i] == b'"'
                            && i + 2 < len
                            && bytes[i + 1] == b'"'
                            && bytes[i + 2] == b'"'
                        {
                            i += 3;
                            col += 3;
                            break;
                        }
                        if bytes[i] == b'\n' {
                            line += 1;
                            col = 1;
                        } else {
                            col += 1;
                        }
                        i += 1;
                    }
                } else {
                    // Double-quoted string with backslash escapes.
                    i += 1;
                    col += 1;
                    while i < len {
                        match bytes[i] {
                            b'\\' => {
                                i += 2;
                                col += 2;
                            }
                            b'"' => {
                                i += 1;
                                col += 1;
                                break;
                            }
                            b'\n' => {
                                line += 1;
                                col = 1;
                                i += 1;
                            }
                            _ => {
                                col += 1;
                                i += 1;
                            }
                        }
                    }
                }
            }
            b'/' => {
                // A `/` may open a regex literal (`/.../`) in a trigger/where
                // clause, where pest parses the whole literal as one atomic
                // token and never recurses into its brackets — so its
                // contents are legitimately opaque to the depth cap. But `/`
                // is also an ordinary byte inside a JSON `schema:`/`examples:`
                // body, where the grammar *does* recurse once per `{`/`[`. We
                // may only leave a span uncounted when pest treats it
                // atomically — i.e. a genuine regex that closes with a
                // matching `/` before the line ends. If no closing `/` appears
                // before the next newline or EOF, this `/` is not a regex; the
                // brackets that follow are real nesting and must be counted, or
                // a payload like `schema: {/{{{...` would hide unbounded `{`
                // from the cap and overflow pest's recursive descent.
                //
                // The look-ahead never re-scans a span it consumes (a closed
                // regex is skipped whole; a non-regex `/` advances a single
                // byte and the main loop counts the rest), so the scan stays
                // linear overall.
                let mut j = i + 1;
                let mut closed = false;
                while j < len {
                    match bytes[j] {
                        b'\\' => j += 2,
                        b'/' => {
                            closed = true;
                            break;
                        }
                        b'\n' => break,
                        _ => j += 1,
                    }
                }
                if closed {
                    // Genuine regex literal — atomic to pest. Skip it whole;
                    // `j` indexes the closing `/`, and a closed regex cannot
                    // contain a newline, so column advance is the byte count.
                    col += j - i + 1;
                    i = j + 1;
                } else {
                    // Not a regex — treat `/` as an ordinary byte and let the
                    // main loop count any brackets that follow.
                    col += 1;
                    i += 1;
                }
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                if depth > MAX_NESTING_DEPTH {
                    return Err(ParseError::Syntax {
                        line,
                        col,
                        message: format!(
                            "bracket nesting depth exceeds maximum of {MAX_NESTING_DEPTH}"
                        ),
                    });
                }
                col += 1;
                i += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                col += 1;
                i += 1;
            }
            _ => {
                col += 1;
                i += 1;
            }
        }
    }

    Ok(())
}

/// Parse a schema document into an AST. Source text is preserved in
/// `Schema.source`. Returns a structured [`ParseError`] with 1-based
/// line/col on failure.
pub fn parse_schema(input: &str) -> Result<Schema, ParseError> {
    // Bound recursion before handing untrusted input to pest, which parses
    // by recursive descent and would otherwise overflow the stack on deeply
    // nested brackets.
    check_nesting_depth(input)?;

    let mut pairs = SchemaParser::parse(Rule::schema, input).map_err(map_pest_error)?;
    let schema_pair = pairs
        .next()
        .expect("Rule::schema always yields exactly one pair");

    let mut schema = Schema {
        source: Some(input.to_string()),
        ..Schema::default()
    };

    for inner in schema_pair.into_inner() {
        match inner.as_rule() {
            Rule::namespace_decl => {
                schema.namespace = parse_namespace_decl(inner);
            }
            Rule::use_decl => {
                // The grammar admits the token; v1 has no multi-document
                // support — accept-and-discard.
                let _ = inner;
            }
            Rule::entity_type_def => {
                schema
                    .items
                    .push(SchemaItem::EntityType(parse_entity_type_def(inner)?));
            }
            Rule::kind_def => {
                schema.items.push(SchemaItem::Kind(parse_kind_def(inner)?));
            }
            Rule::predicate_def => {
                schema
                    .items
                    .push(SchemaItem::Predicate(parse_predicate_def(inner)?));
            }
            Rule::relation_type_def => {
                schema
                    .items
                    .push(SchemaItem::RelationType(parse_relation_type_def(inner)?));
            }
            Rule::extractor_def => {
                schema
                    .items
                    .push(SchemaItem::Extractor(parse_extractor_def(inner)?));
            }
            Rule::EOI => {}
            other => unreachable!("unexpected top-level rule {other:?}"),
        }
    }

    Ok(schema)
}

// ---------------------------------------------------------------------------
// Top-level item parsers.
// ---------------------------------------------------------------------------

fn parse_namespace_decl(pair: Pair<'_, Rule>) -> String {
    let ident = pair
        .into_inner()
        .next()
        .expect("namespace_decl always has an identifier child");
    ident.as_str().to_string()
}

fn parse_entity_type_def(pair: Pair<'_, Rule>) -> Result<EntityTypeDef, ParseError> {
    let mut def = EntityTypeDef::default();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::identifier => def.name = inner.as_str().to_string(),
            Rule::attributes_block => {
                for attr in inner.into_inner() {
                    if attr.as_rule() == Rule::attribute_decl {
                        def.attributes.push(parse_attribute_decl(attr)?);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(def)
}

fn parse_attribute_decl(pair: Pair<'_, Rule>) -> Result<AttributeDecl, ParseError> {
    let line_col = pair.line_col();
    let mut name = String::new();
    let mut attr_type = AttrType::Text;
    let mut required = false;
    let mut unique = false;
    let mut indexed = false;
    let mut default: Option<LiteralValue> = None;

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::identifier => name = child.as_str().to_string(),
            Rule::attr_type => attr_type = parse_attr_type(child),
            Rule::modifier => {
                for m in child.into_inner() {
                    match m.as_rule() {
                        Rule::mod_required => required = true,
                        Rule::mod_optional => required = false,
                        Rule::mod_unique => unique = true,
                        Rule::mod_indexed => indexed = true,
                        Rule::mod_default => {
                            let lit_pair = m
                                .into_inner()
                                .find(|p| p.as_rule() == Rule::literal)
                                .ok_or_else(|| ParseError::MissingField {
                                    line: line_col.0,
                                    col: line_col.1,
                                    field: "default literal".into(),
                                })?;
                            default = Some(parse_literal(lit_pair)?);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    Ok(AttributeDecl {
        name,
        attr_type,
        required,
        unique,
        indexed,
        default,
    })
}

fn parse_attr_type(pair: Pair<'_, Rule>) -> AttrType {
    let inner = pair
        .into_inner()
        .next()
        .expect("attr_type always has exactly one child");
    match inner.as_rule() {
        Rule::attr_type_simple => match inner.as_str() {
            "text" => AttrType::Text,
            "number" => AttrType::Number,
            "bool" => AttrType::Bool,
            "date" => AttrType::Date,
            "timestamp" => AttrType::Timestamp,
            other => unreachable!("attr_type_simple matched {other:?}"),
        },
        Rule::attr_type_enum => {
            let variants = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::identifier_list)
                .map(parse_identifier_list)
                .unwrap_or_default();
            AttrType::Enum { variants }
        }
        Rule::attr_type_ref => {
            let target = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::identifier)
                .map(|p| p.as_str().to_string())
                .unwrap_or_default();
            AttrType::Ref { target }
        }
        other => unreachable!("attr_type produced {other:?}"),
    }
}

fn parse_identifier_list(pair: Pair<'_, Rule>) -> Vec<String> {
    pair.into_inner()
        .filter(|p| p.as_rule() == Rule::identifier)
        .map(|p| p.as_str().to_string())
        .collect()
}

fn parse_literal(pair: Pair<'_, Rule>) -> Result<LiteralValue, ParseError> {
    let line_col = pair.line_col();
    let inner = pair
        .into_inner()
        .next()
        .expect("literal always has one child");
    match inner.as_rule() {
        Rule::string_literal => Ok(LiteralValue::Text(unquote_string(inner))),
        Rule::number_literal => parse_number_literal(inner, line_col).map(LiteralValue::Number),
        Rule::bool_literal => Ok(LiteralValue::Bool(inner.as_str() == "true")),
        other => unreachable!("literal produced {other:?}"),
    }
}

fn parse_number_literal(pair: Pair<'_, Rule>, line_col: (usize, usize)) -> Result<f64, ParseError> {
    pair.as_str()
        .parse::<f64>()
        .map_err(|_| ParseError::InvalidNumber {
            line: line_col.0,
            col: line_col.1,
            value: pair.as_str().to_string(),
        })
}

// ---------------------------------------------------------------------------
// Predicate.
// ---------------------------------------------------------------------------

fn parse_predicate_def(pair: Pair<'_, Rule>) -> Result<PredicateDef, ParseError> {
    let line_col = pair.line_col();
    let mut name = String::new();
    let mut kind: Option<StatementKindAst> = None;
    let mut object: Option<ObjectTypeDecl> = None;
    let mut stateful: Option<bool> = None;
    let mut description: Option<String> = None;

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::identifier => name = child.as_str().to_string(),
            Rule::predicate_kind_field => {
                let kind_pair = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::statement_kind)
                    .expect("kind field always has statement_kind child");
                kind = Some(parse_statement_kind(kind_pair.as_str()));
            }
            Rule::predicate_object_field => {
                let obj_pair = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::object_type)
                    .expect("object field always has object_type child");
                object = Some(parse_object_type(obj_pair));
            }
            Rule::predicate_stateful_field => {
                let b = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::bool_literal)
                    .expect("stateful field always has bool_literal child");
                stateful = Some(b.as_str() == "true");
            }
            Rule::predicate_description_field => {
                let s = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::string_literal)
                    .expect("description field always has string_literal child");
                description = Some(unquote_string(s));
            }
            _ => {}
        }
    }

    let kind = kind.ok_or_else(|| ParseError::MissingField {
        line: line_col.0,
        col: line_col.1,
        field: "kind".into(),
    })?;
    let object = object.ok_or_else(|| ParseError::MissingField {
        line: line_col.0,
        col: line_col.1,
        field: "object".into(),
    })?;

    Ok(PredicateDef {
        name,
        kind,
        object,
        stateful,
        description,
    })
}

fn parse_statement_kind(s: &str) -> StatementKindAst {
    match s {
        "Fact" => StatementKindAst::Fact,
        "Preference" => StatementKindAst::Preference,
        "Event" => StatementKindAst::Event,
        "Attribute" => StatementKindAst::Attribute,
        "Relation" => StatementKindAst::Relation,
        "Directive" => StatementKindAst::Directive,
        "Any" => StatementKindAst::Any,
        other => unreachable!("statement_kind produced {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// User-declared kind.
// ---------------------------------------------------------------------------

fn parse_kind_def(pair: Pair<'_, Rule>) -> Result<KindDef, ParseError> {
    let line_col = pair.line_col();
    let mut name = String::new();
    let mut cardinality: Option<KindCardinalityAst> = None;
    let mut temporal: Option<TemporalModelAst> = None;
    let mut object: Vec<ObjectKindAst> = Vec::new();
    let mut polarity = false;
    let mut hint: Option<String> = None;

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::identifier => name = child.as_str().to_string(),
            Rule::kind_cardinality_field => {
                let p = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::kind_cardinality)
                    .expect("cardinality field always has a kind_cardinality child");
                cardinality = Some(match p.as_str() {
                    "single" => KindCardinalityAst::Single,
                    "set" => KindCardinalityAst::Set,
                    other => unreachable!("kind_cardinality produced {other:?}"),
                });
            }
            Rule::kind_temporal_field => {
                let p = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::temporal_model)
                    .expect("temporal field always has a temporal_model child");
                temporal = Some(match p.as_str() {
                    "state" => TemporalModelAst::State,
                    "event" => TemporalModelAst::Event,
                    "none" => TemporalModelAst::None,
                    other => unreachable!("temporal_model produced {other:?}"),
                });
            }
            Rule::kind_object_field => {
                for ok in child.into_inner() {
                    if ok.as_rule() == Rule::object_kind_list {
                        for k in ok.into_inner() {
                            if k.as_rule() == Rule::object_kind {
                                object.push(match k.as_str() {
                                    "entity" => ObjectKindAst::Entity,
                                    "value" => ObjectKindAst::Value,
                                    "time" => ObjectKindAst::Time,
                                    "quantity" => ObjectKindAst::Quantity,
                                    "list" => ObjectKindAst::List,
                                    other => unreachable!("object_kind produced {other:?}"),
                                });
                            }
                        }
                    }
                }
            }
            Rule::kind_polarity_field => {
                let b = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::bool_literal)
                    .expect("polarity field always has a bool_literal child");
                polarity = b.as_str() == "true";
            }
            Rule::kind_hint_field => {
                let s = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::string_literal)
                    .expect("hint field always has a string_literal child");
                hint = Some(unquote_string(s));
            }
            _ => {}
        }
    }

    let cardinality = cardinality.ok_or_else(|| ParseError::MissingField {
        line: line_col.0,
        col: line_col.1,
        field: "cardinality".into(),
    })?;
    let temporal = temporal.ok_or_else(|| ParseError::MissingField {
        line: line_col.0,
        col: line_col.1,
        field: "temporal".into(),
    })?;

    Ok(KindDef {
        name,
        cardinality,
        temporal,
        object,
        polarity,
        hint,
    })
}

fn parse_object_type(pair: Pair<'_, Rule>) -> ObjectTypeDecl {
    let inner = pair
        .into_inner()
        .next()
        .expect("object_type always has one child");
    match inner.as_rule() {
        Rule::object_type_value => {
            let value_type = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::attr_type)
                .map(parse_attr_type)
                .expect("Value<...> always carries an attr_type");
            ObjectTypeDecl::Value { value_type }
        }
        Rule::object_type_entity => {
            let entity_type = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::identifier)
                .map(|p| p.as_str().to_string())
                .expect("Entity<...> always carries an identifier");
            ObjectTypeDecl::Entity { entity_type }
        }
        Rule::object_type_memory => ObjectTypeDecl::Memory,
        Rule::object_type_statement => ObjectTypeDecl::Statement,
        Rule::object_type_any => ObjectTypeDecl::Any,
        other => unreachable!("object_type produced {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Relation type.
// ---------------------------------------------------------------------------

fn parse_relation_type_def(pair: Pair<'_, Rule>) -> Result<RelationTypeDef, ParseError> {
    let line_col = pair.line_col();
    let mut def = RelationTypeDef {
        name: String::new(),
        from_type: String::new(),
        to_type: String::new(),
        cardinality: CardinalityAst::ManyToMany,
        symmetric: false,
        properties: Vec::new(),
        description: None,
    };
    let mut saw_cardinality = false;
    let mut saw_from = false;
    let mut saw_to = false;

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::identifier => def.name = child.as_str().to_string(),
            Rule::relation_from_field => {
                let ident = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::identifier)
                    .expect("from field has identifier");
                def.from_type = ident.as_str().to_string();
                saw_from = true;
            }
            Rule::relation_to_field => {
                let ident = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::identifier)
                    .expect("to field has identifier");
                def.to_type = ident.as_str().to_string();
                saw_to = true;
            }
            Rule::relation_cardinality_field => {
                let card = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::cardinality)
                    .expect("cardinality field has cardinality");
                def.cardinality = parse_cardinality(card.as_str());
                saw_cardinality = true;
            }
            Rule::relation_symmetric_field => {
                let b = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::bool_literal)
                    .expect("symmetric field has bool_literal");
                def.symmetric = b.as_str() == "true";
            }
            Rule::relation_properties_block => {
                for attr in child.into_inner() {
                    if attr.as_rule() == Rule::attribute_decl {
                        def.properties.push(parse_attribute_decl(attr)?);
                    }
                }
            }
            Rule::relation_description_field => {
                let s = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::string_literal)
                    .expect("description field has string_literal");
                def.description = Some(unquote_string(s));
            }
            _ => {}
        }
    }

    if !saw_from {
        return Err(ParseError::MissingField {
            line: line_col.0,
            col: line_col.1,
            field: "from".into(),
        });
    }
    if !saw_to {
        return Err(ParseError::MissingField {
            line: line_col.0,
            col: line_col.1,
            field: "to".into(),
        });
    }
    // Default cardinality `many-to-many` if unspecified.
    let _ = saw_cardinality;

    Ok(def)
}

fn parse_cardinality(s: &str) -> CardinalityAst {
    match s {
        "one-to-one" => CardinalityAst::OneToOne,
        "one-to-many" => CardinalityAst::OneToMany,
        "many-to-one" => CardinalityAst::ManyToOne,
        "many-to-many" => CardinalityAst::ManyToMany,
        other => unreachable!("cardinality produced {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Extractor.
// ---------------------------------------------------------------------------

fn parse_extractor_def(pair: Pair<'_, Rule>) -> Result<ExtractorDef, ParseError> {
    let line_col = pair.line_col();
    let mut name = String::new();
    let mut kind: Option<ExtractorKindAst> = None;
    let mut target: Option<ExtractorTarget> = None;
    let mut fields: Vec<ExtractorField> = Vec::new();

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::identifier => name = child.as_str().to_string(),
            Rule::extractor_kind_field => {
                let k = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::extractor_kind)
                    .expect("kind field has extractor_kind");
                kind = Some(match k.as_str() {
                    "pattern" => ExtractorKindAst::Pattern,
                    "classifier" => ExtractorKindAst::Classifier,
                    "llm" => ExtractorKindAst::Llm,
                    other => unreachable!("extractor_kind {other:?}"),
                });
            }
            Rule::extractor_target_field => {
                let t = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::target_decl)
                    .expect("target field has target_decl");
                target = Some(parse_target_decl(t));
            }
            Rule::extractor_patterns_field => {
                let patterns: Vec<String> = child
                    .into_inner()
                    .filter(|p| p.as_rule() == Rule::regex_literal)
                    .map(extract_regex_inner)
                    .collect();
                fields.push(ExtractorField::Patterns(patterns));
            }
            Rule::extractor_model_field => {
                let s = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::string_literal)
                    .expect("model field has string_literal");
                fields.push(ExtractorField::Model(unquote_string(s)));
            }
            Rule::extractor_feature_extraction_field => {
                let token = child
                    .into_inner()
                    .find(|p| matches!(p.as_rule(), Rule::kw_builtin | Rule::identifier))
                    .expect("feature_extraction field has identifier|builtin");
                fields.push(ExtractorField::FeatureExtraction(
                    token.as_str().to_string(),
                ));
            }
            Rule::extractor_prompt_field => {
                let p = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::heredoc_or_string)
                    .expect("prompt field has heredoc_or_string");
                fields.push(ExtractorField::Prompt(parse_heredoc_or_string(p)));
            }
            Rule::extractor_examples_field => {
                let arr = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::json_array)
                    .expect("examples field has json_array");
                let value = parse_json(arr)?;
                fields.push(ExtractorField::Examples(value));
            }
            Rule::extractor_schema_field => {
                let obj = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::json_object)
                    .expect("schema field has json_object");
                let value = parse_json(obj)?;
                fields.push(ExtractorField::Schema(value));
            }
            Rule::extractor_cache_field => {
                let setting = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::cache_setting)
                    .expect("cache field has cache_setting");
                fields.push(ExtractorField::Cache(match setting.as_str() {
                    "enabled" => CacheConfig::Enabled,
                    "disabled" => CacheConfig::Disabled,
                    other => unreachable!("cache_setting {other:?}"),
                }));
            }
            Rule::extractor_cache_ttl_field => {
                let d = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::duration_literal)
                    .expect("cache_ttl field has duration_literal");
                fields.push(ExtractorField::CacheTtl(parse_duration_literal(d)?));
            }
            Rule::extractor_confidence_field => {
                let n = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::number_literal)
                    .expect("confidence field has number_literal");
                let lc = n.line_col();
                fields.push(ExtractorField::Confidence(
                    parse_number_literal(n, lc)? as f32
                ));
            }
            Rule::extractor_confidence_threshold_field => {
                let n = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::number_literal)
                    .expect("confidence_threshold field has number_literal");
                let lc = n.line_col();
                fields.push(ExtractorField::ConfidenceThreshold(
                    parse_number_literal(n, lc)? as f32,
                ));
            }
            Rule::extractor_trigger_field => {
                let t = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::trigger_expr)
                    .expect("trigger field has trigger_expr");
                fields.push(ExtractorField::Trigger(parse_trigger_expr(t)?));
            }
            Rule::extractor_cost_budget_field => {
                let c = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::cost_expr)
                    .expect("cost_budget field has cost_expr");
                fields.push(ExtractorField::CostBudget(parse_cost_expr(c)?));
            }
            Rule::extractor_depends_on_field => {
                let list = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::identifier_list)
                    .expect("depends_on field has identifier_list");
                fields.push(ExtractorField::DependsOn(parse_identifier_list(list)));
            }
            Rule::extractor_resolver_field => {
                // Body content is intentionally discarded — the resolver
                // schema isn't defined yet; v1 ships an empty placeholder.
                fields.push(ExtractorField::Resolver(ResolverConfig::default()));
            }
            _ => {}
        }
    }

    let kind = kind.ok_or_else(|| ParseError::MissingField {
        line: line_col.0,
        col: line_col.1,
        field: "kind".into(),
    })?;
    let target = target.ok_or_else(|| ParseError::MissingField {
        line: line_col.0,
        col: line_col.1,
        field: "target".into(),
    })?;

    Ok(ExtractorDef {
        name,
        kind,
        target,
        fields,
    })
}

fn parse_target_decl(pair: Pair<'_, Rule>) -> ExtractorTarget {
    let inner = pair
        .into_inner()
        .next()
        .expect("target_decl always has one child");
    match inner.as_rule() {
        Rule::target_entity => {
            let entity_type = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::identifier)
                .map(|p| p.as_str().to_string())
                .unwrap_or_default();
            ExtractorTarget::Entity { entity_type }
        }
        Rule::target_statement => {
            let kind = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::statement_kind)
                .map(|p| parse_statement_kind(p.as_str()))
                .expect("statement target carries statement_kind");
            ExtractorTarget::Statement { kind }
        }
        Rule::target_relation => {
            let relation_type = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::identifier)
                .map(|p| p.as_str().to_string())
                .unwrap_or_default();
            ExtractorTarget::Relation { relation_type }
        }
        Rule::target_entity_or_statement => ExtractorTarget::EntityOrStatement,
        other => unreachable!("target_decl produced {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Triggers + conditions.
// ---------------------------------------------------------------------------

fn parse_trigger_expr(pair: Pair<'_, Rule>) -> Result<TriggerExpr, ParseError> {
    let inner = pair
        .into_inner()
        .next()
        .expect("trigger_expr has exactly one child");
    match inner.as_rule() {
        Rule::trigger_on_encode => Ok(TriggerExpr::OnEncode),
        Rule::trigger_on_demand => Ok(TriggerExpr::OnDemand),
        Rule::trigger_on_schema_change => Ok(TriggerExpr::OnSchemaChange),
        Rule::trigger_on_encode_where => {
            let cond = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::condition_expr)
                .expect("on encode where carries a condition_expr");
            Ok(TriggerExpr::OnEncodeWhere(parse_condition_expr(cond)?))
        }
        Rule::trigger_periodic => {
            let s = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::string_literal)
                .expect("periodic carries string_literal");
            Ok(TriggerExpr::Periodic {
                cron: unquote_string(s),
            })
        }
        other => unreachable!("trigger_expr produced {other:?}"),
    }
}

fn parse_condition_expr(pair: Pair<'_, Rule>) -> Result<ConditionExpr, ParseError> {
    let mut iter = pair.into_inner();
    let first = iter
        .next()
        .expect("condition_expr always has at least one atom");
    let mut left = parse_condition_atom(first)?;
    while let Some(op_pair) = iter.next() {
        let op = op_pair.as_str();
        let right_pair = iter
            .next()
            .expect("condition_expr binary operator must be followed by an atom");
        let right = parse_condition_atom(right_pair)?;
        left = match op {
            "and" => ConditionExpr::And(Box::new(left), Box::new(right)),
            "or" => ConditionExpr::Or(Box::new(left), Box::new(right)),
            other => unreachable!("condition_binop produced {other:?}"),
        };
    }
    Ok(left)
}

fn parse_condition_atom(pair: Pair<'_, Rule>) -> Result<ConditionExpr, ParseError> {
    let inner = pair
        .into_inner()
        .next()
        .expect("condition_atom always has one child");
    match inner.as_rule() {
        Rule::condition_paren => {
            let expr = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::condition_expr)
                .expect("(expr) always wraps a condition_expr");
            parse_condition_expr(expr)
        }
        Rule::condition_matches => {
            let mut field: Vec<String> = Vec::new();
            let mut regex = String::new();
            for c in inner.into_inner() {
                match c.as_rule() {
                    Rule::field_ref => field = parse_field_ref(c),
                    Rule::regex_literal => regex = extract_regex_inner(c),
                    _ => {}
                }
            }
            Ok(ConditionExpr::Matches { field, regex })
        }
        Rule::condition_compare => {
            let mut field: Vec<String> = Vec::new();
            let mut op = ConditionOp::Eq;
            let mut value = ConditionValue::Bool(false);
            for c in inner.into_inner() {
                match c.as_rule() {
                    Rule::field_ref => field = parse_field_ref(c),
                    Rule::condition_op => op = parse_condition_op(c.as_str()),
                    Rule::condition_value => value = parse_condition_value(c)?,
                    _ => {}
                }
            }
            Ok(ConditionExpr::Atom { field, op, value })
        }
        other => unreachable!("condition_atom produced {other:?}"),
    }
}

fn parse_field_ref(pair: Pair<'_, Rule>) -> Vec<String> {
    pair.into_inner()
        .filter(|p| p.as_rule() == Rule::identifier)
        .map(|p| p.as_str().to_string())
        .collect()
}

fn parse_condition_op(s: &str) -> ConditionOp {
    match s {
        "=" => ConditionOp::Eq,
        "!=" => ConditionOp::Neq,
        "<" => ConditionOp::Lt,
        "<=" => ConditionOp::Lte,
        ">" => ConditionOp::Gt,
        ">=" => ConditionOp::Gte,
        "in" => ConditionOp::In,
        other => unreachable!("condition_op produced {other:?}"),
    }
}

fn parse_condition_value(pair: Pair<'_, Rule>) -> Result<ConditionValue, ParseError> {
    let inner = pair
        .into_inner()
        .next()
        .expect("condition_value always has one child");
    let line_col = inner.line_col();
    match inner.as_rule() {
        Rule::condition_value_list => {
            let mut items = Vec::new();
            for c in inner.into_inner() {
                if c.as_rule() == Rule::condition_value {
                    items.push(parse_condition_value(c)?);
                }
            }
            Ok(ConditionValue::List(items))
        }
        Rule::string_literal => Ok(ConditionValue::Text(unquote_string(inner))),
        Rule::number_literal => Ok(ConditionValue::Number(parse_number_literal(
            inner, line_col,
        )?)),
        Rule::bool_literal => Ok(ConditionValue::Bool(inner.as_str() == "true")),
        Rule::identifier => Ok(ConditionValue::Text(inner.as_str().to_string())),
        other => unreachable!("condition_value produced {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Literal helpers.
// ---------------------------------------------------------------------------

fn unquote_string(pair: Pair<'_, Rule>) -> String {
    // string_literal is `"` string_inner `"` — inner has the raw body
    // with escape sequences preserved. Decode the common ones.
    let inner = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::string_inner)
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    decode_escapes(&inner)
}

fn decode_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some('0') => out.push('\0'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn parse_heredoc_or_string(pair: Pair<'_, Rule>) -> String {
    let inner = pair
        .into_inner()
        .next()
        .expect("heredoc_or_string always has one child");
    match inner.as_rule() {
        Rule::heredoc_literal => inner
            .into_inner()
            .find(|p| p.as_rule() == Rule::heredoc_inner)
            .map(|p| p.as_str().to_string())
            .unwrap_or_default(),
        Rule::string_literal => unquote_string(inner),
        other => unreachable!("heredoc_or_string produced {other:?}"),
    }
}

fn extract_regex_inner(pair: Pair<'_, Rule>) -> String {
    pair.into_inner()
        .find(|p| p.as_rule() == Rule::regex_inner)
        .map(|p| p.as_str().to_string())
        .unwrap_or_default()
}

fn parse_duration_literal(pair: Pair<'_, Rule>) -> Result<DurationAst, ParseError> {
    let line_col = pair.line_col();
    let text = pair.as_str();
    let (digits, unit) = match text.chars().last() {
        Some(c) if matches!(c, 's' | 'm' | 'h' | 'd') => (&text[..text.len() - 1], c),
        _ => {
            return Err(ParseError::InvalidDuration {
                line: line_col.0,
                col: line_col.1,
                value: text.to_string(),
            });
        }
    };
    let amount = digits
        .parse::<u64>()
        .map_err(|_| ParseError::InvalidDuration {
            line: line_col.0,
            col: line_col.1,
            value: text.to_string(),
        })?;
    let unit = match unit {
        's' => DurationUnit::Seconds,
        'm' => DurationUnit::Minutes,
        'h' => DurationUnit::Hours,
        'd' => DurationUnit::Days,
        _ => unreachable!(),
    };
    Ok(DurationAst { amount, unit })
}

fn parse_cost_expr(pair: Pair<'_, Rule>) -> Result<CostExpr, ParseError> {
    let line_col = pair.line_col();
    let mut amount = 0.0_f64;
    let mut unit = CostUnit::PerMemory;
    for c in pair.into_inner() {
        match c.as_rule() {
            Rule::cost_amount => {
                amount = c
                    .as_str()
                    .parse::<f64>()
                    .map_err(|_| ParseError::InvalidCost {
                        line: line_col.0,
                        col: line_col.1,
                        message: format!("invalid amount {:?}", c.as_str()),
                    })?;
            }
            Rule::cost_unit => {
                unit = match c.as_str() {
                    "memory" => CostUnit::PerMemory,
                    "request" => CostUnit::PerRequest,
                    "day" => CostUnit::PerDay,
                    other => unreachable!("cost_unit {other:?}"),
                };
            }
            _ => {}
        }
    }
    Ok(CostExpr { amount, unit })
}

// ---------------------------------------------------------------------------
// JSON capture.
// ---------------------------------------------------------------------------

fn parse_json(pair: Pair<'_, Rule>) -> Result<serde_json::Value, ParseError> {
    let line_col = pair.line_col();
    let raw = pair.as_str();
    serde_json::from_str(raw).map_err(|e| ParseError::InvalidJson {
        line: line_col.0,
        col: line_col.1,
        message: e.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Pest error mapping.
// ---------------------------------------------------------------------------

fn map_pest_error(e: pest::error::Error<Rule>) -> ParseError {
    let (line, col) = match e.line_col {
        pest::error::LineColLocation::Pos((l, c)) => (l, c),
        pest::error::LineColLocation::Span((l, c), _) => (l, c),
    };
    ParseError::Syntax {
        line,
        col,
        message: e.variant.message().to_string(),
    }
}

// ---------------------------------------------------------------------------
// Unit tests for small grammar pieces.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(src: &str) -> Schema {
        parse_schema(src).expect("expected schema to parse")
    }

    #[test]
    fn empty_schema_with_namespace() {
        let s = parse_ok("namespace acme\n");
        assert_eq!(s.namespace, "acme");
        assert!(s.items.is_empty());
        assert!(s.source.is_some());
    }

    #[test]
    fn comment_only_is_ok() {
        let s = parse_ok("# this is a comment\nnamespace acme\n# trailing\n");
        assert_eq!(s.namespace, "acme");
    }

    #[test]
    fn attr_type_simple_variants() {
        let src = r#"
            namespace t
            define entity_type X {
                attributes {
                    a: text
                    b: number
                    c: bool
                    d: date
                    e: timestamp
                }
            }
        "#;
        let s = parse_ok(src);
        let SchemaItem::EntityType(e) = &s.items[0] else {
            panic!("expected EntityType")
        };
        assert_eq!(e.attributes.len(), 5);
        assert_eq!(e.attributes[0].attr_type, AttrType::Text);
        assert_eq!(e.attributes[1].attr_type, AttrType::Number);
        assert_eq!(e.attributes[2].attr_type, AttrType::Bool);
        assert_eq!(e.attributes[3].attr_type, AttrType::Date);
        assert_eq!(e.attributes[4].attr_type, AttrType::Timestamp);
    }

    #[test]
    fn attr_modifiers_compose() {
        let src = r#"
            namespace t
            define entity_type Person {
                attributes {
                    email: text required unique indexed
                    role: text optional
                    active: bool default true
                }
            }
        "#;
        let s = parse_ok(src);
        let SchemaItem::EntityType(e) = &s.items[0] else {
            panic!("entity type expected")
        };
        assert!(e.attributes[0].required);
        assert!(e.attributes[0].unique);
        assert!(e.attributes[0].indexed);
        assert!(!e.attributes[1].required);
        assert_eq!(e.attributes[2].default, Some(LiteralValue::Bool(true)));
    }

    #[test]
    fn predicate_def_round_trip() {
        let src = r#"
            namespace t
            define predicate prefers {
                kind: Preference
                object: Value<text>
                description: "user preference"
            }
        "#;
        let s = parse_ok(src);
        let SchemaItem::Predicate(p) = &s.items[0] else {
            panic!("predicate expected")
        };
        assert_eq!(p.name, "prefers");
        assert_eq!(p.kind, StatementKindAst::Preference);
        assert_eq!(
            p.object,
            ObjectTypeDecl::Value {
                value_type: AttrType::Text
            }
        );
        assert_eq!(p.description.as_deref(), Some("user preference"));
        // No explicit stateful keyword → AST carries None; resolution
        // happens at intern time. Preferences accumulate as a set (a person
        // can like many things), so the kind-derived default is NOT stateful.
        assert_eq!(p.stateful, None);
        assert!(!p.resolved_stateful());
    }

    #[test]
    fn kind_def_round_trip() {
        let src = r#"
            namespace t
            define kind investment {
                cardinality: set
                temporal: event
                object: [entity, quantity]
                polarity: false
                hint: "an entity funded another, with an amount"
            }
        "#;
        let s = parse_ok(src);
        let SchemaItem::Kind(k) = &s.items[0] else {
            panic!("kind expected")
        };
        assert_eq!(k.name, "investment");
        assert_eq!(k.cardinality, KindCardinalityAst::Set);
        assert_eq!(k.temporal, TemporalModelAst::Event);
        assert_eq!(
            k.object,
            vec![ObjectKindAst::Entity, ObjectKindAst::Quantity]
        );
        assert!(!k.polarity);
        assert_eq!(
            k.hint.as_deref(),
            Some("an entity funded another, with an amount")
        );
    }

    #[test]
    fn predicate_def_with_explicit_stateful_true() {
        let src = r#"
            namespace t
            define predicate works_at {
                kind: Fact
                object: Entity<Organization>
                stateful: true
            }
        "#;
        let s = parse_ok(src);
        let SchemaItem::Predicate(p) = &s.items[0] else {
            panic!("predicate expected")
        };
        assert_eq!(p.kind, StatementKindAst::Fact);
        assert_eq!(p.stateful, Some(true));
        assert!(
            p.resolved_stateful(),
            "explicit stateful: true overrides Fact default"
        );
    }

    #[test]
    fn predicate_def_with_explicit_stateful_false_on_preference() {
        // Preference's natural default is stateful: true; an author
        // may opt out to make it cumulative.
        let src = r#"
            namespace t
            define predicate liked_song {
                kind: Preference
                object: Value<text>
                stateful: false
            }
        "#;
        let s = parse_ok(src);
        let SchemaItem::Predicate(p) = &s.items[0] else {
            panic!("predicate expected")
        };
        assert_eq!(p.stateful, Some(false));
        assert!(!p.resolved_stateful());
    }

    #[test]
    fn predicate_def_fact_default_is_cumulative() {
        let src = r#"
            namespace t
            define predicate mentions {
                kind: Fact
                object: Entity<Person>
            }
        "#;
        let s = parse_ok(src);
        let SchemaItem::Predicate(p) = &s.items[0] else {
            panic!("predicate expected")
        };
        assert_eq!(p.stateful, None);
        assert!(!p.resolved_stateful(), "Fact defaults to cumulative");
    }

    #[test]
    fn relation_def_with_properties() {
        let src = r#"
            namespace t
            define relation_type owns {
                from: Person
                to: Project
                cardinality: many-to-many
                properties {
                    since: date optional
                }
            }
        "#;
        let s = parse_ok(src);
        let SchemaItem::RelationType(r) = &s.items[0] else {
            panic!("relation type expected")
        };
        assert_eq!(r.from_type, "Person");
        assert_eq!(r.to_type, "Project");
        assert_eq!(r.cardinality, CardinalityAst::ManyToMany);
        assert_eq!(r.properties.len(), 1);
        assert_eq!(r.properties[0].attr_type, AttrType::Date);
    }

    #[test]
    fn extractor_pattern_with_regex() {
        let src = r#"
            namespace t
            define extractor person_mentions {
                kind: pattern
                target: entity Person
                patterns [
                    /\b([A-Z][a-z]+)\b/
                ]
                confidence: 0.7
            }
        "#;
        let s = parse_ok(src);
        let SchemaItem::Extractor(e) = &s.items[0] else {
            panic!("extractor expected")
        };
        assert_eq!(e.kind, ExtractorKindAst::Pattern);
        assert!(matches!(
            e.target,
            ExtractorTarget::Entity { ref entity_type } if entity_type == "Person"
        ));
        // patterns and confidence preserved in source order.
        let has_patterns = e
            .fields
            .iter()
            .any(|f| matches!(f, ExtractorField::Patterns(p) if !p.is_empty()));
        let has_conf = e
            .fields
            .iter()
            .any(|f| matches!(f, ExtractorField::Confidence(_)));
        assert!(has_patterns);
        assert!(has_conf);
    }

    #[test]
    fn extractor_llm_heredoc_and_json() {
        let src = r#"
            namespace t
            define extractor preferences {
                kind: llm
                target: statement Preference
                prompt: """
                    Extract user preferences.
                """
                examples: [{"input": "x", "output": []}]
                schema: {"type": "object"}
                model: "claude-haiku-4-5"
                confidence_threshold: 0.8
                cache: enabled
                cache_ttl: 24h
                cost_budget: $0.10 per memory
            }
        "#;
        let s = parse_ok(src);
        let SchemaItem::Extractor(e) = &s.items[0] else {
            panic!("extractor expected")
        };
        assert_eq!(e.kind, ExtractorKindAst::Llm);
        assert!(matches!(
            e.target,
            ExtractorTarget::Statement {
                kind: StatementKindAst::Preference
            }
        ));
        assert!(e.fields.iter().any(
            |f| matches!(f, ExtractorField::Prompt(p) if p.contains("Extract user preferences."))
        ));
        assert!(e
            .fields
            .iter()
            .any(|f| matches!(f, ExtractorField::Examples(_))));
        assert!(e
            .fields
            .iter()
            .any(|f| matches!(f, ExtractorField::Schema(_))));
        assert!(e.fields.iter().any(|f| matches!(f, ExtractorField::CacheTtl(d) if d.amount == 24 && d.unit == DurationUnit::Hours)));
        assert!(e.fields.iter().any(|f| matches!(f, ExtractorField::CostBudget(c) if (c.amount - 0.10).abs() < 1e-9 && c.unit == CostUnit::PerMemory)));
    }

    #[test]
    fn condition_expr_compound() {
        let src = r#"
            namespace t
            define extractor x {
                kind: classifier
                target: relation reports_to
                trigger: on encode where memory.text matches /report.*to/ and confidence >= 0.5
                model: "m"
            }
        "#;
        let s = parse_ok(src);
        let SchemaItem::Extractor(e) = &s.items[0] else {
            panic!("extractor expected")
        };
        let trigger = e
            .fields
            .iter()
            .find_map(|f| match f {
                ExtractorField::Trigger(t) => Some(t),
                _ => None,
            })
            .expect("trigger present");
        let TriggerExpr::OnEncodeWhere(expr) = trigger else {
            panic!("expected OnEncodeWhere, got {trigger:?}")
        };
        match expr {
            ConditionExpr::And(left, right) => {
                assert!(matches!(**left, ConditionExpr::Matches { .. }));
                assert!(matches!(**right, ConditionExpr::Atom { .. }));
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn syntax_error_carries_line_col() {
        let err = parse_schema("namespace 123\n").unwrap_err();
        match err {
            ParseError::Syntax { line, col, .. } => {
                assert!(line >= 1);
                assert!(col >= 1);
            }
            other => panic!("expected Syntax error, got {other:?}"),
        }
    }

    #[test]
    fn pathological_nesting_is_rejected_not_crashed() {
        // A low-privilege client could send this via SCHEMA_VALIDATE. The
        // guard must return Err rather than letting pest recurse and abort
        // the process — if this test overflowed, the test runner would crash.
        let src = "(".repeat(100_000);
        let err = parse_schema(&src).unwrap_err();
        match err {
            ParseError::Syntax { message, .. } => {
                assert!(
                    message.contains("nesting depth"),
                    "expected depth error, got {message:?}"
                );
            }
            other => panic!("expected Syntax depth error, got {other:?}"),
        }
    }

    #[test]
    fn nesting_guard_boundary() {
        // At the limit the guard passes; one deeper is rejected.
        assert!(check_nesting_depth(&"(".repeat(MAX_NESTING_DEPTH)).is_ok());
        let err = check_nesting_depth(&"(".repeat(MAX_NESTING_DEPTH + 1)).unwrap_err();
        assert!(matches!(err, ParseError::Syntax { .. }));
    }

    #[test]
    fn nesting_guard_ignores_brackets_in_opaque_spans() {
        // Unbalanced brackets inside strings, heredocs, regex literals, and
        // comments are opaque to the recursive rules and must not accumulate
        // depth (else legitimate documents would be falsely rejected).
        let big = "(".repeat(1000);
        assert!(check_nesting_depth(&format!("\"{big}\"")).is_ok());
        assert!(check_nesting_depth(&format!("\"\"\"{big}\"\"\"")).is_ok());
        assert!(check_nesting_depth(&format!("/{big}/")).is_ok());
        assert!(check_nesting_depth(&format!("# {big}\n")).is_ok());
    }

    #[test]
    fn legit_nested_condition_still_parses() {
        // A few levels of real parens in a where-clause must parse fine.
        let src = r#"
            namespace t
            define extractor x {
                kind: classifier
                target: relation reports_to
                trigger: on encode where ((memory.text matches /a/) and (confidence >= 0.5))
                model: "m"
            }
        "#;
        let s = parse_ok(src);
        assert_eq!(s.items.len(), 1);
    }

    #[test]
    fn json_body_slash_prefixed_nesting_is_rejected() {
        // The `/{{{...` bypass: a lone `/` in JSON-value position must not
        // hide unbounded `{` from the depth cap. Before the fix the scanner
        // entered regex-skip on the `/` and consumed every brace to EOF,
        // returning Ok and letting pest recurse one frame per `{` until the
        // stack overflowed. A few hundred braces is enough to prove the guard
        // now rejects rather than accepts.
        let mut src = String::from("namespace t\ndefine extractor x { schema: {/");
        src.push_str(&"{".repeat(300));
        let err = parse_schema(&src).unwrap_err();
        match err {
            ParseError::Syntax { message, .. } => {
                assert!(
                    message.contains("nesting depth"),
                    "expected depth error, got {message:?}"
                );
            }
            other => panic!("expected Syntax depth error, got {other:?}"),
        }
    }

    #[test]
    fn slash_terminated_by_newline_still_counts_following_braces() {
        // A `/` that never closes before the newline is not a regex; braces
        // after it are real nesting and must count toward the cap.
        let mut src = String::from("/\n");
        src.push_str(&"(".repeat(MAX_NESTING_DEPTH + 1));
        let err = check_nesting_depth(&src).unwrap_err();
        assert!(matches!(err, ParseError::Syntax { .. }));
    }

    #[test]
    fn closed_regex_with_many_brackets_is_skipped() {
        // A genuine single-line regex literal is atomic to pest; its brackets
        // (even 1000 of them, closed by `/`) must not accumulate depth.
        let big = "{".repeat(1000);
        assert!(check_nesting_depth(&format!("/{big}/")).is_ok());
        let parens = "(".repeat(1000);
        assert!(check_nesting_depth(&format!("/{parens}/")).is_ok());
    }

    #[test]
    fn regex_with_braces_in_where_still_parses() {
        // A regex quantifier `{2,4}` in a trigger where-clause is a legit
        // literal and must not be false-rejected by the depth guard.
        let src = r#"
            namespace t
            define extractor x {
                kind: classifier
                target: relation reports_to
                trigger: on encode where memory.text matches /[A-Z]{2,4}/
                model: "m"
            }
        "#;
        let s = parse_ok(src);
        assert_eq!(s.items.len(), 1);
    }

    #[test]
    fn regex_pattern_with_literal_braces_parses() {
        // A pattern matching literal braces `/\{[a-z]+\}/` must parse — the
        // regex is atomic and its brackets do not count against the cap.
        let src = r#"
            namespace t
            define extractor x {
                kind: pattern
                target: entity Person
                patterns [ /\{[a-z]+\}/ ]
                confidence: 0.7
            }
        "#;
        let s = parse_ok(src);
        assert_eq!(s.items.len(), 1);
    }

    #[test]
    fn duration_units_round_trip() {
        for (raw, unit) in [
            ("10s", DurationUnit::Seconds),
            ("3m", DurationUnit::Minutes),
            ("24h", DurationUnit::Hours),
            ("7d", DurationUnit::Days),
        ] {
            let src = format!(
                "namespace t\ndefine extractor x {{ kind: llm target: statement Fact cache_ttl: {raw} }}\n"
            );
            let s = parse_ok(&src);
            let SchemaItem::Extractor(e) = &s.items[0] else {
                panic!("extractor expected for {raw}")
            };
            let d = e
                .fields
                .iter()
                .find_map(|f| match f {
                    ExtractorField::CacheTtl(d) => Some(d),
                    _ => None,
                })
                .expect("cache_ttl present");
            assert_eq!(d.unit, unit);
        }
    }
}
