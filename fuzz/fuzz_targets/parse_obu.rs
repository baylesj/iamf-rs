//! Fuzzes the OBU parser over arbitrary bytes: must never panic, and every
//! yielded payload must be in bounds. Run with `cargo +nightly fuzz run
//! parse_obu` from the repo root. Seed the corpus with iamf-tools test
//! vectors once the conformance rig lands.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for result in iamf_obu::ObuIter::new(data) {
        match result {
            Ok(obu) => {
                // Touch every byte of the payload so ASan sees OOB reads.
                let _ = obu.payload.iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
            }
            Err(_) => break,
        }
    }
});
