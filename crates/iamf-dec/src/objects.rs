//! Object-based audio element position timelines, coordinate conversions, and
//! loudspeaker panning (ITU-R BS.2127 §10 allocentric sector mapping and
//! layer-wise VBAP/DBAP panning).

use std::collections::VecDeque;

use iamf_obu::Error;
use iamf_obu::position::{
    Animated, AnimatedPosition, AnimationType, CartesianPosition, PolarPosition, Position,
    PositionData, PositionParamDefinition, PositionParamType,
};

use crate::DecodeError;
use crate::layout::SoundSystem;
use crate::params::{ParameterBlock, SubblockData};

/// Dequantized spherical position for an audio object.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PolarCoord {
    /// Azimuth angle in degrees `[-180, 180]` (`+` left, `-` right).
    pub azimuth: f32,
    /// Elevation angle in degrees `[-90, 90]` (`+` up, `-` down).
    pub elevation: f32,
    /// Normalized distance in `[0, 1]`.
    pub distance: f32,
}

impl Default for PolarCoord {
    fn default() -> Self {
        Self {
            azimuth: 0.0,
            elevation: 0.0,
            distance: 1.0,
        }
    }
}

impl PolarCoord {
    /// Dequantizes an OBU [`PolarPosition`] (`azimuth` in `[-180, 180]`, `elevation`
    /// in `[-90, 90]`, `distance` in `0..=127` mapped to `[0.0, 1.0]`).
    pub fn from_polar(p: PolarPosition) -> Self {
        Self {
            azimuth: (p.azimuth as f32).clamp(-180.0, 180.0),
            elevation: (p.elevation as f32).clamp(-90.0, 90.0),
            distance: (p.distance as f32 / 127.0).clamp(0.0, 1.0),
        }
    }

    /// Converts an OBU [`Position`] under `param_type` to a [`PolarCoord`].
    pub fn from_position(pos: Position, param_type: PositionParamType) -> Self {
        match pos {
            Position::Polar(p) => Self::from_polar(p),
            Position::Cartesian(c) => {
                let bits = param_type.cartesian_bits().unwrap_or(8) as u8;
                cart_to_polar_sector(CartCoord::from_cartesian(c, bits))
            }
            _ => Self::default(),
        }
    }

    /// Unit-sphere spherical linear interpolation for `(azimuth, elevation)` and
    /// linear interpolation for `distance`.
    #[must_use]
    pub fn slerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let p0 = polar_to_unit_cart(self.azimuth, self.elevation);
        let p1 = polar_to_unit_cart(other.azimuth, other.elevation);

        let dot = (p0.x * p1.x + p0.y * p1.y + p0.z * p1.z).clamp(-1.0, 1.0);
        let omega = dot.acos();
        let sin_omega = omega.sin();

        let (x, y, z) = if sin_omega.abs() < 1e-6 {
            (
                p0.x + (p1.x - p0.x) * t,
                p0.y + (p1.y - p0.y) * t,
                p0.z + (p1.z - p0.z) * t,
            )
        } else {
            let s0 = ((1.0 - t) * omega).sin() / sin_omega;
            let s1 = (t * omega).sin() / sin_omega;
            (
                s0 * p0.x + s1 * p1.x,
                s0 * p0.y + s1 * p1.y,
                s0 * p0.z + s1 * p1.z,
            )
        };

        let r = (x * x + y * y + z * z).sqrt();
        let (azimuth, elevation) = if r < 1e-9 {
            (0.0, 0.0)
        } else {
            let el = (z / r).clamp(-1.0, 1.0).asin().to_degrees();
            let az = (-x).atan2(y).to_degrees();
            (az, el)
        };
        let distance = self.distance + (other.distance - self.distance) * t;
        Self {
            azimuth,
            elevation,
            distance,
        }
    }
}

/// Dequantized allocentric Cartesian coordinate in `[-1, 1]^3`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CartCoord {
    /// Left (`-`) to right (`+`) coordinate in `[-1, 1]`.
    pub x: f32,
    /// Back (`-`) to front (`+`) coordinate in `[-1, 1]`.
    pub y: f32,
    /// Bottom (`-`) to top (`+`) coordinate in `[-1, 1]`.
    pub z: f32,
}

impl CartCoord {
    /// Dequantizes an OBU [`CartesianPosition`] with `bits` (`8` or `16`).
    pub fn from_cartesian(c: CartesianPosition, bits: u8) -> Self {
        let denom = if bits == 16 { 32767.0 } else { 127.0 };
        Self {
            x: (c.x as f32 / denom).clamp(-1.0, 1.0),
            y: (c.y as f32 / denom).clamp(-1.0, 1.0),
            z: (c.z as f32 / denom).clamp(-1.0, 1.0),
        }
    }

    fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        Self {
            x: self.x + (other.x - self.x) * t,
            y: self.y + (other.y - self.y) * t,
            z: self.z + (other.z - self.z) * t,
        }
    }

    fn bezier(self, end: Self, control: Self, control_relative_time: [f32; 3], t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let solve_s = |t_c: f32| -> f32 {
            let t_c = t_c.clamp(0.0, 1.0);
            let alpha = 1.0 - 2.0 * t_c;
            let beta = 2.0 * t_c;
            let gamma = -t;
            if alpha.abs() < 1e-6 {
                if beta.abs() < 1e-6 {
                    0.0
                } else {
                    (-gamma / beta).clamp(0.0, 1.0)
                }
            } else {
                let disc = beta * beta - 4.0 * alpha * gamma;
                if disc < 0.0 {
                    0.0
                } else {
                    ((-beta + disc.sqrt()) / (2.0 * alpha)).clamp(0.0, 1.0)
                }
            }
        };
        let eval_axis = |v0: f32, v1: f32, vc: f32, s: f32| -> f32 {
            let q0 = v0 + s * (vc - v0);
            let q1 = vc + s * (v1 - vc);
            q0 + s * (q1 - q0)
        };
        let sx = solve_s(control_relative_time[0]);
        let sy = solve_s(control_relative_time[1]);
        let sz = solve_s(control_relative_time[2]);
        Self {
            x: eval_axis(self.x, end.x, control.x, sx),
            y: eval_axis(self.y, end.y, control.y, sy),
            z: eval_axis(self.z, end.z, control.z, sz),
        }
    }
}

fn polar_to_unit_cart(azimuth: f32, elevation: f32) -> CartCoord {
    let az = azimuth.to_radians();
    let el = elevation.to_radians();
    let cos_el = el.cos();
    CartCoord {
        x: -cos_el * az.sin(),
        y: cos_el * az.cos(),
        z: el.sin(),
    }
}

fn polar_to_cart(azimuth: f32, elevation: f32, distance: f32) -> CartCoord {
    let u = polar_to_unit_cart(azimuth, elevation);
    CartCoord {
        x: distance * u.x,
        y: distance * u.y,
        z: distance * u.z,
    }
}

struct SectorMapping {
    azimuth: f32,
    x: f32,
    y: f32,
}

