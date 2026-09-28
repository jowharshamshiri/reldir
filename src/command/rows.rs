//! Reading and changing rows one at a time, and in bulk.

use super::{Context, Intent, empty, find_row, require_valid, schema_of};
use crate::{
    db::{Database, Request},
    diagnostic::{DbError, Result},
    output::{Finish, Sink},
    plan::RowChange,
    schema::{ColumnType, GeneratedKind, Schema},
};
use serde_json::{Map, Value, json};

pub fn tables(context: &Context, sink: &mut dyn Sink) -> Result<Finish> {
    let Some(database) = context.open(sink, Intent::Read)? else { return Ok(empty()) };
    for (table, schema) in &database.catalog.schemas {
        let mut record = Map::new();
        record.insert("kind".into(), json!("table"));
        record.insert("table".into(), json!(table));
        record.insert("rows".into(), json!(database.catalog.mirror.count(table)?));
        record.insert("primary_key".into(), json!(schema.primary_key()));
        record.insert("pinned".into(), json!(database.catalog.pinned.contains(table)));
        if let Some(domain) = schema.identity_domain() {
            record.insert("identity_domain".into(), json!(domain));
        }
        sink.record(record)?;
    }
    Ok(Finish::ok(format!("{} table(s)", database.catalog.schemas.len())).with("valid", database.is_valid()))
}

/// A table's schema, its references in and out, and its size.
pub fn describe(context: &Context, sink: &mut dyn Sink, table: &str) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    let schema = schema_of(&database, table)?;
    let schemas = &database.catalog.schemas;
    let referenced_by: Vec<Value> = schemas
        .iter()
        .flat_map(|(other, other_schema)| {
            other_schema.foreign_keys().iter().filter_map(move |fk| {
                crate::integrity::target_tables(schemas, fk.to())
                    .contains(&table.to_string())
                    .then(|| json!({"table": other, "constraint": fk.describe(other), "on_delete": fk.on_delete().name()}))
            })
        })
        .collect();
    let columns: Vec<Value> = schema
        .columns()
        .iter()
        .map(|(name, column)| {
            json!({
                "name": name,
                "type": column.kind().name(),
                "nullable": column.nullable(),
                "required": column.required(),
                "default": column.default(),
                "generated": column.generated().map(GeneratedKind::name),
                "description": column.description(),
            })
        })
        .collect();
    sink.document(
        "table_description",
        json!({
            "table": table,
            "schema_file": database.catalog.schema_files.get(table).map(|file| &file.relative),
            "pinned": database.catalog.pinned.contains(table),
            "rows": database.catalog.mirror.count(table)?,
            "primary_key": schema.primary_key(),
            "identity_domain": schema.identity_domain(),
            "columns": columns,
            "references": schema.foreign_keys().iter().map(|fk| json!({
                "constraint": fk.describe(table),
                "on_delete": fk.on_delete().name(),
                "on_update": fk.on_update().name(),
            })).collect::<Vec<_>>(),
            "referenced_by": referenced_by,
        }),
    )?;
    Ok(Finish::ok(String::new()).with("valid", database.is_valid()))
}

fn gate(context: &Context, database: &Database, sink: &mut dyn Sink) -> Result<()> {
    if context.allow_invalid {
        if !database.is_valid() {
            for diagnostic in &database.verdict.errors {
                sink.diagnostic(diagnostic)?;
            }
            sink.note("answering from an INVALID database because of --allow-invalid");
        }
        return Ok(());
    }
    require_valid(database)
}

pub fn get(context: &Context, sink: &mut dyn Sink, table: &str, key: &str) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    gate(context, &database, sink)?;
    let row = find_row(&database, table, key)?;
    sink.record(row.value.clone())?;
    Ok(Finish::ok(String::new()).with("path", row.relative.to_string_lossy().into_owned()).with("database_valid", database.is_valid()))
}

pub struct ListOptions<'a> {
    pub filter: Option<&'a str>,
    pub order: Option<&'a str>,
    pub limit: Option<usize>,
}

