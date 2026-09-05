//! Durable preset storage.
//!
//! The store deliberately keeps library writes explicit.  A caller must choose
//! [`SaveMode::Create`] or provide the [`Revision`] returned by [`PresetStore::load`]
//! before an existing file can be replaced.  This lets a stale editor report a
//! conflict instead of silently discarding a newer edit.

use crate::preset::PresetDocument;
use crate::validation;
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions, ReadDir};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{ffi::OsString, ops::Deref};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// A content revision for a preset file.
///
/// Revisions are hashes of the exact bytes on disk, rather than hashes of a
/// parsed document.  Consequently a formatting-only external edit is still
/// detected and legacy documents can be upgraded explicitly by a later save.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Revision([u8; 32]);

impl Revision {
    /// Return the raw digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Render the digest as lowercase hexadecimal, useful in diagnostics.
    pub fn to_hex(self) -> String {
        let mut result = String::with_capacity(64);
        for byte in self.0 {
            use std::fmt::Write as _;
            let _ = write!(result, "{byte:02x}");
        }
        result
    }
}

impl std::fmt::Display for Revision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

/// The permitted persistence operations.
///
/// There is intentionally no unconditional replacement mode.  Existing files
/// require the revision observed by the caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SaveMode {
    /// Create a new preset and fail if its path already exists.
    Create,
    /// Replace an existing preset only if its bytes still have this revision.
    Replace(Revision),
}

/// A document together with the revision observed when it was loaded.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedPreset {
    pub document: PresetDocument,
    pub revision: Revision,
}

impl LoadedPreset {
    /// Compatibility accessor for callers that use `preset` terminology.
    pub fn preset(&self) -> &PresetDocument {
        &self.document
    }
}

/// A malformed or unreadable file encountered while listing the library.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

/// The result of listing a store.  Healthy files remain available even when
/// another file cannot be parsed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PresetListing {
    pub entries: Vec<LoadedPreset>,
    pub diagnostics: Vec<StoreDiagnostic>,
}

/// Error returned when another writer owns the store lock.
#[derive(Debug, thiserror::Error)]
#[error("preset store is locked by another writer ({path}); it may be stale")]
pub struct StoreLocked {
    pub path: PathBuf,
}

/// Error returned when a caller's content revision no longer matches the file.
#[derive(Debug, thiserror::Error)]
#[error("preset '{name}' changed concurrently (expected {expected}, found {actual})")]
pub struct RevisionConflict {
    pub name: String,
    pub expected: Revision,
    pub actual: Revision,
}

/// Error returned when create is requested for an existing path.
#[derive(Debug, thiserror::Error)]
#[error("preset '{name}' already exists at {path}")]
pub struct PresetAlreadyExists {
    pub name: String,
    pub path: PathBuf,
}

/// A preset moved to the recoverable trash area by [`PresetStore::remove`].
///
/// The token and path are both exposed so a caller can persist either one and
/// pass it to [`PresetStore::restore`].  The type dereferences to its path for
/// callers that only need to inspect the trash entry as a filesystem path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemovedPreset {
    pub name: String,
    pub token: String,
    pub path: PathBuf,
    pub revision: Revision,
}

/// Compatibility aliases for callers that use a different noun for a trash
/// entry.  New code should prefer [`RemovedPreset`].
pub type DeletedPreset = RemovedPreset;
pub type TrashEntry = RemovedPreset;

