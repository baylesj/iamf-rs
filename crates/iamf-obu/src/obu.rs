use crate::{ByteReader, Error, Result};

/// OBU types per IAMF §3.2 (obu_type).
///
/// Non-exhaustive: post-v1.1 spec revisions add OBU types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObuType {
    /// Codec config descriptor (type 0, §3.6).
    CodecConfig,
    /// Audio element descriptor (type 1, §3.7).
    AudioElement,
    /// Mix presentation descriptor (type 2, §3.8).
    MixPresentation,
    /// Parameter block (type 3, §3.10).
    ParameterBlock,
    /// Temporal-unit boundary marker (type 4).
    TemporalDelimiter,
    /// Audio frame with an explicit substream ID (type 5, §3.9).
    AudioFrame,
    /// Audio frame with an implicit substream ID of 0..=17 (types 6..=23).
    AudioFrameId(u8),
    /// Metadata OBU (type 24, IAMF v2.0; see [`crate::metadata`]).
    Metadata,
    /// IA sequence header (type 31, §3.5).
    SequenceHeader,
    /// A reserved OBU type (25..=30). IAMF v2.0 requires decoders to
    /// ignore these (forward compatibility); the payload is opaque.
    Reserved(u8),
}

impl ObuType {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => ObuType::CodecConfig,
            1 => ObuType::AudioElement,
            2 => ObuType::MixPresentation,
            3 => ObuType::ParameterBlock,
            4 => ObuType::TemporalDelimiter,
            5 => ObuType::AudioFrame,
            6..=23 => ObuType::AudioFrameId(raw - 6),
            24 => ObuType::Metadata,
            31 => ObuType::SequenceHeader,
            // 25..=30; `raw` is a 5-bit field so nothing else reaches here.
            _ => ObuType::Reserved(raw),
        }
    }

    /// True for OBUs carrying coded audio (types 5..=23).
    pub fn is_audio_frame(&self) -> bool {
        matches!(self, ObuType::AudioFrame | ObuType::AudioFrameId(_))
    }
}

/// Parsed OBU header per IAMF §3.2.
///
/// IAMF v2.0 renames v1.1's `obu_trimming_status_flag` to
/// `type_specific_flag`: it signals trimming only for audio frames,
/// `optional_fields_flag` for mix presentations and `is_not_key_frame` for
/// temporal delimiters. Valid v1.1 streams only set it on audio frames, so
/// this is bit-exact for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObuHeader {
    /// The OBU's type (5-bit obu_type field).
    pub obu_type: ObuType,
    /// Whether this OBU is a redundant copy of an earlier descriptor.
    pub redundant_copy: bool,
    /// The raw `type_specific_flag` bit (v1.1 `obu_trimming_status_flag`).
    /// Prefer the typed accessors [`ObuHeader::optional_fields_flag`] and
    /// [`ObuHeader::is_key_frame`].
    pub type_specific_flag: bool,
    /// Trailing samples to trim; nonzero only when
    /// obu_trimming_status_flag was set (audio frames only).
    pub num_samples_to_trim_at_end: u32,
    /// Leading samples to trim, under the same flag.
    pub num_samples_to_trim_at_start: u32,
    /// Size of the (skipped) extension header region, when present.
    pub extension_header_size: u32,
}

impl ObuHeader {
    /// For mix presentation OBUs: whether `MixPresentationOptionalFields`
    /// follow the sub-mixes (IAMF v2.0). False for every other OBU type.
    pub fn optional_fields_flag(&self) -> bool {
        self.obu_type == ObuType::MixPresentation && self.type_specific_flag
    }

    /// For temporal delimiter OBUs: whether the temporal unit is a key frame
    /// (`!is_not_key_frame`, IAMF v2.0). True for every other OBU type.
    pub fn is_key_frame(&self) -> bool {
        !(self.obu_type == ObuType::TemporalDelimiter && self.type_specific_flag)
    }
}

