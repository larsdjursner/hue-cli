//! sRGB <-> CIE xy, using Philips' gamma correction and wide-gamut matrices,
//! plus white tones (mireds) to sRGB for display.
//! The bridge clamps xy to what each lamp can show.

/// What a lamp shows: a CIE xy point, or a white tone in mireds (1,000,000 / kelvin).
#[derive(Clone, Copy)]
pub enum Color {
    Xy([f64; 2]),
    Ct(u16),
}

impl Color {
    /// sRGB at full brightness; brightness is shown separately.
    pub fn to_rgb(self) -> [u8; 3] {
        match self {
            Color::Xy(xy) => xy_to_rgb(xy),
            Color::Ct(ct) => ct_to_rgb(ct),
        }
    }
}

pub fn hex_to_xy(hex: &str) -> Result<[f64; 2], String> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("color must look like rrggbb".into());
    }
    let channel = |i: usize| linear(u8::from_str_radix(&hex[i..i + 2], 16).unwrap() as f64 / 255.0);
    let (r, g, b) = (channel(0), channel(2), channel(4));
    let x = r * 0.664511 + g * 0.154324 + b * 0.162028;
    let y = r * 0.283881 + g * 0.668433 + b * 0.047685;
    let z = r * 0.000088 + g * 0.072310 + b * 0.986039;
    let sum = x + y + z;
    if sum == 0.0 {
        return Err("black isn't a color, use: hue off <target>".into());
    }
    Ok([round4(x / sum), round4(y / sum)])
}

pub fn to_hex([r, g, b]: [u8; 3]) -> String {
    format!("{r:02x}{g:02x}{b:02x}")
}

fn xy_to_rgb([x, y]: [f64; 2]) -> [u8; 3] {
    let y = y.max(1e-6);
    // XYZ with luminance Y = 1, then the inverse of hex_to_xy's matrix.
    let (cx, cz) = (x / y, (1.0 - x - y) / y);
    let rgb = [
        cx * 1.656492 - 0.354851 - cz * 0.255038,
        -cx * 0.707196 + 1.655397 + cz * 0.036152,
        cx * 0.051713 - 0.121364 + cz * 1.011530,
    ]
    .map(|c| c.max(0.0));
    let max = rgb[0].max(rgb[1]).max(rgb[2]).max(1e-6);
    rgb.map(|c| to_u8(gamma(c / max)))
}

/// Tanner Helland's fit of blackbody colors, good enough for a swatch.
fn ct_to_rgb(mireds: u16) -> [u8; 3] {
    let t = 1_000_000.0 / mireds.max(1) as f64 / 100.0;
    let (r, g, b) = if t <= 66.0 {
        let b = if t <= 19.0 {
            0.0
        } else {
            138.5177312231 * (t - 10.0).ln() - 305.0447927307
        };
        (255.0, 99.4708025861 * t.ln() - 161.1195681661, b)
    } else {
        (
            329.698727446 * (t - 60.0).powf(-0.1332047592),
            288.1221695283 * (t - 60.0).powf(-0.0755148492),
            255.0,
        )
    };
    [r, g, b].map(|c| c.clamp(0.0, 255.0).round() as u8)
}

fn linear(c: f64) -> f64 {
    if c > 0.04045 {
        ((c + 0.055) / 1.055).powf(2.4)
    } else {
        c / 12.92
    }
}

fn gamma(c: f64) -> f64 {
    if c <= 0.0031308 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn to_u8(c: f64) -> u8 {
    (c.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

#[cfg(test)]
mod tests {
    use super::{Color, ct_to_rgb, hex_to_xy, to_hex};

    #[test]
    fn matches_philips_reference_points() {
        assert_eq!(hex_to_xy("#ff0000"), Ok([0.7006, 0.2993]));
        assert_eq!(hex_to_xy("ffffff"), Ok([0.3227, 0.329]));
        assert_eq!(hex_to_xy("#0000ff"), Ok([0.1355, 0.0399]));
    }

    #[test]
    fn rejects_bad_input() {
        for bad in ["#000000", "#zzzzzz", "#fff", "+fffff", "ffææ"] {
            assert!(hex_to_xy(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn xy_round_trips_to_rgb() {
        for hex in ["ff0000", "00ff00", "0000ff", "ff8800", "ffffff"] {
            let rgb = Color::Xy(hex_to_xy(hex).unwrap()).to_rgb();
            let want = u32::from_str_radix(hex, 16).unwrap().to_be_bytes();
            for (got, want) in rgb.iter().zip(&want[1..]) {
                assert!(
                    got.abs_diff(*want) <= 3,
                    "{hex} came back as {}",
                    to_hex(rgb)
                );
            }
        }
    }

    #[test]
    fn white_tones_go_from_warm_to_cool() {
        assert_eq!(ct_to_rgb(500), [255, 137, 14]);
        let cool = ct_to_rgb(153);
        assert!(cool[2] > 240, "{cool:?}");
    }
}
