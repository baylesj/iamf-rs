//! End-to-end substream decoding against real libiamf vectors (fetch with
//! tools/fetch_vectors.sh; passes trivially when vectors are absent).
//!
//! test_000002 (LPCM stereo) was verified bit-exact against libiamf's
//! rendered reference WAV; the sample-count and shape assertions here guard
//! that behavior. Opus output is lossy, so only shape is asserted.

use std::path::PathBuf;

use iamf_codecs::DefaultFactory;
use iamf_dec::element::{ElementDecoder, SubstreamPcm};
use iamf_obu::descriptors::{self, Descriptor};
use iamf_obu::{AudioFrame, ObuIter};

fn decode_vector(name: &str) -> Option<Vec<SubstreamPcm>> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../tests/vectors/{name}"));
    let data = std::fs::read(path).ok()?;

    let mut codec_config = None;
    let mut element = None;
    let mut decoder = None;
    for obu in ObuIter::new(&data).map(Result::unwrap) {
        match descriptors::parse(&obu).unwrap() {
            Some(Descriptor::CodecConfig(cc)) => codec_config = Some(cc),
            Some(Descriptor::AudioElement(ae)) if element.is_none() => element = Some(ae),
            _ => {}
        }
        if let Some(frame) = AudioFrame::from_obu(&obu).unwrap() {
            let dec = decoder.get_or_insert_with(|| {
                ElementDecoder::new(
                    element.as_ref().unwrap(),
                    codec_config.as_ref().unwrap(),
                    &DefaultFactory,
                )
                .unwrap()
            });
            assert!(dec.decode_frame(&frame).unwrap());
        }
    }
    Some(decoder?.finish())
}

#[test]
fn lpcm_stereo_shape() {
    let Some(subs) = decode_vector("test_000002.iamf") else {
        eprintln!("vector missing; run tools/fetch_vectors.sh");
        return;
    };
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].channels, 2);
    assert_eq!(subs[0].sample_rate, 16000);
    assert_eq!(subs[0].samples.len(), 8000 * 2);
    // The vector's content is a very quiet ramp (peak ~0.003), verified
    // bit-exact against libiamf's rendered reference — only assert nonzero.
    assert!(subs[0].samples.iter().any(|&s| s != 0.0), "silent output");
}

#[test]
fn opus_stereo_shape() {
    let Some(subs) = decode_vector("test_000026.iamf") else {
        eprintln!("vector missing; run tools/fetch_vectors.sh");
        return;
    };
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].channels, 2);
    assert_eq!(subs[0].sample_rate, 48000);
    // 26 frames x 960 samples, minus 312 pre-skip and 648 end trim.
    assert_eq!(subs[0].samples.len(), 24000 * 2);
    assert!(
        subs[0].samples.iter().any(|&s| s.abs() > 0.01),
        "silent output"
    );
}

#[test]
fn ambisonics_lpcm_shape() {
    let Some(subs) = decode_vector("test_000038.iamf") else {
        eprintln!("vector missing; run tools/fetch_vectors.sh");
        return;
    };
    assert_eq!(subs.len(), 4);
    assert!(subs.iter().all(|s| s.channels == 1));
    assert!(subs.iter().all(|s| s.samples.len() == 24000));
}
