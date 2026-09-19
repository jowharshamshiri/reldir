use crate::{
    catalog::Catalog,
    config::Config,
    diagnostic::{DbError, Diagnostic, Result},
    integrity, metadata,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone)]
pub enum Change {
    Write { path: PathBuf, bytes: Vec<u8> },
    Delete { path: PathBuf },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// What recovery needs to finish or discard an interrupted transaction.
///
/// It records the changes and nothing about the state they were planned
/// against: recovery rolls a marked journal forward from its immutable staged
/// bytes and discards an unmarked one, and neither decision consults the
/// starting state. The journal used to carry the database's root hash at
/// planning time, which nothing ever read -- and once the conflict check became
/// per-path there was no longer a single root it could honestly name.
struct Journal {
    id: String,
    origin: String,
    changes: Vec<JournalChange>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalChange {
    path: PathBuf,
    stage: Option<String>,
}

/// Commit a planned mutation.
///
/// `start_entries` is what the caller observed for the paths it is about to
/// write, taken from the same observation that produced the plan. It is not the
/// whole database: a writer is only entitled to assume that the files it
/// touches have not moved, and holding it to the state of every other file
/// meant that any commit landing while it queued for the lock refused it for a
/// change it had no stake in.
///
/// Concretely: six writers inserting six different rows would serialise on the
/// lock, and the last five would each find the database's root hash advanced by
/// the ones before them and abort, naming no path they actually collided with.
pub fn commit(
    root: &Path,
    config: &Config,
    start_entries: &std::collections::BTreeMap<String, metadata::ManifestEntry>,
    changes: &[Change],
    origin: &str,
    dry_run: bool,
    resource_overrides: &crate::config::ResourceOverrides,
) -> Result<Vec<PathBuf>> {
    if changes.is_empty() {
        return Ok(vec![]);
    }
    let mut mutation_paths = std::collections::BTreeSet::new();
    for change in changes {
        let path = match change {
            Change::Write { path, .. } | Change::Delete { path } => path,
        };
        if !mutation_paths.insert(path.clone()) {
            return Err(DbError::new(
                "MUTATION_CONFLICT",
                format!(
                    "mutation plan contains the path {} more than once",
                    path.display()
                ),
                2,
            ));
        }
    }
    let transaction_bytes: u64 = changes
        .iter()
        .map(|c| match c {
            Change::Write { bytes, .. } => bytes.len() as u64,
            Change::Delete { .. } => 0,
        })
        .sum();
    if transaction_bytes > config.max_transaction_size {
        return Err(DbError::new(
            "RESOURCE_LIMIT",
            format!(
                "transaction stages {transaction_bytes} bytes, exceeding the {} byte limit",
                config.max_transaction_size
            ),
            2,
        ));
    }
    for c in changes {
        safe_relative(match c {
            Change::Write { path, .. } | Change::Delete { path } => path,
        })?
    }
    if dry_run {
        // A dry run performs no writes, so it validates without contending for
        // the writer lock: refusing to report a plan because someone else is
        // mid-commit would make the plan unavailable exactly when it is wanted.
        validate_prospective(root, changes, resource_overrides)?;
        return Ok(changes
            .iter()
            .map(|c| match c {
                Change::Write { path, .. } | Change::Delete { path } => path.clone(),
            })
            .collect());
    }
    let lock_path = root.join(".db/lock");
    validate_lock_path(&lock_path)?;
    // Held until this function returns, which is what serialises writers: the
    // file owns the lock, so the guard's lifetime is the critical section.
    let lock = crate::lock::acquire(
        &lock_path,
        config.lock_budget(),
        crate::lock::Holder::Committing,
    )?;
    // Validation reads the database and builds a shadow of it. Under the lock,
    // because a concurrent commit moving files beneath a validating reader is
    // not a conflict it can report -- it is a torn observation, and the reader
    // would fail on a vanished path rather than saying who won.
    validate_prospective(root, changes, resource_overrides)?;
    let current = Catalog::observe(root, config)?;
    let (_, current_entries) = metadata::state(&current)?;
    if let Some(conflict) = touched_conflict(changes, start_entries, &current_entries) {
        return Err(DbError::from_diag(
            Diagnostic::error(
                "CONCURRENT_MODIFICATION",
                format!(
                    "authoritative files changed while the mutation was being planned: {}",
                    conflict.description
                ),
            )
            .expected(conflict.expected)
            .observed(conflict.observed),
            3,
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let transactions = root.join(".db/transactions");
    metadata::ensure_real_directory(&transactions, true, "transaction store")?;
    let dir = transactions.join(&id);
    let staged = dir.join("staged");
    fs::create_dir_all(&staged).map_err(|e| DbError::io(&staged, e))?;
    let mut jc = vec![];
    for (i, c) in changes.iter().enumerate() {
        match c {
            Change::Write { path, bytes } => {
                let name = format!("{i:08}");
                let p = staged.join(&name);
                let mut f = fs::File::create(&p).map_err(|e| DbError::io(&p, e))?;
                f.write_all(bytes).map_err(|e| DbError::io(&p, e))?;
                f.sync_all().map_err(|e| DbError::io(&p, e))?;
                jc.push(JournalChange {
                    path: path.clone(),
                    stage: Some(name),
                });
            }
            Change::Delete { path } => jc.push(JournalChange {
                path: path.clone(),
                stage: None,
            }),
        }
    }
    let journal = Journal {
        id: id.clone(),
        origin: origin.into(),
        changes: jc,
    };
    metadata::write_json_atomic(&dir.join("journal.json"), &journal)?;
    let rechecked = Catalog::observe(root, config)?;
    let (_, rechecked_entries) = metadata::state(&rechecked)?;
    if let Some(conflict) = touched_conflict(changes, start_entries, &rechecked_entries) {
        fs::remove_dir_all(&dir).map_err(|e| DbError::io(&dir, e))?;
        return Err(DbError::from_diag(
            Diagnostic::error(
                "CONCURRENT_MODIFICATION",
                format!(
                    "authoritative files changed while the transaction was staged: {}",
                    conflict.description
                ),
            )
            .expected(conflict.expected)
            .observed(conflict.observed),
            3,
        ));
    }
    fs::write(dir.join("COMMITTING"), b"commit\n")
        .map_err(|e| DbError::io(&dir.join("COMMITTING"), e))?;
    metadata::sync_parent(&dir.join("COMMITTING"))?;
    apply_journal(root, &dir, &journal)?;
    // The rows are whole again the moment the last rename lands, so the marker
    // that tells readers "these files may be half-applied" comes off here --
    // not after the validation, index rebuild and provenance write below, which
    // touch no authoritative row. Leaving it on for that stretch made every
    // concurrent reader fail with TRANSACTION_INCOMPLETE against a database
    // whose rows were already complete and consistent.
    //
    // Recovery is unaffected: a crash before this point still finds COMMITTING
    // and rolls the journal forward idempotently from the staged bytes, and a
    // crash after it finds a journal without the marker, which is discarded
    // precisely because nothing remains to apply.
    let committing = dir.join("COMMITTING");
    fs::remove_file(&committing).map_err(|e| DbError::io(&committing, e))?;
    metadata::sync_parent(&committing)?;
    let committed_config = crate::db::load_config(root)?;
    let c = Catalog::observe(root, &committed_config)?;
    let errors = integrity::validate(&c);
    if !errors.is_empty() {
        return Err(DbError::new(
            "TRANSACTION_INCOMPLETE",
            "committed transaction materialized an invalid state; recovery required",
            5,
        ));
    }
    crate::index::rebuild(root, &c)?;
    let (hash, entries) = metadata::state(&c)?;
    let old = metadata::load_manifest(root)?;
    if old
        .as_ref()
        .is_none_or(|manifest| manifest.root_hash != hash)
    {
        metadata::record(&c, old.as_ref(), hash, entries, origin, Some(&id))?;
    }
    fs::write(dir.join("COMPLETE"), b"complete\n")
        .map_err(|e| DbError::io(&dir.join("COMPLETE"), e))?;
    if let Err(error) = fs::remove_dir_all(&dir) {
        eprintln!(
            "warning[TRANSACTION_CLEANUP]: committed transaction staging remains at {}: {error}",
            dir.display()
        );
    }
    drop(lock);
    Ok(changes
        .iter()
        .map(|c| match c {
            Change::Write { path, .. } | Change::Delete { path } => path.clone(),
        })
        .collect())
}

/// The first path this mutation touches that moved since it was planned.
struct TouchedConflict {
    description: String,
    expected: String,
    observed: String,
}

/// Whether a path this transaction writes or deletes has changed underneath it.
///
/// A writer's assumption is about the files it is going to replace, so that is
/// what is checked. Another commit landing on unrelated paths while this one
/// queued for the lock changes the database's root hash without invalidating
/// anything this transaction decided, and refusing it there produced a conflict
/// that named no colliding path because there was none.
///
/// A path absent from both observations is not a conflict: a writer creating a
/// row that still does not exist is exactly the case that must succeed.
fn touched_conflict(
    changes: &[Change],
    start: &std::collections::BTreeMap<String, metadata::ManifestEntry>,
    current: &std::collections::BTreeMap<String, metadata::ManifestEntry>,
) -> Option<TouchedConflict> {
    for change in changes {
        let path = match change {
            Change::Write { path, .. } | Change::Delete { path } => path,
        };
        let key = path.to_string_lossy().replace('\\', "/");
        let before = start.get(&key);
        let now = current.get(&key);
        let moved = match (before, now) {
            (Some(before), Some(now)) => before.hash != now.hash,
            // Appeared or vanished since the plan was made. Either way the
            // premise the caller wrote this change under no longer holds.
            (None, Some(_)) | (Some(_), None) => true,
            (None, None) => false,
        };
        if moved {
            return Some(TouchedConflict {
                description: key.clone(),
                expected: before.map_or_else(|| "absent".into(), |entry| entry.hash.clone()),
                observed: now.map_or_else(|| "absent".into(), |entry| entry.hash.clone()),
            });
        }
    }
    None
}

fn validate_prospective(
    root: &Path,
    changes: &[Change],
    resource_overrides: &crate::config::ResourceOverrides,
) -> Result<()> {
    // Prospective validation must be observational from the database's point of
    // view, including for --dry-run. Keeping the shadow outside the database
    // also prevents an interrupted validator from being mistaken for a pending
    // transaction.
    let parent = std::env::temp_dir();
    let temp = tempfile::Builder::new()
        .prefix("reldir-prospective-")
        .tempdir_in(&parent)
        .map_err(|e| DbError::io(&parent, e))?;
    let shadow = temp.path();
    fs::create_dir(shadow.join(".db")).map_err(|e| DbError::io(shadow, e))?;
    for name in ["format", "config"] {
        let source = root.join(".db").join(name);
        if source.exists() {
            fs::copy(&source, shadow.join(".db").join(name))
                .map_err(|e| DbError::io(&source, e))?;
        }
    }
    // Both schema locations are mirrored: the working copies the prospective
    // catalog validates against, and the pins whose agreement with them the
    // observation checks. Copying only one would make the shadow diverge from
    // the database it is standing in for.
    mirror_schema_directory(
        &crate::schema_store::working_dir(root),
        &crate::schema_store::working_dir(shadow),
    )?;
    mirror_schema_directory(
        &crate::schema_store::pin_dir(root),
        &crate::schema_store::pin_dir(shadow),
    )?;
    let config = crate::db::load_config(root)?;
    let c = Catalog::observe(root, &config)?;
    for table in c.schemas.keys() {
        let source = root.join(table);
        if !source.exists() {
            continue;
        }
        let source_metadata = fs::symlink_metadata(&source).map_err(|e| DbError::io(&source, e))?;
        if !source_metadata.file_type().is_dir() {
            fs::write(shadow.join(table), b"unsupported table path\n")
                .map_err(|e| DbError::io(&shadow.join(table), e))?;
            continue;
        }
        fs::create_dir(shadow.join(table)).map_err(|e| DbError::io(&shadow.join(table), e))?;
        for entry in fs::read_dir(&source).map_err(|e| DbError::io(&source, e))? {
            let path = entry.map_err(|e| DbError::io(&source, e))?.path();
            let name = path
                .file_name()
                .ok_or_else(|| DbError::new("PATH_VIOLATION", "table entry has no filename", 2))?;
            let target = shadow.join(table).join(name);
            let metadata = fs::symlink_metadata(&path).map_err(|e| DbError::io(&path, e))?;
            if metadata.file_type().is_file() && !has_multiple_links(&metadata) {
                fs::copy(&path, &target).map_err(|e| DbError::io(&path, e))?;
            } else {
                // Preserve the fact that an unsupported entry exists without
                // dereferencing or reading the special object.
                fs::create_dir(&target).map_err(|e| DbError::io(&target, e))?;
            }
        }
    }
    for change in changes {
        match change {
            Change::Write { path, bytes } => {
                let target = shadow.join(path);
                if let Some(p) = target.parent() {
                    fs::create_dir_all(p).map_err(|e| DbError::io(p, e))?
                }
                if fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.file_type().is_dir())
                {
                    fs::remove_dir_all(&target).map_err(|e| DbError::io(&target, e))?;
                }
                fs::write(&target, bytes).map_err(|e| DbError::io(&target, e))?
            }
            Change::Delete { path } => {
                let target = shadow.join(path);
                match fs::symlink_metadata(&target) {
                    Ok(metadata) if metadata.file_type().is_dir() => {
                        fs::remove_dir_all(&target).map_err(|e| DbError::io(&target, e))?
                    }
                    Ok(_) => fs::remove_file(&target).map_err(|e| DbError::io(&target, e))?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(DbError::io(&target, error)),
                }
            }
        }
    }
    crate::db::validate_format(shadow)?;
    let mut future_config = crate::db::load_config(shadow)?;
    future_config.apply_overrides(resource_overrides);
    future_config
        .validate()
        .map_err(|message| DbError::new("RESOURCE_LIMIT", message, 1))?;
    let future = Catalog::observe(shadow, &future_config)?;
    let errors = integrity::validate(&future);
    if let Some(mut diagnostic) = errors.first().cloned() {
        let residual = errors
            .iter()
            .map(|error| {
                error.path.as_ref().map_or_else(
                    || error.code.clone(),
                    |path| format!("{} at {}", error.code, path.display()),
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        diagnostic.help = Some(format!(
            "the prospective transaction was rejected with {} residual violation(s): {residual}",
            errors.len()
        ));
        return Err(DbError::from_diag(diagnostic, 2));
    }
    Ok(())
}

/// Copy one schema directory into a shadow tree, if it exists.
///
/// Entries that are not private regular files are reproduced as directories so
/// that the shadow still exhibits the fault, rather than dereferencing whatever
/// they point at.
fn mirror_schema_directory(source: &Path, destination: &Path) -> Result<()> {
    match fs::symlink_metadata(source) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(DbError::from_diag(
                Diagnostic::error(
                    "NON_REGULAR_FILE",
                    format!("{} must be a real directory", source.display()),
                )
                .at(source),
                2,
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(DbError::io(source, error)),
    }
    fs::create_dir_all(destination).map_err(|e| DbError::io(destination, e))?;
    for entry in fs::read_dir(source).map_err(|e| DbError::io(source, e))? {
        let path = entry.map_err(|e| DbError::io(source, e))?.path();
        let metadata = fs::symlink_metadata(&path).map_err(|e| DbError::io(&path, e))?;
        let filename = path
            .file_name()
            .ok_or_else(|| DbError::new("PATH_VIOLATION", "schema entry has no filename", 2))?;
        let target = destination.join(filename);
        if metadata.file_type().is_file() && !has_multiple_links(&metadata) {
            fs::copy(&path, &target).map_err(|e| DbError::io(&path, e))?;
        } else {
            fs::create_dir(&target).map_err(|e| DbError::io(&target, e))?;
        }
    }
    Ok(())
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

fn apply_journal(root: &Path, dir: &Path, j: &Journal) -> Result<()> {
    validate_journal(dir, j)?;
    for c in &j.changes {
        let target = root.join(&c.path);
        validate_target_parent(root, &c.path)?;
        if let Some(stage) = &c.stage {
            let source = dir.join("staged").join(stage);
            let source_metadata = fs::symlink_metadata(&source).map_err(|error| {
                DbError::new(
                    "TRANSACTION_INCOMPLETE",
                    format!(
                        "transaction {} is missing or cannot inspect staged object {stage}: {error}",
                        j.id
                    ),
                    5,
                )
            })?;
            if !source_metadata.file_type().is_file() || has_multiple_links(&source_metadata) {
                return Err(DbError::new(
                    "TRANSACTION_INCOMPLETE",
                    format!(
                        "transaction {} staged object {stage} is not a private regular file",
                        j.id
                    ),
                    5,
                ));
            }
            if let Some(p) = target.parent() {
                fs::create_dir_all(p).map_err(|e| DbError::io(p, e))?
            }
            let temp = target.with_extension(format!("reldir-tmp-{}", j.id));
            fs::copy(&source, &temp).map_err(|e| DbError::io(&temp, e))?;
            fs::File::open(&temp)
                .and_then(|f| f.sync_all())
                .map_err(|e| DbError::io(&temp, e))?;
            if fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.file_type().is_dir()) {
                fs::remove_dir_all(&target).map_err(|e| DbError::io(&target, e))?;
            }
            fs::rename(&temp, &target).map_err(|e| DbError::io(&target, e))?;
            metadata::sync_parent(&target)?
        } else {
            match fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.file_type().is_dir() => {
                    fs::remove_dir_all(&target).map_err(|e| DbError::io(&target, e))?;
                    metadata::sync_parent(&target)?
                }
                Ok(_) => {
                    fs::remove_file(&target).map_err(|e| DbError::io(&target, e))?;
                    metadata::sync_parent(&target)?
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(DbError::io(&target, error)),
            }
        }
    }
    Ok(())
}

fn validate_journal(dir: &Path, journal: &Journal) -> Result<()> {
    let directory_id = dir.file_name().and_then(|name| name.to_str());
    if uuid::Uuid::parse_str(&journal.id).is_err() || directory_id != Some(journal.id.as_str()) {
        return Err(DbError::new(
            "TRANSACTION_INCOMPLETE",
            "transaction journal id does not match its directory",
            5,
        ));
    }
    if !matches!(
        journal.origin.as_str(),
        "internal" | "recovery" | "repair" | "migration" | "import" | "snapshot_restore"
    ) {
        return Err(DbError::new(
            "TRANSACTION_INCOMPLETE",
            format!("transaction has invalid origin {:?}", journal.origin),
            5,
        ));
    }
    if journal.changes.is_empty() {
        return Err(DbError::new(
            "TRANSACTION_INCOMPLETE",
            "transaction journal contains no changes",
            5,
        ));
    }
    let mut paths = std::collections::BTreeSet::new();
    let mut stages = std::collections::BTreeSet::new();
    for change in &journal.changes {
        safe_relative(&change.path).map_err(|error| {
            DbError::new(
                "TRANSACTION_INCOMPLETE",
                format!(
                    "unsafe path in transaction journal: {}",
                    error.diagnostic.message
                ),
                5,
            )
        })?;
        if !paths.insert(change.path.clone()) {
            return Err(DbError::new(
                "TRANSACTION_INCOMPLETE",
                format!("transaction repeats path {}", change.path.display()),
                5,
            ));
        }
        if let Some(stage) = &change.stage {
            let stage_path = Path::new(stage);
            if stage_path.components().count() != 1
                || !matches!(stage_path.components().next(), Some(Component::Normal(_)))
                || !stages.insert(stage)
            {
                return Err(DbError::new(
                    "TRANSACTION_INCOMPLETE",
                    format!("transaction has unsafe or duplicate staged object {stage:?}"),
                    5,
                ));
            }
        }
    }
    Ok(())
}

fn validate_target_parent(root: &Path, relative: &Path) -> Result<()> {
    let mut current = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            let Component::Normal(component) = component else {
                return Err(DbError::new(
                    "PATH_VIOLATION",
                    format!("unsafe transaction path {}", relative.display()),
                    5,
                ));
            };
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if !metadata.file_type().is_dir() => {
                    // Not contention and not an ordinary concurrent edit: a
                    // directory this transaction needs has been replaced by
                    // something that is not a directory. Retrying cannot help
                    // -- the next attempt meets the same path -- so this is
                    // named apart from both, and a caller that retries on
                    // conflict does not spin against it.
                    return Err(DbError::new(
                        "PATH_INTERFERENCE",
                        format!(
                            "transaction target parent {} is no longer a real directory",
                            current.display()
                        ),
                        3,
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(DbError::io(&current, error)),
            }
        }
    }
    Ok(())
}

/// What an unfinished transaction on disk means for someone reading the rows.
///
/// The distinction is the `COMMITTING` marker, and it decides whether the
/// authoritative files can be trusted as they stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// Nothing unfinished.
    None,
    /// Staged, but no rename has happened.
    ///
    /// The transaction wrote only inside `.db/transactions/`; recovery deletes
    /// it. The rows are exactly what they were, so a reader answers correctly
    /// from them -- which matters because this is the state every ordinary
    /// write passes through, and treating it as damage would mean a writer
    /// staging a commit broke every concurrent reader.
    Staged,
    /// Materialising: some renames may have been applied and some not.
    ///
    /// Recovery rolls this forward from the staged bytes. Until it does, the
    /// rows are a partial application of a change nobody can see the whole of.
    Materialising,
}

impl Pending {
    /// Whether the rows on disk may be a half-applied change.
    pub fn is_materialising(self) -> bool {
        matches!(self, Self::Materialising)
    }
}

pub fn has_pending(root: &Path) -> Result<Pending> {
    let tx = root.join(".db/transactions");
    if !metadata::ensure_real_directory(&tx, false, "transaction store")? {
        return Ok(Pending::None);
    }
    let mut pending = Pending::None;
    for entry in fs::read_dir(&tx).map_err(|e| DbError::io(&tx, e))? {
        let path = entry.map_err(|e| DbError::io(&tx, e))?.path();
        let entry_metadata = fs::symlink_metadata(&path).map_err(|e| DbError::io(&path, e))?;
        if !entry_metadata.file_type().is_dir() {
            return Err(DbError::new(
                "TRANSACTION_INCOMPLETE",
                format!(
                    "transaction entry {} is not a real directory",
                    path.display()
                ),
                5,
            ));
        }
        // One materialising transaction decides the answer for the whole
        // store: if any renames may be half-applied, the rows are suspect
        // whatever the other entries are doing.
        if path.join("COMMITTING").exists() {
            return Ok(Pending::Materialising);
        }
        pending = Pending::Staged;
    }
    Ok(pending)
}
pub fn recover(root: &Path) -> Result<bool> {
    let tx = root.join(".db/transactions");
    if !metadata::ensure_real_directory(&tx, false, "transaction store")? {
        return Ok(false);
    }
    let mut recovered = false;
    for e in fs::read_dir(&tx).map_err(|e| DbError::io(&tx, e))? {
        let dir = e.map_err(|e| DbError::io(&tx, e))?.path();
        let metadata = fs::symlink_metadata(&dir).map_err(|error| DbError::io(&dir, error))?;
        if !metadata.file_type().is_dir() {
            return Err(DbError::new(
                "TRANSACTION_INCOMPLETE",
                format!(
                    "transaction entry {} is not a real directory",
                    dir.display()
                ),
                5,
            ));
        }
        let jp = dir.join("journal.json");
        if !jp.exists() {
            fs::remove_dir_all(&dir).map_err(|e| DbError::io(&dir, e))?;
            continue;
        }
        let j: Journal = crate::json::parse_as(&fs::read(&jp).map_err(|e| DbError::io(&jp, e))?)
            .map_err(|e| {
                DbError::new(
                    "TRANSACTION_INCOMPLETE",
                    format!("corrupt transaction journal {}: {e}", jp.display()),
                    5,
                )
            })?;
        if dir.join("COMMITTING").exists() {
            if !dir.join("RECOVERED").exists() {
                apply_journal(root, &dir, &j)?;
                fs::write(dir.join("RECOVERED"), b"recovered\n")
                    .map_err(|e| DbError::io(&dir, e))?;
            }
            recovered = true;
        } else {
            fs::remove_dir_all(&dir).map_err(|e| DbError::io(&dir, e))?;
        }
    }
    Ok(recovered)
}
pub fn finalize_recovered(root: &Path) -> Result<()> {
    let tx = root.join(".db/transactions");
    if !tx.exists() {
        return Ok(());
    }
    for e in fs::read_dir(&tx).map_err(|e| DbError::io(&tx, e))? {
        let dir = e.map_err(|e| DbError::io(&tx, e))?.path();
        if dir.join("RECOVERED").exists() {
            fs::remove_dir_all(&dir).map_err(|e| DbError::io(&dir, e))?;
        }
    }
    Ok(())
}
fn safe_relative(path: &Path) -> Result<()> {
    if path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
        || (path.starts_with(".db")
            && path != Path::new(".db/config")
            && path != Path::new(".db/format")
            // Working schemas are written through the same transaction as the
            // rows they describe, so that a migration changing both lands
            // atomically. They are derived state, not authoritative intent.
            && !is_working_schema(path))
    {
        return Err(DbError::new(
            "PATH_VIOLATION",
            format!("unsafe authoritative path {}", path.display()),
            2,
        ));
    }
    Ok(())
}

/// Whether a path names a working schema, which transactions may write.
fn is_working_schema(path: &Path) -> bool {
    path.parent() == Some(Path::new(".db/schema"))
        && path.extension().and_then(|value| value.to_str()) == Some("json")
}

fn validate_lock_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !has_multiple_links(&metadata) => Ok(()),
        Ok(_) => Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("lock {} is not a private regular file", path.display()),
            6,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DbError::io(path, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Section 56: authoritative paths are validated canonical relative paths.
    /// Anything that could escape the database root, or reach into internal
    /// metadata that is not itself authoritative, is refused before staging.
    #[test]
    fn test1105_unsafe_authoritative_paths_are_refused() {
        for rejected in [
            "../escape.json",
            "users/../../escape.json",
            "/absolute/row.json",
            ".db/manifest.json",
            ".db/indexes/users--abc.json",
            ".db/transactions/x/journal.json",
            ".db/schema",
            ".db/schema/users.txt",
            ".db/schema/nested/users.json",
        ] {
            let error = safe_relative(Path::new(rejected))
                .expect_err(&format!("{rejected} must be refused"));
            assert_eq!(error.diagnostic.code, "PATH_VIOLATION");
        }

        // Governed rows, schemas, and the two authoritative .db files pass.
        for accepted in [
            "users/u1.json",
            "schema/users.json",
            ".db/config",
            ".db/format",
            ".db/schema/users.json",
        ] {
            safe_relative(Path::new(accepted))
                .unwrap_or_else(|error| panic!("{accepted} must be allowed: {error:?}"));
        }
    }

    /// Section 31: a journal is replayed only when it is internally consistent.
    /// A journal whose id does not match its directory cannot be trusted to
    /// describe that directory's staged bytes, so it is refused rather than
    /// guessed through.
    #[test]
    fn test1106_journal_identity_must_match_its_directory() {
        let id = uuid::Uuid::new_v4().to_string();
        let journal = Journal {
            id: id.clone(),
            origin: "internal".into(),
            changes: vec![JournalChange {
                path: PathBuf::from("users/u1.json"),
                stage: Some("00000000".into()),
            }],
        };
        let directory = std::path::PathBuf::from("/tmp").join(&id);
        validate_journal(&directory, &journal).expect("a matching id validates");

        let mismatched = std::path::PathBuf::from("/tmp").join(uuid::Uuid::new_v4().to_string());
        let error = validate_journal(&mismatched, &journal).expect_err("mismatch must be refused");
        assert_eq!(error.diagnostic.code, "TRANSACTION_INCOMPLETE");
        assert_eq!(error.exit, 5);

        // A non-UUID id is refused even when the directory agrees.
        let bogus = Journal {
            id: "not-a-uuid".into(),
            ..Journal {
                id: String::new(),
                    origin: "internal".into(),
                changes: vec![JournalChange {
                    path: PathBuf::from("users/u1.json"),
                    stage: None,
                }],
            }
        };
        let directory = std::path::PathBuf::from("/tmp/not-a-uuid");
        assert!(validate_journal(&directory, &bogus).is_err());
    }

    /// Section 23: provenance origin is a closed set. An unrecognised origin
    /// means the journal was not written by this system and must not be
    /// replayed into authoritative state.
    #[test]
    fn test1107_journal_origin_is_a_closed_set() {
        let id = uuid::Uuid::new_v4().to_string();
        let directory = std::path::PathBuf::from("/tmp").join(&id);
        let journal = |origin: &str| Journal {
            id: id.clone(),
            origin: origin.into(),
            changes: vec![JournalChange {
                path: PathBuf::from("users/u1.json"),
                stage: None,
            }],
        };
        for origin in [
            "internal",
            "recovery",
            "repair",
            "migration",
            "import",
            "snapshot_restore",
        ] {
            validate_journal(&directory, &journal(origin))
                .unwrap_or_else(|error| panic!("{origin} is a valid origin: {error:?}"));
        }
        // `external` describes an observation, never a journal this binary wrote.
        for origin in ["external", "", "arbitrary"] {
            assert!(
                validate_journal(&directory, &journal(origin)).is_err(),
                "{origin} must be refused"
            );
        }
    }

    /// Section 31: a journal must describe an unambiguous set of changes. A
    /// repeated path or a reused staged object would make replay order-dependent.
    #[test]
    fn test1108_journals_reject_ambiguous_change_sets() {
        let id = uuid::Uuid::new_v4().to_string();
        let directory = std::path::PathBuf::from("/tmp").join(&id);
        let with = |changes: Vec<JournalChange>| Journal {
            id: id.clone(),
            origin: "internal".into(),
            changes,
        };

        // An empty journal has nothing to commit and is not a valid transaction.
        assert!(validate_journal(&directory, &with(vec![])).is_err());

        // The same path twice is ambiguous.
        let repeated = with(vec![
            JournalChange {
                path: PathBuf::from("users/u1.json"),
                stage: Some("00000000".into()),
            },
            JournalChange {
                path: PathBuf::from("users/u1.json"),
                stage: Some("00000001".into()),
            },
        ]);
        assert!(validate_journal(&directory, &repeated).is_err());

        // Two paths claiming the same staged object is ambiguous.
        let shared_stage = with(vec![
            JournalChange {
                path: PathBuf::from("users/u1.json"),
                stage: Some("00000000".into()),
            },
            JournalChange {
                path: PathBuf::from("users/u2.json"),
                stage: Some("00000000".into()),
            },
        ]);
        assert!(validate_journal(&directory, &shared_stage).is_err());

        // A staged name that is a path rather than a single component would
        // escape the staging directory.
        for escape in ["../evil", "a/b", "/abs", ".."] {
            let traversal = with(vec![JournalChange {
                path: PathBuf::from("users/u1.json"),
                stage: Some(escape.into()),
            }]);
            assert!(
                validate_journal(&directory, &traversal).is_err(),
                "staged name {escape:?} must be refused"
            );
        }

        // An unsafe target path inside the journal is refused as well.
        let traversal = with(vec![JournalChange {
            path: PathBuf::from("../escape.json"),
            stage: None,
        }]);
        assert!(validate_journal(&directory, &traversal).is_err());

        // A well-formed mixed write/delete journal validates.
        let good = with(vec![
            JournalChange {
                path: PathBuf::from("users/u1.json"),
                stage: Some("00000000".into()),
            },
            JournalChange {
                path: PathBuf::from("users/u2.json"),
                stage: None,
            },
        ]);
        validate_journal(&directory, &good).expect("a consistent journal validates");
    }
}
