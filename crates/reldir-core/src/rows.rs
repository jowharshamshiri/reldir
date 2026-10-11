//! Row operations by table and key: what `insert`, `update`, `delete` and
//! `get` mean, shared by every front end -- the command line, the library,
//! the bindings, the MCP server.
//!
//! Each builds the [`RowChange`]s of one operation from the catalog it will
//! be committed against; committing them is [`crate::db::Database`]'s work.

use crate::{
    catalog::{Catalog, Row},
    code::Code,
    diagnostic::{DbError, Result},
    plan::RowChange,
    schema::{GeneratedKind, Schema},
};
use serde_json::{Map, Value};

/// The schema of a table the catalog holds.
pub fn schema<'c>(catalog: &'c Catalog, table: &str) -> Result<&'c Schema> {
    catalog
        .schemas
        .get(table)
        .ok_or_else(|| catalog.unknown_table(table))
}

/// The row a table holds under a key: its values in key order.
pub fn find(catalog: &Catalog, table: &str, key: &[Value]) -> Result<Option<Row>> {
    let schema = schema(catalog, table)?;
    let columns = schema.primary_key();
    if key.len() != columns.len() {
        return Err(DbError::usage(format!(
            "{table} is keyed on ({}), so a key is {} value(s), not {}",
            columns.join(", "),
            columns.len(),
            key.len()
        )));
    }
    let probe: Map<String, Value> = columns.iter().cloned().zip(key.iter().cloned()).collect();
    match crate::mirror::key(&probe, columns, schema) {
        Some(rendered) => catalog.row_by_key(table, &rendered),
        None => Ok(None),
    }
}

/// The row a table holds under a key, which must exist.
pub fn existing(catalog: &Catalog, table: &str, key: &[Value]) -> Result<Row> {
    find(catalog, table, key)?.ok_or_else(|| {
        DbError::new(
            Code::UNKNOWN_ROW,
            format!(
                "{table} has no row with key {}",
                crate::canonical::compact(&Value::Array(key.to_vec()))
            ),
        )
    })
}

/// Fill the generated columns a new row leaves out. A member the row states
/// -- `null` included -- is what the row says, as a column an SQL insert
/// names is. `offset` places the row among others inserted with it, so each
/// takes its own sequence number.
pub fn generate(
    catalog: &Catalog,
    table: &str,
    row: &mut Map<String, Value>,
    offset: i64,
) -> Result<()> {
    let schema = schema(catalog, table)?;
    for (name, column) in schema.columns() {
        let Some(kind) = column.generated() else {
            continue;
        };
        if row.contains_key(name) {
            continue;
        }
        let sequence = match kind {
            GeneratedKind::Sequence => {
                crate::sql::sequence_start(catalog.mirror.connection(), table, name)?
                    .checked_add(offset)
                    .ok_or_else(|| {
                        DbError::new(
                            Code::RESOURCE_LIMIT,
                            format!("the sequence for {table}.{name} is exhausted"),
                        )
                    })?
            }
            _ => 0,
        };
        row.insert(name.clone(), crate::value::generate(kind, sequence));
    }
    Ok(())
}

/// Inserting rows into a table, generated columns filled.
pub fn insert(
    catalog: &Catalog,
    table: &str,
    rows: Vec<Map<String, Value>>,
) -> Result<Vec<RowChange>> {
    schema(catalog, table)?;
    let mut changes = Vec::with_capacity(rows.len());
    for (offset, mut row) in (0i64..).zip(rows) {
        generate(catalog, table, &mut row, offset)?;
        changes.push(RowChange::insert(table, row));
    }
    Ok(changes)
}

/// Setting top-level columns of one row; `null` sets a column to null.
pub fn update(
    catalog: &Catalog,
    table: &str,
    key: &[Value],
    patch: &Map<String, Value>,
) -> Result<RowChange> {
    let schema = schema(catalog, table)?;
    if let Some(unknown) = patch.keys().find(|name| schema.column(name).is_none()) {
        return Err(DbError::new(
            Code::UNKNOWN_COLUMN,
            format!("{table} has no column {unknown:?}"),
        ));
    }
    let row = existing(catalog, table, key)?;
    let mut after = row.value.clone();
    for (name, value) in patch {
        after.insert(name.clone(), value.clone());
    }
    Ok(RowChange::update(row, after))
}

