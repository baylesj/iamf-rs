//! Renderer backend selection.
//!
//! The decoder renders reconstructed audio elements to the output layout
//! through one of two engines:
//!
//! - [`RendererBackend::Builtin`] (default): the IAMF v1.1 gain matrices
//!   ported from libiamf (`render`) plus the native obr-style binaural
//!   renderer (`binaural` feature). Conformance-tested bit-exact against
//!   the libiamf/iamf-tools reference outputs.
//! - [`RendererBackend::Roar`]: ROAR (<https://github.com/AOMediaCodec/roar>),
//!   the Rust port of AOM's Open Audio Renderer (OAR), which the IAMF v2.0
//!   specification references as its rendering algorithm. Requires the
//!   `roar` cargo feature (Rust 1.97.1+); selecting it in a build without
//!   the feature fails decoder construction with
//!   [`DecodeError::UnsupportedRenderer`](crate::DecodeError::UnsupportedRenderer).
//!
//! Everything upstream of rendering — codec decode, trimming, demixing /
//! recon gain, layer selection, parameter-block timing — is shared, so the
//! backends differ only in how element channels become output channels.
//! See `ARCHITECTURE.md` for the mapping of IAMF concepts onto ROAR.

/// Which rendering engine turns reconstructed elements into output
/// channels (see the [module docs](self)).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum RendererBackend {
    /// libiamf-derived gain matrices + native obr-style binaural renderer.
    #[default]
    Builtin,
    /// ROAR (Open Audio Renderer port). Needs the `roar` feature.
    Roar,
}

impl RendererBackend {
    /// Whether this backend was compiled into the library.
    pub fn is_available(self) -> bool {
        match self {
            RendererBackend::Builtin => true,
            RendererBackend::Roar => cfg!(feature = "roar"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_tracks_features() {
        assert!(RendererBackend::Builtin.is_available());
        assert_eq!(RendererBackend::Roar.is_available(), cfg!(feature = "roar"));
        assert_eq!(RendererBackend::default(), RendererBackend::Builtin);
    }
}
