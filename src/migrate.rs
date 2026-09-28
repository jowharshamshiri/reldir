//! Schema migrations: one declarative engine for every structural change.
//!
//! A migration is a list of operations. Each `reldir migrate <op>` command is a
//! one-operation migration, and `reldir migrate apply <file>` runs a list; both
//! go through [`plan`], so there is one meaning for each operation.
//!
//! Operations edit schema *documents* with the schema editor and rewrite the
//! rows they reshape. The result is a set of file changes committed as one
//! transaction, after the whole resulting state -- every schema and every row --
//! has been validated. Migrations are structural: they move and reshape rows
//! but never trigger referential actions, and a migration that would leave a
//! reference dangling is refused like any other change.

use crate::{
    canonical,
    catalog::{Catalog, Row},
    db::{Database, Expected},
    diagnostic::{DbError, Diagnostic, Result},
    schema::{ColumnType, Schema, document::subschema},
    transaction::Change,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{collections::BTreeMap, path::PathBuf};

/// A migration file: `{"operations": [...]}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Migration {
    pub operations: Vec<Operation>,
}

/// One structural change.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    /// Declare a table with a complete schema document.
    AddTable {
        table: String,
        schema: Box<Value>,
    },
    /// Remove a table's schema and every row.
    DropTable {
        table: String,
    },
    /// Rename a table: its schema, its directory, and every reference to it.
    RenameTable {
        table: String,
        new: String,
    },
    /// Add a column. A column that admits no null must have a default for a
    /// table that already has rows; the default is written into every row.
    AddColumn {
        table: String,
        column: String,
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        nullable: bool,
        #[serde(default)]
        default: Option<Value>,
    },
    DropColumn {
        table: String,
        column: String,
    },
    /// Rename a column everywhere: schema, constraints, references, rows.
    RenameColumn {
        table: String,
        column: String,
        new: String,
    },
    /// Change a column's type, converting every value. Without `using` a value
    /// converts only when nothing is lost; `using` is a SQL expression over the
    /// row giving the new value.
    ChangeType {
        table: String,
        column: String,
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        using: Option<String>,
    },
    /// Add a constraint, written as the schema dialect writes it.
    AddConstraint {
        table: String,
        definition: Constraint,
    },
    DropConstraint {
        table: String,
        name: String,
    },
    AddIndex {
        table: String,
        columns: Vec<String>,
    },
    DropIndex {
        table: String,
        columns: Vec<String>,
    },
    /// Join a table to an identity domain, or with `null` leave one.
    SetIdentityDomain {
        table: String,
        domain: Option<String>,
    },
}

/// A constraint, in the dialect's own spelling, tagged with its kind.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Constraint {
    Unique {
        columns: Vec<String>,
    },
    /// A `foreignKeys` entry.
    ForeignKey(Map<String, Value>),
    Check {
        name: String,
        expr: String,
    },
    Acyclic {
        name: String,
        edges: Vec<String>,
    },
    /// An `assertions` entry.
    Assertion(Map<String, Value>),
}

impl Operation {
    pub fn describe(&self) -> String {
        match self {
            Self::AddTable { table, .. } => format!("add table {table}"),
            Self::DropTable { table } => format!("drop table {table} and its rows"),
            Self::RenameTable { table, new } => format!("rename table {table} to {new}"),
            Self::AddColumn {
                table,
                column,
                kind,
                ..
            } => format!("add column {table}.{column} ({kind})"),
            Self::DropColumn { table, column } => format!("drop column {table}.{column}"),
            Self::RenameColumn { table, column, new } => {
                format!("rename column {table}.{column} to {new}")
            }
            Self::ChangeType {
                table,
                column,
                kind,
                ..
            } => format!("change {table}.{column} to {kind}"),
            Self::AddConstraint { table, .. } => format!("add a constraint to {table}"),
            Self::DropConstraint { table, name } => format!("drop constraint {name} from {table}"),
            Self::AddIndex { table, columns } => format!("index {table}({})", columns.join(",")),
            Self::DropIndex { table, columns } => {
                format!("drop index {table}({})", columns.join(","))
            }
            Self::SetIdentityDomain {
                table,
                domain: Some(domain),
            } => format!("join {table} to identity domain {domain}"),
            Self::SetIdentityDomain {
                table,
                domain: None,
            } => format!("remove {table} from its identity domain"),
        }
    }
}

