//! What derivation may hold, measured in bytes rather than counted.
//!
//! Derivation is admitted by the bytes a call holds, never by a count of
//! calls. A count is the wrong unit in both directions: it cannot use a large machine, and on a small one
//! it says nothing about what a call costs. A memory with a long text and a
//! large response schema holds far more while it waits than a one-liner does,
//! and thirty-two of each are not the same load.
//!
//! So nothing here counts tasks. Work is admitted while the bytes it holds fit
//! a budget the machine decides, and a caller that cannot fit yet simply
//! waits. Waiting is nearly free: a future parked on a permit owns no stack —
//! unlike a goroutine — is not polled, and holds only itself. Millions may be
//! created; the budget decides how many are *resident*, which is the only
//! resource that was ever scarce.
//!
//! Nothing is refused. The durable extraction queue is the buffer, so pressure
//! here defers work rather than losing it.
//!
//! ## One budget, two shapes of occupancy
//!
//! A provider payload is not the only thing derivation keeps alive. Before any
//! call, a batch assembles a working set — neighbour texts, a rolling summary,
//! candidate-predicate embeddings, per-namespace vocabulary — and holds it for
//! the whole preparation stage. That set is frequently larger than the request
//! it feeds.
//!
//! Both are charged against the SAME budget, through the same semaphore.
//! Giving preparation a second, independent budget would be the obvious shape
//! and the wrong one: two budgets each sized against the machine can be full
//! at the same time, so their sum exceeds the machine and the bound stops
//! meaning anything. Worse, it would move unbounded growth out of a governed
//! place into an ungoverned one the moment concurrency widened. One budget,
//! drawn on by both, is the only arrangement where the number the machine
//! handed us still describes the machine.
//!
//! The two shapes are metered apart — see [`crate::inflight`] — because the
//! remedies differ. They are *admitted* together, because the memory does not
//! care which of them spent it.

use std::sync::OnceLock;

use tokio::sync::{Semaphore, SemaphorePermit};

use crate::inflight::{self, Occupancy};
use crate::prep::{self, PrepWorkload};
use crate::types::LlmRequest;

/// Permits are kibibytes, so a budget of many gibibytes still fits the `u32`
/// that `acquire_many` takes.
const BYTES_PER_PERMIT: u64 = 1024;

/// Budget used when nothing configured one.
///
/// Deliberately modest: an unconfigured process is one whose memory nobody has
/// measured, and the failure that matters is exhausting it. 64 MiB of
/// resident derivation is enough to keep a small deployment busy and small
/// enough that it cannot be the thing that kills the process.
const DEFAULT_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

static ADMISSION: OnceLock<Admission> = OnceLock::new();

/// Set the derivation budget. First call wins.
///
/// Must run before the first provider call, which in practice means during
/// startup, before any shard is spawned. A later call is ignored rather than
/// panicking: losing a tuning value is not worth refusing to serve, and the
/// returned flag lets a caller log the fact.
pub fn configure(budget_bytes: u64) -> bool {
    ADMISSION.set(Admission::new(budget_bytes)).is_ok()
}

/// The live admission control.
pub fn global() -> &'static Admission {
    ADMISSION.get_or_init(|| Admission::new(DEFAULT_BUDGET_BYTES))
}

/// Admit `request` when its payload fits the budget, accounting for it until
/// the returned guard drops.
pub async fn admit(request: &LlmRequest) -> Reservation {
    global().admit_bytes(inflight::request_bytes(request)).await
}

/// Reserve what a batch's preparation stage will hold, against the same
/// budget provider payloads draw on.
///
/// Hold the returned guard for the whole stage — from the first neighbour
/// fetch to the point the gathered context is no longer referenced. Dropping
/// it early tells the budget the memory is free while it is still resident,
/// which is exactly the overcommit this exists to prevent.
pub async fn reserve_prep(workload: &PrepWorkload) -> Reservation {
    global()
        .reserve_prep_bytes(prep::prep_bytes(workload))
        .await
}

/// A byte budget for everything derivation holds at once.
#[derive(Debug)]
pub struct Admission {
    permits: Semaphore,
    budget_permits: u32,
    budget_bytes: u64,
}