impl RemovedPreset {
    /// Return the unique token assigned to this deletion.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Return the path in the store's `.trash` directory.
    pub fn as_path(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for RemovedPreset {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Deref for RemovedPreset {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

/// Persistent TOML preset library rooted at one directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresetStore {
    root: PathBuf,
}

impl PresetStore {
    /// Construct a store rooted at an explicit directory.
    ///
    /// The directory is created by the first mutating operation, so this
    /// constructor is safe for callers that only want to inspect a path.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Construct the default store using XDG config rules.
    pub fn from_xdg() -> Self {
        Self::new(xdg_preset_dir())
    }

    /// The directory containing preset TOML files.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Acquire the store writer lock for transactional management operations.
    ///
    /// This is crate-visible so preset management can keep a compare, rename,
    /// or recoverable delete sequence under the same lock as saves.
    pub(crate) fn acquire_lock(&self) -> Result<StoreLock> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        StoreLock::acquire(&self.root)
    }

    /// Load one valid document and the exact-byte revision used for CAS saves.
    pub fn load(&self, name: &str) -> Result<LoadedPreset> {
        validation::validate_name(name)?;
        let path = self.path_for_name(name)?;
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let document = parse_document(&bytes, &path)?;
        if document.name != name {
            anyhow::bail!(
                "preset file {} contains document named '{}', expected '{name}'",
                path.display(),
                document.name
            );
        }
        Ok(LoadedPreset {
            document,
            revision: revision(&bytes),
        })
    }

    /// List valid files while collecting a diagnostic for every invalid TOML file.
    pub fn list(&self) -> Result<PresetListing> {
        let mut listing = PresetListing::default();
        let read_dir = match fs::read_dir(&self.root) {
            Ok(read_dir) => read_dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(listing),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", self.root.display()))
            }
        };

        collect_entries(read_dir, &mut listing);
        listing.entries.sort_by(|left, right| {
            left.document
                .name
                .cmp(&right.document.name)
                .then_with(|| left.revision.cmp(&right.revision))
        });
        listing
            .diagnostics
            .sort_by(|left, right| left.path.cmp(&right.path));
        Ok(listing)
    }

    /// Save according to an explicit create or compare-and-swap mode.
    pub fn save(&self, document: &PresetDocument, mode: SaveMode) -> Result<Revision> {
        validation::validate_preset(document, None)?;
        let name = document.name.as_str();
        let path = self.path_for_name(name)?;
        let _lock = self.acquire_lock()?;

        let old_bytes = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };

        match mode {
            SaveMode::Create => {
                if old_bytes.is_some() {
                    return Err(PresetAlreadyExists {
                        name: name.to_string(),
                        path,
                    }
                    .into());
                }
            }
            SaveMode::Replace(expected) => {
                let Some(ref bytes) = old_bytes else {
                    return Err(RevisionConflict {
                        name: name.to_string(),
                        expected,
                        actual: revision(&[]),
                    }
                    .into());
                };
                let actual = revision(bytes);
                if actual != expected {
                    return Err(RevisionConflict {
                        name: name.to_string(),
                        expected,
                        actual,
                    }
                    .into());
                }
                // Do not replace an invalid file through a raw byte token.  The
                // existing content is still recoverable and should be diagnosed.
                parse_document(bytes, &path)?;
            }
        }

        let encoded = toml::to_string_pretty(document).context("serializing preset")?;
        let new_bytes = encoded.into_bytes();

        if let Some(ref old_bytes) = old_bytes {
            // The backup is written before the final rename.  If either this
            // write or the new-file write fails, the original path is untouched.
            let backup = self.backup_path(name)?;
            atomic_write(&backup, old_bytes).with_context(|| {
                format!("backing up {} to {}", path.display(), backup.display())
            })?;
        }

        atomic_replace(&path, &new_bytes)?;
        Ok(revision(&new_bytes))
    }

    /// Convenience wrapper for an explicit create operation.
    pub fn create(&self, document: &PresetDocument) -> Result<Revision> {
        self.save(document, SaveMode::Create)
    }

    /// Convenience wrapper for an explicit compare-and-swap replacement.
    pub fn replace(&self, document: &PresetDocument, expected: Revision) -> Result<Revision> {
        self.save(document, SaveMode::Replace(expected))
    }

    /// Duplicate a valid preset under a new name.
    ///
    /// The source is read and the destination is checked while the store lock
    /// is held.  The copied document receives a fresh stable identity; all
    /// other document fields, including its favorite state, are retained.
    pub fn duplicate(&self, source: &str, new_name: &str) -> Result<Revision> {
        validation::validate_name(source)?;
        validation::validate_name(new_name)?;
        let source_path = self.path_for_name(source)?;
        let target_path = self.path_for_name(new_name)?;
        let _lock = self.acquire_lock()?;

        let (_, _source_bytes, source_document, _) = self.load_locked(source, &source_path)?;
        ensure_absent(&target_path, new_name)?;

        let mut duplicate = source_document;
        duplicate.name = new_name.to_string();
        // Constructing a document is the public way to obtain a fresh identity;
        // the source identity must never be reused by a duplicate.
        duplicate.id = PresetDocument::new(new_name).id;
        validation::validate_preset(&duplicate, None)?;
        let encoded = serialize_document(&duplicate)?;
        atomic_replace(&target_path, &encoded).with_context(|| {
            format!(
                "writing duplicate of {} to {}",
                source_path.display(),
                target_path.display()
            )
        })?;

        Ok(revision(&encoded))
    }

