//! Schema-management SDK surface. Spec §29/00 "Schema management",
//! phase 19.8.
//!
//! ```no_run
//! # use brain_sdk_rust::{Client, ClientError};
//! # async fn ex(client: Client) -> Result<(), ClientError> {
//! let outcome = client.schema()
//!     .upload_text(
//!         "namespace acme\n\
//!          define entity_type Person { attributes {} }\n",
//!     )
//!     .await?;
//! assert_eq!(outcome.namespace, "acme");
//! assert!(outcome.errors.is_empty());
//! # Ok(()) }
//! ```
//!
//! Builders for programmatic upload land alongside `upload_text` —
//! see [`SchemaBuilder`] and [`SchemaClient::upload`].

use brain_protocol::knowledge::{
    SchemaGetRequest, SchemaGetResponse, SchemaListItemWire, SchemaListRequest,
    SchemaListResponseFrame, SchemaUploadRequest, SchemaUploadResponse, SchemaValidateRequest,
    SchemaValidateResponse, SchemaValidationErrorWire,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::request::WireUuid;
use brain_protocol::schema::{
    AttrType, AttributeDecl, CacheConfig, CardinalityAst, ConditionExpr, ConditionOp,
    ConditionValue, CostUnit, DurationUnit, EntityTypeDef, ExtractorDef, ExtractorField,
    ExtractorKindAst, ExtractorTarget, LiteralValue, ObjectTypeDecl, PredicateDef, RelationTypeDef,
    Schema, SchemaItem, StatementKindAst, TriggerExpr,
};
use brain_protocol::{RequestBody, ResponseBody};

use crate::client::Client;
use crate::error::ClientError;

// ---------------------------------------------------------------------------
// Outcome / view types.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaUploadOutcome {
    pub namespace: String,
    /// `Some(v)` on success; `None` if the server rejected with a
    /// validation error list (also in [`Self::errors`]).
    pub schema_version: Option<u32>,
    pub errors: Vec<SchemaValidationIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaValidateOutcome {
    pub namespace: String,
    /// `current_active + 1` on success; `0` otherwise.
    pub would_be_version: u32,
    pub errors: Vec<SchemaValidationIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaValidationIssue {
    pub code: String,
    pub message: String,
    pub line: u32,
    pub column: u32,
    pub length: u32,
}

impl From<SchemaValidationErrorWire> for SchemaValidationIssue {
    fn from(w: SchemaValidationErrorWire) -> Self {
        Self {
            code: w.code,
            message: w.message,
            line: w.line,
            column: w.column,
            length: w.length,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaView {
    pub namespace: String,
    pub schema_version: u32,
    /// Verbatim DSL text, or `""` for programmatic uploads.
    pub schema_document: String,
    /// `serde_json::to_vec(&Schema)` of the parsed AST.
    pub source_blob: Vec<u8>,
    pub uploaded_at_unix_nanos: u64,
    pub validator_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaListView {
    pub namespace: String,
    pub items: Vec<SchemaListEntry>,
    pub total: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaListEntry {
    pub schema_version: u32,
    pub uploaded_at_unix_nanos: u64,
    pub validator_version: u32,
    pub has_source_text: bool,
}

impl From<SchemaListItemWire> for SchemaListEntry {
    fn from(w: SchemaListItemWire) -> Self {
        Self {
            schema_version: w.schema_version,
            uploaded_at_unix_nanos: w.uploaded_at_unix_nanos,
            validator_version: w.validator_version,
            has_source_text: w.has_source_text,
        }
    }
}

// ---------------------------------------------------------------------------
// SchemaClient.
// ---------------------------------------------------------------------------

pub struct SchemaClient<'c> {
    client: &'c Client,
}

impl<'c> SchemaClient<'c> {
    pub(crate) fn new(client: &'c Client) -> Self {
        Self { client }
    }

    /// Upload a programmatically-built schema. The AST is rendered
    /// to DSL text via [`print_schema`] and sent through the same
    /// wire path as [`Self::upload_text`].
    pub async fn upload(&self, schema: &Schema) -> Result<SchemaUploadOutcome, ClientError> {
        let text = print_schema(schema)?;
        self.upload_text(text).await
    }

    /// Upload raw DSL text per §21/01.
    pub async fn upload_text(
        &self,
        source: impl Into<String>,
    ) -> Result<SchemaUploadOutcome, ClientError> {
        let body = RequestBody::SchemaUpload(SchemaUploadRequest {
            schema_document: source.into(),
            dry_run: false,
            allow_breaking: false,
            request_id: WireUuid::default(),
        });
        let resp = send_schema(
            self.client,
            body,
            Opcode::SchemaUploadReq,
            Opcode::SchemaUploadResp,
        )
        .await?;
        let SchemaUploadResponse {
            namespace,
            schema_version,
            validation_errors,
            ..
        } = match resp {
            ResponseBody::SchemaUpload(r) => r,
            other => return Err(unexpected("SchemaUploadResp", other)),
        };
        let errors: Vec<_> = validation_errors.into_iter().map(Into::into).collect();
        let version = if schema_version == 0 {
            None
        } else {
            Some(schema_version)
        };
        Ok(SchemaUploadOutcome {
            namespace,
            schema_version: version,
            errors,
        })
    }

    /// Dry-run: parse + validate without persisting.
    pub async fn validate(
        &self,
        source: impl Into<String>,
    ) -> Result<SchemaValidateOutcome, ClientError> {
        let body = RequestBody::SchemaValidate(SchemaValidateRequest {
            schema_document: source.into(),
        });
        let resp = send_schema(
            self.client,
            body,
            Opcode::SchemaValidateReq,
            Opcode::SchemaValidateResp,
        )
        .await?;
        let SchemaValidateResponse {
            namespace,
            would_be_version,
            validation_errors,
        } = match resp {
            ResponseBody::SchemaValidate(r) => r,
            other => return Err(unexpected("SchemaValidateResp", other)),
        };
        Ok(SchemaValidateOutcome {
            namespace,
            would_be_version,
            errors: validation_errors.into_iter().map(Into::into).collect(),
        })
    }

    /// Fetch a specific version (`version == 0` → active).
    pub async fn get(
        &self,
        namespace: impl Into<String>,
        version: u32,
    ) -> Result<SchemaView, ClientError> {
        let body = RequestBody::SchemaGet(SchemaGetRequest {
            namespace: namespace.into(),
            version,
        });
        let resp = send_schema(
            self.client,
            body,
            Opcode::SchemaGetReq,
            Opcode::SchemaGetResp,
        )
        .await?;
        let SchemaGetResponse {
            namespace,
            schema_version,
            schema_document,
            source_blob,
            uploaded_at_unix_nanos,
            validator_version,
        } = match resp {
            ResponseBody::SchemaGet(r) => r,
            other => return Err(unexpected("SchemaGetResp", other)),
        };
        Ok(SchemaView {
            namespace,
            schema_version,
            schema_document,
            source_blob,
            uploaded_at_unix_nanos,
            validator_version,
        })
    }

    /// List all versions for a namespace, newest first.
    pub async fn list(
        &self,
        namespace: impl Into<String>,
    ) -> Result<SchemaListView, ClientError> {
        let body = RequestBody::SchemaList(SchemaListRequest {
            namespace: namespace.into(),
            limit: 0,
            cursor: Vec::new(),
        });
        let resp = send_schema(
            self.client,
            body,
            Opcode::SchemaListReq,
            Opcode::SchemaListResp,
        )
        .await?;
        let SchemaListResponseFrame {
            namespace,
            items,
            total,
            ..
        } = match resp {
            ResponseBody::SchemaList(r) => r,
            other => return Err(unexpected("SchemaListResp", other)),
        };
        Ok(SchemaListView {
            namespace,
            items: items.into_iter().map(Into::into).collect(),
            total,
        })
    }
}

impl Client {
    /// Entry-point for schema management. Spec §29/00 "Schema management".
    #[must_use]
    pub fn schema(&self) -> SchemaClient<'_> {
        SchemaClient::new(self)
    }
}

// ---------------------------------------------------------------------------
// SchemaBuilder — typed fluent assembler over Schema.
// ---------------------------------------------------------------------------

/// Fluent assembler over the 19.2 AST. Round-trips through DSL text
/// on `upload`.
#[derive(Debug, Clone)]
pub struct SchemaBuilder {
    schema: Schema,
}

impl SchemaBuilder {
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            schema: Schema {
                namespace: namespace.into(),
                items: Vec::new(),
                source: None,
            },
        }
    }

    #[must_use]
    pub fn entity_type(mut self, def: EntityTypeDef) -> Self {
        self.schema.items.push(SchemaItem::EntityType(def));
        self
    }

    #[must_use]
    pub fn predicate(mut self, def: PredicateDef) -> Self {
        self.schema.items.push(SchemaItem::Predicate(def));
        self
    }

    #[must_use]
    pub fn relation_type(mut self, def: RelationTypeDef) -> Self {
        self.schema.items.push(SchemaItem::RelationType(def));
        self
    }

    #[must_use]
    pub fn extractor(mut self, def: ExtractorDef) -> Self {
        self.schema.items.push(SchemaItem::Extractor(def));
        self
    }

    /// Escape hatch when the typed setters above don't fit.
    #[must_use]
    pub fn item(mut self, item: SchemaItem) -> Self {
        self.schema.items.push(item);
        self
    }

    pub fn build(self) -> Schema {
        self.schema
    }
}

// ---------------------------------------------------------------------------
// DSL printer. Renders a Schema AST back to text that re-parses to
// the same AST (modulo `Schema.source`).
// ---------------------------------------------------------------------------

/// Render `schema` to DSL text. Returns `ClientError::Internal` on
/// AST shapes the printer doesn't know how to emit — in v1 the
/// printer covers everything `SchemaBuilder` can construct.
pub fn print_schema(schema: &Schema) -> Result<String, ClientError> {
    let mut out = String::new();
    out.push_str(&format!("namespace {}\n", schema.namespace));
    for item in &schema.items {
        out.push('\n');
        match item {
            SchemaItem::EntityType(e) => print_entity_type(&mut out, e),
            SchemaItem::Predicate(p) => print_predicate(&mut out, p),
            SchemaItem::RelationType(r) => print_relation_type(&mut out, r),
            SchemaItem::Extractor(x) => print_extractor(&mut out, x)?,
        }
    }
    Ok(out)
}

fn print_entity_type(out: &mut String, e: &EntityTypeDef) {
    out.push_str(&format!("define entity_type {} {{\n", e.name));
    if !e.attributes.is_empty() {
        out.push_str("    attributes {\n");
        for attr in &e.attributes {
            out.push_str("        ");
            print_attribute(out, attr);
            out.push('\n');
        }
        out.push_str("    }\n");
    }
    out.push_str("}\n");
}

fn print_attribute(out: &mut String, attr: &AttributeDecl) {
    out.push_str(&format!("{}: ", attr.name));
    print_attr_type(out, &attr.attr_type);
    if attr.required {
        out.push_str(" required");
    } else {
        out.push_str(" optional");
    }
    if attr.unique {
        out.push_str(" unique");
    }
    if attr.indexed {
        out.push_str(" indexed");
    }
    if let Some(d) = &attr.default {
        out.push_str(" default ");
        print_literal(out, d);
    }
}

fn print_attr_type(out: &mut String, t: &AttrType) {
    match t {
        AttrType::Text => out.push_str("text"),
        AttrType::Number => out.push_str("number"),
        AttrType::Bool => out.push_str("bool"),
        AttrType::Date => out.push_str("date"),
        AttrType::Timestamp => out.push_str("timestamp"),
        AttrType::Enum { variants } => {
            out.push_str("enum [");
            out.push_str(&variants.join(", "));
            out.push(']');
        }
        AttrType::Ref { target } => {
            out.push_str(&format!("ref<{target}>"));
        }
    }
}

fn print_literal(out: &mut String, l: &LiteralValue) {
    match l {
        LiteralValue::Text(s) => out.push_str(&quote_string(s)),
        LiteralValue::Number(n) => out.push_str(&format_number(*n)),
        LiteralValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        LiteralValue::Date(s) => out.push_str(&quote_string(s)),
        LiteralValue::Timestamp(n) => out.push_str(&n.to_string()),
        LiteralValue::Null => out.push_str("\"\""),
    }
}

fn print_predicate(out: &mut String, p: &PredicateDef) {
    out.push_str(&format!("define predicate {} {{\n", p.name));
    out.push_str(&format!("    kind: {}\n", statement_kind_to_str(p.kind)));
    out.push_str("    object: ");
    print_object_type(out, &p.object);
    out.push('\n');
    if let Some(d) = &p.description {
        out.push_str(&format!("    description: {}\n", quote_string(d)));
    }
    out.push_str("}\n");
}

fn statement_kind_to_str(k: StatementKindAst) -> &'static str {
    match k {
        StatementKindAst::Fact => "Fact",
        StatementKindAst::Preference => "Preference",
        StatementKindAst::Event => "Event",
        StatementKindAst::Any => "Any",
    }
}

fn print_object_type(out: &mut String, o: &ObjectTypeDecl) {
    match o {
        ObjectTypeDecl::Value { value_type } => {
            out.push_str("Value<");
            print_attr_type(out, value_type);
            out.push('>');
        }
        ObjectTypeDecl::Entity { entity_type } => {
            out.push_str(&format!("Entity<{entity_type}>"));
        }
        ObjectTypeDecl::Memory => out.push_str("Memory"),
        ObjectTypeDecl::Statement => out.push_str("Statement"),
        ObjectTypeDecl::Any => out.push_str("Any"),
    }
}

fn print_relation_type(out: &mut String, r: &RelationTypeDef) {
    out.push_str(&format!("define relation_type {} {{\n", r.name));
    out.push_str(&format!("    from: {}\n", r.from_type));
    out.push_str(&format!("    to: {}\n", r.to_type));
    out.push_str(&format!(
        "    cardinality: {}\n",
        cardinality_to_str(r.cardinality)
    ));
    if r.symmetric {
        out.push_str("    symmetric: true\n");
    }
    if !r.properties.is_empty() {
        out.push_str("    properties {\n");
        for attr in &r.properties {
            out.push_str("        ");
            print_attribute(out, attr);
            out.push('\n');
        }
        out.push_str("    }\n");
    }
    if let Some(d) = &r.description {
        out.push_str(&format!("    description: {}\n", quote_string(d)));
    }
    out.push_str("}\n");
}

fn cardinality_to_str(c: CardinalityAst) -> &'static str {
    match c {
        CardinalityAst::OneToOne => "one-to-one",
        CardinalityAst::OneToMany => "one-to-many",
        CardinalityAst::ManyToOne => "many-to-one",
        CardinalityAst::ManyToMany => "many-to-many",
    }
}

fn print_extractor(out: &mut String, x: &ExtractorDef) -> Result<(), ClientError> {
    out.push_str(&format!("define extractor {} {{\n", x.name));
    out.push_str(&format!("    kind: {}\n", extractor_kind_to_str(x.kind)));
    out.push_str("    target: ");
    print_extractor_target(out, &x.target);
    out.push('\n');
    for f in &x.fields {
        print_extractor_field(out, f)?;
    }
    out.push_str("}\n");
    Ok(())
}

fn extractor_kind_to_str(k: ExtractorKindAst) -> &'static str {
    match k {
        ExtractorKindAst::Pattern => "pattern",
        ExtractorKindAst::Classifier => "classifier",
        ExtractorKindAst::Llm => "llm",
    }
}

