/// Magic number identifying the start of a Zstd data frame (RFC 8878 §3.1.1).
pub const ZSTD_MAGIC_NUMBER: u32 = 0xFD2FB528;

/// Lower bound of the magic number range for skippable frames (RFC 8878 §3.1.2).
/// pzstd uses these frames to store the size of the next compressed data frame,
/// enabling fast frame boundary detection without parsing block headers.
pub const ZSTD_MAGIC_SKIP_MIN: u32 = 0x184D2A50;

/// Upper bound of the magic number range for skippable frames.
/// Valid skippable magic numbers span `0x184D2A50..=0x184D2A5F` (16 values).
pub const ZSTD_MAGIC_SKIP_MAX: u32 = 0x184D2A5F;

/// Size of the magic number field in bytes.
pub const MAGIC_SIZE: usize = 4;

/// Size of a skippable frame header field 4 bytes frame data size.
pub const SKIPPABLE_FIELD_SIZE: usize = 4;

/// Size of a block header in bytes.
/// Layout (little-endian, 24 bits): Last_Block (1 bit) | Block_Type (2 bits) | Block_Size (21 bits).
pub const BLOCK_HEADER_SIZE: usize = 3;

/// Size of the optional content checksum (xxHash-64 truncated to 32 bits).
pub const CHECKSUM_SIZE: usize = 4;

/// Default maximum decompressed size per frame (512 MB).
pub const DEFAULT_FRAME_CAPACITY: usize = 512 * 1024 * 1024;

/// Maximum decompressed size of a single zstd block (128 KB per RFC 8878 §3.1.1.2.3).
/// Used to compute per-frame decompression upper bounds when Frame_Content_Size is absent.
pub const BLOCK_MAX_DECOMPRESSED_SIZE: usize = 128 * 1024;

/// Lookup table: DID_Flag -> Dictionary_ID field size in bytes.
/// Index by `did_flag` (2 bits, 0..=3).
pub const DID_FIELD_SIZES: [usize; 4] = [0, 1, 2, 4];

/// Lookup table: (FCS_Flag, Single_Segment_Flag) -> FCS field size in bytes.
/// Index by `(fcs_flag << 1) | (single_segment as usize)`.
///
///   fcs=0 ss=0 -> 0    fcs=0 ss=1 -> 1
///   fcs=1 ss=0 -> 2    fcs=1 ss=1 -> 2
///   fcs=2 ss=0 -> 4    fcs=2 ss=1 -> 4
///   fcs=3 ss=0 -> 8    fcs=3 ss=1 -> 8
pub const FCS_FIELD_SIZES: [usize; 8] = [0, 1, 2, 2, 4, 4, 8, 8];

/// Returns `true` if the given magic number falls in the skippable frame range.
#[inline(always)]
pub const fn is_skippable_magic(magic: u32) -> bool {
    magic >= ZSTD_MAGIC_SKIP_MIN && magic <= ZSTD_MAGIC_SKIP_MAX
}
