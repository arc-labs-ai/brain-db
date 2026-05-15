//! Frame encode + decode micro-bench.
//!
//! Spec §16/02 §3 attributes ~0.5 ms each to wire/framing in/out for
//! the 20 ms RECALL p99 — i.e. the byte-level cost of going from a
//! `Frame` to wire bytes and back. This bench measures that cost at
//! four payload sizes spanning the realistic distribution.
//!
//! Run with: `cargo bench -p brain-protocol --bench frame_codec`.

use brain_protocol::{Frame, MAX_PAYLOAD_BYTES};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

// Phase 16.6a widened the wire opcode from u8 to u16; this bench
// still uses the substrate HELLO opcode value, just upcast.
const ENCODE_OPCODE: u16 = 0x0001;

fn make_frame(size: usize) -> Frame {
    // Deterministic payload so runs are comparable.
    let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    Frame::new(ENCODE_OPCODE, 0, 0, payload)
}

fn bench_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame_encode");
    for &size in &[64usize, 512, 2 * 1024, 16 * 1024] {
        let frame = make_frame(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &frame, |b, frame| {
            b.iter(|| {
                let bytes = frame.encode();
                black_box(bytes);
            });
        });
    }
    group.finish();
}

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame_decode");
    for &size in &[64usize, 512, 2 * 1024, 16 * 1024] {
        let frame = make_frame(size);
        let bytes = frame.encode();
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &bytes, |b, bytes| {
            b.iter(|| {
                let (decoded, _rest) =
                    Frame::decode_with_max(black_box(bytes), MAX_PAYLOAD_BYTES as u32)
                        .expect("decode");
                black_box(decoded);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_encode, bench_decode);
criterion_main!(benches);