    /// Rename a preset after a compare-and-swap check.
    ///
    /// The document identity is retained.  The exact source bytes are saved in
    /// the source backup before the destination is written, so a failed target
    /// write leaves both the source and a recoverable copy available.
    pub fn rename(&self, source: &str, new_name: &str, expected: Revision) -> Result<Revision> {
        validation::validate_name(source)?;
        validation::validate_name(new_name)?;
        let source_path = self.path_for_name(source)?;
        let target_path = self.path_for_name(new_name)?;
        let _lock = self.acquire_lock()?;

        let (_, source_bytes, mut renamed, actual) = self.load_locked(source, &source_path)?;
        ensure_expected(source, expected, actual)?;
        ensure_absent(&target_path, new_name)?;

        renamed.name = new_name.to_string();
        validation::validate_preset(&renamed, None)?;
        let encoded = serialize_document(&renamed)?;

        // Backup first.  If this write or the target write fails, source_path
        // has not been removed and remains the last valid library file.
        let backup = self.backup_path(source)?;
        atomic_write(&backup, &source_bytes).with_context(|| {
            format!(
                "backing up renamed preset {} to {}",
                source_path.display(),
                backup.display()
            )
        })?;
        atomic_replace(&target_path, &encoded).with_context(|| {
            format!(
                "writing renamed preset {} to {}",
                source_path.display(),
                target_path.display()
            )
        })?;

        // The destination is already durable before the source is removed. If
        // removal itself fails, the original remains recoverable on disk.
        fs::remove_file(&source_path)
            .with_context(|| format!("removing renamed source {}", source_path.display()))?;
        sync_directory(&self.root)?;
        Ok(revision(&encoded))
    }

    /// Remove a preset into `.trash` after a compare-and-swap check.
    ///
    /// The returned token/path is required for restoration, making accidental
    /// restoration of an unrelated deletion impossible.
    pub fn remove(&self, name: &str, expected: Revision) -> Result<RemovedPreset> {
        validation::validate_name(name)?;
        let source_path = self.path_for_name(name)?;
        let _lock = self.acquire_lock()?;

        let (_, _source_bytes, document, actual) = self.load_locked(name, &source_path)?;
        ensure_expected(name, expected, actual)?;
        let (token, trash_path) = self.reserve_trash_path(name)?;

        if let Err(error) = fs::rename(&source_path, &trash_path) {
            let _ = fs::remove_file(&trash_path);
            return Err(error).with_context(|| {
                format!(
                    "moving preset {} to recoverable trash {}",
                    source_path.display(),
                    trash_path.display()
                )
            });
        }
        // Both directory entries changed. Syncing them keeps the move
        // recoverable across a sudden process or machine failure.
        sync_directory(&self.root)?;
        sync_directory(&self.trash_dir())?;

        Ok(RemovedPreset {
            name: document.name,
            token,
            path: trash_path,
            revision: actual,
        })
    }

    /// Restore a deletion identified by its returned path or token.
    ///
    /// The destination must be absent. Existing content is never overwritten;
    /// if writing the destination fails, the trash entry remains untouched and
    /// can be retried later.
    pub fn restore<P: AsRef<Path>>(&self, deleted: P) -> Result<Revision> {
        let trash_path = self.resolve_trash_path(deleted.as_ref())?;
        let _lock = self.acquire_lock()?;

        let bytes = fs::read(&trash_path)
            .with_context(|| format!("reading deleted preset {}", trash_path.display()))?;
        let document = parse_document(&bytes, &trash_path)?;
        let target_path = self.path_for_name(&document.name)?;
        ensure_absent(&target_path, &document.name)?;

        // Preserve the exact bytes that were deleted, including legacy TOML
        // formatting and its stable identity.
        atomic_replace(&target_path, &bytes).with_context(|| {
            format!(
                "restoring deleted preset {} to {}",
                trash_path.display(),
                target_path.display()
            )
        })?;
        fs::remove_file(&trash_path)
            .with_context(|| format!("removing restored trash entry {}", trash_path.display()))?;
        sync_directory(&self.trash_dir())?;
        Ok(revision(&bytes))
    }

