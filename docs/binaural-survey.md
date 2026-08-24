# HRTF binaural rendering: options survey (Aug 2026)

What IAMF binaural needs: render fixed-position loudspeaker beds
(BS.2051 layouts) and 1st–4th order ambisonics (ACN/SN3D) to two ears,
realtime, deterministic, embeddable, license-compatible. Positions are
static — no moving sources, no room simulation, no head tracking (v1).

## How the reference does it

Google's [obr](https://github.com/google/obr) (Open Binaural Renderer,
C++/BSD-style + PATENTS, used by iamf-tools and the Eclipsa ecosystem)
encodes *everything* — channels and objects included — to 3rd-order
ambisonics, then binauralizes with HRIRs decomposed into spherical
harmonics: one partitioned-FFT convolution per SH channel per ear. Its
filter data (direct/ambient/reverberant variants per order, both ears,
derived from Resonance Audio's SADIE-based work) is embedded in-repo as
plain C coefficient arrays — mechanically extractable, exactly like our
libiamf gain matrices.

## Candidates

| Option | Type | License | Verdict |
| --- | --- | --- | --- |
| **obr-style native port** | pure Rust | filters BSD (obr) | **Recommended.** We already have the HOA plumbing (ACN channels, H2M pipeline). Missing pieces: speaker→SH encoder (small, obr's `ambisonic_encoder` is portable), SH-domain binaural filters (extract from obr like `extract_matrices.py`), partitioned FFT convolution (`realfft`/`rustfft`, MIT/Apache). Perceptually matches Chrome/Eclipsa output since it's the same filters. Moderate effort. |
| [`sofar`](https://crates.io/crates/sofar) 0.3 | pure Rust | MIT/Apache-2.0 | Strong building block: pure-Rust SOFA reader (libmysofa port) + uniformly-partitioned convolution renderer, resampling included. Route: virtual speakers + a SOFA HRTF set (e.g. SADIE II). Good fallback if obr filter licensing is a concern; output won't match Eclipsa's reference sound. |
| [`hrtf`](https://crates.io/crates/hrtf) 0.9 (Fyrox) | pure Rust | MIT | Mature (629k downloads, active), but the wrong shape: per-moving-point-source game spatialization over IRCAM HRIR spheres, 44.1 kHz base, known click artifacts on position changes. Usable for static virtual speakers, but no ambisonics path and a game-engine-oriented API. |
| [`audionimbus`](https://crates.io/crates/audionimbus) 0.16 (Steam Audio) | FFI (C++) | wrapper MIT/Apache; Steam Audio Apache-2.0 | Full-featured (HRTF, ambisonics decode, SOFA loading) and actively maintained — but it drags in the whole Steam Audio runtime for what is, for us, a fixed-scene convolution. Reasonable plan B if we want everything off the shelf. |
| obr via FFI | FFI (C++) | BSD-style + PATENTS | Exact parity with Chrome's headphone output. Contradicts the pure-Rust goal and adds Bazel/CMake C++ to the build, but lowest risk for a demo. |
| [`voirs-spatial`](https://crates.io/crates/voirs-spatial) | pure Rust | Apache-2.0 | 0.1.0-rc.1, 1.4k downloads, part of a sprawling TTS framework; claims HRTF + HOA 1–3 but too immature to bet on. |
| `firewheel-ircam-hrtf`, `ambisonic`, `oddio` | pure Rust | MIT/Apache | Game-audio components (HRTF node for Firewheel; FOA with basic stereo decode; ITD/ILD panning). None fit HOA-to-binaural directly. |

## Recommendation

Two-track: **(1)** for a fast demo with exact Chrome-parity sound,
feature-gated FFI to obr (same pattern as `opus-ffi`); **(2)** the real
milestone: a native obr-style renderer — extract obr's SH binaural
filters with a generator script, port the speaker→SH encoder, and drive
both through a `realfft` partitioned convolver. The two share the output
API, so (1) can be dropped once (2) lands.

Licensing note for (2): extracted filter tables carry obr's BSD-style
license + PATENTS grant, same situation as our libiamf-derived gain
matrices — the repo's licensing section should acknowledge both.
