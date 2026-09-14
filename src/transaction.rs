use crate::{
    catalog::Catalog,
    config::Config,
    diagnostic::{DbError, Diagnostic, Result},
    integrity, metadata,
};
use fs2::FileExt;
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
struct Journal {
    id: String,
    start_root: String,
    origin: String,
    changes: Vec<JournalChange>,
}
#[derive(Debug, Serialize, Deserialize)]
struct JournalChange {
    path: PathBuf,
    stage: Option<String>,
}

pub fn commit(
    root: &Path,
    config: &Config,
    start_root: &str,
    changes: &[Change],
    origin: &str,
    dry_run: bool,
) -> Result<Vec<PathBuf>> {
    if changes.is_empty() {
        return Ok(vec![]);
    }
    let mut writes = std::collections::BTreeSet::new();
    let mut deletes = std::collections::BTreeSet::new();
    for change in changes {
        let (path, seen) = match change {
            Change::Write { path, .. } => (path, &mut writes),
            Change::Delete { path } => (path, &mut deletes),
        };
        if !seen.insert(path.clone()) {
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
    validate_prospective(root, changes)?;
    if dry_run {
        return Ok(changes
            .iter()
            .map(|c| match c {
                Change::Write { path, .. } | Change::Delete { path } => path.clone(),
            })
            .collect());
    }
    let lock_path = root.join(".db/lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| DbError::io(&lock_path, e))?;
    lock.try_lock_exclusive().map_err(|_| {
        DbError::new(
            "CONCURRENT_MODIFICATION",
            "another writer holds the database lock",
            3,
        )
    })?;
    let current = Catalog::observe(root, config)?;
    let (current_hash, _) = metadata::state(&current)?;
    if current_hash != start_root {
        return Err(DbError::from_diag(
            Diagnostic::error(
                "CONCURRENT_MODIFICATION",
                "authoritative files changed while the mutation was being planned",
            )
            .expected(start_root)
            .observed(current_hash),
            3,
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let dir = root.join(".db/transactions").join(&id);
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
        start_root: start_root.into(),
        origin: origin.into(),
        changes: jc,
    };
    metadata::write_json_atomic(&dir.join("journal.json"), &journal)?;
    let rechecked = Catalog::observe(root, config)?;
    let (rechecked_root, _) = metadata::state(&rechecked)?;
    if rechecked_root != start_root {
        fs::remove_dir_all(&dir).map_err(|e| DbError::io(&dir, e))?;
        return Err(DbError::from_diag(
            Diagnostic::error(
                "CONCURRENT_MODIFICATION",
                "authoritative files changed while the transaction was staged",
            )
            .expected(start_root)
            .observed(rechecked_root),
            3,
        ));
    }
    fs::write(dir.join("COMMITTING"), b"commit\n")
        .map_err(|e| DbError::io(&dir.join("COMMITTING"), e))?;
    metadata::sync_parent(&dir.join("COMMITTING"))?;
    apply_journal(root, &dir, &journal)?;
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
    metadata::record(&c, old.as_ref(), hash, entries, origin, Some(&id))?;
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

fn validate_prospective(root: &Path, changes: &[Change]) -> Result<()> {
    // Prospective validation must be observational from the database's point of
    // view, including for --dry-run. Keeping the shadow outside the database
    // also prevents an interrupted validator from being mistaken for a pending
    // transaction.
    let parent = std::env::temp_dir();
    let temp = tempfile::Builder::new()
        .prefix("jdb-prospective-")
        .tempdir_in(&parent)
        .map_err(|e| DbError::io(&parent, e))?;
    let shadow = temp.path();
    fs::create_dir(shadow.join("schema")).map_err(|e| DbError::io(shadow, e))?;
    fs::create_dir(shadow.join(".db")).map_err(|e| DbError::io(shadow, e))?;
    for name in ["format", "config"] {
        let source = root.join(".db").join(name);
        if source.exists() {
            fs::copy(&source, shadow.join(".db").join(name))
                .map_err(|e| DbError::io(&source, e))?;
        }
    }
    let source_schema = root.join("schema");
    let source_schema_metadata =
        fs::symlink_metadata(&source_schema).map_err(|e| DbError::io(&source_schema, e))?;
    if !source_schema_metadata.file_type().is_dir() {
        return Err(DbError::from_diag(
            Diagnostic::error("NON_REGULAR_FILE", "schema/ must be a real directory").at("schema"),
            2,
        ));
    }
    for entry in fs::read_dir(&source_schema).map_err(|e| DbError::io(&source_schema, e))? {
        let p = entry.map_err(|e| DbError::io(&source_schema, e))?.path();
        let metadata = fs::symlink_metadata(&p).map_err(|e| DbError::io(&p, e))?;
        let filename = p
            .file_name()
            .ok_or_else(|| DbError::new("PATH_VIOLATION", "schema entry has no filename", 2))?;
        let target = shadow.join("schema").join(filename);
        if metadata.file_type().is_file() && !has_multiple_links(&metadata) {
            fs::copy(&p, &target).map_err(|e| DbError::io(&p, e))?;
        } else {
            fs::create_dir(&target).map_err(|e| DbError::io(&target, e))?;
        }
    }
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
                fs::write(&target, bytes).map_err(|e| DbError::io(&target, e))?
            }
            Change::Delete { path } => {
                let target = shadow.join(path);
                if target.exists() {
                    fs::remove_file(&target).map_err(|e| DbError::io(&target, e))?
                }
            }
        }
    }
    crate::db::validate_format(shadow)?;
    let future_config = crate::db::load_config(shadow)?;
    let future = Catalog::observe(shadow, &future_config)?;
    let errors = integrity::validate(&future);
    if let Some(d) = errors.into_iter().next() {
        return Err(DbError::from_diag(d, 2));
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
    for c in &j.changes {
        let target = root.join(&c.path);
        if let Some(stage) = &c.stage {
            let source = dir.join("staged").join(stage);
            if !source.exists() {
                return Err(DbError::new(
                    "TRANSACTION_INCOMPLETE",
                    format!("transaction {} is missing staged object {stage}", j.id),
                    5,
                ));
            }
            if let Some(p) = target.parent() {
                fs::create_dir_all(p).map_err(|e| DbError::io(p, e))?
            }
            let temp = target.with_extension(format!("jdb-tmp-{}", j.id));
            fs::copy(&source, &temp).map_err(|e| DbError::io(&temp, e))?;
            fs::File::open(&temp)
                .and_then(|f| f.sync_all())
                .map_err(|e| DbError::io(&temp, e))?;
            fs::rename(&temp, &target).map_err(|e| DbError::io(&target, e))?;
            metadata::sync_parent(&target)?
        } else if target.exists() {
            fs::remove_file(&target).map_err(|e| DbError::io(&target, e))?;
            metadata::sync_parent(&target)?
        }
    }
    Ok(())
}

pub fn has_pending(root: &Path) -> Result<bool> {
    let tx = root.join(".db/transactions");
    if !tx.exists() {
        return Ok(false);
    }
    for entry in fs::read_dir(&tx).map_err(|e| DbError::io(&tx, e))? {
        if entry.map_err(|e| DbError::io(&tx, e))?.path().is_dir() {
            return Ok(true);
        }
    }
    Ok(false)
}
pub fn recover(root: &Path) -> Result<bool> {
    let tx = root.join(".db/transactions");
    if !tx.exists() {
        return Ok(false);
    }
    let mut recovered = false;
    for e in fs::read_dir(&tx).map_err(|e| DbError::io(&tx, e))? {
        let dir = e.map_err(|e| DbError::io(&tx, e))?.path();
        if !dir.is_dir() {
            continue;
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
            && path != Path::new(".db/format"))
    {
        return Err(DbError::new(
            "PATH_VIOLATION",
            format!("unsafe authoritative path {}", path.display()),
            2,
        ));
    }
    Ok(())
}
