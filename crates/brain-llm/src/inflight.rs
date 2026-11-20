//! What derivation is holding, right now, split by what is holding it.
//!
//! Derivation occupies memory in two shapes, and one budget covers both.
//!
//! - **LLM payloads.** A request body is alive from the moment it is built
//!   until the provider answers: prompt text, system blocks and the response
//!   schema, which is routinely the largest part.
//! - **Preparation working sets.** Before any provider call, a batch gathers
//!   neighbour texts, a rolling summary, candidate-predicate embeddings and
//!   per-namespace vocabulary. That set is alive for the whole prep stage and
//!   is frequently larger than the request it feeds.
//!
//! Both come out of the same machine memory, so [`crate::admission`] draws
//! them against one budget. Accounting still has to tell them apart, because
//! the remedies do not match: payload pressure is answered by shrinking
//! prompts or schemas, prep pressure by narrowing neighbour fan-out or batch
//! width. An operator staring at one undifferentiated byte count cannot tell
//! which lever to pull.
//!
//! So each shape gets its own [`Pool`], and [`InFlight`] carries the combined
//! figures the budget is actually compared against.
//!
//! NOTHING HERE REFUSES OR DELAYS WORK: it is accounting. Admission is what
//! waits.
//!
//! Process-global on purpose. Shards are share-nothing for *data*, but they
//! face one provider over one connection pool and one machine's memory, so the
//! pressure this describes is a property of the process, not of a shard.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::types::LlmRequest;

/// One shape of occupancy: how much of it is live, and the most there has
/// ever been.
///
/// A "hold" is one outstanding unit of that shape — one provider call, or one
/// prep stage. The count is a diagnostic; bytes are what the budget spends.
#[derive(Debug)]
pub struct Pool {
    holds: AtomicUsize,
    bytes: AtomicU64,
    peak_holds: AtomicUsize,
    peak_bytes: AtomicU64,
}

/// Live and peak derivation occupancy, by shape and in total.
#[derive(Debug)]
pub struct InFlight {
    llm: Pool,
    prep: Pool,
    /// Peak of the *sum*, which is not the sum of the peaks: the two shapes
    /// rarely crest together. The budget bounds the sum, so this is the figure
    /// the budget is sized against.
    peak_bytes: AtomicU64,
}

static GLOBAL: InFlight = InFlight::new();

/// The process-wide meter.
#[must_use]
pub fn global() -> &'static InFlight {
    &GLOBAL
}

/// How much a request holds while it is outstanding.
///
/// The request body as the caller built it: prompt text, system blocks, and
/// the response schema, which is routinely the largest part and the reason a
/// count cannot stand in for this. The provider's own encoding adds framing on
/// top; this is the payload the process is keeping alive, which is what a
/// memory budget has to be drawn against.
#[must_use]
pub fn request_bytes(request: &LlmRequest) -> u64 {
    let text: usize = request.model.len()
        + request
            .system_blocks
            .iter()
            .map(|b| b.text.len())
            .sum::<usize>()
        + request
            .messages
            .iter()
            .map(|m| m.content.len())
            .sum::<usize>();
    let schema = request
        .response_schema
        .as_ref()
        .map_or(0, |s| serde_json::to_string(s).map_or(0, |j| j.len()));
    (text + schema) as u64
}

impl Pool {
    const fn new() -> Self {
        Self {
            holds: AtomicUsize::new(0),
            bytes: AtomicU64::new(0),
            peak_holds: AtomicUsize::new(0),
            peak_bytes: AtomicU64::new(0),
        }
    }

    /// Units of this shape outstanding.
    #[must_use]
    pub fn holds(&self) -> usize {
        self.holds.load(Ordering::Relaxed)
    }

    /// Bytes of this shape outstanding.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Most units of this shape ever outstanding at once.
    #[must_use]
    pub fn peak_holds(&self) -> usize {
        self.peak_holds.load(Ordering::Relaxed)
    }

    /// Most bytes of this shape ever outstanding at once.
    #[must_use]
    pub fn peak_bytes(&self) -> u64 {
        self.peak_bytes.load(Ordering::Relaxed)
    }

    fn enter(&'static self, bytes: u64) -> u64 {
        let holds = self.holds.fetch_add(1, Ordering::Relaxed) + 1;
        let held = self.bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak_holds.fetch_max(holds, Ordering::Relaxed);
        self.peak_bytes.fetch_max(held, Ordering::Relaxed);
        held
    }
}

impl InFlight {
    const fn new() -> Self {
        Self {
            llm: Pool::new(),
            prep: Pool::new(),
            peak_bytes: AtomicU64::new(0),
        }
    }

    /// Provider-payload occupancy.
    #[must_use]
    pub const fn llm(&self) -> &Pool {
        &self.llm
    }

    /// Derivation-preparation occupancy.
    #[must_use]
    pub const fn prep(&self) -> &Pool {
        &self.prep
    }

    /// Record a provider call entering flight. The returned guard releases it
    /// on drop, so a cancelled or panicking call cannot leak occupancy.
    pub fn enter_call(&'static self, bytes: u64) -> Occupancy {
        self.record(&self.llm, bytes)
    }

    /// Record a prep stage entering flight, for the stage's whole lifetime.
    /// The returned guard releases it on drop, so an abandoned batch cannot
    /// leak occupancy.
    pub fn enter_prep(&'static self, bytes: u64) -> Occupancy {
        self.record(&self.prep, bytes)
    }

