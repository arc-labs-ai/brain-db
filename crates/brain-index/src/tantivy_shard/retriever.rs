//! LexicalRetriever — the read side of the tantivy pipeline.
//!
//! Consumers (retrieval query, RECALL paths) hold an
//! `Arc<dyn LexicalRetriever>` and call [`LexicalRetriever::retrieve`].
//! Per-shard wiring is the server's responsibility (see
//! `brain-server::shard::spawn`).

use std::ops::{Bound, RangeInclusive};
use std::sync::Arc;

use brain_core::StatementKind;
use brain_core::{AgentId, EntityId, MemoryId, MemoryKind, RelationId, StatementId};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, RangeQuery, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{DocAddress, IndexReader, Searcher, TantivyDocument, Term};
use thiserror::Error;

use super::{IndexHandle, LexicalScope, TantivyShard};

// ---------------------------------------------------------------------------
// Trait + value types.
// ---------------------------------------------------------------------------

/// The lexical-retrieval trait. Object-safe; consumers hold an
/// `Arc<dyn LexicalRetriever>`.
pub trait LexicalRetriever: Send + Sync {
    fn retrieve(
        &self,
        query: &LexicalQuery,
        scope: LexicalScope,
        config: &LexicalRetrieverConfig,
    ) -> Result<Vec<RankedItem>, LexicalError>;
}

#[derive(Debug, Clone, Default)]
pub struct LexicalQuery {
    /// Free-text terms — combined with OR semantics by tantivy's
    /// `QueryParser`; BM25 ranks by overall match.
    pub terms: Vec<String>,
    /// Each clause is an exact-adjacency phrase; AND-ed against
    /// the `terms` set.
    pub phrase_clauses: Vec<Vec<String>>,
    pub filters: LexicalFilters,
}

