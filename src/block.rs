use crate::error::{PzstdError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockType {
    /// Raw block (type 0): uncompressed data stored as-is.
    /// Block_Size equals the number of data bytes that follow.
    Raw,

    /// RLE block (type 1): a single byte repeated Block_Size times.
    /// Only 1 byte of data follows the header, regardless of Block_Size.
    Rle,

    /// Compressed block (type 2): data compressed using Huffman and FSE.
    /// Block_Size is the compressed size; decompressed size may be larger.
    Compressed,
}

impl TryFrom<u8> for BlockType {
    type Error = u8;

    /// Convert a 2-bit block type value to a [`BlockType`].
    ///
    /// Returns `Err(value)` for unknown types (including 3, which is
    /// reserved by the spec and must not appear in valid frames).
    fn try_from(value: u8) -> core::result::Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Raw),
            1 => Ok(Self::Rle),
            2 => Ok(Self::Compressed),
            _ => Err(value),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    pub last: bool,
    pub block_type: BlockType,
    pub size: u32,
}

impl BlockHeader {
    /// Parse a block header from a 24-bit raw value (3 bytes, little-endian).
    ///
    /// Layout:
    ///   bit 0:     Last_Block
    ///   bits 1-2:  Block_Type
    ///   bits 3-23: Block_Size
    pub fn parse(raw: u32, offset: usize) -> Result<Self> {
        let last = (raw & 1) != 0;
        let btype = ((raw >> 1) & 0x3) as u8;
        let size = (raw >> 3) & 0x1FFFFF;

        let block_type =
            BlockType::try_from(btype).map_err(|bt| PzstdError::InvalidBlockType {
                offset,
                block_type: bt,
            })?;

        Ok(Self {
            last,
            block_type,
            size,
        })
    }
}
