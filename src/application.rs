//! Application services shared by the CLI and TUI.
//!
//! The current backend still writes the filter-chain configuration and reloads
//! PipeWire. Keeping that detail here gives future daemon/engine work one place to
//! replace and prevents frontends from implementing different apply or bypass rules.

use crate::{chain, preset, pw};
use anyhow::{bail, Result};

/// A snapshot of the state visible to frontends, assembled from the current legacy
/// state files and PipeWire adapter.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusSnapshot {
    /// Name persisted as the active preset hint, if one has been applied.
    pub active_preset: Option<String>,
    /// Whether the legacy `peq` sink was found in PipeWire.
    pub connected: bool,
    /// Whether bypass was requested through the application service.
    pub bypassed: bool,
    /// Preamp of the active preset when it is still available on disk.
    pub preamp_db: Option<f64>,
}

impl StatusSnapshot {
    /// Waybar-compatible class for this state.
    pub fn class(&self) -> &'static str {
        if !self.connected {
            "disconnected"
        } else if self.bypassed {
            "off"
        } else {
            "active"
        }
    }

    /// Display text for the active preset column.
    pub fn text(&self) -> &str {
        self.active_preset.as_deref().unwrap_or("-")
    }

    /// Human-readable status tooltip.
    pub fn tooltip(&self) -> String {
        match (&self.active_preset, self.preamp_db) {
            (Some(_), Some(preamp_db)) => format!("preamp {preamp_db:.1} dB"),
            (Some(_), None) => "preset missing on disk".to_string(),
            (None, _) => "no active preset".to_string(),
        }
    }
}

/// Apply a complete preset through the current audio backend.
///
/// The active hint is updated only after PipeWire confirms that its reload finished;
/// failed applications therefore cannot be reported as successful or alter the
/// remembered preset. A successful apply always leaves bypass cleared.
pub fn apply_preset(preset: &preset::Preset) -> Result<()> {
    chain::write_config(preset)?;
    pw::reload()?;
    preset::write_active(&preset.name)?;
    clear_bypass()
}

/// Enable or disable bypass through the current audio backend.
///
/// `enabled = true` writes a silent chain while preserving the active preset name;
/// `enabled = false` restores that remembered preset through [`apply_preset`].
pub fn bypass(enabled: bool) -> Result<()> {
    if enabled {
        let name = preset::read_active().unwrap_or_else(|| "off".to_string());
        chain::write_config(&chain::silent_preset(&name))?;
        pw::reload()?;
        mark_bypass()
    } else {
        let Some(name) = preset::read_active() else {
            bail!("no preset was ever applied - run `peq <preset>` first");
        };
        let preset = preset::load_preset(&name)?;
        apply_preset(&preset)
    }
}

/// Read the current frontend-facing state from the legacy adapters.
pub fn status() -> StatusSnapshot {
    let active_preset = preset::read_active();
    let preamp_db = active_preset
        .as_deref()
        .and_then(|name| preset::load_preset(name).ok())
        .map(|preset| preset.preamp_db);

    StatusSnapshot {
        active_preset,
        connected: pw::sink_exists().unwrap_or(false),
        bypassed: is_bypassed(),
        preamp_db,
    }
}

fn bypass_marker() -> std::path::PathBuf {
    preset::state_dir().join("bypassed")
}

fn mark_bypass() -> Result<()> {
    std::fs::create_dir_all(preset::state_dir())?;
    std::fs::write(bypass_marker(), "")?;
    Ok(())
}

fn clear_bypass() -> Result<()> {
    let _ = std::fs::remove_file(bypass_marker());
    Ok(())
}

fn is_bypassed() -> bool {
    bypass_marker().exists()
}

#[cfg(test)]
mod tests {
    use super::StatusSnapshot;

    #[test]
    fn status_class_prioritizes_connection_then_bypass() {
        let disconnected = StatusSnapshot {
            active_preset: Some("test".into()),
            connected: false,
            bypassed: true,
            preamp_db: Some(-6.0),
        };
        assert_eq!(disconnected.class(), "disconnected");

        let off = StatusSnapshot {
            connected: true,
            ..disconnected.clone()
        };
        assert_eq!(off.class(), "off");

        let active = StatusSnapshot {
            bypassed: false,
            ..off
        };
        assert_eq!(active.class(), "active");
    }

    #[test]
    fn status_text_and_tooltip_handle_missing_preset() {
        let snapshot = StatusSnapshot {
            active_preset: None,
            connected: false,
            bypassed: false,
            preamp_db: None,
        };
        assert_eq!(snapshot.text(), "-");
        assert_eq!(snapshot.tooltip(), "no active preset");

        let snapshot = StatusSnapshot {
            active_preset: Some("test".into()),
            preamp_db: None,
            ..snapshot
        };
        assert_eq!(snapshot.text(), "test");
        assert_eq!(snapshot.tooltip(), "preset missing on disk");
    }
}
