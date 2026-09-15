use crate::{
    FORMAT_VERSION,
    catalog::Catalog,
    config::{Config, ResourceOverrides},
    diagnostic::{DbError, Diagnostic, Result},
    integrity,
    metadata::{self, Manifest},
};
use fs2::FileExt;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// What an observation is permitted to persist.
///
/// Repairing derived state and recording an accepted revision are independent
/// permissions, because they answer to different rules. Rebuilding an index is
/// derived work a diagnosis must not perform -- a command that reports what is
/// wrong cannot alter what it reports on. Recording a valid external change is
/// authoritative: it is the system's central promise, and a command that
/// observed such a change without accepting it would leave the database
/// permanently behind its own files.
///
/// Collapsing the two into one switch made every diagnostic silently stop
/// adopting external edits, so they are kept apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObserveMode {
    /// Rebuild a corrupt manifest or stale indexes when the state is valid.
    pub repair_derived: bool,
    /// Record an observed, valid external transition as a new revision.
    pub record_provenance: bool,
}

impl ObserveMode {
    /// Ordinary operation: establish and record whatever the state requires.
    pub const RECORD: Self = Self {
        repair_derived: true,
        record_provenance: true,
    };

    /// Diagnosis: accept valid external changes, but leave derived state
    /// exactly as found so the report describes the folder as it was.
    pub const DIAGNOSE: Self = Self {
        repair_derived: false,
        record_provenance: true,
    };

    /// Read-only: persist nothing at all.
    pub const READ_ONLY: Self = Self {
        repair_derived: false,
        record_provenance: false,
    };

    /// Whether this observation may take the writer lock. Only a mode that can
    /// persist something needs it.
    fn may_write(self) -> bool {
        self.repair_derived || self.record_provenance
    }
}
pub struct Database {
    pub root: PathBuf,
    pub config: Config,
    pub catalog: Catalog,
    pub diagnostics: Vec<Diagnostic>,
    pub manifest: Option<Manifest>,
    pub external_changes: Vec<String>,
    pub validation_elapsed: std::time::Duration,
    pub manifest_needs_rebuild: bool,
    resource_overrides: ResourceOverrides,
}

impl Database {
    pub fn open(root: PathBuf, mode: ObserveMode) -> Result<Self> {
        Self::open_with_overrides(root, mode, &ResourceOverrides::default())
    }

    /// Build a database entirely in memory from schemas that were never written.
    ///
    /// A folder of JSON with no `.db/` is still a readable relational database:
    /// the schemas that describe it can be inferred without persisting them, and
    /// a read can then be answered against the files as they are. Read-only
    /// operation promises zero writes, so it cannot bootstrap -- but refusing to
    /// answer at all would make `--readonly` useless on exactly the folders it
    /// most needs to inspect.
    ///
    /// The result carries no manifest and records no provenance, because nothing
    /// was accepted: this is an observation, not a revision.
    pub fn ephemeral(
        root: PathBuf,
        schemas: std::collections::BTreeMap<String, crate::schema::Schema>,
        overrides: &ResourceOverrides,
    ) -> Result<Self> {
        let validation_started = std::time::Instant::now();
        // An absent `.db/config` yields defaults; one that exists but cannot be
        // read is a fault the user must see. Falling back to defaults here would
        // answer the query under settings they never chose.
        let mut config = load_config(&root)?;
        config.apply_overrides(overrides);
        config.validate().map_err(|message| {
            DbError::new(
                "RESOURCE_LIMIT",
                format!("invalid command-line resource limit: {message}"),
                1,
            )
        })?;
        let catalog = Catalog::observe_with_schemas(&root, &config, schemas)?;
        let diagnostics = integrity::validate(&catalog);
        Ok(Self {
            root,
            config,
            catalog,
            diagnostics,
            manifest: None,
            external_changes: vec![],
            validation_elapsed: validation_started.elapsed(),
            manifest_needs_rebuild: false,
            resource_overrides: overrides.clone(),
        })
    }

