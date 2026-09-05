//! Preset model, TOML load/save, fuzzy name resolution, AutoEQ import.

use crate::dsp::BandType;
use anyhow::{Context, Result};
use serde::{de, Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_PEAKING_BANDS: usize = 18;
/// Version of the TOML document format understood by this build.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Band {
    #[serde(rename = "type")]
    pub kind: BandType,
    pub freq: f64,
    pub gain: f64,
    pub q: f64,
    /// Whether this band participates in the rendered curve and audio graph.
    /// Missing `enabled` in a legacy document means enabled.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// A versioned preset document.
///
/// `schema_version` and `id` are emitted for new documents. Legacy TOML without
/// either field is accepted and receives a deterministic in-memory ID based on
/// its content; loading never writes that upgraded representation back to disk.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PresetDocument {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub preamp_db: f64,
    #[serde(rename = "match", default, skip_serializing_if = "Vec::is_empty")]
    pub match_patterns: Vec<String>,
    #[serde(rename = "band", default)]
    pub bands: Vec<Band>,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

/// Compatibility name retained for existing chain, CLI and TUI consumers.
pub type Preset = PresetDocument;

#[derive(Debug, Deserialize)]
struct PresetWire {
    #[serde(default)]
    schema_version: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    preamp_db: f64,
    #[serde(rename = "match", default)]
    match_patterns: Vec<String>,
    #[serde(rename = "band", default)]
    bands: Vec<Band>,
    #[serde(default)]
    favorite: bool,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

impl<'de> Deserialize<'de> for PresetDocument {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PresetWire::deserialize(deserializer)?;
        if let Some(version) = wire.schema_version {
            if version != CURRENT_SCHEMA_VERSION {
                return Err(de::Error::custom(format!(
                    "unsupported preset schema version {version}; supported version is {CURRENT_SCHEMA_VERSION}"
                )));
            }
        }

        let mut document = Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            id: wire.id.unwrap_or_default(),
            name: wire.name,
            preamp_db: wire.preamp_db,
            match_patterns: wire.match_patterns,
            bands: wire.bands,
            favorite: wire.favorite,
            metadata: wire.metadata,
        };
        if document.id.trim().is_empty() {
            document.id = legacy_id(&document);
        }
        Ok(document)
    }
}

impl PresetDocument {
    /// Construct a new document with a fresh stable identity.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            id: new_id(),
            name: name.into(),
            preamp_db: 0.0,
            match_patterns: Vec::new(),
            bands: Vec::new(),
            favorite: false,
            metadata: BTreeMap::new(),
        }
    }
}

fn new_id() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut random) = File::open("/dev/urandom") {
        if random.read_exact(&mut bytes).is_ok() {
            return format_id(&bytes);
        }
    }

    // Linux provides /dev/urandom, but retain a dependency-free fallback for
    // platforms where it is unavailable. The process-local counter prevents
    // repeated calls in one process from colliding.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut state = now.as_nanos() as u64
        ^ (std::process::id() as u64).rotate_left(17)
        ^ NEXT_ID.fetch_add(1, Ordering::Relaxed).rotate_left(31);
    for byte in &mut bytes {
        state ^= state << 7;
        state ^= state >> 9;
        state ^= state << 8;
        *byte = state as u8;
    }
    format_id(&bytes)
}

fn format_id(bytes: &[u8; 16]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(result, "{byte:02x}");
    }
    result
}

