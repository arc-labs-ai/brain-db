//! Throughput benchmark for the embedding layer.
//!
//! Spec / phase-doc / orientation plan converge on **≥ 1 000 texts/sec
//! sustained on the reference CPU with the cache cold** (phase-05.md
//! §0; orientation plan §0). Spec §04/03 §6 says 5–10 ms/text single-
//! threaded, so the 1 000/s target implies parallelism. We measure:
//!
//! 1. `single_thread_short` — single embed, short input. Reports the
//!    per-iter latency that spec §04/03 §6 quotes.
//! 2. `single_thread_long`  — single embed, ~500-token input. Worst-
//!    case attention compute. Honest floor for long-text workloads.
//! 3. `concurrent`          — 8 threads × 64 embeds against a shared
//!    `Arc<CpuDispatcher>`. Asserts ≥ 1 000 texts/s; panics with the
//!    actual rate if we miss.
//! 4. `cache_hit`           — pre-warmed `CachingDispatcher`. Per-call
//!    latency expected sub-µs (spec §04/05 §3 cites < 1 µs); observed.
//!
//! Gated on `BRAIN_EMBED_MODEL_DIR`. Without the env var each bench
//! function prints a skip line and returns; criterion still exits 0.
//!
//! Run with:
//! `BRAIN_EMBED_MODEL_DIR=… cargo bench -p brain-embed --bench throughput`

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use brain_embed::{CachingDispatcher, CpuDispatcher, Dispatcher, EmbedderConfig, ModelHandle};
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

const ENV_VAR: &str = "BRAIN_EMBED_MODEL_DIR";
const SPEC_FLOOR_TEXTS_PER_SEC: f64 = 1_000.0;
const CONCURRENT_THREADS: usize = 8;
const CONCURRENT_ITERS_PER_THREAD: usize = 64;

const SHORT_TEXT: &str = "the quick brown fox jumps over the lazy dog";

fn long_text() -> String {
    // ~500 tokens: 100 repetitions of a 5-word phrase. Just under the
    // 512 cap so the full forward pass runs without truncation.
    "the cat sat on mat ".repeat(100)
}

fn try_load_dispatcher() -> Option<CpuDispatcher> {
    let Ok(dir) = std::env::var(ENV_VAR) else {
        eprintln!("skipping: set {ENV_VAR} to a BGE-small directory to run");
        return None;
    };
    let dir = PathBuf::from(dir);
    assert!(
        dir.is_dir(),
        "{ENV_VAR}={} is not a directory",
        dir.display()
    );
    let handle = ModelHandle::load(&EmbedderConfig::new(dir)).expect("model loads");
    Some(CpuDispatcher::new(handle))
}

// ---------------------------------------------------------------------------
// 1. Single-thread short — spec §04/03 §6's 5-10 ms/text reference.
// ---------------------------------------------------------------------------