/// One OBU: its header plus a borrowed view of its payload bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Obu<'a> {
    /// The parsed header fields.
    pub header: ObuHeader,
    /// The payload bytes after the header (and any trimming/extension
    /// fields), borrowed from the input.
    pub payload: &'a [u8],
}

impl<'a> Obu<'a> {
    /// Parses a single OBU from the front of `reader`, leaving the reader
    /// positioned at the next OBU.
    pub fn parse(reader: &mut ByteReader<'a>) -> Result<Self> {
        let header_offset = reader.position();
        let byte = reader.read_u8()?;
        let obu_type = ObuType::from_raw(byte >> 3);
        let redundant_copy = byte & 0x04 != 0;
        let type_specific_flag = byte & 0x02 != 0;
        let trimming_status_flag = type_specific_flag && obu_type.is_audio_frame();
        let extension_flag = byte & 0x01 != 0;

        let obu_size = reader.read_leb128()? as usize;
        let mut body = ByteReader::new(reader.read_bytes(obu_size)?);

        let (trim_end, trim_start) = if trimming_status_flag {
            (body.read_leb128()?, body.read_leb128()?)
        } else {
            (0, 0)
        };
        let extension_header_size = if extension_flag {
            let size = body.read_leb128()?;
            // Extension bytes are skipped, not interpreted (forward compat).
            body.read_bytes(size as usize)
                .map_err(|_| Error::InvalidObuSize {
                    offset: header_offset,
                })?;
            size
        } else {
            0
        };

        let payload = body.rest();
        Ok(Obu {
            header: ObuHeader {
                obu_type,
                redundant_copy,
                type_specific_flag,
                num_samples_to_trim_at_end: trim_end,
                num_samples_to_trim_at_start: trim_start,
                extension_header_size,
            },
            payload,
        })
    }
}

/// Iterator over the OBUs of a standalone IA sequence. Yields an error once
/// and then terminates if the stream is malformed.
#[derive(Debug)]
pub struct ObuIter<'a> {
    reader: ByteReader<'a>,
    failed: bool,
}

impl<'a> ObuIter<'a> {
    /// Iterates the OBUs of `data` from the start.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            reader: ByteReader::new(data),
            failed: false,
        }
    }
}

