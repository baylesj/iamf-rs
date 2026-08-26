//! Audio frame OBU payloads (IAMF §3.9).

use crate::{ByteReader, Obu, ObuType, Result};

/// One coded audio frame for one substream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame<'a> {
    /// The substream this frame belongs to (explicit or from the OBU type).
    pub substream_id: u32,
    /// Coded frame bytes, handed to the codec as-is.
    pub data: &'a [u8],
    /// Decoded samples to drop at the start of this frame (§3.9).
    pub num_samples_to_trim_at_start: u32,
    /// Decoded samples to drop at the end of this frame (§3.9).
    pub num_samples_to_trim_at_end: u32,
}

impl<'a> AudioFrame<'a> {
    /// Extracts the audio frame from an OBU; `None` for non-audio-frame
    /// OBUs. Types 6..=23 carry the substream ID implicitly in the OBU
    /// type; type 5 spells it out as a leb128 prefix of the payload.
    pub fn from_obu(obu: &Obu<'a>) -> Result<Option<Self>> {
        let (substream_id, data) = match obu.header.obu_type {
            ObuType::AudioFrameId(id) => (u32::from(id), obu.payload),
            ObuType::AudioFrame => {
                let mut r = ByteReader::new(obu.payload);
                let id = r.read_leb128()?;
                (id, r.rest())
            }
            _ => return Ok(None),
        };
        Ok(Some(AudioFrame {
            substream_id,
            data,
            num_samples_to_trim_at_start: obu.header.num_samples_to_trim_at_start,
            num_samples_to_trim_at_end: obu.header.num_samples_to_trim_at_end,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObuIter;

    #[test]
    fn implicit_and_explicit_ids() {
        // Type 9 => implicit substream id 3; then type 5 with explicit id 200.
        let mut data = vec![9 << 3, 0x02, 0xaa, 0xbb];
        data.extend([5 << 3, 0x03, 0xc8, 0x01, 0xcc]);
        let obus: Vec<_> = ObuIter::new(&data).collect::<Result<_>>().unwrap();

        let frame = AudioFrame::from_obu(&obus[0]).unwrap().unwrap();
        assert_eq!(frame.substream_id, 3);
        assert_eq!(frame.data, &[0xaa, 0xbb]);

        let frame = AudioFrame::from_obu(&obus[1]).unwrap().unwrap();
        assert_eq!(frame.substream_id, 200);
        assert_eq!(frame.data, &[0xcc]);
    }

    #[test]
    fn non_audio_frame_is_none() {
        let data = [31 << 3, 0x00];
        let obus: Vec<_> = ObuIter::new(&data).collect::<Result<_>>().unwrap();
        assert_eq!(AudioFrame::from_obu(&obus[0]).unwrap(), None);
    }

    #[test]
    fn trimming_propagates() {
        // Type 6 (id 0) with trimming flag: trim_end=64, trim_start=312.
        let data = [(6 << 3) | 0x02, 0x04, 0x40, 0xb8, 0x02, 0xff];
        let obus: Vec<_> = ObuIter::new(&data).collect::<Result<_>>().unwrap();
        let frame = AudioFrame::from_obu(&obus[0]).unwrap().unwrap();
        assert_eq!(frame.num_samples_to_trim_at_end, 64);
        assert_eq!(frame.num_samples_to_trim_at_start, 312);
        assert_eq!(frame.data, &[0xff]);
    }
}