const SECTOR_MAPPINGS: [SectorMapping; 5] = [
    SectorMapping {
        azimuth: 0.0,
        x: 0.0,
        y: 1.0,
    },
    SectorMapping {
        azimuth: -30.0,
        x: 1.0,
        y: 1.0,
    },
    SectorMapping {
        azimuth: -110.0,
        x: 1.0,
        y: -1.0,
    },
    SectorMapping {
        azimuth: 110.0,
        x: -1.0,
        y: -1.0,
    },
    SectorMapping {
        azimuth: 30.0,
        x: -1.0,
        y: 1.0,
    },
];

fn inside_angle_range(mut x: f32, start: f32, mut end: f32, tol: f32) -> bool {
    while end - 360.0 > start {
        end -= 360.0;
    }
    while end < start {
        end += 360.0;
    }
    let start_tol = start - tol;
    while x - 360.0 >= start_tol {
        x -= 360.0;
    }
    while x < start_tol {
        x += 360.0;
    }
    x <= end + tol
}

fn relative_angle(x: f32, mut y: f32) -> f32 {
    while y - 360.0 >= x {
        y -= 360.0;
    }
    while y < x {
        y += 360.0;
    }
    y
}

fn map_linear_to_az(left_az: f32, right_az: f32, x: f32) -> f32 {
    let mid_az = f32::midpoint(left_az, right_az);
    let az_range = right_az - mid_az;
    let gain_l = (x * (std::f32::consts::PI / 2.0)).cos();
    let gain_r = (x * (std::f32::consts::PI / 2.0)).sin();
    let norm_r = gain_r / (gain_l + gain_r);
    let rel_az = ((2.0 * (norm_r - 0.5) * az_range.to_radians().tan()).atan()).to_degrees();
    mid_az + rel_az
}

fn normalize_polar(mut az: f32, mut el: f32, dist: f32) -> PolarCoord {
    el %= 360.0;
    while el > 90.0 {
        el = 180.0 - el;
        az += 180.0;
    }
    while el < -90.0 {
        el = -180.0 - el;
        az += 180.0;
    }
    az %= 360.0;
    if az <= -180.0 {
        az += 360.0;
    }
    if az > 180.0 {
        az -= 360.0;
    }
    if az == -180.0 {
        az = 180.0;
    }
    PolarCoord {
        azimuth: az,
        elevation: el,
        distance: dist.max(0.0),
    }
}

fn cart_to_polar_fallback(c: CartCoord) -> PolarCoord {
    let dist = c.x.hypot(c.y).hypot(c.z);
    if dist < 1e-10 {
        return PolarCoord {
            azimuth: 0.0,
            elevation: 0.0,
            distance: 0.0,
        };
    }
    let az = (-c.x).atan2(c.y).to_degrees();
    let el = (c.z / dist).clamp(-1.0, 1.0).asin().to_degrees();
    normalize_polar(az, el, dist)
}

/// Converts an allocentric Cartesian coordinate in `[-1, 1]^3` to spherical
/// [`PolarCoord`] using the ITU-R BS.2127 §10 sector-based piecewise mapping.
pub fn cart_to_polar_sector(cart: CartCoord) -> PolarCoord {
    const MIN_FLOAT_THRESHOLD: f32 = 1e-10;
    if cart.x.abs() < MIN_FLOAT_THRESHOLD && cart.y.abs() < MIN_FLOAT_THRESHOLD {
        return if cart.z.abs() < MIN_FLOAT_THRESHOLD {
            PolarCoord {
                azimuth: 0.0,
                elevation: 0.0,
                distance: 0.0,
            }
        } else {
            normalize_polar(0.0, cart.z.signum() * 90.0, cart.z.abs())
        };
    }

    let az_val = (-cart.x).atan2(cart.y).to_degrees();
    let Some(sector_idx) = (0..5).find(|&i| {
        let j = (i + 1) % 5;
        let start = (-SECTOR_MAPPINGS[j].x)
            .atan2(SECTOR_MAPPINGS[j].y)
            .to_degrees();
        let end = (-SECTOR_MAPPINGS[i].x)
            .atan2(SECTOR_MAPPINGS[i].y)
            .to_degrees();
        inside_angle_range(az_val, start, end, 0.0)
    }) else {
        return cart_to_polar_fallback(cart);
    };

    let left = &SECTOR_MAPPINGS[sector_idx];
    let right = &SECTOR_MAPPINGS[(sector_idx + 1) % 5];
    let det = left.x * right.y - left.y * right.x;
    if det.abs() < 1e-10 {
        return cart_to_polar_fallback(cart);
    }
    let inv_det = 1.0 / det;
    let inv = [
        right.y * inv_det,
        -left.y * inv_det,
        -right.x * inv_det,
        left.x * inv_det,
    ];
    let g0 = cart.x * inv[0] + cart.y * inv[2];
    let g1 = cart.x * inv[1] + cart.y * inv[3];
    let r_xy = g0 + g1;
    if r_xy.abs() < 1e-10 {
        let dist = (cart.x * cart.x + cart.y * cart.y + cart.z * cart.z).sqrt();
        return normalize_polar(0.0, 0.0, dist);
    }

    let rel_left_az = relative_angle(right.azimuth, left.azimuth);
    let mut az = map_linear_to_az(rel_left_az, right.azimuth, g1 / r_xy);
    az = relative_angle(-180.0, az);

    let el_tilde = (cart.z / r_xy).atan().to_degrees();
    let (el, d) = if el_tilde.abs() > 45.0 {
        let abs_el = 30.0 + (90.0 - 30.0) * (el_tilde.abs() - 45.0) / (90.0 - 45.0);
        (el_tilde.signum() * abs_el, cart.z.abs())
    } else {
        (30.0 * el_tilde / 45.0, r_xy)
    };

    normalize_polar(az, el, d)
}

#[derive(Debug, Clone, Copy)]
enum ResolvedTrack {
    PolarStep(PolarCoord),
    PolarLinear {
        start: PolarCoord,
        end: PolarCoord,
    },
    CartStep(CartCoord),
    CartLinear {
        start: CartCoord,
        end: CartCoord,
    },
    CartBezier {
        start: CartCoord,
        end: CartCoord,
        control: CartCoord,
        control_relative_time: [f32; 3],
    },
}

impl ResolvedTrack {
    fn evaluate(self, t: f32) -> PolarCoord {
        match self {
            Self::PolarStep(p) => p,
            Self::PolarLinear { start, end } => start.slerp(end, t),
            Self::CartStep(c) => cart_to_polar_sector(c),
            Self::CartLinear { start, end } => cart_to_polar_sector(start.lerp(end, t)),
            Self::CartBezier {
                start,
                end,
                control,
                control_relative_time,
            } => cart_to_polar_sector(start.bezier(end, control, control_relative_time, t)),
        }
    }
}

#[derive(Debug, Clone)]
struct PositionSegment {
    duration: usize,
    offset: usize,
    tracks: Vec<ResolvedTrack>,
}

