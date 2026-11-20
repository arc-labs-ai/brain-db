//! Throughput and latency of the ENCODE write path's embedding step, measured
//! against the number of writes a shard has in flight.
//!
//! One process is one measurement: the path and the in-flight count both come
//! from the environment, so a before and an after are two runs of the same
//! binary on the same box. Each shard here is a pinned `LocalExecutor` holding
//! the blocking pool Glommio gives it by default — one thread on the shard's
//! own core — so what this reports is what collecting waiting texts into one
//! forward pass buys on its own, whatever width that pool is given elsewhere.
//!
//! Every measured call runs a real BGE-small forward pass. The harness models
//! neither the WAL commit nor the index write that follow the embed on a true
//! ENCODE, so it bounds the embedding step rather than predicting end-to-end
//! write throughput.

#[cfg(target_os = "linux")]
mod linux {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use brain_embed::{CpuDispatcher, Dispatcher, EmbedderConfig, ModelHandle};
    use brain_ops::embedding::{
        embed_batch_off_reactor, embed_batched_off_reactor, embed_off_reactor, embed_window_stats,
        intra_op_pool_threads,
    };

    const MODEL_ENV: &str = "BRAIN_EMBED_MODEL_DIR";
    const SHARDS_ENV: &str = "BRAIN_BENCH_SHARDS";
    const SECONDS_ENV: &str = "BRAIN_BENCH_SECONDS";
    const CONCURRENCY_ENV: &str = "BRAIN_BENCH_CONCURRENCY";
    const PATH_ENV: &str = "BRAIN_BENCH_PATH";
    const PIN_ENV: &str = "BRAIN_BENCH_PIN";

    struct ShardResult {
        latencies_us: Vec<u64>,
        batches: u64,
        texts: u64,
    }

    fn env_usize(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    }

    fn percentile(samples: &mut [u64], fraction: f64) -> u64 {
        if samples.is_empty() {
            return 0;
        }
        samples.sort_unstable();
        let rank = (fraction * samples.len() as f64).ceil() as usize;
        samples[rank.max(1).min(samples.len()) - 1]
    }

    fn json_f64(value: Option<f64>) -> String {
        match value {
            Some(value) if value.is_finite() => format!("{value:.6}"),
            _ => "null".to_owned(),
        }
    }

    /// Drive `concurrency` writes at once on one shard for the measured window.
    ///
    /// The lanes are what the batch window has to work with: each is an
    /// independent write awaiting its own vector, which is the shape a shard
    /// sees when its request bound is saturated.
    async fn run_shard(
        dispatcher: Arc<dyn Dispatcher>,
        start: Instant,
        end: Instant,
        concurrency: usize,
        batched: bool,
    ) -> ShardResult {
        let mut lanes = Vec::with_capacity(concurrency);
        for lane in 0..concurrency {
            let dispatcher = Arc::clone(&dispatcher);
            lanes.push(glommio::spawn_local(async move {
                if let Some(wait) = start.checked_duration_since(Instant::now()) {
                    glommio::timer::sleep(wait).await;
                }
                let mut latencies = Vec::new();
                let mut sequence = 0_u64;
                while Instant::now() < end {
                    // A distinct text per lane and iteration holds the input
                    // shape constant while denying any downstream reuse.
                    let text = format!("write path passage {lane:04} {sequence:08}");
                    let began = Instant::now();
                    let embedded = if batched {
                        embed_batched_off_reactor(&dispatcher, &text).await
                    } else {
                        embed_off_reactor(&dispatcher, &text).await
                    };
                    embedded.expect("embedding a write path passage");
                    let micros = began.elapsed().as_micros();
                    latencies.push(u64::try_from(micros).unwrap_or(u64::MAX));
                    sequence = sequence.saturating_add(1);
                }
                latencies
            }));
        }
        let mut latencies_us = Vec::new();
        for lane in lanes {
            latencies_us.extend(lane.await);
        }
        // Read on the reactor thread that ran the windows, which is the thread
        // whose counters they are.
        let (batches, texts) = embed_window_stats();
        ShardResult {
            latencies_us,
            batches,
            texts,
        }
    }

    /// The batch widths the sweep prices.
    const SWEEP_WIDTHS: [usize; 6] = [1, 2, 4, 8, 16, 32];

    /// Price one forward pass against the number of texts inside it.
    ///
    /// This is what decides whether collecting writes can pay at all. A cost
    /// that climbs in step with the rows means the pass was already busy and a
    /// batch saves nothing but call overhead; a cost that flattens means the
    /// rows ride along with work the pass was doing anyway.
    async fn run_sweep(dispatcher: Arc<dyn Dispatcher>, repeats: usize) -> Vec<(usize, u64, u64)> {
        let mut priced = Vec::with_capacity(SWEEP_WIDTHS.len());
        for width in SWEEP_WIDTHS {
            let mut per_batch_us = Vec::with_capacity(repeats);
            for round in 0..repeats {
                // Fresh text every pass so nothing downstream can reuse a
                // result, with the shape held constant across the widths.
                let texts: Vec<String> = (0..width)
                    .map(|row| format!("sweep passage {round:04} {row:04}"))
                    .collect();
                let began = Instant::now();
                embed_batch_off_reactor(&dispatcher, texts)
                    .await
                    .expect("embedding a sweep batch");
                let micros = began.elapsed().as_micros();
                per_batch_us.push(u64::try_from(micros).unwrap_or(u64::MAX));
            }
            let median = percentile(&mut per_batch_us, 0.50);
            priced.push((width, median, median / width as u64));
        }
        priced
    }