/// Rows an operation reshaped: each original row, and its new value.
type Reshaped = Vec<(Option<Row>, Map<String, Value>)>;

/// A table as the migration leaves it.
struct Working {
    document: Value,
    /// Where its schema is written: its pin, or its working schema.
    home: PathBuf,
    /// Its rows, when an operation reshaped them: (original row, new value).
    rows: Option<Reshaped>,
}

struct Plan<'c> {
    catalog: &'c Catalog,
    tables: BTreeMap<String, Working>,
    /// Tables whose schema file and rows must be removed: (home, rows).
    dropped: Vec<(PathBuf, Vec<Row>)>,
}

fn refused(code: &str, message: impl Into<String>) -> DbError {
    DbError::new(code, message, 2)
}

impl<'c> Plan<'c> {
    fn table(&mut self, table: &str) -> Result<&mut Working> {
        self.tables
            .get_mut(table)
            .ok_or_else(|| self.catalog.unknown_table(table))
    }

    /// The table's rows, loaded for reshaping.
    fn rows(&mut self, table: &str) -> Result<&mut Reshaped> {
        let catalog = self.catalog;
        let working = self
            .tables
            .get_mut(table)
            .ok_or_else(|| catalog.unknown_table(table))?;
        if working.rows.is_none() {
            let rows = if catalog.schemas.contains_key(table) {
                catalog
                    .rows(table)?
                    .into_iter()
                    .map(|row| {
                        let value = row.value.clone();
                        (Some(row), value)
                    })
                    .collect()
            } else {
                vec![]
            };
            working.rows = Some(rows);
        }
        Ok(working.rows.as_mut().expect("loaded above"))
    }

    fn edit(
        &mut self,
        table: &str,
        change: impl FnOnce(&mut crate::schema::document::Editor) -> Result<()>,
    ) -> Result<()> {
        let working = self.table(table)?;
        let schema = decode(table, working.document.clone())?;
        let mut editor = schema.edit();
        change(&mut editor)?;
        working.document = editor.document().clone();
        Ok(())
    }
}

fn decode(table: &str, document: Value) -> Result<Schema> {
    Schema::from_document(document, None).map_err(|problems| {
        let mut problems = problems.into_iter().map(|d| d.table(table));
        let first = problems
            .next()
            .unwrap_or_else(|| Diagnostic::error("SCHEMA_INVALID", "invalid schema"));
        DbError::from_diag(first, 2).with_related(problems.collect())
    })
}

fn column_type(name: &str) -> Result<ColumnType> {
    ColumnType::from_name(name).ok_or_else(|| {
        DbError::from_diag(
            Diagnostic::error("SCHEMA_TYPE_UNKNOWN", format!("unknown type {name:?}")).help(
                "the types are bool, int, float, decimal, string, bytes, date, timestamp, uuid, ulid, \
                 enum, array, object, json",
            ),
            1,
        )
    })
}

/// Work out every file change a migration makes. Nothing is written.
pub fn plan(database: &Database, migration: &Migration) -> Result<(Vec<Change>, Expected)> {
    let catalog = &database.catalog;
    let mut plan = Plan {
        catalog,
        tables: catalog
            .schemas
            .iter()
            .map(|(table, schema)| {
                (
                    table.clone(),
                    Working {
                        document: schema.document().clone(),
                        home: crate::schema_store::home(catalog, table),
                        rows: None,
                    },
                )
            })
            .collect(),
        dropped: vec![],
    };
    for (index, operation) in migration.operations.iter().enumerate() {
        apply(&mut plan, operation).map_err(|error| {
            let message = format!(
                "operation {} ({}): {}",
                index + 1,
                operation.describe(),
                error.diagnostic.message
            );
            let mut diagnostic = (*error.diagnostic).clone();
            diagnostic.message = message;
            DbError::from_diag(diagnostic, error.exit).with_related(error.related)
        })?;
    }
    render(database, plan)
}

