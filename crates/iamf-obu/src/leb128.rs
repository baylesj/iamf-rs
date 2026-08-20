use crate::Error;

/// Maximum encoded length of a leb128() value, per IAMF §3 (as in AV1).
pub const MAX_LEB128_BYTES: usize = 8;

/// Decodes an unsigned leb128 value from the start of `data`.
///
/// Returns `(value, bytes_consumed)`. IAMF constrains decoded values to fit
/// in 32 bits and the encoding to at most 8 bytes; both are enforced here.
/// `offset` is only used to report error positions.
pub fn decode(data: &[u8], offset: usize) -> Result<(u32, usize), Error> {
    let mut value: u64 = 0;
    for i in 0..MAX_LEB128_BYTES {
        let byte = *data
            .get(i)
            .ok_or(Error::UnexpectedEof { offset: offset + i })?;
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            let value = u32::try_from(value).map_err(|_| Error::InvalidLeb128 { offset })?;
            return Ok((value, i + 1));
        }
    }
    Err(Error::InvalidLeb128 { offset })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_byte_values() {
        assert_eq!(decode(&[0x00], 0).unwrap(), (0, 1));
        assert_eq!(decode(&[0x7f], 0).unwrap(), (127, 1));
    }

    #[test]
    fn multi_byte_values() {
        assert_eq!(decode(&[0x80, 0x01], 0).unwrap(), (128, 2));
        assert_eq!(decode(&[0xff, 0x7f], 0).unwrap(), (16383, 2));
        // u32::MAX encoded in 5 bytes.
        assert_eq!(
            decode(&[0xff, 0xff, 0xff, 0xff, 0x0f], 0).unwrap(),
            (u32::MAX, 5)
        );
    }

    #[test]
    fn trailing_bytes_ignored() {
        assert_eq!(decode(&[0x05, 0xaa, 0xbb], 0).unwrap(), (5, 1));
    }

    #[test]
    fn truncated_input() {
        assert_eq!(decode(&[], 3), Err(Error::UnexpectedEof { offset: 3 }));
        assert_eq!(decode(&[0x80], 0), Err(Error::UnexpectedEof { offset: 1 }));
    }

    #[test]
    fn overflow_rejected() {
        // Value needs 33 bits.
        assert_eq!(
            decode(&[0xff, 0xff, 0xff, 0xff, 0x1f], 0),
            Err(Error::InvalidLeb128 { offset: 0 })
        );
        // 8 continuation bytes with no terminator.
        assert_eq!(
            decode(&[0x80; 8], 0),
            Err(Error::InvalidLeb128 { offset: 0 })
        );
    }
}
