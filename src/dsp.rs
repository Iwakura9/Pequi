//! RBJ Audio EQ Cookbook biquad coefficients and magnitude response.
//! Pure math, no I/O. <https://www.w3.org/2011/audio/audio-eq-cookbook.html>

use std::f64::consts::PI;

use crate::preset::Preset;

pub const FS: f64 = 48_000.0;
/// Maximum number of filters accepted by a preset and stored in a runtime bank.
///
/// The value is kept here as part of the audio-path contract.  Preset validation
/// is still the authority for documents, while the fixed array means processing
/// never has to grow or allocate a collection.
pub const MAX_FILTERS: usize = crate::validation::MAX_BANDS;

/// Values smaller than this are flushed from the filter state.  This keeps a
/// quiet stream from spending time in the CPU's denormal slow path.  The value
/// is far below the useful level of an f32 audio stream.
const DENORMAL_THRESHOLD: f64 = 1.0e-20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BandType {
    Peaking,
    Lowshelf,
    Highshelf,
}

/// Normalized biquad coefficients (a0 already divided out).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coeffs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

const IDENTITY_COEFFS: Coeffs = Coeffs {
    b0: 1.0,
    b1: 0.0,
    b2: 0.0,
    a1: 0.0,
    a2: 0.0,
};

/// A precomputed bank of the enabled filters in a preset.
///
/// Coefficients are stored in preset order, so the runtime graph has the same
/// response as [`preset_response_db`].  Disabled bands are validated with the
/// rest of the document but do not occupy a processing slot.  The array is
/// fixed at [`MAX_FILTERS`] to make the callback allocation-free.
#[derive(Debug, Clone, Copy)]
pub struct FilterBank {
    /// Sample rate used when these coefficients were calculated.
    sample_rate: f64,
    /// Linear gain corresponding to the preset's preamp in dB.
    preamp_linear: f64,
    /// Enabled filter coefficients in the same order as the preset bands.
    coefficients: [Coeffs; MAX_FILTERS],
    /// Number of valid entries at the front of [`Self::coefficients`].
    len: usize,
}

impl FilterBank {
    /// Build a runtime bank after validating every preset field against the
    /// target stream's sample rate.  Validation and coefficient calculation are
    /// deliberately outside the audio callback.
    pub fn new(preset: &Preset, sample_rate: f64) -> anyhow::Result<Self> {
        crate::validation::validate_preset(preset, Some(sample_rate))?;

        let mut coefficients = [IDENTITY_COEFFS; MAX_FILTERS];
        let mut len = 0;
        for band in preset.bands.iter().filter(|band| band.enabled) {
            // Validation above guarantees this cannot be exceeded.  Keep the
            // guard so a future change to the validation limit cannot turn a
            // malformed document into an out-of-bounds write.
            if len == MAX_FILTERS {
                break;
            }
            coefficients[len] = coeffs(band.kind, band.freq, band.gain, band.q, sample_rate);
            len += 1;
        }

        Ok(Self {
            sample_rate,
            preamp_linear: 10f64.powf(preset.preamp_db / 20.0),
            coefficients,
            len,
        })
    }

    /// Number of enabled filters in this bank.
    pub const fn filter_count(&self) -> usize {
        self.len
    }

    /// Whether this bank has no enabled filters and therefore only applies the
    /// preamp gain.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Sample rate captured by this bank.
    pub const fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Linear preamp multiplier captured by this bank.
    pub const fn preamp_linear(&self) -> f64 {
        self.preamp_linear
    }

    /// Precomputed coefficients, with valid entries in the first
    /// [`Self::filter_count`] positions.
    pub const fn coefficients(&self) -> &[Coeffs; MAX_FILTERS] {
        &self.coefficients
    }
}

/// The two delay elements used by one transposed direct-form II biquad.
///
/// A separate array of these states is kept for each stereo channel.  Keeping
/// the state type public lets a native callback owner inspect or explicitly
/// reset a processor without exposing any allocation-based representation.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BiquadState {
    pub z1: f64,
    pub z2: f64,
}