fn resolve_animated_scalar<T: Copy>(anim: Animated<T>, prev: T) -> (T, T) {
    match anim {
        Animated::Step { start } => (start, start),
        Animated::Linear { start, end } | Animated::Bezier { start, end, .. } => (start, end),
        Animated::InterLinear { end } | Animated::InterBezier { end, .. } => (prev, end),
        _ => (prev, prev),
    }
}

fn resolve_animated_bezier<T: Copy>(anim: Animated<T>, prev: T) -> (T, T, T, u8) {
    match anim {
        Animated::Step { start } => (start, start, start, 0),
        Animated::Linear { start, end } => (start, end, start, 0),
        Animated::Bezier {
            start,
            end,
            control,
            control_relative_time,
        } => (start, end, control, control_relative_time),
        Animated::InterLinear { end } => (prev, end, prev, 0),
        Animated::InterBezier {
            end,
            control,
            control_relative_time,
        } => (prev, end, control, control_relative_time),
        _ => (prev, prev, prev, 0),
    }
}

/// Streaming cursor over position parameter subblocks for 1 or 2 objects.
#[derive(Debug, Clone)]
pub(crate) struct PositionCursor {
    param_type: PositionParamType,
    defaults: Vec<Position>,
    default_polar: Vec<PolarCoord>,
    last_end: Vec<Position>,
    in_gap: bool,
    samples_pushed: usize,
    queue: VecDeque<PositionSegment>,
}

impl PositionCursor {
    /// Creates a new position cursor from the element's [`PositionParamDefinition`].
    pub(crate) fn from_definition(
        num_objects: u8,
        def: &PositionParamDefinition,
    ) -> Result<Self, DecodeError> {
        let num_objs = usize::from(num_objects);
        if def.defaults.len() != num_objs {
            return Err(DecodeError::InvalidDescriptors(
                "position parameter object count does not match element num_objects".into(),
            ));
        }
        let defaults = def.defaults.clone();
        let default_polar = defaults
            .iter()
            .map(|&p| PolarCoord::from_position(p, def.param_type))
            .collect();
        Ok(Self {
            param_type: def.param_type,
            last_end: defaults.clone(),
            defaults,
            default_polar,
            in_gap: false,
            samples_pushed: 0,
            queue: VecDeque::new(),
        })
    }

    /// Resets the cursor queue and restores default state.
    pub(crate) fn clear(&mut self) {
        self.queue.clear();
        self.in_gap = false;
        self.samples_pushed = 0;
        self.last_end.clone_from(&self.defaults);
    }

    /// Fills any parameter block gap up to `target_samples` with default positions.
    pub(crate) fn advance_to(&mut self, target_samples: usize) {
        if target_samples > self.samples_pushed {
            let gap = target_samples - self.samples_pushed;
            self.last_end.clone_from(&self.defaults);
            self.in_gap = false;
            let tracks = self
                .default_polar
                .iter()
                .map(|&p| ResolvedTrack::PolarStep(p))
                .collect();
            self.queue.push_back(PositionSegment {
                duration: gap,
                offset: 0,
                tracks,
            });
            self.samples_pushed = target_samples;
        }
    }

    /// Pushes a parsed [`PositionData`] subblock covering `duration` samples at the
    /// element's sample rate.
    pub(crate) fn push(&mut self, data: &PositionData, duration: usize) -> Result<(), DecodeError> {
        let num_objects = self.defaults.len();
        if data.positions.len() != num_objects {
            return Err(DecodeError::CorruptPacket(
                "position subblock object count does not match parameter definition".into(),
            ));
        }
        if duration == 0 {
            return Ok(());
        }
        if self.in_gap {
            self.last_end.clone_from(&self.defaults);
            self.in_gap = false;
        }
        self.samples_pushed = self.samples_pushed.saturating_add(duration);

        let bits = self.param_type.cartesian_bits().unwrap_or(8) as u8;
        let mut tracks = Vec::with_capacity(num_objects);
        for (obj, anim_pos) in data.positions.iter().enumerate() {
            match *anim_pos {
                AnimatedPosition::Polar(ap) => {
                    let prev = match self.last_end.get(obj).copied() {
                        Some(Position::Polar(p)) => p,
                        _ => PolarPosition::new(0, 0, 127),
                    };
                    let (az0, az1) = resolve_animated_scalar(ap.azimuth, prev.azimuth);
                    let (el0, el1) = resolve_animated_scalar(ap.elevation, prev.elevation);
                    let (d0, d1) = resolve_animated_scalar(ap.distance, prev.distance);
                    let start_pos = PolarPosition::new(az0, el0, d0);
                    let end_pos = PolarPosition::new(az1, el1, d1);
                    let track = if matches!(data.animation_type, AnimationType::Step) {
                        ResolvedTrack::PolarStep(PolarCoord::from_polar(start_pos))
                    } else {
                        ResolvedTrack::PolarLinear {
                            start: PolarCoord::from_polar(start_pos),
                            end: PolarCoord::from_polar(end_pos),
                        }
                    };
                    if let Some(slot) = self.last_end.get_mut(obj) {
                        *slot = Position::Polar(end_pos);
                    }
                    tracks.push(track);
                }
                AnimatedPosition::Cartesian(ac) => {
                    let prev = match self.last_end.get(obj).copied() {
                        Some(Position::Cartesian(c)) => c,
                        _ => CartesianPosition::new(0, 0, 0),
                    };
                    let (x0, x1, xc, xt) = resolve_animated_bezier(ac.x, prev.x);
                    let (y0, y1, yc, yt) = resolve_animated_bezier(ac.y, prev.y);
                    let (z0, z1, zc, zt) = resolve_animated_bezier(ac.z, prev.z);
                    let start_pos = CartesianPosition::new(x0, y0, z0);
                    let end_pos = CartesianPosition::new(x1, y1, z1);
                    let ctrl_pos = CartesianPosition::new(xc, yc, zc);
                    let track = match data.animation_type {
                        AnimationType::Step => {
                            ResolvedTrack::CartStep(CartCoord::from_cartesian(start_pos, bits))
                        }
                        AnimationType::Linear | AnimationType::InterLinear => {
                            ResolvedTrack::CartLinear {
                                start: CartCoord::from_cartesian(start_pos, bits),
                                end: CartCoord::from_cartesian(end_pos, bits),
                            }
                        }
                        AnimationType::Bezier | AnimationType::InterBezier => {
                            ResolvedTrack::CartBezier {
                                start: CartCoord::from_cartesian(start_pos, bits),
                                end: CartCoord::from_cartesian(end_pos, bits),
                                control: CartCoord::from_cartesian(ctrl_pos, bits),
                                control_relative_time: [
                                    f32::from(xt) / 256.0,
                                    f32::from(yt) / 256.0,
                                    f32::from(zt) / 256.0,
                                ],
                            }
                        }
                        _ => ResolvedTrack::CartStep(CartCoord::from_cartesian(start_pos, bits)),
                    };
                    if let Some(slot) = self.last_end.get_mut(obj) {
                        *slot = Position::Cartesian(end_pos);
                    }
                    tracks.push(track);
                }
                _ => {
                    tracks.push(ResolvedTrack::PolarStep(PolarCoord::default()));
                }
            }
        }

        self.queue.push_back(PositionSegment {
            duration,
            offset: 0,
            tracks,
        });
        Ok(())
    }