    pub fn open_with_overrides(
        root: PathBuf,
        mode: ObserveMode,
        overrides: &ResourceOverrides,
    ) -> Result<Self> {
        let validation_started = std::time::Instant::now();
        validate_format(&root)?;
        let writer_lock = if mode.may_write() {
            let path = root.join(".db/lock");
            validate_optional_private_file(&path, "lock")?;
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|error| DbError::io(&path, error))?;
            file.try_lock_exclusive().map_err(|_| {
                DbError::new(
                    "CONCURRENT_MODIFICATION",
                    "another writer is validating or changing the database",
                    3,
                )
            })?;
            Some(file)
        } else {
            None
        };
        let pending = crate::transaction::has_pending(&root)?;
        let recovered = if mode.may_write() {
            recover(&root)?
        } else {
            false
        };
        if recovered {
            crate::output::notice_stderr("recovered an interrupted committed transaction");
        }
        if recovered {
            metadata::reconcile_after_recovery(&root)?;
        }
        let mut config = load_config(&root)?;
        config.apply_overrides(overrides);
        config.validate().map_err(|message| {
            DbError::new(
                "RESOURCE_LIMIT",
                format!("invalid command-line resource limit: {message}"),
                1,
            )
        })?;
        let mut catalog = Catalog::observe(&root, &config)?;
        let mut diagnostics = integrity::validate(&catalog);
        if pending && !recovered {
            diagnostics.insert(
                0,
                Diagnostic::error(
                    "TRANSACTION_INCOMPLETE",
                    "pending transaction requires recovery; no-write mode left it untouched",
                )
                .help("run `db recover` with write access"),
            );
        }
        let mut manifest_rebuild = false;
        let loaded = match metadata::load_manifest(&root) {
            Ok(v) => v,
            Err(e) if e.diagnostic.code == "INTERNAL_METADATA_CORRUPT" => {
                manifest_rebuild = true;
                None
            }
            Err(e) => return Err(e),
        };
        let old = match loaded {
            some @ Some(_) => some,
            None => metadata::provenance_head(&root)?,
        };
        metadata::validate_provenance(&root, old.as_ref())?;
        let (hash, entries) = metadata::state(&catalog)?;
        if manifest_rebuild && mode.repair_derived {
            if let Some(head) = &old {
                metadata::write_manifest(&root, head)?;
                crate::output::notice_stderr("rebuilt corrupt derived manifest");
            }
        } else if manifest_rebuild {
            catalog.warnings.push(Diagnostic::warning(
                "MANIFEST_STALE",
                "derived manifest is corrupt and was not rebuilt in no-write mode",
            ));
        }
        let indexes_valid = crate::index::valid(&root, &catalog)?;
        if !indexes_valid {
            if mode.repair_derived && diagnostics.is_empty() {
                crate::index::rebuild(&root, &catalog)?;
                crate::output::notice_stderr("rebuilt stale or corrupt indexes");
            } else {
                catalog.warnings.push(Diagnostic::warning(
                    "INDEX_STALE",
                    "derived indexes are missing, stale, or corrupt",
                ));
            }
        }
        let external_changes = if old.as_ref().is_some_and(|m| m.root_hash == hash) {
            vec![]
        } else {
            metadata::diff_entries(old.as_ref().map(|m| &m.entries), &entries)
        };
        if !external_changes.is_empty() && !mode.record_provenance {
            catalog.warnings.push(Diagnostic::warning(
                "METADATA_STALE_READONLY",
                "authoritative state is valid but differs from recorded metadata; read-only mode did not record it",
            ));
        }
        let manifest =
            if diagnostics.is_empty() && !external_changes.is_empty() && mode.record_provenance {
                Some(metadata::record(
                    &catalog,
                    old.as_ref(),
                    hash,
                    entries,
                    if recovered { "recovery" } else { "external" },
                    None,
                )?)
            } else {
                old
            };
        if recovered && diagnostics.is_empty() {
            crate::transaction::finalize_recovered(&root)?;
        }
        let manifest_needs_rebuild =
            manifest_rebuild && !(mode.repair_derived && diagnostics.is_empty());
        let database = Self {
            root,
            config,
            catalog,
            diagnostics,
            manifest,
            external_changes,
            validation_elapsed: validation_started.elapsed(),
            manifest_needs_rebuild,
            resource_overrides: overrides.clone(),
        };
        drop(writer_lock);
        Ok(database)
    }
    pub fn require_valid(&self) -> Result<()> {
        if let Some(d) = self.diagnostics.first() {
            return Err(DbError::from_diag(
                d.clone(),
                crate::diagnostic::exit_code_for_diagnostics(&self.diagnostics),
            ));
        }
        Ok(())
    }
    pub fn refresh(&mut self, mode: ObserveMode) -> Result<()> {
        *self = Self::open_with_overrides(self.root.clone(), mode, &self.resource_overrides)?;
        Ok(())
    }
    pub fn resource_overrides(&self) -> &ResourceOverrides {
        &self.resource_overrides
    }
}