#[derive(Debug, Clone, Default)]
pub struct LexicalFilters {
    pub agent_ids: Vec<AgentId>,
    pub memory_kind: Option<MemoryKind>,
    pub statement_kind: Option<StatementKind>,
    pub predicate_id: Option<u32>,
    pub confidence_bucket: Option<RangeInclusive<u8>>,
    pub created_at_ms: Option<RangeInclusive<u64>>,
    pub extracted_at_ms: Option<RangeInclusive<u64>>,
    /// Front-gate scope tag for memory text. When non-empty, the
    /// boolean query adds a MUST clause matching any `context` in the
    /// list — BM25 ranks within that universe only.
    pub context_ids: Vec<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct LexicalRetrieverConfig {
    pub top_k: usize,
    pub bm25_k1: f32,
    pub bm25_b: f32,
    pub min_score: Option<f32>,
    pub timeout_ms: u32,
}

impl Default for LexicalRetrieverConfig {
    fn default() -> Self {
        Self {
            top_k: 64,
            bm25_k1: 1.2,
            bm25_b: 0.75,
            min_score: None,
            timeout_ms: 50,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RankedItem {
    pub id: RankedItemId,
    pub rank: u32,
    pub score: f32,
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RankedItemId {
    Memory(MemoryId),
    Statement(StatementId),
    /// Graph retrieval emits entities.
    Entity(EntityId),
    /// Graph retrieval emits relations.
    Relation(RelationId),
}

#[derive(Debug, Error)]
pub enum LexicalError {
    #[error("index unavailable (rebuild in progress or corrupt)")]
    IndexUnavailable,
    #[error("query parse failed: {0}")]
    QueryParseFailed(String),
    #[error("query timed out after {0} ms")]
    Timeout(u32),
    #[error("internal: {0}")]
    Internal(String),
}

// ---------------------------------------------------------------------------
// TantivyLexicalRetriever — production impl.
// ---------------------------------------------------------------------------

/// Production `LexicalRetriever` impl. Holds an `Arc<TantivyShard>`
/// plus cached `IndexReader` per scope; readers auto-refresh on
/// commit per tantivy's default `ReloadPolicy::OnCommit`.
pub struct TantivyLexicalRetriever {
    shard: Arc<TantivyShard>,
    memory_reader: IndexReader,
    statements_reader: IndexReader,
}

impl TantivyLexicalRetriever {
    pub fn new(shard: Arc<TantivyShard>) -> Result<Self, LexicalError> {
        let memory_reader = shard
            .memory_text
            .index
            .reader()
            .map_err(|e| LexicalError::Internal(format!("memory reader: {e}")))?;
        let statements_reader = shard
            .statements
            .index
            .reader()
            .map_err(|e| LexicalError::Internal(format!("statements reader: {e}")))?;
        Ok(Self {
            shard,
            memory_reader,
            statements_reader,
        })
    }
}

impl LexicalRetriever for TantivyLexicalRetriever {
    fn retrieve(
        &self,
        query: &LexicalQuery,
        scope: LexicalScope,
        config: &LexicalRetrieverConfig,
    ) -> Result<Vec<RankedItem>, LexicalError> {
        validate_filters_for_scope(&query.filters, scope)?;

        let (handle, reader) = match scope {
            LexicalScope::MemoryText => (&self.shard.memory_text, &self.memory_reader),
            LexicalScope::StatementText => (&self.shard.statements, &self.statements_reader),
        };
        // Tantivy's default `ReloadPolicy::OnCommitWithDelay` may
        // lag behind the writer's commits by up to ~50 ms. We
        // call `reload()` synchronously so callers see a
        // consistent view of all committed writes (the idempotency
        // contract: identical results between commits).
        reader
            .reload()
            .map_err(|e| LexicalError::Internal(format!("reader reload: {e}")))?;
        let searcher = reader.searcher();
        let q = build_query(query, handle, scope)?;
        let collector = TopDocs::with_limit(config.top_k.max(1)).order_by_score();

        let hits = searcher
            .search(&q, &collector)
            .map_err(|e| LexicalError::Internal(format!("search: {e}")))?;

        project(hits, &searcher, handle, scope, config)
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn validate_filters_for_scope(
    filters: &LexicalFilters,
    scope: LexicalScope,
) -> Result<(), LexicalError> {
    match scope {
        LexicalScope::MemoryText => {
            if filters.statement_kind.is_some() {
                return Err(LexicalError::QueryParseFailed(
                    "statement_kind filter applies only to StatementText".into(),
                ));
            }
            if filters.predicate_id.is_some() {
                return Err(LexicalError::QueryParseFailed(
                    "predicate_id filter applies only to StatementText".into(),
                ));
            }
            if filters.confidence_bucket.is_some() {
                return Err(LexicalError::QueryParseFailed(
                    "confidence_bucket filter applies only to StatementText".into(),
                ));
            }
            if filters.extracted_at_ms.is_some() {
                return Err(LexicalError::QueryParseFailed(
                    "extracted_at_ms filter applies only to StatementText".into(),
                ));
            }
        }
        LexicalScope::StatementText => {
            if !filters.agent_ids.is_empty() {
                return Err(LexicalError::QueryParseFailed(
                    "agent_id filter applies only to MemoryText scope".into(),
                ));
            }
            if filters.memory_kind.is_some() {
                return Err(LexicalError::QueryParseFailed(
                    "memory_kind filter applies only to MemoryText".into(),
                ));
            }
            if filters.created_at_ms.is_some() {
                return Err(LexicalError::QueryParseFailed(
                    "created_at_ms filter applies only to MemoryText".into(),
                ));
            }
        }
    }
    Ok(())
}

fn build_query(
    query: &LexicalQuery,
    handle: &IndexHandle,
    scope: LexicalScope,
) -> Result<Box<dyn Query>, LexicalError> {
    let schema = handle.index.schema();
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    // ----- Text part (terms + phrases) --------------------------------------
    let text_fields = match scope {
        LexicalScope::MemoryText => vec![schema
            .get_field("text")
            .map_err(|e| LexicalError::Internal(format!("text field: {e}")))?],
        LexicalScope::StatementText => vec![
            schema
                .get_field("subject_name")
                .map_err(|e| LexicalError::Internal(format!("subject_name: {e}")))?,
            schema
                .get_field("object_text")
                .map_err(|e| LexicalError::Internal(format!("object_text: {e}")))?,
        ],
    };

    let qp = QueryParser::for_index(&handle.index, text_fields);
    let text_input = compose_text_input(&query.terms, &query.phrase_clauses);
    if !text_input.is_empty() {
        let parsed = qp
            .parse_query(&text_input)
            .map_err(|e| LexicalError::QueryParseFailed(e.to_string()))?;
        clauses.push((Occur::Must, parsed));
    }

    // ----- Filters ----------------------------------------------------------
    let f = &query.filters;
    match scope {
        LexicalScope::MemoryText => {
            if !f.agent_ids.is_empty() {
                let field = schema
                    .get_field("agent_id")
                    .map_err(|e| LexicalError::Internal(format!("agent_id field: {e}")))?;
                // `agent_id IN [..]` = OR-group of TermQuery, wrapped as
                // a single MUST so the BM25 scoring stays inside the
                // requested agent universe.
                let inner: Vec<(Occur, Box<dyn tantivy::query::Query>)> = f
                    .agent_ids
                    .iter()
                    .map(|agent| -> (Occur, Box<dyn tantivy::query::Query>) {
                        let bytes: [u8; 16] = (*agent).into();
                        let term = Term::from_field_bytes(field, &bytes);
                        (
                            Occur::Should,
                            Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
                        )
                    })
                    .collect();
                clauses.push((Occur::Must, Box::new(BooleanQuery::new(inner))));
            }
            if let Some(kind) = f.memory_kind {
                let field = schema
                    .get_field("kind")
                    .map_err(|e| LexicalError::Internal(format!("kind field: {e}")))?;
                let term = Term::from_field_u64(field, memory_kind_to_u64(kind));
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
                ));
            }
            if let Some(range) = f.created_at_ms.as_ref() {
                let field = schema
                    .get_field("created_at")
                    .map_err(|e| LexicalError::Internal(format!("created_at field: {e}")))?;
                clauses.push((Occur::Must, range_query_u64(field, range)));
            }
            if !f.context_ids.is_empty() {
                let field = schema
                    .get_field("context")
                    .map_err(|e| LexicalError::Internal(format!("context field: {e}")))?;
                // `context IN [..]` = OR-group of TermQuery, wrapped as
                // a single MUST so the BM25 scoring stays inside the
                // requested context universe.
                let inner: Vec<(Occur, Box<dyn tantivy::query::Query>)> = f
                    .context_ids
                    .iter()
                    .map(|cid| -> (Occur, Box<dyn tantivy::query::Query>) {
                        let term = Term::from_field_u64(field, *cid);
                        (
                            Occur::Should,
                            Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
                        )
                    })
                    .collect();
                clauses.push((Occur::Must, Box::new(BooleanQuery::new(inner))));
            }
        }
        LexicalScope::StatementText => {
            if let Some(kind) = f.statement_kind {
                let field = schema
                    .get_field("kind")
                    .map_err(|e| LexicalError::Internal(format!("kind field: {e}")))?;
                let term = Term::from_field_u64(field, u64::from(kind.as_u8()));
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
                ));
            }
            if let Some(pid) = f.predicate_id {
                let field = schema
                    .get_field("predicate_id")
                    .map_err(|e| LexicalError::Internal(format!("predicate_id field: {e}")))?;
                let term = Term::from_field_u64(field, u64::from(pid));
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
                ));
            }
            if let Some(range) = f.confidence_bucket.as_ref() {
                let field = schema
                    .get_field("confidence_bucket")
                    .map_err(|e| LexicalError::Internal(format!("bucket field: {e}")))?;
                let lower = u64::from(*range.start());
                let upper = u64::from(*range.end());
                clauses.push((Occur::Must, range_query_u64(field, &(lower..=upper))));
            }
            if let Some(range) = f.extracted_at_ms.as_ref() {
                let field = schema
                    .get_field("extracted_at")
                    .map_err(|e| LexicalError::Internal(format!("extracted_at field: {e}")))?;
                clauses.push((Occur::Must, range_query_u64(field, range)));
            }
        }
    }

