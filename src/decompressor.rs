use std::cell::RefCell;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{
    Arc, Condvar, LazyLock, Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;

use crate::{
    consts::{DEFAULT_FRAME_CAPACITY, WILDCOPY_OVERLENGTH},
    error::{PzstdError, Result},
    frame::{Frame, FrameScanMode},
};

type Task = Box<dyn FnOnce() + Send>;

struct WorkerSlot {
    task: Mutex<Option<Task>>,
    notify: Condvar,
}

struct ThreadPool {
    slots: Arc<[WorkerSlot]>,
    size: usize,
}

impl ThreadPool {
    fn new(size: usize) -> Self {
        let slots: Arc<[WorkerSlot]> = (0..size)
            .map(|_| WorkerSlot {
                task: Mutex::new(None),
                notify: Condvar::new(),
            })
            .collect();

        for id in 0..size {
            let slots = Arc::clone(&slots);
            thread::Builder::new()
                .name("pzstd-worker".into())
                .spawn(move || {
                    let slot = &slots[id];
                    loop {
                        let task = {
                            let mut guard = slot.task.lock().unwrap();
                            loop {
                                if let Some(task) = guard.take() {
                                    break task;
                                }
                                guard = slot.notify.wait(guard).unwrap();
                            }
                        };
                        // catch_unwind keeps the worker alive if a task panics.
                        let _ = panic::catch_unwind(AssertUnwindSafe(task));
                    }
                })
                .expect("failed to spawn pzstd worker thread");
        }

        Self { slots, size }
    }
}

static POOL: LazyLock<ThreadPool> = LazyLock::new(|| {
    let n = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    ThreadPool::new(n)
});

thread_local! {
    /// Per-thread reusable decompression context.
    /// Avoids creating/destroying a context for every frame.
    static DCTX: RefCell<zstd::bulk::Decompressor<'static>> =
        RefCell::new(zstd::bulk::Decompressor::new().expect("failed to create zstd decompressor"));
}

/// Borrow the thread-local decompressor and invoke `f` with it.
#[inline(always)]
fn with_decompressor<T>(
    frame_index: usize,
    f: impl FnOnce(&mut zstd::bulk::Decompressor<'static>) -> std::io::Result<T>,
) -> Result<T> {
    DCTX.with(|cell| {
        f(&mut cell.borrow_mut()).map_err(|e| PzstdError::DecompressFailed {
            frame_index,
            source: e,
        })
    })
}

/// Allocate `len` bytes without zeroing. Every byte must be written before read.
///
/// # Safety
/// Caller must ensure all bytes in `0..len` are written before being read.
/// Safe to drop without writing (no drop glue for `u8`).
#[inline]
unsafe fn alloc_uninit(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    #[allow(clippy::uninit_vec)]
    unsafe {
        v.set_len(len)
    };
    v
}

/// Run `f(frame_index, frame)` in parallel across the global thread pool.
/// Frames are partitioned into contiguous chunks, one per worker.
/// Blocks until all chunks complete. Returns the first error encountered.
fn parallel_for_each(
    frames: &[Frame],
    f: impl Fn(usize, &Frame) -> Result<()> + Sync,
) -> Result<()> {
    let n_threads = POOL.size.min(frames.len());
    let chunk_size = frames.len().div_ceil(n_threads);
    let error: OnceLock<PzstdError> = OnceLock::new();

    // Completion barrier: each task decrements; last one signals the caller.
    let remaining = AtomicUsize::new(0);
    let done_lock = Mutex::new(false);
    let done_cvar = Condvar::new();

    let chunks: Vec<_> = frames.chunks(chunk_size).collect();
    remaining.store(chunks.len(), Ordering::Release);

    for (worker_id, chunk) in chunks.into_iter().enumerate() {
        let base_idx = worker_id * chunk_size;
        let error = &error;
        let f = &f;
        let remaining = &remaining;
        let done_lock = &done_lock;
        let done_cvar = &done_cvar;

        // SAFETY: We block below (done_cvar.wait) until every task has
        // completed, so all borrowed data outlives the closures. This is
        // the same lifetime-erasure technique used by std::thread::scope.
        let task: Task = unsafe {
            std::mem::transmute::<Box<dyn FnOnce() + Send + '_>, Task>(Box::new(move || {
                for (i, frame) in chunk.iter().enumerate() {
                    if error.get().is_some() {
                        break;
                    }
                    let idx = base_idx + i;
                    if let Err(e) = f(idx, frame) {
                        let _ = error.set(e);
                    }
                }
                // Signal completion — last task wakes the caller.
                if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                    *done_lock.lock().unwrap() = true;
                    done_cvar.notify_one();
                }
            }))
        };

        let slot = &POOL.slots[worker_id];
        *slot.task.lock().unwrap() = Some(task);
        slot.notify.notify_one();
    }

    // Block until all tasks finish — guarantees soundness of lifetime erasure.
    let mut done = done_lock.lock().unwrap();
    while !*done {
        done = done_cvar.wait(done).unwrap();
    }

    match error.into_inner() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Decompress a zstd or pzstd compressed input in parallel.
