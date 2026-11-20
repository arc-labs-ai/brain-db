//! Process-oriented contention harness for the production embedding path.
//!
//! One process is one measurement. The embed path's intra-op width is derived
//! from the blocking thread's placement rather than configured, so there is no
//! width to sweep. This harness reports the width that derivation produced
//! alongside the throughput and shard-side scheduling latency it measures.
//! Criterion owns one routine's warm-up and sampling schedule, so it cannot
//! deterministically overlap those populations.
//!
//! The probe is a multiply-accumulate sweep over a 384-float resident buffer,
//! shaped like an HNSW distance computation and used as a scheduling probe. It
//! shows whether embedding work takes CPU from a shard reactor, but models
//! neither redb page-cache pressure, Tantivy I/O, nor memory-bandwidth
//! contention.

#[cfg(target_os = "linux")]
mod linux {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use brain_embed::{CpuDispatcher, Dispatcher, EmbedderConfig, ModelHandle};
    use brain_ops::embedding::embed_off_reactor;

    const MODEL_ENV: &str = "BRAIN_EMBED_MODEL_DIR";
    const SHARDS_ENV: &str = "BRAIN_BENCH_SHARDS";
    const SECONDS_ENV: &str = "BRAIN_BENCH_SECONDS";
    const TARGET_PROBE_NANOS: u128 = 50_000;
    const SETTLE: Duration = Duration::from_millis(200);

    #[derive(Default)]
    struct PhaseResult {
        embed_latencies_us: Vec<u64>,
        probe_latencies_us: Vec<u64>,
    }

    struct ShardResult {
        baseline: PhaseResult,
        isolated: PhaseResult,
        contended: PhaseResult,
    }

    fn env_usize(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    }

