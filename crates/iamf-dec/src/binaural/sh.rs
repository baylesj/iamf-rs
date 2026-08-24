//! Spherical harmonics encoding (ACN ordering, SN3D normalization),
//! ported from obr's `AmbisonicEncoder` and
//! `AssociatedLegendrePolynomialsGenerator` (without Condon–Shortley
//! phase, positive orders only).

use super::speakers::Source;

fn factorial(n: i32) -> f32 {
    (1..=n).map(|v| v as f32).product()
}

fn double_factorial(n: i32) -> f32 {
    let mut result = 1.0f32;
    let mut v = n;
    while v > 1 {
        result *= v as f32;
        v -= 2;
    }
    result
}

/// ACN index for (degree, order).
fn acn(degree: i32, order: i32) -> usize {
    (degree * degree + degree + order) as usize
}

/// SN3D (Schmidt semi-normalized) normalization factor.
fn sn3d(degree: i32, order: i32) -> f32 {
    let m = order.abs();
    ((if order == 0 { 1.0f32 } else { 2.0 }) * factorial(degree - m) / factorial(degree + m)).sqrt()
}

/// Associated Legendre polynomials P_l^m(x) for l ≤ max_degree, m ≥ 0,
/// with the Condon–Shortley phase removed (obr passes
/// `condon_shortley_phase = false`).
fn alp(max_degree: i32, x: f32) -> Vec<f32> {
    let idx = |degree: i32, order: i32| -> usize { ((degree * (degree + 1)) / 2 + order) as usize };
    let count = ((max_degree + 1) * (max_degree + 2) / 2) as usize;
    let mut values = vec![0.0f32; count];

    values[idx(0, 0)] = 1.0;
    if max_degree >= 1 {
        values[idx(1, 0)] = x;
    }
    for degree in 2..=max_degree {
        // (degree, 0) from (degree-1, 0) and (degree-2, 0).
        values[idx(degree, 0)] = ((2 * degree - 1) as f32 * x * values[idx(degree - 1, 0)]
            - (degree - 1) as f32 * values[idx(degree - 2, 0)])
            / degree as f32;
    }
    for degree in 1..=max_degree {
        // (degree, degree).
        values[idx(degree, degree)] = (-1.0f32).powi(degree)
            * double_factorial(2 * degree - 1)
            * (1.0 - x * x).powf(0.5 * degree as f32);
    }
    for degree in 2..=max_degree {
        // (degree, degree - 1).
        values[idx(degree, degree - 1)] =
            x * (2 * degree - 1) as f32 * values[idx(degree - 1, degree - 1)];
    }
    for degree in 3..=max_degree {
        for order in 1..=degree - 2 {
            values[idx(degree, order)] =
                ((2 * degree - 1) as f32 * x * values[idx(degree - 1, order)]
                    - (degree - 1 + order) as f32 * values[idx(degree - 2, order)])
                    / (degree - order) as f32;
        }
    }
    // Undo the Condon–Shortley phase.
    for degree in 1..=max_degree {
        for order in 0..=degree {
            values[idx(degree, order)] *= (-1.0f32).powi(order);
        }
    }
    values
}

/// SN3D/ACN spherical harmonic coefficients for a direction.
pub fn sh_coeffs(azimuth_deg: f32, elevation_deg: f32, order: usize) -> Vec<f32> {
    let azimuth = azimuth_deg.to_radians();
    let elevation = elevation_deg.to_radians();
    let max_degree = order as i32;
    let polynomials = alp(max_degree, elevation.sin());
    let alp_index = |degree: i32, m: i32| -> usize { ((degree * (degree + 1)) / 2 + m) as usize };

    let mut coeffs = vec![0.0f32; (order + 1) * (order + 1)];
    for degree in 0..=max_degree {
        for m in -degree..=degree {
            let last_term = if m >= 0 {
                (m as f32 * azimuth).cos()
            } else {
                ((-m) as f32 * azimuth).sin()
            };
            coeffs[acn(degree, m)] =
                sn3d(degree, m) * polynomials[alp_index(degree, m.abs())] * last_term;
        }
    }
    coeffs
}

/// obr mutes sources whose overall gain falls below -120 dB.
const NEGATIVE_120_DB: f32 = 1e-6;

/// Builds the (order+1)² × sources encoding matrix
/// (obr `AmbisonicEncoder::SetSource` with gain 1.0 per source).
pub fn encoding_matrix(sources: &[Source], order: usize) -> Vec<Vec<f32>> {
    let rows = (order + 1) * (order + 1);
    let mut matrix = vec![vec![0.0f32; sources.len()]; rows];
    for (col, source) in sources.iter().enumerate() {
        let overall_gain = 1.0f32 / source.distance.max(0.5);
        if overall_gain < NEGATIVE_120_DB {
            continue;
        }
        let coeffs = sh_coeffs(source.azimuth, source.elevation, order);
        for (row, &c) in coeffs.iter().enumerate() {
            matrix[row][col] = c * overall_gain;
        }
    }
    matrix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeroth_order_is_unity() {
        let coeffs = sh_coeffs(30.0, 45.0, 3);
        assert!((coeffs[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn first_order_front_source() {
        // Source straight ahead (az 0, el 0): Y = sin(az) = 0, Z = sin(el)
        // = 0, X = cos(el)cos(az) = 1 (SN3D FOA).
        let coeffs = sh_coeffs(0.0, 0.0, 1);
        assert!((coeffs[1]).abs() < 1e-6, "Y = {}", coeffs[1]);
        assert!((coeffs[2]).abs() < 1e-6, "Z = {}", coeffs[2]);
        assert!((coeffs[3] - 1.0).abs() < 1e-6, "X = {}", coeffs[3]);
    }

    #[test]
    fn left_source_positive_y() {
        // az +90° (left): Y = 1, X = 0.
        let coeffs = sh_coeffs(90.0, 0.0, 1);
        assert!((coeffs[1] - 1.0).abs() < 1e-6, "Y = {}", coeffs[1]);
        assert!(coeffs[3].abs() < 1e-6, "X = {}", coeffs[3]);
    }
}
