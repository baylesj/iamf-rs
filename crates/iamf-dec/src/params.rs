//! Parameter block OBU parsing (IAMF §3.10).
//!
//! Parameter blocks are context-dependent: their syntax hinges on the
//! parameter definition (from an audio element or mix presentation) that
//! declared the parameter ID, which is why this lives in `iamf-dec` rather
//! than `iamf-obu`.

use iamf_obu::descriptors::{ChannelAudioLayer, ParamDefinition};
use iamf_obu::{ByteReader, Error};

/// What kind of parameter a block's ID resolves to, plus the context needed
/// to parse its payload.
#[non_exhaustive]
pub enum ParamContext<'a> {
    MixGain,
    Demixing,
    /// Channel layers of the owning element; recon gain data exists only
    /// for layers with `recon_gain_is_present`.
    ReconGain(&'a [ChannelAudioLayer]),
}

/// §3.10.2 animated mix gain over one subblock. Values are Q7.8 dB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MixGainAnimation {
    Step {
        start: i16,
    },
    Linear {
        start: i16,
        end: i16,
    },
    Bezier {
        start: i16,
        end: i16,
        control: i16,
        control_relative_time: u8,
    },
}

/// Q7.8 dB → linear gain.
pub fn q78_db_to_linear(q: i16) -> f32 {
    10f32.powf(f32::from(q) / 256.0 / 20.0)
}

impl MixGainAnimation {
    /// Linear gain at sample `i` of a subblock of `duration` samples.
    /// Matches libiamf: endpoints are converted to linear first and
    /// interpolated in the linear domain (`mix_gain_bezier_linear` /
    /// `mix_gain_bezier_quad`).
    pub fn evaluate_at(&self, duration: usize, i: usize) -> f32 {
        match *self {
            MixGainAnimation::Step { start } => q78_db_to_linear(start),
            MixGainAnimation::Linear { start, end } => {
                let s = q78_db_to_linear(start);
                let e = q78_db_to_linear(end);
                s + (e - s) * i as f32 / duration.max(1) as f32
            }
            MixGainAnimation::Bezier {
                start,
                end,
                control,
                control_relative_time,
            } => {
                let s = f64::from(q78_db_to_linear(start));
                let e = f64::from(q78_db_to_linear(end));
                let c = f64::from(q78_db_to_linear(control));
                let crt = f64::from(control_relative_time) / 255.0;
                // libiamf truncates the control time to whole samples.
                let ct = (crt * (duration as f64 + 0.1)) as i64;
                let alpha = duration as i64 - 2 * ct;
                let i = i as f64;
                let a = if alpha != 0 {
                    (((ct * ct) as f64 + alpha as f64 * i).sqrt() - ct as f64) / alpha as f64
                } else {
                    i / (2 * ct) as f64
                };
                ((s + e - 2.0 * c) * a * a + 2.0 * a * (c - s) + s) as f32
            }
        }
    }

    /// Writes per-sample linear gains for one subblock of `duration`
    /// samples into `out` (`out.len() <= duration`).
    pub fn evaluate(&self, duration: usize, out: &mut [f32]) {
        for (i, o) in out.iter_mut().enumerate() {
            *o = self.evaluate_at(duration, i);
        }
    }
}

