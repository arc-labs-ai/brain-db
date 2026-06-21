//! Write-time HyPE generation: hypothetical-question embeddings.
//!
//! For each freshly-encoded memory the generator asks an LLM for several
//! diverse questions whose answer is the memory, embeds each locally, and
//! both persists the vectors (`hype_question_vectors` table) and inserts
//! them into the live per-shard [`HypeHnswIndex`]. At read time the user's
//! query vector probes that pool — a hit on a stored question maps back to
//! the owning memory, bridging the query↔memory phrasing gap that the
//! direct passage embedding misses.
//!
//! Cost lands entirely here, at write time: the generation LLM call is
//! cached (keyed on the memory text) so a re-ingest is free, and it
//! respects the extractor worker's per-cycle LLM budget. The embedding is
//! the same local BGE model the rest of the write path uses.

use std::sync::Arc;
use std::time::Duration;

use brain_core::MemoryId;
use brain_embed::Dispatcher;
use brain_index::HypeHnswIndex;
use brain_llm::types::SystemBlock;
use brain_llm::{LlmClient, LlmMessage, LlmRequest, LlmRole};
use brain_metadata::llm_cache::{LlmResponse as CachedResponse, LLM_RESPONSES_TABLE};
use brain_metadata::{
    hype_neighborhood_hash_get, hype_neighborhood_hash_put, hype_vector_put,
    hype_vectors_delete_memory, LlmCacheDb, MetadataDb,
};
use parking_lot::{Mutex, RwLock};

/// Cache namespace for HyPE generations — ASCII "HYPE". Distinct from
/// any extraction tier's id so the two never collide in the shared
/// `llm_cache.redb`.
const HYPE_CACHE_EXTRACTOR_ID: u32 = 0x4859_5045;
/// Bump to invalidate cached generations after a prompt change.
const HYPE_CACHE_VERSION: u32 = 1;
/// Cached generation lifetime — long, since the memory text is immutable.
const HYPE_CACHE_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Hard cap on questions stored per memory. The 17-byte storage key's
/// index byte allows 256; we generate far fewer but clamp defensively so
/// a runaway LLM reply can't overflow the `u8` index.
const MAX_QUESTIONS_PER_MEMORY: usize = 16;

/// Write-time HyPE generator. One per shard; cheap to hold (`Arc`
/// handles). Constructed only when the LLM tier is provisioned and HyPE
/// is enabled — otherwise the extractor worker leaves its slot `None` and
/// skips generation entirely.
#[derive(Clone)]
pub struct HypeGenerator {
    client: Arc<dyn LlmClient>,
    model: String,
    embedder: Arc<dyn Dispatcher>,
    index: Arc<RwLock<HypeHnswIndex>>,
    metadata: Arc<MetadataDb>,
    cache: Arc<Mutex<LlmCacheDb>>,
    /// How many questions to ask for per memory (clamped to
    /// [`MAX_QUESTIONS_PER_MEMORY`]).
    num_questions: usize,
}

/// Outcome of one memory's generation, folded into the worker's
/// per-cycle bookkeeping.
#[derive(Debug, Default, Clone, Copy)]
pub struct HypeGenOutcome {
    /// Questions embedded + persisted + inserted.
    pub questions_written: usize,
    /// LLM micro-USD spent (0 on a cache hit).
    pub cost_micro_usd: u64,
}

impl HypeGenerator {
    pub fn new(
        client: Arc<dyn LlmClient>,
        model: String,
        embedder: Arc<dyn Dispatcher>,
        index: Arc<RwLock<HypeHnswIndex>>,
        metadata: Arc<MetadataDb>,
        cache: Arc<Mutex<LlmCacheDb>>,
        num_questions: usize,
    ) -> Self {
        Self {
            client,
            model,
            embedder,
            index,
            metadata,
            cache,
            num_questions: num_questions.clamp(1, MAX_QUESTIONS_PER_MEMORY),
        }
    }

