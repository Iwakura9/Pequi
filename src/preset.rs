//! Preset model, TOML load/save, fuzzy name resolution, PEQ import.

use crate::dsp::BandType;
use crate::validation;
use anyhow::{Context, Result};
use serde::{de, Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
#[cfg(test)]
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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

#[cfg(test)]
fn load_preset_file(path: &Path) -> Result<Preset> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_preset(&text, path)
}

#[cfg(test)]
fn parse_preset(text: &str, path: &Path) -> Result<Preset> {
    let preset: Preset =
        toml::from_str(text).with_context(|| format!("parsing {}", path.display()))?;
    validation::validate_preset(&preset, None)
        .with_context(|| format!("validating {}", path.display()))?;
    Ok(preset)
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

/// Severity assigned to a line-level PEQ import diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeqDiagnosticSeverity {
    Warning,
    Error,
}

impl std::fmt::Display for PeqDiagnosticSeverity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Warning => formatter.write_str("warning"),
            Self::Error => formatter.write_str("error"),
        }
    }
}

/// A problem found while parsing one line of a PEQ file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeqDiagnostic {
    pub line: usize,
    pub severity: PeqDiagnosticSeverity,
    pub reason: String,
}

/// Result of parsing a PEQ text file.
#[derive(Clone, Debug, PartialEq)]
pub struct PeqImport {
    pub preamp_db: f64,
    pub bands: Vec<Band>,
    pub diagnostics: Vec<PeqDiagnostic>,
    /// Compatibility view retained for callers that only displayed warnings.
    /// New callers should use [`Self::diagnostics`] so severity and line number
    /// remain available.
    pub warnings: Vec<String>,
    pub partial: bool,
}

impl PeqImport {
    /// Whether the import contains a diagnostic that prevents saving it.
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == PeqDiagnosticSeverity::Error)
    }

    /// Whether importing this result would discard an active filter.
    pub fn is_partial(&self) -> bool {
        self.partial
    }
}

