use crate::{leb128, Error, Result};

/// A bounds-checked cursor over untrusted input.
#[derive(Debug, Clone)]
pub struct ByteReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Absolute byte offset from the start of the input.
    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        let byte = *self
            .data
            .get(self.pos)
            .ok_or(Error::UnexpectedEof { offset: self.pos })?;
        self.pos += 1;
        Ok(byte)
    }

    pub fn read_leb128(&mut self) -> Result<u32> {
        let (value, consumed) = leb128::decode(&self.data[self.pos..], self.pos)?;
        self.pos += consumed;
        Ok(value)
    }

    pub fn read_u16_be(&mut self) -> Result<u16> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    pub fn read_i16_be(&mut self) -> Result<i16> {
        Ok(self.read_u16_be()? as i16)
    }

    pub fn read_u32_be(&mut self) -> Result<u32> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

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
