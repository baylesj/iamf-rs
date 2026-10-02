//! IAMF v2.0 object positions: the position parameter data carried by
//! parameter blocks, and its animation over the output sample clock.
//!
//! An object-based audio element's position comes from the position
//! parameter its mix presentation declares (`PolarParamDefinition`,
//! `Cart8ParamDefinition`, ... in the rendering config). Parameter blocks
//! with that parameter id animate it subblock by subblock (step, linear,
//! Bezier, and the "inter" forms that start where the previous subblock
//! ended); where no block covers a temporal unit, the definition's default
//! applies. Polar linear animations follow the shortest great-circle arc
//! (spherical linear interpolation); everything else is interpolated per
//! coordinate. See the spec's "Animated Parameters" section.

use std::collections::VecDeque;

use iamf_obu::descriptors::{PositionKind, PositionParam};
use iamf_obu::{BitReader, ByteReader, Error};

/// A position as IAMF defines it for objects (ITU-R BS.2076 axes).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ObjectPosition {
    /// Degrees and a normalized distance.
    Polar {
        /// -180..=180, 0 straight ahead, positive to the left.
        azimuth: f32,
        /// -90..=90, positive up.
        elevation: f32,
        /// 0.0..=1.0, 1.0 on the unit sphere.
        distance: f32,
    },
    /// Normalized cube coordinates, -1.0..=1.0.
    Cartesian {
        /// Positive to the right.
        x: f32,
        /// Positive to the front.
        y: f32,
        /// Positive up.
        z: f32,
    },
}

/// How a subblock animates its value (`animation_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionAnimationType {
    /// One value for the whole subblock.
    Step,
    /// From a start to an end value.
    Linear,
    /// Quadratic Bezier from a start to an end value through a control
    /// point.
    Bezier,
    /// Linear from where the previous subblock ended.
    InterLinear,
    /// Bezier from where the previous subblock ended.
    InterBezier,
}

impl PositionAnimationType {
    fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => Self::Step,
            1 => Self::Linear,
            2 => Self::Bezier,
            3 => Self::InterLinear,
            4 => Self::InterBezier,
            _ => return None,
        })
    }

    /// Values coded per coordinate (start, end, control as present).
    fn coded_values(self) -> u32 {
        match self {
            Self::Step | Self::InterLinear => 1,
            Self::Linear | Self::InterBezier => 2,
            Self::Bezier => 3,
        }
    }

    fn has_control(self) -> bool {
        matches!(self, Self::Bezier | Self::InterBezier)
    }

    fn explicit_start(self) -> bool {
        matches!(self, Self::Step | Self::Linear | Self::Bezier)
    }
}

/// One coordinate's `AnimatedParameterData`, as coded. Fields the
/// animation type does not carry are 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AxisAnimation {
    /// `start_point_value` (step, linear, Bezier).
    pub start: i32,
    /// `end_point_value` (all but step).
    pub end: i32,
    /// `control_point_value` (Bezier forms).
    pub control: i32,
    /// `control_point_relative_time`, ×2⁻⁸ of the subblock duration.
    pub control_time: u8,
}

/// The position parameter data of one subblock: one animation type, and
/// per object the animation of each of its three coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionData {
    /// The subblock's animation type.
    pub animation: PositionAnimationType,
    /// Per object (1 or 2), per coordinate in [`PositionKind`] order.
    pub objects: Vec<[AxisAnimation; 3]>,
}