fn apply(plan: &mut Plan<'_>, operation: &Operation) -> Result<()> {
    match operation {
        Operation::AddTable { table, schema } => {
            if plan.tables.contains_key(table) {
                return Err(refused(
                    "SCHEMA_TABLE_EXISTS",
                    format!("table {table} already exists"),
                ));
            }
            let decoded = decode(table, (**schema).clone())?;
            if decoded.table() != table {
                return Err(refused(
                    "SCHEMA_TABLE_NAME_MISMATCH",
                    format!(
                        "the schema declares table {:?}, not {table:?}",
                        decoded.table()
                    ),
                ));
            }
            plan.tables.insert(
                table.clone(),
                Working {
                    document: (**schema).clone(),
                    home: crate::schema_store::working_relative(table),
                    rows: Some(vec![]),
                },
            );
        }
        Operation::DropTable { table } => {
            let working = plan
                .tables
                .remove(table)
                .ok_or_else(|| plan.catalog.unknown_table(table))?;
            let rows = if plan.catalog.schemas.contains_key(table) {
                plan.catalog.rows(table)?
            } else {
                vec![]
            };
            plan.dropped.push((working.home, rows));
        }
        Operation::RenameTable { table, new } => {
            if !crate::schema::valid_name(new) {
                return Err(refused(
                    "SCHEMA_INVALID_TABLE_NAME",
                    format!("{new:?} cannot be a table name"),
                ));
            }
            if plan.tables.contains_key(new) || plan.catalog.root.join(new).exists() {
                return Err(refused(
                    "SCHEMA_TABLE_EXISTS",
                    format!("{new} already exists"),
                ));
            }
            plan.rows(table)?;
            let mut working = plan
                .tables
                .remove(table)
                .ok_or_else(|| plan.catalog.unknown_table(table))?;
            let schema = decode(table, working.document.clone())?;
            let mut editor = schema.edit();
            editor.rename_table(new);
            working.document = editor.document().clone();
            let pinned = working.home.starts_with("schema");
            plan.dropped.push((working.home.clone(), vec![]));
            working.home = if pinned {
                PathBuf::from(crate::schema_store::pin_relative(new))
            } else {
                crate::schema_store::working_relative(new)
            };
            plan.tables.insert(new.clone(), working);
            let names: Vec<String> = plan.tables.keys().cloned().collect();
            for name in names {
                plan.edit(&name, |editor| {
                    editor.rename_referenced_table(table, new);
                    Ok(())
                })?;
            }
        }
        Operation::AddColumn {
            table,
            column,
            kind,
            nullable,
            default,
        } => {
            let kind = column_type(kind)?;
            let has_rows = !plan.rows(table)?.is_empty();
            if plan
                .table(table)?
                .document
                .pointer(&format!(
                    "/properties/{}",
                    crate::schema::path::escape_pointer(column)
                ))
                .is_some()
            {
                return Err(refused(
                    "SCHEMA_COLUMN_EXISTS",
                    format!("{table}.{column} already exists"),
                ));
            }
            if !nullable && default.is_none() && has_rows {
                return Err(DbError::from_diag(
                    Diagnostic::error(
                        "ROW_MISSING_FIELD",
                        format!("{table} has rows, and {column} admits no null, so every row needs a value"),
                    )
                    .help("give a --default, or add the column as --nullable"),
                    2,
                ));
            }
            let mut definition = subschema(&kind, *nullable);
            if let Some(default) = default {
                definition["default"] = default.clone();
            }
            plan.edit(table, |editor| {
                editor.add_column(column, definition, !nullable);
                Ok(())
            })?;
            if let Some(default) = default {
                for (_, row) in plan.rows(table)? {
                    row.insert(column.clone(), default.clone());
                }
            }
        }
        Operation::DropColumn { table, column } => {
            plan.edit(table, |editor| {
                if editor.column(column).is_none() {
                    return Err(DbError::new(
                        "UNKNOWN_COLUMN",
                        format!("{table}.{column} does not exist"),
                        4,
                    ));
                }
                editor.drop_column(column);
                Ok(())
            })?;
            for (_, row) in plan.rows(table)? {
                row.shift_remove(column);
            }
        }
        Operation::RenameColumn { table, column, new } => {
            plan.edit(table, |editor| {
                if editor.column(column).is_none() {
                    return Err(DbError::new(
                        "UNKNOWN_COLUMN",
                        format!("{table}.{column} does not exist"),
                        4,
                    ));
                }
                if editor.column(new).is_some() {
                    return Err(refused(
                        "SCHEMA_COLUMN_EXISTS",
                        format!("{table}.{new} already exists"),
                    ));
                }
                editor
                    .rename_column(column, new, table)
                    .map_err(|message| refused("SCHEMA_CHECK_INVALID", message))?;
                Ok(())
            })?;
            let names: Vec<String> = plan
                .tables
                .keys()
                .filter(|name| *name != table)
                .cloned()
                .collect();
            for name in names {
                plan.edit(&name, |editor| {
                    editor.rename_referenced_column(table, column, new);
                    Ok(())
                })?;
            }
            for (_, row) in plan.rows(table)? {
                if let Some(value) = row.shift_remove(column) {
                    row.insert(new.clone(), value);
                }
            }
        }
        Operation::ChangeType {
            table,
            column,
            kind,
            using,
        } => {
            let kind = column_type(kind)?;
            let pointer = format!(
                "/properties/{}",
                crate::schema::path::escape_pointer(column)
            );
            let current = plan
                .table(table)?
                .document
                .pointer(&pointer)
                .cloned()
                .ok_or_else(|| {
                    DbError::new(
                        "UNKNOWN_COLUMN",
                        format!("{table}.{column} does not exist"),
                        4,
                    )
                })?;
            let nullable = decode(table, plan.table(table)?.document.clone())?
                .column(column)
                .is_some_and(|c| c.nullable());
            let mut replacement = subschema(&kind, nullable);
            for kept in ["description", "title", "$comment"] {
                if let Some(value) = current.get(kept) {
                    replacement[kept] = value.clone();
                }
            }
            plan.edit(table, |editor| {
                editor.set_column(column, replacement);
                Ok(())
            })?;
            let computed = match using {
                Some(expression) => Some(evaluate(plan.catalog, table, expression)?),
                None => None,
            };
            let schema_key = plan
                .catalog
                .schemas
                .get(table)
                .map(|s| s.primary_key().to_vec())
                .unwrap_or_default();
            for (before, row) in plan.rows(table)? {
                let source = match (&computed, before.as_ref()) {
                    (Some(values), Some(before)) => {
                        let key = crate::canonical::compact(&Value::Array(
                            schema_key
                                .iter()
                                .map(|k| before.value.get(k).cloned().unwrap_or(Value::Null))
                                .collect(),
                        ));
                        values.get(&key).cloned()
                    }
                    _ => row.get(column).cloned(),
                };
                let Some(value) = source else { continue };
                if value.is_null() {
                    row.insert(column.clone(), Value::Null);
                    continue;
                }
                let converted = crate::value::lossless_convert(&value, &kind)
                    .or_else(|| value_has_type(&value, &kind).then(|| value.clone()))
                    .ok_or_else(|| {
                        let at = before
                            .as_ref()
                            .map(|row| row.relative.display().to_string())
                            .unwrap_or_default();
                        DbError::from_diag(
                            Diagnostic::error(
                                "TYPE_MISMATCH",
                                format!(
                                    "{value} in {at} cannot become a {} without losing information",
                                    kind.name()
                                ),
                            )
                            .help("give --using with a SQL expression that computes the new value"),
                            2,
                        )
                    })?;
                row.insert(column.clone(), converted);
            }
        }
        Operation::AddConstraint { table, definition } => {
            plan.edit(table, |editor| {
                match definition {
                    Constraint::Unique { columns } => {
                        if !editor.add_list("unique", columns) {
                            return Err(refused(
                                "SCHEMA_CONSTRAINT_EXISTS",
                                "that unique constraint already exists",
                            ));
                        }
                    }
                    Constraint::ForeignKey(entry) => {
                        editor.add_foreign_key(Value::Object(entry.clone()));
                    }
                    Constraint::Check { name, expr } => {
                        editor.add_check(name, expr);
                    }
                    Constraint::Acyclic { name, edges } => {
                        editor.add_acyclic(name, edges);
                    }
                    Constraint::Assertion(entry) => {
                        editor.add_assertion(Value::Object(entry.clone()));
                    }
                }
                Ok(())
            })?;
        }
        Operation::DropConstraint { table, name } => {
            plan.edit(table, |editor| {
                if editor.drop_constraint(name) {
                    Ok(())
                } else {
                    Err(DbError::new(
                        "UNKNOWN_CONSTRAINT",
                        format!("{table} has no constraint named {name:?}"),
                        4,
                    ))
                }
            })?;
        }
        Operation::AddIndex { table, columns } => {
            plan.edit(table, |editor| {
                if columns.is_empty() {
                    return Err(DbError::usage("an index needs at least one column"));
                }
                if !editor.add_list("indexes", columns) {
                    return Err(refused(
                        "SCHEMA_CONSTRAINT_EXISTS",
                        "that index already exists",
                    ));
                }
                Ok(())
            })?;
        }
        Operation::DropIndex { table, columns } => {
            plan.edit(table, |editor| {
                if editor.remove_list("indexes", columns) {
                    Ok(())
                } else {
                    Err(DbError::new(
                        "UNKNOWN_CONSTRAINT",
                        format!("{table} has no index on ({})", columns.join(",")),
                        4,
                    ))
                }
            })?;
        }
        Operation::SetIdentityDomain { table, domain } => {
            plan.edit(table, |editor| {
                editor.set_identity_domain(domain.as_deref());
                Ok(())
            })?;
        }
    }
    Ok(())
}