/// Parse the text PEQ format: a `Preamp:` line plus `Filter N: ON PK Fc … Gain … Q …` lines.
///
/// Matching is case-insensitive and whitespace-tolerant. `OFF` filters are
/// ignored before their parameters are inspected, which handles the zero
/// placeholders some EQ tools emit. Unsupported active filter types are
/// warnings and make the result partial. Malformed active filters are errors.
/// Empty input and input without any `Preamp` or `Filter` line are hard errors.
pub fn parse_peq(text: &str) -> Result<PeqImport> {
    if text.trim().is_empty() {
        anyhow::bail!("PEQ input is empty");
    }

    let mut preamp_db = 0.0;
    let mut bands = Vec::new();
    let mut diagnostics = Vec::new();
    let mut partial = false;
    let mut recognized_line = false;
    let mut active_filters = 0usize;

    for (lineno, original_line) in text.lines().enumerate() {
        let line_number = lineno + 1;
        let line = original_line.trim().trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let tokens = normalized_tokens(line);
        let Some(first) = tokens.first() else {
            continue;
        };

        if first.eq_ignore_ascii_case("preamp") {
            recognized_line = true;
            match parse_preamp(&tokens) {
                Ok(value) => preamp_db = value,
                Err(reason) => add_diagnostic(
                    &mut diagnostics,
                    line_number,
                    PeqDiagnosticSeverity::Error,
                    reason,
                ),
            }
            continue;
        }

        if !first.eq_ignore_ascii_case("filter") {
            continue;
        }
        recognized_line = true;

        let state_index = tokens.iter().position(|token| {
            token.eq_ignore_ascii_case("on") || token.eq_ignore_ascii_case("off")
        });
        let Some(state_index) = state_index else {
            add_diagnostic(
                &mut diagnostics,
                line_number,
                PeqDiagnosticSeverity::Error,
                "missing ON/OFF filter state".to_string(),
            );
            continue;
        };

        if tokens[state_index].eq_ignore_ascii_case("off") {
            // Some tools emit `Fc 0 Hz Gain 0 dB Q 0` placeholders for disabled
            // filters; those are skipped silently. A disabled filter with real
            // values (as written by `to_peq`) is kept as a disabled band.
            let kind = tokens
                .get(state_index + 1)
                .and_then(|t| match_filter_type(t));
            let fields = (
                parse_field(&tokens, "fc", "Fc"),
                parse_field(&tokens, "gain", "Gain"),
                parse_field(&tokens, "q", "Q"),
            );
            if let (Some(kind), (Ok(freq), Ok(gain), Ok(q))) = (kind, fields) {
                if freq > 0.0 && q > 0.0 {
                    bands.push(Band {
                        kind,
                        freq,
                        gain,
                        q,
                        enabled: false,
                    });
                }
            }
            continue;
        }

        active_filters += 1;
        if active_filters > validation::MAX_BANDS {
            if tokens
                .get(state_index + 1)
                .is_some_and(|token| match_filter_type(token).is_none())
            {
                partial = true;
            }
            add_diagnostic(
                &mut diagnostics,
                line_number,
                PeqDiagnosticSeverity::Error,
                format!(
                    "more than {} active filters; maximum is {}",
                    validation::MAX_BANDS,
                    validation::MAX_BANDS
                ),
            );
            continue;
        }

        let Some(type_token) = tokens.get(state_index + 1) else {
            add_diagnostic(
                &mut diagnostics,
                line_number,
                PeqDiagnosticSeverity::Error,
                "missing active filter type".to_string(),
            );
            continue;
        };
        let kind = match_filter_type(type_token);
        let Some(kind) = kind else {
            partial = true;
            add_diagnostic(
                &mut diagnostics,
                line_number,
                PeqDiagnosticSeverity::Warning,
                format!("unsupported active filter type '{type_token}'"),
            );
            continue;
        };

        let freq = match parse_field(&tokens, "fc", "Fc") {
            Ok(value) => value,
            Err(reason) => {
                add_diagnostic(
                    &mut diagnostics,
                    line_number,
                    PeqDiagnosticSeverity::Error,
                    reason,
                );
                continue;
            }
        };
        let gain = match parse_field(&tokens, "gain", "Gain") {
            Ok(value) => value,
            Err(reason) => {
                add_diagnostic(
                    &mut diagnostics,
                    line_number,
                    PeqDiagnosticSeverity::Error,
                    reason,
                );
                continue;
            }
        };
        let q = match parse_field(&tokens, "q", "Q") {
            Ok(value) => value,
            Err(reason) => {
                add_diagnostic(
                    &mut diagnostics,
                    line_number,
                    PeqDiagnosticSeverity::Error,
                    reason,
                );
                continue;
            }
        };

        bands.push(Band {
            kind,
            freq,
            gain,
            q,
            enabled: true,
        });
    }

    if !recognized_line {
        anyhow::bail!("unrecognized PEQ input: no Preamp or Filter lines found");
    }

    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == PeqDiagnosticSeverity::Warning)
        .map(|diagnostic| format!("line {}: {}", diagnostic.line, diagnostic.reason))
        .collect();
    Ok(PeqImport {
        preamp_db,
        bands,
        diagnostics,
        warnings,
        partial,
    })
}

fn normalized_tokens(line: &str) -> Vec<&str> {
    // `Filter 1: ...` is the usual spelling, while SquigLink exports can use
    // spaces around the colon. Treat the separator uniformly.
    line.split(':').flat_map(str::split_whitespace).collect()
}

fn parse_preamp(tokens: &[&str]) -> std::result::Result<f64, String> {
    let value = tokens
        .get(1)
        .ok_or_else(|| "missing preamp value".to_string())?;
    let parsed = value
        .parse::<f64>()
        .map_err(|_| format!("invalid preamp value '{value}'"))?;
    if !parsed.is_finite() {
        return Err(format!("preamp value '{value}' is not finite"));
    }
    Ok(parsed)
}

