//! Output layouts (IAMF §7.3.2 sound systems).

/// Target loudspeaker layout for rendering, following ITU-R BS.2051 sound
/// systems as referenced by the IAMF spec. Binaural is a distinct output
/// mode, deferred to a later milestone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Mono,
    /// Sound system A (0+2+0).
    Stereo,
    /// Sound system B (0+5+0).
    Surround5_1,
    /// Sound system I (4+7+0), a.k.a. 7.1.4.
    Surround7_1_4,
}

impl Layout {
    pub fn channel_count(&self) -> u8 {
        match self {
            Layout::Mono => 1,
            Layout::Stereo => 2,
            Layout::Surround5_1 => 6,
            Layout::Surround7_1_4 => 12,
        }
    }
}