    /// Generate, embed, persist, and index hypothetical questions for one
    /// memory. Best-effort: any failure (LLM transport, empty reply,
    /// embed error, redb error) is logged and yields a zero outcome
    /// rather than failing the surrounding extraction cycle — HyPE is a
    /// recall enhancement, never a correctness dependency.
    ///
    /// `neighborhood` is a terse rendering of the typed-graph facts already
    /// known about the entities this memory mentions (their statements and
    /// relation edges). It is empty for the first memory about a subject and
    /// fills in as the graph grows. Feeding it to the generator lets the LLM
    /// write *bridge* questions that span more than this one memory — e.g.
    /// "Where did Niraj's manager work before?" when the neighborhood already
    /// records the manager and their prior employer. Those bridge questions
    /// are embedded into the same HyPE pool the read path probes, so a
    /// multi-hop query resolves to a single cheap ANN lookup with no read-side
    /// LLM call.
    pub async fn generate_for(
        &self,
        memory_id: MemoryId,
        text: &str,
        neighborhood: &str,
    ) -> HypeGenOutcome {
        if text.trim().is_empty() {
            return HypeGenOutcome::default();
        }

        let (questions, cost_micro) = match self.questions_for(text, neighborhood).await {
            Ok(qs) => qs,
            Err(e) => {
                tracing::warn!(
                    target: "brain_workers::hype",
                    memory_id = ?memory_id,
                    error = %e,
                    "HyPE question generation failed; skipping",
                );
                return HypeGenOutcome::default();
            }
        };
        if questions.is_empty() {
            return HypeGenOutcome {
                questions_written: 0,
                cost_micro_usd: cost_micro,
            };
        }

        // Embed each question with the query transform — at read time the
        // user query is embedded the same way, so question↔query cosine
        // is in-distribution.
        let mut vectors: Vec<[f32; 384]> = Vec::with_capacity(questions.len());
        for q in &questions {
            match self.embedder.embed_query(q) {
                Ok(v) => vectors.push(v),
                Err(e) => {
                    tracing::warn!(
                        target: "brain_workers::hype",
                        memory_id = ?memory_id,
                        error = %e,
                        "HyPE question embed failed; skipping this question",
                    );
                }
            }
        }
        if vectors.is_empty() {
            return HypeGenOutcome {
                questions_written: 0,
                cost_micro_usd: cost_micro,
            };
        }

        // Persist first (durable source of truth), then publish to the
        // live index. A crash between the two is harmless: boot rebuilds
        // the index from the persisted rows. `replace=false`: this is the
        // first generation for the memory, so there are no prior rows to
        // clear and the points are appended to the live index.
        let nbhd_hash = input_hash(text, neighborhood);
        match self.persist(memory_id, &vectors, nbhd_hash, false) {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(
                    target: "brain_workers::hype",
                    memory_id = ?memory_id,
                    error = %e,
                    "HyPE vector persist failed; not indexing",
                );
                return HypeGenOutcome {
                    questions_written: 0,
                    cost_micro_usd: cost_micro,
                };
            }
        }
        {
            let mut idx = self.index.write();
            for v in &vectors {
                idx.insert(memory_id, v);
            }
        }