    async fn sleep_until(deadline: Instant) {
        if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            glommio::timer::sleep(remaining).await;
        }
    }

    fn probe_once(values: &[f32], iterations: usize) {
        let mut accumulator = 0.0_f32;
        for _ in 0..iterations {
            for value in values {
                accumulator += *value * *value;
            }
        }
        std::hint::black_box(accumulator);
    }

    fn calibrate_probe() -> usize {
        let values: Vec<f32> = (0..384).map(|value| value as f32 / 384.0).collect();
        let mut iterations = 1_usize;
        loop {
            let start = Instant::now();
            probe_once(&values, iterations);
            let elapsed = start.elapsed().as_nanos();
            if elapsed >= 5_000_000 || iterations >= 1_048_576 {
                let estimate = (u128::from(iterations as u64) * TARGET_PROBE_NANOS)
                    .saturating_add(elapsed.saturating_sub(1))
                    / elapsed.max(1);
                return usize::try_from(estimate).unwrap_or(usize::MAX).max(1);
            }
            iterations = iterations.saturating_mul(2);
        }
    }

    async fn run_phase(
        dispatcher: Arc<dyn Dispatcher>,
        phase_start: Instant,
        phase_end: Instant,
        probe_iterations: usize,
        run_embed: bool,
        run_probe: bool,
    ) -> PhaseResult {
        let embed_handle = {
            let dispatcher = Arc::clone(&dispatcher);
            glommio::spawn_local(async move {
                sleep_until(phase_start).await;
                let mut latencies = Vec::new();
                let mut sequence = 0_u64;
                while run_embed && Instant::now() < phase_end {
                    // The fixed-width suffix changes every request while keeping
                    // the input shape constant, preventing downstream reuse.
                    let text = format!("contention probe passage {sequence:08}");
                    let start = Instant::now();
                    embed_off_reactor(&dispatcher, &text)
                        .await
                        .expect("embedding probe");
                    let micros = start.elapsed().as_micros();
                    latencies.push(u64::try_from(micros).unwrap_or(u64::MAX));
                    sequence = sequence.saturating_add(1);
                }
                latencies
            })
        };
        let probe_handle = glommio::spawn_local(async move {
            sleep_until(phase_start).await;
            let values: Vec<f32> = (0..384).map(|value| value as f32 / 384.0).collect();
            let mut latencies = Vec::new();
            while run_probe && Instant::now() < phase_end {
                let start = Instant::now();
                probe_once(&values, probe_iterations);
                let micros = start.elapsed().as_micros();
                latencies.push(u64::try_from(micros).unwrap_or(u64::MAX));
                glommio::executor().yield_if_needed().await;
            }
            latencies
        });
        let embed_latencies_us = embed_handle.await;
        let probe_latencies_us = probe_handle.await;
        PhaseResult {
            embed_latencies_us,
            probe_latencies_us,
        }
    }

    async fn run_shard(
        dispatcher: Arc<dyn Dispatcher>,
        phase_a: (Instant, Instant),
        phase_b: (Instant, Instant),
        phase_c: (Instant, Instant),
        probe_iterations: usize,
    ) -> ShardResult {
        let baseline = run_phase(
            Arc::clone(&dispatcher),
            phase_a.0,
            phase_a.1,
            probe_iterations,
            false,
            true,
        )
        .await;
        let isolated = run_phase(
            Arc::clone(&dispatcher),
            phase_b.0,
            phase_b.1,
            probe_iterations,
            true,
            false,
        )
        .await;
        let contended = run_phase(
            dispatcher,
            phase_c.0,
            phase_c.1,
            probe_iterations,
            true,
            true,
        )
        .await;
        ShardResult {
            baseline,
            isolated,
            contended,
        }
    }

    fn percentile(samples: &mut [u64], fraction: f64) -> u64 {
        if samples.is_empty() {
            return 0;
        }
        samples.sort_unstable();
        let rank = (fraction * samples.len() as f64).ceil() as usize;
        samples[rank.max(1).min(samples.len()) - 1]
    }

    fn rate(count: usize, seconds: f64) -> f64 {
        count as f64 / seconds
    }

    fn json_f64(value: Option<f64>) -> String {
        match value {
            Some(value) if value.is_finite() => format!("{value:.6}"),
            _ => "null".to_owned(),
        }
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
        // Both figures are per-thread: Candle's split comes from the calling
        // thread's view of the machine and the pool's width from that thread's
        // placement. Reading them anywhere but a pinned blocking thread would
        // describe a different thread than the one that embeds.
        let (candle_split, intra_op_pool_threads) =
            glommio::LocalExecutorBuilder::new(glommio::Placement::Fixed(0))
                .name("bench-placement-probe")
                .spawn(|| async {
                    glommio::executor()
                        .spawn_blocking(|| {
                            (
                                brain_embed::matmul_split_count(),
                                brain_ops::embedding::intra_op_pool_threads(),
                            )
                        })
                        .await
                })
                .expect("spawn placement probe")
                .join()
                .expect("placement probe join");
        let candle_parallelism = if candle_split > 1 { "rayon" } else { "none" };
        let handle = ModelHandle::load(&EmbedderConfig::new(PathBuf::from(model_dir)))
            .expect("embedding model loads");
        // A bare CPU dispatcher forces every measured call through a real forward pass.
        let dispatcher: Arc<dyn Dispatcher> = Arc::new(CpuDispatcher::new(handle));
        let probe_iterations = calibrate_probe();
        let phase_seconds = Duration::from_secs(seconds as u64);
        let phase_a_start = Instant::now() + Duration::from_secs(1);
        let phase_a_end = phase_a_start + phase_seconds;
        let phase_b_start = phase_a_end + SETTLE;
        let phase_b_end = phase_b_start + phase_seconds;
        let phase_c_start = phase_b_end + SETTLE;
        let phase_c_end = phase_c_start + phase_seconds;
        let mut handles = Vec::with_capacity(shards);
        for index in 0..shards {
            let dispatcher = Arc::clone(&dispatcher);
            let phase_a = (phase_a_start, phase_a_end);
            let phase_b = (phase_b_start, phase_b_end);
            let phase_c = (phase_c_start, phase_c_end);
            let cpu = index % available;
            handles.push(
                glommio::LocalExecutorBuilder::new(glommio::Placement::Fixed(cpu))
                    .name(&format!("bench-shard-{index}"))
                    .spawn(move || async move {
                        run_shard(dispatcher, phase_a, phase_b, phase_c, probe_iterations).await
                    })
                    .expect("spawn benchmark shard"),
            );
        }
        let results: Vec<ShardResult> = handles
            .into_iter()
            .map(|handle| handle.join().expect("benchmark shard join"))
            .collect();
        let mut isolated_embeds = Vec::new();
        let mut contended_embeds = Vec::new();
        let mut isolated_probes = Vec::new();
        let mut contended_probes = Vec::new();
        let mut embed_counts = Vec::with_capacity(results.len());
        let mut probe_counts = Vec::with_capacity(results.len());
        let mut contended_embed_counts = Vec::with_capacity(results.len());
        let mut contended_probe_counts = Vec::with_capacity(results.len());
        for result in results {
            embed_counts.push(result.isolated.embed_latencies_us.len());
            probe_counts.push(result.baseline.probe_latencies_us.len());
            contended_embed_counts.push(result.contended.embed_latencies_us.len());
            contended_probe_counts.push(result.contended.probe_latencies_us.len());
            isolated_embeds.extend(result.isolated.embed_latencies_us);
            contended_embeds.extend(result.contended.embed_latencies_us);
            isolated_probes.extend(result.baseline.probe_latencies_us);
            contended_probes.extend(result.contended.probe_latencies_us);
        }
        // A zero-sample phase is a failed run, not a measurement: with nothing
        // in a phase there is no baseline to divide by, so every derived ratio
        // is undefined.
        let missing_samples = [
            ("isolated embedding", isolated_embeds.is_empty()),
            ("contended embedding", contended_embeds.is_empty()),
            ("isolated probe", isolated_probes.is_empty()),
            ("contended probe", contended_probes.is_empty()),
        ];
        for &(phase, is_empty) in &missing_samples {
            if is_empty {
                eprintln!("failed run: {phase} phase produced no samples");
            }
        }
        let embed_p50_isolated = percentile(&mut isolated_embeds, 0.50);
        let embed_p95_isolated = percentile(&mut isolated_embeds, 0.95);
        let embed_p50_contended = percentile(&mut contended_embeds, 0.50);
        let embed_p95_contended = percentile(&mut contended_embeds, 0.95);
        let probe_p50_isolated = percentile(&mut isolated_probes, 0.50);
        let probe_p95_isolated = percentile(&mut isolated_probes, 0.95);
        let probe_p99_isolated = percentile(&mut isolated_probes, 0.99);
        let probe_p50_contended = percentile(&mut contended_probes, 0.50);
        let probe_p95_contended = percentile(&mut contended_probes, 0.95);
        let probe_p99_contended = percentile(&mut contended_probes, 0.99);
        let isolated_count = isolated_embeds.len();
        let contended_count = contended_embeds.len();
        let phase_seconds = seconds as f64;
        let isolated_rate = rate(isolated_count, phase_seconds);
        let contended_rate = rate(contended_count, phase_seconds);
        let inflation = if probe_p95_isolated == 0 {
            None
        } else {
            Some(probe_p95_contended as f64 / probe_p95_isolated as f64)
        };
        let ratio = if isolated_rate == 0.0 {
            None
        } else {
            Some(contended_rate / isolated_rate)
        };
        println!(
            "{{\"candle_split\":{candle_split},\"candle_parallelism\":\"{candle_parallelism}\",\"intra_op_pool_threads\":{intra_op_pool_threads},\"shards\":{shards},\"available_parallelism\":{available},\"phase_seconds\":{seconds},\"embed_rate_isolated\":{},\"embed_rate_contended\":{},\"embed_p50_us\":{{\"isolated\":{embed_p50_isolated},\"contended\":{embed_p50_contended}}},\"embed_p95_us\":{{\"isolated\":{embed_p95_isolated},\"contended\":{embed_p95_contended}}},\"probe_p50_us\":{{\"isolated\":{probe_p50_isolated},\"contended\":{probe_p50_contended}}},\"probe_p95_us\":{{\"isolated\":{probe_p95_isolated},\"contended\":{probe_p95_contended}}},\"probe_p99_us\":{{\"isolated\":{probe_p99_isolated},\"contended\":{probe_p99_contended}}},\"probe_p95_inflation\":{},\"embed_rate_ratio\":{},\"embed_counts_per_shard\":{:?},\"probe_counts_per_shard\":{:?},\"embed_contended_counts_per_shard\":{:?},\"probe_contended_counts_per_shard\":{:?}}}",
            json_f64(Some(isolated_rate)),
            json_f64(Some(contended_rate)),
            json_f64(inflation),
            json_f64(ratio),
            embed_counts,
            probe_counts,
            contended_embed_counts,
            contended_probe_counts,
        );
        if missing_samples.iter().any(|(_, is_empty)| *is_empty) {
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("embed_contention requires Linux because Glommio is Linux-only");
}