/// Whether a value is already one a column of this type holds.
fn value_has_type(value: &Value, kind: &ColumnType) -> bool {
    let probe = crate::schema::document::TableBuilder::new("probe")
        .column("id", subschema(&ColumnType::Int, false), true)
        .column("v", subschema(kind, false), true)
        .primary_key(vec!["id".into()])
        .build();
    probe.is_ok_and(|schema| {
        schema
            .validator()
            .is_valid(&serde_json::json!({"id": 1, "v": value}))
    })
}

/// A SQL expression's value for every row of a table, by rendered key.
fn evaluate(catalog: &Catalog, table: &str, expression: &str) -> Result<BTreeMap<String, Value>> {
    let schema = catalog
        .schemas
        .get(table)
        .ok_or_else(|| catalog.unknown_table(table))?;
    let keys: Vec<String> = schema
        .primary_key()
        .iter()
        .map(|k| crate::mirror::quote(k))
        .collect();
    let text = format!(
        "SELECT json_array({}) AS \"_reldir_key\", ({expression}) AS \"_reldir_value\" FROM {}",
        keys.join(", "),
        crate::mirror::quote(table)
    );
    let mut out = BTreeMap::new();
    crate::sql::query(
        &catalog.mirror,
        &catalog.schemas,
        &text,
        &[],
        crate::sql::QueryLimits {
            timeout: None,
            max_rows: usize::MAX,
            max_memory: 1 << 30,
        },
        |row| {
            let key = row
                .get("_reldir_key")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let key = crate::json::parse_str(key)
                .map(|value| canonical::compact(&value))
                .unwrap_or_default();
            out.insert(
                key,
                row.get("_reldir_value").cloned().unwrap_or(Value::Null),
            );
            Ok(())
        },
    )?;
    Ok(out)
}

