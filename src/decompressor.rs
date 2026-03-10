use std::cell::RefCell;

use rayon::iter::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator,
};

use crate::{
    consts::{DEFAULT_FRAME_CAPACITY, WILDCOPY_OVERLENGTH},
    error::{PzstdError, Result},
    frame::{Frame, FrameScanMode},
};

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

/// Decompress a zstd or pzstd compressed input in parallel.
///
/// Scans the input for independent zstd frame boundaries, then
/// decompresses each data frame concurrently using rayon's thread pool.
/// Each rayon thread reuses a decompression context to avoid per-frame
/// setup overhead. Frames are reassembled in original order.
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
    // Overallocate by WILDCOPY_OVERLENGTH so zstd's internal wildcopy loop
    // can use fast SIMD copies for the last frame without bounds-checking.
    // Interior frames already have implicit headroom from the next frame's data.
    // Zeroed allocation pre-faults pages sequentially, warming the TLB and
    // cache hierarchy before parallel decompression touches them.
    let mut output = vec![0u8; total_decompressed + WILDCOPY_OVERLENGTH];

    // skip rayon overhead for a single frame
    if frames.len() == 1 {
        let src = frames[0].bytes(input)?;
        with_decompressor(0, |dctx| dctx.decompress_to_buffer(src, &mut output))?;
        output.truncate(total_decompressed);
        return Ok(output);
    }

    // Split output into disjoint per-frame destination slices.
    // Each interior frame gets exactly its decompressed size (the adjacent
    // next frame provides implicit wildcopy headroom). The last frame gets
    // size + WILDCOPY_OVERLENGTH from the trailing padding.
    let mut dst_slices: Vec<&mut [u8]> = Vec::with_capacity(frames.len());
    let mut rest = output.as_mut_slice();
      if let Some((last, rest_sizes)) = sizes.split_last() {
        for &size in rest_sizes {
            let (chunk, remainder) = rest.split_at_mut(size);
            dst_slices.push(chunk);
            rest = remainder;
        }
        let (chunk, _) = rest.split_at_mut(last + WILDCOPY_OVERLENGTH);
        dst_slices.push(chunk);
    }

    // Decompress each frame in parallel into its pre-allocated output slice.
    frames
        .par_iter()
        .zip(dst_slices.into_par_iter())
        .enumerate()
        .try_for_each(|(idx, (frame, dst))| {
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
    // skip rayon overhead for a single frame
    if frames.len() == 1 {
        let bound = frames[0].decompressed_bound.min(max_frame_size);
        let mut output = vec![0u8; bound + WILDCOPY_OVERLENGTH];

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

    // Zeroed allocation: pre-faults pages sequentially before parallel decompression.
    let mut output = vec![0u8; total_bound];
    let base_addr = output.as_mut_ptr() as usize;

    let actual_sizes: Vec<usize> = frames
        .par_iter()
        .enumerate()
        .map(|(idx, frame)| {
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
            with_decompressor(idx, |dctx| dctx.decompress_to_buffer(src, dst))
        })
        .collect::<Result<Vec<usize>>>()?;

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