impl Admission {
    fn new(budget_bytes: u64) -> Self {
        // At least one permit: a budget rounding to zero would admit nothing
        // and stall derivation completely, which is worse than overshooting a
        // tiny budget by one call.
        let budget_permits = u32::try_from(budget_bytes / BYTES_PER_PERMIT)
            .unwrap_or(u32::MAX)
            .max(1);
        Self {
            permits: Semaphore::new(budget_permits as usize),
            budget_permits,
            budget_bytes,
        }
    }

    /// The configured budget.
    #[must_use]
    pub const fn budget_bytes(&self) -> u64 {
        self.budget_bytes
    }

    /// Bytes that may still be admitted right now.
    #[must_use]
    pub fn available_bytes(&self) -> u64 {
        self.permits.available_permits() as u64 * BYTES_PER_PERMIT
    }

    /// Wait until a provider payload of `bytes` fits, then admit it.
    pub async fn admit_bytes(&'static self, bytes: u64) -> Reservation {
        let permit = self.acquire(bytes, self.budget_permits).await;
        Reservation {
            _permit: permit,
            _occupancy: inflight::global().enter_call(bytes),
        }
    }

    /// Wait until a preparation working set of `bytes` fits, then reserve it.
    ///
    /// Same budget, same queue, same deadlock guard as a provider payload —
    /// only the meter it reports to differs.
    pub async fn reserve_prep_bytes(&'static self, bytes: u64) -> Reservation {
        let permit = self.acquire(bytes, self.prep_ceiling()).await;
        Reservation {
            _permit: permit,
            _occupancy: inflight::global().enter_prep(bytes),
        }
    }

    /// The most permits one preparation reservation may hold.
    ///
    /// A payload reservation is taken by a caller holding nothing else, so
    /// letting an oversized one take the whole budget is safe — it runs
    /// alone and then releases. A PREPARATION reservation is not like that.
    /// Its holder keeps the prepared context resident and then asks this
    /// same budget for the payload bytes to send it, so a prep reservation
    /// that took everything would be waiting for budget only it can free.
    /// That is a deadlock, not backpressure, and no amount of waiting
    /// resolves it.
    ///
    /// Half, so a batch can always afford the payloads it is preparing for.
    /// A prep estimate above this is admitted at the cap rather than
    /// refused: running one oversized batch slightly over budget is
    /// recoverable, and stalling derivation forever is not.
    fn prep_ceiling(&self) -> u32 {
        (self.budget_permits / 2).max(1)
    }

    async fn acquire(&'static self, bytes: u64, ceiling: u32) -> SemaphorePermit<'static> {
        // Work larger than the WHOLE budget would wait for permits that can
        // never all exist, which is a deadlock rather than backpressure. It is
        // admitted alone instead: it holds everything, nothing else runs
        // beside it, and the budget is honoured as closely as a single
        // oversized unit allows. One memory with an enormous schema — or one
        // batch with an enormous neighbourhood — must not be able to stop
        // derivation forever.
        let want = u32::try_from(bytes.div_ceil(BYTES_PER_PERMIT))
            .unwrap_or(u32::MAX)
            .clamp(1, ceiling);
        self.permits
            .acquire_many(want)
            .await
            .expect("invariant: the admission semaphore is never closed")
    }
}

/// Admitted work. Releases its budget and its accounting on drop, including
/// when the work is cancelled or panics.
///
/// One type for both shapes, because the budget treats them identically; the
/// meter the occupancy was entered against carries the distinction.
#[derive(Debug)]
pub struct Reservation {
    _permit: SemaphorePermit<'static>,
    _occupancy: Occupancy,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admission(budget: u64) -> &'static Admission {
        Box::leak(Box::new(Admission::new(budget)))
    }

    /// Long enough that a semaphore handoff would certainly have happened,
    /// short enough to keep the suite quick.
    const PARK: std::time::Duration = std::time::Duration::from_millis(50);

    #[tokio::test]
    async fn work_within_budget_is_admitted_immediately() {
        let a = admission(1024 * 1024);
        let _held = a.admit_bytes(4096).await;
        assert!(a.available_bytes() < a.budget_bytes());
    }

    #[tokio::test]
    async fn the_budget_is_released_on_drop() {
        let a = admission(1024 * 1024);
        let before = a.available_bytes();
        {
            let _held = a.admit_bytes(64 * 1024).await;
            assert!(a.available_bytes() < before);
        }
        assert_eq!(a.available_bytes(), before, "a finished call holds nothing");
    }