/// Parses `PolarParameterData`, `Cart8ParameterData`, ... and their `Dual`
/// forms: an `animation_type` then bit-packed coordinates.
pub(crate) fn parse_position_data(
    r: &mut ByteReader<'_>,
    kind: PositionKind,
    objects: usize,
) -> Result<PositionData, Error> {
    let at = r.position();
    let animation = PositionAnimationType::from_code(r.read_leb128()?)
        .ok_or(Error::InvalidDescriptor { offset: at })?;
    let bits = kind.field_bits();
    let signed = kind.field_signed();
    let per_axis = |axis: usize| {
        animation.coded_values() * bits[axis] + if animation.has_control() { 8 } else { 0 }
    };
    let total: u32 = (0..3).map(per_axis).sum::<u32>() * objects as u32;
    let start = r.position();
    let data = r.read_bytes(total.div_ceil(8) as usize)?;
    let mut b = BitReader::new(data, start);
    let mut parsed = Vec::with_capacity(objects);
    for _ in 0..objects {
        let mut axes = [AxisAnimation::default(); 3];
        for (axis, slot) in axes.iter_mut().enumerate() {
            // One AnimatedParameterData per coordinate: its values in
            // syntax order, then (Bezier forms) the 8-bit control time.
            let mut value = || -> Result<i32, Error> {
                if signed[axis] {
                    b.read_signed(bits[axis])
                } else {
                    Ok(b.read_bits(bits[axis])? as i32)
                }
            };
            if animation.explicit_start() {
                slot.start = value()?;
            }
            if animation != PositionAnimationType::Step {
                slot.end = value()?;
            }
            if animation.has_control() {
                slot.control = value()?;
                slot.control_time = b.read_bits(8)? as u8;
            }
        }
        parsed.push(axes);
    }
    Ok(PositionData {
        animation,
        objects: parsed,
    })
}

/// A coded coordinate to its value: degrees (clipped) for polar angles,
/// `/127` for the polar distance and 8-bit coordinates, `/32767` for 16-bit
/// ones (normalized coordinates clipped to -1..=1).
fn decode_axis(kind: PositionKind, axis: usize, coded: i32) -> f64 {
    let coded = f64::from(coded);
    match (kind, axis) {
        (PositionKind::Polar, 0) => coded.clamp(-180.0, 180.0),
        (PositionKind::Polar, 1) => coded.clamp(-90.0, 90.0),
        (PositionKind::Polar, _) => (coded / 127.0).clamp(0.0, 1.0),
        (PositionKind::Cart8, _) => (coded / 127.0).clamp(-1.0, 1.0),
        (PositionKind::Cart16, _) => (coded / 32767.0).clamp(-1.0, 1.0),
    }
}

fn to_position(kind: PositionKind, v: [f64; 3]) -> ObjectPosition {
    match kind {
        PositionKind::Polar => ObjectPosition::Polar {
            azimuth: v[0] as f32,
            elevation: v[1] as f32,
            distance: v[2] as f32,
        },
        PositionKind::Cart8 | PositionKind::Cart16 => ObjectPosition::Cartesian {
            x: v[0] as f32,
            y: v[1] as f32,
            z: v[2] as f32,
        },
    }
}

/// Spherical linear interpolation of (azimuth, elevation) in degrees along
/// the shortest great-circle arc, with the spec's axis conventions.
fn slerp(start: [f64; 2], end: [f64; 2], a: f64) -> [f64; 2] {
    let unit = |az: f64, el: f64| {
        let (az, el) = (az.to_radians(), el.to_radians());
        [el.cos() * (-az).sin(), el.cos() * (-az).cos(), el.sin()]
    };
    let vs = unit(start[0], start[1]);
    let ve = unit(end[0], end[1]);
    let dot = (vs[0] * ve[0] + vs[1] * ve[1] + vs[2] * ve[2]).clamp(-1.0, 1.0);
    let omega = dot.acos();
    if omega.abs() < 1e-9 {
        return start;
    }
    let ws = ((1.0 - a) * omega).sin() / omega.sin();
    let we = (a * omega).sin() / omega.sin();
    let v = [
        ws * vs[0] + we * ve[0],
        ws * vs[1] + we * ve[1],
        ws * vs[2] + we * ve[2],
    ];
    [
        (-v[0].atan2(v[1])).to_degrees(),
        v[2].clamp(-1.0, 1.0).asin().to_degrees(),
    ]
}