fn print_extractor_target(out: &mut String, t: &ExtractorTarget) {
    match t {
        ExtractorTarget::Entity { entity_type } => {
            out.push_str(&format!("entity {entity_type}"));
        }
        ExtractorTarget::Statement { kind } => {
            out.push_str(&format!("statement {}", statement_kind_to_str(*kind)));
        }
        ExtractorTarget::Relation { relation_type } => {
            out.push_str(&format!("relation {relation_type}"));
        }
        ExtractorTarget::EntityOrStatement => out.push_str("entity_or_statement"),
    }
}

fn print_extractor_field(out: &mut String, f: &ExtractorField) -> Result<(), ClientError> {
    match f {
        ExtractorField::Patterns(p) => {
            out.push_str("    patterns [\n");
            for pat in p {
                out.push_str(&format!("        /{pat}/\n"));
            }
            out.push_str("    ]\n");
        }
        ExtractorField::Model(m) => {
            out.push_str(&format!("    model: {}\n", quote_string(m)));
        }
        ExtractorField::FeatureExtraction(s) => {
            out.push_str(&format!("    feature_extraction: {s}\n"));
        }
        ExtractorField::Prompt(p) => {
            out.push_str(&format!("    prompt: \"\"\"{p}\"\"\"\n"));
        }
        ExtractorField::Examples(v) => {
            let s = serde_json::to_string(v)
                .map_err(|e| ClientError::Internal(format!("examples encode: {e}")))?;
            out.push_str(&format!("    examples: {s}\n"));
        }
        ExtractorField::Schema(v) => {
            let s = serde_json::to_string(v)
                .map_err(|e| ClientError::Internal(format!("schema encode: {e}")))?;
            out.push_str(&format!("    schema: {s}\n"));
        }
        ExtractorField::Cache(c) => {
            out.push_str(&format!(
                "    cache: {}\n",
                match c {
                    CacheConfig::Enabled => "enabled",
                    CacheConfig::Disabled => "disabled",
                }
            ));
        }
        ExtractorField::CacheTtl(d) => {
            out.push_str(&format!(
                "    cache_ttl: {}{}\n",
                d.amount,
                duration_unit_suffix(d.unit)
            ));
        }
        ExtractorField::Confidence(c) => {
            out.push_str(&format!("    confidence: {}\n", format_number(*c as f64)));
        }
        ExtractorField::ConfidenceThreshold(c) => {
            out.push_str(&format!(
                "    confidence_threshold: {}\n",
                format_number(*c as f64)
            ));
        }
        ExtractorField::Trigger(t) => {
            out.push_str("    trigger: ");
            print_trigger(out, t);
            out.push('\n');
        }
        ExtractorField::CostBudget(c) => {
            out.push_str(&format!(
                "    cost_budget: ${} per {}\n",
                format_number(c.amount),
                cost_unit_str(c.unit)
            ));
        }
        ExtractorField::DependsOn(names) => {
            out.push_str(&format!("    depends_on: [{}]\n", names.join(", ")));
        }
        ExtractorField::Resolver(_) => {
            // Phase 22+ owns the resolver body shape; v1 emits an
            // empty block.
            out.push_str("    resolver { }\n");
        }
    }
    Ok(())
}