/// Derive a stable identity for a legacy document. The canonical field stream
/// deliberately excludes generated fields, so equivalent legacy documents get
/// the same ID regardless of TOML whitespace or key ordering.
fn legacy_id(document: &PresetDocument) -> String {
    let mut bytes = Vec::new();
    append_string(&mut bytes, &document.name);
    bytes.extend_from_slice(&document.preamp_db.to_bits().to_le_bytes());
    bytes.extend_from_slice(&(document.match_patterns.len() as u64).to_le_bytes());
    for pattern in &document.match_patterns {
        append_string(&mut bytes, pattern);
    }
    bytes.extend_from_slice(&(document.bands.len() as u64).to_le_bytes());
    for band in &document.bands {
        bytes.push(match band.kind {
            BandType::Peaking => 0,
            BandType::Lowshelf => 1,
            BandType::Highshelf => 2,
        });
        bytes.extend_from_slice(&band.freq.to_bits().to_le_bytes());
        bytes.extend_from_slice(&band.gain.to_bits().to_le_bytes());
        bytes.extend_from_slice(&band.q.to_bits().to_le_bytes());
        bytes.push(u8::from(band.enabled));
    }
    bytes.push(u8::from(document.favorite));
    bytes.extend_from_slice(&(document.metadata.len() as u64).to_le_bytes());
    for (key, value) in &document.metadata {
        append_string(&mut bytes, key);
        append_string(&mut bytes, value);
    }

    // FNV-1a is tiny, deterministic and available without another dependency.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("legacy-{hash:016x}")
}

fn append_string(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(&(value.len() as u64).to_le_bytes());
    bytes.extend_from_slice(value.as_bytes());
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
    load_preset_file(&path)
}

fn load_preset_file(path: &Path) -> Result<Preset> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_preset(&text, path)
}

fn parse_preset(text: &str, path: &Path) -> Result<Preset> {
    let preset: Preset =
        toml::from_str(text).with_context(|| format!("parsing {}", path.display()))?;
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
            enabled: true,
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

    const LEGACY_TOML: &str = r#"name = "Legacy"
preamp_db = -6.0
match = ["Example *"]

[[band]]
type = "peaking"
freq = 1000.0
gain = 3.0
q = 0.7
"#;

    #[test]
    fn legacy_toml_gets_defaults_and_stable_identity() {
        let first: PresetDocument = toml::from_str(LEGACY_TOML).unwrap();
        let second: PresetDocument = toml::from_str(LEGACY_TOML).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.schema_version, CURRENT_SCHEMA_VERSION);
        assert!(first.id.starts_with("legacy-"));
        assert!(first.bands[0].enabled);
    }

    #[test]
    fn legacy_identity_changes_when_content_changes() {
        let first: PresetDocument = toml::from_str(LEGACY_TOML).unwrap();
        let changed = LEGACY_TOML.replace("gain = 3.0", "gain = 3.5");
        let second: PresetDocument = toml::from_str(&changed).unwrap();

        assert_ne!(first.id, second.id);
    }

    #[test]
    fn legacy_load_does_not_rewrite_toml() {
        let dir = std::env::temp_dir().join(format!("peq-preset-test-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.toml");
        std::fs::write(&path, LEGACY_TOML).unwrap();
        let before = std::fs::read(&path).unwrap();

        let loaded = load_preset_file(&path).unwrap();
        let after = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.name, "Legacy");
        assert_eq!(before, after);
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let text = format!(
            "schema_version = {}\nname = \"future\"\n",
            CURRENT_SCHEMA_VERSION + 1
        );
        let error = toml::from_str::<PresetDocument>(&text).unwrap_err();
        assert!(error
            .to_string()
            .contains("unsupported preset schema version"));
    }

    #[test]
    fn new_document_roundtrips_identity_and_metadata() {
        let mut preset = PresetDocument::new("roundtrip");
        preset.favorite = true;
        preset.metadata.insert("source".into(), "test".into());
        preset.bands.push(Band {
            kind: BandType::Peaking,
            freq: 1000.0,
            gain: -2.0,
            q: 1.0,
            enabled: false,
        });
        let encoded = toml::to_string(&preset).unwrap();
        let decoded: PresetDocument = toml::from_str(&encoded).unwrap();

        assert_eq!(decoded, preset);
    }

    #[test]
    fn new_documents_receive_distinct_identities() {
        let first = PresetDocument::new("same name");
        let second = PresetDocument::new("same name");

        assert_ne!(first.id, second.id);
        assert!(!first.id.is_empty());
    }

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
