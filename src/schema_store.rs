//! Where schemas live, and what the two locations mean.
//!
//! A database has one working schema per table and may have a pin for it.
//!
//! The working schema lives in `.db/schema/<table>.json`. It is what every
//! other subsystem validates against, queries through, and reports on. jdb owns
//! it: inference writes it, maintenance updates it, and deleting `.db/`
//! discards it exactly as it discards an index, because it can be rebuilt.
//!
//! The pin lives in `schema/<table>.json`. It is optional, and it is the user's
//! declaration rather than jdb's derivation: a schema they wrote, or one they
//! promoted from inference with `db schema pin`. A pinned table's working
//! schema is copied from the pin instead of inferred from data, so pinning is
//! how a refinement inference could never re-derive -- an enum, a check, a
//! foreign key whose column name breaks the convention -- survives `rm -rf
//! .db`.
//!
//! Where both exist they are expected to agree, and when they do not the pin
//! wins: it is the declaration, and the working copy is derived state that is
//! rebuilt from it. That is what makes `schema/` worth keeping in version
//! control and `.db/` safe to delete at any time.

use crate::{
    canonical,
    diagnostic::{DbError, Diagnostic, Result},
    schema::{self, Schema},
};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

/// The directory holding working schemas, which jdb maintains.
pub fn working_dir(root: &Path) -> PathBuf {
    root.join(".db/schema")
}

/// The directory holding pinned schemas, which the user declares.
pub fn pin_dir(root: &Path) -> PathBuf {
    root.join("schema")
}

/// The working schema file for one table.
pub fn working_path(root: &Path, table: &str) -> PathBuf {
    working_dir(root).join(format!("{table}.json"))
}

/// The pin file for one table.
pub fn pin_path(root: &Path, table: &str) -> PathBuf {
    pin_dir(root).join(format!("{table}.json"))
}

/// The working schema's path relative to the database root, as transactions
/// and change plans name it.
pub fn working_relative(table: &str) -> String {
    format!(".db/schema/{table}.json")
}

/// The pin's path relative to the database root, as provenance and the manifest
/// record it.
pub fn pin_relative(table: &str) -> String {
    format!("schema/{table}.json")
}

/// Whether a relative path names a working schema.
///
/// Distinct from [`is_pin_relative`] because `.db/schema/x.json` also ends in
/// `schema/x.json`: a prefix test that did not know the difference would count
/// jdb's own copy as the user's declaration.
fn is_working_relative(path: &str) -> bool {
    path.starts_with(".db/schema/") && path.ends_with(".json")
}

/// Whether a relative path names either schema location.
pub fn is_schema_relative(path: &str) -> bool {
    is_working_relative(path) || is_pin_relative(path)
}

/// The table a working-schema path names, if it is one.
pub fn working_table(path: &str) -> Option<&str> {
    path.strip_prefix(".db/schema/")?.strip_suffix(".json")
}

/// Whether a relative path names a pin.
pub fn is_pin_relative(path: &str) -> bool {
    path.starts_with("schema/") && path.ends_with(".json")
}

/// Write a working schema, creating `.db/schema/` if this is the first.
///
/// Writing here needs no authorization: the working schema is derived state,
/// reconstructible from the rows it describes or from the pin that fixes it.
pub fn write_working(root: &Path, schema: &Schema, indentation_width: usize) -> Result<()> {
    let directory = working_dir(root);
    fs::create_dir_all(&directory).map_err(|error| DbError::io(&directory, error))?;
    // One serialization for both locations. A schema jdb writes must read back
    // exactly as the pin it came from would: two renderings of the same
    // declaration differ in nesting and byte length, so a limit one form passes
    // the other can fail.
    let bytes = canonical_bytes(schema, indentation_width)?;
    crate::metadata::write_bytes_atomic(&working_path(root, &schema.table), &bytes)
}

/// The tables that have a pin.
///
/// An absent `schema/` is the ordinary case -- pinning is opt-in -- and yields
/// no tables rather than an error.
pub fn pinned_tables(root: &Path) -> Result<BTreeSet<String>> {
    let directory = pin_dir(root);
    let mut out = BTreeSet::new();
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(error) => return Err(DbError::io(&directory, error)),
    };
    if !metadata.file_type().is_dir() {
        return Err(DbError::from_diag(
            Diagnostic::error("NON_REGULAR_FILE", "schema/ must be a real directory").at("schema"),
            2,
        ));
    }
    for entry in fs::read_dir(&directory).map_err(|error| DbError::io(&directory, error))? {
        let path = entry
            .map_err(|error| DbError::io(&directory, error))?
            .path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            out.insert(stem.to_string());
        }
    }
    Ok(out)
}

