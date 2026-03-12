use crate::{
    block::{BlockHeader, BlockType},
    consts::{
        BLOCK_HEADER_SIZE, BLOCK_MAX_DECOMPRESSED_SIZE, CHECKSUM_SIZE, DID_FIELD_SIZES,
        FCS_FIELD_SIZES, MAGIC_NUMBER_SIZE, SKIPPABLE_FIELD_SIZE, ZSTD_MAGIC_NUMBER,
        is_skippable_magic,
    },
    error::{PzstdError, Result},
    helpers::{read_block_header, read_u8, read_u16, read_u32, read_u64},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Data,
    Skippable,
}

impl FrameKind {
    /// Determine frame kind from a magic number.
    pub fn from_magic_with_offset(magic: u32, offset: usize) -> Result<Self> {
        if magic == ZSTD_MAGIC_NUMBER {
            Ok(FrameKind::Data)
        } else if is_skippable_magic(magic) {
            Ok(FrameKind::Skippable)
        } else {
            Err(PzstdError::InvalidMagic {
                offset,
                found: magic,
            })
        }
    }
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub offset: usize,
    pub len: usize,
    pub kind: FrameKind,
    /// Decompressed size of this frame, if known from the frame header.
    /// Only present for Data frames that have Frame_Content_Size set.
    pub decompressed_size: Option<u64>,
    /// Upper bound on decompressed size, computed from block headers.
    /// Always available for Data frames (Raw/RLE give exact block sizes,
    /// Compressed blocks are bounded by 128 KB per RFC 8878).
    /// Zero for Skippable frames.
    pub decompressed_bound: usize,
}

/// Controls which frame types are returned by the scanner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameScanMode {
    /// Return all frames (data + skippable).
    All,
    /// Return only data frames, skip metadata frames.
    DataOnly,
}

impl Frame {
    pub fn scan_frames(input: &[u8], mode: FrameScanMode) -> Result<Vec<Frame>> {
        // Heuristic: assume ~256KB average frame size for initial capacity
        let mut frames = Vec::with_capacity((input.len() / (256 * 1024)).max(1));
        let mut pos = 0;

        while pos < input.len() {
            let frame_start = pos;

            let magic = read_u32(input, pos)?;
            pos += MAGIC_NUMBER_SIZE;
            let kind = FrameKind::from_magic_with_offset(magic, frame_start)?;

            let mut decompressed_size = None;
            let mut decompressed_bound: usize = 0;

            match kind {
                FrameKind::Skippable => {
                    let frame_size = read_u32(input, pos)? as usize;
                    let payload_start = pos + SKIPPABLE_FIELD_SIZE;
                    let frame_end = payload_start.checked_add(frame_size).ok_or(
                        PzstdError::UnexpectedEof {
                            offset: payload_start,
                            needed: frame_size,
                            available: input.len().saturating_sub(payload_start),
                        },
                    )?;
                    if frame_end > input.len() {
                        return Err(PzstdError::UnexpectedEof {
                            offset: payload_start,
                            needed: frame_size,
                            available: input.len().saturating_sub(payload_start),
                        });
                    }
                    pos = frame_end;
                }
                FrameKind::Data => {
                    let desc_byte = read_u8(input, pos)?;
                    let desc = FrameDescriptor::parse(desc_byte);

                    // parse Frame_Content_Size before skipping the header
                    // FCS field sits at: descriptor(1) + window + did
                    let window_size = if desc.single_segment { 0 } else { 1 };
                    let did_size = desc.did_field_size();
                    let fcs_offset = pos + 1 + window_size + did_size;
                    decompressed_size = desc.parse_fcs(input, fcs_offset)?;

                    pos += desc.header_size();

                    loop {
                        let raw = read_block_header(input, pos)?;
                        let block = BlockHeader::parse(raw, pos)?;
                        pos += BLOCK_HEADER_SIZE;

                        match block.block_type {
                            BlockType::Rle => {
                                decompressed_bound += block.size as usize;
                                pos += 1;
                            }
                            BlockType::Compressed => {
                                decompressed_bound += BLOCK_MAX_DECOMPRESSED_SIZE;
                                pos += block.size as usize;
                            }
                            _ => {
                                decompressed_bound += block.size as usize;
                                pos += block.size as usize;
                            }
                        }

                        if block.last {
                            break;
                        }
                    }

                    if desc.has_checksum {
                        pos += CHECKSUM_SIZE;
                    }
                }
            }

            let should_record = match mode {
                FrameScanMode::All => true,
                FrameScanMode::DataOnly => matches!(kind, FrameKind::Data),
            };

            if should_record {
                frames.push(Frame {
                    offset: frame_start,
                    len: pos - frame_start,
                    kind,
                    decompressed_size,
                    decompressed_bound,
                });
            }
        }

        if frames.is_empty() {
            return Err(PzstdError::NoFrames);
        }

        Ok(frames)
    }

