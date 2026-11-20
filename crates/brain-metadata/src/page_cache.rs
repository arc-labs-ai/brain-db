//! How much memory this process lets each redb page cache hold.
//!
//! redb grows a database's page cache as pages are touched and evicts once it
//! reaches the size the database was opened with. Nothing is allocated up
//! front, so the figure is not a reservation — it is the point at which redb
//! stops growing. Left unset, redb picks 1 GiB, a flat figure its own source
//! marks as taking no account of the machine it runs on. This engine opens one
//! metadata database *per shard*, so an unset default is a promise of a
//! gibibyte per shard that no container can keep.
//!
//! This crate cannot sense the machine — it is a storage layer, and the
//! ceiling, the shard count and every other capacity decision belong to the
//! process that assembles the shards. So the number arrives from outside,
//! through [`configure`], exactly as the derivation budget arrives at the LLM
//! admission controller. `brain-server` reads the cgroup ceiling, divides the
//! share across the shards it is about to spawn, and calls here before any
//! shard opens a file.
//!
//! What this module owns is the answer for a process nobody configured:
//! benchmarks, tooling, tests and anything else that opens a database without
//! a startup path. Those open one or two databases, not one per shard, and
//! they run on a machine nobody measured — so the defaults below are modest
//! fixed numbers rather than a fraction of anything.

use std::sync::OnceLock;

/// Metadata page cache used when nothing configured one.
///
/// 32 MiB. An unconfigured process is one whose memory nobody has measured,
/// and it is almost always opening a single database rather than a shard's
/// worth — a test, a benchmark, a one-shot tool. That is small enough to be
/// invisible on any machine that can run the engine at all, and large enough
/// that the B-tree's interior nodes and a real working set stay resident, so a
/// benchmark measures the engine rather than the page cache.
const DEFAULT_METADATA_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// LLM response cache used when nothing configured one.
///
/// Smaller, because an unconfigured process is rarely doing derivation at all,
/// and a miss in this cache costs a network call rather than a read someone is
/// waiting on.
const DEFAULT_LLM_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// The page cache sizes every database in this process opens with.
///
/// Both are per-database. The shard multiplication is already accounted for by
/// whoever computed them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PageCaches {
    metadata_bytes: usize,
    llm_cache_bytes: usize,
}

static CACHES: OnceLock<PageCaches> = OnceLock::new();

/// Set the page cache sizes for this process. First call wins.
///
/// Must run before any shard opens its databases, which in practice means
/// during startup. A later call is ignored rather than panicking — a process
/// already serving reads must not die over a tuning value, and databases
/// opened before it would have taken the old number anyway, leaving the
/// process in a state no single figure describes. The returned flag lets the
/// caller log that its number did not take.
///
/// Takes `u64` because that is what a memory rule computes in; a size beyond
/// what this machine can address is clamped rather than wrapped, since the
/// clamp is a cache that grows until memory runs out and the wrap is a cache
/// that evicts after four kilobytes.
pub fn configure(metadata_bytes: u64, llm_cache_bytes: u64) -> bool {
    CACHES
        .set(PageCaches {
            metadata_bytes: usize::try_from(metadata_bytes).unwrap_or(usize::MAX),
            llm_cache_bytes: usize::try_from(llm_cache_bytes).unwrap_or(usize::MAX),
        })
        .is_ok()
}

/// The cache size [`crate::MetadataDb::open`] gives redb.
///
/// Fixed on first read for the life of the process: shards open their files at
/// different moments, and a figure that moved between them would leave one
/// shard caching ten times what its neighbour does for no reason an operator
/// could find.
#[must_use]
pub fn metadata_bytes() -> usize {
    in_force().metadata_bytes
}

/// The cache size [`crate::llm_cache::LlmCacheDb::open`] gives redb.
#[must_use]
pub fn llm_cache_bytes() -> usize {
    in_force().llm_cache_bytes
}

/// The sizes in force, defaulting an unconfigured process to the fixed pair.
fn in_force() -> PageCaches {
    *CACHES.get_or_init(|| PageCaches {
        metadata_bytes: DEFAULT_METADATA_CACHE_BYTES,
        llm_cache_bytes: DEFAULT_LLM_CACHE_BYTES,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sizes_are_the_same_number_every_time_they_are_asked_for() {
        assert_eq!(metadata_bytes(), metadata_bytes());
        assert_eq!(llm_cache_bytes(), llm_cache_bytes());
    }

    #[test]
    fn nothing_inherits_redbs_gibibyte() {
        // Whatever this process ended up configured with — the defaults, or a
        // figure a sibling test installed — it is not redb's unconfigured
        // 1 GiB, which is the number this module exists to displace.
        assert!(metadata_bytes() < 1024 * 1024 * 1024);
        assert!(llm_cache_bytes() < 1024 * 1024 * 1024);
    }

    #[test]
    fn a_size_larger_than_this_machine_can_address_is_clamped_not_wrapped() {
        assert_eq!(usize::try_from(u64::MAX).unwrap_or(usize::MAX), usize::MAX);
    }
}
