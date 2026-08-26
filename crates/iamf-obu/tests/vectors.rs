//! Parses real libiamf test vectors from tests/vectors/ (populate with
//! tools/fetch_vectors.sh; a missing vectors directory fails the test
//! unless IAMF_VECTORS_OPTIONAL=1 downgrades it to a skip).

use std::path::PathBuf;

use iamf_obu::{ObuIter, descriptors};

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/vectors")
}

/// True when the vector's textproto marks it decodable by a compliant
/// decoder. Vectors without a textproto are treated as should-pass.
fn is_valid_to_decode(iamf_path: &std::path::Path) -> bool {
    let textproto = iamf_path.with_extension("textproto");
    match std::fs::read_to_string(textproto) {
        Ok(text) => text.contains("is_valid_to_decode: true"),
        Err(_) => true,
    }
}

#[test]
#[allow(clippy::match_wild_err_arm)] // any read_dir error means "no vectors"
fn parse_fetched_vectors() {
    let dir = vectors_dir();
    let mut paths: Vec<_> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "iamf"))
            .collect(),
        Err(_) if std::env::var_os("IAMF_VECTORS_OPTIONAL").is_some() => {
            eprintln!(
                "SKIPPED: no vectors in {}; run tools/fetch_vectors.sh",
                dir.display()
            );
            return;
        }
        Err(_) => panic!(
            "no vectors in {}; run tools/fetch_vectors.sh \
             (or set IAMF_VECTORS_OPTIONAL=1 to skip vector tests)",
            dir.display()
        ),
    };
    paths.sort();
    assert!(
        !paths.is_empty(),
        "vectors dir exists but holds no .iamf files"
    );

    let mut parsed = 0usize;
    for path in &paths {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let data = std::fs::read(path).unwrap();
        let should_pass = is_valid_to_decode(path);

        let mut counts = [0usize; 4]; // seq header, codec config, element, mix
        let mut error = None;
        for result in ObuIter::new(&data) {
            let obu = match result {
                Ok(obu) => obu,
                Err(e) => {
                    error = Some(format!("OBU framing: {e}"));
                    break;
                }
            };
            match descriptors::parse(&obu) {
                Ok(Some(descriptors::Descriptor::SequenceHeader(_))) => counts[0] += 1,
                Ok(Some(descriptors::Descriptor::CodecConfig(_))) => counts[1] += 1,
                Ok(Some(descriptors::Descriptor::AudioElement(_))) => counts[2] += 1,
                Ok(Some(descriptors::Descriptor::MixPresentation(_))) => counts[3] += 1,
                Ok(None) => {}
                Err(e) => {
                    error = Some(format!("descriptor: {e}"));
                    break;
                }
            }
        }

        if should_pass {
            assert_eq!(error, None, "{name} failed");
            assert!(
                counts.iter().all(|&c| c > 0),
                "{name}: missing descriptors (seq/codec/element/mix = {counts:?})"
            );
        }
        // Should-fail vectors exercise semantic violations; syntax-level
        // parsing may legitimately succeed or fail, it just must not panic
        // (the fuzzer's job to prove more broadly).
        parsed += 1;
    }
    println!("parsed {parsed} vectors from {}", dir.display());
}
