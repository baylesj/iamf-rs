//! Hook for replacing the streaming decoder's builtin render step.
//!
//! By default [`StreamDecoder`](crate::stream::StreamDecoder) renders
//! reconstructed audio elements with the builtin renderer: the IAMF v1.1
//! gain matrices ported from libiamf ([`render`](crate::render)) plus the
//! native obr-style binaural renderer (`binaural` feature). That path is
//! conformance-tested bit-exact against the reference decoders.
//!
//! A [`MixRendererFactory`] passed to
//! [`StreamDecoder::new_from_descriptors_with_renderer`](crate::stream::StreamDecoder::new_from_descriptors_with_renderer)
//! replaces only the render-and-mix step. Everything upstream (codec
//! decode, demixing and recon gain, ambisonics channel mapping, layer
//! selection, parameter timing, element mix gain) and downstream (output
//! mix gain, trimming, loudness normalization, limiter, quantization,
//! channel ordering) stays in the decoder, so an external renderer only
//! turns element channels into output channels.
//!
//! The decoder creates the [`MixRenderer`] at the first temporal unit with
//! audio (the codec's output sample rate is known only then), and again
//! after [`StreamDecoder::reset`](crate::stream::StreamDecoder::reset) or
//! [`StreamDecoder::reset_with_new_mix`](crate::stream::StreamDecoder::reset_with_new_mix).
//! Errors from [`MixRendererFactory::create`] or [`MixRenderer::render`]
//! are returned from
//! [`StreamDecoder::get_output_temporal_unit`](crate::stream::StreamDecoder::get_output_temporal_unit);
//! use [`DecodeError::UnsupportedRenderer`] to refuse a configuration.
//!
//! The `iamf-roar` crate in this repository uses this hook to render
//! through ROAR, AOM's Rust port of the Open Audio Renderer.

use crate::DecodeError;
use crate::layout::SoundSystem;

/// The channel format of one element's planes, as handed to a
/// [`MixRenderer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ElementLayout {
    /// Channel-based: the IAMF `loudspeaker_layout` (§3.6.2) of the layer
    /// the decoder reconstructed, with planes in that layout's channel
    /// order. The value 9 (BINAURAL) means pre-rendered binaural input
    /// (two planes, left and right) that should be passed through, not
    /// rendered.
    Channels {
        /// IAMF `loudspeaker_layout` value.
        loudspeaker_layout: u8,
    },
    /// Scene-based: full-order ambisonics, `(order + 1)^2` planes in ACN
    /// order with SN3D normalization (the decoder has already applied the
    /// element's channel mapping or demixing matrix).
    Ambisonics {
        /// Ambisonics order (0 to 4).
        order: u8,
    },
}

/// Static description of one audio element of the selected sub-mix.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ElementInfo {
    /// IAMF `audio_element_id`.
    pub audio_element_id: u32,
    /// What the element's planes carry.
    pub layout: ElementLayout,
    /// §3.8.2 `headphones_rendering_mode` from the sub-mix (0 stereo,
    /// 1 binaural world-locked, 2 binaural head-locked). Only meaningful
    /// when the target is [`SoundSystem::Binaural`]. For modes 1 and 2
    /// the decoder reconstructs the element's highest layer.
    pub headphones_rendering_mode: u8,
    /// `default_demixing_mode` of the element's demixing parameter
    /// definition, if it has one.
    pub default_demixing_mode: Option<u8>,
}

/// The configuration a [`MixRenderer`] is created for.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct MixConfig<'a> {
    /// Output layout. The rendered planes must match its channel count
    /// and IAMF channel order.
    pub target: SoundSystem,
    /// Output sample rate in Hz.
    pub sample_rate: u32,
    /// Codec frame size (`num_samples_per_frame`), the nominal number of
    /// samples per temporal unit. Individual units may be shorter.
    pub frame_size: u32,
    /// The sub-mix's elements, in the order every [`MixRenderer::render`]
    /// call presents them.
    pub elements: &'a [ElementInfo],
}

/// One element's audio for one temporal unit.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ElementFrame<'a> {
    /// IAMF `audio_element_id` (matches the [`ElementInfo`] at the same
    /// position in [`MixConfig::elements`]).
    pub audio_element_id: u32,
    /// The demixing mode (`dmixp_mode`) a parameter block assigns to this
    /// unit, if any.
    pub demixing_mode: Option<u8>,
    /// One plane per channel, all of the unit's (untrimmed) length. The
    /// element mix gain, including animation, is already applied.
    pub planes: &'a [Vec<f32>],
}

/// Renders and mixes the elements of one sub-mix, one temporal unit at a
/// time. Implementations keep whatever state they need across units.
pub trait MixRenderer {
    /// Renders one temporal unit. `elements` has the same elements in the
    /// same order as [`MixConfig::elements`]. Returns one plane per output
    /// channel of [`MixConfig::target`], each as long as the input planes.
    fn render(&mut self, elements: &[ElementFrame<'_>]) -> Result<Vec<Vec<f32>>, DecodeError>;
}

/// Creates [`MixRenderer`]s. The decoder keeps the factory for its whole
/// lifetime, so it can create a fresh renderer after a reset or a mix
/// change.
pub trait MixRendererFactory {
    /// Creates a renderer for `config`, or refuses it (preferably with
    /// [`DecodeError::UnsupportedRenderer`]).
    fn create(&self, config: &MixConfig<'_>) -> Result<Box<dyn MixRenderer>, DecodeError>;
}