/// Rows of a table, optionally filtered by a SQL condition and ordered by a
/// column -- a query written for you.
pub fn list(context: &Context, sink: &mut dyn Sink, table: &str, options: ListOptions<'_>) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    gate(context, &database, sink)?;
    let schema = schema_of(&database, table)?;
    let mut text = format!("SELECT * FROM {}", crate::mirror::quote(table));
    if let Some(filter) = options.filter {
        text.push_str(&format!(" WHERE ({filter})"));
    }
    match options.order {
        Some(order) => {
            let (column, direction) = match order.strip_prefix('-') {
                Some(column) => (column, "DESC"),
                None => (order, "ASC"),
            };
            if schema.column(column).is_none() {
                return Err(DbError::new("UNKNOWN_COLUMN", format!("{table} has no column {column:?} to order by"), 4));
            }
            text.push_str(&format!(" ORDER BY {} {direction}", crate::mirror::quote(column)));
        }
        None => {
            let key: Vec<String> = schema.primary_key().iter().map(|c| crate::mirror::quote(c)).collect();
            text.push_str(&format!(" ORDER BY {}", key.join(", ")));
        }
    }
    if let Some(limit) = options.limit {
        text.push_str(&format!(" LIMIT {limit}"));
    }
    let count = crate::sql::query(&database.catalog.mirror, &database.catalog.schemas, &text, &[], database.query_limits(), |row| {
        sink.record(row)
    })?;
    Ok(Finish::ok(format!("{count} row(s)")).with("rows", count).with("database_valid", database.is_valid()))
}

/// Fill the generated columns a new row leaves empty.
fn generate(database: &Database, table: &str, schema: &Schema, row: &mut Map<String, Value>, offset: i64) -> Result<()> {
    for (name, column) in schema.columns() {
        let Some(kind) = column.generated() else { continue };
        if row.get(name).is_some_and(|value| !value.is_null()) {
            continue;
        }
        let sequence = match kind {
            GeneratedKind::Sequence => crate::sql::sequence_start(database.catalog.mirror.connection(), table, name)?
                .checked_add(offset)
                .ok_or_else(|| DbError::new("RESOURCE_LIMIT", format!("the sequence for {table}.{name} is exhausted"), 2))?,
            _ => 0,
        };
        row.insert(name.clone(), crate::value::generate(kind, sequence));
    }
    Ok(())
}

fn objects(value: Value) -> Result<Vec<Map<String, Value>>> {
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
                "ROW_ROOT_NOT_OBJECT",
                format!("item {} is {}, not an object; a row is one JSON object", index + 1, other),
                2,
            )),
        })
        .collect()
}

/// Insert one row, or an array of rows, as one transaction.
pub fn insert(context: &Context, sink: &mut dyn Sink, table: &str, json_text: &str) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    require_valid(&database)?;
    let value = crate::json::parse_str(json_text).map_err(|error| DbError::usage(format!("the row is not JSON: {error}")))?;
    let rows = objects(value)?;
    insert_rows(context, sink, &mut database, table, rows, "internal", "inserted")
}

fn insert_rows(
    context: &Context,
    sink: &mut dyn Sink,
    database: &mut Database,
    table: &str,
    rows: Vec<Map<String, Value>>,
    origin: &'static str,
    verb: &str,
) -> Result<Finish> {
    let schema = schema_of(database, table)?.clone();
    let mut changes = vec![];
    let mut sequences = 0i64;
    for mut row in rows {
        generate(database, table, &schema, &mut row, sequences)?;
        sequences += 1;
        changes.push(RowChange::insert(table, row));
    }
    let outcome = database.apply(changes, Request { origin, ..Request::internal(context.dry_run) })?;
    super::committed(sink, &outcome, verb)
}

/// Set top-level columns of one row. `null` sets a column to null.
pub fn update(context: &Context, sink: &mut dyn Sink, table: &str, key: &str, patch: &str) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    require_valid(&database)?;
    let patch = match crate::json::parse_str(patch).map_err(|error| DbError::usage(format!("the patch is not JSON: {error}")))? {
        Value::Object(map) => map,
        _ => return Err(DbError::usage("a patch is a JSON object of the columns to set")),
    };
    let schema = schema_of(&database, table)?;
    for name in patch.keys() {
        if schema.column(name).is_none() {
            return Err(DbError::new("UNKNOWN_COLUMN", format!("{table} has no column {name:?}"), 4));
        }
    }
    let row = find_row(&database, table, key)?;
    let mut after = row.value.clone();
    for (name, value) in patch {
        after.insert(name, value);
    }
    let outcome = database.apply(vec![RowChange::update(row, after)], Request::internal(context.dry_run))?;
    super::committed(sink, &outcome, "updated")
}

/// Delete one row, with whatever its referrers' `onDelete` actions require.
pub fn delete(context: &Context, sink: &mut dyn Sink, table: &str, key: &str) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    require_valid(&database)?;
    let row = find_row(&database, table, key)?;
    let outcome = database.apply(vec![RowChange::delete(row)], Request::internal(context.dry_run))?;
    super::committed(sink, &outcome, "deleted")
}

