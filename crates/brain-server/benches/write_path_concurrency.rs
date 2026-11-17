//! Concurrent ENCODE write-path harness.
//!
//! This measures client-observed latency and throughput while concurrent
//! writers drive one space per shard through the production dispatch channel.

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use brain_embed::{CpuDispatcher, Dispatcher, EmbedderConfig, ModelHandle};
    use brain_metadata::api_keys::bits::FULL;
    use brain_ops::DispatchOutcome;
    use brain_protocol::envelope::request::{EncodeRequest, RequestBody, WaitMode};
    use brain_protocol::envelope::response::ResponseBody;
    use brain_server::shard::{spawn_shard, ShardSpawnConfig};

    const MODEL_ENV: &str = "BRAIN_EMBED_MODEL_DIR";
    const CONCURRENCY_ENV: &str = "BRAIN_BENCH_CONCURRENCY";
    const SHARDS_ENV: &str = "BRAIN_BENCH_SHARDS";
    const SECONDS_ENV: &str = "BRAIN_BENCH_SECONDS";
    const DISPATCHER_ENV: &str = "BRAIN_BENCH_DISPATCHER";

    struct StubDispatcher;

    impl brain_embed::Dispatcher for StubDispatcher {
        fn embed(
            &self,
            text: &str,
        ) -> Result<[f32; brain_embed::VECTOR_DIM], brain_embed::EmbedError> {
            // A deterministic unit vector makes the control run exercise the
            // write path without spending time in a model forward pass.
            let mut v = [0.0_f32; brain_embed::VECTOR_DIM];
            let h = text.bytes().fold(0usize, |hash, byte| {
                hash.wrapping_mul(31).wrapping_add(byte as usize)
            }) % brain_embed::VECTOR_DIM;
            v[h] = 1.0;
            Ok(v)
        }

        fn embed_batch(
            &self,
            texts: &[&str],
        ) -> Result<Vec<[f32; brain_embed::VECTOR_DIM]>, brain_embed::EmbedError> {
            texts.iter().map(|text| self.embed(text)).collect()
        }

        fn fingerprint(&self) -> [u8; 16] {
            [0; 16]
        }
    }

    #[derive(Debug)]
    struct CellResult {
        concurrency: usize,
        completions_per_writer: Vec<usize>,
        latencies_us: Vec<u64>,
        memory_ids: Vec<u128>,
        deduplicated_count: usize,
        max_queue_depth: usize,
        elapsed_seconds: f64,
    }

    fn env_usize(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    }

    fn concurrency_list() -> Vec<usize> {
        let values = std::env::var(CONCURRENCY_ENV)
            .unwrap_or_else(|_| "1,2,4,8,16".to_owned())
            .split(',')
            .filter_map(|value| value.trim().parse::<usize>().ok())
            .filter(|value| *value > 0)
            .collect::<Vec<_>>();
        if values.is_empty() {
            vec![1]
        } else {
            values
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

    fn norm(vector: &[f32; brain_embed::VECTOR_DIM]) -> f32 {
        vector.iter().map(|value| value * value).sum::<f32>().sqrt()
    }

    fn cosine(
        left: &[f32; brain_embed::VECTOR_DIM],
        right: &[f32; brain_embed::VECTOR_DIM],
    ) -> f32 {
        left.iter().zip(right).map(|(a, b)| a * b).sum::<f32>()
    }

    fn failed_run(reason: impl std::fmt::Display) -> ! {
        eprintln!("failed run: {reason}");
        std::process::exit(1);
    }

    fn full_caller(space: brain_core::SpaceId, namespace: &str) -> brain_ops::RequestCaller {
        brain_ops::RequestCaller::from_scope(
            space,
            [0u8; 16],
            [0u8; 16],
            namespace.to_owned(),
            FULL,
        )
    }

    fn placement_probe(placement: glommio::Placement) -> (usize, usize) {
        glommio::LocalExecutorBuilder::new(placement)
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
            .expect("placement probe join")
    }

    fn window_probe() -> (u64, u64) {
        glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
            .name("bench-window-probe")
            .spawn(|| async { brain_ops::embedding::embed_window_stats() })
            .expect("spawn window probe")
            .join()
            .expect("window probe join")
    }

    fn build_dispatcher(dispatcher_name: &str, model_dir: Option<String>) -> Arc<dyn Dispatcher> {
        match dispatcher_name {
            "stub" => Arc::new(StubDispatcher),
            "cpu" => {
                let model_dir = model_dir.expect("embedding model directory");
                let handle = ModelHandle::load(&EmbedderConfig::new(PathBuf::from(model_dir)))
                    .expect("embedding model loads");
                Arc::new(CpuDispatcher::new(handle))
            }
            other => failed_run(format!("unknown dispatcher {other:?}")),
        }
    }

    fn preflight(dispatcher: &Arc<dyn Dispatcher>) -> (f32, f32, f32, f32) {
        let texts = [
            "preflight alpha",
            "preflight beta",
            "preflight gamma",
            "preflight delta",
        ];
        let refs = texts.iter().copied().collect::<Vec<_>>();
        let batch = dispatcher
            .embed_batch(&refs)
            .unwrap_or_else(|error| failed_run(format!("preflight batch embed: {error}")));
        if batch.len() != texts.len() {
            failed_run(format!("preflight batch returned {} rows", batch.len()));
        }
        let singles = texts
            .iter()
            .map(|text| {
                dispatcher
                    .embed(text)
                    .unwrap_or_else(|error| failed_run(format!("preflight single embed: {error}")))
            })
            .collect::<Vec<_>>();
        let mut norms = Vec::with_capacity(batch.len() + singles.len());
        norms.extend(batch.iter().map(norm));
        norms.extend(singles.iter().map(norm));
        let norm_min = norms.iter().copied().fold(f32::INFINITY, f32::min);
        let norm_max = norms.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        if !norm_min.is_finite()
            || !norm_max.is_finite()
            || !(0.99..=1.01).contains(&norm_min)
            || !(0.99..=1.01).contains(&norm_max)
        {
            failed_run(format!("preflight norms outside 0.99..=1.01: {norms:?}"));
        }
        let matching = batch
            .iter()
            .zip(&singles)
            .map(|(batch, single)| cosine(batch, single))
            .collect::<Vec<_>>();
        let batch_vs_single_min = matching.iter().copied().fold(f32::INFINITY, f32::min);
        if matching.iter().any(|value| *value <= 0.9999) {
            failed_run(format!(
                "preflight batch/single cosine values: {matching:?}"
            ));
        }
        let mut distinct = Vec::new();
        for left in 0..singles.len() {
            for right in left + 1..singles.len() {
                distinct.push(cosine(&singles[left], &singles[right]));
            }
        }
        let distinct_max = distinct.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        if distinct.iter().any(|value| *value >= 0.9999) {
            failed_run(format!("preflight distinct cosine values: {distinct:?}"));
        }
        (norm_min, norm_max, batch_vs_single_min, distinct_max)
    }

    async fn sleep_until(deadline: Instant) {
        if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            tokio::time::sleep(remaining).await;
        }
    }

    async fn run_cell(
        dispatcher: Arc<dyn Dispatcher>,
        concurrency: usize,
        shards: usize,
        seconds: usize,
    ) -> CellResult {
        let temp_dir = tempfile::tempdir().expect("create cell temporary directory");
        let mut handles = Vec::with_capacity(shards);
        let mut joiners = Vec::with_capacity(shards);
        let mut callers = Vec::with_capacity(shards);
        for shard_index in 0..shards {
            let mut config = ShardSpawnConfig::new(
                temp_dir.path().join(format!("shard-{shard_index}")),
                Arc::clone(&dispatcher),
            );
            config.total_shards = shards;
            let (handle, joiner) = spawn_shard(shard_index as u16, config).expect("spawn shard");
            handles.push(handle);
            joiners.push(joiner);
            let space = brain_core::SpaceId::derive_from_string(
                "bench",
                &format!("write-path-{shard_index}"),
            );
            callers.push(full_caller(space, "bench"));
        }

        let start = Instant::now() + Duration::from_secs(1);
        let end = start + Duration::from_secs(seconds as u64);
        let mut writers = Vec::with_capacity(shards * concurrency);
        let mut samplers = Vec::with_capacity(shards);
        let warmup_barrier = Arc::new(tokio::sync::Barrier::new(shards * concurrency));
        for shard_index in 0..shards {
            let handle = handles[shard_index].clone();
            let sample_handle = handle.clone();
            samplers.push(tokio::spawn(async move {
                sleep_until(start).await;
                let mut maximum = 0;
                while Instant::now() < end {
                    maximum = maximum.max(sample_handle.queue_depth());
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                maximum
            }));
            for writer_id in 0..concurrency {
                let handle = handle.clone();
                let caller = callers[shard_index].clone();
                let warmup_barrier = Arc::clone(&warmup_barrier);
                writers.push(tokio::spawn(async move {
                    // Warm-up writes establish steady state and are excluded from measurements.
                    for warmup_index in 0..3 {
                        let request = RequestBody::Encode(EncodeRequest {
                            text: format!(
                                "warmup {warmup_index} shard {shard_index} writer {writer_id}"
                            )
                            .into(),
                            session_id: 1,
                            request_id: *uuid::Uuid::now_v7().as_bytes(),
                            txn_id: None,
                            occurred_at_unix_nanos: None,
                            act_as: None,
                            wait: WaitMode::Ack,
                            allow_duplicates: false,
                        });
                        let outcome = handle
                            .dispatch_op(request, caller.clone(), tracing::Span::none())
                            .await
                            .expect("dispatch");
                        let body = match outcome {
                            DispatchOutcome::Single(body) => body,
                            DispatchOutcome::Stream(_) => panic!("ENCODE returned a stream"),
                        };
                        match body {
                            ResponseBody::Encode(_) => {}
                            other => panic!("expected an Encode response, got {other:?}"),
                        }
                    }
                    warmup_barrier.wait().await;
                    sleep_until(start).await;
                    let mut latencies_us = Vec::new();
                    let mut memory_ids = Vec::new();
                    let mut deduplicated_count = 0;
                    let mut sequence = 0_u64;
    let mut first_start: Option<Instant> = None;
    let mut last_end: Option<Instant> = None;
                    // The measured span covers the first issued request through the last completion.
                    while Instant::now() < end {
                        let text = format!(
                            "write path probe shard {shard_index} writer {writer_id} seq {sequence:08}"
                        );
                        let request = RequestBody::Encode(EncodeRequest {
                            text: text.into(),
                            session_id: 1,
                            request_id: *uuid::Uuid::now_v7().as_bytes(),
                            txn_id: None,
                            occurred_at_unix_nanos: None,
                            act_as: None,
                            wait: WaitMode::Ack,
                            allow_duplicates: false,
                        });
                        let request_start = Instant::now();
                        first_start.get_or_insert(request_start);
                        let outcome = handle
                            .dispatch_op(request, caller.clone(), tracing::Span::none())
                            .await
                            .expect("dispatch");
                        let body = match outcome {
                            DispatchOutcome::Single(body) => body,
                            DispatchOutcome::Stream(_) => panic!("ENCODE returned a stream"),
                        };
                        let response = match body {
                            ResponseBody::Encode(response) => response,
                            other => panic!("expected an Encode response, got {other:?}"),
                        };
                        last_end = Some(Instant::now());
                        latencies_us.push(
                            u64::try_from(request_start.elapsed().as_micros()).unwrap_or(u64::MAX),
                        );
                        memory_ids.push(response.memory_id);
                        if response.was_deduplicated {
                            deduplicated_count += 1;
                        }
                        sequence += 1;
                    }
                    (
                        latencies_us,
                        memory_ids,
                        deduplicated_count,
                        first_start,
                        last_end,
                    )
                }));
            }
        }

        let mut result = CellResult {
            concurrency,
            completions_per_writer: Vec::with_capacity(writers.len()),
            latencies_us: Vec::new(),
            memory_ids: Vec::new(),
            deduplicated_count: 0,
            max_queue_depth: 0,
            elapsed_seconds: 0.0,
        };
        let mut first_start: Option<Instant> = None;
        let mut last_end: Option<Instant> = None;
        for writer in writers {
            let (latencies, memory_ids, deduplicated_count, writer_first_start, writer_last_end) =
                writer.await.expect("writer task");
            result.completions_per_writer.push(latencies.len());
            result.latencies_us.extend(latencies);
            result.memory_ids.extend(memory_ids);
            result.deduplicated_count += deduplicated_count;
            if let Some(writer_first_start) = writer_first_start {
                first_start = Some(first_start.map_or(writer_first_start, |cell_first_start| {
                    cell_first_start.min(writer_first_start)
                }));
            }
            if let Some(writer_last_end) = writer_last_end {
                last_end = Some(last_end.map_or(writer_last_end, |cell_last_end| {
                    cell_last_end.max(writer_last_end)
                }));
            }
        }
        if let (Some(first_start), Some(last_end)) = (first_start, last_end) {
            result.elapsed_seconds = last_end.duration_since(first_start).as_secs_f64();
        }
        for sampler in samplers {
            result.max_queue_depth = result
                .max_queue_depth
                .max(sampler.await.expect("queue sampler"));
        }
        drop(handles);
        for joiner in joiners {
            tokio::task::spawn_blocking(move || joiner.join())
                .await
                .expect("shard join task")
                .expect("shard join");
        }
        result
    }

    async fn run_sweep(
        dispatcher: Arc<dyn Dispatcher>,
        concurrency: &[usize],
        shards: usize,
        seconds: usize,
    ) -> Vec<CellResult> {
        let mut results = Vec::with_capacity(concurrency.len());
        for &n in concurrency {
            results.push(run_cell(Arc::clone(&dispatcher), n, shards, seconds).await);
        }
        results
    }

    pub fn main() {
        let available = thread::available_parallelism()
            .expect("available parallelism")
            .get();
        let dispatcher_name = std::env::var(DISPATCHER_ENV).unwrap_or_else(|_| "cpu".to_owned());
        let model_dir = std::env::var(MODEL_ENV).ok();
        if dispatcher_name == "cpu" && model_dir.is_none() {
            eprintln!("skipping: set BRAIN_EMBED_MODEL_DIR to an embedding model directory");
            return;
        }
        let shards = env_usize(SHARDS_ENV, 1).max(1);
        let seconds = env_usize(SECONDS_ENV, 5).max(1);
        let concurrency = concurrency_list();
        // Size the embedding fan-out the way the server does, from this
        // machine and this run's shard count, so the harness measures the
        // width production runs with rather than a default of its own.
        let embed_threads = brain_server::machine::install_embed_thread_width(shards);
        let dispatcher = build_dispatcher(&dispatcher_name, model_dir);
        let (norm_min, norm_max, batch_vs_single_min, distinct_cosine_max) = preflight(&dispatcher);
        let probe_unbound = placement_probe(glommio::Placement::Unbound);
        let probe_fixed0 = placement_probe(glommio::Placement::Fixed(0));
        let (window_batches, window_texts) = window_probe();
        println!(
            concat!(
                "{{\"available_parallelism\":{available},",
                "\"embed_threads\":{embed_threads},",
                "\"dispatcher\":\"{dispatcher_name}\",\"shards\":{shards},",
                "\"seconds\":{seconds},\"concurrency_list\":{:?},",
                "\"window_batches\":{window_batches},\"window_texts\":{window_texts},",
                "\"probe_unbound\":{{\"candle_split\":{},",
                "\"intra_op_pool_threads\":{}}},",
                "\"probe_fixed0\":{{\"candle_split\":{},",
                "\"intra_op_pool_threads\":{}}},",
                "\"preflight\":{{\"norm_min\":{norm_min:.6},",
                "\"norm_max\":{norm_max:.6},",
                "\"batch_vs_single_cosine_min\":{batch_vs_single_min:.6},",
                "\"distinct_cosine_max\":{distinct_cosine_max:.6}}},",
                "\"admission_permits\":15}}"
            ),
            concurrency,
            probe_unbound.0,
            probe_unbound.1,
            probe_fixed0.0,
            probe_fixed0.1,
            available = available,
            embed_threads = embed_threads,
            dispatcher_name = dispatcher_name,
            shards = shards,
            seconds = seconds,
            window_batches = window_batches,
            window_texts = window_texts,
            norm_min = norm_min,
            norm_max = norm_max,
            batch_vs_single_min = batch_vs_single_min,
            distinct_cosine_max = distinct_cosine_max,
        );
        // Fifteen is the shard's documented in-flight bound: the 60 KiB
        // group-commit batch divided by the 4 KiB record budget in
        // `max_in_flight_for_wal`; higher concurrency crosses admission backpressure.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(available)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let results = runtime.block_on(run_sweep(dispatcher, &concurrency, shards, seconds));
        let mut failed = false;
        for mut result in results {
            let completions_total = result.latencies_us.len();
            if result
                .completions_per_writer
                .iter()
                .any(|count| *count == 0)
            {
                eprintln!(
                    "failed run: writer completed nothing at concurrency {}",
                    result.concurrency
                );
                failed = true;
            }
            if result.deduplicated_count != 0 {
                eprintln!(
                    "failed run: {} deduplicated responses at concurrency {}",
                    result.deduplicated_count, result.concurrency
                );
                failed = true;
            }
            let distinct_memory_ids = result
                .memory_ids
                .iter()
                .copied()
                .collect::<HashSet<_>>()
                .len();
            if distinct_memory_ids != completions_total {
                eprintln!(
                    "failed run: distinct ids {distinct_memory_ids} != completions {completions_total} at concurrency {}",
                    result.concurrency
                );
                failed = true;
            }
            let p50 = percentile(&mut result.latencies_us, 0.50);
            let p99 = percentile(&mut result.latencies_us, 0.99);
            let max = result.latencies_us.iter().copied().max().unwrap_or(0);
            println!(
                concat!(
                    "{{\"concurrency\":{},\"shards\":{shards},",
                    "\"dispatcher\":\"{dispatcher_name}\",\"seconds\":{seconds},",
                    "\"elapsed_seconds\":{elapsed_seconds:.6},",
                    "\"completions_total\":{completions_total},",
                    "\"completions_per_writer\":{:?},",
                    "\"throughput_total_per_sec\":{throughput_total_per_sec:.6},",
                    "\"throughput_per_shard_per_sec\":{throughput_per_shard_per_sec:.6},",
                    "\"latency_us\":{{\"p50\":{p50},\"p99\":{p99},",
                    "\"max\":{max}}},\"distinct_memory_ids\":{distinct_memory_ids},",
                    "\"deduplicated_count\":{},\"max_queue_depth\":{}}}"
                ),
                result.concurrency,
                result.completions_per_writer,
                result.deduplicated_count,
                result.max_queue_depth,
                shards = shards,
                dispatcher_name = dispatcher_name,
                seconds = seconds,
                elapsed_seconds = result.elapsed_seconds,
                completions_total = completions_total,
                throughput_total_per_sec = if result.elapsed_seconds > 0.0 {
                    rate(completions_total, result.elapsed_seconds)
                } else {
                    0.0
                },
                throughput_per_shard_per_sec = if result.elapsed_seconds > 0.0 {
                    rate(completions_total, result.elapsed_seconds * shards as f64)
                } else {
                    0.0
                },
                p50 = p50,
                p99 = p99,
                max = max,
                distinct_memory_ids = distinct_memory_ids,
            );
        }
        if failed {
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
    eprintln!("write_path_concurrency requires Linux because Glommio is Linux-only");
}
