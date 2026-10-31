# HRTF binaural rendering: options survey (Aug 2026)

> **Status:** This survey is kept for historical context. Its recommended
> option, a native pure-Rust port of google/obr's binaural renderer, is now
> in the tree as [`iamf_dec::binaural`](../crates/iamf-dec/src/binaural/),
> behind the `binaural` feature. Since this survey was written:
>
> - The port uses its own partitioned FFT convolution, not `realfft`. So
>   `iamf-dec` still has no external dependencies.
> - It ships the `Reverberant` filter profile as well as `Ambient` and
>   `Direct`.
> - With IAMF v2.0, it also renders moving objects, encoded to third-order
>   ambisonics as obr does.
> - The FFI-to-obr demo track (option 1 below) was never built.

HRTF (head-related transfer function) rendering makes headphone audio sound
like it comes from around the listener.

## What IAMF binaural needs

- Render fixed-position loudspeaker beds (BS.2051 layouts) to two ears.
- Render first- to fourth-order ambisonics (ACN/SN3D) to two ears.
- Run in real time, deterministically, and embed cleanly.
- Use a license compatible with this project.

Positions are static in IAMF v1.1: no moving sources, no room simulation and
no head tracking.

## How the reference does it

Google's [obr](https://github.com/google/obr) (Open Binaural Renderer) is a
C++ library under the BSD 3-Clause Clear License plus a patent license.
iamf-tools and the Eclipsa ecosystem use it.

1. obr encodes everything, channels and objects included, to third-order
   ambisonics.
2. It then binauralizes with HRIRs (head-related impulse responses)
   decomposed into SH (spherical harmonics). That takes one partitioned FFT
   convolution per SH channel per ear.

Its filter data is embedded in the repository as plain C coefficient
arrays. There are direct, ambient and reverberant variants for each order
and both ears, derived from Resonance Audio's SADIE-based work. The arrays
can be extracted mechanically, just like our libiamf gain matrices.

## Candidates

| Option | Type | License | Verdict |
| --- | --- | --- | --- |
| **obr-style native port** | pure Rust | filters: obr's license | **Recommended.** We already have the HOA (higher-order ambisonics) plumbing: ACN channels and the H2M pipeline. Missing pieces: a speaker-to-SH encoder (small; obr's `ambisonic_encoder` ports easily), SH-domain binaural filters (extract from obr like `extract_matrices.py`), and partitioned FFT convolution (`realfft`/`rustfft`, MIT/Apache). It sounds the same as Chrome/Eclipsa output, because it uses the same filters. Moderate effort. |
| [`sofar`](https://crates.io/crates/sofar) 0.3 | pure Rust | MIT/Apache-2.0 | A strong building block. It is a pure-Rust SOFA reader (a libmysofa port) plus a uniformly partitioned convolution renderer, with resampling. Route: virtual speakers plus a SOFA HRTF set (for example SADIE II). A good fallback if obr's filter license is a concern. Output won't match Eclipsa's reference sound. |
| [`hrtf`](https://crates.io/crates/hrtf) 0.9 (Fyrox) | pure Rust | MIT | Mature (629k downloads, active), but the wrong shape. It spatializes moving point sources for games over IRCAM HRIR spheres, at a 44.1 kHz base, with known clicks on position changes. Usable for static virtual speakers, but it has no ambisonics path and a game-engine API. |
| [`audionimbus`](https://crates.io/crates/audionimbus) 0.16 (Steam Audio) | FFI (C++) | wrapper MIT/Apache; Steam Audio Apache-2.0 | Full-featured (HRTF, ambisonics decode, SOFA loading) and actively maintained. But it pulls in the whole Steam Audio runtime for what is, for us, a fixed-scene convolution. A reasonable plan B if we want everything off the shelf. |
| obr via FFI | FFI (C++) | BSD 3-Clause Clear plus patent license | Exact parity with Chrome's headphone output. It goes against the pure-Rust goal and adds a Bazel/CMake C++ build. Lowest risk for a demo. |
| [`voirs-spatial`](https://crates.io/crates/voirs-spatial) | pure Rust | Apache-2.0 | Version 0.1.0-rc.1 with 1.4k downloads, part of a sprawling TTS framework. It claims HRTF and first- to third-order HOA, but it is too immature to bet on. |
| `firewheel-ircam-hrtf`, `ambisonic`, `oddio` | pure Rust | MIT/Apache | Game audio components: an HRTF node for Firewheel, first-order ambisonics with a basic stereo decode, and ITD/ILD panning. None fit HOA-to-binaural directly. |

## Recommendation

Work on two tracks:

1. **Fast demo with exact Chrome-parity sound:** a feature-gated FFI binding
   to obr, using the same pattern as `opus-ffi`.
2. **The real milestone:** a native obr-style renderer.
   - Extract obr's SH binaural filters with a generator script.
   - Port the speaker-to-SH encoder.
   - Drive both through a `realfft` partitioned convolver.

Both tracks share the output API, so track 1 can be dropped once track 2
lands.

Licensing note for track 2: the extracted filter tables carry obr's license
and patent grant. This is the same situation as our libiamf-derived gain
matrices. The repository's licensing section should acknowledge both.