    // ----- Empty query handling --------------------------------------------
    if clauses.is_empty() {
        // Neither text nor filters → return zero results; build a
        // boolean query with `Occur::MustNot` over the all-docs
        // query so the searcher cleanly returns nothing without
        // erroring.
        return Ok(Box::new(BooleanQuery::new(vec![(
            Occur::MustNot,
            Box::new(tantivy::query::AllQuery) as Box<dyn Query>,
        )])));
    }

    Ok(Box::new(BooleanQuery::new(clauses)))
}

fn range_query_u64(field: tantivy::schema::Field, range: &RangeInclusive<u64>) -> Box<dyn Query> {
    Box::new(RangeQuery::new(
        Bound::Included(Term::from_field_u64(field, *range.start())),
        Bound::Included(Term::from_field_u64(field, *range.end())),
    ))
}

fn compose_text_input(terms: &[String], phrases: &[Vec<String>]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(terms.len() + phrases.len());
    for t in terms {
        let trimmed = t.trim();
        if !trimmed.is_empty() {
            parts.push(escape_query_token(trimmed));
        }
    }
    for ph in phrases {
        if ph.is_empty() {
            continue;
        }
        let inner = ph
            .iter()
            .map(|w| w.trim())
            .filter(|w| !w.is_empty())
            .map(escape_query_token)
            .collect::<Vec<_>>()
            .join(" ");
        if !inner.is_empty() {
            parts.push(format!("+\"{inner}\""));
        }
    }
    parts.join(" ")
}

/// Escape tantivy `QueryParser` syntax characters so the token is
/// matched literally. Phrase tokens are already wrapped in quotes
/// so we only need to escape backslashes + quotes inside.
///
/// Apostrophes (and the curly/back variants) are mapped to a space rather
/// than escaped: the indexing tokenizer splits on them, so "Niraj's" is stored
/// as the token "niraj", and a raw apostrophe in the query string makes the
/// `QueryParser` grammar fail outright ("Syntax Error: niraj's …"). Mapping to
/// a space yields "niraj s", which tokenizes to the same indexed terms and
/// parses cleanly — so possessive cues ("Niraj's manager") match instead of
/// dropping the whole lexical lane.
fn escape_query_token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\'' | '\u{2019}' | '\u{2018}' | '`' => out.push(' '),
            '\\' | '"' | '+' | '-' | '!' | '(' | ')' | '^' | '{' | '}' | '[' | ']' | ':' | '~'
            | '*' | '?' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

fn memory_kind_to_u64(kind: MemoryKind) -> u64 {
    match kind {
        MemoryKind::Episodic => 0,
        MemoryKind::Semantic => 1,
        MemoryKind::Consolidated => 2,
    }
}

fn project(
    hits: Vec<(f32, DocAddress)>,
    searcher: &Searcher,
    handle: &IndexHandle,
    scope: LexicalScope,
    config: &LexicalRetrieverConfig,
) -> Result<Vec<RankedItem>, LexicalError> {
    let schema = handle.index.schema();
    let id_field = match scope {
        LexicalScope::MemoryText => schema
            .get_field("memory_id")
            .map_err(|e| LexicalError::Internal(format!("memory_id field: {e}")))?,
        LexicalScope::StatementText => schema
            .get_field("statement_id")
            .map_err(|e| LexicalError::Internal(format!("statement_id field: {e}")))?,
    };

    let mut out = Vec::with_capacity(hits.len());
    let mut rank: u32 = 0;
    for (score, addr) in hits {
        if let Some(min) = config.min_score {
            if score < min {
                continue;
            }
        }
        let doc: TantivyDocument = searcher
            .doc(addr)
            .map_err(|e| LexicalError::Internal(format!("doc fetch: {e}")))?;
        let bytes = doc
            .get_first(id_field)
            .and_then(|v| v.as_bytes())
            .ok_or_else(|| LexicalError::Internal("doc missing id field".into()))?;
        let id_arr: [u8; 16] = bytes
            .try_into()
            .map_err(|_| LexicalError::Internal("id field not 16 bytes".into()))?;
        let id = match scope {
            LexicalScope::MemoryText => {
                RankedItemId::Memory(MemoryId::from_raw(u128::from_be_bytes(id_arr)))
            }
            LexicalScope::StatementText => RankedItemId::Statement(StatementId::from(id_arr)),
        };
        rank += 1;
        out.push(RankedItem {
            id,
            rank,
            score,
            snippet: None,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
