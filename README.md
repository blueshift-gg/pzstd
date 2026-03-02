# pzstd

Parallel zstd decompression library for Rust.

Scans inputs for independent frame boundaries and decompresses
each frame concurrently using rayon. Achieves **3x+ throughput** over
single-threaded zstd on multi-core hardware.

## Performance

Benchmarked on 16-thread hardware with 4MB frame chunks:

| Input Size | zstd (single-threaded) | pzstd (parallel) | Speedup |
|------------|----------------------|------------------|---------|
| 100 MB     | 2.0 GiB/s            | 5.9 GiB/s        | **3.0x**  |
| 500 MB     | 2.0 GiB/s            | 6.1 GiB/s        | **3.1x**  |

## Usage

```rust
// Decompress a multi-frame zstd file
let compressed = std::fs::read("snapshot.tar.zst").unwrap();
let data = pzstd::decompressor::decompress(&compressed).unwrap();

// With custom per-frame capacity limit
let data = pzstd::decompressor::decompress_with_max_frame_size(&compressed, 256 * 1024 * 1024).unwrap();
```

## Architecture
```text
Input ──> Frame Scanner ──> Extract Frame_Content_Size
                                      │
                            ┌─────────┴─────────┐
                            │                   │
                        All known           Some missing
                            │                   │
                            │                   │
                      ┌─────┴─────┐      ┌──────┴─────┐
                      │ Fast Path │      │  Fallback  │
                      ├───────────┤      ├────────────┤
                      │ Single    │      │ Per-frame  │
                      │ alloc     │      │ alloc      │
                      │           │      │            │
                      │ split_at  │      │ Thread-    │
                      │ _mut      │      │ local DCTX │
                      │ slices    │      │            │
                      │           │      │ Parallel   │
                      │ Thread-   │      │ via rayon  │
                      │ local     │      │            │
                      │ DCTX      │      └──────┬─────┘
                      │           │             │
                      │ Zero-copy │             │
                      │ to buffer │             │
                      └─────┬─────┘             │
                            │                   │
                            │                   │
                      ┌─────┴───────────────────┴─────┐
                      │   Parallel Decompression      │
                      │   (rayon thread pool)         │
                      ├───────────────────────────────┤
                      │                               │
                      │  Thread 0: DCTX.decompress    │
                      │    frame[0] ──> output[0..N]  │
                      │                               │
                      │  Thread 1: DCTX.decompress    │
                      │    frame[1] ──> output[N..M]  │
                      │                               │
                      │  Thread 2: DCTX.decompress    │
                      │    frame[2] ──> output[M..P]  │
                      │                               │
                      │  ...                          │
                      └───────────────┬───────────────┘
                                      │
                                      │
                                   Output
```

### Key Optimizations

**Thread-local decompression contexts** — Each rayon thread reuses a
persistent `zstd::bulk::Decompressor` instead of creating and destroying
one per frame. With 125 frames across 16 threads, this reduces context
allocations from 125 to 16.

**Pre-allocated output buffer** — When `Frame_Content_Size` is known
(the fast path), a single output buffer is allocated upfront. Each frame
decompresses directly into its slice of the final buffer via
`split_at_mut`, eliminating all intermediate allocations and copies.

**Zero-copy frame slicing** — The frame scanner records byte offsets
into the input buffer. During decompression, frames reference the
original input directly — no copying of compressed data.

## How It Works

1. **Frame scanning** — Walks the input parsing zstd magic numbers,
   frame headers, and block headers to locate independent frame
   boundaries. Skippable frames (pzstd metadata) are recognized but
   filtered out.

2. **Size extraction** — Parses `Frame_Content_Size` from each frame
   header when available, enabling the fast pre-allocation path.

3. **Parallel decompression** — Dispatches frames to rayon's thread
   pool. Each thread decompresses its assigned frames using a reusable
   context, writing directly into the pre-allocated output buffer.

4. **Ordered reassembly** — rayon preserves iteration order, so the
   output is correctly assembled without explicit sorting or reordering.

## Compatibility

- Decompresses standard single-frame zstd files (no parallelism benefit)
- Decompresses multi-frame pzstd files (parallel decompression)
- Handles files with or without skippable frame headers
- Handles files with or without content checksums

## Dependencies

- `zstd` — Per-frame decompression (wraps the C zstd library)
- `rayon` — Work-stealing thread pool for parallel iteration
- `thiserror` — Error type derivation

## License

MIT OR Apache-2.0