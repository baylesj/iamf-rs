//! Metadata OBU payloads (OBU type 24, IAMF v2.0).
//!
//! Metadata OBUs carry information that does not affect decoding: ITU-T
//! T.35 registered payloads and IAMF tag strings. Decoders may ignore them;
//! this module exposes them for tools. Parsing follows iamf-tools v3.0.0
//! (`iamf/obu/metadata_obu.cc`): unknown `metadata_type` values are kept as
//! opaque bytes rather than rejected.

use crate::{ByteReader, Obu, ObuType, Result};

/// `metadata_type` for ITU-T T.35 payloads.
pub const METADATA_TYPE_ITU_T_T35: u32 = 1;
/// `metadata_type` for IAMF tags.
pub const METADATA_TYPE_IAMF_TAGS: u32 = 2;

/// A parsed metadata OBU payload.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Metadata {
    /// An ITU-T T.35 registered payload (`metadata_type` 1).
    ItuTT35 {
        /// `itu_t_t35_country_code`.
        country_code: u8,
        /// `itu_t_t35_country_code_extension_byte`, present iff
        /// `country_code == 0xFF`.
        country_code_extension: Option<u8>,
        /// The remaining payload bytes (terminal provider code onwards).
        payload: Vec<u8>,
    },
    /// IAMF tags (`metadata_type` 2): `(name, value)` string pairs.
    IamfTags(Vec<(String, String)>),
    /// A reserved `metadata_type`; the payload is kept opaque.
    Reserved {
        /// The raw `metadata_type`.
        metadata_type: u32,
        /// The payload bytes after `metadata_type`.
        payload: Vec<u8>,
    },
}

impl Metadata {
    /// Parses the payload of a metadata OBU. Returns `None` for OBUs of any
    /// other type.
    pub fn parse(obu: &Obu<'_>) -> Result<Option<Self>> {
        if obu.header.obu_type != ObuType::Metadata {
            return Ok(None);
        }
        Self::parse_payload(&mut ByteReader::new(obu.payload)).map(Some)
    }

    /// Parses a metadata OBU payload from `r`, consuming the remainder of
    /// `r` for sized payloads (ITU-T T.35, reserved types).
    pub fn parse_payload(r: &mut ByteReader<'_>) -> Result<Self> {
        let metadata_type = r.read_leb128()?;
        Ok(match metadata_type {
            METADATA_TYPE_ITU_T_T35 => {
                let country_code = r.read_u8()?;
                let country_code_extension = if country_code == 0xFF {
                    Some(r.read_u8()?)
                } else {
                    None
                };
                Metadata::ItuTT35 {
                    country_code,
                    country_code_extension,
                    payload: r.rest().to_vec(),
                }
            }
            METADATA_TYPE_IAMF_TAGS => {
                let num_tags = r.read_u8()?;
                let mut tags = Vec::with_capacity(usize::from(num_tags));
                for _ in 0..num_tags {
                    let name = r.read_string()?;
                    let value = r.read_string()?;
                    tags.push((name, value));
                }
                Metadata::IamfTags(tags)
            }
            _ => Metadata::Reserved {
                metadata_type,
                payload: r.rest().to_vec(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObuIter;

    fn metadata_obu(payload: &[u8]) -> Vec<u8> {
        let mut out = vec![24 << 3, payload.len() as u8];
        out.extend_from_slice(payload);
        out
    }

    fn parse_one(payload: &[u8]) -> Result<Metadata> {
        let data = metadata_obu(payload);
        let obu = ObuIter::new(&data).next().unwrap().unwrap();
        Metadata::parse(&obu).map(Option::unwrap)
    }

    #[test]
    fn itu_t_t35() {
        let m = parse_one(&[1, 0xb5, 0x00, 0x3c, 0xaa]).unwrap();
        assert_eq!(
            m,
            Metadata::ItuTT35 {
                country_code: 0xb5,
                country_code_extension: None,
                payload: vec![0x00, 0x3c, 0xaa],
            }
        );
    }

    #[test]
    fn itu_t_t35_extension_byte() {
        let m = parse_one(&[1, 0xff, 0x07, 0x01]).unwrap();
        assert_eq!(
            m,
            Metadata::ItuTT35 {
                country_code: 0xff,
                country_code_extension: Some(0x07),
                payload: vec![0x01],
            }
        );
        // Country code 0xFF without its extension byte.
        assert!(parse_one(&[1, 0xff]).is_err());
        assert!(parse_one(&[1]).is_err());
    }

    #[test]
    fn iamf_tags() {
        let m = parse_one(b"\x02\x02a\0b\0cd\0\0").unwrap();
        assert_eq!(
            m,
            Metadata::IamfTags(vec![("a".into(), "b".into()), ("cd".into(), String::new())])
        );
        // Truncated second tag.
        assert!(parse_one(b"\x02\x02a\0b\0").is_err());
    }

    #[test]
    fn reserved_type_is_opaque() {
        let m = parse_one(&[9, 1, 2, 3]).unwrap();
        assert_eq!(
            m,
            Metadata::Reserved {
                metadata_type: 9,
                payload: vec![1, 2, 3],
            }
        );
        // Type 0 is reserved too (iamf-tools does not reject it).
        assert!(matches!(
            parse_one(&[0]).unwrap(),
            Metadata::Reserved {
                metadata_type: 0,
                ..
            }
        ));
    }

    #[test]
    fn non_metadata_obu_is_none() {
        let data = [31 << 3, 0];
        let obu = ObuIter::new(&data).next().unwrap().unwrap();
        assert_eq!(Metadata::parse(&obu).unwrap(), None);
    }
}