        HypeGenOutcome {
            questions_written: vectors.len(),
            cost_micro_usd: cost_micro,
        }
    }

    /// Regenerate a memory's HyPE questions when its typed-graph neighborhood
    /// has grown since they were last generated — the Phase-3 refresh path.
    ///
    /// The bridge questions a multi-hop read needs ("Where did Niraj's manager
    /// work before?") can only be written once the connecting facts exist, but
    /// extraction is incremental: a memory is first HyPE'd against whatever
    /// graph existed at encode time, which may be empty. This recomputes the
    /// `(text, neighborhood)` hash and, **only if it differs** from the stored
    /// one, regenerates: the LLM is called (cache-keyed on the new pair, so a
    /// genuinely-changed neighborhood misses), the new vectors **replace** the
    /// old rows (delete-then-put) and the live index points are swapped via
    /// [`HypeHnswIndex::refresh_memory`]. A matching hash is a cheap no-op (one
    /// redb point read, no LLM). Best-effort, like [`Self::generate_for`].
    pub async fn refresh_for(
        &self,
        memory_id: MemoryId,
        text: &str,
        neighborhood: &str,
    ) -> HypeGenOutcome {
        if text.trim().is_empty() || neighborhood.trim().is_empty() {
            return HypeGenOutcome::default();
        }
        let nbhd_hash = input_hash(text, neighborhood);

        // Gate: skip unless the neighborhood actually changed. The stored hash
        // is the one that produced the memory's current questions. On a read
        // error we fall through and regenerate — correctness over a saved call.
        if let Ok(rtxn) = self.metadata.read_txn() {
            if hype_neighborhood_hash_get(&rtxn, memory_id).ok().flatten() == Some(nbhd_hash) {
                return HypeGenOutcome::default();
            }
        }

        let (questions, cost_micro) = match self.questions_for(text, neighborhood).await {
            Ok(qs) => qs,
            Err(e) => {
                tracing::warn!(
                    target: "brain_workers::hype",
                    memory_id = ?memory_id,
                    error = %e,
                    "HyPE refresh generation failed; keeping prior questions",
                );
                return HypeGenOutcome::default();
            }
        };
        if questions.is_empty() {
            return HypeGenOutcome {
                questions_written: 0,
                cost_micro_usd: cost_micro,
            };
        }

        let mut vectors: Vec<[f32; 384]> = Vec::with_capacity(questions.len());
        for q in &questions {
            if let Ok(v) = self.embedder.embed_query(q) {
                vectors.push(v);
            }
        }
        if vectors.is_empty() {
            return HypeGenOutcome {
                questions_written: 0,
                cost_micro_usd: cost_micro,
            };
        }

        // Replace prior rows (delete-then-put) and advance the stored hash in
        // one txn, then swap the live index points.
        match self.persist(memory_id, &vectors, nbhd_hash, true) {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(
                    target: "brain_workers::hype",
                    memory_id = ?memory_id,
                    error = %e,
                    "HyPE refresh persist failed; not reindexing",
                );
                return HypeGenOutcome {
                    questions_written: 0,
                    cost_micro_usd: cost_micro,
                };
            }
        }
        self.index.write().refresh_memory(memory_id, &vectors);

        HypeGenOutcome {
            questions_written: vectors.len(),
            cost_micro_usd: cost_micro,
        }
    }

    /// Persist all question vectors for a memory in one write transaction,
    /// recording the `(text, neighborhood)` hash that produced them so the
    /// refresh worker can detect a later neighborhood change. When `replace`
    /// is set, every prior question row for the memory is deleted first, so a
    /// regeneration that yields fewer questions leaves no orphan rows.
    fn persist(
        &self,
        memory_id: MemoryId,
        vectors: &[[f32; 384]],
        neighborhood_hash: [u8; 32],
        replace: bool,
    ) -> Result<(), String> {
        let wtxn = self
            .metadata
            .write_txn()
            .map_err(|e| format!("hype write_txn: {e}"))?;
        if replace {
            hype_vectors_delete_memory(&wtxn, memory_id)
                .map_err(|e| format!("hype_vectors_delete_memory: {e}"))?;
        }
        for (i, v) in vectors.iter().enumerate() {
            let idx = u8::try_from(i).map_err(|_| "hype question index overflow".to_string())?;
            hype_vector_put(&wtxn, memory_id, idx, v)
                .map_err(|e| format!("hype_vector_put: {e}"))?;
        }
        hype_neighborhood_hash_put(&wtxn, memory_id, neighborhood_hash)
            .map_err(|e| format!("hype_neighborhood_hash_put: {e}"))?;
        wtxn.commit().map_err(|e| format!("hype commit: {e}"))?;
        Ok(())
    }

    /// Return the generated questions and the LLM cost (0 on a cache
    /// hit). Reads the shared LLM cache first; on a miss it calls the
    /// model and writes the reply back.
    async fn questions_for(
        &self,
        text: &str,
        neighborhood: &str,
    ) -> Result<(Vec<String>, u64), String> {
        // The cache key folds in the neighborhood: the same memory text with a
        // richer graph around it should regenerate (different bridge
        // questions), so a neighborhood change must miss the cache.
        let input_hash = input_hash(text, neighborhood);
        let model_id_hash = self.client.model_id_hash();

        if let Some(blob) = self.cache_get(input_hash, model_id_hash) {
            let raw = String::from_utf8_lossy(&blob);
            return Ok((parse_questions(&raw, self.num_questions), 0));
        }

        let req = LlmRequest {
            model: self.model.clone(),
            system_blocks: vec![SystemBlock::cached(Self::system_prompt())],
            messages: vec![LlmMessage {
                role: LlmRole::User,
                content: Self::user_prompt(text, neighborhood, self.num_questions),
            }],
            response_schema: None,
            temperature: 0.3,
            max_tokens: 256,
            timeout: Duration::from_secs(30),
        };
        let resp = self
            .client
            .complete(req)
            .await
            .map_err(|e| format!("hype llm transport: {e}"))?;

        let questions = parse_questions(&resp.content, self.num_questions);
        if !questions.is_empty() {
            // Cache the raw reply so a re-ingest re-parses the same text
            // for free. A parse-empty reply is not cached (it may be a
            // transient model hiccup worth retrying next time).
            self.cache_put(
                input_hash,
                model_id_hash,
                resp.content.clone().into_bytes(),
                resp.tokens_out as u32,
            );
        }
        Ok((questions, resp.cost_micro_usd))
    }

    fn system_prompt() -> String {
        // Stable wording: Anthropic prompt caching keys on byte-identical
        // blocks, so any drift wipes the cache.
        "You generate retrieval questions for a memory database. Given a \
short piece of text, output diverse questions that this text answers — \
the questions a user might later ask to find this exact fact. Vary the \
phrasing and angle (who/what/when/where/why/how). Output ONE question \
per line, nothing else: no numbering, no preamble, no blank lines. Each \
line must end with a question mark."
            .to_owned()
    }

    fn user_prompt(text: &str, neighborhood: &str, n: usize) -> String {
        if neighborhood.trim().is_empty() {
            return format!(
                "Write {n} diverse questions that the following text answers.\n\nText:\n{text}"
            );
        }
        // With a neighborhood present, ask for a mix: questions the text answers
        // directly, PLUS bridge questions that chain the text through the
        // connected facts (so a later multi-hop query lands on a stored
        // question). The connected facts are context only — every question must
        // still be answerable from the text together with those facts.
        format!(
            "Write {n} diverse questions answerable from the text below. Include both \
direct questions the text answers on its own AND bridge questions that combine \
the text with one or more of the connected facts to follow a chain (for example, \
asking about an attribute of a person the text links to). Resolve names fully; \
do not use pronouns.\n\nText:\n{text}\n\nConnected facts:\n{neighborhood}"
        )
    }

    fn cache_get(&self, input_hash: [u8; 32], model_id_hash: u64) -> Option<Vec<u8>> {
        let db = self.cache.lock();
        let rtxn = db.read_txn().ok()?;
        let t = rtxn.open_table(LLM_RESPONSES_TABLE).ok()?;
        let key = (
            input_hash,
            HYPE_CACHE_EXTRACTOR_ID,
            HYPE_CACHE_VERSION,
            model_id_hash,
        );
        let row = t.get(&key).ok().flatten()?;
        Some(row.value().response_blob)
    }

    fn cache_put(&self, input_hash: [u8; 32], model_id_hash: u64, blob: Vec<u8>, tokens: u32) {
        let now_nanos = now_unix_nanos();
        let expires = now_nanos.saturating_add(HYPE_CACHE_TTL.as_nanos() as u64);
        let mut db = self.cache.lock();
        let Ok(wtxn) = db.write_txn() else {
            return;
        };
        let key = (
            input_hash,
            HYPE_CACHE_EXTRACTOR_ID,
            HYPE_CACHE_VERSION,
            model_id_hash,
        );
        let value = CachedResponse::new(blob, now_nanos, expires, tokens, model_id_hash);
        {
            let Ok(mut t) = wtxn.open_table(LLM_RESPONSES_TABLE) else {
                return;
            };
            if t.insert(&key, &value).is_err() {
                return;
            }
        }
        let _ = wtxn.commit();
    }
}