/// Read a pin, if the table has one.
pub fn load_pin(root: &Path, table: &str) -> Result<Option<Schema>> {
    let path = pin_path(root, table);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() && !has_multiple_links(&metadata) => {
            schema::load(&path).map(Some)
        }
        Ok(_) => Err(DbError::from_diag(
            Diagnostic::error(
                "NON_REGULAR_FILE",
                "pinned schemas must be private regular files",
            )
            .at(pin_relative(table)),
            2,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(DbError::io(&path, error)),
    }
}

/// The canonical bytes of a schema, as both locations store it and as the
/// manifest hashes it. One rendering, so that a pin and the working copy taken
/// from it compare equal byte for byte.
pub fn canonical_bytes(schema: &Schema, indentation_width: usize) -> Result<Vec<u8>> {
    let value = serde_json::to_value(schema)
        .map_err(|error| DbError::new("INTERNAL_METADATA_CORRUPT", error.to_string(), 6))?;
    Ok(canonical::pretty_with_indent(&value, indentation_width))
}

/// Whether two schemas are the same declaration.
///
/// Compared as canonical values rather than as bytes, so that indentation or
/// key order -- neither of which changes what the schema says -- is not
/// mistaken for divergence.
pub fn equivalent(left: &Schema, right: &Schema) -> Result<bool> {
    let left = serde_json::to_value(left)
        .map_err(|error| DbError::new("INTERNAL_METADATA_CORRUPT", error.to_string(), 6))?;
    let right = serde_json::to_value(right)
        .map_err(|error| DbError::new("INTERNAL_METADATA_CORRUPT", error.to_string(), 6))?;
    Ok(canonical::normalize(&left) == canonical::normalize(&right))
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
    use crate::schema::{AdditionalFields, Column, ColumnType};
    use indexmap::IndexMap;

    fn schema(table: &str) -> Schema {
        let mut columns = IndexMap::new();
        columns.insert(
            "id".to_string(),
            Column {
                kind: ColumnType::String,
                nullable: false,
                description: None,
                values: None,
                items: None,
                properties: None,
                generated: None,
                default: None,
            },
        );
        Schema {
            table: table.into(),
            schema_version: 1,
            schema_format: None,
            description: None,
            primary_key: vec!["id".into()],
            columns,
            unique: vec![],
            foreign_keys: vec![],
            check: vec![],
            indexes: vec![],
            storage: None,
            additional_fields: AdditionalFields::Reject,
        }
    }

    /// The two locations are distinct and neither is inside the other's tree:
    /// the working schema is metadata, the pin is a file the user keeps in
    /// version control beside their data.
    #[test]
    fn test1109_working_schemas_and_pins_occupy_separate_locations() {
        let root = Path::new("/db");
        assert_eq!(
            working_path(root, "users"),
            root.join(".db/schema/users.json")
        );
        assert_eq!(pin_path(root, "users"), root.join("schema/users.json"));
        assert!(working_path(root, "users").starts_with(root.join(".db")));
        assert!(!pin_path(root, "users").starts_with(root.join(".db")));
    }

    /// Provenance records pins, never working schemas: the working copy is
    /// derived, so recording it would make a rebuild look like a change.
    #[test]
    fn test1110_only_pins_have_a_recorded_relative_path() {
        assert_eq!(pin_relative("users"), "schema/users.json");
        assert!(is_pin_relative("schema/users.json"));

        // Working schemas live under `.db/` and are not manifest paths. The
        // two spellings share a suffix, so a prefix test that did not know the
        // difference would record jdb's own copy as the user's declaration.
        assert_eq!(working_relative("users"), ".db/schema/users.json");
        assert!(!is_pin_relative(".db/schema/users.json"));
        assert!(is_schema_relative(".db/schema/users.json"));
        assert!(!is_pin_relative("users/u1.json"));
        assert!(!is_schema_relative("users/u1.json"));
        assert!(!is_pin_relative("schema/users.txt"));
    }

    /// Equivalence is about what the schema says, not how it was written. A pin
    /// re-serialized at a different indentation still fixes the same schema.
    #[test]
    fn test1111_equivalence_ignores_formatting_but_not_meaning() {
        let left = schema("users");
        let mut right = schema("users");
        assert!(equivalent(&left, &right).unwrap());

        right.additional_fields = AdditionalFields::Allow;
        assert!(
            !equivalent(&left, &right).unwrap(),
            "a different rule is a different schema"
        );

        // Canonical bytes at different widths render differently but mean the
        // same thing, which is exactly what `equivalent` must see through.
        let narrow = canonical_bytes(&left, 2).unwrap();
        let wide = canonical_bytes(&left, 4).unwrap();
        assert_ne!(narrow, wide);
        let from_narrow: Schema = serde_json::from_slice(&narrow).unwrap();
        let from_wide: Schema = serde_json::from_slice(&wide).unwrap();
        assert!(equivalent(&from_narrow, &from_wide).unwrap());
    }

    /// An absent `schema/` means nothing is pinned, which is the ordinary state
    /// of a database nobody has pinned yet -- not a fault.
    #[test]
    fn test1112_an_absent_pin_directory_means_nothing_is_pinned() {
        let directory = tempfile::tempdir().unwrap();
        assert!(pinned_tables(directory.path()).unwrap().is_empty());
        assert!(load_pin(directory.path(), "users").unwrap().is_none());

        fs::create_dir(directory.path().join("schema")).unwrap();
        fs::write(
            directory.path().join("schema/users.json"),
            canonical_bytes(&schema("users"), 2).unwrap(),
        )
        .unwrap();
        assert_eq!(
            pinned_tables(directory.path()).unwrap(),
            BTreeSet::from(["users".to_string()])
        );
        assert_eq!(
            load_pin(directory.path(), "users").unwrap().unwrap().table,
            "users"
        );
    }

    /// A file where `schema/` should be is refused rather than read around: a
    /// database whose pin directory is a regular file is not one jdb can
    /// interpret, and guessing would mean ignoring a declaration the user made.
    #[test]
    fn test1113_a_pin_directory_that_is_not_a_directory_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("schema"), b"not a directory").unwrap();
        let error = pinned_tables(directory.path()).expect_err("must refuse");
        assert_eq!(error.diagnostic.code, "NON_REGULAR_FILE");
    }
}
