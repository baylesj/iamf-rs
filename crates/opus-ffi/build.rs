//! Links an external libopus. Resolution order:
//! 1. `OPUS_LIB_DIR` env var (plus `OPUS_STATIC=1` for static linking) —
//!    for integrators like Chromium that provide their own libopus.
//! 2. pkg-config.

fn main() {
    // docs.rs builds have no libopus and never link; skip probing.
    if std::env::var("DOCS_RS").is_ok() {
        return;
    }
    println!("cargo:rerun-if-env-changed=OPUS_LIB_DIR");
    println!("cargo:rerun-if-env-changed=OPUS_STATIC");
    if let Ok(dir) = std::env::var("OPUS_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
        let kind = if std::env::var("OPUS_STATIC").is_ok() {
            "static"
        } else {
            "dylib"
        };
        println!("cargo:rustc-link-lib={kind}=opus");
        return;
    }
    pkg_config::probe_library("opus").expect(
        "libopus not found: install it (e.g. `brew install opus`, \
         `apt install libopus-dev`) or set OPUS_LIB_DIR",
    );
}
