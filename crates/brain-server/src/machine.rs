//! How much memory this process may use.
//!
//! Every other capacity knob in the engine is a number an operator typed.
//! This module is the one that asks the machine instead, so the rest can be
//! derived from a real ceiling rather than from a guess that happens to be
//! right on a laptop and wrong in a 512 MiB container.
//!
//! The ceiling is whatever binds first: a cgroup limit if the process is in a
//! limited cgroup, otherwise what the host can actually hand out. A cgroup
//! limit is a *cap*, not a reservation — a container limited to 8 GiB on a
//! host with 1 GiB free still cannot get 8 GiB — so when both are known the
//! smaller one wins.
//!
//! Sensing can fail: an unreadable `/proc`, a hostile sandbox, a non-Linux
//! build. A failed reading is reported as `None` rather than smoothed over
//! with a plausible number, and the budgets below fall back to a floor small
//! enough to be safe anywhere. Pretending to know is the one answer that
//! cannot be recovered from.
//!
//! What the budget bounds is derivation's *in-flight* bytes — prompt, response
//! schema and response buffer held while an LLM call is outstanding — not
//! stored data. Stored data is cheap by comparison (a memory is ~82 KB on disk
//! and 7 vectors of 384 f32s, ~10 KB resident) and is already bounded by the
//! arena and the index configuration.

use std::sync::OnceLock;

/// The share of the sensed ceiling that derivation may hold in flight.
///
/// One tenth. The process is not an LLM client with a database attached: the
/// HNSW graphs, the write arena, the metadata cache and the request path all
/// live in the same address space and all of them are *durable* holders, while
/// in-flight derivation payloads are transient. Transient work that can be
/// queued must never be the reason a durable structure cannot grow, so it gets
/// the small share. Ten percent of 512 MiB is ~51 MiB, which at the few
/// hundred KB a prompt-plus-response occupies still admits dozens of
/// concurrent derivations — the fraction constrains a pathological burst, not
/// normal operation.
pub const DERIVATION_MEMORY_PERCENT: u64 = 10;

/// The smallest derivation budget this server will operate with.
///
/// Two cases land here: sensing failed entirely, and the sensed ceiling is so
/// small that a tenth of it rounds towards nothing. In both, a budget of zero
/// would stall derivation permanently with no way for an operator to see why,
/// which is strictly worse than admitting a few requests under memory
/// pressure. 8 MiB holds a handful of concurrent prompt-and-response pairs.
pub const MIN_DERIVATION_BUDGET_BYTES: u64 = 8 * 1024 * 1024;

/// The minimum locked-memory limit for a shard to start.
///
/// This is the limit glommio's io_uring reactor checks before it will create
/// itself. The figure is fixed rather than derived from the shard count or the
/// ring size, and a process under it cannot bring up a single shard.
pub const MIN_LOCKED_MEMORY_BYTES: u64 = 512 * 1024;

/// The share of the sensed ceiling the metadata page caches may occupy, summed
/// over every shard.
///
/// Fifteen percent — half as much again as derivation's tenth, and for the
/// opposite reason. A redb page cache is not a reservation: redb allocates
/// nothing at open and grows the cache as pages are touched, so this figure is
/// the point at which eviction starts rather than a block of memory taken out
/// of circulation. On a machine larger than the data it will never be reached;
/// on a machine smaller than the data it is the only thing that stops the
/// cache growing until the kernel intervenes. That asymmetry — costs nothing
/// when generous, saves the process when tight — is what earns it the larger
/// share. Derivation's tenth has to be reserved whether or not it is used,
/// because admission hands it out in advance.
///
/// Fifteen percent is the *process* allowance; each shard opens its own
/// metadata database and receives this divided by the shard count. Nine shards
/// in a 512 MiB container get ~8.5 MiB each, which is what redb's unconfigured
/// 1 GiB per shard was really asking the kernel for.
pub const METADATA_CACHE_MEMORY_PERCENT: u64 = 15;

/// The smallest metadata page cache a shard will run with.
///
/// A cache below the B-tree's interior nodes turns every lookup into a file
/// read, and the metadata database is on the path of every read and every
/// write — a shard that reached that state would look hung rather than slow.
/// 4 MiB is a thousand 4 KiB pages: enough for the upper levels of all
/// thirteen tables plus a working set of leaves.
///
/// Reached on a machine too small for the fraction to clear it, and on one
/// that would not say how large it is. The floor wins there, so a 256 MiB box
/// running nine shards spends ~36 MiB on metadata caches rather than the
/// ~38 MiB the fraction would allow — the overshoot against the fraction is
/// deliberate, because the alternative is a cache that cannot do its job.
pub const MIN_METADATA_CACHE_BYTES: u64 = 4 * 1024 * 1024;

/// The share of the sensed ceiling the LLM response caches may occupy, summed
/// over every shard.
///
/// A third of what the metadata cache gets. This database is consulted once per
/// derivation and a miss costs an LLM round trip already measured in seconds,
/// so the marginal value of a larger cache is small where the metadata cache's
/// is large. It is sized at all only because it is the *other* database every
/// shard opens, and an unsized one beside a sized one would leave the
/// per-shard multiplication only half fixed.
pub const LLM_CACHE_MEMORY_PERCENT: u64 = 5;

/// The smallest LLM response cache a shard will run with.
///
/// Smaller floor for the same reason as the smaller share: a miss here is
/// absorbed by a network call, not paid in a read the caller is waiting on.
pub const MIN_LLM_CACHE_BYTES: u64 = 1024 * 1024;