/// The quadratic Bezier interpolation factor at sample `n` of a subblock
/// of `len` samples whose control point sits at sample `ctrl` (the spec's
/// α, β, γ solution with the subblock starting at 0).
fn bezier_factor(n: f64, len: f64, ctrl: f64) -> f64 {
    let alpha = len - 2.0 * ctrl;
    let beta = 2.0 * ctrl;
    let a = if alpha.abs() < 1e-12 {
        if beta.abs() < 1e-12 { 0.0 } else { n / beta }
    } else {
        (-beta + (beta * beta + 4.0 * alpha * n).max(0.0).sqrt()) / (2.0 * alpha)
    };
    a.clamp(0.0, 1.0)
}

/// One subblock queued on the sample clock.
#[derive(Debug, Clone)]
struct QueuedSubblock {
    data: PositionData,
    /// Duration in parameter ticks and in output samples.
    ticks: u32,
    samples: usize,
    /// Output samples per parameter tick.
    scale: f64,
    /// Start values per object, resolved when the subblock begins.
    start: Option<Vec<[f64; 3]>>,
}

/// The position timeline of one object-based element: subblocks queued in
/// arrival order on the sample clock, consumed one temporal unit at a time.
#[derive(Debug, Clone)]
pub(crate) struct PositionCursor {
    kind: PositionKind,
    defaults: Vec<[f64; 3]>,
    queue: VecDeque<QueuedSubblock>,
    /// Samples of the front subblock already consumed.
    offset: usize,
    /// Where the next subblock that does not state a start begins, per
    /// object: the previous subblock's end (its start for a step), or the
    /// default after a gap.
    carry: Vec<[f64; 3]>,
}

impl PositionCursor {
    pub(crate) fn new(param: &PositionParam) -> Self {
        let defaults: Vec<[f64; 3]> = param
            .defaults
            .iter()
            .map(|d| [0, 1, 2].map(|axis| decode_axis(param.kind, axis, d[axis])))
            .collect();
        Self {
            kind: param.kind,
            carry: defaults.clone(),
            defaults,
            queue: VecDeque::new(),
            offset: 0,
        }
    }

    /// The coordinate system of the parameter.
    pub(crate) fn kind(&self) -> PositionKind {
        self.kind
    }

    /// Objects this cursor positions.
    pub(crate) fn num_objects(&self) -> usize {
        self.defaults.len()
    }

    /// Queues one subblock of `ticks` parameter ticks; `scale` converts
    /// ticks to output samples.
    pub(crate) fn push(&mut self, data: PositionData, ticks: u32, scale: f64) {
        let samples = (f64::from(ticks) * scale) as usize;
        if samples > 0 && data.objects.len() == self.defaults.len() {
            self.queue.push_back(QueuedSubblock {
                data,
                ticks,
                samples,
                scale,
                start: None,
            });
        }
    }

    /// Drops queued subblocks and returns to the defaults (seek).
    pub(crate) fn clear(&mut self) {
        self.queue.clear();
        self.offset = 0;
        self.carry.clone_from(&self.defaults);
    }

    /// Positions of every object at the given sample offsets of the next
    /// `unit_len` samples (offsets ascending, each `< unit_len`), then
    /// advances the clock by `unit_len`. Returns `[object][point]`.
    pub(crate) fn positions_for_unit(
        &mut self,
        unit_len: usize,
        points: &[usize],
    ) -> Vec<Vec<ObjectPosition>> {
        let mut out = vec![Vec::with_capacity(points.len()); self.defaults.len()];
        let mut clock = 0usize; // samples of this unit already walked
        let mut next_point = 0usize;
        while clock < unit_len {
            let Some(front) = self.queue.front_mut() else {
                // A gap: no block covers the rest of the unit, so the
                // default holds, and an "inter" animation after it starts
                // from the default too.
                self.carry.clone_from(&self.defaults);
                for (o, object) in out.iter_mut().enumerate() {
                    let default = to_position(self.kind, self.defaults[o]);
                    object.extend(std::iter::repeat_n(default, points.len() - next_point));
                }
                return out;
            };
            if front.start.is_none() {
                let start = if front.data.animation.explicit_start() {
                    front
                        .data
                        .objects
                        .iter()
                        .map(|axes| [0, 1, 2].map(|i| decode_axis(self.kind, i, axes[i].start)))
                        .collect()
                } else {
                    self.carry.clone()
                };
                front.start = Some(start);
            }
            let remaining = front.samples - self.offset;
            let span = remaining.min(unit_len - clock);
            while next_point < points.len() && points[next_point] < clock + span {
                let n = self.offset + (points[next_point] - clock);
                for (o, object) in out.iter_mut().enumerate() {
                    object.push(evaluate(self.kind, front, o, n));
                }
                next_point += 1;
            }
            clock += span;
            self.offset += span;
            if self.offset >= front.samples {
                let done = self.queue.pop_front().expect("front exists");
                let start = done.start.expect("resolved above");
                self.carry = done
                    .data
                    .objects
                    .iter()
                    .enumerate()
                    .map(|(o, axes)| {
                        if done.data.animation == PositionAnimationType::Step {
                            start[o]
                        } else {
                            [0, 1, 2].map(|i| decode_axis(self.kind, i, axes[i].end))
                        }
                    })
                    .collect();
                self.offset = 0;
            }
        }
        out
    }
}