/// The file changes the migrated state amounts to, and what each path held
/// when planned.
fn render(database: &Database, plan: Plan<'_>) -> Result<(Vec<Change>, Expected)> {
    let width = database.config.indentation_width;
    let mut writes: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut deletes: Vec<PathBuf> = vec![];
    for (home, rows) in &plan.dropped {
        deletes.push(home.clone());
        deletes.extend(rows.iter().map(|row| row.relative.clone()));
    }
    for (table, working) in &plan.tables {
        let schema = decode(table, working.document.clone())?;
        let original = plan.catalog.schemas.get(table);
        if original.is_none_or(|original| original.document() != schema.document()) {
            writes.insert(
                working.home.clone(),
                canonical::pretty_with_indent(schema.document(), width),
            );
        }
        let Some(rows) = &working.rows else { continue };
        for (before, value) in rows {
            let path = crate::plan::row_path(&schema, value)?;
            let bytes =
                canonical::pretty_with_indent(&canonical::canonical_row(value, &schema), width);
            if let Some(before) = before
                && before.relative != path
            {
                deletes.push(before.relative.clone());
            }
            if writes.insert(path.clone(), bytes).is_some() {
                return Err(DbError::from_diag(
                    Diagnostic::error(
                        "PRIMARY_KEY_VIOLATION",
                        format!("two rows would both be written to {}", path.display()),
                    )
                    .at(path),
                    2,
                ));
            }
        }
    }
    let mut expected = Expected::new();
    let mut changes: Vec<Change> = vec![];
    for path in deletes {
        if writes.contains_key(&path) {
            continue;
        }
        expected.insert(path.clone(), database.fingerprint(&path)?);
        changes.push(Change::Delete { path });
    }
    for (path, bytes) in writes {
        let now = database.fingerprint(&path)?;
        if now.as_deref() == Some(canonical::hash_bytes(&bytes).as_str()) {
            continue;
        }
        expected.insert(path.clone(), now);
        changes.push(Change::Write { path, bytes });
    }
    Ok((changes, expected))
}
