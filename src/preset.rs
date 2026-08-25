//! Preset model, TOML load/save, fuzzy name resolution, AutoEQ import.

use crate::dsp::BandType;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const MAX_PEAKING_BANDS: usize = 18;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Band {
    #[serde(rename = "type")]
    pub kind: BandType,
    pub freq: f64,
    pub gain: f64,
    pub q: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub preamp_db: f64,
    #[serde(rename = "match", default, skip_serializing_if = "Vec::is_empty")]
    pub match_patterns: Vec<String>,
    #[serde(rename = "band", default)]
    pub bands: Vec<Band>,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no preset matches '{0}'")]
    NotFound(String),
    #[error("'{query}' is ambiguous, matches: {}", .candidates.join(", "))]
    Ambiguous {
        query: String,
        candidates: Vec<String>,
    },
}

fn xdg_dir(env_var: &str, home_fallback: &str) -> PathBuf {
    if let Ok(dir) = std::env::var(env_var) {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join(home_fallback)
}

pub fn preset_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("peq/presets")
}

pub fn state_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state").join("peq")
}

/// Sorted list of preset names (file stems) available in `preset_dir()`.
pub fn list_presets() -> Result<Vec<String>> {
    let dir = preset_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "toml"))
        .filter_map(|e| {
            e.path()
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    Ok(names)
}

pub fn preset_path(name: &str) -> PathBuf {
    preset_dir().join(format!("{name}.toml"))
}

pub fn load_preset(name: &str) -> Result<Preset> {
    let path = preset_path(name);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let preset: Preset =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    validate_match_patterns(&preset.match_patterns)
        .with_context(|| format!("`match` in {}", path.display()))?;
    Ok(preset)
}

pub fn save_preset(preset: &Preset) -> Result<()> {
    validate_match_patterns(&preset.match_patterns)?;
    let dir = preset_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = preset_path(&preset.name);
    let text = toml::to_string_pretty(preset).context("serializing preset")?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
}

/// Resolve a (possibly partial) name against the available preset list. Tries, in order:
/// exact match, unique prefix, unique substring, unique subsequence. Errors if none or
/// more than one candidate matches at the first tier that produces any match.
pub fn resolve_name(query: &str, available: &[String]) -> Result<String, ResolveError> {
    let q = query.to_lowercase();

    if let Some(exact) = available.iter().find(|n| n.to_lowercase() == q) {
        return Ok(exact.clone());
    }

    let tiers: [fn(&str, &str) -> bool; 3] = [
        |n, q| n.starts_with(q),
        |n, q| n.contains(q),
        |n, q| is_subsequence(q, n),
    ];

    for tier in tiers {
        let matches: Vec<&String> = available
            .iter()
            .filter(|n| tier(&n.to_lowercase(), &q))
            .collect();
        match matches.len() {
            0 => continue,
            1 => return Ok(matches[0].clone()),
            _ => {
                return Err(ResolveError::Ambiguous {
                    query: query.to_string(),
                    candidates: matches.into_iter().cloned().collect(),
                })
            }
        }
    }
    Err(ResolveError::NotFound(query.to_string()))
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|c| chars.any(|h| h == c))
}