/// The value of object `o` at sample `n` of subblock `sb`.
fn evaluate(kind: PositionKind, sb: &QueuedSubblock, o: usize, n: usize) -> ObjectPosition {
    let start = sb.start.as_ref().expect("resolved before evaluation")[o];
    let axes = &sb.data.objects[o];
    let end = [0, 1, 2].map(|i| decode_axis(kind, i, axes[i].end));
    let len = sb.samples as f64;
    let n = n as f64;
    let value = match sb.data.animation {
        PositionAnimationType::Step => start,
        PositionAnimationType::Linear | PositionAnimationType::InterLinear => {
            let a = if len > 0.0 {
                (n / len).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let lerp = |i: usize| (1.0 - a) * start[i] + a * end[i];
            if kind == PositionKind::Polar {
                let [az, el] = slerp([start[0], start[1]], [end[0], end[1]], a);
                [az, el, lerp(2)]
            } else {
                [lerp(0), lerp(1), lerp(2)]
            }
        }
        PositionAnimationType::Bezier | PositionAnimationType::InterBezier => [0, 1, 2].map(|i| {
            let control = decode_axis(kind, i, axes[i].control);
            let time = axes[i].control_time;
            let a = if time == 0 {
                // A zero control time is a linear Bezier: the control
                // value is ignored.
                if len > 0.0 {
                    (n / len).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            } else {
                let ctrl_ticks = (f64::from(sb.ticks) * f64::from(time) / 256.0).round();
                bezier_factor(n, len, (ctrl_ticks * sb.scale).floor())
            };
            if time == 0 {
                (1.0 - a) * start[i] + a * end[i]
            } else {
                (1.0 - a) * (1.0 - a) * start[i] + 2.0 * a * (1.0 - a) * control + a * a * end[i]
            }
        }),
    };
    to_position(kind, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use iamf_obu::descriptors::ParamDefinition;

    fn polar_param(default: [i32; 3]) -> PositionParam {
        PositionParam {
            base: ParamDefinition {
                parameter_id: 1,
                parameter_rate: 48000,
                mode: false,
                duration: 1024,
                constant_subblock_duration: 1024,
                subblock_durations: vec![],
            },
            kind: PositionKind::Polar,
            defaults: vec![default],
        }
    }

    fn polar(p: ObjectPosition) -> (f32, f32, f32) {
        match p {
            ObjectPosition::Polar {
                azimuth,
                elevation,
                distance,
            } => (azimuth, elevation, distance),
            ObjectPosition::Cartesian { .. } => panic!("expected polar"),
        }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn parses_the_step_and_inter_linear_blocks_of_test_000800() {
        // Payloads after the parameter id, from the vector's textproto.
        let step = parse_position_data(
            &mut ByteReader::new(&[0x00, 0x00, 0x00, 0x7f]),
            PositionKind::Polar,
            1,
        )
        .unwrap();
        assert_eq!(step.animation, PositionAnimationType::Step);
        assert_eq!(step.objects[0][0].start, 0);
        assert_eq!(step.objects[0][2].start, 127);
        let left = parse_position_data(
            &mut ByteReader::new(&[0x03, 0x2d, 0x00, 0x7f]),
            PositionKind::Polar,
            1,
        )
        .unwrap();
        assert_eq!(left.animation, PositionAnimationType::InterLinear);
        assert_eq!(
            [
                left.objects[0][0].end,
                left.objects[0][1].end,
                left.objects[0][2].end
            ],
            [90, 0, 127]
        );
        let right = parse_position_data(
            &mut ByteReader::new(&[0x03, 0xd3, 0x00, 0x7f]),
            PositionKind::Polar,
            1,
        )
        .unwrap();
        assert_eq!(right.objects[0][0].end, -90);
    }

    #[test]
    fn inter_linear_follows_the_great_circle_from_the_previous_end() {
        let mut cursor = PositionCursor::new(&polar_param([0, 0, 127]));
        let data = |code: u8, az: u8| {
            parse_position_data(
                &mut ByteReader::new(&[code, az, 0x00, 0x7f]),
                PositionKind::Polar,
                1,
            )
            .unwrap()
        };
        cursor.push(data(0x00, 0x00), 1024, 1.0); // step, front
        cursor.push(data(0x03, 0x2d), 1024, 1.0); // to +90
        cursor.push(data(0x03, 0x5a), 1024, 1.0); // to 180
        let points = [0, 512];
        let u0 = cursor.positions_for_unit(1024, &points);
        assert!(close(polar(u0[0][0]).0, 0.0) && close(polar(u0[0][1]).0, 0.0));
        let u1 = cursor.positions_for_unit(1024, &points);
        assert!(close(polar(u1[0][0]).0, 0.0));
        // Halfway along the horizontal arc from 0 to +90 is +45.
        assert!(close(polar(u1[0][1]).0, 45.0), "{:?}", u1[0][1]);
        let u2 = cursor.positions_for_unit(1024, &points);
        assert!(close(polar(u2[0][0]).0, 90.0));
        assert!(close(polar(u2[0][1]).0, 135.0), "{:?}", u2[0][1]);
        // Past the queued blocks the default (front) returns.
        let u3 = cursor.positions_for_unit(1024, &points);
        assert_eq!(polar(u3[0][0]), (0.0, 0.0, 1.0));
    }

    #[test]
    fn slerp_crosses_the_rear_by_the_short_way() {
        // 180 to -90 (= 270) is a 90° arc through the rear-right.
        let [az, el] = slerp([180.0, 0.0], [-90.0, 0.0], 0.5);
        assert!((az + 135.0).abs() < 1e-6, "{az}");
        assert!(el.abs() < 1e-6);
    }

    #[test]
    fn a_block_spanning_two_units_animates_across_both() {
        let mut cursor = PositionCursor::new(&polar_param([0, 0, 127]));
        // Linear from 0 to +60 over 2048 samples.
        let linear = PositionData {
            animation: PositionAnimationType::Linear,
            objects: vec![[
                AxisAnimation {
                    start: 0,
                    end: 60,
                    ..AxisAnimation::default()
                },
                AxisAnimation::default(),
                AxisAnimation {
                    start: 127,
                    end: 127,
                    ..AxisAnimation::default()
                },
            ]],
        };
        cursor.push(linear, 2048, 1.0);
        let first = cursor.positions_for_unit(1024, &[0]);
        let second = cursor.positions_for_unit(1024, &[0]);
        assert!(close(polar(first[0][0]).0, 0.0));
        assert!(close(polar(second[0][0]).0, 30.0), "{:?}", second[0][0]);
    }

    #[test]
    fn bezier_with_a_centred_control_point_is_symmetric() {
        // Control at half time: a = n / len, so the value at mid-point is
        // (start + 2·control + end) / 4.
        let a = bezier_factor(512.0, 1024.0, 512.0);
        assert!((a - 0.5).abs() < 1e-12);
        let a = bezier_factor(0.0, 1024.0, 300.0);
        assert!(a.abs() < 1e-12);
        let a = bezier_factor(1024.0, 1024.0, 300.0);
        assert!((a - 1.0).abs() < 1e-9);
    }
}