    /// Evaluates the object positions at the start of the current temporal unit
    /// and advances the cursor by `unit_len` samples.
    pub(crate) fn take_for_unit(&mut self, unit_len: usize) -> Vec<PolarCoord> {
        let coords = if let Some(front) = self.queue.front() {
            let t = if front.duration == 0 {
                0.0
            } else {
                (front.offset as f32 / front.duration as f32).clamp(0.0, 1.0)
            };
            front.tracks.iter().map(|trk| trk.evaluate(t)).collect()
        } else {
            self.in_gap = true;
            self.default_polar.clone()
        };

        let mut rem = unit_len;
        while rem > 0 {
            let Some(front) = self.queue.front_mut() else {
                self.in_gap = true;
                self.samples_pushed = self.samples_pushed.saturating_add(rem);
                break;
            };
            let avail = front.duration.saturating_sub(front.offset);
            let step = rem.min(avail);
            front.offset += step;
            rem -= step;
            if front.offset >= front.duration {
                self.queue.pop_front();
            }
        }

        coords
    }
}

/// Validates a parsed [`PositionData`] against the parameter's [`PositionParamType`].
pub(crate) fn validate_position_data(
    param_type: PositionParamType,
    data: &PositionData,
    offset: usize,
) -> Result<(), Error> {
    if param_type.is_polar()
        && matches!(
            data.animation_type,
            AnimationType::Bezier | AnimationType::InterBezier
        )
    {
        return Err(Error::InvalidDescriptor { offset });
    }
    Ok(())
}

/// Evaluates a full-stream position parameter track for [`crate::PresentationDecoder`],
/// returning one `Vec<PolarCoord>` per audio frame.
pub(crate) fn evaluate_position_track(
    num_objects: u8,
    def: &PositionParamDefinition,
    blocks: &[(usize, ParameterBlock)],
    sample_rate: u32,
    unit_lengths: &[usize],
) -> Result<Vec<Vec<PolarCoord>>, DecodeError> {
    let mut cursor = PositionCursor::from_definition(num_objects, def)?;
    let scale = if sample_rate == 0 {
        1.0
    } else {
        (f64::from(sample_rate) + 0.1) / f64::from(def.base.parameter_rate.max(1))
    };
    let mut unit_offsets = Vec::with_capacity(unit_lengths.len() + 1);
    let mut acc = 0usize;
    for &len in unit_lengths {
        unit_offsets.push(acc);
        acc += len;
    }
    unit_offsets.push(acc);
    for (unit_index, block) in blocks {
        if let Some(&target_samples) = unit_offsets.get(*unit_index) {
            cursor.advance_to(target_samples);
        }
        for sb in &block.subblocks {
            if let SubblockData::Position(pos) = &sb.data {
                cursor.push(pos, (f64::from(sb.duration) * scale) as usize)?;
            }
        }
    }
    let mut out = Vec::with_capacity(unit_lengths.len());
    for &unit_len in unit_lengths {
        out.push(cursor.take_for_unit(unit_len));
    }
    Ok(out)
}

// ============================================================================
// Object Loudspeaker Renderer (OLR: Layer-wise Tangent Law + 2D VBAP/DBAP)
// ============================================================================

#[derive(Debug, Clone, Copy)]
struct SpeakerPos {
    azimuth: f32,
    elevation: f32,
    is_lfe: bool,
}

impl SpeakerPos {
    const fn new(azimuth: f32, elevation: f32, is_lfe: bool) -> Self {
        Self {
            azimuth,
            elevation,
            is_lfe,
        }
    }
}

const SP_MP000: SpeakerPos = SpeakerPos::new(0.0, 0.0, false);
const SP_MM025: SpeakerPos = SpeakerPos::new(-22.5, 0.0, false);
const SP_MP025: SpeakerPos = SpeakerPos::new(22.5, 0.0, false);
const SP_MM030: SpeakerPos = SpeakerPos::new(-30.0, 0.0, false);
const SP_MP030: SpeakerPos = SpeakerPos::new(30.0, 0.0, false);
const SP_MM060: SpeakerPos = SpeakerPos::new(-60.0, 0.0, false);
const SP_MP060: SpeakerPos = SpeakerPos::new(60.0, 0.0, false);
const SP_MM090: SpeakerPos = SpeakerPos::new(-90.0, 0.0, false);
const SP_MP090: SpeakerPos = SpeakerPos::new(90.0, 0.0, false);
const SP_MM110: SpeakerPos = SpeakerPos::new(-110.0, 0.0, false);
const SP_MP110: SpeakerPos = SpeakerPos::new(110.0, 0.0, false);
const SP_MM135: SpeakerPos = SpeakerPos::new(-135.0, 0.0, false);
const SP_MP135: SpeakerPos = SpeakerPos::new(135.0, 0.0, false);
const SP_MP180: SpeakerPos = SpeakerPos::new(180.0, 0.0, false);

const SP_UP000: SpeakerPos = SpeakerPos::new(0.0, 30.0, false);
const SP_UM030: SpeakerPos = SpeakerPos::new(-30.0, 30.0, false);
const SP_UP030: SpeakerPos = SpeakerPos::new(30.0, 30.0, false);
const SP_UM045: SpeakerPos = SpeakerPos::new(-45.0, 30.0, false);
const SP_UP045: SpeakerPos = SpeakerPos::new(45.0, 30.0, false);
const SP_UM090: SpeakerPos = SpeakerPos::new(-90.0, 30.0, false);
const SP_UP090: SpeakerPos = SpeakerPos::new(90.0, 30.0, false);
const SP_UM110: SpeakerPos = SpeakerPos::new(-110.0, 30.0, false);
const SP_UP110: SpeakerPos = SpeakerPos::new(110.0, 30.0, false);
const SP_UM135: SpeakerPos = SpeakerPos::new(-135.0, 30.0, false);
const SP_UP135: SpeakerPos = SpeakerPos::new(135.0, 30.0, false);
const SP_UP180: SpeakerPos = SpeakerPos::new(180.0, 30.0, false);
const SP_UHP180: SpeakerPos = SpeakerPos::new(180.0, 15.0, false);

const SP_BP000: SpeakerPos = SpeakerPos::new(0.0, -30.0, false);
const SP_BM045: SpeakerPos = SpeakerPos::new(-45.0, -30.0, false);
const SP_BP045: SpeakerPos = SpeakerPos::new(45.0, -30.0, false);
const SP_BM135: SpeakerPos = SpeakerPos::new(-135.0, -30.0, false);
const SP_BP135: SpeakerPos = SpeakerPos::new(135.0, -30.0, false);