fn match_filter_type(token: &str) -> Option<BandType> {
    if token.eq_ignore_ascii_case("pk") {
        Some(BandType::Peaking)
    } else if token.eq_ignore_ascii_case("ls") || token.eq_ignore_ascii_case("lsc") {
        Some(BandType::Lowshelf)
    } else if token.eq_ignore_ascii_case("hs") || token.eq_ignore_ascii_case("hsc") {
        Some(BandType::Highshelf)
    } else {
        None
    }
}

fn add_diagnostic(
    diagnostics: &mut Vec<PeqDiagnostic>,
    line: usize,
    severity: PeqDiagnosticSeverity,
    reason: String,
) {
    diagnostics.push(PeqDiagnostic {
        line,
        severity,
        reason,
    });
}

fn parse_field(tokens: &[&str], key: &str, label: &str) -> std::result::Result<f64, String> {
    let idx = tokens
        .iter()
        .position(|token| token.eq_ignore_ascii_case(key))
        .ok_or_else(|| format!("missing {label} value"))?;
    let value = tokens
        .get(idx + 1)
        .ok_or_else(|| format!("missing {label} value"))?;
    let parsed = value
        .parse::<f64>()
        .map_err(|_| format!("invalid {label} value '{value}'"))?;
    if !parsed.is_finite() {
        return Err(format!("{label} value '{value}' is not finite"));
    }
    Ok(parsed)
}

/// Read a PEQ file into a preset named after the file stem. Errors and partial
/// imports are rejected so a later save can never silently drop filters.
pub fn load_peq_file(path: &std::path::Path) -> Result<Preset> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let import = parse_peq(&text).with_context(|| format!("parsing {}", path.display()))?;
    if let Some(d) = import
        .diagnostics
        .iter()
        .find(|d| d.severity == PeqDiagnosticSeverity::Error || import.partial)
    {
        anyhow::bail!("line {}: {}", d.line, d.reason);
    }
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut preset = PresetDocument::new(name);
    preset.preamp_db = import.preamp_db;
    preset.bands = import.bands;
    Ok(preset)
}

/// Replace `path` with `contents` via a temp file in the same directory + rename.
pub fn write_atomic(path: &std::path::Path, contents: &str) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".pequi-tmp");
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", path.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

/// Serialize to the text PEQ format understood by [`parse_peq`].
pub fn to_peq(preset: &Preset) -> String {
    let mut out = format!("Preamp: {:.1} dB\n", preset.preamp_db);
    for (i, band) in preset.bands.iter().enumerate() {
        let kind = match band.kind {
            BandType::Peaking => "PK",
            BandType::Lowshelf => "LSC",
            BandType::Highshelf => "HSC",
        };
        out.push_str(&format!(
            "Filter {}: {} {kind} Fc {} Hz Gain {:.1} dB Q {:.3}\n",
            i + 1,
            if band.enabled { "ON" } else { "OFF" },
            band.freq.round(),
            band.gain,
            band.q,
        ));
    }
    out
}

/// Validate `match` glob patterns eagerly (used by `pequi watch` in the future). A pattern
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

    const SAMPLE_PEQ: &str = "Preamp: -4.2 dB
