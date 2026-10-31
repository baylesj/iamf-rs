//! Fuzzes the batch decoder (`PresentationDecoder`, used by the `iamfdec`
//! CLI) over arbitrary bytes: descriptor collection, every mix
//! presentation and layout, codec decode, reconstruction, rendering and
//! the final mix must never panic. The streaming decoder has its own
//! target; the two share most stages but not their drivers.
//!
//! Run with `cd fuzz && cargo fuzz run decode_batch`.

#![no_main]

use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::presentation::{Descriptors, PresentationDecoder};
use iamf_obu::ObuIter;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let layout = SoundSystem::from_u8(data[0] % 15).expect("0..=14 are valid");
    let mix_index = usize::from(data[1] & 3);
    let payload = &data[2..];
    let Ok(descriptors) = Descriptors::collect(payload) else {
        return;
    };
    let Ok(mut decoder) =
        PresentationDecoder::new(&descriptors, mix_index, layout, &DefaultFactory)
    else {
        return;
    };
    for obu in ObuIter::new(payload) {
        let Ok(obu) = obu else { break };
        if decoder.process_obu(&obu).is_err() {
            return;
        }
    }
    let _ = decoder.finish();
});
