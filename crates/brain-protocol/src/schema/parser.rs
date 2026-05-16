//! Pest-driven schema DSL parser (spec §21/01).
//!
//! Entry point: [`parse_schema`]. The parser consumes a single
//! schema document and produces the value-typed [`Schema`] from
//! [`super::ast`]. The original input text is preserved in
//! `Schema.source`.
//!
//! Pest 2.7. Grammar lives in `grammar.pest`.

use pest::Parser as _;
use pest::iterators::Pair;

use crate::schema::ast::{
    AttrType, AttributeDecl, CacheConfig, CardinalityAst, ConditionExpr, ConditionOp,
    ConditionValue, CostExpr, CostUnit, DurationAst, DurationUnit, EntityTypeDef, ExtractorDef,
    ExtractorField, ExtractorKindAst, ExtractorTarget, LiteralValue, ObjectTypeDecl, PredicateDef,
    RelationTypeDef, ResolverConfig, Schema, SchemaItem, StatementKindAst, TriggerExpr,
};
use crate::schema::parse_error::ParseError;

#[derive(pest_derive::Parser)]
#[grammar = "schema/grammar.pest"]
struct SchemaParser;

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

/// Parse a schema document into an AST. Source text is preserved in
/// `Schema.source`. Returns a structured [`ParseError`] with 1-based
/// line/col on failure.
pub fn parse_schema(input: &str) -> Result<Schema, ParseError> {
    let mut pairs = SchemaParser::parse(Rule::schema, input).map_err(map_pest_error)?;
    let schema_pair = pairs
        .next()
        .expect("Rule::schema always yields exactly one pair");

    let mut schema = Schema::default();
    schema.source = Some(input.to_string());

    for inner in schema_pair.into_inner() {
        match inner.as_rule() {
            Rule::namespace_decl => {
                schema.namespace = parse_namespace_decl(inner);
            }
            Rule::use_decl => {
                // §21/01 admits the token; v1 has no multi-document support
                // (§21/04 + §21/07 Q6) — accept-and-discard.
                let _ = inner;
            }
            Rule::entity_type_def => {
                schema
                    .items
                    .push(SchemaItem::EntityType(parse_entity_type_def(inner)?));
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
        description,
    })
}

fn parse_statement_kind(s: &str) -> StatementKindAst {
    match s {
        "Fact" => StatementKindAst::Fact,
        "Preference" => StatementKindAst::Preference,
        "Event" => StatementKindAst::Event,
        "Any" => StatementKindAst::Any,
        other => unreachable!("statement_kind produced {other:?}"),
    }
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
    // Default cardinality `many-to-many` per §21/02 if unspecified.
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
                    .find(|p| {
                        matches!(p.as_rule(), Rule::kw_builtin | Rule::identifier)
                    })
                    .expect("feature_extraction field has identifier|builtin");
                fields.push(ExtractorField::FeatureExtraction(token.as_str().to_string()));
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
                    parse_number_literal(n, lc)? as f32,
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
                // Body content is intentionally discarded — §22 will define
                // the schema. Phase 19 ships an empty placeholder.
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
    loop {
        let Some(op_pair) = iter.next() else { break };
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
        let has_patterns = e.fields.iter().any(|f| matches!(f, ExtractorField::Patterns(p) if !p.is_empty()));
        let has_conf = e.fields.iter().any(|f| matches!(f, ExtractorField::Confidence(_)));
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
        assert!(e.fields.iter().any(|f| matches!(f, ExtractorField::Prompt(p) if p.contains("Extract user preferences."))));
        assert!(e.fields.iter().any(|f| matches!(f, ExtractorField::Examples(_))));
        assert!(e.fields.iter().any(|f| matches!(f, ExtractorField::Schema(_))));
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