    #[tokio::test]
    async fn a_request_larger_than_the_whole_budget_runs_alone() {
        // The deadlock this exists to prevent: asking for more permits than
        // can ever exist waits forever. One oversized memory must not stop
        // derivation for every other memory.
        let a = admission(16 * 1024);
        let held = a.admit_bytes(1024 * 1024 * 1024).await;
        assert_eq!(a.available_bytes(), 0, "it holds the whole budget");
        drop(held);
        assert_eq!(a.available_bytes(), a.budget_bytes());
    }

    #[tokio::test]
    async fn a_zero_byte_request_still_takes_a_permit() {
        // Otherwise an empty prompt would be unbounded concurrency.
        let a = admission(1024 * 1024);
        let before = a.available_bytes();
        let _held = a.admit_bytes(0).await;
        assert!(a.available_bytes() < before);
    }

    #[tokio::test]
    async fn a_budget_that_rounds_to_zero_still_admits() {
        // A 100-byte budget must not stall derivation forever.
        let a = admission(100);
        let _held = a.admit_bytes(4096).await;
    }

    #[tokio::test]
    async fn work_waits_when_the_budget_is_spent_and_proceeds_when_it_frees() {
        let a = admission(8 * 1024);
        let first = a.admit_bytes(8 * 1024).await;
        // Nothing is left, so a second caller must park rather than proceed.
        let parked = tokio::time::timeout(PARK, a.admit_bytes(4 * 1024)).await;
        assert!(
            parked.is_err(),
            "a caller must park while the budget is fully held, not proceed"
        );
        drop(first);
        // With the budget returned it proceeds.
        let _third = a.admit_bytes(4 * 1024).await;
    }

    #[tokio::test]
    async fn prep_within_budget_is_reserved_immediately() {
        let a = admission(1024 * 1024);
        let _held = a.reserve_prep_bytes(64 * 1024).await;
        assert!(a.available_bytes() < a.budget_bytes());
    }

    #[tokio::test]
    async fn a_prep_reservation_is_released_on_drop() {
        // Prep is released by the guard, not by the stage ending, so an
        // abandoned or panicking batch cannot leak the budget.
        let a = admission(1024 * 1024);
        let before = a.available_bytes();
        {
            let _held = a.reserve_prep_bytes(256 * 1024).await;
            assert!(a.available_bytes() < before);
        }
        assert_eq!(a.available_bytes(), before);
    }

