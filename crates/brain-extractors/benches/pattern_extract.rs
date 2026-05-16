//! Pattern extractor perf bench (sub-task 20.10).
//!
//! Spec targets per `spec/16_benchmarks_acceptance/02_latency_targets.md`
//! §2.7 at a single-extractor dispatch over a 4 KiB memory:
//!
//! - Pattern extractor: p50 30 µs, p99 100 µs.
//!
//! Run: `cargo bench -p brain-extractors --bench pattern_extract`.

use brain_core::{AgentId, ContextId, ExtractorId, Memory, MemoryId, MemoryKind, Salience};
use brain_extractors::{ExtractionContext, Extractor, ExtractorRegistry, PatternExtractor};
use brain_protocol::schema::ExtractorTarget;
use criterion::{black_box, criterion_group, Criterion};

const PATTERNS_TYPICAL: &[&str] = &[
    r"\b([A-Z][a-z]+\s+[A-Z][a-z]+)\b",
    r"\b([A-Z]\.\s*[A-Z][a-z]+)\b",
    r"\b([A-Z][A-Z]+-\d+)\b",
    r"\b(\w+@\w+\.\w+)\b",
    r"\b(\d{4}-\d{2}-\d{2})\b",
];

fn build_extractor() -> PatternExtractor {
    let raw: Vec<String> = PATTERNS_TYPICAL.iter().map(|s| (*s).to_string()).collect();
    PatternExtractor::try_new(
        ExtractorId::from(1),
        "bench:extract".into(),
        ExtractorTarget::Entity {
            entity_type: "brain:Person".into(),
        },
        1,
        &raw,
        0.7,
    )
    .expect("build")
}

fn build_memory(size_bytes: usize) -> Memory {
    let base = "Alice Cooper met Bob Smith on 2026-05-16 at TICKET-1234. ";
    let mut text = String::with_capacity(size_bytes + base.len());
    while text.len() < size_bytes {
        text.push_str(base);
    }
    text.truncate(size_bytes);
    Memory {
        id: MemoryId::pack(0, 1, 0),
        agent: AgentId::new(),
        context: ContextId(0),
        kind: MemoryKind::Episodic,
        salience: Salience::default(),
        text: Some(text),
        created_at_unix_ms: 0,
        last_accessed_at_unix_ms: 0,
    }
}

fn bench_pattern_extract(c: &mut Criterion) {
    let ext = build_extractor();
    let mem = build_memory(4096);
    let reg = ExtractorRegistry::new();
    let ctx = ExtractionContext {
        schema_version: 1,
        now_unix_nanos: 0,
        registry: &reg,
    };

    c.bench_function("pattern_extract 4KiB / 5 regexes", |b| {
        b.iter(|| {
            let r = ext.run(&ctx, black_box(&mem));
            black_box(r);
        });
    });
}

fn bench_pattern_extract_short(c: &mut Criterion) {
    let ext = build_extractor();
    let mem = build_memory(256);
    let reg = ExtractorRegistry::new();
    let ctx = ExtractionContext {
        schema_version: 1,
        now_unix_nanos: 0,
        registry: &reg,
    };

    c.bench_function("pattern_extract 256B / 5 regexes", |b| {
        b.iter(|| {
            let r = ext.run(&ctx, black_box(&mem));
            black_box(r);
        });
    });
}

fn print_corpus_summary() {
    let ext = build_extractor();
    let mem = build_memory(4096);
    let reg = ExtractorRegistry::new();
    let ctx = ExtractionContext {
        schema_version: 1,
        now_unix_nanos: 0,
        registry: &reg,
    };
    let r = ext.run(&ctx, &mem);
    eprintln!(
        "pattern_extract bench setup: patterns={} text_bytes=4096 items_per_run={}",
        ext.patterns().len(),
        r.items.len()
    );
}

criterion_group!(
    name = pattern_extract_benches;
    config = Criterion::default();
    targets = bench_pattern_extract, bench_pattern_extract_short
);

fn main() {
    print_corpus_summary();
    pattern_extract_benches();
    Criterion::default().configure_from_args().final_summary();
}