pub fn validate_format(root: &Path) -> Result<()> {
    let meta = root.join(".db");
    match fs::symlink_metadata(&meta) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(DbError::from_diag(
                Diagnostic::error(
                    "UNINITIALIZED",
                    format!("{} has no .db metadata", root.display()),
                )
                .help(format!(
                    "run `db init {}` or `db init {} --adopt`",
                    root.display(),
                    root.display()
                )),
                10,
            ));
        }
        Err(error) => return Err(DbError::io(&meta, error)),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                ".db must be a real directory, not a symlink or special file",
                6,
            ));
        }
        Ok(_) => {}
    }
    let p = meta.join("format");
    require_private_regular_file(&p, "format marker")?;
    let format_size = fs::symlink_metadata(&p)
        .map_err(|error| DbError::io(&p, error))?
        .len();
    if format_size > 4096 {
        return Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            ".db/format exceeds the 4096-byte format marker limit",
            6,
        ));
    }
    let text = fs::read_to_string(&p).map_err(|e| DbError::io(&p, e))?;
    let found = text
        .lines()
        .find_map(|l| l.strip_prefix("format_version = "))
        .and_then(|x| x.parse::<u32>().ok())
        .ok_or_else(|| {
            DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                ".db/format has no valid format_version",
                6,
            )
        })?;
    if found != FORMAT_VERSION {
        return Err(DbError::new(
            "FORMAT_UNSUPPORTED",
            format!("format {found} is unsupported; this binary supports format {FORMAT_VERSION}"),
            6,
        ));
    }
    Ok(())
}
pub fn load_config(root: &Path) -> Result<Config> {
    let p = root.join(".db/config");
    if !p.exists() {
        return Ok(Config::default());
    }
    require_private_regular_file(&p, "configuration")?;
    let config_size = fs::symlink_metadata(&p)
        .map_err(|error| DbError::io(&p, error))?
        .len();
    if config_size > crate::config::BOOTSTRAP_MAX_CONFIG_SIZE {
        return Err(DbError::new(
            "CONFIG_INVALID",
            format!(
                ".db/config exceeds the {} byte bootstrap limit",
                crate::config::BOOTSTRAP_MAX_CONFIG_SIZE
            ),
            1,
        ));
    }
    let b = fs::read(&p).map_err(|e| DbError::io(&p, e))?;
    let value = crate::json::parse(&b)
        .map_err(|e| DbError::new("CONFIG_INVALID", format!("invalid .db/config: {e}"), 1))?;
    let config: Config = serde_json::from_value(value)
        .map_err(|e| DbError::new("CONFIG_INVALID", format!("invalid .db/config: {e}"), 1))?;
    config.validate().map_err(|message| {
        DbError::new(
            "CONFIG_INVALID",
            format!("invalid .db/config: {message}"),
            1,
        )
    })?;
    Ok(config)
}

