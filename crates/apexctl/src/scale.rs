//! Display scales as mutter 51 sees them, and the number formatting of `monitors.xml`.
//!
//! mutter keeps scales as C `float`s and accepts a configured scale only if it is one of
//! the scales it offers for the mode (`meta_monitor_calculate_supported_scales`,
//! `src/backends/meta-monitor.c`). A `monitors.xml` scale is read with `g_ascii_strtod`
//! and cast to `float`, and mutter writes numbers with `g_ascii_dtostr` (`%.17g`). This
//! module ports the offered-scale rule (so the renderer can refuse a scale mutter would
//! silently ignore) and formats numbers exactly like `%.17g`.

use std::fmt;

use serde::Deserialize;

/// Largest scale denominator mutter offers.
pub const MAX_DENOMINATOR: u32 = 4;
/// Smallest integer scale.
pub const MIN_INTEGER_SCALE: u32 = 1;
/// Largest integer scale.
pub const MAX_INTEGER_SCALE: u32 = 4;
/// Smallest logical area mutter 51 accepts (600 × 600, mutter commit 8ade30eb2a42).
pub const MIN_LOGICAL_AREA: u64 = 600 * 600;

/// A scale as an exact fraction, as the hardware profile writes it (`{ num = 5, den = 3 }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scale {
    /// Numerator.
    pub num: u32,
    /// Denominator.
    pub den: u32,
}

impl Scale {
    /// The `float` mutter uses: `(float) num / den`.
    pub fn as_f32(self) -> f32 {
        self.num as f32 / self.den as f32
    }

    /// `true` when `num/den` is in lowest terms (mutter offers only reduced fractions).
    pub fn is_reduced(self) -> bool {
        gcd(self.num, self.den) == 1
    }

    /// The text for `<scale>` in `monitors.xml`: `2` for integers, otherwise the `float`
    /// value printed with `%.17g`, which `g_ascii_strtod` followed by a cast to `float`
    /// turns back into exactly the same `float`.
    pub fn xml_text(self) -> String {
        format_g17(f64::from(self.as_f32()))
    }
}

impl fmt::Display for Scale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.den == 1 {
            write!(f, "{}", self.num)
        } else {
            write!(f, "{}/{}", self.num, self.den)
        }
    }
}

const fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// `is_scale_valid_for_size`: the logical size at `scale` must be large enough.
fn is_valid_for_size(width: u32, height: u32, scale: f32) -> bool {
    if scale < MIN_INTEGER_SCALE as f32 || scale > MAX_INTEGER_SCALE as f32 {
        return false;
    }
    // C: (int) floorf (width / scale), in float arithmetic.
    let logical_width = (width as f32 / scale).floor() as u64;
    let logical_height = (height as f32 / scale).floor() as u64;
    logical_width * logical_height >= MIN_LOGICAL_AREA
}

/// The scales mutter 51 offers for a `width` × `height` mode with fractional scaling
/// (the default since mutter 50), sorted by their `float` value.
///
/// A scale `num/den` is offered when it divides both dimensions exactly
/// (`width·den mod num = 0`), is in lowest terms, lies in `[1, 4]` and leaves a logical
/// area of at least [`MIN_LOGICAL_AREA`]. With no candidate, mutter falls back to 1.
pub fn supported_scales(width: u32, height: u32) -> Vec<Scale> {
    let mut scales = Vec::new();
    for den in 1..=MAX_DENOMINATOR {
        for num in (MIN_INTEGER_SCALE * den)..=(MAX_INTEGER_SCALE * den) {
            let scale = Scale { num, den };
            if (u64::from(width) * u64::from(den)) % u64::from(num) != 0
                || (u64::from(height) * u64::from(den)) % u64::from(num) != 0
                || !scale.is_reduced()
                || !is_valid_for_size(width, height, scale.as_f32())
            {
                continue;
            }
            scales.push(scale);
        }
    }
    scales.sort_by(|a, b| a.as_f32().total_cmp(&b.as_f32()));
    if scales.is_empty() {
        scales.push(Scale { num: 1, den: 1 });
    }
    scales
}

