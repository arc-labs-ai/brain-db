//! How many threads provider HTTP gets on this machine.
//!
//! The provider runtime is one Tokio runtime for the whole process: every
//! shard's extractions and HyPE generations are spawned onto it. Its size used
//! to be a constant, which meant the same two threads on a laptop and on a
//! 128-core host — and, worse, two threads nobody had counted against the
//! shards. A shard owns a glommio executor and that executor owns a core, so a
//! machine sized at one shard per available core minus one was already running
//! more threads than it had cores the moment this runtime started.
//!
//! ## The rule
//!
//! Provider HTTP gets the cores the shard tier did not take.
//!
//! No thread in this process is pinned to a core: a shard's executor and a
//! Tokio worker are both schedulable anywhere. So a provider worker beyond the
//! unclaimed headroom is one more runnable thread contending for the cores the
//! shards are working on, and the scheduler settles that contention by
//! preempting somebody. Sizing to the headroom is what keeps provider HTTP out
//! of that contention in the common case.
//!
//! Spending the headroom and no more is affordable because this runtime is not
//! where the time goes. A provider call is seconds of waiting on a socket
//! around milliseconds of TLS and JSON decode; derivation measured ~40
//! memories a minute at fan-out 32 on one shard, so even every shard at once
//! leaves the decode work a few percent of a single core. Threads here buy
//! simultaneous *decode*, which nothing is short of; concurrency in flight is
//! bought by tasks, which are free.
//!
//! ## Deciding before the first call
//!
//! The runtime is built in a `OnceLock` on first use, so whatever size is in
//! force at that moment is the size for the process. [`reserve_for_shards`]
//! lets the server state the resolved shard count first — and because it
//! reports which number ended up in force, a caller that arrived too late
//! learns it rather than being silently ignored. When nobody calls it, the
//! size settles against this machine's own default shard layout, which is what
//! an embedder of this crate without a shard tier should get.

use std::sync::OnceLock;

/// The most threads provider HTTP is ever given, however large the machine.
///
/// Past a handful, extra workers buy simultaneous decode that no measured load
/// asks for, and each one costs a stack and a scheduler slot. A host with
/// hundreds of cores and a small pinned shard count would otherwise hand this
/// runtime a thread per spare core for work that uses a fraction of one.
const MAX_WORKER_THREADS: usize = 8;

/// The size in force for this process, once anything has settled it.
static WORKER_THREADS: OnceLock<usize> = OnceLock::new();

/// Cores this process may actually run on.
///
/// `available_parallelism` reads the cgroup quota on Linux, so a container
/// limited to two CPUs derives from two and not from the host's core count.
/// Reading `/proc/cpuinfo` instead would size every container to its host.
fn cores() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
}

/// Shards the server runs on a machine of `cores` when no operator pinned a
/// count — one per core, less one for the runtimes that are not a shard.
///
/// Mirrors `brain_server::config::shards_for_this_machine`. brain-llm cannot
/// depend on brain-server, and a server that knows its real shard count says
/// so through [`reserve_for_shards`]; this is the shape to assume when nobody
/// does.
fn default_shards(cores: usize) -> usize {
    cores.saturating_sub(1).max(1)
}

/// Threads for provider HTTP on a machine of `cores` already running `shards`.
///
/// The floor of 1 is load-bearing twice over: a machine whose shards claim
/// every core still has to be able to talk to a provider, and Tokio's
/// `worker_threads` panics on zero.
fn worker_threads_for(cores: usize, shards: usize) -> usize {
    cores.saturating_sub(shards).clamp(1, MAX_WORKER_THREADS)
}

/// Size provider HTTP for a process running `shards` shard threads, and report
/// the size in force.
///
/// `Ok` is the size this call chose. `Err` carries the size already settled,
/// which means something reached a provider first and this runtime is already
/// built — the caller should log it, because the process is then running a
/// size that was not sized against its shards.
///
/// Call it once the shard count is resolved and before any shard spawns.
///
/// # Errors
///
/// When the size was already settled, by an earlier call or by a provider call
/// that got there first.
pub fn reserve_for_shards(shards: usize) -> Result<usize, usize> {
    let threads = worker_threads_for(cores(), shards);
    match WORKER_THREADS.set(threads) {
        Ok(()) => Ok(threads),
        Err(_) => Err(*WORKER_THREADS.get().expect("set failed, so it is set")),
    }
}

/// The size to build the provider runtime at, settling it against this machine
/// if nobody has said.
pub(super) fn worker_threads() -> usize {
    *WORKER_THREADS.get_or_init(|| {
        let cores = cores();
        let threads = worker_threads_for(cores, default_shards(cores));
        tracing::debug!(
            target: "brain_llm::providers",
            cores,
            threads,
            "no shard count was reserved; sizing provider HTTP to this machine",
        );
        threads
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_layout_never_oversubscribes() {
        // The defect this module exists to prevent: shards and provider threads
        // sized independently exceed the core count on every machine with more
        // than two cores.
        for cores in 1..=256 {
            let shards = default_shards(cores);
            let threads = worker_threads_for(cores, shards);
            assert!(
                shards + threads <= cores.max(2),
                "{cores} cores: {shards} shards + {threads} provider threads",
            );
        }
    }

    #[test]
    fn a_ten_core_box_fits_exactly() {
        // 9 pinned shards leave one core, and that core is what provider HTTP
        // gets: 10 threads on 10 cores, not the 11 that `cores - 1` shards
        // beside a fixed provider-thread count would ask for.
        assert_eq!(default_shards(10), 9);
        assert_eq!(worker_threads_for(10, 9), 1);
    }

    #[test]
    fn one_core_still_gets_a_thread() {
        // Shards floor at 1, so there is no headroom at all. A runtime of zero
        // threads is not a degraded mode, it is a panic — and a machine this
        // small still has to be able to reach a provider.
        assert_eq!(default_shards(1), 1);
        assert_eq!(worker_threads_for(1, 1), 1);
    }

    #[test]
    fn shards_beyond_the_cores_still_leave_a_thread() {
        // An operator may pin more shards than the cgroup allows; the
        // subtraction must not wrap, and the floor still applies.
        assert_eq!(worker_threads_for(2, 16), 1);
    }

    #[test]
    fn a_huge_machine_does_not_get_a_huge_runtime() {
        // Spare cores are not a reason to hold stacks open for work that uses
        // a few percent of one core.
        assert_eq!(worker_threads_for(1024, 4), MAX_WORKER_THREADS);
    }

    #[test]
    fn headroom_is_spent_when_the_shards_leave_some() {
        // A pinned shard count below the machine's size hands the rest over,
        // up to the cap.
        assert_eq!(worker_threads_for(16, 12), 4);
    }

    #[test]
    fn the_size_is_decided_once() {
        // Whoever gets there first decides, and everyone after is told what is
        // in force rather than quietly ignored — which is how a late caller
        // can report that it lost.
        let in_force = reserve_for_shards(4).unwrap_or_else(|already| already);
        assert_eq!(reserve_for_shards(999).unwrap_err(), in_force);
        assert_eq!(worker_threads(), in_force);
    }
}