fn duration_unit_suffix(u: DurationUnit) -> &'static str {
    match u {
        DurationUnit::Seconds => "s",
        DurationUnit::Minutes => "m",
        DurationUnit::Hours => "h",
        DurationUnit::Days => "d",
    }
}

fn cost_unit_str(u: CostUnit) -> &'static str {
    match u {
        CostUnit::PerMemory => "memory",
        CostUnit::PerRequest => "request",
        CostUnit::PerDay => "day",
    }
}

fn print_trigger(out: &mut String, t: &TriggerExpr) {
    match t {
        TriggerExpr::OnEncode => out.push_str("on encode"),
        TriggerExpr::OnEncodeWhere(c) => {
            out.push_str("on encode where ");
            print_condition(out, c);
        }
        TriggerExpr::OnDemand => out.push_str("on demand"),
        TriggerExpr::OnSchemaChange => out.push_str("on schema_change"),
        TriggerExpr::Periodic { cron } => {
            out.push_str(&format!("periodic at {}", quote_string(cron)));
        }
    }
}

fn print_condition(out: &mut String, c: &ConditionExpr) {
    match c {
        ConditionExpr::Atom { field, op, value } => {
            out.push_str(&field.join("."));
            out.push(' ');
            out.push_str(condition_op_str(*op));
            out.push(' ');
            print_condition_value(out, value);
        }
        ConditionExpr::Matches { field, regex } => {
            out.push_str(&field.join("."));
            out.push_str(&format!(" matches /{regex}/"));
        }
        ConditionExpr::And(l, r) => {
            out.push('(');
            print_condition(out, l);
            out.push_str(") and (");
            print_condition(out, r);
            out.push(')');
        }
        ConditionExpr::Or(l, r) => {
            out.push('(');
            print_condition(out, l);
            out.push_str(") or (");
            print_condition(out, r);
            out.push(')');
        }
    }
}