    fn record(&'static self, pool: &'static Pool, bytes: u64) -> Occupancy {
        let in_pool = pool.enter(bytes);
        // The other pool is read without a lock, so a concurrent entry can
        // make this total momentarily stale. Taking a lock on the hot path to
        // sharpen a high-water mark would cost more than the mark is worth —
        // it is a sizing hint, not an invariant.
        let total = in_pool.saturating_add(self.other_than(pool));
        self.peak_bytes.fetch_max(total, Ordering::Relaxed);
        Occupancy { pool, bytes }
    }

    fn other_than(&self, pool: &Pool) -> u64 {
        if std::ptr::eq(pool, &self.llm) {
            self.prep.bytes()
        } else {
            self.llm.bytes()
        }
    }

    /// Bytes outstanding across both shapes — what the admission budget is
    /// spent on.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.llm.bytes().saturating_add(self.prep.bytes())
    }

    /// Units outstanding across both shapes.
    #[must_use]
    pub fn holds(&self) -> usize {
        self.llm.holds().saturating_add(self.prep.holds())
    }

    /// Most bytes ever outstanding at once across both shapes — the number a
    /// memory budget is sized against.
    #[must_use]
    pub fn peak_bytes(&self) -> u64 {
        self.peak_bytes.load(Ordering::Relaxed)
    }
}

/// One unit of occupancy, released on drop.
///
/// Which pool it was entered against is fixed at creation, so a guard always
/// returns its bytes to the shape that took them.
#[derive(Debug)]
pub struct Occupancy {
    pool: &'static Pool,
    bytes: u64,
}

impl Drop for Occupancy {
    fn drop(&mut self) {
        self.pool.holds.fetch_sub(1, Ordering::Relaxed);
        self.pool.bytes.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{LlmMessage, LlmRole, SystemBlock};

    fn request(system: &str, user: &str, schema: Option<serde_json::Value>) -> LlmRequest {
        LlmRequest {
            model: "m".to_string(),
            system_blocks: vec![SystemBlock {
                text: system.to_string(),
                cache: false,
            }],
            messages: vec![LlmMessage {
                role: LlmRole::User,
                content: user.to_string(),
            }],
            response_schema: schema,
            temperature: 0.0,
            max_tokens: 256,
            timeout: std::time::Duration::from_secs(30),
        }
    }

    #[test]
    fn weight_counts_prompt_and_schema() {
        // The point of weighing in bytes: two calls, same count, very
        // different load.
        let small = request_bytes(&request("s", "u", None));
        let large = request_bytes(&request("s", &"u".repeat(10_000), None));
        assert!(large > small + 9_000, "{large} vs {small}");
    }

    #[test]
    fn the_response_schema_is_counted() {
        // Routinely the biggest part of a request, and invisible to a count.
        let without = request_bytes(&request("s", "u", None));
        let with = request_bytes(&request(
            "s",
            "u",
            Some(serde_json::json!({"type": "object", "properties": {"a": {"type": "string"}}})),
        ));
        assert!(with > without, "{with} vs {without}");
    }

    #[test]
    fn occupancy_is_released_on_drop() {
        let m = global();
        let before_holds = m.llm().holds();
        let before_bytes = m.llm().bytes();
        {
            let _g = m.enter_call(1_000);
            assert_eq!(m.llm().holds(), before_holds + 1);
            assert_eq!(m.llm().bytes(), before_bytes + 1_000);
        }
        assert_eq!(
            m.llm().holds(),
            before_holds,
            "a finished call holds nothing"
        );
        assert_eq!(m.llm().bytes(), before_bytes);
    }

    #[test]
    fn peaks_do_not_fall_back() {
        // The peak is what a budget is sized against, so it must survive the
        // calls that set it.
        let m = global();
        {
            let _a = m.enter_call(10);
            let _b = m.enter_call(10);
        }
        assert!(m.llm().peak_holds() >= 2);
        assert!(m.llm().peak_bytes() >= 20);
    }

    #[test]
    fn prep_and_calls_are_metered_apart_but_total_together() {
        // The whole reason for two pools: an operator must be able to say
        // which shape is eating the budget, while the budget itself is still
        // spent on the sum.
        let m = global();
        let before_llm = m.llm().bytes();
        let before_prep = m.prep().bytes();
        let before_total = m.bytes();

        let call = m.enter_call(1_000);
        assert_eq!(m.llm().bytes(), before_llm + 1_000);
        assert_eq!(m.prep().bytes(), before_prep, "a call is not prep");

        let prep = m.enter_prep(5_000);
        assert_eq!(m.prep().bytes(), before_prep + 5_000);
        assert_eq!(m.llm().bytes(), before_llm + 1_000, "prep is not a call");
        assert_eq!(m.bytes(), before_total + 6_000, "the total covers both");
        assert_eq!(m.holds(), m.llm().holds() + m.prep().holds());

        drop(prep);
        assert_eq!(m.prep().bytes(), before_prep);
        assert_eq!(m.llm().bytes(), before_llm + 1_000, "the call still holds");
        drop(call);
        assert_eq!(m.bytes(), before_total);
    }

    #[test]
    fn the_total_peak_spans_both_pools() {
        // A peak taken per pool would under-report the moment that actually
        // stressed the machine, when prep and payloads were resident at once.
        let m = global();
        let _prep = m.enter_prep(100_000);
        let _call = m.enter_call(100_000);
        assert!(
            m.peak_bytes() >= 200_000,
            "peak {} must cover both shapes at once",
            m.peak_bytes()
        );
    }
}