/// The three machine-derived memory shares cannot promise more than the
/// machine has.
///
/// Thirty percent in total, leaving the HNSW graphs, the write arena, the
/// embedding weights (~130 MiB, loaded once for the whole process), the
/// request path and the allocator's own slack the remaining seventy. The
/// assertion is here so that raising any one share has to be argued against
/// the others at compile time rather than discovered under memory pressure.
const _: () = assert!(
    DERIVATION_MEMORY_PERCENT + METADATA_CACHE_MEMORY_PERCENT + LLM_CACHE_MEMORY_PERCENT <= 50,
    "the machine-derived shares must leave the majority of memory to the \
     indexes, the arena, the model weights and the request path"
);

/// Memory this process may use, in bytes, or `None` if the machine would not
/// say.
///
/// A live reading, not a cached one: `MemAvailable` moves with the rest of the
/// host and a caller asking a second time wants the second answer. Callers
/// that need a number to stay still for the life of the process should derive
/// it once — [`derivation_memory_budget_bytes`] does exactly that.
#[must_use]
pub fn available_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        linux::sense()
    }
    #[cfg(not(target_os = "linux"))]
    {
        // The engine runs on Linux; other hosts build the portable half of the
        // crate for tooling and tests. Nothing here can sense them honestly,
        // so nothing here claims to.
        None
    }
}

/// The soft locked-memory limit for this process, in bytes, or `None` if the
/// machine would not say.
#[must_use]
pub fn locked_memory_limit_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        linux::locked_memory_limit_bytes()
    }
    #[cfg(not(target_os = "linux"))]
    {
        // The engine runs on Linux; other hosts build the portable half of the
        // crate for tooling and tests. Nothing here can sense them honestly,
        // so nothing here claims to.
        None
    }
}

/// The share of available memory derivation may hold in flight at once.
///
/// Sensed once and then fixed for the life of the process. Admission control
/// compares against this on every request; a bound that drifted with host
/// memory pressure would make the same request admissible and then not, which
/// is indistinguishable from a bug from the outside.
#[must_use]
pub fn derivation_memory_budget_bytes() -> u64 {
    static BUDGET: OnceLock<u64> = OnceLock::new();
    *BUDGET.get_or_init(|| derivation_budget_for(available_memory_bytes()))
}

/// The budget that a given ceiling implies.
///
/// Separated from the sensing so a caller holding its own number — an operator
/// override, a figure from a different machine — can apply the same rule, and
/// so the rule itself is testable without a machine that exhibits it.
#[must_use]
pub fn derivation_budget_for(available_bytes: Option<u64>) -> u64 {
    available_bytes
        // Divide before multiplying: the product of a plausible byte count and
        // a percentage cannot overflow, but the rule should not depend on that
        // being true of every future percentage. The cost is under 100 bytes.
        .map_or(0, |bytes| (bytes / 100).saturating_mul(DERIVATION_MEMORY_PERCENT))
        .max(MIN_DERIVATION_BUDGET_BYTES)
}

/// The metadata page cache one shard may hold, given a ceiling and how many
/// shards divide it.
///
/// Every shard opens its own metadata database, so the figure an operator
/// cares about is the product of this and the shard count. Taking the division
/// here rather than at the call site keeps the promise the process makes
/// visible in one expression: whatever the shard count, the shards together
/// ask for the fraction and no more.
#[must_use]
pub fn metadata_cache_for(available_bytes: Option<u64>, shards: usize) -> u64 {
    per_shard_share(
        available_bytes,
        shards,
        METADATA_CACHE_MEMORY_PERCENT,
        MIN_METADATA_CACHE_BYTES,
    )
}

/// The LLM response cache one shard may hold, on the same terms.
#[must_use]
pub fn llm_cache_for(available_bytes: Option<u64>, shards: usize) -> u64 {
    per_shard_share(
        available_bytes,
        shards,
        LLM_CACHE_MEMORY_PERCENT,
        MIN_LLM_CACHE_BYTES,
    )
}

/// One shard's slice of a process-wide share, never below `floor`.
///
/// An unknown ceiling lands on the floor rather than on a guess. redb's own
/// default for an unconfigured cache is 1 GiB, which is the bug this rule
/// exists to remove, not a fallback worth inheriting: a process that could not
/// read its own cgroup is the *least* entitled to assume a gigabyte, since not
/// being able to sense the machine is itself what a hostile sandbox looks
/// like. The two failure modes are not symmetric — a cache that is too small
/// costs latency an operator can measure and correct, while one that is too
/// large costs an OOM kill that reads as a crash — so the unknown case takes
/// the loss that can be recovered from.
fn per_shard_share(available_bytes: Option<u64>, shards: usize, percent: u64, floor: u64) -> u64 {
    // A shard count of zero is not a configuration anyone can run, but it is a
    // number this function can be handed, and dividing by it would abort the
    // process at the one moment it is trying to be careful about resources.
    let shards = u64::try_from(shards).unwrap_or(u64::MAX).max(1);
    available_bytes
        // Divide before multiplying, as the derivation budget does, so the
        // rule does not rely on every future percentage being small enough for
        // the product to fit.
        .map_or(0, |bytes| (bytes / 100).saturating_mul(percent))
        .div_euclid(shards)
        .max(floor)
}

