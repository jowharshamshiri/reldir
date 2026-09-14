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
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveMode {
    Record,
    NoWrite,
    ReadOnly,
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
    pub fn discover(explicit: Option<&Path>) -> Result<PathBuf> {
        if let Some(p) = explicit {
            return abs(p);
        }
        if let Ok(p) = env::var("DB_DIR") {
            return abs(Path::new(&p));
        }
        let mut cur = env::current_dir().map_err(|e| DbError::io(Path::new("."), e))?;
        let mut tried = vec![];
        loop {
            tried.push(cur.display().to_string());
            if cur.join(".db").is_dir() {
                return Ok(cur);
            }
            if !cur.pop() {
                break;
            }
        }
        Err(DbError::from_diag(
            Diagnostic::error(
                "UNINITIALIZED",
                format!("no .db directory found; searched {}", tried.join(", ")),
            )
            .help("run `db init` or `db init --adopt`"),
            10,
        ))
    }
    pub fn open(root: PathBuf, mode: ObserveMode) -> Result<Self> {
        Self::open_with_overrides(root, mode, &ResourceOverrides::default())
    }

    pub fn open_with_overrides(
        root: PathBuf,
        mode: ObserveMode,
        overrides: &ResourceOverrides,
    ) -> Result<Self> {
        let validation_started = std::time::Instant::now();
        validate_format(&root)?;
        let writer_lock = if mode == ObserveMode::Record {
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
        let recovered = if mode == ObserveMode::Record {
            recover(&root)?
        } else {
            false
        };
        if recovered {
            eprintln!("recovered an interrupted committed transaction");
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
        if manifest_rebuild && mode == ObserveMode::Record {
            if let Some(head) = &old {
                metadata::write_manifest(&root, head)?;
                eprintln!("rebuilt corrupt derived manifest");
            }
        } else if manifest_rebuild {
            catalog.warnings.push(Diagnostic::warning(
                "MANIFEST_STALE",
                "derived manifest is corrupt and was not rebuilt in no-write mode",
            ));
        }
        let indexes_valid = crate::index::valid(&root, &catalog)?;
        if !indexes_valid {
            if mode == ObserveMode::Record && diagnostics.is_empty() {
                crate::index::rebuild(&root, &catalog)?;
                eprintln!("rebuilt stale or corrupt indexes");
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
        if !external_changes.is_empty() && mode != ObserveMode::Record {
            catalog.warnings.push(Diagnostic::warning(
                "METADATA_STALE_READONLY",
                "authoritative state is valid but differs from recorded metadata; read-only mode did not record it",
            ));
        }
        let manifest = if diagnostics.is_empty()
            && !external_changes.is_empty()
            && mode == ObserveMode::Record
        {
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
            manifest_rebuild && !(mode == ObserveMode::Record && diagnostics.is_empty());
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
    let b = fs::read(&p).map_err(|e| DbError::io(&p, e))?;
    let value = crate::json::parse(&b).map_err(|e| {
        DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("invalid .db/config: {e}"),
            6,
        )
    })?;
    let config: Config = serde_json::from_value(value).map_err(|e| {
        DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("invalid .db/config: {e}"),
            6,
        )
    })?;
    config.validate().map_err(|message| {
        DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("invalid .db/config: {message}"),
            6,
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
            format!("{description} {} must be a private regular file", path.display()),
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
fn abs(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(env::current_dir()
            .map_err(|e| DbError::io(Path::new("."), e))?
            .join(p))
    }
}
pub fn recover(root: &Path) -> Result<bool> {
    crate::transaction::recover(root)
}
