//! Fuzzes the OBU parser over arbitrary bytes: must never panic, and every
//! yielded payload must be in bounds. Each payload is also run through the
//! descriptor, metadata and position-data parsers, which must reject
//! malformed input without panicking. Run with `cd fuzz && cargo fuzz run
//! parse_obu`; seed the corpus with `../tests/vectors/*.iamf`. Use a
//! large `-max_len` (CI uses 1 MB): some parser bugs (quadratic time,
//! allocation size) only show on long OBUs.

#![no_main]

use iamf_obu::ByteReader;
use iamf_obu::descriptors;
use iamf_obu::metadata::Metadata;
use iamf_obu::position::{PositionData, PositionParamType};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for result in iamf_obu::ObuIter::new(data) {
        match result {
            Ok(obu) => {
                // Touch every byte of the payload so ASan sees OOB reads.
                let _ = obu.payload.iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
                let _ = descriptors::parse(&obu);
                let _ = Metadata::parse(&obu);
                for ty in (3..=8).filter_map(PositionParamType::from_param_definition_type) {
                    let _ = PositionData::parse(&mut ByteReader::new(obu.payload), ty);
                }
            }
            Err(_) => break,
        }
    }
});
