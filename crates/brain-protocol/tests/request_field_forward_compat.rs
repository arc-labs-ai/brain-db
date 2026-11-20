//! A mandatory key is one every peer must send, so adding one to an existing
//! request makes that operation undecodable for every client built before the
//! key existed. The ordinary way to add a request field is `#[serde(default)]`,
//! with `#[serde(skip_serializing_if = ...)]` when it is optional.
//!
//! The in-scope set is every type reachable from a request body, not every type
//! whose name ends in `Request`, so a view type is guarded from the moment a
//! request nests it. Today that is 92 types carrying 408 fields, of which 336
//! are mandatory keys.
//!
//! A tuple-style enum variant's payload struct is followed, but a struct-like
//! variant's inline fields are not, because they are not `pub` fields and no
//! declaration-level field scan sees them. `BackfillScope::MemoryRange {
//! start, end_inclusive }`, reached from `AdminBackfillRequest`, is the one
//! place in the crate where that matters.
//!
//! Response-direction types are not covered here at all.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Each row lists the keys a peer must send today for that operation. A row
/// grows only when someone argues that no older client can exist for the new
/// key. The ordinary way to add a request field is `#[serde(default)]` plus
/// `#[serde(skip_serializing_if = ...)]` when it is optional, not a new line
/// here.
const MANDATORY_KEYS: &[(&str, &[&str])] = &[
    ("ActAs", &["namespace", "space"]),
    ("AdminBackfillCancelRequest", &["backfill_id", "request_id"]),
    (
        "AdminBackfillRequest",
        &["scope", "extractor_ids", "dry_run", "request_id"],
    ),
    (
        "AdminCreateSessionRequest",
        &["name", "description", "request_id"],
    ),
    (
        "AdminIntegrityCheckRequest",
        &["scope", "repair_if_possible"],
    ),
    ("AdminListPendingContradictionsRequest", &["limit"]),
    (
        "AdminListTombstonedRequest",
        &["session_id", "max_age_seconds", "limit"],
    ),
    (
        "AdminMigrateEmbeddingsRequest",
        &["target_model", "batch_size", "rate_limit_qps"],
    ),
    ("AdminMoveMemoryRequest", &["memory_id", "new_session_id"]),
    ("AdminReclassifyRequest", &["memory_id", "new_kind"]),
    ("AdminRenameSessionRequest", &["session_id", "new_name"]),
    (
        "AdminRestoreRequest",
        &["snapshot_name", "target_shard", "request_id"],
    ),
    (
        "AdminSnapshotRequest",
        &["snapshot_name", "target_path", "include_wal", "request_id"],
    ),
    ("AdminStatsRequest", &["detail"]),
    ("AuthPayload", &["method", "credentials"]),
    ("ByeRequest", &["reason"]),
    ("CancelStreamRequest", &["target_stream_id", "reason"]),
    (
        "ClientPongRequest",
        &["server_timestamp_unix_nanos", "client_timestamp_unix_nanos"],
    ),
    ("EdgeRequest", &["target", "kind", "weight"]),
    (
        "EncodeRequest",
        &[
            "text",
            "session_id",
            "request_id",
            "txn_id",
            "occurred_at_unix_nanos",
        ],
    ),
    (
        "EncodeVectorDirectRequest",
        &[
            "text",
            "model_fingerprint",
            "session_id",
            "kind",
            "salience_hint",
            "edges",
            "request_id",
            "txn_id",
            "deduplicate",
        ],
    ),
    (
        "EntityCreateRequest",
        &[
            "entity_type_id",
            "canonical_name",
            "aliases",
            "attributes_blob",
            "request_id",
        ],
    ),
    ("EntityGetRequest", &["entity_id"]),
    (
        "EntityListRequest",
        &[
            "entity_type_id",
            "name_prefix",
            "mention_count_min",
            "include_tombstoned",
            "include_merged",
            "limit",
            "cursor",
        ],
    ),
    (
        "EntityMergeRequest",
        &["survivor", "merged", "confidence", "reason", "request_id"],
    ),
    (
        "EntityRenameRequest",
        &[
            "entity_id",
            "new_canonical_name",
            "move_to_alias",
            "request_id",
        ],
    ),
    (
        "EntityResolveRequest",
        &[
            "candidate_name",
            "resolution_context",
            "entity_type_hint",
            "allow_create",
            "request_id",
        ],
    ),
    (
        "EntityTombstoneRequest",
        &["entity_id", "reason", "request_id"],
    ),
    ("EntityTypeListRequest", &[]),
    ("EntityUnmergeRequest", &["merged_entity", "request_id"]),
    (
        "EntityUpdateRequest",
        &[
            "entity_id",
            "canonical_name",
            "aliases",
            "attributes_blob",
            "request_id",
        ],
    ),
    ("ExtractBackfillRequest", &["selector"]),
    ("ExtractorListRequest", &[]),
    (
        "ForgetRequest",
        &["memory_id", "mode", "request_id", "txn_id"],
    ),
    (
        "FusionConfigWire",
        &["k", "semantic_weight", "lexical_weight", "graph_weight"],
    ),
    ("GetCapabilitiesRequest", &[]),
    (
        "GraphFetchRequest",
        &[
            "limit",
            "cursor",
            "include_statements",
            "include_memories",
            "include_memory_edges",
            "include_tombstoned",
        ],
    ),
    (
        "HelloCapabilities",
        &["streaming", "compression_zstd", "server_push"],
    ),
    (
        "HelloPayload",
        &[
            "client_id",
            "supported_versions",
            "capabilities",
            "client_connection_token",
        ],
    ),
    (
        "LinkRequest",
        &["source", "target", "kind", "weight", "request_id", "txn_id"],
    ),
    ("LlmCredentialDeleteRequest", &["request_id"]),
    ("LlmCredentialGetRequest", &[]),
    (
        "LlmCredentialPutRequest",
        &["provider", "model", "api_key", "request_id"],
    ),
    ("LlmCredentialVerifyRequest", &["request_id"]),
    (
        "MaterializeProceduralRequest",
        &[
            "space_id",
            "session_filter",
            "top_k",
            "min_confidence",
            "categories",
            "request_id",
        ],
    ),
    ("MemoryInspectRequest", &["memory_id"]),
    (
        "MemoryListRequest",
        &[
            "sort",
            "dir",
            "limit",
            "cursor",
            "kinds",
            "include_tombstoned",
            "time_axis",
            "from_unix_nanos",
            "to_unix_nanos",
            "salience_min",
            "salience_max",
            "text_contains",
        ],
    ),
    ("ModelIdentifier", &["name", "fingerprint"]),
    ("MtlsClaim", &["cert_fingerprint", "asserted_subject"]),
    ("PingRequest", &["client_timestamp_unix_nanos"]),
    (
        "PlanBudget",
        &["max_steps", "max_wall_time_ms", "max_branches_explored"],
    ),
    (
        "PlanRequest",
        &[
            "start",
            "goal",
            "budget",
            "strategy_hint",
            "session_filter",
            "request_id",
            "txn_id",
        ],
    ),
    ("QueryExplainRequest", &["query"]),
    (
        "QueryRequest",
        &[
            "text",
            "entity_anchor",
            "kind_filter",
            "predicate_filter",
            "session_filter",
            "time_filter",
            "as_of_record_time_unix_nanos",
            "confidence_min",
            "include_tombstoned",
            "include_superseded",
            "limit",
            "retrievers",
            "fusion_config",
            "request_id",
        ],
    ),
    ("QueryTraceRequest", &["query"]),
    (
        "ReasonRequest",
        &[
            "observation",
            "depth",
            "confidence_threshold",
            "session_filter",
            "max_inferences",
            "budget_wall_time_ms",
            "request_id",
            "txn_id",
        ],
    ),
    (
        "RecallRequest",
        &[
            "cue_text",
            "subject_name",
            "max_results",
            "confidence_threshold",
            "session_filter",
            "age_bound_unix_nanos",
            "as_of_record_time_unix_nanos",
            "kind_filter",
            "salience_floor",
            "include_edges",
            "include_graph",
            "include_text",
            "request_id",
            "txn_id",
        ],
    ),
    (
        "RelationCreateRequest",
        &[
            "relation_type",
            "from_entity",
            "to_entity",
            "properties_blob",
            "evidence",
            "extractor_id",
            "confidence",
            "valid_from_unix_nanos",
            "valid_to_unix_nanos",
            "request_id",
        ],
    ),
    (
        "RelationGetRequest",
        &["relation_id", "follow_supersession"],
    ),
    (
        "RelationListFromRequest",
        &[
            "from_entity",
            "relation_type_filter",
            "time_range_start_unix_nanos",
            "time_range_end_unix_nanos",
            "include_superseded",
            "include_tombstoned",
            "limit",
            "cursor",
        ],
    ),
    (
        "RelationListToRequest",
        &[
            "to_entity",
            "relation_type_filter",
            "time_range_start_unix_nanos",
            "time_range_end_unix_nanos",
            "include_superseded",
            "include_tombstoned",
            "limit",
            "cursor",
        ],
    ),
    (
        "RelationSupersedeRequest",
        &["old_relation_id", "new_relation", "request_id"],
    ),
    (
        "RelationTombstoneRequest",
        &["relation_id", "reason", "reason_message", "request_id"],
    ),
    (
        "RelationTraverseRequest",
        &[
            "start_entity",
            "relation_types",
            "direction",
            "max_depth",
            "max_nodes",
            "time_at_unix_nanos",
            "include_superseded",
            "request_id",
        ],
    ),
    (
        "SchemaDropRequest",
        &[
            "namespace",
            "target_kind",
            "target_name",
            "force",
            "request_id",
            "scope",
        ],
    ),
    ("SchemaGetRequest", &["namespace", "version"]),
    ("SchemaListRequest", &["namespace", "limit", "cursor"]),
    (
        "SchemaReplaceRequest",
        &[
            "schema_document",
            "force_drop_existing",
            "request_id",
            "scope",
        ],
    ),
    (
        "SchemaUploadRequest",
        &[
            "schema_document",
            "dry_run",
            "allow_breaking",
            "request_id",
            "scope",
        ],
    ),
    ("SchemaValidateRequest", &["schema_document", "scope"]),
    ("SessionCreateRequest", &["session_id", "request_id"]),
    ("SessionDeleteRequest", &["session_id", "request_id"]),
    ("SessionListRequest", &["limit"]),
    ("SimilarityFilter", &["reference_memory_id", "threshold"]),
    ("SpaceCreateRequest", &["request_id"]),
    ("SpaceDeleteRequest", &["request_id"]),
    ("SpaceListRequest", &["limit"]),
    (
        "StatementCreateRequest",
        &[
            "kind",
            "subject",
            "predicate",
            "object",
            "confidence",
            "evidence",
            "extractor_id",
            "valid_from_unix_nanos",
            "valid_to_unix_nanos",
            "event_at_unix_nanos",
            "schema_version",
            "request_id",
        ],
    ),
    (
        "StatementGetRequest",
        &["statement_id", "follow_supersession"],
    ),
    (
        "StatementHistoryRequest",
        &["anchor_id", "include_tombstoned", "limit", "cursor"],
    ),
    (
        "StatementListRequest",
        &[
            "subject",
            "predicate",
            "kind",
            "min_confidence",
            "time_range_start_unix_nanos",
            "time_range_end_unix_nanos",
            "only_current",
            "include_tombstoned",
            "limit",
            "cursor",
        ],
    ),
    (
        "StatementRetractRequest",
        &["statement_id", "reason", "reason_message", "request_id"],
    ),
    (
        "StatementSupersedeRequest",
        &["old_statement_id", "new_statement", "request_id"],
    ),
    (
        "StatementTombstoneRequest",
        &["statement_id", "reason", "reason_message", "request_id"],
    ),
    (
        "SubscribeRequest",
        &["filter", "include_history", "from_lsn", "max_inflight"],
    ),
    (
        "SubscriptionFilter",
        &[
            "session_filter",
            "kinds",
            "similar_to",
            "spaces",
            "memory_ids",
        ],
    ),
    ("TimeRangeWire", &["from_unix_ms", "to_unix_ms"]),
    ("TxnAbortRequest", &["txn_id"]),
    ("TxnBeginRequest", &["txn_id", "timeout_seconds"]),
    ("TxnCommitRequest", &["txn_id"]),
    (
        "UnlinkRequest",
        &["source", "target", "kind", "request_id", "txn_id"],
    ),
    ("UnsubscribeRequest", &["target_stream_id"]),
];