/// Parse a newline-delimited question list, stripping common list noise
/// (numbering, bullets, surrounding quotes), dropping non-questions and
/// blanks, deduping, and clamping to `num_questions`. Newline-delimited
/// output is far more robust to parse than JSON from a small model.
fn parse_questions(raw: &str, num_questions: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in raw.lines() {
        let mut s = line.trim();
        // Strip leading "1.", "1)", "-", "*", "•" list markers.
        s = s.trim_start_matches(|c: char| {
            c.is_ascii_digit() || matches!(c, '.' | ')' | '-' | '*' | '•' | ' ' | '\t')
        });
        let s = s.trim().trim_matches('"').trim();
        if s.len() < 8 || !s.contains('?') {
            continue;
        }
        let key = s.to_lowercase();
        if seen.insert(key) {
            out.push(s.to_string());
        }
        if out.len() >= num_questions {
            break;
        }
    }
    out
}

/// Hash of the `(text, neighborhood)` pair. Used both as the LLM-cache key
/// (a changed neighborhood must miss the cache and regenerate) and as the
/// stored neighborhood-gate hash the refresh worker compares against. A `0x00`
/// separator keeps `(text, nbhd)` and `(text+nbhd, "")` from colliding.
fn input_hash(text: &str, neighborhood: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(text.as_bytes());
    hasher.update(&[0x00]);
    hasher.update(neighborhood.as_bytes());
    *hasher.finalize().as_bytes()
}

fn now_unix_nanos() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::parse_questions;

    #[test]
    fn parse_strips_numbering_and_bullets() {
        let raw =
            "1. Where did Caroline move from?\n- What country is Caroline from?\n* Who is Caroline?";
        let qs = parse_questions(raw, 6);
        assert_eq!(qs.len(), 3);
        assert_eq!(qs[0], "Where did Caroline move from?");
        assert_eq!(qs[1], "What country is Caroline from?");
        assert_eq!(qs[2], "Who is Caroline?");
    }

    #[test]
    fn parse_drops_non_questions_and_blanks() {
        let raw = "Here are some questions:\n\nWhat is the capital?\nshort\nThis is a statement.";
        let qs = parse_questions(raw, 6);
        assert_eq!(qs, vec!["What is the capital?".to_string()]);
    }

    #[test]
    fn parse_dedups_and_caps() {
        let raw = "What is X?\nWhat is X?\nWho is Y?\nWhen is Z?";
        let qs = parse_questions(raw, 2);
        assert_eq!(qs.len(), 2, "capped at num_questions");
        assert_eq!(qs[0], "What is X?");
        assert_eq!(qs[1], "Who is Y?", "duplicate skipped, next distinct kept");
    }
}
