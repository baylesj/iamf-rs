//! IAMF v2.0 object positions: position parameter definitions (declared in
//! a sub-mix element's `rendering_config`) and the animated position data
//! their parameter blocks carry (`param_definition_type` 3..=8).
//!
//! Field widths and value clipping follow iamf-tools
//! (`iamf/obu/polar_position_data.cc`, `cartesian_position_data.cc` and
//! `param_definitions/*_param_definition.cc`): polar positions are a
//! signed 9-bit azimuth clipped to [-180, 180] degrees, a signed 8-bit
//! elevation clipped to [-90, 90] degrees and an unsigned 7-bit distance;
//! cartesian positions are signed 8- or 16-bit x/y/z, unclipped.

use crate::bits::BitReader;
use crate::descriptors::ParamDefinition;
use crate::{ByteReader, Error, Result};

/// The six position `param_definition_type`s (IAMF v2.0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PositionParamType {
    /// Type 3: one polar position.
    Polar,
    /// Type 4: one cartesian position, 8-bit coordinates.
    Cartesian8,
    /// Type 5: one cartesian position, 16-bit coordinates.
    Cartesian16,
    /// Type 6: two polar positions (one per object of a two-object element).
    DualPolar,
    /// Type 7: two cartesian positions, 8-bit coordinates.
    DualCartesian8,
    /// Type 8: two cartesian positions, 16-bit coordinates.
    DualCartesian16,
}

impl PositionParamType {
    /// Maps a `param_definition_type` to a position type (`None` for the
    /// non-position types 0..=2 and reserved types 9+).
    pub fn from_param_definition_type(param_definition_type: u32) -> Option<Self> {
        Some(match param_definition_type {
            3 => PositionParamType::Polar,
            4 => PositionParamType::Cartesian8,
            5 => PositionParamType::Cartesian16,
            6 => PositionParamType::DualPolar,
            7 => PositionParamType::DualCartesian8,
            8 => PositionParamType::DualCartesian16,
            _ => return None,
        })
    }

    /// The `param_definition_type` value.
    pub fn param_definition_type(self) -> u32 {
        match self {
            PositionParamType::Polar => 3,
            PositionParamType::Cartesian8 => 4,
            PositionParamType::Cartesian16 => 5,
            PositionParamType::DualPolar => 6,
            PositionParamType::DualCartesian8 => 7,
            PositionParamType::DualCartesian16 => 8,
        }
    }

    /// Positions carried per value: 1, or 2 for the dual types.
    pub fn num_positions(self) -> usize {
        match self {
            PositionParamType::Polar
            | PositionParamType::Cartesian8
            | PositionParamType::Cartesian16 => 1,
            PositionParamType::DualPolar
            | PositionParamType::DualCartesian8
            | PositionParamType::DualCartesian16 => 2,
        }
    }

    /// Whether positions are polar (else cartesian).
    pub fn is_polar(self) -> bool {
        matches!(
            self,
            PositionParamType::Polar | PositionParamType::DualPolar
        )
    }

    /// Coordinate width in bits for cartesian types (8 or 16); `None` for
    /// polar types.
    pub fn cartesian_bits(self) -> Option<u32> {
        match self {
            PositionParamType::Cartesian8 | PositionParamType::DualCartesian8 => Some(8),
            PositionParamType::Cartesian16 | PositionParamType::DualCartesian16 => Some(16),
            PositionParamType::Polar | PositionParamType::DualPolar => None,
        }
    }
}

/// A polar position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PolarPosition {
    /// Degrees in [-180, 180]; positive is to the listener's left.
    pub azimuth: i16,
    /// Degrees in [-90, 90]; positive is up.
    pub elevation: i8,
    /// Distance code, 0..=127.
    pub distance: u8,
}

impl PolarPosition {
    /// Builds a position, clipping azimuth/elevation like the parser does
    /// and masking distance to 7 bits.
    pub fn new(azimuth: i16, elevation: i8, distance: u8) -> Self {
        Self {
            azimuth: clip_azimuth(azimuth),
            elevation: clip_elevation(elevation),
            distance: distance & 0x7f,
        }
    }
}