/// Insert rows from a file: a JSON array or object, JSON lines, or CSV with a
/// header naming columns.
pub fn import(context: &Context, sink: &mut dyn Sink, table: &str, source: &str) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    require_valid(&database)?;
    let schema = schema_of(&database, table)?.clone();
    let text = super::read_input(source, database.config.max_transaction_size)?;
    let extension = std::path::Path::new(source).extension().and_then(|e| e.to_str()).unwrap_or("");
    let rows = match extension {
        "csv" => csv_rows(&text, &schema)?,
        "jsonl" | "ndjson" => text
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| {
                let value = crate::json::parse_str(line)
                    .map_err(|error| DbError::new("INVALID_JSON", format!("line {}: {error}", index + 1), 2))?;
                objects(value).map(|mut rows| rows.remove(0))
            })
            .collect::<Result<Vec<_>>>()?,
        _ => objects(crate::json::parse_str(&text).map_err(|error| DbError::new("INVALID_JSON", error.to_string(), 2))?)?,
    };
    insert_rows(context, sink, &mut database, table, rows, "import", "imported")
}

fn csv_rows(text: &str, schema: &Schema) -> Result<Vec<Map<String, Value>>> {
    let mut reader = csv::Reader::from_reader(text.as_bytes());
    let headers = reader.headers().map_err(|error| DbError::new("INVALID_CSV", error.to_string(), 2))?.clone();
    for header in headers.iter() {
        if schema.column(header).is_none() {
            return Err(DbError::new("UNKNOWN_COLUMN", format!("the CSV header names {header:?}, which {} does not have", schema.table()), 4));
        }
    }
    let mut out = vec![];
    for record in reader.records() {
        let record = record.map_err(|error| DbError::new("INVALID_CSV", error.to_string(), 2))?;
        let mut row = Map::new();
        for (header, cell) in headers.iter().zip(record.iter()) {
            let column = schema.column(header).expect("checked above");
            let value = if cell.is_empty() && column.nullable() {
                Value::Null
            } else {
                match column.kind() {
                    ColumnType::String | ColumnType::Enum | ColumnType::Decimal | ColumnType::Bytes | ColumnType::Date
                    | ColumnType::Timestamp | ColumnType::Uuid | ColumnType::Ulid => Value::String(cell.to_string()),
                    kind => {
                        let parsed = crate::json::parse_str(cell).unwrap_or_else(|_| Value::String(cell.to_string()));
                        crate::value::lossless_convert(&parsed, kind).unwrap_or(parsed)
                    }
                }
            };
            row.insert(header.to_string(), value);
        }
        out.push(row);
    }
    Ok(out)
}

/// Every row of a table in canonical form, to stdout or to a new file.
pub fn export(context: &Context, sink: &mut dyn Sink, table: &str, out: Option<&std::path::Path>, as_csv: bool) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    require_valid(&database)?;
    let schema = schema_of(&database, table)?;
    let rows = database.catalog.rows(table)?;
    let Some(path) = out else {
        for row in &rows {
            sink.record(crate::canonical::canonical_row(&row.value, schema).as_object().cloned().unwrap_or_default())?;
        }
        return Ok(Finish::ok(format!("{} row(s)", rows.len())));
    };
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::AlreadyExists => DbError::usage(format!("{} exists; export never overwrites a file", path.display())),
            _ => DbError::io(path, error),
        })?;
    let bytes = if as_csv {
        let mut writer = csv::Writer::from_writer(vec![]);
        let headers: Vec<&String> = schema.columns().keys().collect();
        writer.write_record(&headers).map_err(|e| DbError::new("IO_ERROR", e.to_string(), 6))?;
        for row in &rows {
            writer
                .write_record(headers.iter().map(|header| match row.value.get(*header) {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::String(text)) => text.clone(),
                    Some(other) => crate::canonical::compact(other),
                }))
                .map_err(|e| DbError::new("IO_ERROR", e.to_string(), 6))?;
        }
        writer.into_inner().map_err(|e| DbError::new("IO_ERROR", e.to_string(), 6))?
    } else {
        let mut bytes = vec![];
        for row in &rows {
            bytes.extend(crate::canonical::compact(&crate::canonical::canonical_row(&row.value, schema)).into_bytes());
            bytes.push(b'\n');
        }
        bytes
    };
    file.write_all(&bytes).and_then(|()| file.sync_all()).map_err(|error| DbError::io(path, error))?;
    Ok(Finish::ok(format!("exported {} row(s) to {}", rows.len(), path.display())).with("rows", rows.len()))
}
