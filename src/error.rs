use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PzstdError {
    #[error("input is empty")]
    EmptyInput,

    #[error(
        "unexpected end of input at offset {offset:#x}: needed {needed} bytes, got {available}"
    )]
    UnexpectedEof {
        offset: usize,
        needed: usize,
        available: usize,
    },

    #[error("invalid magic at offset {offset:#x}: found {found:#010x}")]
    InvalidMagic { offset: usize, found: u32 },

    #[error("invalid block type {block_type} at offset {offset:#x}")]
    InvalidBlockType { offset: usize, block_type: u8 },

    #[error("no frames found in input")]
    NoFrames,

    #[error("decompression failed for frame {frame_index}: {source}")]
    DecompressFailed {
        frame_index: usize,
        source: std::io::Error,
    },
}

pub type Result<T> = core::result::Result<T, PzstdError>;
