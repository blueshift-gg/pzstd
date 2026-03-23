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
let data = pzstd::decompress(&compressed).unwrap();

// With custom per-frame capacity limit
let data = pzstd::decompress_with_max_frame_size(&compressed, 256 * 1024 * 1024).unwrap();
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
                      ┌─────┴─────┐      ┌──────┴──────┐
                      │ Fast Path │      │  Fallback   │
                      ├───────────┤      ├─────────────┤
                      │ Single    │      │ Single      │
                      │ alloc     │      │ alloc       │
                      │ (exact)   │      │ (bounded)   │
                      │           │      │             │
                      │ Disjoint  │      │ Disjoint    │
                      │ slices    │      │ slices      │
                      │           │      │             │
                      │ Thread-   │      │ Thread-     │
                      │ local     │      │ local DCTX  │
                      │ DCTX      │      │             │
                      │           │      │ In-place    │
                      │ Zero-copy │      │ compaction  │
                      │ to buffer │      │             │
                      └─────┬─────┘      └──────┬──────┘
                            │                   │
                            │                   │
                      ┌─────┴───────────────────┴─────┐
                      │   Parallel Decompression      │
                      │   (persistent thread pool)    │
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

**Persistent thread pool** — A `LazyLock` thread pool sized to
`available_parallelism()` is created once and reused across calls.
Workers park on a per-slot condvar, so there is no per-call thread
spawn overhead.

**Thread-local decompression contexts** — Each worker thread reuses a
persistent `zstd::bulk::Decompressor` via `thread_local!`, reducing
context allocations from N-frames to N-threads.

**Pre-allocated output buffer** — Both paths allocate a single output
buffer upfront. The fast path (all `Frame_Content_Size` known) allocates
exactly; the fallback path allocates an upper bound derived from block
headers and compacts in-place afterward.

**Uninit allocation** — Output buffers skip zero-initialization via
`set_len` on a `Vec::with_capacity`, avoiding a full memset pass over
the output (saves ~25ms at 500 MB).

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

3. **Parallel decompression** — Partitions frames into contiguous
   chunks and dispatches them across a persistent thread pool. Each
   worker decompresses its assigned frames using a thread-local
   context, writing directly into disjoint regions of the output buffer.

4. **Ordered reassembly** — Frames are assigned to output regions by
   their original index, so the output is correctly assembled without
   explicit sorting or reordering.

## Compatibility

- Decompresses standard single-frame zstd files (no parallelism benefit)
- Decompresses multi-frame pzstd files (parallel decompression)
- Handles files with or without skippable frame headers
- Handles files with or without content checksums

## Dependencies

- `zstd` — Per-frame decompression (wraps the C zstd library)
- `thiserror` — Error type derivation

## License

MIT OR Apache-2.0