/// A cartesian position; the value range is the full signed range of the
/// coded width (8 or 16 bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CartesianPosition {
    /// Coded x coordinate.
    pub x: i16,
    /// Coded y coordinate.
    pub y: i16,
    /// Coded z coordinate.
    pub z: i16,
}

impl CartesianPosition {
    /// Builds a position from coded coordinates.
    pub fn new(x: i16, y: i16, z: i16) -> Self {
        Self { x, y, z }
    }
}

/// A static position (a parameter definition's default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Position {
    /// Polar coordinates.
    Polar(PolarPosition),
    /// Cartesian coordinates.
    Cartesian(CartesianPosition),
}

/// IAMF v2.0 animation types, coded as uleb128 (mix gains use the same
/// values 0..=2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AnimationType {
    /// 0: constant over the subblock.
    Step,
    /// 1: linear from start to end.
    Linear,
    /// 2: quadratic bezier through a control point.
    Bezier,
    /// 3: linear from the previous subblock's end value to end.
    InterLinear,
    /// 4: bezier from the previous subblock's end value.
    InterBezier,
}

impl AnimationType {
    /// Maps the coded value (`None` for reserved values 5+).
    pub fn from_u32(value: u32) -> Option<Self> {
        Some(match value {
            0 => AnimationType::Step,
            1 => AnimationType::Linear,
            2 => AnimationType::Bezier,
            3 => AnimationType::InterLinear,
            4 => AnimationType::InterBezier,
            _ => return None,
        })
    }
}

/// One animated scalar over a subblock. The `Inter*` variants start from
/// the value the previous subblock ended on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Animated<T> {
    /// Constant value.
    Step {
        /// The value.
        start: T,
    },
    /// Linear ramp.
    Linear {
        /// Value at the subblock start.
        start: T,
        /// Value at the subblock end.
        end: T,
    },
    /// Quadratic bezier.
    Bezier {
        /// Value at the subblock start.
        start: T,
        /// Value at the subblock end.
        end: T,
        /// Control point value.
        control: T,
        /// Control point time, 0..=255 over the subblock.
        control_relative_time: u8,
    },
    /// Linear ramp from the previous value.
    InterLinear {
        /// Value at the subblock end.
        end: T,
    },
    /// Quadratic bezier from the previous value.
    InterBezier {
        /// Value at the subblock end.
        end: T,
        /// Control point value.
        control: T,
        /// Control point time, 0..=255 over the subblock.
        control_relative_time: u8,
    },
}

impl<T: Copy> Animated<T> {
    fn read(
        ty: AnimationType,
        bits: &mut BitReader<'_, '_>,
        mut value: impl FnMut(&mut BitReader<'_, '_>) -> Result<T>,
    ) -> Result<Self> {
        Ok(match ty {
            AnimationType::Step => Animated::Step {
                start: value(bits)?,
            },
            AnimationType::Linear => Animated::Linear {
                start: value(bits)?,
                end: value(bits)?,
            },
            AnimationType::Bezier => Animated::Bezier {
                start: value(bits)?,
                end: value(bits)?,
                control: value(bits)?,
                control_relative_time: bits.read_u8()?,
            },
            AnimationType::InterLinear => Animated::InterLinear { end: value(bits)? },
            AnimationType::InterBezier => Animated::InterBezier {
                end: value(bits)?,
                control: value(bits)?,
                control_relative_time: bits.read_u8()?,
            },
        })
    }

    /// The animation type of this value.
    pub fn animation_type(&self) -> AnimationType {
        match self {
            Animated::Step { .. } => AnimationType::Step,
            Animated::Linear { .. } => AnimationType::Linear,
            Animated::Bezier { .. } => AnimationType::Bezier,
            Animated::InterLinear { .. } => AnimationType::InterLinear,
            Animated::InterBezier { .. } => AnimationType::InterBezier,
        }
    }

    /// The value at the end of the subblock.
    pub fn end_value(&self) -> T {
        match *self {
            Animated::Step { start } => start,
            Animated::Linear { end, .. }
            | Animated::Bezier { end, .. }
            | Animated::InterLinear { end }
            | Animated::InterBezier { end, .. } => end,
        }
    }
}