/// Deleting one row; its referrers' `onDelete` actions follow when it is
/// committed.
pub fn delete(catalog: &Catalog, table: &str, key: &[Value]) -> Result<RowChange> {
    Ok(RowChange::delete(existing(catalog, table, key)?))
}

/// The rows of one or more JSON objects: an object, or an array of them.
pub fn objects(value: Value) -> Result<Vec<Map<String, Value>>> {
    let items = match value {
        Value::Array(items) => items,
        other => vec![other],
    };
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| match item {
            Value::Object(map) => Ok(map),
            other => Err(DbError::new(
                Code::INPUT_INVALID,
                format!(
                    "item {} is {}, not an object; a row is one JSON object",
                    index + 1,
                    other
                ),
            )),
        })
        .collect()
}

/// How imported rows meet the rows a table already has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    /// Every row is new; a key the table already holds is refused.
    Insert,
    /// A row whose key the table holds sets the members it states on that
    /// row; any other row is inserted.
    Upsert,
    /// The table becomes exactly the imported rows: each existing row is
    /// replaced whole by the row with its key, and a row the import does not
    /// name is deleted -- with its referrers' actions.
    Replace,
}

impl ImportMode {
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "insert" => Ok(Self::Insert),
            "upsert" => Ok(Self::Upsert),
            "replace" => Ok(Self::Replace),
            other => Err(DbError::usage(format!(
                "{other:?} is not an import mode: insert, upsert or replace"
            ))),
        }
    }
}

/// What an import did to its table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Imported {
    pub inserted: usize,
    pub updated: usize,
    pub deleted: usize,
    pub unchanged: usize,
}

/// The changes importing `rows` into a table makes, under `mode`.
pub fn import(
    catalog: &Catalog,
    table: &str,
    rows: Vec<Map<String, Value>>,
    mode: ImportMode,
) -> Result<(Vec<RowChange>, Imported)> {
    let mut counts = Imported::default();
    if mode == ImportMode::Insert {
        counts.inserted = rows.len();
        return Ok((insert(catalog, table, rows)?, counts));
    }
    let schema = schema(catalog, table)?;
    let key_columns = schema.primary_key().to_vec();
    let mut changes = vec![];
    let mut fresh = vec![];
    let mut named = std::collections::BTreeSet::new();
    let mut keys = std::collections::BTreeSet::new();
    for (index, row) in rows.into_iter().enumerate() {
        let key: Option<Vec<Value>> = key_columns
            .iter()
            .map(|column| row.get(column).cloned())
            .collect();
        let Some(key) = key else {
            // A row without its key is new: its key is generated, or its
            // absence is a fault the insert reports.
            fresh.push(row);
            continue;
        };
        let rendered = crate::canonical::compact(&Value::Array(key.clone()));
        if !keys.insert(rendered.clone()) {
            return Err(DbError::new(
                Code::INPUT_INVALID,
                format!(
                    "row {} has the key {rendered}, as an earlier row of the import does; an \
                     import names each row once",
                    index + 1
                ),
            ));
        }
        match find(catalog, table, &key)? {
            Some(existing) => {
                named.insert(existing.relative.clone());
                let after = match mode {
                    ImportMode::Upsert => {
                        let mut after = existing.value.clone();
                        for (name, value) in row {
                            after.insert(name, value);
                        }
                        after
                    }
                    _ => row,
                };
                if after == existing.value {
                    counts.unchanged += 1;
                } else {
                    counts.updated += 1;
                    changes.push(RowChange::update(existing, after));
                }
            }
            None => fresh.push(row),
        }
    }
    counts.inserted = fresh.len();
    changes.extend(insert(catalog, table, fresh)?);
    if mode == ImportMode::Replace {
        catalog.each_row(table, |row| {
            if !named.contains(&row.relative) {
                counts.deleted += 1;
                changes.push(RowChange::delete(row));
            }
            Ok(())
        })?;
    }
    Ok((changes, counts))
}
