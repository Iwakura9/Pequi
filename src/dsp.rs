//! RBJ Audio EQ Cookbook biquad coefficients and magnitude response.
//! Pure math, no I/O. <https://www.w3.org/2011/audio/audio-eq-cookbook.html>

use std::f64::consts::PI;

pub const FS: f64 = 48_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BandType {
    Peaking,
    Lowshelf,
    Highshelf,
}

/// Normalized biquad coefficients (a0 already divided out).
#[derive(Debug, Clone, Copy)]
pub struct Coeffs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

/// Compute RBJ biquad coefficients for one band at the fixed sample rate `FS`.
pub fn coeffs(kind: BandType, freq: f64, gain_db: f64, q: f64) -> Coeffs {
    let a = 10f64.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * freq / FS;
    let cos_w0 = w0.cos();
    let sin_w0 = w0.sin();
    let alpha = sin_w0 / (2.0 * q);

    let (b0, b1, b2, a0, a1, a2) = match kind {
        BandType::Peaking => (
            1.0 + alpha * a,
            -2.0 * cos_w0,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos_w0,
            1.0 - alpha / a,
        ),
        BandType::Lowshelf => {
            let sqrt_a = a.sqrt();
            (
                a * ((a + 1.0) - (a - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0),
                a * ((a + 1.0) - (a - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha),
                (a + 1.0) + (a - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0),
                (a + 1.0) + (a - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha,
            )
        }
        BandType::Highshelf => {
            let sqrt_a = a.sqrt();
            (
                a * ((a + 1.0) + (a - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0),
                a * ((a + 1.0) + (a - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha),
                (a + 1.0) - (a - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha,
                2.0 * ((a - 1.0) - (a + 1.0) * cos_w0),
                (a + 1.0) - (a - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha,
            )
        }
    };

    Coeffs {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

/// |H(e^jw)| in dB at frequency `freq` (Hz), w = 2*pi*freq/FS.
pub fn magnitude_db(c: Coeffs, freq: f64) -> f64 {
    let w = 2.0 * PI * freq / FS;
    let (cos1, sin1) = (w.cos(), w.sin());
    let (cos2, sin2) = ((2.0 * w).cos(), (2.0 * w).sin());

    let num_re = c.b0 + c.b1 * cos1 + c.b2 * cos2;
    let num_im = -(c.b1 * sin1 + c.b2 * sin2);
    let den_re = 1.0 + c.a1 * cos1 + c.a2 * cos2;
    let den_im = -(c.a1 * sin1 + c.a2 * sin2);

    let num_mag = (num_re * num_re + num_im * num_im).sqrt();
    let den_mag = (den_re * den_re + den_im * den_im).sqrt();

    20.0 * (num_mag / den_mag).log10()
}

/// One band's contribution to the summed response, in dB, at `freq`.
pub fn band_response_db(kind: BandType, freq: f64, gain_db: f64, q: f64, at: f64) -> f64 {
    magnitude_db(coeffs(kind, freq, gain_db, q), at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaking_gain_at_fc_matches_configured_gain() {
        for gain in [-12.0, -3.5, 6.0] {
            let c = coeffs(BandType::Peaking, 1000.0, gain, 0.7);
            let db = magnitude_db(c, 1000.0);
            assert!(
                (db - gain).abs() < 0.05,
                "gain {gain} -> measured {db} at Fc"
            );
        }
    }

    #[test]
    fn peaking_far_from_band_tends_to_zero_db() {
        let c = coeffs(BandType::Peaking, 1000.0, 12.0, 1.0);
        // four octaves away, well outside the bump
        let db = magnitude_db(c, 16.0);
        assert!(db.abs() < 0.5, "expected near 0dB far from band, got {db}");
    }

    #[test]
    fn lowshelf_reaches_gain_at_dc_and_zero_near_nyquist() {
        let c = coeffs(BandType::Lowshelf, 200.0, -6.0, 0.7);
        let dc = magnitude_db(c, 1.0);
        assert!(
            (dc - (-6.0)).abs() < 0.2,
            "DC should be near gain, got {dc}"
        );
        let hi = magnitude_db(c, 20_000.0);
        assert!(hi.abs() < 0.5, "high end should be near 0dB, got {hi}");
    }

    #[test]
    fn highshelf_reaches_gain_near_nyquist_and_zero_at_dc() {
        let c = coeffs(BandType::Highshelf, 8000.0, 5.0, 0.7);
        let hi = magnitude_db(c, 20_000.0);
        assert!(
            (hi - 5.0).abs() < 0.3,
            "high end should be near gain, got {hi}"
        );
        let dc = magnitude_db(c, 1.0);
        assert!(dc.abs() < 0.5, "DC should be near 0dB, got {dc}");
    }
}