    pub fn main() {
        let Ok(model_dir) = std::env::var(MODEL_ENV) else {
            eprintln!("skipping: set {MODEL_ENV} to an embedding model directory");
            return;
        };
        let available = thread::available_parallelism()
            .expect("available parallelism")
            .get();
        let shards = env_usize(SHARDS_ENV, 4).max(1);
        let seconds = env_usize(SECONDS_ENV, 10).max(1);
        let concurrency = env_usize(CONCURRENCY_ENV, 15).max(1);
        let requested_path = std::env::var(PATH_ENV).unwrap_or_else(|_| "single".to_owned());
        let batched = match requested_path.as_str() {
            "batched" => true,
            "single" | "sweep" => false,
            other => {
                eprintln!("{PATH_ENV} must be `single`, `batched` or `sweep`, got `{other}`");
                std::process::exit(2);
            }
        };
        let handle = ModelHandle::load(&EmbedderConfig::new(PathBuf::from(model_dir)))
            .expect("embedding model loads");
        // A bare CPU dispatcher forces every measured call through a real
        // forward pass rather than a cache.
        let dispatcher: Arc<dyn Dispatcher> = Arc::new(CpuDispatcher::new(handle));
        if requested_path == "sweep" {
            // Placement decides the width of the intra-op pool that runs the
            // pass, because that pool takes its width from the blocking
            // thread's affinity mask. Pinned prices one core's pass; unbound
            // prices a pass free to spread over the machine. The two are
            // different regimes for a batch, so the sweep prices either.
            let pinned = env_usize(PIN_ENV, 1) != 0;
            let placement = if pinned {
                glommio::Placement::Fixed(0)
            } else {
                glommio::Placement::Unbound
            };
            let priced = glommio::LocalExecutorBuilder::new(placement)
                .name("bench-sweep")
                .spawn(move || async move {
                    // Both figures are per-thread and only meaningful on the
                    // thread that embeds, so they are read from inside the
                    // blocking pool that runs the passes.
                    let widths = glommio::executor()
                        .spawn_blocking(|| {
                            (brain_embed::matmul_split_count(), intra_op_pool_threads())
                        })
                        .await;
                    (run_sweep(dispatcher, seconds).await, widths)
                })
                .expect("spawn sweep shard")
                .join()
                .expect("sweep shard join");
            let (priced, (candle_split, pool_threads)) = priced;
            let rows: Vec<String> = priced
                .iter()
                .map(|(width, batch_us, per_text_us)| {
                    format!(
                        "{{\"texts\":{width},\"batch_us\":{batch_us},\"per_text_us\":{per_text_us}}}"
                    )
                })
                .collect();
            println!(
                "{{\"path\":\"sweep\",\"repeats\":{seconds},\"candle_split\":{candle_split},\"intra_op_pool_threads\":{pool_threads},\"available_parallelism\":{available},\"priced\":[{}]}}",
                rows.join(","),
            );
            return;
        }
        let start = Instant::now() + Duration::from_secs(1);
        let end = start + Duration::from_secs(seconds as u64);
        let mut shard_handles = Vec::with_capacity(shards);
        for index in 0..shards {
            let dispatcher = Arc::clone(&dispatcher);
            let cpu = index % available;
            shard_handles.push(
                glommio::LocalExecutorBuilder::new(glommio::Placement::Fixed(cpu))
                    .name(&format!("bench-shard-{index}"))
                    .spawn(move || async move {
                        run_shard(dispatcher, start, end, concurrency, batched).await
                    })
                    .expect("spawn benchmark shard"),
            );
        }
        let results: Vec<ShardResult> = shard_handles
            .into_iter()
            .map(|handle| handle.join().expect("benchmark shard join"))
            .collect();
        let mut latencies = Vec::new();
        let mut counts = Vec::with_capacity(results.len());
        let mut batches_total = 0_u64;
        let mut texts_total = 0_u64;
        for result in results {
            counts.push(result.latencies_us.len());
            batches_total = batches_total.saturating_add(result.batches);
            texts_total = texts_total.saturating_add(result.texts);
            latencies.extend(result.latencies_us);
        }
        // An empty run is a failed run, not a measurement of zero.
        if latencies.is_empty() {
            eprintln!("failed run: no embedding completed inside the measured window");
            std::process::exit(1);
        }
        let embeds = latencies.len();
        let window_seconds = seconds as f64;
        let aggregate_rate = embeds as f64 / window_seconds;
        let per_shard_rate = aggregate_rate / shards as f64;
        // Texts per batch is the whole question for a window with no timer: at
        // one, the writes never overlapped and the window bought nothing.
        let mean_batch_texts = if batches_total == 0 {
            None
        } else {
            Some(texts_total as f64 / batches_total as f64)
        };
        let p50 = percentile(&mut latencies, 0.50);
        let p95 = percentile(&mut latencies, 0.95);
        let p99 = percentile(&mut latencies, 0.99);
        println!(
            "{{\"path\":\"{requested_path}\",\"shards\":{shards},\"concurrency\":{concurrency},\"available_parallelism\":{available},\"window_seconds\":{seconds},\"embeds\":{embeds},\"embed_rate_aggregate\":{},\"embed_rate_per_shard\":{},\"embed_latency_us\":{{\"p50\":{p50},\"p95\":{p95},\"p99\":{p99}}},\"batches\":{batches_total},\"batched_texts\":{texts_total},\"mean_batch_texts\":{},\"embeds_per_shard\":{:?}}}",
            json_f64(Some(aggregate_rate)),
            json_f64(Some(per_shard_rate)),
            json_f64(mean_batch_texts),
            counts,
        );
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("embed_batch_window requires Linux because Glommio is Linux-only");
}