/// Allocation-free stereo processor for a precomputed [`FilterBank`].
///
/// Filters are evaluated in transposed direct-form II order for each channel:
/// `y = b0*x + z1`, `z1 = b1*x - a1*y + z2`, and
/// `z2 = b2*x - a2*y`.  Left and right channels own independent state arrays;
/// processing one channel never changes the other channel's history.
#[derive(Debug, Clone)]
pub struct StereoProcessor {
    bank: FilterBank,
    left_state: [BiquadState; MAX_FILTERS],
    right_state: [BiquadState; MAX_FILTERS],
    bypassed: bool,
}

impl StereoProcessor {
    /// Construct a processor and validate the preset for `sample_rate`.
    pub fn new(preset: &Preset, sample_rate: f64) -> anyhow::Result<Self> {
        Ok(Self {
            bank: FilterBank::new(preset, sample_rate)?,
            left_state: [BiquadState::default(); MAX_FILTERS],
            right_state: [BiquadState::default(); MAX_FILTERS],
            bypassed: false,
        })
    }

    /// Return the stream rate captured when the bank was built.
    pub const fn sample_rate(&self) -> f64 {
        self.bank.sample_rate()
    }

    /// Return the preamp as a linear multiplier.
    pub const fn preamp_linear(&self) -> f64 {
        self.bank.preamp_linear()
    }

    /// Borrow the precomputed bank used by this processor.
    pub const fn bank(&self) -> &FilterBank {
        &self.bank
    }

    /// Enable or disable exact passthrough.  A transition resets both channel
    /// states, so bypass never resumes from stale wet history.  C05 can add a
    /// crossfade around this explicit transition without changing the callback
    /// contract.
    pub fn set_bypassed(&mut self, bypassed: bool) {
        if self.bypassed != bypassed {
            self.reset();
        }
        self.bypassed = bypassed;
    }

    pub const fn is_bypassed(&self) -> bool {
        self.bypassed
    }

    /// Clear both channels' delay state. This is a control-path operation also
    /// used when bypass changes, so stale wet history cannot reappear later.
    pub fn reset(&mut self) {
        self.left_state = [BiquadState::default(); MAX_FILTERS];
        self.right_state = [BiquadState::default(); MAX_FILTERS];
    }

    /// Process an interleaved `[left, right, left, right, ...]` block in place.
    ///
    /// The method performs no allocation, locking, I/O, or validation.  A
    /// trailing sample in an odd-length slice is left unchanged because it has
    /// no complete stereo frame.  When bypassed, the method returns before
    /// touching the slice, providing exact bit-for-bit passthrough.
    #[inline]
    pub fn process_interleaved(&mut self, samples: &mut [f32]) {
        if self.bypassed {
            return;
        }

        let mut index = 0;
        while index + 1 < samples.len() {
            let left = samples[index] as f64;
            let right = samples[index + 1] as f64;
            let (left, right) = self.process_frame(left, right);
            samples[index] = left as f32;
            samples[index + 1] = right as f32;
            index += 2;
        }
    }

    /// Process a planar stereo block in place.
    ///
    /// Complete frames up to the shorter input are processed.  Any unmatched
    /// tail in either slice remains unchanged, and bypass is exact passthrough.
    #[inline]
    pub fn process_planar(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.bypassed {
            return;
        }

        let frames = left.len().min(right.len());
        for index in 0..frames {
            let (left_sample, right_sample) =
                self.process_frame(left[index] as f64, right[index] as f64);
            left[index] = left_sample as f32;
            right[index] = right_sample as f32;
        }
    }

    /// Process one stereo frame without allocating.  This is useful for a
    /// callback that receives one frame at a time.
    #[inline]
    pub fn process_frame(&mut self, left: f64, right: f64) -> (f64, f64) {
        if self.bypassed {
            return (left, right);
        }

        let left = process_channel(
            left * self.bank.preamp_linear,
            &self.bank.coefficients,
            self.bank.len,
            &mut self.left_state,
        );
        let right = process_channel(
            right * self.bank.preamp_linear,
            &self.bank.coefficients,
            self.bank.len,
            &mut self.right_state,
        );
        (left, right)
    }