/// An animated polar position (all three components share one animation
/// type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AnimatedPolar {
    /// Azimuth, degrees (clipped to [-180, 180]).
    pub azimuth: Animated<i16>,
    /// Elevation, degrees (clipped to [-90, 90]).
    pub elevation: Animated<i8>,
    /// Distance code, 0..=127.
    pub distance: Animated<u8>,
}

/// An animated cartesian position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AnimatedCartesian {
    /// x coordinate.
    pub x: Animated<i16>,
    /// y coordinate.
    pub y: Animated<i16>,
    /// z coordinate.
    pub z: Animated<i16>,
}

/// One animated position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AnimatedPosition {
    /// Polar coordinates.
    Polar(AnimatedPolar),
    /// Cartesian coordinates.
    Cartesian(AnimatedCartesian),
}

/// The payload of one position parameter-block subblock: one position per
/// object (two for the dual types), sharing one animation type.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PositionData {
    /// The shared animation type.
    pub animation_type: AnimationType,
    /// One entry per position (1 or 2).
    pub positions: Vec<AnimatedPosition>,
}

fn clip_azimuth(value: i16) -> i16 {
    value.clamp(-180, 180)
}

fn clip_elevation(value: i8) -> i8 {
    value.clamp(-90, 90)
}

fn read_azimuth(bits: &mut BitReader<'_, '_>) -> Result<i16> {
    Ok(clip_azimuth(bits.read_signed(9)? as i16))
}

fn read_elevation(bits: &mut BitReader<'_, '_>) -> Result<i8> {
    Ok(clip_elevation(bits.read_signed(8)? as i8))
}

fn read_distance(bits: &mut BitReader<'_, '_>) -> Result<u8> {
    Ok(bits.read_bits(7)? as u8)
}

fn read_coordinate(bits: &mut BitReader<'_, '_>, width: u32) -> Result<i16> {
    Ok(bits.read_signed(width)? as i16)
}

impl PositionData {
    /// Parses one subblock's position data for a parameter of type `ty`:
    /// a uleb128 animation type, then the positions.
    pub fn parse(r: &mut ByteReader<'_>, ty: PositionParamType) -> Result<Self> {
        let animation_type =
            AnimationType::from_u32(r.read_leb128()?).ok_or(Error::InvalidDescriptor {
                offset: r.position(),
            })?;
        let mut bits = BitReader::new(r);
        let mut positions = Vec::with_capacity(ty.num_positions());
        for _ in 0..ty.num_positions() {
            positions.push(match ty.cartesian_bits() {
                None => AnimatedPosition::Polar(AnimatedPolar {
                    azimuth: Animated::read(animation_type, &mut bits, read_azimuth)?,
                    elevation: Animated::read(animation_type, &mut bits, read_elevation)?,
                    distance: Animated::read(animation_type, &mut bits, read_distance)?,
                }),
                Some(width) => {
                    let mut coordinate = |b: &mut BitReader<'_, '_>| read_coordinate(b, width);
                    AnimatedPosition::Cartesian(AnimatedCartesian {
                        x: Animated::read(animation_type, &mut bits, &mut coordinate)?,
                        y: Animated::read(animation_type, &mut bits, &mut coordinate)?,
                        z: Animated::read(animation_type, &mut bits, &mut coordinate)?,
                    })
                }
            });
        }
        bits.finish()?;
        Ok(PositionData {
            animation_type,
            positions,
        })
    }
}

/// A position parameter definition (from a sub-mix element's
/// `rendering_config`): the common parameter definition plus the default
/// position(s) used until a parameter block arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PositionParamDefinition {
    /// Which position type (and so which parameter-block payload).
    pub param_type: PositionParamType,
    /// Common definition fields (id, rate, timing mode).
    pub base: ParamDefinition,
    /// Default position(s): one, or two for the dual types.
    pub defaults: Vec<Position>,
}