/// Per-layer recon gain: `(recon_gain_flags, gains)`, one gain byte per set
/// flag bit, in ascending bit order. `None` for layers without recon gain.
pub type ReconGainLayers = Vec<Option<(u32, Vec<u8>)>>;

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SubblockData {
    MixGain(MixGainAnimation),
    Demixing { dmixp_mode: u8 },
    ReconGain(ReconGainLayers),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterSubblock {
    pub duration: u32,
    pub data: SubblockData,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterBlock {
    pub parameter_id: u32,
    pub duration: u32,
    pub constant_subblock_duration: u32,
    pub subblocks: Vec<ParameterSubblock>,
}

impl ParameterBlock {
    /// Reads only the parameter ID, so callers can look up the matching
    /// definition before parsing the rest with [`ParameterBlock::parse`].
    pub fn peek_parameter_id(payload: &[u8]) -> Result<u32, Error> {
        ByteReader::new(payload).read_leb128()
    }

    pub fn parse(
        payload: &[u8],
        definition: &ParamDefinition,
        context: &ParamContext<'_>,
    ) -> Result<Self, Error> {
        let mut r = ByteReader::new(payload);
        let parameter_id = r.read_leb128()?;

        // Timing comes from the block itself in mode 1, from the parameter
        // definition in mode 0.
        let (duration, constant_subblock_duration, explicit_count) = if definition.mode {
            let duration = r.read_leb128()?;
            let csd = r.read_leb128()?;
            let count = if csd == 0 {
                Some(r.read_leb128()?)
            } else {
                None
            };
            (duration, csd, count)
        } else {
            (
                definition.duration,
                definition.constant_subblock_duration,
                None,
            )
        };

        let num_subblocks = match (constant_subblock_duration, explicit_count) {
            (0, Some(count)) => count,
            (0, None) => definition.subblock_durations.len() as u32,
            (csd, _) => duration.div_ceil(csd),
        };

        let mut subblocks = Vec::new();
        for i in 0..num_subblocks {
            // A subblock consumes at least one byte; bail before the loop
            // can be driven far by a hostile count.
            if r.is_empty() {
                return Err(Error::UnexpectedEof {
                    offset: r.position(),
                });
            }
            let subblock_duration = if constant_subblock_duration == 0 {
                if definition.mode {
                    r.read_leb128()?
                } else {
                    definition.subblock_durations[i as usize]
                }
            } else if i == num_subblocks - 1 && duration % constant_subblock_duration != 0 {
                duration % constant_subblock_duration
            } else {
                constant_subblock_duration
            };

            let data = match context {
                ParamContext::MixGain => SubblockData::MixGain(parse_mix_gain(&mut r)?),
                ParamContext::Demixing => SubblockData::Demixing {
                    dmixp_mode: r.read_u8()? >> 5 & 0x07,
                },
                ParamContext::ReconGain(layers) => {
                    SubblockData::ReconGain(parse_recon_gain(&mut r, layers)?)
                }
            };
            subblocks.push(ParameterSubblock {
                duration: subblock_duration,
                data,
            });
        }

        Ok(ParameterBlock {
            parameter_id,
            duration,
            constant_subblock_duration,
            subblocks,
        })
    }
}

fn parse_mix_gain(r: &mut ByteReader<'_>) -> Result<MixGainAnimation, Error> {
    const STEP: u32 = 0;
    const LINEAR: u32 = 1;
    const BEZIER: u32 = 2;
    match r.read_leb128()? {
        STEP => Ok(MixGainAnimation::Step {
            start: r.read_i16_be()?,
        }),
        LINEAR => Ok(MixGainAnimation::Linear {
            start: r.read_i16_be()?,
            end: r.read_i16_be()?,
        }),
        BEZIER => Ok(MixGainAnimation::Bezier {
            start: r.read_i16_be()?,
            end: r.read_i16_be()?,
            control: r.read_i16_be()?,
            control_relative_time: r.read_u8()?,
        }),
        _ => Err(Error::InvalidDescriptor {
            offset: r.position(),
        }),
    }
}

fn parse_recon_gain(
    r: &mut ByteReader<'_>,
    layers: &[ChannelAudioLayer],
) -> Result<ReconGainLayers, Error> {
    layers
        .iter()
        .map(|layer| {
            if !layer.recon_gain_is_present {
                return Ok(None);
            }
            let flags = r.read_leb128()?;
            let gains = (0..u32::BITS)
                .filter(|bit| flags & (1 << bit) != 0)
                .map(|_| r.read_u8())
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some((flags, gains)))
        })
        .collect()
}

/// Timeline of parameter values over the output sample clock, fed from
/// parameter-block subblocks and consumed one temporal unit at a time.
///
/// Each pushed value covers `duration` samples. [`ParamCursor::take_for_unit`]
/// returns the value covering the start of the next unit and advances the
/// clock by the unit length, so a single parameter block whose subblocks span
/// several temporal units applies each subblock to the units it covers.
/// When the timeline runs dry the last value stays in effect (parameter
/// values persist until the next block, matching the previous frame-snapshot
/// behavior); `None` is returned only before any value has arrived, letting
/// callers keep the descriptor defaults.
#[derive(Debug, Clone)]
pub struct ParamCursor<T> {
    queue: std::collections::VecDeque<(T, usize)>,
    last: Option<T>,
}

impl<T> Default for ParamCursor<T> {
    fn default() -> Self {
        ParamCursor {
            queue: std::collections::VecDeque::new(),
            last: None,
        }
    }
}

impl<T: Clone> ParamCursor<T> {
    pub fn push(&mut self, value: T, duration: usize) {
        if duration > 0 {
            self.queue.push_back((value, duration));
        } else {
            // Zero-length coverage still updates the sticky value.
            self.last = Some(value);
        }
    }

    /// Value in effect at the start of a `unit_len`-sample temporal unit;
    /// consumes the unit from the timeline.
    pub fn take_for_unit(&mut self, unit_len: usize) -> Option<T> {
        let value = self
            .queue
            .front()
            .map(|(v, _)| v.clone())
            .or_else(|| self.last.clone());
        let mut remaining = unit_len;
        while remaining > 0 {
            let Some((_, duration)) = self.queue.front_mut() else {
                break;
            };
            let consumed = remaining.min(*duration);
            *duration -= consumed;
            remaining -= consumed;
            if *duration == 0 {
                let (v, _) = self.queue.pop_front().expect("checked non-empty");
                self.last = Some(v);
            }
        }
        value
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(mode: bool) -> ParamDefinition {
        ParamDefinition {
            parameter_id: 7,
            parameter_rate: 48000,
            mode,
            duration: if mode { 0 } else { 1920 },
            constant_subblock_duration: if mode { 0 } else { 960 },
            subblock_durations: Vec::new(),
        }
    }

    #[test]
    fn mix_gain_mode0_constant_subblocks() {
        // Mode 0: timing from the definition (1920/960 => 2 subblocks).
        let mut payload = vec![0x07]; // parameter_id
        payload.push(0x00); // subblock 0: animation_type step
        payload.extend((-256i16).to_be_bytes());
        payload.push(0x01); // subblock 1: linear
        payload.extend((-256i16).to_be_bytes());
        payload.extend(0i16.to_be_bytes());

        let block =
            ParameterBlock::parse(&payload, &definition(false), &ParamContext::MixGain).unwrap();
        assert_eq!(block.subblocks.len(), 2);
        assert_eq!(block.subblocks[0].duration, 960);
        assert_eq!(
            block.subblocks[0].data,
            SubblockData::MixGain(MixGainAnimation::Step { start: -256 })
        );
        assert_eq!(
            block.subblocks[1].data,
            SubblockData::MixGain(MixGainAnimation::Linear {
                start: -256,
                end: 0
            })
        );
    }

    #[test]
    fn demixing_mode1_inline_timing() {
        // Mode 1: duration=960, constant_subblock_duration=960 => 1 subblock.
        let mut payload = vec![0x07];
        payload.extend([0xc0, 0x07]); // duration = 960
        payload.extend([0xc0, 0x07]); // constant_subblock_duration = 960
        payload.push(0x03 << 5); // dmixp_mode = 3

        let block =
            ParameterBlock::parse(&payload, &definition(true), &ParamContext::Demixing).unwrap();
        assert_eq!(block.subblocks.len(), 1);
        assert_eq!(
            block.subblocks[0].data,
            SubblockData::Demixing { dmixp_mode: 3 }
        );
    }

    #[test]
    fn recon_gain_respects_layer_flags() {
        let layer = |present| ChannelAudioLayer {
            loudspeaker_layout: 2,
            substream_count: 1,
            coupled_substream_count: 0,
            recon_gain_is_present: present,
            output_gain: None,
            expanded_loudspeaker_layout: None,
        };
        let mut def = definition(false);
        def.duration = 960;
        def.constant_subblock_duration = 960;

        let mut payload = vec![0x07];
        payload.push(0x05); // layer 1 flags: bits 0 and 2
        payload.extend([200, 180]); // two gain bytes

        let layers = [layer(false), layer(true)];
        let block =
            ParameterBlock::parse(&payload, &def, &ParamContext::ReconGain(&layers)).unwrap();
        let SubblockData::ReconGain(per_layer) = &block.subblocks[0].data else {
            panic!("expected recon gain");
        };
        assert_eq!(per_layer[0], None);
        assert_eq!(per_layer[1], Some((0x05, vec![200, 180])));
    }

    #[test]
    fn cursor_spanning_block_covers_multiple_units() {
        // One block: two 960-sample subblocks with different values.
        let mut cursor = ParamCursor::default();
        cursor.push(1u8, 960);
        cursor.push(2u8, 960);
        assert_eq!(cursor.take_for_unit(960), Some(1));
        assert_eq!(cursor.take_for_unit(960), Some(2));
        // Timeline dry: last value stays in effect.
        assert_eq!(cursor.take_for_unit(960), Some(2));
    }

    #[test]
    fn cursor_uses_value_covering_unit_start() {
        // A subblock boundary mid-unit: the unit takes the value at its
        // start, and the next unit lands in the following subblock.
        let mut cursor = ParamCursor::default();
        cursor.push(7u8, 1440);
        cursor.push(8u8, 480);
        assert_eq!(cursor.take_for_unit(960), Some(7));
        assert_eq!(cursor.take_for_unit(960), Some(7));
        assert_eq!(cursor.take_for_unit(960), Some(8));
    }

    #[test]
    fn cursor_empty_returns_none_until_first_value() {
        let mut cursor: ParamCursor<u8> = ParamCursor::default();
        assert_eq!(cursor.take_for_unit(960), None);
        cursor.push(3, 960);
        assert_eq!(cursor.take_for_unit(960), Some(3));
        cursor.clear();
        assert_eq!(cursor.take_for_unit(960), None);
    }

    #[test]
    fn hostile_subblock_count_bounded() {
        // Mode 1 with a huge explicit num_subblocks and no data.
        let mut payload = vec![0x07];
        payload.extend([0xc0, 0x07]); // duration
        payload.push(0x00); // constant_subblock_duration = 0
        payload.extend([0xff, 0xff, 0xff, 0x7f]); // num_subblocks huge
        assert!(
            ParameterBlock::parse(&payload, &definition(true), &ParamContext::MixGain).is_err()
        );
    }
}