pub fn read_active() -> Option<String> {
    std::fs::read_to_string(state_dir().join("active"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn write_active(name: &str) -> Result<()> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(dir.join("active"), name)
        .with_context(|| format!("writing {}", dir.join("active").display()))
}

/// Result of parsing an AutoEQ `ParametricEQ.txt` file.
pub struct AutoEqImport {
    pub preamp_db: f64,
    pub bands: Vec<Band>,
    pub warnings: Vec<String>,
}

/// Parse the AutoEQ/SquigLink `ParametricEQ.txt` format. `OFF` filters are dropped
/// silently; unsupported filter types warn and are skipped; more than
/// [`MAX_PEAKING_BANDS`] peaking filters warns and truncates.
pub fn parse_autoeq(text: &str) -> AutoEqImport {
    let mut preamp_db = 0.0;
    let mut bands = Vec::new();
    let mut warnings = Vec::new();
    let mut peaking_count = 0;

    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        let n = lineno + 1;
        if let Some(rest) = line.strip_prefix("Preamp:") {
            if let Some(tok) = rest.split_whitespace().next() {
                match tok.parse() {
                    Ok(v) => preamp_db = v,
                    Err(_) => warnings.push(format!("line {n}: couldn't parse preamp value")),
                }
            }
            continue;
        }
        if !line.starts_with("Filter") {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 4 {
            continue;
        }
        if tokens[2] == "OFF" {
            continue;
        }
        if tokens[2] != "ON" {
            warnings.push(format!(
                "line {n}: unrecognized filter state '{}', skipped",
                tokens[2]
            ));
            continue;
        }
        let kind = match tokens[3] {
            "PK" => BandType::Peaking,
            "LSC" | "LS" => BandType::Lowshelf,
            "HSC" | "HS" => BandType::Highshelf,
            other => {
                warnings.push(format!(
                    "line {n}: unsupported filter type '{other}', skipped"
                ));
                continue;
            }
        };
        let freq = find_after(&tokens, "Fc");
        let gain = find_after(&tokens, "Gain");
        let q = find_after(&tokens, "Q");
        let (Some(freq), Some(gain), Some(q)) = (freq, gain, q) else {
            warnings.push(format!("line {n}: missing Fc/Gain/Q, skipped"));
            continue;
        };

        if kind == BandType::Peaking {
            if peaking_count >= MAX_PEAKING_BANDS {
                warnings.push(format!(
                    "more than {MAX_PEAKING_BANDS} peaking filters, truncating at line {n}"
                ));
                break;
            }
            peaking_count += 1;
        }

        bands.push(Band {
            kind,
            freq,
            gain,
            q,
        });
    }

    AutoEqImport {
        preamp_db,
        bands,
        warnings,
    }
}

fn find_after(tokens: &[&str], key: &str) -> Option<f64> {
    let idx = tokens.iter().position(|t| *t == key)?;
    tokens.get(idx + 1)?.parse().ok()
}

/// Validate `match` glob patterns eagerly (used by `peq watch` in the future). A pattern
/// is just `*`-wildcard glob text; we don't compile it here, only reject empty patterns.
pub fn validate_match_patterns(patterns: &[String]) -> Result<()> {
    for p in patterns {
        if p.trim().is_empty() {
            anyhow::bail!("empty `match` pattern");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefix() {
        let names = vec!["hd6xx".to_string(), "hd800".to_string()];
        assert_eq!(resolve_name("hd6", &names).unwrap(), "hd6xx");
    }

    #[test]
    fn resolve_ambiguous_prefix() {
        let names = vec!["hd6xx".to_string(), "hd650".to_string()];
        assert!(matches!(
            resolve_name("hd6", &names),
            Err(ResolveError::Ambiguous { .. })
        ));
    }

    #[test]
    fn resolve_not_found() {
        let names = vec!["hd6xx".to_string()];
        assert!(matches!(
            resolve_name("zzz", &names),
            Err(ResolveError::NotFound(_))
        ));
    }

    #[test]
    fn autoeq_parses_basic_file() {
        let text = "Preamp: -6.8 dB\n\
                     Filter 1: ON PK Fc 21 Hz Gain 6.7 dB Q 1.100\n\
                     Filter 2: OFF PK Fc 85 Hz Gain 6.9 dB Q 3.000\n\
                     Filter 3: ON LSC Fc 105 Hz Gain 4.0 dB Q 0.70\n";
        let r = parse_autoeq(text);
        assert_eq!(r.preamp_db, -6.8);
        assert_eq!(r.bands.len(), 2);
        assert_eq!(r.bands[0].kind, BandType::Peaking);
        assert_eq!(r.bands[1].kind, BandType::Lowshelf);
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn autoeq_warns_on_unsupported_type() {
        let text = "Filter 1: ON XY Fc 21 Hz Gain 6.7 dB Q 1.100\n";
        let r = parse_autoeq(text);
        assert!(r.bands.is_empty());
        assert_eq!(r.warnings.len(), 1);
    }

    #[test]
    fn autoeq_truncates_after_18_peaking() {
        let mut text = String::new();
        for i in 1..=25 {
            text.push_str(&format!(
                "Filter {i}: ON PK Fc {f} Hz Gain 1.0 dB Q 1.0\n",
                f = 100 + i
            ));
        }
        let r = parse_autoeq(&text);
        assert_eq!(r.bands.len(), MAX_PEAKING_BANDS);
        assert!(r.warnings.iter().any(|w| w.contains("truncating")));
    }
}