/// The environment variable that decides how wide one embedding forward pass
/// fans out.
///
/// Rayon and candle both read this name, and that shared reading is the whole
/// mechanism: candle's `get_num_threads` consults it and otherwise falls back
/// to the host's core count, and it exposes no setter, so an in-process call
/// cannot narrow candle's matmul split any other way. A rayon pool built
/// without an explicit width reads the same name. Setting it once therefore
/// aims both consumers at one number instead of leaving each to guess from the
/// machine.
const EMBED_THREADS_ENV: &str = "RAYON_NUM_THREADS";

/// Settle how wide one embedding forward pass may fan out, for this process.
///
/// Returns the width it settled on, so the caller can log what the process
/// will run with.
///
/// Must be called before the embedding model loads and before any shard
/// spawns. Two things read the variable after this point and neither re-reads
/// it: a rayon pool fixes its width when it is built, and the per-shard
/// intra-op pool is built on first use inside a shard. Writing it later would
/// leave pools already built at the host's core count. It is also the only
/// write to the process environment, and it happens while the process is still
/// single-threaded, so no thread can be reading the variable as it changes.
pub fn install_embed_thread_width(shards: usize) -> usize {
    let width = embed_thread_width_for(
        std::thread::available_parallelism()
            .ok()
            .map(std::num::NonZeroUsize::get),
        shards,
    );
    std::env::set_var(EMBED_THREADS_ENV, width.to_string());
    width
}