impl<'a> Iterator for ObuIter<'a> {
    type Item = Result<Obu<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.reader.is_empty() {
            return None;
        }
        let result = Obu::parse(&mut self.reader);
        if result.is_err() {
            self.failed = true;
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an OBU byte sequence: first header byte, leb128 size, body.
    fn obu_bytes(first_byte: u8, body: &[u8]) -> Vec<u8> {
        assert!(body.len() < 128, "test helper handles 1-byte sizes only");
        let mut out = vec![first_byte, body.len() as u8];
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn sequence_header_roundtrip() {
        // obu_type=31, no flags; payload is opaque here.
        let data = obu_bytes(31 << 3, &[0x69, 0x61, 0x6d, 0x66]);
        let obu = Obu::parse(&mut ByteReader::new(&data)).unwrap();
        assert_eq!(obu.header.obu_type, ObuType::SequenceHeader);
        assert!(!obu.header.redundant_copy);
        assert_eq!(obu.payload, b"iamf");
    }

    #[test]
    fn audio_frame_with_trimming() {
        // obu_type=6 (audio frame id 0), trimming flag set; body starts with
        // trim_at_end=64, trim_at_start=0, then two payload bytes.
        let data = obu_bytes((6 << 3) | 0x02, &[0x40, 0x00, 0xde, 0xad]);
        let obu = Obu::parse(&mut ByteReader::new(&data)).unwrap();
        assert_eq!(obu.header.obu_type, ObuType::AudioFrameId(0));
        assert_eq!(obu.header.num_samples_to_trim_at_end, 64);
        assert_eq!(obu.header.num_samples_to_trim_at_start, 0);
        assert_eq!(obu.payload, &[0xde, 0xad]);
    }

    #[test]
    fn extension_header_skipped() {
        // Extension flag set: body = ext_size=2, 2 ext bytes, 1 payload byte.
        let data = obu_bytes((31 << 3) | 0x01, &[0x02, 0xaa, 0xbb, 0xcc]);
        let obu = Obu::parse(&mut ByteReader::new(&data)).unwrap();
        assert_eq!(obu.header.extension_header_size, 2);
        assert_eq!(obu.payload, &[0xcc]);
    }

    #[test]
    fn reserved_types_are_opaque() {
        for raw in 25..=30u8 {
            let data = obu_bytes(raw << 3, &[0x01, 0x02]);
            let obu = Obu::parse(&mut ByteReader::new(&data)).unwrap();
            assert_eq!(obu.header.obu_type, ObuType::Reserved(raw));
            assert_eq!(obu.payload, &[0x01, 0x02]);
        }
    }

    #[test]
    fn type_specific_flag_only_trims_audio_frames() {
        // Mix presentation with the flag set: optional_fields_flag, and the
        // body is NOT consumed as trimming fields.
        let data = obu_bytes((2 << 3) | 0x02, &[0x40, 0x00]);
        let obu = Obu::parse(&mut ByteReader::new(&data)).unwrap();
        assert!(obu.header.type_specific_flag);
        assert!(obu.header.optional_fields_flag());
        assert_eq!(obu.header.num_samples_to_trim_at_end, 0);
        assert_eq!(obu.payload, &[0x40, 0x00]);

        // Temporal delimiter with the flag set: is_not_key_frame.
        let data = obu_bytes((4 << 3) | 0x02, &[]);
        let obu = Obu::parse(&mut ByteReader::new(&data)).unwrap();
        assert!(!obu.header.is_key_frame());
        assert!(!obu.header.optional_fields_flag());
        let data = obu_bytes(4 << 3, &[]);
        assert!(
            Obu::parse(&mut ByteReader::new(&data))
                .unwrap()
                .header
                .is_key_frame()
        );

        // Sequence header: reserved meaning, still no trimming read.
        let data = obu_bytes((31 << 3) | 0x02, &[0x69]);
        let obu = Obu::parse(&mut ByteReader::new(&data)).unwrap();
        assert!(!obu.header.optional_fields_flag());
        assert!(obu.header.is_key_frame());
        assert_eq!(obu.payload, &[0x69]);
    }

    #[test]
    fn truncated_body_rejected() {
        // Declares a 4-byte body but provides 1.
        let data = [31 << 3, 0x04, 0xff];
        assert!(matches!(
            Obu::parse(&mut ByteReader::new(&data)),
            Err(Error::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn iterator_walks_multiple_obus() {
        let mut data = obu_bytes(31 << 3, &[0x00]);
        data.extend(obu_bytes(0 << 3, &[0x01, 0x02]));
        data.extend(obu_bytes(4 << 3, &[]));
        let obus: Vec<_> = ObuIter::new(&data).collect::<Result<_>>().unwrap();
        assert_eq!(obus.len(), 3);
        assert_eq!(obus[1].header.obu_type, ObuType::CodecConfig);
        assert_eq!(obus[2].header.obu_type, ObuType::TemporalDelimiter);
    }

    #[test]
    fn iterator_passes_reserved_types_through() {
        let mut data = obu_bytes(31 << 3, &[0x00]);
        data.extend([25 << 3, 0x00]); // reserved type
        data.extend(obu_bytes(0 << 3, &[]));
        let obus: Vec<_> = ObuIter::new(&data).collect::<Result<_>>().unwrap();
        assert_eq!(obus[1].header.obu_type, ObuType::Reserved(25));
        assert_eq!(obus[2].header.obu_type, ObuType::CodecConfig);
    }

    #[test]
    fn iterator_stops_after_error() {
        let mut data = obu_bytes(31 << 3, &[0x00]);
        data.extend([31 << 3, 0x04, 0xff]); // truncated body
        data.extend(obu_bytes(0 << 3, &[])); // never reached
        let results: Vec<_> = ObuIter::new(&data).collect();
        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        assert!(results[1].is_err());
    }
}
