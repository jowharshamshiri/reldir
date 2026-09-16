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
    let expected_indexes = expected(c);
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
        let Some(want) = expected_indexes.get(&rel) else {
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
    Ok(actual == expected_indexes.keys().cloned().collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Row;
    use crate::schema::{AdditionalFields, Column, ColumnType, Schema};
    use indexmap::IndexMap;
    use serde_json::{Map, Value, json};

    fn column(kind: ColumnType, nullable: bool) -> Column {
        Column {
            kind,
            nullable,
            default: None,
            generated: None,
            values: None,
            items: None,
            properties: None,
            pattern: None,
            additional_properties: true,
            description: None,
            annotations: Default::default(),
        }
    }

    fn schema(columns: &[(&str, ColumnType, bool)], primary_key: &[&str]) -> Schema {
        let mut map = IndexMap::new();
        for (name, kind, nullable) in columns {
            map.insert((*name).to_string(), column(kind.clone(), *nullable));
        }
        Schema {
            table: "t".into(),
            schema_version: 1,
            schema_format: None,
            description: None,
            primary_key: primary_key.iter().map(|k| (*k).to_string()).collect(),
            columns: map,
            unique: vec![],
            foreign_keys: vec![],
            check: vec![],
            indexes: vec![],
            storage: None,
            additional_fields: AdditionalFields::Reject,
            annotations: Default::default(),
        }
    }

    fn catalog(schema: Schema, rows: &[Value]) -> Catalog {
        let table = schema.table.clone();
        let rows: Vec<Row> = rows
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let object: Map<String, Value> = value.as_object().unwrap().clone();
                Row {
                    table: table.clone(),
                    path: PathBuf::from(format!("/tmp/{table}/{index}.json")),
                    relative: PathBuf::from(format!("{table}/{index}.json")),
                    value: object,
                    raw: b"{}\n".to_vec(),
                }
            })
            .collect();
        Catalog {
            root: PathBuf::from("/tmp"),
            ungoverned: vec![],
            pinned: BTreeSet::new(),
            schemas: BTreeMap::from([(table.clone(), schema)]),
            schema_sources: BTreeMap::new(),
            rows: BTreeMap::from([(table, rows)]),
            diagnostics: vec![],
            warnings: vec![],
            indentation_width: 2,
        }
    }

    /// Section 35: the derived index set covers the primary key, every unique
    /// constraint, every declared secondary index, and every foreign key, with
    /// no duplicates when those definitions overlap.
    #[test]
    fn test1032_every_declared_access_path_gets_exactly_one_index() {
        let mut s = schema(
            &[
                ("id", ColumnType::String, false),
                ("email", ColumnType::String, false),
                ("team", ColumnType::String, true),
            ],
            &["id"],
        );
        s.unique = vec![vec!["email".into()]];
        s.indexes = vec![vec!["team".into()], vec!["email".into()]];
        let c = catalog(s, &[json!({"id": "a", "email": "a@x", "team": "t1"})]);

        let built = expected(&c);
        let mut covered: Vec<Vec<String>> = built.values().map(|i| i.columns.clone()).collect();
        covered.sort();
        // id (primary key), email (unique and declared index -- once), team.
        assert_eq!(
            covered,
            vec![
                vec!["email".to_string()],
                vec!["id".to_string()],
                vec!["team".to_string()]
            ],
            "overlapping definitions must not produce duplicate indexes"
        );
    }

    /// Section 35: an index maps a key to every row holding it, so a duplicate
    /// key is represented rather than silently collapsed -- the index must be
    /// able to describe an invalid state, not hide it.
    #[test]
    fn test1033_index_entries_list_every_row_for_a_key() {
        let s = schema(
            &[
                ("id", ColumnType::String, false),
                ("team", ColumnType::String, true),
            ],
            &["id"],
        );
        let mut s = s;
        s.indexes = vec![vec!["team".into()]];
        let c = catalog(
            s,
            &[
                json!({"id": "a", "team": "shared"}),
                json!({"id": "b", "team": "shared"}),
                json!({"id": "c", "team": "alone"}),
            ],
        );
        let built = expected(&c);
        let team = built
            .values()
            .find(|index| index.columns == vec!["team".to_string()])
            .expect("a team index exists");
        let shared = team
            .rows
            .values()
            .find(|paths| paths.len() == 2)
            .expect("two rows share a key");
        assert!(shared.contains(&"t/0.json".to_string()));
        assert!(shared.contains(&"t/1.json".to_string()));
        assert_eq!(team.rows.len(), 2, "one entry per distinct key");
    }

    /// Section 16: a null has no key, so a row with a null indexed column simply
    /// does not appear in that index rather than being grouped under a
    /// synthetic null key.
    #[test]
    fn test1034_rows_with_a_null_key_are_absent_from_the_index() {
        let mut s = schema(
            &[
                ("id", ColumnType::String, false),
                ("team", ColumnType::String, true),
            ],
            &["id"],
        );
        s.indexes = vec![vec!["team".into()]];
        let c = catalog(
            s,
            &[
                json!({"id": "a", "team": Value::Null}),
                json!({"id": "b", "team": "t1"}),
            ],
        );
        let built = expected(&c);
        let team = built
            .values()
            .find(|index| index.columns == vec!["team".to_string()])
            .unwrap();
        assert_eq!(team.rows.len(), 1, "only the non-null row is indexed");
    }

    /// Section 74: index derivation is deterministic, so the same catalog always
    /// produces byte-identical index files and a clone rebuilds to the same
    /// state.
    #[test]
    fn test1035_index_derivation_is_deterministic() {
        let mut s = schema(
            &[
                ("id", ColumnType::String, false),
                ("email", ColumnType::String, false),
            ],
            &["id"],
        );
        s.unique = vec![vec!["email".into()]];
        let c = catalog(
            s,
            &[
                json!({"id": "a", "email": "a@x"}),
                json!({"id": "b", "email": "b@x"}),
            ],
        );
        let first = expected(&c);
        let second = expected(&c);
        assert_eq!(
            first.keys().collect::<Vec<_>>(),
            second.keys().collect::<Vec<_>>()
        );
        for (path, index) in &first {
            assert_eq!(&second[path], index);
        }
    }

    /// A composite index is named and keyed by its whole column list, so two
    /// indexes over the same columns in a different order stay distinct.
    #[test]
    fn test1036_composite_indexes_are_keyed_by_their_full_column_list() {
        let mut s = schema(
            &[
                ("id", ColumnType::String, false),
                ("a", ColumnType::String, false),
                ("b", ColumnType::String, false),
            ],
            &["id"],
        );
        s.indexes = vec![vec!["a".into(), "b".into()], vec!["b".into(), "a".into()]];
        let c = catalog(s, &[json!({"id": "x", "a": "1", "b": "2"})]);
        let built = expected(&c);
        assert_eq!(
            built.len(),
            3,
            "primary key plus two distinct composite indexes"
        );
    }
}
