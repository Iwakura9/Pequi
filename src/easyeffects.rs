//! EasyEffects adapter: write a preset JSON holding only an LSP equalizer and ask the
//! running EasyEffects to load it. All audio processing belongs to EasyEffects.

use crate::dsp::BandType;
use crate::preset::Preset;
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::process::Command;

/// Name of the scratch preset peq overwrites on every apply/preview.
pub const PRESET_NAME: &str = "peq";

pub fn preset_json(preset: &Preset) -> Value {
    let mut channel = Map::new();
    for (i, band) in preset.bands.iter().enumerate() {
        let kind = match band.kind {
            BandType::Peaking => "Bell",
            BandType::Lowshelf => "Lo-shelf",
            BandType::Highshelf => "Hi-shelf",
        };
        channel.insert(
            format!("band{i}"),
            json!({
                "type": kind,
                "mode": "RLC (BT)",
                "slope": "x1",
                "frequency": band.freq,
                "gain": band.gain,
                "q": band.q,
                "mute": !band.enabled,
                "solo": false,
            }),
        );
    }
    json!({
        "output": {
            "blocklist": [],
            "plugins_order": ["equalizer#0"],
            "equalizer#0": {
                "bypass": false,
                "balance": 0.0,
                "input-gain": preset.preamp_db,
                "output-gain": 0.0,
                "mode": "IIR",
                "num-bands": preset.bands.len(),
                "split-channels": false,
                "left": channel.clone(),
                "right": channel,
            }
        }
    })
}

fn output_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("neither XDG_DATA_HOME nor HOME is set")?;
    Ok(base.join("easyeffects/output"))
}

/// Write `preset` as the `peq` EasyEffects preset and load it.
pub fn load(preset: &Preset) -> Result<()> {
    let dir = output_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{PRESET_NAME}.json"));
    crate::preset::write_atomic(&path, &serde_json::to_string_pretty(&preset_json(preset))?)?;
    ee(&["-l", PRESET_NAME]).map(drop)
}

/// Whether EasyEffects' global bypass is on.
pub fn bypassed() -> Result<bool> {
    Ok(ee(&["-b", "3"])?.trim() == "1")
}

pub fn set_bypass(on: bool) -> Result<()> {
    ee(&["-b", if on { "1" } else { "2" }]).map(drop)
}

fn ee(args: &[&str]) -> Result<String> {
    let out = Command::new("easyeffects")
        .args(args)
        .output()
        .context("running easyeffects")?;
    if !out.status.success() {
        bail!(
            "easyeffects {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preset::Band;

    #[test]
    fn json_has_one_band_per_preset_band_on_both_channels() {
        let mut p = Preset::new("t");
        p.preamp_db = -3.0;
        for (kind, enabled) in [
            (BandType::Lowshelf, true),
            (BandType::Peaking, false),
            (BandType::Highshelf, true),
        ] {
            p.bands.push(Band {
                kind,
                freq: 1000.0,
                gain: 2.0,
                q: 0.7,
                enabled,
            });
        }
        let v = preset_json(&p);
        let eq = &v["output"]["equalizer#0"];
        assert_eq!(v["output"]["plugins_order"][0], "equalizer#0");
        assert_eq!(eq["num-bands"], 3);
        assert_eq!(eq["input-gain"], -3.0);
        for side in ["left", "right"] {
            assert_eq!(eq[side]["band0"]["type"], "Lo-shelf");
            assert_eq!(eq[side]["band1"]["type"], "Bell");
            assert_eq!(eq[side]["band1"]["mute"], true);
            assert_eq!(eq[side]["band2"]["type"], "Hi-shelf");
        }
    }

    /// Manual check against the running EasyEffects: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn loads_into_running_easyeffects() {
        let parsed = crate::preset::parse_autoeq(
            "Preamp: -3.0 dB\n\
             Filter 1: ON LSC Fc 105 Hz Gain 3.0 dB Q 0.700\n\
             Filter 2: ON PK Fc 1000 Hz Gain -2.0 dB Q 1.400\n\
             Filter 3: ON HSC Fc 8000 Hz Gain 2.0 dB Q 0.700\n",
        )
        .unwrap();
        let mut preset = Preset::new("t");
        preset.preamp_db = parsed.preamp_db;
        preset.bands = parsed.bands;
        load(&preset).unwrap();
    }
}