    #[tokio::test]
    async fn a_prep_holder_can_still_afford_the_payload_it_prepared_for() {
        // The deadlock this guards: `drain_batch` holds its prep
        // reservation across its own provider calls, so the holder asks
        // this same budget for payload bytes BEFORE releasing. A prep
        // reservation allowed to take everything would be waiting for
        // budget that only it can free — no amount of waiting resolves
        // that, and derivation stops forever.
        let a = admission(64 * 1024);

        // A prep estimate larger than the entire budget: the worst case.
        let _prep = a.reserve_prep_bytes(256 * 1024).await;

        // The payload the same holder now needs must still be admittable.
        let payload = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            a.admit_bytes(8 * 1024),
        )
        .await;
        assert!(
            payload.is_ok(),
            "a prep holder must be able to admit the payload it prepared for; \
             waiting here is a deadlock, not backpressure",
        );
    }

    #[tokio::test]
    async fn a_prep_reservation_larger_than_the_whole_budget_runs_alone() {
        // A batch whose neighbourhood outweighs the machine must still make
        // progress rather than parking on permits that can never exist.
        //
        // It is capped at half the budget rather than all of it, because the
        // holder goes on to request payload bytes from this same budget
        // before releasing — see `prep_ceiling`.
        let a = admission(16 * 1024);
        let held = a.reserve_prep_bytes(4 * 1024 * 1024 * 1024).await;
        assert!(
            a.available_bytes() > 0,
            "an oversized prep reservation must leave room for the payloads \
             its own holder will ask for",
        );
        assert!(
            a.available_bytes() <= a.budget_bytes() / 2,
            "it should still hold the bulk of the budget",
        );
        drop(held);
        assert_eq!(a.available_bytes(), a.budget_bytes());
    }

    #[tokio::test]
    async fn prep_spends_the_same_budget_a_payload_would() {
        // The load-bearing test. Against two independent budgets a payload
        // would be admitted instantly here, because the payload budget would
        // be untouched. It parks only because prep already spent THIS budget.
        //
        // Prep is capped at half (see `prep_ceiling`), so the payload that
        // must park is one larger than the half it leaves behind — not one
        // that any remaining budget at all would satisfy.
        let a = admission(64 * 1024);
        let prep = a.reserve_prep_bytes(64 * 1024).await;
        let left = a.available_bytes();
        assert!(
            left <= a.budget_bytes() / 2,
            "prep must have spent the bulk of this budget, leaving {left}",
        );

        let parked = tokio::time::timeout(PARK, a.admit_bytes(left + 8 * 1024)).await;
        assert!(
            parked.is_err(),
            "a payload larger than what prep left must park; it did not, so the two budgets are separate"
        );

        drop(prep);
        let _payload = a.admit_bytes(8 * 1024).await;
        assert!(a.available_bytes() < a.budget_bytes());
    }

    #[tokio::test]
    async fn a_payload_holds_prep_out_of_the_same_budget() {
        // The converse, for the same reason: widening prep must not be able to
        // sneak past a budget that in-flight payloads have already spent.
        let a = admission(64 * 1024);
        let payload = a.admit_bytes(64 * 1024).await;
        assert_eq!(a.available_bytes(), 0, "the payload took the whole budget");

        let parked = tokio::time::timeout(PARK, a.reserve_prep_bytes(8 * 1024)).await;
        assert!(
            parked.is_err(),
            "prep must park behind a payload; it did not, so the two budgets are separate"
        );

        drop(payload);
        let _prep = a.reserve_prep_bytes(8 * 1024).await;
        assert!(a.available_bytes() < a.budget_bytes());
    }

    #[tokio::test]
    async fn the_two_shapes_sum_to_one_budget() {
        // Neither alone fills the budget; together they exactly do, and a
        // third caller of either shape must wait. Two independent budgets
        // would admit all three.
        let a = admission(64 * 1024);
        let prep = a.reserve_prep_bytes(32 * 1024).await;
        let payload = a.admit_bytes(32 * 1024).await;
        assert_eq!(a.available_bytes(), 0, "the two shapes share one budget");

        let parked = tokio::time::timeout(PARK, a.reserve_prep_bytes(1024)).await;
        assert!(parked.is_err(), "the budget is spent by the pair of them");

        drop(prep);
        drop(payload);
        assert_eq!(a.available_bytes(), a.budget_bytes());
    }

    #[tokio::test]
    async fn each_shape_is_metered_to_its_own_pool() {
        // The budget cannot tell them apart; the operator must be able to.
        let a = admission(1024 * 1024);
        let meter = inflight::global();
        let before_llm = meter.llm().bytes();
        let before_prep = meter.prep().bytes();
        let before_total = meter.bytes();

        let payload = a.admit_bytes(4 * 1024).await;
        let prep = a.reserve_prep_bytes(16 * 1024).await;

        assert_eq!(meter.llm().bytes(), before_llm + 4 * 1024);
        assert_eq!(meter.prep().bytes(), before_prep + 16 * 1024);
        assert_eq!(
            meter.bytes(),
            before_total + 20 * 1024,
            "the pools still sum to what the budget spent"
        );

        drop(prep);
        drop(payload);
        assert_eq!(meter.bytes(), before_total);
    }

    #[tokio::test]
    async fn a_prep_workload_is_priced_before_it_is_reserved() {
        // The seam a caller outside this crate uses: describe the batch, get
        // a guard sized to it.
        let a = admission(1024 * 1024);
        let before = a.available_bytes();
        let _held = a
            .reserve_prep_bytes(prep::prep_bytes(&PrepWorkload {
                memories: 4,
                memory_text_bytes: 1_000,
                neighbours_per_memory: 10,
                neighbour_text_bytes: 1_000,
                summary_bytes: 0,
                predicate_candidates: 100,
                embedding_dimensions: 384,
                namespaces: 1,
                vocabulary_bytes_per_namespace: 2_000,
            }))
            .await;
        assert!(
            a.available_bytes() < before,
            "a described batch must cost something"
        );
    }
}
