use crate::{Error, Result, leb128};

/// A bounds-checked cursor over untrusted input.
#[derive(Debug, Clone)]
pub struct ByteReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    /// Wraps `data` with the cursor at offset 0.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Absolute byte offset from the start of the input.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes left between the cursor and the end of the input.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// Whether the cursor has reached the end of the input.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Reads one byte.
    pub fn read_u8(&mut self) -> Result<u8> {
        let byte = *self
            .data
            .get(self.pos)
            .ok_or(Error::UnexpectedEof { offset: self.pos })?;
        self.pos += 1;
        Ok(byte)
    }

    /// Reads an IAMF leb128 (§3.1: at most 8 bytes, value fits in u32).
    pub fn read_leb128(&mut self) -> Result<u32> {
        let (value, consumed) = leb128::decode(&self.data[self.pos..], self.pos)?;
        self.pos += consumed;
        Ok(value)
    }

    /// Reads a big-endian u16.
    pub fn read_u16_be(&mut self) -> Result<u16> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// Reads a big-endian i16.
    pub fn read_i16_be(&mut self) -> Result<i16> {
        let bytes = self.read_bytes(2)?;
        Ok(i16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// Reads a big-endian u32.
    pub fn read_u32_be(&mut self) -> Result<u32> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Reads a four-character code.
    pub fn read_fourcc(&mut self) -> Result<[u8; 4]> {
        let bytes = self.read_bytes(4)?;
        Ok([bytes[0], bytes[1], bytes[2], bytes[3]])
    }

    /// Reads a NUL-terminated UTF-8 string of at most 128 bytes including
    /// the terminator (IAMF annotations; libiamf's string128). Invalid UTF-8
    /// is replaced rather than rejected.
    pub fn read_string(&mut self) -> Result<String> {
        const MAX: usize = 128;
        let start = self.pos;
        let limit = (self.data.len() - start).min(MAX);
        let window = &self.data[start..start + limit];
        let len = window
            .iter()
            .position(|&b| b == 0)
            .ok_or(Error::UnexpectedEof {
                offset: start + limit,
            })?;
        self.pos = start + len + 1;
        Ok(String::from_utf8_lossy(&window[..len]).into_owned())
    }

    /// Skips `len` bytes.
    pub fn skip(&mut self, len: usize) -> Result<()> {
        self.read_bytes(len).map(|_| ())
    }

    /// Consumes and returns all remaining bytes.
    pub fn rest(&mut self) -> &'a [u8] {
        let bytes = &self.data[self.pos..];
        self.pos = self.data.len();
        bytes
    }

    /// Reads exactly `len` bytes, borrowing from the input.
    pub fn read_bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|&end| end <= self.data.len())
            .ok_or(Error::UnexpectedEof {
                offset: self.data.len(),
            })?;
        let bytes = &self.data[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }
}

/// An MSB-first bit cursor over a byte slice, for the bit-packed fields of
/// IAMF v2.0 position parameters (§3.8 `signed int (9)` azimuths and the
/// like). Reading past the end is an error, never a panic.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
    /// Byte offset of `data` in the enclosing input, for error reports.
    base: usize,
}

impl<'a> BitReader<'a> {
    /// Wraps `data`, whose first byte sits at `base` in the enclosing input.
    pub fn new(data: &'a [u8], base: usize) -> Self {
        Self { data, bit: 0, base }
    }

    /// Bits consumed so far.
    pub fn bit_position(&self) -> usize {
        self.bit
    }

    /// Reads an unsigned `n`-bit field (`n` ≤ 32).
    pub fn read_bits(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        let mut value = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.bit / 8).ok_or(Error::UnexpectedEof {
                offset: self.base + self.bit / 8,
            })?;
            let set = byte >> (7 - self.bit % 8) & 1;
            value = value << 1 | u32::from(set);
            self.bit += 1;
        }
        Ok(value)
    }

    /// Reads a two's complement `n`-bit field (`1 ≤ n ≤ 32`).
    pub fn read_signed(&mut self, n: u32) -> Result<i32> {
        let raw = self.read_bits(n)?;
        let shift = 32 - n;
        Ok(((raw << shift) as i32) >> shift)
    }
}

#[cfg(test)]
mod bit_tests {
    use super::BitReader;

    #[test]
    fn reads_the_packed_polar_fields_of_a_libiamf_vector() {
        // test_000800: azimuth +90, elevation 0, distance 127 as s9/s8/u7.
        let mut r = BitReader::new(&[0x2d, 0x00, 0x7f], 0);
        assert_eq!(r.read_signed(9).unwrap(), 90);
        assert_eq!(r.read_signed(8).unwrap(), 0);
        assert_eq!(r.read_bits(7).unwrap(), 127);
        assert_eq!(r.bit_position(), 24);
        // Azimuth -90 and +180.
        assert_eq!(
            BitReader::new(&[0xd3, 0x00], 0).read_signed(9).unwrap(),
            -90
        );
        assert_eq!(
            BitReader::new(&[0x5a, 0x00], 0).read_signed(9).unwrap(),
            180
        );
    }

    #[test]
    fn running_out_of_bits_is_an_error() {
        let mut r = BitReader::new(&[0xff], 10);
        assert_eq!(r.read_bits(8).unwrap(), 0xff);
        assert!(r.read_bits(1).is_err());
    }
}