/// `    pub <name>: <type>,` → `(name, type)`.
// Copied from `tests/id_shape_consistency.rs` so both tests parse declarations
// the same way.
fn pub_field(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("pub ")?;
    let (name, after) = rest.split_once(':')?;
    let name = name.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let ty = after.trim().strip_suffix(',')?;
    Some((name.to_string(), ty.to_string()))
}

// Copied from `tests/id_shape_consistency.rs` so both tests walk Rust files
// the same way.
fn rust_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Every wire field of one in-scope type: the key a peer sends, and whether it
/// carries `#[serde(default)]` and may therefore be omitted by a peer that
/// predates it.
type ParsedFields = BTreeMap<String, Vec<(String, bool)>>;

fn parsed_fields() -> ParsedFields {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut all_structs: BTreeMap<String, Vec<(String, String, bool)>> = BTreeMap::new();
    let mut all_enums: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in rust_files(&src) {
        let text = std::fs::read_to_string(&file).expect("read source");
        let mut current_struct = String::new();
        let mut current_enum = String::new();
        let mut annotations = Vec::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("pub struct ") {
                current_struct = rest
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("")
                    .to_string();
                current_enum.clear();
                all_structs.entry(current_struct.clone()).or_default();
                annotations.clear();
                continue;
            }
            if let Some(rest) = line.strip_prefix("pub enum ") {
                current_enum = rest
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("")
                    .to_string();
                current_struct.clear();
                all_enums.entry(current_enum.clone()).or_default();
                annotations.clear();
                continue;
            }
            if !current_enum.is_empty() && line.trim() == "}" {
                current_enum.clear();
                continue;
            }
            if !current_enum.is_empty() {
                let trimmed = line.trim();
                if let Some((variant, rest)) = trimmed.split_once('(') {
                    if let Some(types) = rest.strip_suffix("),") {
                        if !variant.is_empty()
                            && variant.chars().all(|c| c.is_alphanumeric() || c == '_')
                        {
                            all_enums
                                .entry(current_enum.clone())
                                .or_default()
                                .push(types.to_string());
                        }
                    }
                }
            }
            if line.trim_start().starts_with("#[") || line.trim_start().starts_with("//") {
                annotations.push(line.trim_start().to_string());
                continue;
            }
            if !current_struct.is_empty() {
                if let Some((field, ty)) = pub_field(line) {
                    let defaulted = annotations
                        .iter()
                        .any(|line| line.starts_with("#[serde(") && line.contains("default"));
                    all_structs
                        .entry(current_struct.clone())
                        .or_default()
                        .push((field, ty, defaulted));
                }
                annotations.clear();
                continue;
            }
            annotations.clear();
        }
    }

    let mut in_scope: BTreeSet<String> = all_structs
        .keys()
        .filter(|name| {
            name.ends_with("Request")
                || name.as_str() == "HelloPayload"
                || name.as_str() == "AuthPayload"
        })
        .cloned()
        .collect();
    let mut queue: Vec<String> = in_scope.iter().cloned().collect();
    let mut index = 0;
    while index < queue.len() {
        let name = &queue[index];
        if let Some(fields) = all_structs.get(name) {
            for (_, ty, _) in fields {
                for token in identifier_tokens(ty) {
                    if all_structs.contains_key(&token) && in_scope.insert(token.clone()) {
                        queue.push(token.clone());
                    }
                    if let Some(payloads) = all_enums.get(&token) {
                        for payload in payloads {
                            for payload_token in identifier_tokens(payload) {
                                if all_structs.contains_key(&payload_token)
                                    && in_scope.insert(payload_token.clone())
                                {
                                    queue.push(payload_token);
                                }
                            }
                        }
                    }
                }
            }
        }
        index += 1;
    }

    let mut fields: ParsedFields = BTreeMap::new();
    for name in in_scope {
        fields.insert(
            name.clone(),
            all_structs[&name]
                .iter()
                .map(|(field, _, defaulted)| (field.clone(), *defaulted))
                .collect(),
        );
    }
    fields
}