///
/// Scans the input for independent zstd frame boundaries, then
/// decompresses each data frame concurrently using a persistent thread
/// pool. Each worker reuses a thread-local decompression context to
/// avoid per-frame setup overhead. Frames are reassembled in original order.
///
/// When `Frame_Content_Size` is available in all frames, uses a fast path
/// that pre-allocates a single output buffer and decompresses directly
/// into non-overlapping slices — zero intermediate allocations.
///
/// Falls back to per-frame allocation when frame sizes are unknown.
///
/// # Arguments
/// * `input` - Raw bytes of the compressed zstd/pzstd data.
/// * `max_frame_size` - Maximum decompressed size per frame in bytes.
///   Only used in the fallback path when `Frame_Content_Size` is not
///   available. Acts as a safety limit to prevent memory exhaustion
///   from malicious or corrupted frames.
///
/// # Errors
/// - [`PzstdError::EmptyInput`] if the input is empty.
/// - [`PzstdError::InvalidMagic`] if an unrecognized frame magic is encountered.
/// - [`PzstdError::NoFrames`] if no valid data frames are found.
/// - [`PzstdError::DecompressFailed`] if any frame fails to decompress,
///   including when decompressed size exceeds `max_frame_size`.
pub fn decompress_with_max_frame_size(input: &[u8], max_frame_size: usize) -> Result<Vec<u8>> {
    let frames = Frame::scan_frames(input, FrameScanMode::DataOnly)?;

    // Single pass: check all frames have known sizes within limit and collect them
    let mut sizes: Vec<usize> = Vec::with_capacity(frames.len());
    let mut total_decompressed: usize = 0;
    let all_sizes_known = frames.iter().all(|f| match f.decompressed_size {
        Some(s) if (s as usize) <= max_frame_size => {
            let size = s as usize;
            sizes.push(size);
            total_decompressed += size;
            true
        }
        _ => false,
    });

    if all_sizes_known {
        decompress_fast_path(input, &frames, &sizes, total_decompressed)
    } else {
        decompress_fallback(input, &frames, max_frame_size)
    }
}

/// Fast path: all frames have known decompressed sizes.
/// Pre-allocates a single output buffer and decompresses directly
/// into non-overlapping regions — zero intermediate allocations.
fn decompress_fast_path(
    input: &[u8],
    frames: &[Frame],
    sizes: &[usize],
    total_decompressed: usize,
) -> Result<Vec<u8>> {
    let alloc_len = total_decompressed + WILDCOPY_OVERLENGTH;

    // SAFETY: every byte in 0..total_decompressed is written by decompression,
    // and the WILDCOPY_OVERLENGTH tail is only touched by zstd's internal
    // wildcopy (overwrite-only, never read by our code). Skipping the zero-fill
    // avoids a full memset pass over the output (~25ms saved at 500MB).
    let mut output = unsafe { alloc_uninit(alloc_len) };

    // Skip thread pool overhead for a single frame.
    if frames.len() == 1 {
        let src = frames[0].bytes(input)?;
        with_decompressor(0, |dctx| dctx.decompress_to_buffer(src, &mut output))?;
        output.truncate(total_decompressed);
        return Ok(output);
    }

    // Compute per-frame offsets into the output buffer.
    let mut offsets = Vec::with_capacity(frames.len());
    let mut offset = 0usize;
    for &size in sizes {
        offsets.push(offset);
        offset += size;
    }

    let base_addr = output.as_mut_ptr() as usize;
    let total_len = output.len();

    parallel_for_each(frames, |idx, frame| {
        let region_offset = offsets[idx];
        // Interior frames: exactly their size (next frame provides wildcopy headroom).
        // Last frame: size + WILDCOPY_OVERLENGTH from trailing padding.
        let region_size = if idx + 1 < offsets.len() {
            offsets[idx + 1] - region_offset
        } else {
            total_len - region_offset
        };

        // SAFETY: each thread writes to a disjoint region derived from
        // non-overlapping size-based offsets.
        let dst = unsafe {
            std::slice::from_raw_parts_mut((base_addr + region_offset) as *mut u8, region_size)
        };

        let src = frame.bytes(input)?;
        with_decompressor(idx, |dctx| dctx.decompress_to_buffer(src, dst))?;
        Ok(())
    })?;

    output.truncate(total_decompressed);
    Ok(output)
}