    /// Set a preset's favorite flag after a compare-and-swap check.
    pub fn set_favorite(&self, name: &str, expected: Revision, favorite: bool) -> Result<Revision> {
        validation::validate_name(name)?;
        let path = self.path_for_name(name)?;
        let _lock = self.acquire_lock()?;

        let (_, bytes, mut document, actual) = self.load_locked(name, &path)?;
        ensure_expected(name, expected, actual)?;
        if document.favorite == favorite {
            return Ok(actual);
        }
        document.favorite = favorite;
        validation::validate_preset(&document, None)?;
        let encoded = serialize_document(&document)?;
        let backup = self.backup_path(name)?;
        atomic_write(&backup, &bytes)
            .with_context(|| format!("backing up favorite update for {name}"))?;
        atomic_replace(&path, &encoded)
            .with_context(|| format!("writing favorite update for {name}"))?;
        Ok(revision(&encoded))
    }

    /// List valid presets and retain diagnostics for malformed files.
    ///
    /// This named alias makes the diagnostic-preserving API discoverable for
    /// callers migrating from [`crate::preset::list_presets`].
    pub fn list_with_diagnostics(&self) -> Result<PresetListing> {
        self.list()
    }

    /// Return the path used for a named preset after validating the name.
    pub fn path_for_name(&self, name: &str) -> Result<PathBuf> {
        validation::validate_name(name)?;
        Ok(self.root.join(format!("{name}.toml")))
    }

    /// Return the sidecar path used for the latest valid previous document.
    pub fn backup_path(&self, name: &str) -> Result<PathBuf> {
        self.path_for_name(name)?;
        Ok(self.root.join(".backups").join(format!("{name}.toml")))
    }

    /// Load the latest backup, if it contains a valid document.
    pub fn load_backup(&self, name: &str) -> Result<LoadedPreset> {
        validation::validate_name(name)?;
        let path = self.backup_path(name)?;
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let document = parse_document(&bytes, &path)?;
        if document.name != name {
            anyhow::bail!(
                "backup {} contains document named '{}', expected '{name}'",
                path.display(),
                document.name
            );
        }
        Ok(LoadedPreset {
            document,
            revision: revision(&bytes),
        })
    }

    fn load_locked(
        &self,
        name: &str,
        path: &Path,
    ) -> Result<(PathBuf, Vec<u8>, PresetDocument, Revision)> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let document = parse_document(&bytes, path)?;
        if document.name != name {
            anyhow::bail!(
                "preset file {} contains document named '{}', expected '{name}'",
                path.display(),
                document.name
            );
        }
        let current_revision = revision(&bytes);
        Ok((path.to_path_buf(), bytes, document, current_revision))
    }

    fn trash_dir(&self) -> PathBuf {
        self.root.join(".trash")
    }

    fn reserve_trash_path(&self, name: &str) -> Result<(String, PathBuf)> {
        let directory = self.trash_dir();
        fs::create_dir_all(&directory)
            .with_context(|| format!("creating recoverable trash {}", directory.display()))?;
        for _ in 0..100 {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let token = format!("{name}-{stamp}-{id}");
            let path = directory.join(format!("{token}.toml"));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            set_private_mode(&mut options);
            match options.open(&path) {
                Ok(_) => return Ok((token, path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("creating {}", path.display()))
                }
            }
        }
        anyhow::bail!(
            "could not allocate a unique deletion token in {}",
            directory.display()
        )
    }

    fn resolve_trash_path(&self, input: &Path) -> Result<PathBuf> {
        let trash_directory = self.trash_dir();
        let mut candidate = input.to_path_buf();
        if input.components().count() == 1 {
            let mut file_name = OsString::from(input.as_os_str());
            if input.extension().is_none() {
                file_name.push(".toml");
            }
            candidate = trash_directory.join(file_name);
        }
        if candidate.parent() != Some(trash_directory.as_path()) {
            anyhow::bail!(
                "deletion path must be a direct child of {}",
                trash_directory.display()
            );
        }
        if candidate
            .extension()
            .is_none_or(|extension| extension != "toml")
        {
            anyhow::bail!("deletion path must identify a TOML trash entry");
        }
        Ok(candidate)
    }
}

/// Resolve the default preset directory under the XDG Base Directory rules.
///
/// Relative XDG variables are ignored.  This is important because joining a
/// relative value to `peq/presets` would otherwise put the library under the
/// process's current directory.  The fallback is `$HOME/.config/peq/presets`.
pub fn xdg_preset_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/"));
    xdg_preset_dir_from(std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from), home)
}