fn identifier_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            token.push(ch);
        } else if !token.is_empty() {
            if token
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            {
                tokens.push(std::mem::take(&mut token));
            } else {
                token.clear();
            }
        }
    }
    if !token.is_empty()
        && token
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        tokens.push(token);
    }
    tokens
}

#[test]
fn every_mandatory_request_key_is_in_the_ledger() {
    let fields = parsed_fields();
    let total_fields: usize = fields.values().map(Vec::len).sum();
    let mandatory_fields: usize = fields
        .values()
        .flat_map(|fields| fields.iter())
        .filter(|(_, defaulted)| !defaulted)
        .count();
    // The covered type floor also catches lost enum edges and field types that
    // would otherwise shrink the guarded surface without a ledger failure.
    assert!(
        fields.len() >= 88,
        "only {} in-scope structs parsed; the parser is broken, not the protocol",
        fields.len()
    );
    assert!(
        total_fields >= 380,
        "only {} request fields parsed; the parser is broken, not the protocol",
        total_fields
    );
    assert!(
        mandatory_fields >= 320,
        "only {} mandatory request fields found; the parser is broken, not the protocol",
        mandatory_fields
    );

    let ledger: BTreeMap<&str, BTreeSet<&str>> = MANDATORY_KEYS
        .iter()
        .map(|(name, fields)| (*name, fields.iter().copied().collect()))
        .collect();
    let mut failures = String::new();
    for name in fields.keys() {
        if !ledger.contains_key(name.as_str()) {
            failures.push_str(&format!("\n`{name}` has no ledger row; add its row."));
        }
    }
    for name in ledger.keys() {
        if !fields.contains_key(*name) {
            failures.push_str(&format!(
                "\n`{name}` has no struct in src/; remove its row."
            ));
        }
    }
    for (name, fields) in &fields {
        let Some(row) = ledger.get(name.as_str()) else {
            continue;
        };
        for (field, defaulted) in fields {
            if !defaulted && !row.contains(field.as_str()) {
                failures.push_str(&format!(
                    "\n`{name}.{field}` is mandatory but absent from the ledger; give it \
                     `#[serde(default)]`, or add the name with the argument for why \
                     no client predating it can exist."
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "request-field forward-compatibility ledger drifted:{failures}"
    );
}

#[test]
fn an_omittable_field_is_also_a_defaultable_one() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut omissions = Vec::new();
    let mut found = 0;
    for file in rust_files(&src) {
        let text = std::fs::read_to_string(&file).expect("read source");
        let mut current_struct = String::new();
        let mut annotations = Vec::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("pub struct ") {
                current_struct = rest
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("")
                    .to_string();
                annotations.clear();
                continue;
            }
            if line.trim_start().starts_with("#[") || line.trim_start().starts_with("//") {
                annotations.push(line.trim_start().to_string());
                continue;
            }
            if let Some((field, _)) = pub_field(line) {
                let skipped = annotations.iter().any(|line| {
                    line.starts_with("#[serde(") && line.contains("skip_serializing_if")
                });
                let defaulted = annotations
                    .iter()
                    .any(|line| line.starts_with("#[serde(") && line.contains("default"));
                if skipped {
                    found += 1;
                    if !defaulted {
                        omissions.push(format!("`{current_struct}.{field}`"));
                    }
                }
                annotations.clear();
                continue;
            }
            annotations.clear();
        }
    }
    assert!(
        found >= 80,
        "only {found} skip_serializing_if fields found; the parser is broken, not the protocol"
    );
    assert!(
        omissions.is_empty(),
        "omittable fields must also default when absent; add `#[serde(default)]` to: {}",
        omissions.join(", ")
    );
}
