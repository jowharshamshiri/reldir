//! Reading and changing rows one at a time, and in bulk.

use super::{Context, Intent, empty, find_row, gate, require_valid, schema_of};
use crate::output::{Finish, Sink};
use reldir_core::code::Code;
use reldir_core::{
    db::Database,
    diagnostic::{DbError, Result},
    schema::GeneratedKind,
    store::revision::Origin,
};
use serde_json::{Map, Value, json};

pub fn tables(context: &Context, sink: &mut dyn Sink) -> Result<Finish> {
    let Some(database) = context.open(sink, Intent::Read)? else {
        return Ok(empty());
    };
    for (table, schema) in &database.catalog.schemas {
        let mut record = Map::new();
        record.insert("kind".into(), json!("table"));
        record.insert("table".into(), json!(table));
        record.insert("rows".into(), json!(database.catalog.mirror.count(table)?));
        record.insert("primary_key".into(), json!(schema.primary_key()));
        record.insert(
            "pinned".into(),
            json!(database.catalog.pinned.contains(table)),
        );
        if let Some(domain) = schema.identity_domain() {
            record.insert("identity_domain".into(), json!(domain));
        }
        sink.record(record)?;
    }
    Ok(
        Finish::ok(format!("{} table(s)", database.catalog.schemas.len()))
            .with("valid", database.is_valid()),
    )
}

/// A table's schema, its references in and out, and its size.
pub fn describe(context: &Context, sink: &mut dyn Sink, table: &str) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    let schema = schema_of(&database, table)?;
    let schemas = &database.catalog.schemas;
    let referenced_by: Vec<Value> = schemas
        .iter()
        .flat_map(|(other, other_schema)| {
            other_schema
                .foreign_keys()
                .iter()
                .filter(move |fk| reldir_core::integrity::target_tables(schemas, fk.to()).contains(&table.to_string()))
                .map(move |fk| json!({"table": other, "constraint": fk.describe(other), "on_delete": fk.on_delete().name()}))
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
    sink.description(
        "table_description",
        json!({
            "table": table,
            "schema_file": database.catalog.schema_files.get(table).map(|file| reldir_core::catalog::slash(&file.relative)),
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

pub fn get(context: &Context, sink: &mut dyn Sink, table: &str, key: &str) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    gate(context, &database, sink)?;
    let row = find_row(&database.catalog, table, key)?;
    sink.row(row.value.clone())?;
    Ok(Finish::ok(String::new())
        .with("path", row.relative.to_string_lossy().into_owned())
        .with("database_valid", database.is_valid()))
}

pub struct ListOptions<'a> {
    pub filter: Option<&'a str>,
    pub order: Option<&'a str>,
    pub limit: Option<usize>,
}

/// Rows of a table, optionally filtered by a SQL condition and ordered by a
/// column -- a query written for you.
pub fn list(
    context: &Context,
    sink: &mut dyn Sink,
    table: &str,
    options: ListOptions<'_>,
) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    gate(context, &database, sink)?;
    let schema = schema_of(&database, table)?;
    let mut text = format!("SELECT * FROM {}", reldir_core::mirror::quote(table));
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
                return Err(DbError::new(
                    Code::UNKNOWN_COLUMN,
                    format!("{table} has no column {column:?} to order by"),
                ));
            }
            text.push_str(&format!(
                " ORDER BY {} {direction}",
                reldir_core::mirror::quote(column)
            ));
        }
        None => {
            let key: Vec<String> = schema
                .primary_key()
                .iter()
                .map(|c| reldir_core::mirror::quote(c))
                .collect();
            text.push_str(&format!(" ORDER BY {}", key.join(", ")));
        }
    }
    if let Some(limit) = options.limit {
        text.push_str(&format!(" LIMIT {limit}"));
    }
    let count = reldir_core::sql::query(
        &database.catalog.mirror,
        &database.catalog.schemas,
        &text,
        &[],
        database.query_limits(),
        |row| sink.row(row),
    )?;
    Ok(Finish::ok(format!("{count} row(s)"))
        .with("rows", count)
        .with("database_valid", database.is_valid()))
}

/// Insert one row, or an array of rows, as one transaction.
pub fn insert(
    context: &Context,
    sink: &mut dyn Sink,
    table: &str,
    json_text: &str,
) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    require_valid(&database)?;
    let value = reldir_core::json::parse_str(json_text).map_err(|error| {
        DbError::new(Code::INPUT_INVALID, format!("the row is not JSON: {error}"))
    })?;
    let rows = reldir_core::rows::objects(value)?;
    insert_rows(
        context,
        sink,
        &mut database,
        table,
        rows,
        reldir_core::store::revision::Origin::User,
        "inserted",
    )
}

fn insert_rows(
    context: &Context,
    sink: &mut dyn Sink,
    database: &mut Database,
    table: &str,
    rows: Vec<Map<String, Value>>,
    origin: reldir_core::store::revision::Origin,
    verb: &str,
) -> Result<Finish> {
    schema_of(database, table)?;
    // Generated values -- a sequence above all -- are taken under the lock,
    // from the state the rows are committed to.
    let (outcome, _) = database.apply_with(context.request(origin), |catalog| {
        Ok((reldir_core::rows::insert(catalog, table, rows)?, ()))
    })?;
    report_written(sink, &outcome, table)?;
    super::committed(sink, &outcome, verb)
}

/// The rows a command wrote to its table, as they are written -- generated
/// keys and all. Rows a referential action changed are reported as events.
fn report_written(
    sink: &mut dyn Sink,
    outcome: &reldir_core::db::Outcome,
    table: &str,
) -> Result<()> {
    let induced: std::collections::BTreeSet<&std::path::Path> = outcome
        .induced
        .iter()
        .map(|induced| induced.path.as_path())
        .collect();
    for change in &outcome.rows {
        let Some(after) = &change.after else {
            continue;
        };
        let induced_here = change
            .before
            .as_ref()
            .is_some_and(|before| induced.contains(before.relative.as_path()));
        if change.table == table && !induced_here {
            sink.row(after.clone())?;
        }
    }
    Ok(())
}

/// Set top-level columns of one row. `null` sets a column to null.
pub fn update(
    context: &Context,
    sink: &mut dyn Sink,
    table: &str,
    key: &str,
    patch: &str,
) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    require_valid(&database)?;
    let patch = match reldir_core::json::parse_str(patch).map_err(|error| {
        DbError::new(
            Code::INPUT_INVALID,
            format!("the patch is not JSON: {error}"),
        )
    })? {
        Value::Object(map) => map,
        _ => {
            return Err(DbError::usage(
                "a patch is a JSON object of the columns to set",
            ));
        }
    };
    let (outcome, _) = database.apply_with(context.request(Origin::User), |catalog| {
        let key = super::key_values(catalog, table, key)?;
        Ok((
            vec![reldir_core::rows::update(catalog, table, &key, &patch)?],
            (),
        ))
    })?;
    report_written(sink, &outcome, table)?;
    super::committed(sink, &outcome, "updated")
}

/// Delete one row, with whatever its referrers' `onDelete` actions require.
pub fn delete(context: &Context, sink: &mut dyn Sink, table: &str, key: &str) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    require_valid(&database)?;
    let (outcome, _) = database.apply_with(context.request(Origin::User), |catalog| {
        let key = super::key_values(catalog, table, key)?;
        Ok((vec![reldir_core::rows::delete(catalog, table, &key)?], ()))
    })?;
    super::committed(sink, &outcome, "deleted")
}
