//! Named copies of the whole authoritative state.
//!
//! A snapshot is a directory under `.db/snapshots/<name>/` laid out exactly
//! like the database: `.db/format`, `.db/config`, working schemas, pins, and
//! every row file -- including files that are not valid rows, because a
//! snapshot taken before a repair must be able to put back what the repair
//! changed. Restoring one is an ordinary transaction, validated like any
//! other, so a restore can never leave the database invalid.

use crate::{
    catalog::slash,
    db::{Database, Expected},
    diagnostic::{DbError, Result},
    transaction::Change,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

fn base(root: &Path) -> PathBuf {
    root.join(".db/snapshots")
}

/// Refuse names that are not portable directory names.
pub fn validate_name(name: &str) -> Result<()> {
    let lower = name.to_ascii_lowercase();
    let stem = lower.split('.').next().unwrap_or(&lower);
    let reserved = matches!(stem, "con" | "prn" | "aux" | "nul")
        || stem
            .strip_prefix("com")
            .or_else(|| stem.strip_prefix("lpt"))
            .is_some_and(|n| n.len() == 1 && matches!(n.as_bytes()[0], b'1'..=b'9'));
    let bad = name.is_empty()
        || name.len() > 128
        || name.starts_with('.')
        || name.ends_with(['.', ' '])
        || name.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
        || reserved;
    if bad {
        return Err(DbError::new(
            "PATH_VIOLATION",
            format!("{name:?} cannot name a snapshot: use letters, digits, '-', '_' and '.'"),
            1,
        ));
    }
    Ok(())
}

/// The authoritative files of the database, relative to the root: what a
/// snapshot keeps.
fn authoritative(database: &Database) -> Result<BTreeSet<PathBuf>> {
    let mut out = BTreeSet::new();
    for name in [".db/format", ".db/config"] {
        if database.root.join(name).is_file() {
            out.insert(PathBuf::from(name));
        }
    }
    for file in database.catalog.schema_files.values() {
        out.insert(file.relative.clone());
    }
    for table in database.catalog.schemas.keys() {
        for path in database.catalog.mirror.paths(table)? {
            out.insert(PathBuf::from(path));
        }
    }
    Ok(out)
}

pub fn exists(root: &Path, name: &str) -> bool {
    base(root).join(name).is_dir()
}

/// Copy the authoritative state to `.db/snapshots/<name>/`. The copy is built
/// beside its destination and renamed into place, so a snapshot is either
/// complete or absent.
pub fn create(database: &Database, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    let base = base(&database.root);
    crate::metadata::ensure_real_directory(&base, true, "snapshot directory")?;
    let destination = base.join(name);
    for entry in fs::read_dir(&base).map_err(|error| DbError::io(&base, error))? {
        let existing = entry
            .map_err(|error| DbError::io(&base, error))?
            .file_name()
            .to_string_lossy()
            .to_lowercase();
        if existing == name.to_lowercase() {
            return Err(DbError::new(
                "SNAPSHOT_EXISTS",
                format!("snapshot {name:?} already exists"),
                1,
            )
            .with_help("choose another name, or delete it first with `reldir snapshot delete`"));
        }
    }
    let staging = base.join(format!(".creating-{}", uuid::Uuid::new_v4()));
    let copied = (|| -> Result<()> {
        for relative in authoritative(database)? {
            let source = database.root.join(&relative);
            let target = staging.join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| DbError::io(parent, error))?;
            }
            let bytes = fs::read(&source).map_err(|error| DbError::io(&source, error))?;
            crate::metadata::write_bytes_atomic(&target, &bytes)?;
        }
        Ok(())
    })();
    if let Err(error) = copied {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    fs::rename(&staging, &destination).map_err(|error| {
        let _ = fs::remove_dir_all(&staging);
        DbError::io(&destination, error)
    })?;
    crate::metadata::sync_parent(&destination)?;
    Ok(destination)
}

/// Every snapshot, by name.
pub fn list(root: &Path) -> Result<Vec<String>> {
    let base = base(root);
    if !crate::metadata::ensure_real_directory(&base, false, "snapshot directory")? {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for entry in fs::read_dir(&base).map_err(|error| DbError::io(&base, error))? {
        let entry = entry.map_err(|error| DbError::io(&base, error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            return Err(DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                format!("snapshot entry {name} is not a directory"),
                6,
            ));
        }
        out.push(name);
    }
    out.sort();
    Ok(out)
}

/// The changes that make the database what the snapshot holds.
pub fn restore(database: &Database, name: &str) -> Result<(Vec<Change>, Expected)> {
    validate_name(name)?;
    let source = base(&database.root).join(name);
    if !source.is_dir() {
        return Err(DbError::new(
            "UNKNOWN_SNAPSHOT",
            format!("there is no snapshot {name:?}"),
            4,
        )
        .with_help("list them with `reldir snapshot list`"));
    }
    let mut held: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    collect(&source, &source, &mut held)?;
    let mut changes = vec![];
    let mut expected = Expected::new();
    for relative in authoritative(database)? {
        if !held.contains_key(&relative) && relative != Path::new(".db/format") {
            expected.insert(relative.clone(), database.fingerprint(&relative)?);
            changes.push(Change::Delete { path: relative });
        }
    }
    for (relative, bytes) in held {
        if relative == Path::new(".db/format") {
            let current = fs::read(database.root.join(&relative)).unwrap_or_default();
            if current != bytes {
                return Err(DbError::new(
                    "FORMAT_UNSUPPORTED",
                    format!("snapshot {name:?} was taken in another format and cannot be restored"),
                    6,
                ));
            }
            continue;
        }
        let now = database.fingerprint(&relative)?;
        if now.as_deref() != Some(crate::canonical::hash_bytes(&bytes).as_str()) {
            expected.insert(relative.clone(), now);
            changes.push(Change::Write {
                path: relative,
                bytes,
            });
        }
    }
    Ok((changes, expected))
}

fn collect(base: &Path, directory: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) -> Result<()> {
    for entry in fs::read_dir(directory).map_err(|error| DbError::io(directory, error))? {
        let path = entry.map_err(|error| DbError::io(directory, error))?.path();
        let meta = fs::symlink_metadata(&path).map_err(|error| DbError::io(&path, error))?;
        if meta.is_dir() {
            collect(base, &path, out)?;
        } else if meta.is_file() {
            let relative = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
            out.insert(
                PathBuf::from(slash(&relative)),
                fs::read(&path).map_err(|error| DbError::io(&path, error))?,
            );
        } else {
            return Err(DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                format!("snapshot entry {} is not a regular file", path.display()),
                6,
            ));
        }
    }
    Ok(())
}

pub fn delete(root: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    let path = base(root).join(name);
    if !path.is_dir() {
        return Err(DbError::new(
            "UNKNOWN_SNAPSHOT",
            format!("there is no snapshot {name:?}"),
            4,
        ));
    }
    fs::remove_dir_all(&path).map_err(|error| DbError::io(&path, error))?;
    crate::metadata::sync_parent(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test2220_snapshot_names_are_portable_directory_names() {
        for good in ["before-import", "pre-doctor-12", "v1.2"] {
            validate_name(good).unwrap();
        }
        for bad in [
            "",
            ".hidden",
            "a/b",
            "con",
            "lpt1.txt",
            "trailing.",
            "x:y",
            "a\u{0}b",
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }
}