/// Resolve a preset directory from explicit values.  This pure helper is handy
/// for tests and avoids requiring tests to mutate process-wide environment.
pub fn xdg_preset_dir_from(config_home: Option<PathBuf>, home: PathBuf) -> PathBuf {
    let base = config_home
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    base.join("peq/presets")
}

/// Write bytes to a path using a same-directory temporary file and atomic rename.
///
/// This helper is intentionally path-oriented and has no preset-library lookup;
/// daemon state can use it for snapshots while [`PresetStore::save`] remains the
/// only API that writes library documents and performs CAS checks.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let temporary = create_temp(parent)?;
    let result = (|| {
        write_synced(&temporary.file, bytes)?;
        fs::rename(&temporary.path, path).with_context(|| {
            format!(
                "renaming {} to {}",
                temporary.path.display(),
                path.display()
            )
        })?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary.path);
    }
    result
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write(path, bytes)
}

fn serialize_document(document: &PresetDocument) -> Result<Vec<u8>> {
    Ok(toml::to_string_pretty(document)
        .context("serializing preset")?
        .into_bytes())
}

fn ensure_expected(name: &str, expected: Revision, actual: Revision) -> Result<()> {
    if actual != expected {
        return Err(RevisionConflict {
            name: name.to_string(),
            expected,
            actual,
        }
        .into());
    }
    Ok(())
}

fn ensure_absent(path: &Path, name: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(PresetAlreadyExists {
            name: name.to_string(),
            path: path.to_path_buf(),
        }
        .into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("checking {}", path.display())),
    }
}

fn collect_entries(read_dir: ReadDir, listing: &mut PresetListing) {
    for item in read_dir {
        let entry = match item {
            Ok(entry) => entry,
            Err(error) => {
                listing.diagnostics.push(StoreDiagnostic {
                    path: PathBuf::new(),
                    message: format!("reading directory entry: {error}"),
                });
                continue;
            }
        };
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "toml") {
            continue;
        }
        let stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Err(error) = validation::validate_name(&stem) {
            add_diagnostic(listing, path, error.to_string());
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                add_diagnostic(listing, path, format!("reading file: {error}"));
                continue;
            }
        };
        let document = match parse_document(&bytes, &path) {
            Ok(document) => document,
            Err(error) => {
                add_diagnostic(listing, path, error.to_string());
                continue;
            }
        };
        if document.name != stem {
            add_diagnostic(
                listing,
                path,
                format!(
                    "document name '{}' does not match file name '{stem}'",
                    document.name
                ),
            );
            continue;
        }
        listing.entries.push(LoadedPreset {
            document,
            revision: revision(&bytes),
        });
    }
}

fn add_diagnostic(listing: &mut PresetListing, path: PathBuf, message: String) {
    listing.diagnostics.push(StoreDiagnostic { path, message });
}

fn parse_document(bytes: &[u8], path: &Path) -> Result<PresetDocument> {
    let text = std::str::from_utf8(bytes)
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
    let document: PresetDocument =
        toml::from_str(text).with_context(|| format!("parsing {}", path.display()))?;
    validation::validate_preset(&document, None)
        .with_context(|| format!("validating {}", path.display()))?;
    Ok(document)
}

struct TemporaryFile {
    path: PathBuf,
    file: File,
}

fn create_temp(parent: &Path) -> Result<TemporaryFile> {
    for _ in 0..100 {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let name = format!(".peq-tmp-{}-{id}", stamp);
        let path = parent.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        set_private_mode(&mut options);
        match options.open(&path) {
            Ok(file) => return Ok(TemporaryFile { path, file }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", path.display()))
            }
        }
    }
    anyhow::bail!(
        "could not allocate a unique temporary file in {}",
        parent.display()
    )
}