const SP_TP000: SpeakerPos = SpeakerPos::new(0.0, 90.0, false);
const SP_LFE1: SpeakerPos = SpeakerPos::new(45.0, -30.0, true);
const SP_LFE2: SpeakerPos = SpeakerPos::new(-45.0, -30.0, true);

fn olr_speakers(target: SoundSystem) -> Option<&'static [SpeakerPos]> {
    match target {
        SoundSystem::Mono => Some(&[SP_MP000]),
        SoundSystem::A => Some(&[SP_MP030, SP_MM030]),
        SoundSystem::B => Some(&[SP_MP030, SP_MM030, SP_MP000, SP_LFE1, SP_MP110, SP_MM110]),
        SoundSystem::C => Some(&[
            SP_MP030, SP_MM030, SP_MP000, SP_LFE1, SP_MP110, SP_MM110, SP_UP030, SP_UM030,
        ]),
        SoundSystem::D => Some(&[
            SP_MP030, SP_MM030, SP_MP000, SP_LFE1, SP_MP110, SP_MM110, SP_UP030, SP_UM030,
            SP_UP110, SP_UM110,
        ]),
        SoundSystem::E => Some(&[
            SP_MP030, SP_MM030, SP_MP000, SP_LFE1, SP_MP110, SP_MM110, SP_UP030, SP_UM030,
            SP_UP110, SP_UM110, SP_BP000,
        ]),
        SoundSystem::F => Some(&[
            SP_MP000, SP_MP030, SP_MM030, SP_UP045, SP_UM045, SP_MP090, SP_MM090, SP_MP135,
            SP_MM135, SP_UHP180, SP_LFE1, SP_LFE2,
        ]),
        SoundSystem::G => Some(&[
            SP_MP030, SP_MM030, SP_MP000, SP_LFE1, SP_MP090, SP_MM090, SP_MP135, SP_MM135,
            SP_UP045, SP_UM045, SP_UP135, SP_UM135, SP_MP025, SP_MM025,
        ]),
        SoundSystem::H => Some(&[
            SP_MP060, SP_MM060, SP_MP000, SP_LFE1, SP_MP135, SP_MM135, SP_MP030, SP_MM030,
            SP_MP180, SP_LFE2, SP_MP090, SP_MM090, SP_UP045, SP_UM045, SP_UP000, SP_TP000,
            SP_UP135, SP_UM135, SP_UP090, SP_UM090, SP_UP180, SP_BP000, SP_BP045, SP_BM045,
        ]),
        SoundSystem::I => Some(&[
            SP_MP030, SP_MM030, SP_MP000, SP_LFE1, SP_MP090, SP_MM090, SP_MP135, SP_MM135,
        ]),
        SoundSystem::J => Some(&[
            SP_MP030, SP_MM030, SP_MP000, SP_LFE1, SP_MP090, SP_MM090, SP_MP135, SP_MM135,
            SP_UP045, SP_UM045, SP_UP135, SP_UM135,
        ]),
        SoundSystem::Ext712 => Some(&[
            SP_MP030, SP_MM030, SP_MP000, SP_LFE1, SP_MP090, SP_MM090, SP_MP135, SP_MM135,
            SP_UP045, SP_UM045,
        ]),
        SoundSystem::Ext312 => Some(&[SP_MP030, SP_MM030, SP_MP000, SP_LFE1, SP_UP045, SP_UM045]),
        SoundSystem::Ext916 => Some(&[
            SP_MP060, SP_MM060, SP_MP000, SP_LFE1, SP_MP135, SP_MM135, SP_MP030, SP_MM030,
            SP_MP090, SP_MM090, SP_UP045, SP_UM045, SP_UP135, SP_UM135, SP_UP090, SP_UM090,
        ]),
        SoundSystem::Ext7154 => Some(&[
            SP_MP030, SP_MM030, SP_MP000, SP_LFE1, SP_MP090, SP_MM090, SP_MP135, SP_MM135,
            SP_UP045, SP_UM045, SP_TP000, SP_UP135, SP_UM135, SP_BP045, SP_BM045, SP_BP135,
            SP_BM135,
        ]),
        SoundSystem::Binaural => None,
    }
}

#[derive(Debug, Clone, Copy)]
struct HullNode {
    x: f32,
    y: f32,
    index: usize,
}

fn dist_sq_2d(p1: HullNode, p2: HullNode) -> f32 {
    (p1.x - p2.x) * (p1.x - p2.x) + (p1.y - p2.y) * (p1.y - p2.y)
}

fn orientation_2d(p: HullNode, q: HullNode, r: HullNode) -> i32 {
    let val = (q.y - p.y) * (r.x - q.x) - (q.x - p.x) * (r.y - q.y);
    if val.abs() < 1e-9 {
        0
    } else if val > 0.0 {
        1
    } else {
        2
    }
}

