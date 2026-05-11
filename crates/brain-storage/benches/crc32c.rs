//! CRC32C micro-bench — the most-hit per-record path in the WAL write/read
//! loop. Establishes the criterion harness pattern; later phases add op-
//! level benches per spec §16/02 latency targets.
//!
//! Run with: `cargo bench -p brain-storage --bench crc32c`.
//!
//! The bench measures throughput of `crc32c::crc32c` over four payload sizes
//! that bracket the WAL record-payload distribution:
//!
//! - **64 B** — minimal-payload records (e.g., `UpdateSalience` with one
//!   update, `Reclaim`, `TxnBegin`).
//! - **512 B** — typical `Forget` / `Link` records.
//! - **2 KiB** — `Encode` payloads with a short text + vector reference.
//! - **16 KiB** — large `Encode` payloads with embedded text/vector or
//!   batched `UpdateSalience`.
//!
//! On a 2024-era laptop the function clocks ~10 GB/s (uses SSE 4.2 / ARM
//! crc32 intrinsics), so all four sizes should complete a million
//! iterations in well under a second.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

fn bench_crc32c(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc32c");

    for &size in &[64usize, 512, 2 * 1024, 16 * 1024] {
        let buf: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &buf, |b, buf| {
            b.iter(|| {
                let crc = crc32c::crc32c(black_box(buf));
                black_box(crc);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_crc32c);
criterion_main!(benches);