fn write_synced(file: &File, bytes: &[u8]) -> Result<()> {
    let mut file = file;
    file.write_all(bytes).context("writing temporary file")?;
    file.flush().context("flushing temporary file")?;
    file.sync_all().context("syncing temporary file")
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("opening directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("syncing directory {}", path.display()))
}

fn set_private_mode(options: &mut OpenOptions) {
    #[cfg(unix)]
    options.mode(0o600);
}

/// A lock file held from CAS read through final rename.
pub(crate) struct StoreLock {
    path: PathBuf,
}

impl StoreLock {
    pub(crate) fn acquire(root: &Path) -> Result<Self> {
        let path = root.join(".peq-store.lock");
        for _ in 0..100 {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            set_private_mode(&mut options);
            match options.open(&path) {
                Ok(mut file) => {
                    let _ = writeln!(file, "pid={}", std::process::id());
                    let _ = file.sync_all();
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("creating lock {}", path.display()))
                }
            }
        }
        Err(StoreLocked { path }.into())
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

// A small dependency-free 256-bit content hash.  Four independent FNV-1a
// streams are sufficient for an opaque CAS token and work across processes.
fn revision(bytes: &[u8]) -> Revision {
    let mut hashes = [
        0xcbf29ce484222325u64,
        0x84222325cbf29ce4u64,
        0x9e3779b185ebca87u64,
        0xd6e8feb86659fd93u64,
    ];
    for (index, byte) in bytes.iter().copied().enumerate() {
        for (stream, hash) in hashes.iter_mut().enumerate() {
            *hash ^= u64::from(byte).wrapping_add((index as u64).rotate_left(stream as u32));
            *hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    let mut digest = [0u8; 32];
    for (offset, hash) in hashes.into_iter().enumerate() {
        digest[offset * 8..offset * 8 + 8].copy_from_slice(&hash.to_le_bytes());
    }
    Revision(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::BandType;

    fn temp_root(label: &str) -> PathBuf {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("peq-storage-{label}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn document(name: &str, gain: f64) -> PresetDocument {
        let mut document = PresetDocument::new(name);
        document.bands.push(crate::preset::Band {
            kind: BandType::Peaking,
            freq: 1_000.0,
            gain,
            q: 1.0,
            enabled: true,
        });
        document
    }

    #[test]
    fn create_load_replace_and_backup_roundtrip() {
        let root = temp_root("roundtrip");
        let store = PresetStore::new(&root);
        let first = document("test", 1.0);
        let first_revision = store.create(&first).unwrap();
        assert_eq!(store.load("test").unwrap().revision, first_revision);

        let second = document("test", 2.0);
        let second_revision = store.replace(&second, first_revision).unwrap();
        assert_ne!(first_revision, second_revision);
        assert_eq!(store.load_backup("test").unwrap().document, first);
        assert_eq!(store.load("test").unwrap().document, second);
        assert!(store.create(&second).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_entries_are_diagnosed_without_hiding_valid_entries() {
        let root = temp_root("listing");
        let store = PresetStore::new(&root);
        store.create(&document("good", 1.0)).unwrap();
        fs::write(root.join("bad.toml"), b"this is not TOML = [").unwrap();
        fs::write(root.join("good.toml.bak"), b"ignored").unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.entries.len(), 1);
        assert_eq!(listed.entries[0].document.name, "good");
        assert_eq!(listed.diagnostics.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn relative_xdg_value_falls_back_to_home() {
        let home = PathBuf::from("/tmp/peq-home");
        assert_eq!(
            xdg_preset_dir_from(Some(PathBuf::from("relative")), home.clone()),
            home.join(".config/peq/presets")
        );
        assert_eq!(
            xdg_preset_dir_from(Some(PathBuf::from("/tmp/config")), home),
            PathBuf::from("/tmp/config/peq/presets")
        );
    }

    #[test]
    fn concurrent_replacements_have_one_winner() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let root = temp_root("concurrent");
        let first_store = PresetStore::new(&root);
        let revision = first_store.create(&document("test", 0.0)).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for gain in [1.0, 2.0] {
            let barrier = Arc::clone(&barrier);
            let root = root.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                PresetStore::new(root).replace(&document("test", gain), revision)
            }));
        }
        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_backup_leaves_previous_bytes_untouched() {
        let root = temp_root("failure");
        let store = PresetStore::new(&root);
        let first = document("test", 1.0);
        let revision = store.create(&first).unwrap();
        let before = fs::read(root.join("test.toml")).unwrap();
        fs::create_dir_all(root.join(".backups")).unwrap();
        fs::create_dir(store.backup_path("test").unwrap()).unwrap();
        assert!(store.replace(&document("test", 2.0), revision).is_err());
        assert_eq!(fs::read(root.join("test.toml")).unwrap(), before);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn duplicate_copies_content_with_a_new_identity_and_rejects_collision() {
        let root = temp_root("duplicate");
        let store = PresetStore::new(&root);
        let mut source = document("source", 1.0);
        source.favorite = true;
        store.create(&source).unwrap();

        let duplicate_revision = store.duplicate("source", "copy").unwrap();
        let duplicate = store.load("copy").unwrap();
        assert_eq!(duplicate.revision, duplicate_revision);
        assert_eq!(duplicate.document.name, "copy");
        assert_eq!(duplicate.document.bands, source.bands);
        assert!(duplicate.document.favorite);
        assert_ne!(duplicate.document.id, source.id);
        assert_eq!(store.load("source").unwrap().document, source);

        assert!(store.duplicate("source", "copy").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rename_checks_revision_keeps_identity_and_backs_up_source() {
        let root = temp_root("rename");
        let store = PresetStore::new(&root);
        let source = document("source", 1.0);
        let first_revision = store.create(&source).unwrap();

        let changed_revision = store.set_favorite("source", first_revision, true).unwrap();
        assert!(store.rename("source", "renamed", first_revision).is_err());
        assert!(store.load("source").is_ok());

        let renamed_revision = store.rename("source", "renamed", changed_revision).unwrap();
        let renamed = store.load("renamed").unwrap();
        assert_eq!(renamed.revision, renamed_revision);
        assert_eq!(renamed.document.id, source.id);
        assert_eq!(renamed.document.name, "renamed");
        assert!(renamed.document.favorite);
        assert!(store.load("source").is_err());
        assert_eq!(store.load_backup("source").unwrap().document.name, "source");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rename_failure_preserves_source_file() {
        let root = temp_root("rename-failure");
        let store = PresetStore::new(&root);
        let source = document("source", 1.0);
        let revision = store.create(&source).unwrap();
        // A directory at the destination exercises the preflight collision
        // path before any source removal or backup mutation.
        fs::create_dir_all(root.join("destination.toml")).unwrap();

        assert!(store.rename("source", "destination", revision).is_err());
        assert_eq!(store.load("source").unwrap().document, source);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remove_returns_unique_trash_entry_and_restore_is_collision_safe() {
        let root = temp_root("remove-restore");
        let store = PresetStore::new(&root);
        let source = document("recover", 1.0);
        let revision = store.create(&source).unwrap();

        let stale = Revision([0; 32]);
        assert!(store.remove("recover", stale).is_err());
        assert!(store.load("recover").is_ok());

        let deleted = store.remove("recover", revision).unwrap();
        assert_eq!(deleted.name, "recover");
        assert!(!deleted.token.is_empty());
        assert!(deleted.path.starts_with(root.join(".trash")));
        assert!(deleted.path.exists());
        assert!(store.load("recover").is_err());

        // An explicit restore refuses to replace a newly-created preset.
        let replacement = document("recover", 9.0);
        store.create(&replacement).unwrap();
        assert!(store.restore(&deleted).is_err());
        assert!(deleted.path.exists());
        assert_eq!(store.load("recover").unwrap().document, replacement);

        fs::remove_file(root.join("recover.toml")).unwrap();
        let restored_revision = store.restore(&deleted.token).unwrap();
        let restored = store.load("recover").unwrap();
        assert_eq!(restored.revision, restored_revision);
        assert_eq!(restored.document, source);
        assert!(!deleted.path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn favorite_update_persists_and_rejects_stale_revision() {
        let root = temp_root("favorite");
        let store = PresetStore::new(&root);
        let preset = document("favorite", 1.0);
        let first_revision = store.create(&preset).unwrap();
        let favorite_revision = store
            .set_favorite("favorite", first_revision, true)
            .unwrap();
        let favorite = store.load("favorite").unwrap();
        assert_eq!(favorite.revision, favorite_revision);
        assert!(favorite.document.favorite);
        assert!(store
            .set_favorite("favorite", first_revision, false)
            .is_err());

        let plain_revision = store
            .set_favorite("favorite", favorite_revision, false)
            .unwrap();
        assert!(!store.load("favorite").unwrap().document.favorite);
        assert_ne!(plain_revision, favorite_revision);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn listing_ignores_backup_and_trash_subdirectories() {
        let root = temp_root("listing-subdirs");
        let store = PresetStore::new(&root);
        store.create(&document("good", 1.0)).unwrap();
        fs::create_dir_all(root.join(".backups")).unwrap();
        fs::create_dir_all(root.join(".trash")).unwrap();
        fs::write(root.join(".backups/bad.toml"), b"invalid").unwrap();
        fs::write(root.join(".trash/bad.toml"), b"invalid").unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.entries.len(), 1);
        assert!(listed.diagnostics.is_empty());
        let _ = fs::remove_dir_all(root);
    }
}
