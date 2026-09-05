//! RBJ Audio EQ Cookbook biquad coefficients and magnitude response.
//! Pure math, no I/O. <https://www.w3.org/2011/audio/audio-eq-cookbook.html>

use std::f64::consts::PI;

use crate::preset::Preset;

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

/// Compute RBJ biquad coefficients for one band at `sample_rate` Hz.
///
/// The caller must provide finite, positive values within the preset validation
/// limits, with `freq` strictly below the sample rate's Nyquist frequency. The
/// routine is intentionally unchecked so its result can be used in an audio
/// callback without validation or allocation; validate preset data before
/// building coefficients for a stream.
pub fn coeffs(kind: BandType, freq: f64, gain_db: f64, q: f64, sample_rate: f64) -> Coeffs {
    let a = 10f64.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * freq / sample_rate;
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

/// Return `|H(e^jw)|` in dB at `freq` Hz, where `w = 2*pi*freq/sample_rate`.
///
/// The caller must provide finite, positive values within the preset validation
/// limits. As with [`coeffs`], validation belongs outside real-time processing.
pub fn magnitude_db(c: Coeffs, freq: f64, sample_rate: f64) -> f64 {
    let w = 2.0 * PI * freq / sample_rate;
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
pub fn band_response_db(
    kind: BandType,
    freq: f64,
    gain_db: f64,
    q: f64,
    at: f64,
    sample_rate: f64,
) -> f64 {
    magnitude_db(coeffs(kind, freq, gain_db, q, sample_rate), at, sample_rate)
}

/// Return a preset's summed response in dB at `frequency` Hz.
///
/// Only enabled bands contribute; the preset preamp is included once. The
/// caller must validate the preset and frequencies for the target stream before
/// calling this unchecked mathematical helper.
pub fn preset_response_db(preset: &Preset, frequency: f64, sample_rate: f64) -> f64 {
    preset.preamp_db
        + preset
            .bands
            .iter()
            .filter(|band| band.enabled)
            .map(|band| {
                band_response_db(
                    band.kind,
                    band.freq,
                    band.gain,
                    band.q,
                    frequency,
                    sample_rate,
                )
            })
            .sum::<f64>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaking_gain_at_fc_matches_configured_gain() {
        for sample_rate in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            for gain in [-12.0, -3.5, 6.0] {
                let c = coeffs(BandType::Peaking, 1000.0, gain, 0.7, sample_rate);
                let db = magnitude_db(c, 1000.0, sample_rate);
                assert!(
                    (db - gain).abs() < 0.05,
                    "rate {sample_rate}, gain {gain} -> measured {db} at Fc"
                );
            }
        }
    }

    #[test]
    fn peaking_far_from_band_tends_to_zero_db() {
        for sample_rate in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let c = coeffs(BandType::Peaking, 1000.0, 12.0, 1.0, sample_rate);
            // four octaves away, well outside the bump
            let db = magnitude_db(c, 16.0, sample_rate);
            assert!(
                db.abs() < 0.5,
                "rate {sample_rate}: expected near 0dB far from band, got {db}"
            );
        }
    }

    #[test]
    fn lowshelf_reaches_gain_at_dc_and_zero_near_nyquist() {
        for sample_rate in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let c = coeffs(BandType::Lowshelf, 200.0, -6.0, 0.7, sample_rate);
            let dc = magnitude_db(c, 1.0, sample_rate);
            assert!(
                (dc - (-6.0)).abs() < 0.2,
                "rate {sample_rate}: DC should be near gain, got {dc}"
            );
            let hi = magnitude_db(c, 20_000.0, sample_rate);
            assert!(
                hi.abs() < 0.5,
                "rate {sample_rate}: high end should be near 0dB, got {hi}"
            );
        }
    }

    #[test]
    fn highshelf_reaches_gain_near_nyquist_and_zero_at_dc() {
        for sample_rate in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let c = coeffs(BandType::Highshelf, 8000.0, 5.0, 0.7, sample_rate);
            let hi = magnitude_db(c, 20_000.0, sample_rate);
            assert!(
                (hi - 5.0).abs() < 0.3,
                "rate {sample_rate}: high end should be near gain, got {hi}"
            );
            let dc = magnitude_db(c, 1.0, sample_rate);
            assert!(
                dc.abs() < 0.5,
                "rate {sample_rate}: DC should be near 0dB, got {dc}"
            );
        }
    }

    #[test]
    fn extreme_valid_bands_have_finite_stable_coefficients() {
        let sample_rates = [44_100.0, 48_000.0, 96_000.0, 192_000.0];
        let frequencies = [20.0, 20_000.0];
        let gains = [-24.0, 24.0];
        let qs = [0.05, 50.0];

        for sample_rate in sample_rates {
            for kind in [BandType::Peaking, BandType::Lowshelf, BandType::Highshelf] {
                for freq in frequencies {
                    for gain in gains {
                        for q in qs {
                            let c = coeffs(kind, freq, gain, q, sample_rate);
                            for value in [c.b0, c.b1, c.b2, c.a1, c.a2] {
                                assert!(
                                    value.is_finite(),
                                    "non-finite coefficient for {kind:?} at {sample_rate} Hz"
                                );
                            }

                            // Jury conditions for z² + a1 z + a2: both poles
                            // lie strictly inside the unit circle.
                            assert!(c.a2.abs() < 1.0, "unstable a2: {c:?}");
                            assert!(1.0 + c.a1 + c.a2 > 0.0, "unstable pole at +1: {c:?}");
                            assert!(1.0 - c.a1 + c.a2 > 0.0, "unstable pole at -1: {c:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn preset_response_includes_preamp_and_enabled_bands_only() {
        use crate::preset::Band;

        let mut preset = Preset::new("response");
        preset.preamp_db = 2.0;
        preset.bands = vec![
            Band {
                kind: BandType::Peaking,
                freq: 1_000.0,
                gain: 6.0,
                q: 0.7,
                enabled: true,
            },
            Band {
                kind: BandType::Peaking,
                freq: 1_000.0,
                gain: 24.0,
                q: 0.7,
                enabled: false,
            },
        ];

        let response = preset_response_db(&preset, 1_000.0, 48_000.0);
        assert!((response - 8.0).abs() < 0.05, "got {response} dB");
    }
}
