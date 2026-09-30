//! MSB-first bit reader for the few IAMF v2 syntax elements that are not
//! byte-aligned (position values: 9-bit azimuth, 7-bit distance, ...).

use crate::{ByteReader, Error, Result};

/// Reads bit fields MSB-first on top of a [`ByteReader`], pulling whole
/// bytes from it as needed. Every IAMF structure that uses sub-byte fields
/// is a whole number of bytes long, so callers finish with
/// [`BitReader::is_aligned`] true and continue on the byte reader.
pub(crate) struct BitReader<'r, 'a> {
    reader: &'r mut ByteReader<'a>,
    current: u8,
    /// Unread bits left in `current` (0..=7).
    bits_left: u32,
}

impl<'r, 'a> BitReader<'r, 'a> {
    pub(crate) fn new(reader: &'r mut ByteReader<'a>) -> Self {
        Self {
            reader,
            current: 0,
            bits_left: 0,
        }
    }

    /// Whether the cursor sits on a byte boundary.
    pub(crate) fn is_aligned(&self) -> bool {
        self.bits_left == 0
    }

    /// Reads an unsigned `n`-bit field (`n <= 32`).
    pub(crate) fn read_bits(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        let mut value = 0u64;
        let mut needed = n;
        while needed > 0 {
            if self.bits_left == 0 {
                self.current = self.reader.read_u8()?;
                self.bits_left = 8;
            }
            let take = needed.min(self.bits_left);
            let shift = self.bits_left - take;
            let bits = (u32::from(self.current) >> shift) & ((1 << take) - 1);
            value = value << take | u64::from(bits);
            self.bits_left -= take;
            needed -= take;
        }
        Ok(value as u32)
    }

    /// Reads a two's-complement signed `n`-bit field (`1 <= n <= 32`).
    pub(crate) fn read_signed(&mut self, n: u32) -> Result<i32> {
        let raw = self.read_bits(n)?;
        let shift = 32 - n;
        Ok(((raw << shift) as i32) >> shift)
    }

    /// Reads a byte (need not be aligned).
    pub(crate) fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_bits(8)? as u8)
    }

    /// Reads a big-endian i16 (need not be aligned).
    #[cfg(test)]
    pub(crate) fn read_i16(&mut self) -> Result<i16> {
        Ok(self.read_signed(16)? as i16)
    }

    /// Fails unless the cursor is byte-aligned (internal consistency check
    /// for syntax that must end on a byte boundary).
    pub(crate) fn finish(self) -> Result<()> {
        if self.is_aligned() {
            Ok(())
        } else {
            Err(Error::InvalidDescriptor {
                offset: self.reader.position(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unaligned_fields() {
        // 9-bit -1 (0x1ff), 8-bit -90, 7-bit 100: 24 bits.
        // 1_1111_1111 1010_0110 110_0100
        let bytes = [0b1111_1111, 0b1101_0011, 0b0110_0100];
        let mut r = ByteReader::new(&bytes);
        let mut b = BitReader::new(&mut r);
        assert_eq!(b.read_signed(9).unwrap(), -1);
        assert_eq!(b.read_signed(8).unwrap(), -90);
        assert_eq!(b.read_bits(7).unwrap(), 100);
        assert!(b.is_aligned());
        b.finish().unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn signed_extremes_and_eof() {
        let bytes = [0x80, 0x00, 0x7f];
        let mut r = ByteReader::new(&bytes);
        let mut b = BitReader::new(&mut r);
        assert_eq!(b.read_i16().unwrap(), i16::MIN);
        assert_eq!(b.read_signed(8).unwrap(), 127);
        assert!(matches!(b.read_bits(1), Err(Error::UnexpectedEof { .. })));
    }

    #[test]
    fn unaligned_finish_is_an_error() {
        let bytes = [0xff];
        let mut r = ByteReader::new(&bytes);
        let mut b = BitReader::new(&mut r);
        b.read_bits(3).unwrap();
        assert!(b.finish().is_err());
    }
}
