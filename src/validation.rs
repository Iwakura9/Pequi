//! Validation for preset documents and names.
//!
//! All callers that accept preset data from disk, the CLI, or an editor should
//! use this module before parsing, rendering, or persisting that data.  The
//! limits here are deliberately independent of the current filter-chain layout:
//! a preset may contain any mix of the supported band types.

use crate::preset::{Preset, CURRENT_SCHEMA_VERSION};
use anyhow::{bail, Result};

/// The maximum number of bands in a preset document.
pub const MAX_BANDS: usize = 20;
/// The lowest frequency accepted by a preset band, in Hz.
pub const MIN_FREQUENCY_HZ: f64 = 20.0;
/// The highest frequency accepted by a preset band, in Hz.
pub const MAX_FREQUENCY_HZ: f64 = 20_000.0;
/// The minimum and maximum accepted Q values.
pub const MIN_Q: f64 = 0.05;
pub const MAX_Q: f64 = 50.0;
/// The minimum and maximum accepted band gains, in dB.
pub const MIN_GAIN_DB: f64 = -24.0;
pub const MAX_GAIN_DB: f64 = 24.0;
/// The minimum and maximum accepted preamp, in dB.
pub const MIN_PREAMP_DB: f64 = -60.0;
pub const MAX_PREAMP_DB: f64 = 12.0;

// Leave room for the `.toml` suffix on filesystems with a 255-byte component
// limit.
const MAX_NAME_BYTES: usize = 250;
const MAX_ID_BYTES: usize = 128;
const MAX_METADATA_ENTRIES: usize = 64;
const MAX_METADATA_KEY_BYTES: usize = 128;
const MAX_METADATA_VALUE_BYTES: usize = 4096;
const MAX_MATCH_PATTERN_BYTES: usize = 4096;

/// Validate a preset name before using it as a file stem or a CLI operand.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("preset name must not be empty");
    }
    if name != name.trim() {
        bail!("preset name must not have leading or trailing whitespace");
    }
    if name.len() > MAX_NAME_BYTES {
        bail!("preset name is too long (maximum {MAX_NAME_BYTES} bytes)");
    }
    if name == "." || name == ".." {
        bail!("preset name must not be a path traversal component");
    }
    if name.chars().any(|c| matches!(c, '/' | '\\')) {
        bail!("preset name must not contain path separators");
    }
    if name.chars().any(char::is_control) {
        bail!("preset name must not contain control characters");
    }
    if name.starts_with('-') {
        bail!("preset name must not start with '-'");
    }

    Ok(())
}

/// Validate a preset document.  When `sample_rate` is supplied, every band
/// frequency must also be strictly below that stream's Nyquist frequency.
pub fn validate_preset(preset: &Preset, sample_rate: Option<f64>) -> Result<()> {
    validate_name(&preset.name)?;
    validate_id(&preset.id)?;

    if preset.schema_version != CURRENT_SCHEMA_VERSION {
        bail!(
            "unsupported preset schema version {}; supported version is {}",
            preset.schema_version,
            CURRENT_SCHEMA_VERSION
        );
    }
    if preset.bands.len() > MAX_BANDS {
        bail!(
            "preset has {} bands; maximum is {MAX_BANDS}",
            preset.bands.len()
        );
    }
    validate_finite_range("preamp_db", preset.preamp_db, MIN_PREAMP_DB, MAX_PREAMP_DB)?;

    let nyquist = match sample_rate {
        Some(rate) => {
            if !rate.is_finite() || rate <= 2.0 * MIN_FREQUENCY_HZ {
                bail!(
                    "sample rate must be finite and greater than {} Hz",
                    2.0 * MIN_FREQUENCY_HZ
                );
            }
            Some(rate / 2.0)
        }
        None => None,
    };

    for (index, band) in preset.bands.iter().enumerate() {
        let band_number = index + 1;
        let field = |name: &str| format!("band {band_number} {name}");
        validate_finite_range(
            &field("freq"),
            band.freq,
            MIN_FREQUENCY_HZ,
            MAX_FREQUENCY_HZ,
        )?;
        if let Some(nyquist) = nyquist {
            if band.freq >= nyquist {
                bail!(
                    "band {band_number} freq {} Hz must be below Nyquist ({nyquist} Hz)",
                    band.freq
                );
            }
        }
        validate_finite_range(&field("gain"), band.gain, MIN_GAIN_DB, MAX_GAIN_DB)?;
        validate_finite_range(&field("q"), band.q, MIN_Q, MAX_Q)?;
    }

    if preset.metadata.len() > MAX_METADATA_ENTRIES {
        bail!(
            "preset metadata has {} entries; maximum is {MAX_METADATA_ENTRIES}",
            preset.metadata.len()
        );
    }
    for (key, value) in &preset.metadata {
        if key.is_empty() || key != key.trim() {
            bail!("metadata keys must be nonempty and trimmed");
        }
        if key.len() > MAX_METADATA_KEY_BYTES {
            bail!("metadata key is too long (maximum {MAX_METADATA_KEY_BYTES} bytes)");
        }
        if key.chars().any(char::is_control) {
            bail!("metadata key '{key}' must not contain control characters");
        }
        if value.len() > MAX_METADATA_VALUE_BYTES {
            bail!(
                "metadata value for '{key}' is too long (maximum {MAX_METADATA_VALUE_BYTES} bytes)"
            );
        }
        if value.chars().any(char::is_control) {
            bail!("metadata value for '{key}' must not contain control characters");
        }
    }

    for pattern in &preset.match_patterns {
        if pattern.len() > MAX_MATCH_PATTERN_BYTES {
            bail!("match pattern is too long (maximum {MAX_MATCH_PATTERN_BYTES} bytes)");
        }
    }
    crate::preset::validate_match_patterns(&preset.match_patterns)?;

    Ok(())
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id != id.trim() {
        bail!("preset id must be nonempty and trimmed");
    }
    if id.len() > MAX_ID_BYTES {
        bail!("preset id is too long (maximum {MAX_ID_BYTES} bytes)");
    }
    if id == "." || id == ".." {
        bail!("preset id must not be a path traversal component");
    }
    if id.chars().any(|c| matches!(c, '/' | '\\')) {
        bail!("preset id must not contain path separators");
    }
    if id.chars().any(char::is_control) {
        bail!("preset id must not contain control characters");
    }
    Ok(())
}

