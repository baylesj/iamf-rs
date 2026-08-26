//! Shared helpers for the vector-driven conformance tests.

use std::path::PathBuf;

pub fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/vectors")
}

/// Unwraps a vector-dependent resource. Missing vectors FAIL the test with
/// fetch instructions so absent fixtures can't masquerade as green runs;
/// set `IAMF_VECTORS_OPTIONAL=1` to skip (with a message) instead, e.g.
/// for offline environments.
#[macro_export]
macro_rules! require_vectors {
    ($opt:expr, $what:expr) => {
        match $opt {
            Some(v) => v,
            None if std::env::var_os("IAMF_VECTORS_OPTIONAL").is_some() => {
                eprintln!("SKIPPED: {} missing; run tools/fetch_vectors.sh", $what);
                return;
            }
            None => panic!(
                "{} missing; run tools/fetch_vectors.sh \
                 (or set IAMF_VECTORS_OPTIONAL=1 to skip vector tests)",
                $what
            ),
        }
    };
}