fn require_private_regular_file(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("cannot inspect {description} {}: {error}", path.display()),
            6,
        )
    })?;
    if !metadata.file_type().is_file() || has_multiple_links(&metadata) {
        return Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!(
                "{description} {} must be a private regular file",
                path.display()
            ),
            6,
        ));
    }
    Ok(())
}

fn validate_optional_private_file(path: &Path, description: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => require_private_regular_file(path, description),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DbError::io(path, error)),
    }
}

#[cfg(unix)]
fn has_multiple_links(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() > 1
}

#[cfg(not(unix))]
fn has_multiple_links(_metadata: &fs::Metadata) -> bool {
    false
}
pub fn init_layout(root: &Path, track_provenance: bool) -> Result<()> {
    if root.join(".db").exists() {
        return Err(DbError::new(
            "ALREADY_INITIALIZED",
            format!("{} is already initialized", root.display()),
            1,
        ));
    }
    fs::create_dir_all(root).map_err(|e| DbError::io(root, e))?;
    if !root.join("schema").exists() {
        fs::create_dir(root.join("schema")).map_err(|e| DbError::io(&root.join("schema"), e))?
    }
    fs::create_dir(root.join(".db")).map_err(|e| DbError::io(&root.join(".db"), e))?;
    for d in [
        "provenance",
        "indexes",
        "statistics",
        "transactions",
        "snapshots",
    ] {
        fs::create_dir(root.join(".db").join(d))
            .map_err(|e| DbError::io(&root.join(".db").join(d), e))?
    }
    fs::write(
        root.join(".db/format"),
        format!("format_version = {FORMAT_VERSION}\n"),
    )
    .map_err(|e| DbError::io(&root.join(".db/format"), e))?;
    metadata::write_json_atomic(&root.join(".db/config"), &Config::default())?;
    let ignore = if track_provenance {
        "*\n!format\n!config\n!provenance/\n!provenance/**\n!objects/\n!objects/**\n"
    } else {
        "*\n!format\n!config\n"
    };
    fs::write(root.join(".db/.gitignore"), ignore)
        .map_err(|e| DbError::io(&root.join(".db/.gitignore"), e))?;
    Ok(())
}
pub fn init_empty(root: &Path, track_provenance: bool) -> Result<()> {
    init_layout(root, track_provenance)?;
    let c = Catalog::observe(root, &Config::default())?;
    let (hash, entries) = metadata::state(&c)?;
    metadata::record(&c, None, hash, entries, "import", None)?;
    // An empty database derives no indexes, so this writes nothing today. It is
    // here because the rule is that establishment leaves derived state
    // complete, not that it does so when the result happens to be empty: one
    // invariant across every path that creates a database.
    crate::index::rebuild(root, &c)?;
    Ok(())
}
pub fn write_schema(root: &Path, s: &crate::schema::Schema) -> Result<()> {
    metadata::write_json_atomic(&root.join("schema").join(format!("{}.json", s.table)), s)
}
pub fn json_key_arg(text: &str) -> Result<Value> {
    serde_json::from_str(text)
        .or_else(|_| Ok(Value::String(text.into())))
        .map_err(|_: serde_json::Error| DbError::usage("invalid key"))
}
pub fn recover(root: &Path) -> Result<bool> {
    crate::transaction::recover(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Section 29: a primary key given on the command line is decoded against
    /// the value it denotes. JSON syntax wins where it parses, so an integer key
    /// arrives as a number, and bare text is taken as a string rather than being
    /// rejected -- `db get users abc` must work without shell quoting games.
    #[test]
    fn test1021_primary_key_arguments_decode_json_then_fall_back_to_text() {
        assert_eq!(json_key_arg("123").unwrap(), Value::from(123));
        assert_eq!(json_key_arg("1.5").unwrap(), Value::from(1.5));
        assert_eq!(json_key_arg("true").unwrap(), Value::Bool(true));
        assert_eq!(json_key_arg("null").unwrap(), Value::Null);
        assert_eq!(
            json_key_arg("\"quoted\"").unwrap(),
            Value::String("quoted".into())
        );

        // Text that is not JSON is the string it looks like.
        for text in ["abc", "u1", "not json", "2026-09-14", ""] {
            assert_eq!(
                json_key_arg(text).unwrap(),
                Value::String(text.into()),
                "{text:?} should decode as a string"
            );
        }

        // A structured key round-trips as structure.
        assert_eq!(json_key_arg("[1,2]").unwrap(), serde_json::json!([1, 2]));
    }

    /// Root resolution never walks upward from a path the user named: operating
    /// on a different database than the one they pointed at would be a surprise
    /// no diagnostic could undo.
    #[test]
    fn test1022_an_explicitly_named_root_is_used_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("child");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(dir.path().join(".db")).unwrap();

        let resolved = crate::state::resolve_root(Some(&nested)).unwrap();
        assert!(resolved.path.is_absolute());
        assert_eq!(
            resolved.path, nested,
            "the named directory is the root, even though an ancestor has .db"
        );
    }

    /// Section 65: the on-disk format is explicitly versioned and an unsupported
    /// version is refused rather than interpreted optimistically.
    #[test]
    fn test1023_unsupported_formats_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        init_layout(dir.path(), false).unwrap();
        validate_format(dir.path()).expect("a freshly written format is supported");

        fs::write(
            dir.path().join(".db/format"),
            format!("format_version = {}\n", FORMAT_VERSION + 1),
        )
        .unwrap();
        let error = validate_format(dir.path()).unwrap_err();
        assert_eq!(error.diagnostic.code, "FORMAT_UNSUPPORTED");

        // Unreadable garbage is corrupt metadata, not a silently older format.
        fs::write(dir.path().join(".db/format"), "not a format\n").unwrap();
        assert!(validate_format(dir.path()).is_err());
    }

    /// Section 50: initialisation writes the metadata layout a clone needs, and
    /// ignores derived state so only authoritative files are versioned.
    #[test]
    fn test1024_initialisation_writes_the_documented_layout() {
        let dir = tempfile::tempdir().unwrap();
        init_layout(dir.path(), false).unwrap();
        for expected in [
            ".db",
            ".db/format",
            ".db/config",
            ".db/.gitignore",
            "schema",
        ] {
            assert!(
                dir.path().join(expected).exists(),
                "{expected} must be created"
            );
        }
        let ignore = fs::read_to_string(dir.path().join(".db/.gitignore")).unwrap();
        assert!(
            ignore.contains("format") && ignore.contains("config"),
            "format and config stay versioned: {ignore}"
        );

        // The written configuration is valid and parses back.
        let config = load_config(dir.path()).unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.indentation_width, 2);
    }

    /// Section 50: `--track-provenance` opts history into version control, which
    /// is a different ignore policy from the default.
    #[test]
    fn test1025_tracked_provenance_changes_the_ignore_policy() {
        let default_dir = tempfile::tempdir().unwrap();
        init_layout(default_dir.path(), false).unwrap();
        let default_ignore = fs::read_to_string(default_dir.path().join(".db/.gitignore")).unwrap();

        let tracked_dir = tempfile::tempdir().unwrap();
        init_layout(tracked_dir.path(), true).unwrap();
        let tracked_ignore = fs::read_to_string(tracked_dir.path().join(".db/.gitignore")).unwrap();

        assert_ne!(
            default_ignore, tracked_ignore,
            "tracking provenance must change what is ignored"
        );
        assert!(tracked_ignore.contains("provenance"));
    }
}