/// Formats `value` like C's `printf("%.17g")` (and so like `g_ascii_dtostr` of `GLib`).
///
/// 17 significant digits always identify a `double`, so the text parses back to exactly
/// `value`. Rust's precision formatting rounds the exact binary value correctly, like
/// glibc, so the digits match C's.
pub fn format_g17(value: f64) -> String {
    const PRECISION: i32 = 17;
    if value == 0.0 || !value.is_finite() {
        // %g prints "0" (or "-0"), "inf" and "nan"; none of them is used by the renderer.
        return if value == 0.0 && value.is_sign_negative() {
            "-0".to_owned()
        } else if value == 0.0 {
            "0".to_owned()
        } else {
            value.to_string()
        };
    }
    // %g takes the exponent X of the %e conversion at precision P - 1.
    let scientific = format!("{:.*e}", (PRECISION - 1) as usize, value);
    let (mantissa, exponent) = scientific
        .split_once('e')
        .unwrap_or((scientific.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if (-4..PRECISION).contains(&exponent) {
        let decimals = (PRECISION - 1 - exponent) as usize;
        trim_fraction(&format!("{value:.decimals$}")).to_owned()
    } else {
        let mantissa = trim_fraction(mantissa);
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exponent.abs())
    }
}

/// Removes trailing zeros of a fraction, and the point if nothing is left after it.
fn trim_fraction(text: &str) -> &str {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn s(num: u32, den: u32) -> Scale {
        Scale { num, den }
    }

    #[test]
    fn scales_offered_for_the_panel_match_mutter_51() {
        // Test vectors from the display research (mutter 51.0 meta-monitor.c port).
        assert_eq!(
            supported_scales(2880, 1800),
            [
                s(1, 1),
                s(5, 4),
                s(4, 3),
                s(3, 2),
                s(5, 3),
                s(2, 1),
                s(9, 4),
                s(5, 2),
                s(8, 3),
                s(3, 1),
                s(10, 3),
                s(15, 4)
            ]
        );
        assert_eq!(
            supported_scales(1920, 1200),
            [
                s(1, 1),
                s(5, 4),
                s(4, 3),
                s(3, 2),
                s(5, 3),
                s(2, 1),
                s(5, 2)
            ]
        );
        assert_eq!(supported_scales(800, 600), [s(1, 1)]);
        assert_eq!(supported_scales(640, 480), [s(1, 1)]);
    }

    #[test]
    fn xml_scale_text() {
        assert_eq!(s(2, 1).xml_text(), "2");
        assert_eq!(s(5, 3).xml_text(), "1.6666666269302368");
        assert_eq!(s(5, 4).xml_text(), "1.25");
        assert_eq!(s(4, 3).xml_text(), "1.3333333730697632");
    }

    #[test]
    fn g17_matches_c() {
        // Values printed by glibc printf("%.17g").
        let ninety: f64 = "90.001434326171875".parse().unwrap();
        assert_eq!(format_g17(ninety), "90.001434326171875");
        let sixty: f64 = "60.000957489013672".parse().unwrap();
        assert_eq!(format_g17(sixty), "60.000957489013672");
        assert_eq!(format_g17(0.1), "0.10000000000000001");
        assert_eq!(format_g17(2.0), "2");
        assert_eq!(format_g17(100.0), "100");
        assert_eq!(format_g17(1e17), "1e+17");
        assert_eq!(format_g17(1.5e-5), "1.5e-05");
        assert_eq!(format_g17(0.000_1), "0.0001");
        assert_eq!(format_g17(-2.5), "-2.5");
        assert_eq!(format_g17(0.0), "0");
    }

    proptest! {
        #[test]
        fn g17_round_trips_every_double(bits: u64) {
            let value = f64::from_bits(bits);
            prop_assume!(value.is_finite());
            let text = format_g17(value);
            prop_assert_eq!(text.parse::<f64>().unwrap().to_bits(), value.to_bits());
        }

        #[test]
        fn xml_scale_round_trips_through_strtod_and_float(num in 1_u32..=16, den in 1_u32..=4) {
            let scale = Scale { num, den };
            let parsed = scale.xml_text().parse::<f64>().unwrap() as f32;
            prop_assert_eq!(parsed.to_bits(), scale.as_f32().to_bits());
        }
    }
}