impl PositionParamDefinition {
    /// Parses the definition body that follows `param_definition_type`.
    pub fn parse(r: &mut ByteReader<'_>, param_type: PositionParamType) -> Result<Self> {
        let base = ParamDefinition::parse(r)?;
        let mut bits = BitReader::new(r);
        let mut defaults = Vec::with_capacity(param_type.num_positions());
        for _ in 0..param_type.num_positions() {
            defaults.push(match param_type.cartesian_bits() {
                None => Position::Polar(PolarPosition {
                    azimuth: read_azimuth(&mut bits)?,
                    elevation: read_elevation(&mut bits)?,
                    distance: read_distance(&mut bits)?,
                }),
                Some(width) => Position::Cartesian(CartesianPosition {
                    x: read_coordinate(&mut bits, width)?,
                    y: read_coordinate(&mut bits, width)?,
                    z: read_coordinate(&mut bits, width)?,
                }),
            });
        }
        bits.finish()?;
        Ok(PositionParamDefinition {
            param_type,
            base,
            defaults,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MSB-first bit writer for building payloads from the syntax tables.
    #[derive(Default)]
    struct Bits {
        bytes: Vec<u8>,
        used: u32,
    }

    impl Bits {
        fn put(&mut self, value: i64, width: u32) -> &mut Self {
            for i in (0..width).rev() {
                if self.used % 8 == 0 {
                    self.bytes.push(0);
                }
                let bit = ((value >> i) & 1) as u8;
                *self.bytes.last_mut().unwrap() |= bit << (7 - self.used % 8);
                self.used += 1;
            }
            self
        }
        fn done(&self) -> Vec<u8> {
            assert_eq!(self.used % 8, 0, "payload must be byte-aligned");
            self.bytes.clone()
        }
    }

    fn parse_data(payload: &[u8], ty: PositionParamType) -> Result<PositionData> {
        let mut r = ByteReader::new(payload);
        let data = PositionData::parse(&mut r, ty)?;
        assert!(r.is_empty(), "trailing bytes");
        Ok(data)
    }

    #[test]
    fn type_mapping_round_trips() {
        for t in 3..=8 {
            let ty = PositionParamType::from_param_definition_type(t).unwrap();
            assert_eq!(ty.param_definition_type(), t);
        }
        for t in [0, 1, 2, 9, 1000] {
            assert_eq!(PositionParamType::from_param_definition_type(t), None);
        }
        assert_eq!(PositionParamType::DualCartesian16.num_positions(), 2);
        assert_eq!(PositionParamType::Cartesian8.cartesian_bits(), Some(8));
        assert!(PositionParamType::DualPolar.is_polar());
    }

    #[test]
    fn polar_step() {
        let mut b = Bits::default();
        b.put(0, 8); // animation_type = step (uleb128)
        b.put(90, 9).put(-30, 8).put(100, 7);
        let data = parse_data(&b.done(), PositionParamType::Polar).unwrap();
        assert_eq!(data.animation_type, AnimationType::Step);
        assert_eq!(
            data.positions,
            vec![AnimatedPosition::Polar(AnimatedPolar {
                azimuth: Animated::Step { start: 90 },
                elevation: Animated::Step { start: -30 },
                distance: Animated::Step { start: 100 },
            })]
        );
    }

    #[test]
    fn polar_values_are_clipped() {
        // 9-bit azimuth 255 and -256, 8-bit elevation 127 / -128: out of
        // range, clipped per iamf-tools.
        let mut b = Bits::default();
        b.put(1, 8); // linear
        b.put(255, 9).put(-256, 9); // azimuth start, end
        b.put(127, 8).put(-128, 8); // elevation start, end
        b.put(0, 7).put(127, 7); // distance start, end
        let data = parse_data(&b.done(), PositionParamType::Polar).unwrap();
        let AnimatedPosition::Polar(p) = data.positions[0] else {
            panic!("polar expected");
        };
        assert_eq!(
            p.azimuth,
            Animated::Linear {
                start: 180,
                end: -180
            }
        );
        assert_eq!(
            p.elevation,
            Animated::Linear {
                start: 90,
                end: -90
            }
        );
        assert_eq!(p.distance, Animated::Linear { start: 0, end: 127 });
    }

    #[test]
    fn dual_polar_bezier() {
        let mut b = Bits::default();
        b.put(2, 8); // bezier
        for (az, el, d) in [(-90i64, 10i64, 5i64), (45, -45, 64)] {
            b.put(az, 9).put(az + 1, 9).put(az + 2, 9).put(200, 8);
            b.put(el, 8).put(el + 1, 8).put(el + 2, 8).put(100, 8);
            b.put(d, 7).put(d + 1, 7).put(d + 2, 7).put(50, 8);
        }
        let data = parse_data(&b.done(), PositionParamType::DualPolar).unwrap();
        assert_eq!(data.positions.len(), 2);
        let AnimatedPosition::Polar(second) = data.positions[1] else {
            panic!("polar expected");
        };
        assert_eq!(
            second.azimuth,
            Animated::Bezier {
                start: 45,
                end: 46,
                control: 47,
                control_relative_time: 200
            }
        );
        assert_eq!(second.distance.end_value(), 65);
    }

    #[test]
    fn cartesian8_inter_linear_and_16_inter_bezier() {
        let mut b = Bits::default();
        b.put(3, 8); // inter-linear
        b.put(-128, 8).put(0, 8).put(127, 8);
        let data = parse_data(&b.done(), PositionParamType::Cartesian8).unwrap();
        assert_eq!(
            data.positions,
            vec![AnimatedPosition::Cartesian(AnimatedCartesian {
                x: Animated::InterLinear { end: -128 },
                y: Animated::InterLinear { end: 0 },
                z: Animated::InterLinear { end: 127 },
            })]
        );

        let mut b = Bits::default();
        b.put(4, 8); // inter-bezier
        for _ in 0..2 {
            b.put(-32768, 16).put(32767, 16).put(7, 8); // x
            b.put(1, 16).put(2, 16).put(8, 8); // y
            b.put(-1, 16).put(-2, 16).put(9, 8); // z
        }
        let data = parse_data(&b.done(), PositionParamType::DualCartesian16).unwrap();
        let AnimatedPosition::Cartesian(c) = data.positions[1] else {
            panic!("cartesian expected");
        };
        assert_eq!(
            c.x,
            Animated::InterBezier {
                end: -32768,
                control: 32767,
                control_relative_time: 7
            }
        );
        assert_eq!(c.z.animation_type(), AnimationType::InterBezier);
    }

    #[test]
    fn reserved_animation_type_and_truncation_rejected() {
        assert!(parse_data(&[5, 0, 0, 0], PositionParamType::Polar).is_err());
        // Step needs 3 bytes after the type.
        assert!(matches!(
            parse_data(&[0, 0, 0], PositionParamType::Polar),
            Err(Error::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn definitions_with_defaults() {
        // parameter_id 7, rate 48000, mode 1 (block timing), then defaults.
        let head = [0x07, 0x80, 0xf7, 0x02, 0x80];
        let mut b = Bits::default();
        b.put(-200, 9).put(45, 8).put(3, 7); // azimuth clipped to -180
        let mut payload = head.to_vec();
        payload.extend(b.done());
        let def = PositionParamDefinition::parse(
            &mut ByteReader::new(&payload),
            PositionParamType::Polar,
        )
        .unwrap();
        assert_eq!(def.base.parameter_id, 7);
        assert_eq!(def.base.parameter_rate, 48000);
        assert!(def.base.mode);
        assert_eq!(
            def.defaults,
            vec![Position::Polar(PolarPosition::new(-180, 45, 3))]
        );

        let mut payload = head.to_vec();
        payload.extend([1, 2, 3, 0xff, 0xfe, 0xfd]);
        let def = PositionParamDefinition::parse(
            &mut ByteReader::new(&payload),
            PositionParamType::DualCartesian8,
        )
        .unwrap();
        assert_eq!(
            def.defaults,
            vec![
                Position::Cartesian(CartesianPosition::new(1, 2, 3)),
                Position::Cartesian(CartesianPosition::new(-1, -2, -3)),
            ]
        );

        let mut payload = head.to_vec();
        payload.extend([0x12, 0x34, 0x80, 0x00, 0x7f, 0xff]);
        let def = PositionParamDefinition::parse(
            &mut ByteReader::new(&payload),
            PositionParamType::Cartesian16,
        )
        .unwrap();
        assert_eq!(
            def.defaults,
            vec![Position::Cartesian(CartesianPosition::new(
                0x1234,
                i16::MIN,
                i16::MAX
            ))]
        );
    }
}
