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
    if !dir.is_dir() {
        return Ok(false);
    }
    let expected = expected(c);
    let mut actual = BTreeSet::new();
    for e in fs::read_dir(&dir).map_err(|e| DbError::io(&dir, e))? {
        let path = e.map_err(|e| DbError::io(&dir, e))?.path();
        if !path.is_file() {
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
        let Ok(got) = serde_json::from_slice::<IndexFile>(&bytes) else {
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
    if old.exists() {
        fs::remove_dir_all(&old).map_err(|e| DbError::io(&old, e))?
    }
    if dir.exists() {
        fs::rename(&dir, &old).map_err(|e| DbError::io(&dir, e))?
    }
    fs::rename(temp.keep(), &dir).map_err(|e| DbError::io(&dir, e))?;
    metadata::sync_parent(&dir)?;
    if old.exists() {
        fs::remove_dir_all(&old).map_err(|e| DbError::io(&old, e))?
    }
    Ok(())
}
