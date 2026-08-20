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