    /// Extract this frame's raw bytes from the input buffer.
    ///
    /// # Errors
    /// Returns [`PzstdError::UnexpectedEof`] if the frame's range
    /// exceeds the input buffer bounds.
    pub fn bytes<'a>(&self, input: &'a [u8]) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(self.len)
            .ok_or(PzstdError::UnexpectedEof {
                offset: self.offset,
                needed: self.len,
                available: input.len().saturating_sub(self.offset),
            })?;

        input
            .get(self.offset..end)
            .ok_or(PzstdError::UnexpectedEof {
                offset: self.offset,
                needed: self.len,
                available: input.len().saturating_sub(self.offset),
            })
    }
}

/// Parsed flags from the Frame_Header_Descriptor byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameDescriptor {
    /// Frame_Content_Size_Flag (bits 7-6). Determines FCS field size.
    pub fcs_flag: u8,
    /// Single_Segment_Flag (bit 5). If set, no Window_Descriptor field.
    pub single_segment: bool,
    /// Content_Checksum_Flag (bit 2). If set, 4-byte checksum after last block.
    pub has_checksum: bool,
    /// Dictionary_ID_Flag (bits 1-0). Determines DID field size.
    pub did_flag: u8,
}

impl From<u8> for FrameDescriptor {
    fn from(byte: u8) -> Self {
        Self::parse(byte)
    }
}

impl FrameDescriptor {
    /// Parse the Frame_Header_Descriptor byte.
    ///
    /// Layout:
    ///   bits 7-6: FCS_Flag
    ///   bit 5:    Single_Segment_Flag
    ///   bit 4:    unused
    ///   bit 3:    unused
    ///   bit 2:    Content_Checksum_Flag
    ///   bits 1-0: DID_Flag
    pub const fn parse(byte: u8) -> Self {
        Self {
            fcs_flag: (byte >> 6) & 0x3,
            single_segment: (byte >> 5) & 1 == 1,
            has_checksum: (byte >> 2) & 1 == 1,
            did_flag: byte & 0x3,
        }
    }

    /// Size of the Dictionary_ID field in bytes.
    pub const fn did_field_size(&self) -> usize {
        DID_FIELD_SIZES[self.did_flag as usize]
    }

    /// Size of the Frame_Content_Size field in bytes.
    pub const fn fcs_field_size(&self) -> usize {
        FCS_FIELD_SIZES[((self.fcs_flag as usize) << 1) | (self.single_segment as usize)]
    }

    /// Size of the full frame header including the descriptor byte.
    /// This is needed to skip past the header to reach the first block.
    pub const fn header_size(&self) -> usize {
        1 + (!self.single_segment as usize) + self.did_field_size() + self.fcs_field_size()
    }

    /// Parse the Frame_Content_Size value from the input.
    ///
    /// Note: the 2-byte case adds 256 to the value per the zstd spec (RFC 8878).
    /// Returns None if FCS field is not present in this frame.
    pub fn parse_fcs(&self, input: &[u8], offset: usize) -> Result<Option<u64>> {
        match self.fcs_field_size() {
            0 => Ok(None),
            1 => Ok(Some(read_u8(input, offset)? as u64)),
            2 => Ok(Some(read_u16(input, offset)? as u64 + 256)),
            4 => Ok(Some(read_u32(input, offset)? as u64)),
            8 => Ok(Some(read_u64(input, offset)?)),
            _ => unreachable!(),
        }
    }
}
