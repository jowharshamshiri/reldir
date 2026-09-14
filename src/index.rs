use crate::{
    canonical,
    catalog::Catalog,
    diagnostic::{DbError, Result},
    integrity, metadata,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct IndexFile {
    table: String,
    columns: Vec<String>,
    rows: BTreeMap<String, Vec<String>>,
}

fn expected(c: &Catalog) -> BTreeMap<PathBuf, IndexFile> {
    let mut out = BTreeMap::new();
    for (table, s) in &c.schemas {
        let mut defs = BTreeSet::new();
        defs.insert(s.primary_key.clone());
        defs.extend(s.unique.clone());
        defs.extend(s.indexes.clone());
        defs.extend(s.foreign_keys.iter().map(|f| f.columns.clone()));
        for columns in defs {
            let mut rows = BTreeMap::<String, Vec<String>>::new();
            for r in &c.rows[table] {
                if let Some(key) = integrity::key(&r.value, &columns, s) {
                    rows.entry(key)
                        .or_default()
                        .push(r.relative.to_string_lossy().replace('\\', "/"));
                }
            }
            let name = format!(
                "{}--{}.json",
                table,
                columns
                    .iter()
                    .map(|x| canonical::hash_bytes(x.as_bytes())[..12].to_string())
                    .collect::<Vec<_>>()
                    .join("-")
            );
            out.insert(
                PathBuf::from(name),
                IndexFile {
                    table: table.clone(),
                    columns,
                    rows,
                },
            );
        }
    }
    out
}
pub fn valid(root: &Path, c: &Catalog) -> Result<bool> {
    let dir = root.join(".db/indexes");
    match fs::symlink_metadata(&dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(DbError::io(&dir, error)),
    }
    let expected = expected(c);
    let mut actual = BTreeSet::new();
    for e in fs::read_dir(&dir).map_err(|e| DbError::io(&dir, e))? {
        let path = e.map_err(|e| DbError::io(&dir, e))?.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| DbError::io(&path, error))?;
        if !metadata.file_type().is_file() || has_multiple_links(&metadata) {
            return Ok(false);
        }
        let rel = PathBuf::from(path.file_name().ok_or_else(|| {
            DbError::new("INTERNAL_METADATA_CORRUPT", "index has no filename", 6)
        })?);
        actual.insert(rel.clone());
        let Some(want) = expected.get(&rel) else {
            return Ok(false);
        };
        let Ok(bytes) = fs::read(&path) else {
            return Ok(false);
        };
        let Ok(got) = crate::json::parse_as::<IndexFile>(&bytes) else {
            return Ok(false);
        };
        if &got != want {
            return Ok(false);
        }
    }
    Ok(actual == expected.keys().cloned().collect())
}
pub fn rebuild(root: &Path, c: &Catalog) -> Result<()> {
    let parent = root.join(".db");
    let temp = tempfile::Builder::new()
        .prefix("indexes-")
        .tempdir_in(&parent)
        .map_err(|e| DbError::io(&parent, e))?;
    for (path, index) in expected(c) {
        metadata::write_json_atomic(&temp.path().join(path), &index)?
    }
    let dir = root.join(".db/indexes");
    let old = root.join(".db/indexes.old");
    remove_internal_path(&old)?;
    match fs::symlink_metadata(&dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            fs::rename(&dir, &old).map_err(|e| DbError::io(&dir, e))?
        }
        Ok(_) => remove_internal_path(&dir)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(DbError::io(&dir, error)),
    }
    fs::rename(temp.keep(), &dir).map_err(|e| DbError::io(&dir, e))?;
    metadata::sync_parent(&dir)?;
    remove_internal_path(&old)?;
    Ok(())
}

fn remove_internal_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            fs::remove_dir_all(path).map_err(|error| DbError::io(path, error))
        }
        Ok(_) => fs::remove_file(path).map_err(|error| DbError::io(path, error)),
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