fn condition_op_str(op: ConditionOp) -> &'static str {
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

fn print_condition_value(out: &mut String, v: &ConditionValue) {
    match v {
        ConditionValue::Text(s) => out.push_str(&quote_string(s)),
        ConditionValue::Number(n) => out.push_str(&format_number(*n)),
        ConditionValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        ConditionValue::List(items) => {
            out.push('[');
            let mut first = true;
            for v in items {
                if !first {
                    out.push_str(", ");
                }
                first = false;
                print_condition_value(out, v);
            }
            out.push(']');
        }
    }
}

fn quote_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}.0", n as i64)
    } else {
        format!("{n}")
    }
}

// ---------------------------------------------------------------------------
// Transport.
// ---------------------------------------------------------------------------

async fn send_schema(
    client: &Client,
    body: RequestBody,
    req_opcode: Opcode,
    resp_opcode: Opcode,
) -> Result<ResponseBody, ClientError> {
    client
        .send_knowledge_request(body, req_opcode, resp_opcode)
        .await
}

fn unexpected(expected: &str, body: ResponseBody) -> ClientError {
    ClientError::Protocol(brain_protocol::error::ProtocolError::BadFrame(format!(
        "expected {expected}, got {:?}",
        std::mem::discriminant(&body)
    )))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use brain_protocol::schema::{parse_schema, validate};

    fn person_entity() -> EntityTypeDef {
        EntityTypeDef {
            name: "Person".into(),
            attributes: vec![AttributeDecl {
                name: "email".into(),
                attr_type: AttrType::Text,
                required: false,
                unique: true,
                indexed: false,
                default: None,
            }],
        }
    }

    fn prefers_predicate() -> PredicateDef {
        PredicateDef {
            name: "prefers".into(),
            kind: StatementKindAst::Preference,
            object: ObjectTypeDecl::Value {
                value_type: AttrType::Text,
            },
            description: Some("user preference".into()),
        }
    }

    fn reports_to_relation() -> RelationTypeDef {
        RelationTypeDef {
            name: "reports_to".into(),
            from_type: "Person".into(),
            to_type: "Person".into(),
            cardinality: CardinalityAst::ManyToOne,
            symmetric: false,
            properties: vec![],
            description: None,
        }
    }

    #[test]
    fn builder_assembles_schema() {
        let s = SchemaBuilder::new("acme")
            .entity_type(person_entity())
            .predicate(prefers_predicate())
            .relation_type(reports_to_relation())
            .build();
        assert_eq!(s.namespace, "acme");
        assert_eq!(s.items.len(), 3);
    }

    #[test]
    fn print_round_trips_entity_type() {
        let s = SchemaBuilder::new("acme").entity_type(person_entity()).build();
        let text = print_schema(&s).unwrap();
        let parsed = parse_schema(&text).expect("printed text re-parses");
        let validated = validate(&parsed).expect("printed text validates");
        let again = validated.into_schema();
        assert_eq!(again.namespace, "acme");
        assert_eq!(again.items.len(), 1);
    }

    #[test]
    fn print_round_trips_predicate() {
        let s = SchemaBuilder::new("acme").predicate(prefers_predicate()).build();
        let text = print_schema(&s).unwrap();
        let parsed = parse_schema(&text).expect("printed text re-parses");
        let SchemaItem::Predicate(p) = &parsed.items[0] else {
            panic!("expected predicate");
        };
        assert_eq!(p.name, "prefers");
        assert_eq!(p.kind, StatementKindAst::Preference);
        assert_eq!(p.description.as_deref(), Some("user preference"));
    }

    #[test]
    fn print_round_trips_relation_type() {
        let s = SchemaBuilder::new("acme")
            .entity_type(person_entity())
            .relation_type(reports_to_relation())
            .build();
        let text = print_schema(&s).unwrap();
        let parsed = parse_schema(&text).expect("printed text re-parses");
        let validated = validate(&parsed).expect("printed text validates");
        let s = validated.into_schema();
        assert_eq!(s.items.len(), 2);
    }

    #[test]
    fn print_round_trips_pattern_extractor() {
        let ext = ExtractorDef {
            name: "person_mentions".into(),
            kind: ExtractorKindAst::Pattern,
            target: ExtractorTarget::Entity {
                entity_type: "Person".into(),
            },
            fields: vec![
                ExtractorField::Patterns(vec![r"\b[A-Z][a-z]+\b".into()]),
                ExtractorField::Confidence(0.7),
            ],
        };
        let s = SchemaBuilder::new("acme")
            .entity_type(person_entity())
            .extractor(ext)
            .build();
        let text = print_schema(&s).unwrap();
        let parsed = parse_schema(&text).expect("printed text re-parses");
        let _ = validate(&parsed).expect("printed text validates");
    }

    #[test]
    fn print_round_trips_full_schema() {
        let s = SchemaBuilder::new("acme")
            .entity_type(person_entity())
            .predicate(prefers_predicate())
            .relation_type(reports_to_relation())
            .build();
        let text = print_schema(&s).unwrap();
        let parsed = parse_schema(&text).expect("printed text re-parses");
        let validated = validate(&parsed).expect("printed text validates");
        let again = validated.into_schema();
        assert_eq!(again.namespace, "acme");
        assert_eq!(again.items.len(), 3);
    }

    #[test]
    fn validation_issue_from_wire_drops_severity() {
        let wire = SchemaValidationErrorWire {
            code: "X".into(),
            message: "y".into(),
            line: 1,
            column: 2,
            length: 3,
            severity: 2,
        };
        let issue: SchemaValidationIssue = wire.into();
        assert_eq!(issue.code, "X");
        assert_eq!(issue.line, 1);
        assert_eq!(issue.column, 2);
        assert_eq!(issue.length, 3);
    }
}
