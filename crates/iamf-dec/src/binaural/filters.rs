//! Embedded SH-HRIR filter assets, extracted from obr (see NOTICE in the
//! assets directory and tools/extract_binaural_filters.py).

use crate::DecodeError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterProfile {
    /// obr / iamf-tools default.
    Ambient,
    /// Anechoic short HRIRs (unused by the default pipeline, kept for
    /// parity with obr's profiles).
    #[allow(dead_code)]
    Direct,
}

macro_rules! asset {
    ($name:literal) => {
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/binaural/",
            $name
        ))
    };
}

fn asset(order: usize, profile: FilterProfile, right: bool) -> Option<&'static [u8]> {
    Some(match (order, profile, right) {
        (1, FilterProfile::Ambient, false) => asset!("1oa_ambient_l.wav"),
        (1, FilterProfile::Ambient, true) => asset!("1oa_ambient_r.wav"),
        (2, FilterProfile::Ambient, false) => asset!("2oa_ambient_l.wav"),
        (2, FilterProfile::Ambient, true) => asset!("2oa_ambient_r.wav"),
        (3, FilterProfile::Ambient, false) => asset!("3oa_ambient_l.wav"),
        (3, FilterProfile::Ambient, true) => asset!("3oa_ambient_r.wav"),
        (4, FilterProfile::Ambient, false) => asset!("4oa_ambient_l.wav"),
        (4, FilterProfile::Ambient, true) => asset!("4oa_ambient_r.wav"),
        (1, FilterProfile::Direct, false) => asset!("1oa_direct_l.wav"),
        (1, FilterProfile::Direct, true) => asset!("1oa_direct_r.wav"),
        (2, FilterProfile::Direct, false) => asset!("2oa_direct_l.wav"),
        (2, FilterProfile::Direct, true) => asset!("2oa_direct_r.wav"),
        (3, FilterProfile::Direct, false) => asset!("3oa_direct_l.wav"),
        (3, FilterProfile::Direct, true) => asset!("3oa_direct_r.wav"),
        (4, FilterProfile::Direct, false) => asset!("4oa_direct_l.wav"),
        (4, FilterProfile::Direct, true) => asset!("4oa_direct_r.wav"),
        _ => return None,
    })
}

/// Parses a 16-bit PCM RIFF WAV into per-channel f32 planes (i16 / 32768,
/// matching obr's `FillAudioBuffer`).
fn parse_wav(data: &[u8]) -> Result<Vec<Vec<f32>>, DecodeError> {
    let bad = || DecodeError::InvalidDescriptors("bad filter asset".into());
    if data.len() < 12 || &data[..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err(bad());
    }
    let mut pos = 12;
    let mut channels = 0usize;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = data.get(pos + 8..pos + 8 + size).ok_or_else(bad)?;
        match id {
            b"fmt " => {
                let format = u16::from_le_bytes(body[0..2].try_into().unwrap());
                let bits = u16::from_le_bytes(body[14..16].try_into().unwrap());
                if format != 1 || bits != 16 {
                    return Err(bad());
                }
                channels = usize::from(u16::from_le_bytes(body[2..4].try_into().unwrap()));
            }
            b"data" => {
                if channels == 0 {
                    return Err(bad());
                }
                let samples: Vec<f32> = body
                    .chunks_exact(2)
                    .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
                    .collect();
                let frames = samples.len() / channels;
                return Ok((0..channels)
                    .map(|c| (0..frames).map(|t| samples[t * channels + c]).collect())
                    .collect());
            }
            _ => {}
        }
        pos += 8 + size + (size & 1);
    }
    Err(bad())
}

/// Per-channel filter planes.
pub type FilterPlanes = Vec<Vec<f32>>;

/// Loads the (left, right) SH-HRIR planes for an ambisonic order. Channel
/// count is (order+1)²; filters are 48 kHz.
pub fn sh_hrirs(
    order: usize,
    profile: FilterProfile,
) -> Result<(FilterPlanes, FilterPlanes), DecodeError> {
    let unsupported = || DecodeError::Unimplemented("no binaural filters for this order");
    let left = parse_wav(asset(order, profile, false).ok_or_else(unsupported)?)?;
    let right = parse_wav(asset(order, profile, true).ok_or_else(unsupported)?)?;
    let expected = (order + 1) * (order + 1);
    if left.len() != expected || right.len() != expected {
        return Err(DecodeError::InvalidDescriptors(
            "filter channel count mismatch".into(),
        ));
    }
    Ok((left, right))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_load_for_all_orders() {
        for order in 1..=4 {
            for profile in [FilterProfile::Ambient, FilterProfile::Direct] {
                let (l, r) = sh_hrirs(order, profile).unwrap();
                assert_eq!(l.len(), (order + 1) * (order + 1));
                assert_eq!(l[0].len(), r[0].len());
                assert!(l[0].iter().any(|&s| s != 0.0));
            }
        }
    }
}