Filter 1: ON LSC Fc 105 Hz Gain 4.0 dB Q 0.700
Filter 2: ON PK Fc 250 Hz Gain -1.5 dB Q 1.000
Filter 3: ON PK Fc 2800 Hz Gain 2.5 dB Q 1.400
Filter 4: ON PK Fc 6500 Hz Gain -3.0 dB Q 3.000
Filter 5: ON HSC Fc 10000 Hz Gain 1.5 dB Q 0.700
Filter 6: OFF PK Fc 0 Hz Gain 0.0 dB Q 0.000
";

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
        let dir = std::env::temp_dir().join(format!("pequi-preset-test-{}", new_id()));
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
    fn peq_parses_basic_file() {
        let text = "Preamp: -6.8 dB\n\
                     Filter 1: ON PK Fc 21 Hz Gain 6.7 dB Q 1.100\n\
                     Filter 2: OFF PK Fc 85 Hz Gain 6.9 dB Q 3.000\n\
                     Filter 3: ON LSC Fc 105 Hz Gain 4.0 dB Q 0.70\n";
        let r = parse_peq(text).unwrap();
        assert_eq!(r.preamp_db, -6.8);
        assert_eq!(r.bands.len(), 3);
        assert_eq!(r.bands[0].kind, BandType::Peaking);
        assert!(!r.bands[1].enabled);
        assert_eq!(r.bands[2].kind, BandType::Lowshelf);
        assert!(r.diagnostics.is_empty());
    }

    #[test]
    fn peq_roundtrips_through_to_peq() {
        let parsed = parse_peq(SAMPLE_PEQ).unwrap();
        let mut preset = PresetDocument::new("x");
        preset.preamp_db = parsed.preamp_db;
        preset.bands = parsed.bands;
        preset.bands[0].enabled = false;
        let again = parse_peq(&to_peq(&preset)).unwrap();
        assert_eq!(again.preamp_db, preset.preamp_db);
        assert_eq!(again.bands, preset.bands);
    }

    #[test]
    fn example_profiles_parse_cleanly() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/profiles");
        let mut count = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let r = parse_peq(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert!(r.diagnostics.is_empty(), "{}", path.display());
            assert!(!r.bands.is_empty(), "{}", path.display());
            count += 1;
        }
        assert!(count > 0);
    }

    #[test]
    fn peq_warns_on_unsupported_type() {
        let text = "Filter 1: ON XY Fc 21 Hz Gain 6.7 dB Q 1.100\n";
        let r = parse_peq(text).unwrap();
        assert!(r.bands.is_empty());
        assert!(r.partial);
        assert_eq!(r.diagnostics.len(), 1);
        assert_eq!(r.diagnostics[0].line, 1);
        assert_eq!(r.diagnostics[0].severity, PeqDiagnosticSeverity::Warning);
    }

    #[test]
    fn peq_rejects_more_than_twenty_active_filters() {
        let mut text = String::new();
        for i in 1..=21 {
            text.push_str(&format!(
                "Filter {i}: ON PK Fc {f} Hz Gain 1.0 dB Q 1.0\n",
                f = 100 + i
            ));
        }
        let r = parse_peq(&text).unwrap();
        assert_eq!(r.bands.len(), validation::MAX_BANDS);
        assert!(r.has_errors());
        assert!(r.diagnostics.iter().any(|diagnostic| {
            diagnostic.line == 21 && diagnostic.reason.contains("more than 20 active filters")
        }));
    }

    #[test]
    fn peq_accepts_case_whitespace_and_crlf() {
        let text = "  PREAMP : -3.5 dB\r\n\
                     fIlTeR 1 : oN pK fC 100 Hz gAiN 2 dB q 1\r\n\
                     FILTER 2: ON hSc Fc 10000 Hz Gain -1 dB Q 0.7\r\n";
        let r = parse_peq(text).unwrap();
        assert_eq!(r.preamp_db, -3.5);
        assert_eq!(r.bands.len(), 2);
        assert_eq!(r.bands[0].kind, BandType::Peaking);
        assert_eq!(r.bands[1].kind, BandType::Highshelf);
        assert!(r.diagnostics.is_empty());
    }

    #[test]
    fn peq_ignores_malformed_off_placeholders() {
        let text =
            "Preamp: -6 dB\nFilter 1: OFF PK Fc 0 Hz Gain 0 dB Q 0\nFilter 2: OFF nonsense\n";
        let r = parse_peq(text).unwrap();
        assert!(r.bands.is_empty());
        assert!(r.diagnostics.is_empty());
    }

    #[test]
    fn peq_reports_malformed_active_filter_with_line_and_reason() {
        let text = "Preamp: -6 dB\nFilter 1: ON PK Fc nope Hz Gain 1 dB Q 1\n";
        let r = parse_peq(text).unwrap();
        assert!(r.has_errors());
        assert_eq!(r.diagnostics[0].line, 2);
        assert!(r.diagnostics[0].reason.contains("Fc"));
    }

    #[test]
    fn peq_rejects_empty_and_unrecognized_input() {
        assert!(parse_peq("  \r\n\t").is_err());
        assert!(parse_peq("GraphicEQ: 20 0; 100 1").is_err());
    }
}
