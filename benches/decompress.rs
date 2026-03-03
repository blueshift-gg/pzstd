use std::io::Read;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

fn compress_frame(data: &[u8], level: i32) -> Vec<u8> {
    zstd::bulk::compress(data, level).expect("compression failed")
}

fn make_multi_frame(data: &[u8], chunk_size: usize, level: i32) -> Vec<u8> {
    data.chunks(chunk_size)
        .flat_map(|chunk| compress_frame(chunk, level))
        .collect()
}

fn generate_realistic_data(size: usize) -> Vec<u8> {
    let mut data = vec![0u8; size];
    let mut state: u64 = 0xDEADBEEF;
    for (i, byte) in data.iter_mut().enumerate() {
        if i % 128 < 48 {
            // ~37% structured/repeated data for realistic compressibility
            *byte = (i % 256) as u8;
        } else {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
    }
    data
}

fn bench_decompress(c: &mut Criterion) {
    let sizes = [
        ("10MB", 10 * 1024 * 1024),
        ("100MB", 100 * 1024 * 1024),
        ("500MB", 500 * 1024 * 1024),
    ];

    let mut group = c.benchmark_group("decompress");
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(10));
    group.warm_up_time(std::time::Duration::from_secs(5));

    for (label, size) in sizes {
        let data = generate_realistic_data(size);
        let single = compress_frame(&data, 3);
        let multi = make_multi_frame(&data, 4 * 1024 * 1024, 3);

        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(
            BenchmarkId::new("zstd-single-frame", label),
            &single,
            |b, compressed| b.iter(|| zstd::bulk::decompress(compressed, size + 1024).unwrap()),
        );

        group.bench_with_input(
            BenchmarkId::new("zstd-sequential-multi-frame", label),
            &multi,
            |b, compressed| {
                b.iter(|| {
                    let mut decoder = zstd::Decoder::new(&compressed[..]).unwrap();
                    let mut output = Vec::with_capacity(size);
                    decoder.read_to_end(&mut output).unwrap();
                    output
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("pzstd-parallel-multi", label),
            &multi,
            |b, compressed| b.iter(|| pzstd::decompressor::decompress(compressed).unwrap()),
        );
    }

    group.finish();
}

criterion_group!(benches, bench_decompress);
criterion_main!(benches);
