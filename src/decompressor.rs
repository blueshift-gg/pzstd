use std::cell::RefCell;

use rayon::iter::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator,
};

use crate::{
    error::{PzstdError, Result},
    frame::{Frame, FrameScanMode},
    helpers::DEFAULT_FRAME_CAPACITY,
};

thread_local! {
    /// Per-thread reusable decompression context.
    /// Avoids creating/destroying a context for every frame.
    static DCTX: RefCell<zstd::bulk::Decompressor<'static>> =
        RefCell::new(zstd::bulk::Decompressor::new().unwrap());
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

    // Check if all frames have known decompressed sizes
    let sizes: Option<Vec<usize>> = frames
        .iter()
        .map(|f| f.decompressed_size.map(|s| s as usize))
        .collect();

    match sizes {
        Some(sizes) => decompress_fast_path(input, &frames, &sizes),
        None => decompress_fallback(input, &frames, max_frame_size),
    }
}

/// Fast path: all frames have known decompressed sizes.
/// Pre-allocates a single output buffer and decompresses directly
/// into non-overlapping mutable slices.
fn decompress_fast_path(input: &[u8], frames: &[Frame], sizes: &[usize]) -> Result<Vec<u8>> {
    let total_size: usize = sizes.iter().sum();
    let mut output = vec![0u8; total_size];

    // Split output into non-overlapping mutable slices, one per frame.
    // This satisfies the borrow checker — each thread gets exclusive
    // ownership of its slice.
    let mut slices: Vec<&mut [u8]> = Vec::with_capacity(frames.len());
    let mut remaining = output.as_mut_slice();

    for &size in sizes {
        let (slice, rest) = remaining.split_at_mut(size);
        slices.push(slice);
        remaining = rest;
    }

    // Parallel decompress: each thread writes to its own slice
    slices
        .into_par_iter()
        .zip(frames.par_iter())
        .enumerate()
        .try_for_each(|(idx, (dst, frame))| {
            let src = frame.bytes(input)?;

            DCTX.with(|dctx| {
                dctx.borrow_mut()
                    .decompress_to_buffer(src, dst)
                    .map_err(|e| PzstdError::DecompressFailed {
                        frame_index: idx,
                        source: e,
                    })
            })?;

            Ok(())
        })?;

    Ok(output)
}

/// Fallback path: frame sizes unknown, allocate per frame.
fn decompress_fallback(input: &[u8], frames: &[Frame], max_frame_size: usize) -> Result<Vec<u8>> {
    let results: Vec<Vec<u8>> = frames
        .par_iter()
        .enumerate()
        .map(|(idx, frame)| {
            let bytes = frame.bytes(input)?;
            DCTX.with(|dctx| {
                dctx.borrow_mut()
                    .decompress(bytes, max_frame_size)
                    .map_err(|e| PzstdError::DecompressFailed {
                        frame_index: idx,
                        source: e,
                    })
            })
        })
        .collect::<Result<Vec<Vec<u8>>>>()?;

    let total_size: usize = results.iter().map(|r| r.len()).sum();
    let mut output = Vec::with_capacity(total_size);
    for chunk in &results {
        output.extend_from_slice(chunk);
    }

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
/// let data = pzstd::decompress(&compressed).unwrap();
/// ```
///
/// # Errors
/// See [`decompress_with_max_frame_size`] for the full list of error conditions.
pub fn decompress(input: &[u8]) -> Result<Vec<u8>> {
    decompress_with_max_frame_size(input, DEFAULT_FRAME_CAPACITY)
}
