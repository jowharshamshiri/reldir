//! What a mutation changes, before it is written.
//!
//! Every way the binary changes rows -- SQL, the row commands, import, doctor,
//! migrations -- describes its effect as row changes: this row, as it was, and
//! as it will be. The plan is completed by referential actions
//! ([`crate::referential`]), rendered to file changes, validated as the state
//! it would produce, and only then committed.

use crate::code::Code;
use crate::{
    catalog::{Catalog, Row},
    diagnostic::{DbError, Diagnostic, Result},
    schema::Schema,
};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// One file a change writes or removes, relative to the database root: what a
/// plan amounts to on disk, before the commit protocol makes it durable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Write { path: PathBuf, bytes: Vec<u8> },
    Delete { path: PathBuf },
}

impl FileChange {
    pub fn path(&self) -> &Path {
        match self {
            Self::Write { path, .. } | Self::Delete { path } => path,
        }
    }
}

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

/// Why a row's key cannot name its file, as the diagnostic that says so.
pub fn naming_diagnostic(schema: &Schema, error: &crate::naming::NamingError) -> Diagnostic {
    use crate::naming::NamingError as Naming;
    let code = match error {
        Naming::Empty | Naming::Unkeyable { .. } => Code::NOT_NULL_VIOLATION,
        Naming::TooLong { .. } => Code::FILENAME_TOO_LONG,
        Naming::OutsideAlphabet { .. } => Code::FILENAME_RULE_UNDETERMINED,
        _ => Code::PATH_VIOLATION,
    };
    Diagnostic::error(
        code,
        format!(
            "a {} row's key cannot name its file: {error}",
            schema.table()
        ),
    )
    .table(schema.table())
}

/// The file a row lives in: its key, spelled by the table's naming rule.
pub fn row_path(schema: &Schema, row: &Map<String, Value>) -> Result<PathBuf> {
    let stem = schema
        .row_identity()
        .stem(row)
        .map_err(|error| DbError::from_diag(naming_diagnostic(schema, &error)))?;
    // Separated by `/` on every platform: a row's path is its name in the
    // mirror, in history and in the protocol, which must not differ by OS.
    Ok(PathBuf::from(format!(
        "{}/{stem}{}",
        schema.table(),
        crate::naming::EXTENSION
    )))
}

/// A row's body: what its file holds, without the column a table keyed by
/// file name reads from the name.
pub fn body(schema: &Schema, row: &Map<String, Value>) -> Map<String, Value> {
    let mut body = row.clone();
    if let Some(column) = schema.stem_column() {
        body.remove(column);
    }
    body
}

/// The bytes a row's file holds after a change: `previous` -- the file it
/// had -- edited in place, so every member the change leaves alone keeps its
/// exact bytes, or a new file in the table's style.
pub fn row_bytes(
    schema: &Schema,
    previous: Option<&[u8]>,
    after: &Map<String, Value>,
) -> Result<Vec<u8>> {
    crate::json_edit::write_document(
        previous,
        &Value::Object(body(schema, after)),
        &schema.style(),
    )
    .map_err(|error| {
        DbError::new(
            Code::INTERNAL_ERROR,
            format!("a {} row could not be written: {error}", schema.table()),
        )
    })
}

/// The file changes a set of row changes amounts to. A row whose file would
/// hold exactly the bytes it holds is not rewritten.
pub fn render(catalog: &Catalog, rows: &[RowChange]) -> Result<Vec<FileChange>> {
    render_with(catalog, rows, &|row: &Row| row.raw(&catalog.root))
}

/// [`render`], reading the bytes a row's file holds through `previous`: for a
/// transaction, whose earlier statements wrote files that are not on disk yet.
pub fn render_with(
    catalog: &Catalog,
    rows: &[RowChange],
    previous: &dyn Fn(&Row) -> Result<Vec<u8>>,
) -> Result<Vec<FileChange>> {
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
            && target
                .as_ref()
                .is_none_or(|(path, _)| path != &before.relative)
        {
            deletes.push(before.relative.clone());
        }
        if let Some((path, after)) = target {
            let held = change.before.as_ref().map(previous).transpose()?;
            let bytes = row_bytes(schema, held.as_deref(), after)?;
            let unchanged = match (&change.before, &held) {
                (Some(before), Some(held)) if before.relative == path => *held == bytes,
                _ => false,
            };
            if !unchanged {
                writes.insert(path, bytes);
            }
        }
    }
    // A row may only land on a path that is free, or that a row of this plan
    // is leaving. Anything else would silently replace a file.
    let vacated: std::collections::BTreeSet<&PathBuf> = rows
        .iter()
        .filter_map(|change| change.before.as_ref().map(|row| &row.relative))
        .collect();
    for path in writes.keys() {
        if !vacated.contains(path) && catalog.mirror.file(&crate::catalog::slash(path))?.is_some() {
            return Err(DbError::from_diag(
                crate::diagnostic::Diagnostic::error(
                    Code::PRIMARY_KEY_VIOLATION,
                    format!(
                        "{} already exists, and the change would replace it",
                        path.display()
                    ),
                )
                .at(path.clone())
                .help("update the existing row instead, or choose a different key"),
            ));
        }
    }
    let mut out: Vec<FileChange> = deletes
        .into_iter()
        .filter(|path| !writes.contains_key(path))
        .map(|path| FileChange::Delete { path })
        .collect();
    out.extend(
        writes
            .into_iter()
            .map(|(path, bytes)| FileChange::Write { path, bytes }),
    );
    Ok(out)
}