fn bench_single_thread_short(c: &mut Criterion) {
    let Some(dispatcher) = try_load_dispatcher() else {
        return;
    };
    let mut group = c.benchmark_group("single_thread_short");
    group.throughput(Throughput::Elements(1));
    group.bench_function("embed", |b| {
        b.iter(|| {
            let v = dispatcher.embed(black_box(SHORT_TEXT)).expect("embed");
            black_box(v);
        });
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// 2. Single-thread long — worst case, full attention compute.
// ---------------------------------------------------------------------------

fn bench_single_thread_long(c: &mut Criterion) {
    let Some(dispatcher) = try_load_dispatcher() else {
        return;
    };
    let text = long_text();
    let mut group = c.benchmark_group("single_thread_long");
    group.throughput(Throughput::Elements(1));
    // Long-text iterations are ~10 ms each; criterion's default 100
    // samples × ~10 ms = ~1 s per measurement window. Bump the
    // measurement time a touch so warmup + samples both have room.
    group.measurement_time(Duration::from_secs(8));
    group.bench_function("embed", |b| {
        b.iter(|| {
            let v = dispatcher.embed(black_box(&text)).expect("embed");
            black_box(v);
        });
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// 3. Concurrent — the floor-asserted measurement.
// ---------------------------------------------------------------------------

fn floor_check_or_panic(dispatcher: &Arc<CpuDispatcher>) {
    // One hand-timed pass before criterion runs. If we're below the
    // floor, panic with a clear message; criterion never reports the
    // group, the bench exits non-zero.
    let start = Instant::now();
    let mut handles = Vec::with_capacity(CONCURRENT_THREADS);
    for _ in 0..CONCURRENT_THREADS {
        let d = Arc::clone(dispatcher);
        handles.push(thread::spawn(move || {
            for _ in 0..CONCURRENT_ITERS_PER_THREAD {
                let v = d.embed(SHORT_TEXT).expect("embed");
                black_box(v);
            }
        }));
    }
    for h in handles {
        h.join().expect("thread panicked");
    }
    let elapsed = start.elapsed();
    let total = (CONCURRENT_THREADS * CONCURRENT_ITERS_PER_THREAD) as f64;
    let rate = total / elapsed.as_secs_f64();
    if rate < SPEC_FLOOR_TEXTS_PER_SEC {
        panic!(
            "concurrent throughput {rate:.0} texts/s < {SPEC_FLOOR_TEXTS_PER_SEC:.0} floor \
             ({CONCURRENT_THREADS} threads × {CONCURRENT_ITERS_PER_THREAD} iters, \
             {:.2} ms total)",
            elapsed.as_secs_f64() * 1000.0
        );
    }
    eprintln!(
        "concurrent floor check: {rate:.0} texts/s (≥ {SPEC_FLOOR_TEXTS_PER_SEC:.0} floor); \
         {CONCURRENT_THREADS} threads × {CONCURRENT_ITERS_PER_THREAD} iters in {:.2} ms",
        elapsed.as_secs_f64() * 1000.0
    );
}

fn bench_concurrent(c: &mut Criterion) {
    let Some(dispatcher) = try_load_dispatcher() else {
        return;
    };
    let dispatcher = Arc::new(dispatcher);

    // Spec-floor sanity check before criterion measurements.
    floor_check_or_panic(&dispatcher);

    let mut group = c.benchmark_group("concurrent");
    let per_iter = CONCURRENT_THREADS * CONCURRENT_ITERS_PER_THREAD;
    group.throughput(Throughput::Elements(per_iter as u64));
    // Each iter spawns 8 threads × 64 embeds. At 1 000/s that's ~500 ms
    // per iter; warmup + 100 samples needs a longer measurement window.
    group.measurement_time(Duration::from_secs(20));
    group.sample_size(10);
    group.bench_function("8_threads_64_iters", |b| {
        b.iter(|| {
            let mut handles = Vec::with_capacity(CONCURRENT_THREADS);
            for _ in 0..CONCURRENT_THREADS {
                let d = Arc::clone(&dispatcher);
                handles.push(thread::spawn(move || {
                    for _ in 0..CONCURRENT_ITERS_PER_THREAD {
                        let v = d.embed(SHORT_TEXT).expect("embed");
                        black_box(v);
                    }
                }));
            }
            for h in handles {
                h.join().expect("thread panicked");
            }
        });
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// 4. Cache hit — spec §04/05 §3 cites < 1 µs.
// ---------------------------------------------------------------------------

fn bench_cache_hit(c: &mut Criterion) {
    let Some(cpu) = try_load_dispatcher() else {
        return;
    };
    let cache = CachingDispatcher::new(cpu, 100);

    // Pre-warm: the very first call is a miss that populates the entry.
    let _ = cache.embed(SHORT_TEXT).expect("warm-up");
    assert_eq!(cache.stats().misses, 1);

    let mut group = c.benchmark_group("cache_hit");
    group.throughput(Throughput::Elements(1));
    group.bench_function("embed", |b| {
        b.iter(|| {
            let v = cache.embed(black_box(SHORT_TEXT)).expect("hit");
            black_box(v);
        });
    });
    group.finish();

    // Sanity: every iteration after the warm-up should have hit.
    let stats = cache.stats();
    assert!(
        stats.hits > 0,
        "cache_hit bench saw no hits (stats: {stats:?})"
    );
    eprintln!(
        "cache_hit bench summary: hits={} misses={} size={}",
        stats.hits, stats.misses, stats.size
    );
}

criterion_group! {
    name = throughput;
    config = Criterion::default().measurement_time(Duration::from_secs(5));
    targets =
        bench_single_thread_short,
        bench_single_thread_long,
        bench_concurrent,
        bench_cache_hit
}
criterion_main!(throughput);