fn solve_convex_hull(mut nodes: Vec<HullNode>) -> Option<Vec<HullNode>> {
    let n = nodes.len();
    if n < 3 {
        return None;
    }
    let mut min_idx = 0;
    let mut ymin = nodes[0].y;
    for i in 1..n {
        let y = nodes[i].y;
        if (y < ymin) || ((y - ymin).abs() < 1e-9 && nodes[i].x < nodes[min_idx].x) {
            ymin = y;
            min_idx = i;
        }
    }
    nodes.swap(0, min_idx);
    let p0 = nodes[0];
    nodes[1..].sort_by(|&p1, &p2| {
        let o = orientation_2d(p0, p1, p2);
        if o == 0 {
            if dist_sq_2d(p0, p2) >= dist_sq_2d(p0, p1) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        } else if o == 2 {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    });

    let mut m = 1;
    let mut i = 1;
    while i < n {
        while i < n - 1 && orientation_2d(p0, nodes[i], nodes[i + 1]) == 0 {
            i += 1;
        }
        nodes[m] = nodes[i];
        m += 1;
        i += 1;
    }
    if m < 3 {
        return None;
    }

    let mut stack = Vec::with_capacity(m);
    stack.push(nodes[0]);
    stack.push(nodes[1]);
    stack.push(nodes[2]);
    for &node in nodes.iter().take(m).skip(3) {
        while stack.len() > 1 {
            let top = stack[stack.len() - 1];
            let next_to_top = stack[stack.len() - 2];
            if orientation_2d(next_to_top, top, node) == 2 {
                break;
            }
            stack.pop();
        }
        stack.push(node);
    }
    Some(stack)
}

#[derive(Debug, Clone)]
struct VbapRegion2D {
    speaker_indices: [usize; 2],
    inv_matrix: [[f32; 2]; 2],
}

impl VbapRegion2D {
    fn new(speaker_indices: [usize; 2], pos0: CartCoord, pos1: CartCoord) -> Option<Self> {
        let det = pos0.x * pos1.y - pos1.x * pos0.y;
        if det.abs() < 1e-6 {
            return None;
        }
        let inv_det = 1.0 / det;
        Some(Self {
            speaker_indices,
            inv_matrix: [
                [pos1.y * inv_det, -pos1.x * inv_det],
                [-pos0.y * inv_det, pos0.x * inv_det],
            ],
        })
    }

    fn calculate_gains(&self, azimuth: f32, elevation: f32, distance: f32) -> Option<[f32; 2]> {
        let c = polar_to_cart(azimuth, elevation, distance);
        let g0 = c.x * self.inv_matrix[0][0] + c.y * self.inv_matrix[0][1];
        let g1 = c.x * self.inv_matrix[1][0] + c.y * self.inv_matrix[1][1];
        if g0 < -1e-6 || g1 < -1e-6 {
            return None;
        }
        Some([g0.max(0.0), g1.max(0.0)])
    }
}

#[derive(Debug, Clone)]
struct VbapPanner2D {
    target_speakers: Vec<SpeakerPos>,
    is_convex_hull: bool,
    regions: Vec<VbapRegion2D>,
}

impl VbapPanner2D {
    fn new(speakers: &[SpeakerPos]) -> Self {
        let mut min_azi = 360.0f32;
        let mut max_azi = -360.0f32;
        for sp in speakers {
            min_azi = min_azi.min(sp.azimuth);
            max_azi = max_azi.max(sp.azimuth);
        }
        let is_convex_hull = max_azi > 90.0 && min_azi < -90.0;

        let cart: Vec<CartCoord> = speakers
            .iter()
            .map(|sp| polar_to_cart(sp.azimuth, sp.elevation, 1.0))
            .collect();

        let simplices: Vec<[usize; 2]> = if speakers.len() > 1 {
            if is_convex_hull {
                let nodes: Vec<HullNode> = cart
                    .iter()
                    .enumerate()
                    .map(|(i, c)| HullNode {
                        x: c.x,
                        y: c.y,
                        index: i,
                    })
                    .collect();
                if let Some(hull) = solve_convex_hull(nodes) {
                    let m = hull.len();
                    (0..m)
                        .map(|i| {
                            let idx1 = hull[i].index;
                            let idx2 = hull[(i + 1) % m].index;
                            if cart[idx1].x < cart[idx2].x {
                                [idx1, idx2]
                            } else {
                                [idx2, idx1]
                            }
                        })
                        .collect()
                } else {
                    Vec::new()
                }
            } else {
                let mut indexed: Vec<(usize, f32)> = speakers
                    .iter()
                    .enumerate()
                    .map(|(i, sp)| (i, sp.azimuth))
                    .collect();
                indexed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
                (0..indexed.len() - 1)
                    .map(|i| [indexed[i].0, indexed[i + 1].0])
                    .collect()
            }
        } else {
            Vec::new()
        };

        let mut regions = Vec::with_capacity(simplices.len());
        for pair in simplices {
            if let Some(reg) = VbapRegion2D::new(pair, cart[pair[0]], cart[pair[1]]) {
                regions.push(reg);
            }
        }

        Self {
            target_speakers: speakers.to_vec(),
            is_convex_hull,
            regions,
        }
    }

    fn calculate_gains(&self, azimuth: f32, elevation: f32, distance: f32, gains: &mut [f32]) {
        gains.fill(0.0);
        for reg in &self.regions {
            if let Some(rg) = reg.calculate_gains(azimuth, elevation, distance) {
                let norm = (rg[0] * rg[0] + rg[1] * rg[1]).sqrt();
                if norm > 0.0 {
                    gains[reg.speaker_indices[0]] += rg[0] / norm;
                    gains[reg.speaker_indices[1]] += rg[1] / norm;
                    return;
                }
            }
        }

        let mut min_diff = 360.0f32;
        let mut closest = 0usize;
        for (j, sp) in self.target_speakers.iter().enumerate() {
            let diff = (sp.azimuth - azimuth).abs();
            if diff < min_diff {
                min_diff = diff;
                closest = j;
            }
        }
        if closest < gains.len() {
            gains[closest] = 1.0;
        }
    }
}

#[derive(Debug, Clone)]
struct DbapPanner2D {
    speaker_coords: Vec<CartCoord>,
}

impl DbapPanner2D {
    fn new(speakers: &[SpeakerPos]) -> Self {
        let speaker_coords = speakers
            .iter()
            .map(|sp| polar_to_cart(sp.azimuth, sp.elevation, 1.0))
            .collect();
        Self { speaker_coords }
    }

    fn calculate_gains(&self, azimuth: f32, elevation: f32, distance: f32, gains: &mut [f32]) {
        gains.fill(0.0);
        let n = self.speaker_coords.len();
        if n == 0 || gains.len() < n {
            return;
        }
        let src = polar_to_cart(azimuth, elevation, distance);
        let mut sum_of_weights = 0.0f32;
        for (sp, g) in self.speaker_coords.iter().zip(gains.iter_mut()) {
            let dx = sp.x - src.x;
            let dy = sp.y - src.y;
            let dz = sp.z - src.z;
            let dist = (dx * dx + dy * dy + dz * dz).sqrt().max(0.1);
            let weight = 1.0 / (dist * dist);
            *g = weight;
            sum_of_weights += weight;
        }
        if sum_of_weights > 0.0 {
            for g in gains.iter_mut().take(n) {
                *g /= sum_of_weights;
            }
        }
        let l2_norm = gains.iter().take(n).map(|&g| g * g).sum::<f32>().sqrt();
        if l2_norm > 1e-10 {
            for g in gains.iter_mut().take(n) {
                *g /= l2_norm;
            }
        }
    }
}

fn tanlaw(source_angle: f32, speaker_angle: f32) -> Option<[f32; 2]> {
    if source_angle > speaker_angle || !(0.0..90.0).contains(&speaker_angle) {
        return None;
    }
    let c = source_angle.to_radians().cos() / speaker_angle.to_radians().cos();
    let s = source_angle.to_radians().sin() / speaker_angle.to_radians().sin();
    let g0 = c + s;
    let g1 = c - s;
    let norm = (g0 * g0 + g1 * g1).sqrt();
    if norm == 0.0 {
        return None;
    }
    Some([g0 / norm, g1 / norm])
}

#[derive(Debug, Clone)]
enum LayerPanner {
    SingleSpeaker,
    Multi {
        vbap: VbapPanner2D,
        dbap: DbapPanner2D,
    },
}

#[derive(Debug, Clone)]
struct ElevationLayer {
    elevation: f32,
    speaker_indices_wo_lfe: Vec<usize>,
    panner: LayerPanner,
}

#[derive(Debug, Clone)]
struct CustomGainCalculator {
    speakers: &'static [SpeakerPos],
    num_wo_lfe: usize,
    layers: Vec<ElevationLayer>,
}

impl CustomGainCalculator {
    fn new(target: SoundSystem) -> Option<Self> {
        let speakers = olr_speakers(target)?;
        let speakers_wo_lfe: Vec<SpeakerPos> =
            speakers.iter().copied().filter(|sp| !sp.is_lfe).collect();
        let num_wo_lfe = speakers_wo_lfe.len();

        let mut layer_elevations: Vec<f32> = Vec::new();
        for sp in &speakers_wo_lfe {
            if !layer_elevations
                .iter()
                .any(|&el| (el - sp.elevation).abs() < 1e-5)
            {
                layer_elevations.push(sp.elevation);
            }
        }

        let mut layers = Vec::with_capacity(layer_elevations.len());
        for el in layer_elevations {
            let mut indices = Vec::new();
            let mut layer_speakers = Vec::new();
            for (idx, sp) in speakers_wo_lfe.iter().enumerate() {
                if (sp.elevation - el).abs() < 1e-5 {
                    indices.push(idx);
                    layer_speakers.push(*sp);
                }
            }
            let panner = if layer_speakers.len() <= 1 {
                LayerPanner::SingleSpeaker
            } else {
                LayerPanner::Multi {
                    vbap: VbapPanner2D::new(&layer_speakers),
                    dbap: DbapPanner2D::new(&layer_speakers),
                }
            };
            layers.push(ElevationLayer {
                elevation: el,
                speaker_indices_wo_lfe: indices,
                panner,
            });
        }

        Some(Self {
            speakers,
            num_wo_lfe,
            layers,
        })
    }

    fn layerwise_gains(&self, elevation: f32) -> Vec<(f32, f32)> {
        let mut min_ele = 360.0f32;
        let mut max_ele = -360.0f32;
        for l in &self.layers {
            min_ele = min_ele.min(l.elevation);
            max_ele = max_ele.max(l.elevation);
        }

        if let Some(l) = self
            .layers
            .iter()
            .find(|l| (l.elevation - elevation).abs() < 1e-5)
        {
            return vec![(l.elevation, 1.0)];
        }
        if elevation < min_ele {
            return vec![(min_ele, 1.0)];
        }
        if elevation > max_ele {
            return vec![(max_ele, 1.0)];
        }

        let mut lower_layer = min_ele;
        let mut upper_layer = max_ele;
        for l in &self.layers {
            if l.elevation < elevation {
                lower_layer = lower_layer.max(l.elevation);
            }
            if l.elevation > elevation {
                upper_layer = upper_layer.min(l.elevation);
            }
        }
        let span = upper_layer - lower_layer;
        let mid = lower_layer + span / 2.0;
        if let Some(tg) = tanlaw(elevation - mid, span / 2.0) {
            vec![(upper_layer, tg[0]), (lower_layer, tg[1])]
        } else {
            Vec::new()
        }
    }

    fn calculate_gains(&self, coord: PolarCoord, out_gains: &mut [f32]) {
        out_gains.fill(0.0);
        let mut gains_wo_lfe = vec![0.0f32; self.num_wo_lfe];
        let mut layer_gains = self.layerwise_gains(coord.elevation);

        let mut min_layer_el = 360.0f32;
        let mut max_layer_el = -360.0f32;
        for l in &self.layers {
            min_layer_el = min_layer_el.min(l.elevation);
            max_layer_el = max_layer_el.max(l.elevation);
        }

        let mut layer_cnt = layer_gains.len() as isize;
        let mut new_elevation = 360.0f32;
        let mut i = 0usize;

        while i < layer_gains.len() {
            let (ele_key, ele_val) = layer_gains[i];
            i += 1;
            layer_cnt -= 1;

            let Some(layer) = self
                .layers
                .iter()
                .find(|l| (l.elevation - ele_key).abs() < 1e-5)
            else {
                continue;
            };

            if let LayerPanner::Multi { ref vbap, .. } = layer.panner {
                let check_hull = layer_cnt >= 0 && ele_key.abs() > 1e-5 && !vbap.is_convex_hull;
                if check_hull {
                    let mut max_az = -360.0f32;
                    for sp in &vbap.target_speakers {
                        max_az = max_az.max(sp.azimuth);
                    }
                    if coord.azimuth.abs() > max_az {
                        new_elevation =
                            ele_key * (1.0 - ((coord.azimuth.abs() - max_az) / (180.0 - max_az)));
                        let sub_gains = self.layerwise_gains(new_elevation);
                        for (sub_el, sub_g) in sub_gains {
                            layer_gains.push((sub_el, sub_g * ele_val));
                        }
                        continue;
                    }
                }
            }

            let mut current_el = coord.elevation;
            if layer_cnt < 0 {
                current_el = new_elevation;
            }
            let mut current_dist = coord.distance;
            if current_el > max_layer_el || current_el < min_layer_el {
                current_dist *= current_el.to_radians().cos();
            }

            let mut vbap_gains = vec![0.0f32; self.num_wo_lfe];
            let mut dbap_gains = vec![0.0f32; self.num_wo_lfe];
            let mut gains_2d = vec![0.0f32; self.num_wo_lfe];

            let (dbap_dist, alpha) = if current_dist < 0.5 {
                (0.0, current_dist)
            } else if current_dist < 0.9 {
                (2.0 * current_dist - 1.0, 0.5)
            } else {
                (0.8, (0.5 + 0.5 * (current_dist - 0.9) / 0.1).min(1.0))
            };

            match &layer.panner {
                LayerPanner::SingleSpeaker => {
                    vbap_gains[0] = 1.0;
                    dbap_gains[0] = 1.0;
                }
                LayerPanner::Multi { vbap, dbap } => {
                    let mut final_az = coord.azimuth;
                    if !vbap.is_convex_hull {
                        if final_az > 90.0 && final_az <= 180.0 {
                            final_az = 180.0 - final_az;
                        } else if (-180.0..-90.0).contains(&final_az) {
                            final_az = -180.0 - final_az;
                        }
                    }
                    vbap.calculate_gains(final_az, current_el, 1.0, &mut vbap_gains);
                    dbap.calculate_gains(coord.azimuth, current_el, dbap_dist, &mut dbap_gains);
                }
            }

            let mut norm_2d_sq = 0.0f32;
            for j in 0..self.num_wo_lfe {
                gains_2d[j] = alpha * vbap_gains[j] + (1.0 - alpha) * dbap_gains[j];
                norm_2d_sq += gains_2d[j] * gains_2d[j];
            }
            let norm_2d = norm_2d_sq.sqrt();
            for g in &mut gains_2d {
                *g /= norm_2d + 1e-10;
            }

            for (j, &spk_idx) in layer.speaker_indices_wo_lfe.iter().enumerate() {
                if spk_idx < gains_wo_lfe.len() && j < gains_2d.len() {
                    gains_wo_lfe[spk_idx] += gains_2d[j] * ele_val;
                }
            }
        }

        let norm_wo_lfe: f32 = gains_wo_lfe.iter().map(|g| g * g).sum::<f32>().sqrt();
        if norm_wo_lfe > 0.0 {
            let mut j = 0usize;
            for (i, sp) in self.speakers.iter().enumerate() {
                if !sp.is_lfe {
                    if i < out_gains.len() && j < gains_wo_lfe.len() {
                        out_gains[i] = gains_wo_lfe[j] / norm_wo_lfe;
                    }
                    j += 1;
                }
            }
        }
    }
}

/// Stateful per-element object loudspeaker renderer (1 or 2 objects -> target [`SoundSystem`]).
#[derive(Debug, Clone)]
pub(crate) struct ObjectLoudspeakerRenderer {
    calc: CustomGainCalculator,
    num_speakers: usize,
    prev_gains: Vec<Option<Vec<f32>>>,
    curr_gains: Vec<f32>,
}

impl ObjectLoudspeakerRenderer {
    /// Creates an [`ObjectLoudspeakerRenderer`] for `target` and `num_objects` (`1` or `2`).
    pub(crate) fn new(target: SoundSystem, num_objects: usize) -> Result<Self, DecodeError> {
        let calc = CustomGainCalculator::new(target).ok_or_else(|| {
            DecodeError::InvalidDescriptors(
                "unsupported target layout for object loudspeaker renderer".into(),
            )
        })?;
        let num_speakers = calc.speakers.len();
        Ok(Self {
            calc,
            num_speakers,
            prev_gains: vec![None; num_objects],
            curr_gains: vec![0.0; num_speakers],
        })
    }

    /// Renders one temporal unit of `objects` (`[num_objects][unit_len]`) at
    /// `positions` (`[num_objects]`) and returns `[num_speakers][unit_len]`.
    pub(crate) fn process_frame(
        &mut self,
        objects: &[Vec<f32>],
        positions: &[PolarCoord],
    ) -> Vec<Vec<f32>> {
        let unit_len = objects.first().map_or(0, Vec::len);
        let mut output = vec![vec![0.0; unit_len]; self.num_speakers];
        self.process(objects, positions, unit_len, &mut output);
        output
    }

    /// Renders `objects` (`[num_objects][unit_len]`) at `positions` (`[num_objects]`)
    /// into `output` (`[num_speakers][unit_len]`, cleared before accumulation).
    pub(crate) fn process(
        &mut self,
        objects: &[Vec<f32>],
        positions: &[PolarCoord],
        unit_len: usize,
        output: &mut [Vec<f32>],
    ) {
        for ch in output.iter_mut().take(self.num_speakers) {
            ch[..unit_len].fill(0.0);
        }
        if unit_len == 0 {
            return;
        }

        let num_objs = objects.len().min(self.prev_gains.len());
        for (obj_idx, obj_plane) in objects.iter().enumerate().take(num_objs) {
            let pos = positions.get(obj_idx).copied().unwrap_or_default();
            self.calc.calculate_gains(pos, &mut self.curr_gains);

            let in_ch = &obj_plane[..unit_len];
            if let Some(ref mut prev) = self.prev_gains[obj_idx] {
                let inv_len = 1.0 / (unit_len as f32);
                for (spk, out_plane) in output.iter_mut().enumerate().take(self.num_speakers) {
                    let g0 = prev[spk];
                    let g1 = self.curr_gains[spk];
                    let out_ch = &mut out_plane[..unit_len];
                    if (g1 - g0).abs() < 1e-9 {
                        if g1 != 0.0 {
                            for (dst, &src) in out_ch.iter_mut().zip(in_ch.iter()) {
                                *dst += src * g1;
                            }
                        }
                    } else {
                        for (i, (dst, &src)) in out_ch.iter_mut().zip(in_ch.iter()).enumerate() {
                            let w = (i as f32) * inv_len;
                            let g = (1.0 - w) * g0 + w * g1;
                            *dst += src * g;
                        }
                    }
                }
                prev.copy_from_slice(&self.curr_gains);
            } else {
                for (spk, out_plane) in output.iter_mut().enumerate().take(self.num_speakers) {
                    let g = self.curr_gains[spk];
                    if g != 0.0 {
                        let out_ch = &mut out_plane[..unit_len];
                        for (dst, &src) in out_ch.iter_mut().zip(in_ch.iter()) {
                            *dst += src * g;
                        }
                    }
                }
                self.prev_gains[obj_idx] = Some(self.curr_gains.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the position cursor added scaled durations with `+=`
    /// (a panic with overflow checks, e.g. u32::MAX-sample subblocks at
    /// parameter_rate 1) and truncated segment durations to u32.
    #[test]
    fn position_cursor_huge_durations_saturate() {
        let mut bytes = vec![7, 1, 0x80];
        bytes.extend([0; 12]);
        let def = PositionParamDefinition::parse(
            &mut iamf_obu::ByteReader::new(&bytes),
            PositionParamType::Polar,
        )
        .unwrap();
        let mut cursor = PositionCursor::from_definition(1, &def).unwrap();
        // Step animation (type 0) of one zeroed polar position.
        let data = PositionData::parse(
            &mut iamf_obu::ByteReader::new(&[0, 0, 0, 0]),
            PositionParamType::Polar,
        )
        .unwrap();
        cursor.push(&data, usize::MAX).unwrap();
        cursor.push(&data, usize::MAX).unwrap();
        assert_eq!(cursor.take_for_unit(960).len(), 1);
        cursor.clear();
        cursor.advance_to(usize::MAX);
        cursor.take_for_unit(usize::MAX);
        cursor.take_for_unit(960);
    }

    #[test]
    fn cart_to_polar_front_left_top() {
        let front = cart_to_polar_sector(CartCoord {
            x: 0.0,
            y: 1.0,
            z: 0.0,
        });
        assert!((front.azimuth - 0.0).abs() < 1e-4);
        assert!((front.elevation - 0.0).abs() < 1e-4);
        assert!((front.distance - 1.0).abs() < 1e-4);

        let left_front = cart_to_polar_sector(CartCoord {
            x: -1.0,
            y: 1.0,
            z: 0.0,
        });
        assert!((left_front.azimuth - 30.0).abs() < 1e-3);
        assert!((left_front.elevation - 0.0).abs() < 1e-4);

        let left_mid = cart_to_polar_sector(CartCoord {
            x: -1.0,
            y: 0.0,
            z: 0.0,
        });
        assert!((left_mid.azimuth - 70.0).abs() < 1e-3);
        assert!((left_mid.elevation - 0.0).abs() < 1e-4);

        let top = cart_to_polar_sector(CartCoord {
            x: 0.0,
            y: 0.0,
            z: 1.0,
        });
        assert!((top.elevation - 90.0).abs() < 1e-4);
    }

    #[test]
    fn olr_stereo_front_pans_equally() {
        let mut olr = ObjectLoudspeakerRenderer::new(SoundSystem::A, 1).unwrap();
        let input = vec![vec![1.0f32; 4]];
        let mut output = vec![vec![0.0f32; 4]; 2];
        olr.process(&input, &[PolarCoord::default()], 4, &mut output);
        let expected = std::f32::consts::FRAC_1_SQRT_2;
        assert!((output[0][0] - expected).abs() < 1e-5);
        assert!((output[1][0] - expected).abs() < 1e-5);
    }
}
