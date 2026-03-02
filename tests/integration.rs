use pzstd::error::PzstdError;

/// Compress a single chunk into an independent zstd frame.
fn compress_frame(data: &[u8], level: i32) -> Vec<u8> {
    zstd::bulk::compress(data, level).expect("compression failed")
}

/// Create a multi-frame pzstd-style input by compressing
/// each chunk independently and concatenating the frames.
fn make_multi_frame(chunks: &[&[u8]], level: i32) -> Vec<u8> {
    chunks
        .iter()
        .flat_map(|chunk| compress_frame(chunk, level))
        .collect()
}

/// Generate deterministic test data of a given size.
/// Repeating patterns compress well, which is what we want
/// for testing — we care about correctness, not ratio.
fn generate_test_data(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

// ─── Single frame (standard zstd) ───────────────────────────

#[test]
fn single_frame_round_trip() {
    let original = b"hello world from pzstd";
    let compressed = compress_frame(original, 3);
    let result = pzstd::decompressor::decompress(&compressed).unwrap();
    assert_eq!(result, original);
}

#[test]
fn single_frame_larger_data() {
    let original = generate_test_data(1_000_000);
    let compressed = compress_frame(&original, 3);
    let result = pzstd::decompressor::decompress(&compressed).unwrap();
    assert_eq!(result, original);
}

// ─── Multi-frame (parallel pzstd::decompressor::decompression) ───────────────────

#[test]
fn multi_frame_round_trip() {
    let chunk1 = b"first frame data";
    let chunk2 = b"second frame data";
    let chunk3 = b"third frame data";

    let compressed = make_multi_frame(&[chunk1, chunk2, chunk3], 3);

    let result = pzstd::decompressor::decompress(&compressed).unwrap();

    let mut expected = Vec::new();
    expected.extend_from_slice(chunk1);
    expected.extend_from_slice(chunk2);
    expected.extend_from_slice(chunk3);

    assert_eq!(result, expected);
}

#[test]
fn multi_frame_large_chunks() {
    let chunk_size = 500_000;
    let num_chunks = 8;

    let chunks: Vec<Vec<u8>> = (0..num_chunks)
        .map(|i| generate_test_data(chunk_size + i * 1000))
        .collect();

    let chunk_refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();
    let compressed = make_multi_frame(&chunk_refs, 3);

    let result = pzstd::decompressor::decompress(&compressed).unwrap();

    let expected: Vec<u8> = chunks.into_iter().flatten().collect();
    assert_eq!(result, expected);
}

#[test]
fn multi_frame_preserves_order() {
    // Each chunk has a unique byte so we can verify ordering
    let chunks: Vec<Vec<u8>> = (0u8..20).map(|i| vec![i; 10_000]).collect();

    let chunk_refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();
    let compressed = make_multi_frame(&chunk_refs, 3);

    let result = pzstd::decompressor::decompress(&compressed).unwrap();

    let mut offset = 0;
    for (i, chunk) in chunks.iter().enumerate() {
        assert_eq!(
            &result[offset..offset + chunk.len()],
            chunk.as_slice(),
            "frame {i} data mismatch — order may be wrong"
        );
        offset += chunk.len();
    }
}

// ─── Edge cases ─────────────────────────────────────────────

#[test]
fn empty_input_returns_error() {
    let result = pzstd::decompressor::decompress(&[]);
    assert!(matches!(
        result,
        Err(PzstdError::NoFrames | PzstdError::EmptyInput)
    ));
}

#[test]
fn invalid_magic_returns_error() {
    let garbage = b"this is not zstd data at all";
    let result = pzstd::decompressor::decompress(garbage);
    assert!(matches!(result, Err(PzstdError::InvalidMagic { .. })));
}

#[test]
fn truncated_frame_returns_error() {
    let original = generate_test_data(100_000);
    let compressed = compress_frame(&original, 3);
    // chop off the last 20 bytes to corrupt the frame
    let truncated = &compressed[..compressed.len() - 20];
    let result = pzstd::decompressor::decompress(truncated);
    assert!(result.is_err());
}

#[test]
fn capacity_exceeded_returns_error() {
    let original = generate_test_data(100_000);
    let compressed = compress_frame(&original, 3);
    // set max_frame_size smaller than pzstd::decompressor::decompressed size
    let result = pzstd::decompressor::decompress_with_max_frame_size(&compressed, 1024);
    assert!(matches!(result, Err(PzstdError::DecompressFailed { .. })));
}

// ─── Compression levels ─────────────────────────────────────

#[test]
fn works_across_compression_levels() {
    let original = generate_test_data(200_000);
    for level in [1, 3, 9, 19] {
        let compressed = compress_frame(&original, level);
        let result = pzstd::decompressor::decompress(&compressed).unwrap();
        assert_eq!(result, original, "failed at compression level {level}");
    }
}

#[test]
fn multi_frame_mixed_compression_levels() {
    // Each frame compressed at a different level — should still work
    let chunks: Vec<Vec<u8>> = (0..4)
        .map(|i| generate_test_data(50_000 + i * 10_000))
        .collect();

    let levels = [1, 3, 9, 15];

    let compressed: Vec<u8> = chunks
        .iter()
        .zip(levels.iter())
        .flat_map(|(chunk, &level)| compress_frame(chunk, level))
        .collect();

    let result = pzstd::decompressor::decompress(&compressed).unwrap();
    let expected: Vec<u8> = chunks.into_iter().flatten().collect();
    assert_eq!(result, expected);
}
