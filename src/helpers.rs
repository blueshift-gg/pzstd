use crate::error::{PzstdError, Result};

/// Read exactly `N` bytes at the given offset, returning them as a fixed-size array.
///
/// This is the single bounds-checked read primitive that all typed readers
/// are built on. The const generic `N` is known at compile time, so each
/// monomorphization produces a single N-byte load with no branching.
#[inline(always)]
pub fn read_bytes<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N]> {
    let b = bytes
        .get(offset..offset + N)
        .ok_or(PzstdError::UnexpectedEof {
            offset,
            needed: N,
            available: bytes.len().saturating_sub(offset),
        })?;
    // SAFETY: `b` is exactly `N` bytes from the `.get()` above.
    // `try_into()` is infallible here; the compiler elides the panic path.
    Ok(b.try_into().unwrap())
}

/// Read a 3-byte little-endian block header at the given offset.
/// Returns the raw 24-bit value zero-extended to u32.
#[inline(always)]
pub fn read_block_header(bytes: &[u8], offset: usize) -> Result<u32> {
    let [a, b, c] = read_bytes::<3>(bytes, offset)?;
    Ok(u32::from_le_bytes([a, b, c, 0]))
}

/// Read a little-endian u32 at the given offset.
#[inline(always)]
pub fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(read_bytes::<4>(bytes, offset)?))
}

/// Read a little-endian u64 at the given offset.
#[inline(always)]
pub fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(read_bytes::<8>(bytes, offset)?))
}

/// Read a little-endian u16 at the given offset.
#[inline(always)]
pub fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(read_bytes::<2>(bytes, offset)?))
}

/// Read a single byte at the given offset.
#[inline(always)]
pub fn read_u8(bytes: &[u8], offset: usize) -> Result<u8> {
    let [b] = read_bytes::<1>(bytes, offset)?;
    Ok(b)
}