fn validate_finite_range(field: &str, value: f64, min: f64, max: f64) -> Result<()> {
    if !value.is_finite() {
        bail!("{field} must be finite");
    }
    if !(min..=max).contains(&value) {
        bail!("{field} must be between {min} and {max}, got {value}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::BandType;
    use crate::preset::{Band, Preset};

    fn valid_preset() -> Preset {
        let mut preset = Preset::new("valid");
        preset.bands.push(Band {
            kind: BandType::Peaking,
            freq: 1_000.0,
            gain: 0.0,
            q: 1.0,
            enabled: true,
        });
        preset
    }

    #[test]
    fn rejects_unsafe_names() {
        for name in ["", " ", " ../x", "..", "a/b", r"a\b", "a\n"] {
            assert!(validate_name(name).is_err(), "accepted {name:?}");
        }
        assert!(validate_name("status").is_ok());
        assert!(validate_name("safe name").is_ok());
    }

    #[test]
    fn accepts_twenty_mixed_bands_but_not_twenty_one() {
        let mut preset = Preset::new("twenty");
        for index in 0..20 {
            preset.bands.push(Band {
                kind: match index % 3 {
                    0 => BandType::Peaking,
                    1 => BandType::Lowshelf,
                    _ => BandType::Highshelf,
                },
                freq: 20.0 + index as f64,
                gain: 0.0,
                q: 1.0,
                enabled: index % 2 == 0,
            });
        }
        assert!(validate_preset(&preset, None).is_ok());
        preset.bands.push(preset.bands[0].clone());
        assert!(validate_preset(&preset, None).is_err());
    }

    #[test]
    fn disabled_bands_still_need_valid_values() {
        let mut preset = valid_preset();
        preset.bands[0].enabled = false;
        preset.bands[0].freq = f64::NAN;
        assert!(validate_preset(&preset, None).is_err());
    }

    #[test]
    fn rejects_non_finite_and_out_of_range_values() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut preset = valid_preset();
            preset.preamp_db = value;
            assert!(validate_preset(&preset, None).is_err());
        }

        for (field, value) in [
            ("freq", 19.9),
            ("freq", 20_000.1),
            ("gain", -24.1),
            ("gain", 24.1),
            ("q", 0.049),
            ("q", 50.1),
        ] {
            let mut preset = valid_preset();
            match field {
                "freq" => preset.bands[0].freq = value,
                "gain" => preset.bands[0].gain = value,
                "q" => preset.bands[0].q = value,
                _ => unreachable!(),
            }
            assert!(validate_preset(&preset, None).is_err(), "accepted {field}");
        }
    }

    #[test]
    fn validates_sample_rate_and_nyquist() {
        let preset = valid_preset();
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(validate_preset(&preset, Some(rate)).is_err());
        }

        let mut edge = preset.clone();
        edge.bands[0].freq = 20_000.0;
        assert!(validate_preset(&edge, Some(40_000.0)).is_err());
        assert!(validate_preset(&edge, Some(40_001.0)).is_ok());
    }

    #[test]
    fn validates_id_and_metadata_limits() {
        let mut preset = valid_preset();
        preset.id.clear();
        assert!(validate_preset(&preset, None).is_err());

        preset.id = "valid-id".into();
        preset.metadata.insert("source".into(), "test".into());
        assert!(validate_preset(&preset, None).is_ok());

        preset
            .metadata
            .insert("too-long".into(), "x".repeat(MAX_METADATA_VALUE_BYTES + 1));
        assert!(validate_preset(&preset, None).is_err());
    }
}