/// Fallback path: frame sizes unknown, use block-header bounds to
/// pre-allocate a single output buffer and decompress directly into it.
/// Gaps between bounded and actual sizes are compacted in-place afterward.
fn decompress_fallback(input: &[u8], frames: &[Frame], max_frame_size: usize) -> Result<Vec<u8>> {
    // Skip thread pool overhead for a single frame.
    if frames.len() == 1 {
        let bound = frames[0].decompressed_bound.min(max_frame_size);
        // SAFETY: all bytes up to `written` are filled by decompression;
        // truncated immediately after.
        let mut output = unsafe { alloc_uninit(bound + WILDCOPY_OVERLENGTH) };

        let src = frames[0].bytes(input)?;
        let written = with_decompressor(0, |dctx| dctx.decompress_to_buffer(src, &mut output))?;

        output.truncate(written);
        return Ok(output);
    }

    let mut offsets = Vec::with_capacity(frames.len());
    let mut total_bound: usize = 0;
    for frame in frames {
        offsets.push(total_bound);
        // Add WILDCOPY_OVERLENGTH per frame for zstd's fast wildcopy headroom.
        total_bound += frame.decompressed_bound.min(max_frame_size) + WILDCOPY_OVERLENGTH;
    }

    // SAFETY: every decompressed region is fully written by zstd; gaps between
    // bounded and actual sizes are compacted below before the Vec is returned.
    let mut output = unsafe { alloc_uninit(total_bound) };
    let base_addr = output.as_mut_ptr() as usize;

    // Per-frame actual decompressed sizes, written by each thread to its own index.
    let mut actual_sizes: Vec<usize> = vec![0usize; frames.len()];
    let sizes_addr = actual_sizes.as_mut_ptr() as usize;

    parallel_for_each(frames, |idx, frame| {
        let region_offset = offsets[idx];
        let region_size = if idx + 1 < offsets.len() {
            offsets[idx + 1] - region_offset
        } else {
            total_bound - region_offset
        };

        // SAFETY: each thread writes to a disjoint region derived from
        // non-overlapping bound-based offsets.
        let dst = unsafe {
            std::slice::from_raw_parts_mut((base_addr + region_offset) as *mut u8, region_size)
        };

        let src = frame.bytes(input)?;
        let written = with_decompressor(idx, |dctx| dctx.decompress_to_buffer(src, dst))?;

        // SAFETY: each thread writes to its own index — no overlap.
        unsafe { *(sizes_addr as *mut usize).add(idx) = written };
        Ok(())
    })?;

    // Compact: close gaps where actual < bound by shifting data left in-place.
    // This is always safe because write_pos <= read_pos (cumulative actuals ≤
    // cumulative bounds), so we use ptr::copy to handle potential overlap.
    let total_actual: usize = actual_sizes.iter().sum();
    let ptr = output.as_mut_ptr();
    let mut write_pos: usize = 0;
    for (idx, &actual) in actual_sizes.iter().enumerate() {
        let read_pos = offsets[idx];
        if write_pos != read_pos && actual > 0 {
            unsafe { std::ptr::copy(ptr.add(read_pos), ptr.add(write_pos), actual) };
        }
        write_pos += actual;
    }

    output.truncate(total_actual);
    Ok(output)
}

/// Decompress a zstd or pzstd compressed input in parallel.
///
/// Convenience wrapper around [`decompress_with_max_frame_size`] using a
/// default per-frame limit of 512 MB. Suitable for most use cases
/// including Solana snapshot decompression.
///
/// For inputs with exceptionally large frames, or to set a stricter
/// memory limit, use [`decompress_with_max_frame_size`] directly.
///
/// # Example
/// ```no_run
/// let compressed = std::fs::read("snapshot.tar.zst").unwrap();
/// let data = pzstd::decompressor::decompress(&compressed).unwrap();
/// ```
///
/// # Errors
/// See [`decompress_with_max_frame_size`] for the full list of error conditions.
pub fn decompress(input: &[u8]) -> Result<Vec<u8>> {
    decompress_with_max_frame_size(input, DEFAULT_FRAME_CAPACITY)
}