    /// Process one f32 stereo frame without allocating.
    #[inline]
    pub fn process_frame_f32(&mut self, left: f32, right: f32) -> (f32, f32) {
        let (left, right) = self.process_frame(left as f64, right as f64);
        (left as f32, right as f32)
    }
}

/// Process one channel through all active bank entries.
#[inline]
fn process_channel(
    mut value: f64,
    coefficients: &[Coeffs; MAX_FILTERS],
    len: usize,
    states: &mut [BiquadState; MAX_FILTERS],
) -> f64 {
    for index in 0..len {
        let c = coefficients[index];
        let state = &mut states[index];

        // Transposed direct-form II.  Keeping state in f64 avoids accumulating
        // avoidable rounding error while the public block API remains f32.
        let output = c.b0 * value + state.z1;
        let z1 = c.b1 * value - c.a1 * output + state.z2;
        let z2 = c.b2 * value - c.a2 * output;
        state.z1 = flush_denormal(z1);
        state.z2 = flush_denormal(z2);
        value = flush_denormal(output);
    }
    value
}

#[inline]
fn flush_denormal(value: f64) -> f64 {
    if value.is_finite() && value.abs() < DENORMAL_THRESHOLD {
        0.0
    } else {
        value
    }
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

    fn one_band_preset(kind: BandType, freq: f64, gain: f64, q: f64) -> Preset {
        use crate::preset::Band;

        let mut preset = Preset::new("runtime");
        preset.bands.push(Band {
            kind,
            freq,
            gain,
            q,
            enabled: true,
        });
        preset
    }

    #[test]
    fn filter_bank_precomputes_only_enabled_bands_and_checks_nyquist() {
        use crate::preset::Band;

        let mut preset = Preset::new("bank");
        preset.bands = vec![
            Band {
                kind: BandType::Peaking,
                freq: 1_000.0,
                gain: 6.0,
                q: 1.0,
                enabled: false,
            },
            Band {
                kind: BandType::Highshelf,
                freq: 8_000.0,
                gain: -3.0,
                q: 0.7,
                enabled: true,
            },
        ];

        let bank = FilterBank::new(&preset, 48_000.0).expect("valid bank");
        assert_eq!(bank.filter_count(), 1);
        assert_eq!(
            bank.coefficients[0],
            coeffs(BandType::Highshelf, 8_000.0, -3.0, 0.7, 48_000.0)
        );

        preset.bands[1].freq = 24_000.0;
        assert!(FilterBank::new(&preset, 48_000.0).is_err());
    }

    #[test]
    fn impulse_matches_transposed_direct_form_reference() {
        let preset = {
            let mut preset = Preset::new("impulse");
            preset.preamp_db = -2.0;
            preset.bands = vec![
                crate::preset::Band {
                    kind: BandType::Lowshelf,
                    freq: 120.0,
                    gain: 3.0,
                    q: 0.7,
                    enabled: true,
                },
                crate::preset::Band {
                    kind: BandType::Peaking,
                    freq: 1_200.0,
                    gain: -5.0,
                    q: 1.3,
                    enabled: true,
                },
                crate::preset::Band {
                    kind: BandType::Highshelf,
                    freq: 8_000.0,
                    gain: 2.0,
                    q: 0.8,
                    enabled: true,
                },
            ];
            preset
        };
        let rate = 48_000.0;
        let bank = FilterBank::new(&preset, rate).expect("valid bank");
        let mut processor = StereoProcessor::new(&preset, rate).expect("valid processor");
        let mut states = [BiquadState::default(); MAX_FILTERS];

        let mut block = vec![0.0_f32; 2 * 64];
        block[0] = 1.0;
        for frame in block.chunks_exact_mut(2) {
            let input = frame[0] as f64 * bank.preamp_linear;
            let mut expected = input;
            for (index, state) in states.iter_mut().take(bank.len).enumerate() {
                let c = bank.coefficients[index];
                let output = c.b0 * expected + state.z1;
                state.z1 = c.b1 * expected - c.a1 * output + state.z2;
                state.z2 = c.b2 * expected - c.a2 * output;
                expected = output;
            }

            processor.process_interleaved(frame);
            assert!((frame[0] as f64 - expected).abs() < 1.0e-6);
            assert_eq!(frame[1], 0.0);
        }
    }

    #[test]
    fn centered_sine_reaches_configured_gain_after_settling() {
        let rate = 48_000.0;
        let frequency = 1_000.0;
        let preset = one_band_preset(BandType::Peaking, frequency, 6.0, 0.7);
        let expected_gain = 10f64.powf(preset_response_db(&preset, frequency, rate) / 20.0);
        let mut processor = StereoProcessor::new(&preset, rate).expect("valid processor");
        let mut input_sum = 0.0;
        let mut output_sum = 0.0;
        let total = rate as usize;
        let settled = total - 4_800;
        for index in 0..total {
            let input = 0.1 * (2.0 * PI * frequency * index as f64 / rate).sin();
            let (left, right) = processor.process_frame_f32(input as f32, input as f32);
            if index >= settled {
                input_sum += input * input;
                output_sum += left as f64 * left as f64;
                assert_eq!(left, right);
            }
        }
        let measured_gain = (output_sum / input_sum).sqrt();
        assert!(
            (measured_gain - expected_gain).abs() < 1.0e-4,
            "expected gain {expected_gain}, measured {measured_gain}"
        );
    }

    #[test]
    fn silence_stays_zero_and_finite() {
        let mut preset = Preset::new("silence");
        for index in 0..MAX_FILTERS {
            preset.bands.push(crate::preset::Band {
                kind: match index % 3 {
                    0 => BandType::Peaking,
                    1 => BandType::Lowshelf,
                    _ => BandType::Highshelf,
                },
                freq: 40.0 + index as f64 * 100.0,
                gain: if index % 2 == 0 { 12.0 } else { -12.0 },
                q: 0.7,
                enabled: true,
            });
        }
        let mut processor = StereoProcessor::new(&preset, 48_000.0).expect("valid processor");
        let mut block = vec![0.0_f32; 2 * 4_096];
        processor.process_interleaved(&mut block);
        assert!(block
            .iter()
            .all(|sample| *sample == 0.0 && sample.is_finite()));
    }

    #[test]
    fn stereo_channels_have_independent_state() {
        let preset = one_band_preset(BandType::Peaking, 2_000.0, 12.0, 4.0);
        let mut processor = StereoProcessor::new(&preset, 48_000.0).expect("valid processor");
        let mut block = vec![0.0_f32; 2 * 128];
        block[0] = 1.0;
        processor.process_interleaved(&mut block);
        assert!(block.chunks_exact(2).all(|frame| frame[1] == 0.0));

        let mut right_only = vec![0.0_f32; 2 * 128];
        right_only[1] = 1.0;
        processor.reset();
        processor.process_interleaved(&mut right_only);
        assert!(right_only.chunks_exact(2).all(|frame| frame[0] == 0.0));
    }

    #[test]
    fn bypass_is_exact_passthrough_and_resets_state_on_transition() {
        let preset = one_band_preset(BandType::Peaking, 1_000.0, 9.0, 1.0);
        let mut processor = StereoProcessor::new(&preset, 48_000.0).expect("valid processor");
        let mut warmup = [1.0_f32, 0.0, 0.0, 0.0];
        processor.process_interleaved(&mut warmup);

        let mut bypassed = [
            f32::from_bits(0x8000_0000),
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::MIN,
            -0.25,
        ];
        let original = bypassed;
        processor.set_bypassed(true);
        processor.process_interleaved(&mut bypassed);
        assert!(bypassed
            .iter()
            .zip(original.iter())
            .all(|(actual, expected)| actual.to_bits() == expected.to_bits()));
        assert!(processor.is_bypassed());

        processor.set_bypassed(false);
        let mut fresh = StereoProcessor::new(&preset, 48_000.0).expect("valid processor");
        let mut resumed = [0.25_f32, -0.5];
        let mut from_fresh = resumed;
        processor.process_interleaved(&mut resumed);
        fresh.process_interleaved(&mut from_fresh);
        assert_eq!(resumed, from_fresh);
    }
}