/// How many threads one embedding forward pass may use, given the cores this
/// machine has and the shards that divide them.
///
/// A shard embeds on its own blocking thread while every other shard does the
/// same, so the cores a single forward pass may claim are the machine's cores
/// divided by the shards competing for them — not the machine's cores, which
/// is what both rayon and candle assume when nobody tells them otherwise.
///
/// Measured on 10 cores, one shard, ENCODE through the real dispatch path, as
/// the rayon pool width and candle's matmul split were varied independently:
/// 10 and 10 gave a p50 of 46.0 ms, 10 and 1 gave 26.4 ms, 1 and 10 gave
/// 30.2 ms, and 1 and 1 gave 20.7 ms. Narrowing either half alone leaves most
/// of the difference unclaimed, which is why one variable that both halves
/// read is the mechanism rather than two settings that can disagree.
///
/// The result tracks the deployment, and is not a figure that holds
/// everywhere: it is 1 at the default shard count of one per core less one,
/// and 5 for two shards on ten cores.
///
/// A machine whose core count cannot be read lands on 1. Of the two ways to be
/// wrong, a pass that is narrower than the machine could afford costs latency
/// that shows up in a measurement, while one that is wider than the shards
/// allow costs the oversubscription this rule exists to remove.
#[must_use]
pub fn embed_thread_width_for(cores: Option<usize>, shards: usize) -> usize {
    // A shard count of zero is not something anyone can run, but it is a
    // number this function can be handed, and dividing by it would abort the
    // process at the moment it is trying to be careful about cores.
    cores
        .map_or(1, |cores| cores.div_euclid(shards.max(1)))
        .max(1)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Where the cgroup hierarchy is mounted. Inside a container with its own
    /// cgroup namespace this *is* the container's own cgroup, which is why
    /// `/sys/fs/cgroup/memory.max` is the limit a container reads directly.
    const CGROUP_ROOT: &str = "/sys/fs/cgroup";

    /// Holds `self/cgroup` (which cgroup this process is in) and `meminfo`
    /// (what the host has left). Both are read through this one path so tests
    /// can supply a directory that looks like either.
    const PROC_ROOT: &str = "/proc";

    /// cgroup v2: one file per cgroup, `max` or a byte count.
    const V2_LIMIT_FILE: &str = "memory.max";

    /// cgroup v1: the memory controller is a separate hierarchy.
    const V1_CONTROLLER_DIR: &str = "memory";
    const V1_LIMIT_FILE: &str = "memory.limit_in_bytes";

    /// At or above this, a cgroup limit is a sentinel for "no limit".
    ///
    /// cgroup v1 spells unlimited as `i64::MAX` rounded down to a page
    /// boundary: `9223372036854771712` on a 4 KiB-page kernel, a different
    /// number on a 64 KiB-page one, and `u64::MAX` on some. Rather than
    /// enumerate kernels, anything at or above 4 EiB is read as a sentinel —
    /// no machine has four exabytes of RAM, so such a value cannot be a real
    /// limit, and treating it as one would hand out an absurd budget.
    const UNLIMITED_AT_OR_ABOVE_BYTES: u64 = 1 << 62;

    /// How a cgroup limit file reads.
    #[derive(Debug, PartialEq, Eq)]
    enum Limit {
        /// A real ceiling, in bytes.
        Bytes(u64),
        /// The cgroup imposes none. This is an answer, not a failure: `max`
        /// and the v1 sentinel both mean "ask someone else", and a reader that
        /// treated them as parse errors would be right by accident and wrong
        /// as soon as it logged why.
        Unlimited,
        /// The file was not there or did not parse. Also "ask someone else",
        /// but for a reason worth distinguishing when reasoning about a
        /// machine that answers unexpectedly.
        Unknown,
    }

    /// The ceiling this machine imposes, as the public sensor reports it.
    pub(super) fn sense() -> Option<u64> {
        sense_at(Path::new(CGROUP_ROOT), Path::new(PROC_ROOT))
    }

    /// The soft locked-memory limit published for this process by Linux.
    pub(super) fn locked_memory_limit_bytes() -> Option<u64> {
        locked_memory_limit_at(Path::new(PROC_ROOT).join("self/limits").as_path())
    }

    /// Read and parse a process limits file at an injectable path.
    fn locked_memory_limit_at(path: &Path) -> Option<u64> {
        fs::read_to_string(path)
            .ok()
            .and_then(|limits| parse_locked_memory_limit(&limits))
    }

    /// Parse the soft `Max locked memory` value from `/proc/self/limits`.
    fn parse_locked_memory_limit(limits: &str) -> Option<u64> {
        limits.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            if fields.next()? != "Max" || fields.next()? != "locked" || fields.next()? != "memory" {
                return None;
            }
            match fields.next()? {
                "unlimited" => Some(u64::MAX),
                value => value.parse::<u64>().ok(),
            }
        })
    }

    /// The ceiling implied by a cgroup tree and a `/proc` tree.
    ///
    /// Both limits are advisory on their own and binding together: the cgroup
    /// says how much this process is allowed, the host says how much exists to
    /// be allowed. Whichever is smaller is what a large allocation will
    /// actually hit.
    fn sense_at(cgroup_root: &Path, proc_root: &Path) -> Option<u64> {
        let self_cgroup = fs::read_to_string(proc_root.join("self/cgroup")).unwrap_or_default();
        let cgroup_limit = cgroup_v2_limit(cgroup_root, &self_cgroup)
            .or_else(|| cgroup_v1_limit(cgroup_root, &self_cgroup));

        let host_available = fs::read_to_string(proc_root.join("meminfo"))
            .ok()
            .and_then(|meminfo| parse_mem_available_bytes(&meminfo));

        match (cgroup_limit, host_available) {
            (Some(limit), Some(available)) => Some(limit.min(available)),
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        }
    }

    /// The effective cgroup v2 limit for this process.
    ///
    /// A limit set on a parent binds every descendant, so the effective
    /// ceiling is the smallest limit anywhere on the chain from this
    /// process's own cgroup up to the mount root — not the leaf's own value.
    /// The leaf saying `max` is therefore not the end of the question, and
    /// neither is the leaf saying a number: both cases walk the whole chain.
    /// This also covers the container case from the other direction, where the
    /// mount root *is* the container's cgroup and the walk is one step long.
    fn cgroup_v2_limit(cgroup_root: &Path, self_cgroup: &str) -> Option<u64> {
        let relative = v2_relative_path(self_cgroup);
        min_limit_up_the_chain(cgroup_root, &relative, V2_LIMIT_FILE)
    }

    /// The effective cgroup v1 memory limit for this process.
    ///
    /// Same chain rule as v2. The memory controller is mounted under its own
    /// directory, and a process not in a v1 memory cgroup simply has no such
    /// files to read.
    fn cgroup_v1_limit(cgroup_root: &Path, self_cgroup: &str) -> Option<u64> {
        let controller_root = cgroup_root.join(V1_CONTROLLER_DIR);
        let relative = v1_relative_path(self_cgroup).unwrap_or_else(|| "/".to_string());
        min_limit_up_the_chain(&controller_root, &relative, V1_LIMIT_FILE)
    }

    /// The smallest real limit found between `root/relative` and `root`.
    ///
    /// Missing files and unlimited values are skipped rather than ending the
    /// walk: an intermediate cgroup with no limit of its own sits between two
    /// that have one, and the outer one still binds. `None` means nothing on
    /// the chain imposed a ceiling.
    fn min_limit_up_the_chain(root: &Path, relative: &str, file: &str) -> Option<u64> {
        let mut current = join_under(root, relative);
        let mut smallest: Option<u64> = None;

        loop {
            if let Limit::Bytes(bytes) = read_limit(&current.join(file)) {
                smallest = Some(smallest.map_or(bytes, |seen: u64| seen.min(bytes)));
            }
            if current == root {
                break;
            }
            match current.parent() {
                // Stop at the mount root. Anything above it belongs to a
                // hierarchy this process is not part of.
                Some(parent) if parent.starts_with(root) => current = parent.to_path_buf(),
                _ => break,
            }
        }

        smallest
    }

    /// `root` joined with a cgroup path, which is always absolute in `/proc`.
    ///
    /// `Path::join` with an absolute argument discards the left side, which
    /// would silently read the host's `/` instead of the mount root, so the
    /// leading separator is stripped first. `..` components are dropped for
    /// the same reason: a cgroup path never contains one, and honouring it
    /// would walk out of the hierarchy.
    fn join_under(root: &Path, relative: &str) -> PathBuf {
        let mut path = root.to_path_buf();
        for part in relative.split('/') {
            if !part.is_empty() && part != "." && part != ".." {
                path.push(part);
            }
        }
        path
    }

    /// This process's cgroup path from a `/proc/self/cgroup` body.
    ///
    /// The v2 entry is the one with an empty hierarchy id and no controller:
    /// `0::/some/path`. A container with its own cgroup namespace reports
    /// `0::/`, which is correct — relative to its mount root, it is at the
    /// root.
    fn v2_relative_path(self_cgroup: &str) -> String {
        self_cgroup
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .unwrap_or("/")
            .trim()
            .to_string()
    }

    /// This process's memory-controller path from a `/proc/self/cgroup` body.
    ///
    /// v1 entries are `<id>:<controllers>:<path>` and the memory controller
    /// may be co-mounted with others, so the controller field is a
    /// comma-separated list that has to be searched rather than compared.
    fn v1_relative_path(self_cgroup: &str) -> Option<String> {
        self_cgroup.lines().find_map(|line| {
            let mut fields = line.splitn(3, ':');
            let _id = fields.next()?;
            let controllers = fields.next()?;
            let path = fields.next()?;
            controllers
                .split(',')
                .any(|c| c == V1_CONTROLLER_DIR)
                .then(|| path.trim().to_string())
        })
    }

    /// Read one cgroup limit file.
    fn read_limit(path: &Path) -> Limit {
        match fs::read_to_string(path) {
            Ok(raw) => parse_limit(&raw),
            Err(_) => Limit::Unknown,
        }
    }

    /// Interpret the contents of a cgroup limit file.
    fn parse_limit(raw: &str) -> Limit {
        let text = raw.trim();
        if text == "max" {
            return Limit::Unlimited;
        }
        match text.parse::<u64>() {
            Ok(bytes) if bytes >= UNLIMITED_AT_OR_ABOVE_BYTES => Limit::Unlimited,
            // Zero is a real setting (nothing may be charged to this cgroup)
            // but as a ceiling it is unusable, and a cgroup that truly had it
            // would have killed this process before it could read the file.
            // Treating it as no answer lets the host reading stand.
            Ok(0) => Limit::Unknown,
            Ok(bytes) => Limit::Bytes(bytes),
            Err(_) => Limit::Unknown,
        }
    }

    /// `MemAvailable` from a `/proc/meminfo` body, in bytes.
    ///
    /// `MemAvailable`, not `MemTotal`: total counts memory already spoken for
    /// by other processes, and sizing to it is how a server ends up asking for
    /// memory that exists only on paper. The kernel reports it in kB — the
    /// file's own spelling for KiB — so the value is scaled by 1024.
    ///
    /// Kernels before 3.14 do not publish the field. `None` there, so the
    /// caller falls back rather than silently sizing to something else.
    fn parse_mem_available_bytes(meminfo: &str) -> Option<u64> {
        meminfo
            .lines()
            .find_map(|line| line.strip_prefix("MemAvailable:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|kib| kib.saturating_mul(1024))
    }

    #[cfg(test)]
    mod tests {
        use std::fs;

        use tempfile::TempDir;

        use super::*;

        /// Build a cgroup-shaped directory tree: each entry is a path
        /// relative to the root and the contents of a file at that path.
        fn tree(files: &[(&str, &str)]) -> TempDir {
            let dir = TempDir::new().expect("tempdir");
            for (relative, contents) in files {
                let path = dir.path().join(relative);
                fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
                fs::write(&path, contents).expect("write");
            }
            dir
        }

        #[test]
        fn locked_memory_limit_parses_bytes() {
            assert_eq!(
                parse_locked_memory_limit("Max locked memory 524288 524288 bytes\n"),
                Some(524288)
            );
        }

        #[test]
        fn locked_memory_limit_parses_unlimited() {
            assert_eq!(
                parse_locked_memory_limit("Max locked memory unlimited unlimited bytes\n"),
                Some(u64::MAX)
            );
        }

        #[test]
        fn locked_memory_limit_missing_row_is_unknown() {
            assert_eq!(
                parse_locked_memory_limit("Max open files 1024 1024 files\n"),
                None
            );
        }

        #[test]
        fn locked_memory_limit_malformed_row_is_unknown() {
            assert_eq!(
                parse_locked_memory_limit("Max locked memory not-a-number 524288 bytes\n"),
                None
            );
        }

        #[test]
        fn locked_memory_limit_ignores_rows_before_target() {
            let limits = concat!(
                "Limit  Soft Limit  Hard Limit  Units\n",
                "Max open files 1024 1024 files\n",
                "Max processes 256 256 processes\n",
                "Max locked memory 524288 1048576 bytes\n",
            );

            assert_eq!(parse_locked_memory_limit(limits), Some(524288));
        }

        #[test]
        fn the_literal_max_is_unlimited_not_a_parse_failure() {
            assert_eq!(parse_limit("max\n"), Limit::Unlimited);
            assert_eq!(parse_limit("  max  "), Limit::Unlimited);
        }

        #[test]
        fn the_v1_unlimited_sentinel_is_not_a_real_limit() {
            // i64::MAX rounded down to a 4 KiB page, the usual spelling.
            assert_eq!(parse_limit("9223372036854771712"), Limit::Unlimited);
            // The same idea on a 64 KiB-page kernel.
            assert_eq!(parse_limit("9223372036854710272"), Limit::Unlimited);
            assert_eq!(parse_limit(&u64::MAX.to_string()), Limit::Unlimited);
            assert_eq!(parse_limit(&i64::MAX.to_string()), Limit::Unlimited);
        }

        #[test]
        fn a_plausible_limit_is_taken_at_face_value() {
            assert_eq!(parse_limit("536870912"), Limit::Bytes(536_870_912));
            // One byte under the sentinel floor is still nonsense as a real
            // machine, but the rule is a threshold and the threshold is where
            // it is on purpose.
            let below = UNLIMITED_AT_OR_ABOVE_BYTES - 1;
            assert_eq!(parse_limit(&below.to_string()), Limit::Bytes(below));
        }

        #[test]
        fn unreadable_contents_are_unknown_not_zero() {
            assert_eq!(parse_limit(""), Limit::Unknown);
            assert_eq!(parse_limit("garbage"), Limit::Unknown);
            assert_eq!(parse_limit("-1"), Limit::Unknown);
            assert_eq!(parse_limit("0"), Limit::Unknown);
        }

        #[test]
        fn a_container_reads_the_limit_at_its_own_mount_root() {
            // What a cgroup-namespaced container sees: it is at `/`, and the
            // limit file sits directly under the mount.
            let cgroup = tree(&[("memory.max", "536870912\n")]);
            assert_eq!(
                cgroup_v2_limit(cgroup.path(), "0::/\n"),
                Some(536_870_912),
                "512 MiB container limit"
            );
        }

        #[test]
        fn a_leaf_saying_max_yields_to_a_parent_that_does_not() {
            let cgroup = tree(&[
                ("memory.max", "max\n"),
                ("system.slice/memory.max", "2147483648\n"),
                ("system.slice/brain.service/memory.max", "max\n"),
            ]);
            assert_eq!(
                cgroup_v2_limit(cgroup.path(), "0::/system.slice/brain.service\n"),
                Some(2_147_483_648),
                "the parent's limit binds the leaf that set none"
            );
        }

        #[test]
        fn the_smallest_limit_on_the_chain_wins_even_when_the_leaf_has_one() {
            let cgroup = tree(&[
                ("memory.max", "max\n"),
                ("pod/memory.max", "268435456\n"),
                ("pod/brain/memory.max", "4294967296\n"),
            ]);
            assert_eq!(
                cgroup_v2_limit(cgroup.path(), "0::/pod/brain\n"),
                Some(268_435_456),
                "a leaf cannot grant itself more than its parent allows"
            );
        }

        #[test]
        fn an_all_unlimited_chain_imposes_nothing() {
            let cgroup = tree(&[("memory.max", "max\n"), ("pod/memory.max", "max\n")]);
            assert_eq!(cgroup_v2_limit(cgroup.path(), "0::/pod\n"), None);
        }

        #[test]
        fn a_missing_hierarchy_imposes_nothing() {
            let empty = TempDir::new().expect("tempdir");
            assert_eq!(cgroup_v2_limit(empty.path(), "0::/\n"), None);
            assert_eq!(cgroup_v1_limit(empty.path(), ""), None);
        }

        #[test]
        fn cgroup_v1_reads_the_memory_controller_hierarchy() {
            let cgroup = tree(&[("memory/docker/abc/memory.limit_in_bytes", "1073741824\n")]);
            let self_cgroup = "9:cpu,cpuacct:/docker/abc\n4:memory:/docker/abc\n";
            assert_eq!(
                cgroup_v1_limit(cgroup.path(), self_cgroup),
                Some(1_073_741_824)
            );
        }

        #[test]
        fn cgroup_v1_sentinel_falls_through_to_nothing() {
            let cgroup = tree(&[
                ("memory/memory.limit_in_bytes", "9223372036854771712\n"),
                (
                    "memory/docker/abc/memory.limit_in_bytes",
                    "9223372036854771712\n",
                ),
            ]);
            assert_eq!(
                cgroup_v1_limit(cgroup.path(), "4:memory:/docker/abc\n"),
                None
            );
        }

        #[test]
        fn the_memory_controller_is_found_when_co_mounted() {
            let self_cgroup = "3:cpuset,memory,blkio:/limited\n";
            assert_eq!(v1_relative_path(self_cgroup).as_deref(), Some("/limited"));
        }

        #[test]
        fn a_process_outside_the_memory_controller_has_no_v1_path() {
            assert_eq!(v1_relative_path("5:cpu:/some/path\n"), None);
            assert_eq!(v1_relative_path("0::/\n"), None);
        }

        #[test]
        fn the_v2_path_defaults_to_the_root_when_absent() {
            assert_eq!(v2_relative_path(""), "/");
            assert_eq!(v2_relative_path("4:memory:/docker/abc\n"), "/");
            assert_eq!(v2_relative_path("0::/pod/brain\n"), "/pod/brain");
        }

        #[test]
        fn an_absolute_cgroup_path_stays_under_the_mount_root() {
            let root = Path::new("/sys/fs/cgroup");
            assert_eq!(
                join_under(root, "/system.slice/brain.service"),
                Path::new("/sys/fs/cgroup/system.slice/brain.service")
            );
            assert_eq!(join_under(root, "/"), root);
            assert_eq!(
                join_under(root, "/../../etc"),
                Path::new("/sys/fs/cgroup/etc"),
                "a traversal component cannot escape the hierarchy"
            );
        }

        #[test]
        fn mem_available_is_read_in_kib_and_mem_total_is_ignored() {
            let meminfo = "MemTotal:       16384000 kB\nMemAvailable:    1048576 kB\n";
            assert_eq!(
                parse_mem_available_bytes(meminfo),
                Some(1_073_741_824),
                "1 GiB available, not the 16 GB total"
            );
        }

        #[test]
        fn a_meminfo_without_mem_available_says_nothing() {
            assert_eq!(parse_mem_available_bytes("MemTotal: 16384000 kB\n"), None);
            assert_eq!(parse_mem_available_bytes(""), None);
        }

        #[test]
        fn a_cgroup_limit_above_what_the_host_has_is_capped_by_the_host() {
            let cgroup = tree(&[("memory.max", "8589934592\n")]);
            let proc_root = tree(&[
                ("self/cgroup", "0::/\n"),
                (
                    "meminfo",
                    "MemTotal: 16384000 kB\nMemAvailable: 1048576 kB\n",
                ),
            ]);
            assert_eq!(
                sense_at(cgroup.path(), proc_root.path()),
                Some(1_073_741_824),
                "an 8 GiB allowance on a host with 1 GiB free is still 1 GiB"
            );
        }

        #[test]
        fn a_cgroup_limit_below_what_the_host_has_binds() {
            let cgroup = tree(&[("memory.max", "268435456\n")]);
            let proc_root = tree(&[
                ("self/cgroup", "0::/\n"),
                ("meminfo", "MemAvailable: 8388608 kB\n"),
            ]);
            assert_eq!(
                sense_at(cgroup.path(), proc_root.path()),
                Some(268_435_456),
                "the container's 256 MiB, not the host's 8 GiB"
            );
        }

        #[test]
        fn an_unlimited_cgroup_falls_through_to_the_host() {
            let cgroup = tree(&[("memory.max", "max\n")]);
            let proc_root = tree(&[
                ("self/cgroup", "0::/\n"),
                ("meminfo", "MemAvailable: 2097152 kB\n"),
            ]);
            assert_eq!(
                sense_at(cgroup.path(), proc_root.path()),
                Some(2_147_483_648)
            );
        }

        #[test]
        fn a_machine_that_answers_nothing_is_reported_as_unknown() {
            let nothing = TempDir::new().expect("tempdir");
            let also_nothing = TempDir::new().expect("tempdir");
            assert_eq!(sense_at(nothing.path(), also_nothing.path()), None);
        }

        #[test]
        fn this_machine_answers() {
            // The build and test host is Linux, so sensing must succeed on it.
            // A `None` here means the resolution order missed a real machine,
            // which is the failure this module exists to prevent.
            let sensed = sense().expect("a Linux host reports its memory");
            assert!(
                sensed >= 1024 * 1024,
                "a machine running this test has at least a MiB: {sensed}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_is_a_tenth_of_what_was_sensed() {
        assert_eq!(
            derivation_budget_for(Some(8 * 1024 * 1024 * 1024)),
            858_993_450,
            "8 GiB sensed"
        );
        assert_eq!(derivation_budget_for(Some(512 * 1024 * 1024)), 53_687_090);
    }

    #[test]
    fn an_unknown_machine_gets_the_floor_rather_than_the_benefit_of_the_doubt() {
        assert_eq!(derivation_budget_for(None), MIN_DERIVATION_BUDGET_BYTES);
    }

    #[test]
    fn a_tiny_ceiling_still_yields_a_usable_budget() {
        // A tenth of 16 MiB is 1.6 MiB, below what one derivation needs; a
        // tenth of 50 bytes is zero, which would stall derivation forever.
        assert_eq!(
            derivation_budget_for(Some(16 * 1024 * 1024)),
            MIN_DERIVATION_BUDGET_BYTES
        );
        assert_eq!(derivation_budget_for(Some(50)), MIN_DERIVATION_BUDGET_BYTES);
        assert_eq!(derivation_budget_for(Some(0)), MIN_DERIVATION_BUDGET_BYTES);
    }

    #[test]
    fn an_implausible_ceiling_does_not_overflow_the_budget() {
        assert_eq!(
            derivation_budget_for(Some(u64::MAX)),
            (u64::MAX / 100) * DERIVATION_MEMORY_PERCENT
        );
    }

    #[test]
    fn the_budget_is_the_same_number_every_time_it_is_asked_for() {
        assert_eq!(
            derivation_memory_budget_bytes(),
            derivation_memory_budget_bytes()
        );
        assert!(derivation_memory_budget_bytes() >= MIN_DERIVATION_BUDGET_BYTES);
    }

    #[test]
    fn a_shard_gets_its_slice_of_the_metadata_share() {
        // 8 GiB sensed, four shards: 15% is ~1.2 GiB, a shard holds ~307 MiB,
        // and the four together still ask for the fraction and no more.
        let eight_gib = 8 * 1024 * 1024 * 1024;
        let per_shard = metadata_cache_for(Some(eight_gib), 4);
        assert_eq!(per_shard, 322_122_543);
        assert!(per_shard * 4 <= eight_gib / 100 * METADATA_CACHE_MEMORY_PERCENT);
    }

    #[test]
    fn one_shard_holds_the_whole_share() {
        let eight_gib = 8 * 1024 * 1024 * 1024;
        assert_eq!(
            metadata_cache_for(Some(eight_gib), 1),
            eight_gib / 100 * METADATA_CACHE_MEMORY_PERCENT
        );
    }

    #[test]
    fn a_high_shard_count_divides_a_modest_budget_down_to_the_floor() {
        // 2 GiB across 64 shards: 15% is ~307 MiB, a shard's slice is ~4.8
        // MiB and still clears the floor.
        let two_gib = 2 * 1024 * 1024 * 1024;
        assert_eq!(metadata_cache_for(Some(two_gib), 64), 5_033_164);
        // 128 shards takes the same budget under the floor, and the floor
        // wins: the shards now collectively promise more than the fraction,
        // which is the documented cost of refusing to run a useless cache.
        assert_eq!(
            metadata_cache_for(Some(two_gib), 128),
            MIN_METADATA_CACHE_BYTES
        );
    }

    #[test]
    fn a_small_container_lands_on_the_floor_rather_than_on_nothing() {
        // 256 MiB with nine shards: 15% is ~38 MiB, ~4.2 MiB a shard, just
        // over the floor. 128 MiB with the same shard count is under it.
        assert_eq!(metadata_cache_for(Some(256 * 1024 * 1024), 9), 4_473_923);
        assert_eq!(
            metadata_cache_for(Some(128 * 1024 * 1024), 9),
            MIN_METADATA_CACHE_BYTES
        );
        assert_eq!(metadata_cache_for(Some(0), 1), MIN_METADATA_CACHE_BYTES);
    }

    #[test]
    fn an_unknown_machine_gets_the_floor_not_redbs_gibibyte() {
        // redb's own unconfigured default is 1 GiB. A process that could not
        // read its own limits is the last one that should claim it.
        assert_eq!(metadata_cache_for(None, 1), MIN_METADATA_CACHE_BYTES);
        assert_eq!(metadata_cache_for(None, 64), MIN_METADATA_CACHE_BYTES);
        const { assert!(MIN_METADATA_CACHE_BYTES < 1024 * 1024 * 1024) };
        assert_eq!(llm_cache_for(None, 9), MIN_LLM_CACHE_BYTES);
    }

    #[test]
    fn a_shard_count_of_zero_does_not_divide_by_zero() {
        let eight_gib = 8 * 1024 * 1024 * 1024;
        assert_eq!(
            metadata_cache_for(Some(eight_gib), 0),
            metadata_cache_for(Some(eight_gib), 1),
            "an impossible shard count is read as the one shard that must exist"
        );
        assert_eq!(
            llm_cache_for(Some(eight_gib), 0),
            llm_cache_for(Some(eight_gib), 1)
        );
    }

    #[test]
    fn an_implausible_ceiling_does_not_overflow_the_cache_size() {
        assert_eq!(
            metadata_cache_for(Some(u64::MAX), 1),
            (u64::MAX / 100) * METADATA_CACHE_MEMORY_PERCENT
        );
        assert_eq!(
            metadata_cache_for(Some(u64::MAX), usize::MAX),
            MIN_METADATA_CACHE_BYTES,
            "an implausible shard count divides the share away, and the floor catches it"
        );
    }

    #[test]
    fn the_llm_response_cache_takes_the_smaller_share() {
        let eight_gib = 8 * 1024 * 1024 * 1024;
        assert!(llm_cache_for(Some(eight_gib), 4) < metadata_cache_for(Some(eight_gib), 4));
        assert_eq!(
            llm_cache_for(Some(eight_gib), 1),
            eight_gib / 100 * LLM_CACHE_MEMORY_PERCENT
        );
    }

    #[test]
    fn the_shares_together_fit_inside_the_machine() {
        // The compile-time assertion covers the percentages; this covers the
        // bytes a real shard count actually produces, floors included.
        let gib = 1024 * 1024 * 1024;
        for shards in [1_u64, 2, 4, 9, 16] {
            let count = usize::try_from(shards).expect("a plausible shard count");
            let promised = derivation_budget_for(Some(gib))
                + metadata_cache_for(Some(gib), count) * shards
                + llm_cache_for(Some(gib), count) * shards;
            assert!(
                promised < gib,
                "{shards} shards promise {promised} of a {gib}-byte machine"
            );
        }
    }

    #[test]
    fn the_budget_never_exceeds_what_the_machine_has() {
        // Not compared against a second reading of the sensor: `MemAvailable`
        // moves, and a test that demanded two readings agree would fail for
        // the one reason this module is allowed to behave that way.
        let ceiling = available_memory_bytes().unwrap_or(0);
        assert!(
            derivation_memory_budget_bytes() <= ceiling.max(MIN_DERIVATION_BUDGET_BYTES),
            "budget {} over ceiling {ceiling}",
            derivation_memory_budget_bytes()
        );
    }

    #[test]
    fn one_forward_pass_gets_the_cores_its_shard_is_not_sharing() {
        // Fewer shards than cores: each pass may use the slice left over.
        assert_eq!(embed_thread_width_for(Some(10), 2), 5);
        assert_eq!(embed_thread_width_for(Some(10), 4), 2);
    }

    #[test]
    fn the_default_shard_count_leaves_one_core_per_pass() {
        // One shard per core less one is what an unpinned deployment sizes
        // itself to, and it is the shape production runs in.
        assert_eq!(embed_thread_width_for(Some(10), 9), 1);
        assert_eq!(embed_thread_width_for(Some(64), 63), 1);
    }

    #[test]
    fn a_shard_per_core_leaves_one_core_per_pass() {
        assert_eq!(embed_thread_width_for(Some(10), 10), 1);
    }

    #[test]
    fn more_shards_than_cores_still_leaves_a_thread_to_run_on() {
        // Oversubscribed by shard count already; a width of zero would leave
        // no thread to embed on at all.
        assert_eq!(embed_thread_width_for(Some(4), 10), 1);
        assert_eq!(embed_thread_width_for(Some(1), 9), 1);
    }

    #[test]
    fn a_single_core_machine_runs_one_thread_wide() {
        assert_eq!(embed_thread_width_for(Some(1), 1), 1);
    }

    #[test]
    fn an_unreadable_core_count_takes_the_narrow_side() {
        assert_eq!(embed_thread_width_for(None, 1), 1);
        assert_eq!(embed_thread_width_for(None, 9), 1);
    }

    #[test]
    fn a_shard_count_of_zero_does_not_divide_by_it() {
        assert_eq!(embed_thread_width_for(Some(10), 0), 10);
    }
}
