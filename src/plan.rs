//! What a mutation changes, before it is written.
//!
//! Every way the binary changes rows -- SQL, the row commands, import, doctor,
//! migrations -- describes its effect as row changes: this row, as it was, and
//! as it will be. The plan is completed by referential actions
//! ([`crate::referential`]), rendered to file changes, validated as the state
//! it would produce, and only then committed.

use crate::{
    canonical,
    catalog::{Catalog, Row},
    diagnostic::{DbError, Result},
    schema::Schema,
    transaction::Change,
};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// One row, before and after.
#[derive(Debug, Clone)]
pub struct RowChange {
    pub table: String,
    /// The row as it stands, when it exists.
    pub before: Option<Row>,
    /// The row as it will stand, when it will exist.
    pub after: Option<Map<String, Value>>,
}

impl RowChange {
    pub fn insert(table: &str, after: Map<String, Value>) -> Self {
        Self {
            table: table.to_string(),
            before: None,
            after: Some(after),
        }
    }

    pub fn update(before: Row, after: Map<String, Value>) -> Self {
        Self {
            table: before.table.clone(),
            before: Some(before),
            after: Some(after),
        }
    }

    pub fn delete(before: Row) -> Self {
        Self {
            table: before.table.clone(),
            before: Some(before),
            after: None,
        }
    }
}

/// The file a row lives in.
pub fn row_path(schema: &Schema, row: &Map<String, Value>) -> Result<PathBuf> {
    let name = canonical::filename(schema, row).ok_or_else(|| {
        DbError::new(
            "NOT_NULL_VIOLATION",
            format!(
                "a {} row must carry {}, which its filename is built from",
                schema.table(),
                schema.filename_columns().join(", ")
            ),
            2,
        )
    })?;
    if !canonical::filename_fits(&name) {
        return Err(DbError::new(
            "FILENAME_TOO_LONG",
            format!(
                "the row's identity renders to a {}-byte filename, beyond the {}-byte limit filesystems share",
                name.len(),
                canonical::MAX_FILENAME_BYTES
            ),
            2,
        ));
    }
    Ok(Path::new(schema.table()).join(name))
}

/// The file changes a set of row changes amounts to. A row whose canonical
/// form and path are unchanged is not rewritten.
pub fn render(catalog: &Catalog, rows: &[RowChange]) -> Result<Vec<Change>> {
    let mut writes: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut deletes: Vec<PathBuf> = vec![];
    for change in rows {
        let schema = catalog
            .schemas
            .get(&change.table)
            .ok_or_else(|| catalog.unknown_table(&change.table))?;
        let target = match &change.after {
            Some(after) => Some((row_path(schema, after)?, after)),
            None => None,
        };
        if let Some(before) = &change.before
            && target.as_ref().is_none_or(|(path, _)| path != &before.relative)
        {
            deletes.push(before.relative.clone());
        }
        if let Some((path, after)) = target {
            let canonical_after = canonical::canonical_row(after, schema);
            let unchanged = change.before.as_ref().is_some_and(|before| {
                before.relative == path && canonical::canonical_row(&before.value, schema) == canonical_after
            });
            if !unchanged {
                writes.insert(
                    path,
                    canonical::pretty_with_indent(&canonical_after, catalog.indentation_width),
                );
            }
        }
    }
    // A row may only land on a path that is free, or that a row of this plan
    // is leaving. Anything else would silently replace a file.
    let vacated: std::collections::BTreeSet<&PathBuf> =
        rows.iter().filter_map(|change| change.before.as_ref().map(|row| &row.relative)).collect();
    for path in writes.keys() {
        if !vacated.contains(path) && catalog.mirror.file(&crate::catalog::slash(path))?.is_some() {
            return Err(DbError::from_diag(
                crate::diagnostic::Diagnostic::error(
                    "PRIMARY_KEY_VIOLATION",
                    format!("{} already exists, and the change would replace it", path.display()),
                )
                .at(path.clone())
                .help("update the existing row instead, or choose a different key"),
                2,
            ));
        }
    }
    let mut out: Vec<Change> = deletes
        .into_iter()
        .filter(|path| !writes.contains_key(path))
        .map(|path| Change::Delete { path })
        .collect();
    out.extend(writes.into_iter().map(|(path, bytes)| Change::Write { path, bytes }));
    Ok(out)
}
