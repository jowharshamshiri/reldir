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

use crate::code::Code;
use crate::{
    canonical,
    catalog::{Catalog, Row},
    db::{Database, Expected},
    diagnostic::{DbError, Diagnostic, Result},
    plan::FileChange,
    schema::{ColumnType, Schema, document::subschema},
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

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
    /// FileChange a column's type, converting every value. Without `using` a value
    /// converts only when nothing is lost; `using` is a SQL expression over the
    /// row giving the new value.
    ChangeType {
        table: String,
        column: String,
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        using: Option<String>,
        /// The members, when the new type is `enum`.
        #[serde(default)]
        values: Option<Vec<String>>,
        /// Rules of the column that mean nothing for the new type, dropped by
        /// name: without this the change is refused rather than loosened.
        #[serde(default)]
        drop_constraints: Vec<String>,
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
    /// Re-key a table: a new `x-reldir.identity`. Every file is renamed to
    /// its new key, a column read from file names becomes a member of each
    /// body (or the reverse), and every reference to a re-keyed row is
    /// carried by its foreign key's `onUpdate` action.
    ChangeIdentity {
        table: String,
        identity: Value,
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
            Self::ChangeIdentity { table, .. } => format!("re-key {table}"),
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

fn refused(code: Code, message: impl Into<String>) -> DbError {
    DbError::new(code, message)
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
        let first = problems.next().unwrap_or_else(|| {
            Diagnostic::error(
                Code::INTERNAL_ERROR,
                "a schema was refused with no reason given",
            )
        });
        DbError::from_diag(first).with_related(problems.collect())
    })
}

fn column_type(name: &str) -> Result<ColumnType> {
    ColumnType::from_name(name).ok_or_else(|| {
        DbError::from_diag(
            Diagnostic::error(Code::SCHEMA_TYPE_UNKNOWN, format!("unknown type {name:?}")).help(
                "the types are bool, int, float, decimal, string, bytes, date, timestamp, uuid, ulid, \
                 enum, array, object, json",
            ),
        )
    })
}

/// Work out every file change a migration makes. Nothing is written.
pub fn plan(database: &Database, migration: &Migration) -> Result<(Vec<FileChange>, Expected)> {
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
            DbError::from_diag(diagnostic).with_related(error.related)
        })?;
    }
    render(database, plan)
}

fn apply(plan: &mut Plan<'_>, operation: &Operation) -> Result<()> {
    match operation {
        Operation::AddTable { table, schema } => {
            if plan.tables.contains_key(table) {
                return Err(refused(
                    Code::SCHEMA_TABLE_EXISTS,
                    format!("table {table} already exists"),
                ));
            }
            let decoded = decode(table, (**schema).clone())?;
            if decoded.table() != table {
                return Err(refused(
                    Code::SCHEMA_TABLE_NAME_MISMATCH,
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
                    // A table a migration declares is the user's declaration,
                    // pinned like any other; only inference writes contracts.
                    home: PathBuf::from(crate::schema_store::pin_relative(table)),
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
                    Code::SCHEMA_INVALID_TABLE_NAME,
                    format!("{new:?} cannot be a table name"),
                ));
            }
            if plan.tables.contains_key(new) || plan.catalog.root.join(new).exists() {
                return Err(refused(
                    Code::SCHEMA_TABLE_EXISTS,
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
                    Code::SCHEMA_COLUMN_EXISTS,
                    format!("{table}.{column} already exists"),
                ));
            }
            if !nullable && default.is_none() && has_rows {
                return Err(DbError::from_diag(
                    Diagnostic::error(
                        Code::ROW_MISSING_FIELD,
                        format!("{table} has rows, and {column} admits no null, so every row needs a value"),
                    )
                    .help("give a --default, or add the column as --nullable"),
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
                        Code::UNKNOWN_COLUMN,
                        format!("{table}.{column} does not exist"),
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
                        Code::UNKNOWN_COLUMN,
                        format!("{table}.{column} does not exist"),
                    ));
                }
                if editor.column(new).is_some() {
                    return Err(refused(
                        Code::SCHEMA_COLUMN_EXISTS,
                        format!("{table}.{new} already exists"),
                    ));
                }
                editor
                    .rename_column(column, new, table)
                    .map_err(|message| refused(Code::MIGRATION_REFUSED, message))?;
                Ok(())
            })?;
            let names: Vec<String> = plan
                .tables
                .keys()
                .filter(|name| *name != table)
                .cloned()
                .collect();
            for name in &names {
                plan.edit(name, |editor| {
                    editor.rename_referenced_column(table, column, new);
                    Ok(())
                })?;
            }
            // Assertions read any table, so every schema's are renamed.
            let columns: BTreeMap<String, BTreeSet<String>> = plan
                .tables
                .iter()
                .map(|(name, working)| {
                    let declared = working
                        .document
                        .get("properties")
                        .and_then(Value::as_object)
                        .map(|properties| properties.keys().cloned().collect())
                        .unwrap_or_default();
                    (name.clone(), declared)
                })
                .collect();
            let has_column = |table: &str, column: &str| {
                columns.iter().any(|(name, declared)| {
                    name.eq_ignore_ascii_case(table) && declared.contains(column)
                })
            };
            let every: Vec<String> = plan.tables.keys().cloned().collect();
            for name in every {
                plan.edit(&name, |editor| {
                    editor
                        .rename_in_assertions(table, column, new, &has_column)
                        .map_err(|message| refused(Code::MIGRATION_REFUSED, message))?;
                    Ok(())
                })?;
            }
            // The member keeps its place in every row.
            for (_, row) in plan.rows(table)? {
                if row.contains_key(column) {
                    *row = std::mem::take(row)
                        .into_iter()
                        .map(|(key, value)| (if key == *column { new.clone() } else { key }, value))
                        .collect();
                }
            }
        }
        Operation::ChangeType {
            table,
            column,
            kind,
            using,
            values,
            drop_constraints,
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
                        Code::UNKNOWN_COLUMN,
                        format!("{table}.{column} does not exist"),
                    )
                })?;
            let decoded = decode(table, plan.table(table)?.document.clone())?;
            let (nullable, old_kind) = decoded
                .column(column)
                .map(|c| (c.nullable(), c.kind().clone()))
                .unwrap_or((false, ColumnType::Json));
            let replacement = retyped(
                &current,
                &old_kind,
                &kind,
                nullable,
                values.as_deref(),
                drop_constraints,
            )?;
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
                                Code::TYPE_MISMATCH,
                                format!(
                                    "{value} in {at} cannot become a {} without losing information",
                                    kind.name()
                                ),
                            )
                            .help("give --using with a SQL expression that computes the new value"),
                        )
                    })?;
                row.insert(column.clone(), converted);
            }
        }
        Operation::ChangeIdentity { table, identity } => change_identity(plan, table, identity)?,
        Operation::AddConstraint { table, definition } => {
            plan.edit(table, |editor| {
                match definition {
                    Constraint::Unique { columns } => {
                        if !editor.add_list("unique", columns) {
                            return Err(refused(
                                Code::SCHEMA_CONSTRAINT_EXISTS,
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
                        Code::UNKNOWN_CONSTRAINT,
                        format!("{table} has no constraint named {name:?}"),
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
                        Code::SCHEMA_CONSTRAINT_EXISTS,
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
                        Code::UNKNOWN_CONSTRAINT,
                        format!("{table} has no index on ({})", columns.join(",")),
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
/// Re-key a table. Only the schema says which column is the file name and
/// which are the key; a row's values -- including the one its name held --
/// stay as they are, so the files are renamed and rewritten from the same
/// rows under the new identity.
fn change_identity(plan: &mut Plan<'_>, table: &str, identity: &Value) -> Result<()> {
    use crate::naming::{Identity, StemKind};
    let new: Identity = serde_json::from_value(identity.clone()).map_err(|error| {
        DbError::new(
            Code::SCHEMA_IDENTITY_INVALID,
            format!("{identity} is not an identity: {error}"),
        )
    })?;
    let before = decode(table, plan.table(table)?.document.clone())?;
    let old = before.row_identity().clone();
    if old == new {
        return Ok(());
    }
    plan.edit(table, |editor| {
        // A column read from file names that the new identity does not read
        // from them is written into each body instead.
        if let Some((column, kind)) = old.stem_column()
            && new.stem_column().map(|(name, _)| name) != Some(column)
        {
            let kind = match kind {
                StemKind::Int => ColumnType::Int,
                StemKind::String => ColumnType::String,
            };
            editor.add_column(column, subschema(&kind, false), true);
        }
        // A body column the new identity reads from file names leaves the
        // body: its value is the name.
        if let Some((column, kind)) = new.stem_column()
            && old.stem_column().map(|(name, _)| name) != Some(column)
        {
            let declared = before
                .column(column)
                .map(|c| c.kind().clone())
                .ok_or_else(|| {
                    DbError::new(
                        Code::SCHEMA_IDENTITY_INVALID,
                        format!("{table} has no column {column:?} to name its files by"),
                    )
                })?;
            let fits = matches!(
                (kind, &declared),
                (StemKind::Int, ColumnType::Int) | (StemKind::String, ColumnType::String)
            );
            if !fits {
                return Err(DbError::new(
                    Code::SCHEMA_IDENTITY_INVALID,
                    format!(
                        "{table}.{column} is {}, and a file name holds a string or an int",
                        declared.name()
                    ),
                ));
            }
            editor.drop_column(column);
        }
        editor.set_identity(&new);
        Ok(())
    })?;
    let after = decode(table, plan.table(table)?.document.clone())?;
    // Each row's key before and after, as references spell keys.
    let mut rekeyed: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for (_, row) in plan.rows(table)?.iter() {
        let was = crate::mirror::key(row, before.primary_key(), &before);
        let is = crate::mirror::key(row, after.primary_key(), &after);
        if let (Some(was), Some(is)) = (was, is)
            && was != is
        {
            let values = after
                .primary_key()
                .iter()
                .map(|column| row.get(column).cloned().unwrap_or(Value::Null))
                .collect();
            rekeyed.insert(was, values);
        }
    }
    if rekeyed.is_empty() {
        return Ok(());
    }
    // Every reference to a re-keyed row is carried by its onUpdate action.
    let schemas: BTreeMap<String, Schema> = plan
        .tables
        .iter()
        .map(|(name, working)| Ok((name.clone(), decode(name, working.document.clone())?)))
        .collect::<Result<_>>()?;
    for (referencing, schema) in &schemas {
        for fk in schema.foreign_keys() {
            let targets = crate::integrity::target_tables(&schemas, fk.to());
            if !targets.iter().any(|target| target == table) || !fk.columns().is_empty() {
                continue;
            }
            if fk.from().len() != 1
                || before.primary_key().len() != 1
                || after.primary_key().len() != 1
            {
                return Err(refused(
                    Code::MIGRATION_REFUSED,
                    format!(
                        "{} references {table} by a composite key, which re-keying cannot carry; \
                         re-key in steps a single-column key at a time",
                        fk.describe(referencing)
                    ),
                ));
            }
            let path = &fk.from()[0];
            let kind = crate::schema::document::resolve_path(schema, path)
                .map(|leaf| leaf.column.kind().clone())
                .unwrap_or(ColumnType::Json);
            for (original, row) in plan.rows(referencing)?.iter_mut() {
                let mut hits: Vec<(String, Vec<Value>)> = vec![];
                for occurrence in path.occurrences(row) {
                    let spelled =
                        canonical::compact(&Value::Array(vec![crate::value::key_component(
                            occurrence.value,
                            &kind,
                        )]));
                    if let Some(values) = rekeyed.get(&spelled) {
                        hits.push((occurrence.pointer.clone(), values.clone()));
                    }
                }
                if hits.is_empty() {
                    continue;
                }
                let at = original
                    .as_ref()
                    .map(|row| row.relative.display().to_string())
                    .unwrap_or_default();
                match fk.on_update() {
                    crate::schema::Action::Restrict | crate::schema::Action::NoAction => {
                        return Err(DbError::from_diag(
                            Diagnostic::error(
                                Code::FOREIGN_KEY_VIOLATION,
                                format!(
                                    "re-keying {table} would leave {at} naming a row by its old key \
                                     ({}, onUpdate {})",
                                    fk.describe(referencing),
                                    fk.on_update().name()
                                ),
                            )
                            .help("declare onUpdate cascade on the reference to carry the new keys"),
                        ));
                    }
                    crate::schema::Action::Cascade => {
                        for (pointer, values) in hits {
                            crate::referential::set(row, &pointer, values[0].clone())?;
                        }
                    }
                    crate::schema::Action::SetNull => {
                        for (pointer, _) in hits {
                            crate::referential::set(row, &pointer, Value::Null)?;
                        }
                    }
                    crate::schema::Action::SetDefault => {
                        let default = crate::schema::document::resolve_path(schema, path)
                            .ok()
                            .and_then(|leaf| leaf.column.default().cloned())
                            .unwrap_or(Value::Null);
                        for (pointer, _) in hits {
                            crate::referential::set(row, &pointer, default.clone())?;
                        }
                    }
                    crate::schema::Action::Remove => {
                        let mut removals: Vec<Vec<String>> = hits
                            .iter()
                            .map(|(pointer, _)| {
                                let element = path
                                    .occurrences(row)
                                    .into_iter()
                                    .find(|occurrence| &occurrence.pointer == pointer)
                                    .and_then(|occurrence| occurrence.element)
                                    .unwrap_or_else(|| pointer.clone());
                                crate::schema::path::pointer_tokens(&element)
                            })
                            .collect();
                        // From the end, so no removal shifts another.
                        removals
                            .sort_by(|left, right| crate::referential::compare_tokens(right, left));
                        removals.dedup();
                        for tokens in removals {
                            crate::referential::remove_at(row, &tokens)?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// The subschema a column takes when its type changes: the new type's own,
/// keeping its annotations and every rule of the old one that means the same
/// for the new type -- listed values (`enum`, `const`, `default`,
/// `examples`) converted without loss. A rule that cannot carry over is
/// refused by name unless `drop` names it: changing a type never quietly
/// loosens what a column admits.
fn retyped(
    current: &Value,
    old_kind: &ColumnType,
    kind: &ColumnType,
    nullable: bool,
    values: Option<&[String]>,
    drop: &[String],
) -> Result<Value> {
    #[derive(PartialEq)]
    enum Family {
        Number,
        Text,
        Boolean,
        List,
        Record,
        Any,
    }
    let family = match kind {
        ColumnType::Int | ColumnType::Integer | ColumnType::Float | ColumnType::Number => {
            Family::Number
        }
        ColumnType::Bool => Family::Boolean,
        ColumnType::Array => Family::List,
        ColumnType::Object => Family::Record,
        ColumnType::Json => Family::Any,
        _ => Family::Text,
    };
    let convert = |value: &Value| -> Option<Value> {
        if value.is_null() {
            return nullable.then(|| Value::Null);
        }
        crate::value::lossless_convert(value, kind)
            .or_else(|| value_has_type(value, kind).then(|| value.clone()))
    };
    let current = current.as_object().cloned().unwrap_or_default();
    let mut out = match (kind, values) {
        (ColumnType::Enum, Some(values)) => {
            crate::schema::document::enum_subschema(values, nullable)
        }
        (ColumnType::Enum, None) => {
            let members: Option<Vec<String>> =
                current
                    .get("enum")
                    .and_then(Value::as_array)
                    .map(|members| {
                        members
                            .iter()
                            .filter_map(&convert)
                            .filter_map(|m| m.as_str().map(String::from))
                            .collect()
                    });
            match members {
                Some(members) if !members.is_empty() => {
                    crate::schema::document::enum_subschema(&members, nullable)
                }
                _ => {
                    return Err(DbError::usage(
                        "an enum lists its members; give them with --values",
                    ));
                }
            }
        }
        (_, Some(_)) => {
            return Err(DbError::usage(format!(
                "--values lists the members of an enum, and the new type is {}",
                kind.name()
            )));
        }
        _ => subschema(kind, nullable),
    };
    let implied = subschema(old_kind, false);
    let mut dropped: Vec<&String> = vec![];
    for (keyword, value) in &current {
        if keyword == "type" || keyword == crate::schema::meta::TYPE_TAG {
            continue;
        }
        // What the old type itself wrote -- a uuid's format, a decimal's
        // pattern -- goes with the old type.
        if implied.get(keyword) == Some(value) {
            continue;
        }
        if *kind == ColumnType::Enum && keyword == "enum" {
            continue;
        }
        let carried: Option<Value> = match keyword.as_str() {
            "description" | "title" | "$comment" | "deprecated" | "readOnly" | "writeOnly" => {
                Some(value.clone())
            }
            "default" | "const" => convert(value),
            "enum" | "examples" => value
                .as_array()
                .and_then(|members| members.iter().map(&convert).collect::<Option<Vec<_>>>())
                .map(Value::Array),
            _ if family == Family::Any => Some(value.clone()),
            "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" | "multipleOf"
                if family == Family::Number =>
            {
                Some(value.clone())
            }
            "minLength" | "maxLength" | "pattern" | "format" | "contentEncoding"
            | "contentMediaType"
                if family == Family::Text =>
            {
                Some(value.clone())
            }
            "items" | "prefixItems" | "contains" | "minContains" | "maxContains" | "minItems"
            | "maxItems" | "uniqueItems" | "unevaluatedItems"
                if family == Family::List =>
            {
                Some(value.clone())
            }
            "properties"
            | "required"
            | "additionalProperties"
            | "patternProperties"
            | "propertyNames"
            | "minProperties"
            | "maxProperties"
            | "dependentRequired"
            | "dependentSchemas"
            | "unevaluatedProperties"
                if family == Family::Record =>
            {
                Some(value.clone())
            }
            _ => None,
        };
        // The new type may already say this, its own way.
        let carried = carried.filter(|carried| out.get(keyword).is_none_or(|own| own == carried));
        match carried {
            Some(carried) => {
                out[keyword] = carried;
            }
            None if drop.contains(keyword) => dropped.push(keyword),
            None => {
                return Err(refused(
                    Code::MIGRATION_REFUSED,
                    format!(
                        "`{keyword}` {} does not carry over to a {} column; name it with \
                         --drop-constraint {keyword} to drop it",
                        crate::canonical::compact(value),
                        kind.name()
                    ),
                ));
            }
        }
    }
    if let Some(unknown) = drop.iter().find(|name| !dropped.contains(name)) {
        return Err(DbError::usage(format!(
            "--drop-constraint {unknown}: the column has no `{unknown}` that would need dropping"
        )));
    }
    Ok(out)
}

fn value_has_type(value: &Value, kind: &ColumnType) -> bool {
    let probe = crate::schema::document::TableBuilder::new("probe")
        .column("id", subschema(&ColumnType::Int, false), true)
        .column("v", subschema(kind, false), true)
        .key(vec!["id".into()])
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
    let expression = crate::sql::expression(expression)
        .map_err(|reason| DbError::usage(format!("--using {expression:?} {reason}")))?;
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
            cancel: None,
            max_rows: usize::MAX,
            max_memory: 1 << 30,
        },
        |row| {
            let key = row
                .get("_reldir_key")
                .and_then(Value::as_str)
                .and_then(|key| crate::json::parse_str(key).ok())
                .map(|value| canonical::compact(&value))
                .ok_or_else(|| {
                    DbError::new(
                        Code::INTERNAL_ERROR,
                        "a row's key did not read back as JSON",
                    )
                })?;
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
fn render(database: &Database, plan: Plan<'_>) -> Result<(Vec<FileChange>, Expected)> {
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
            let previous = plan
                .catalog
                .schema_files
                .get(table)
                .filter(|file| file.relative == working.home)
                .map(|file| file.bytes.as_slice());
            writes.insert(working.home.clone(), schema.render(previous)?);
        }
        let Some(rows) = &working.rows else { continue };
        for (before, value) in rows {
            let path = crate::plan::row_path(&schema, value)?;
            let held = before
                .as_ref()
                .map(|row| row.raw(&plan.catalog.root))
                .transpose()?;
            let bytes = crate::plan::row_bytes(&schema, held.as_deref(), value)?;
            if let Some(before) = before
                && before.relative != path
            {
                deletes.push(before.relative.clone());
            }
            if writes.insert(path.clone(), bytes).is_some() {
                return Err(DbError::from_diag(
                    Diagnostic::error(
                        Code::PRIMARY_KEY_VIOLATION,
                        format!("two rows would both be written to {}", path.display()),
                    )
                    .at(path),
                ));
            }
        }
    }
    let mut expected = Expected::new();
    let mut changes: Vec<FileChange> = vec![];
    for path in deletes {
        if writes.contains_key(&path) {
            continue;
        }
        expected.insert(path.clone(), database.fingerprint(&path)?);
        changes.push(FileChange::Delete { path });
    }
    for (path, bytes) in writes {
        let now = database.fingerprint(&path)?;
        if now.as_deref() == Some(canonical::hash_bytes(&bytes).as_str()) {
            continue;
        }
        expected.insert(path.clone(), now);
        changes.push(FileChange::Write { path, bytes });
    }
    Ok((changes, expected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn carried(current: Value, from: ColumnType, to: ColumnType, drop: &[&str]) -> Result<Value> {
        let drop: Vec<String> = drop.iter().map(|name| name.to_string()).collect();
        retyped(&current, &from, &to, false, None, &drop)
    }

    #[test]
    fn test2240_a_type_change_keeps_every_rule_that_still_means_something() {
        let int = json!({"type": "integer", "x-reldir-type": "int", "minimum": 0, "maximum": 9,
            "default": 3, "description": "how many"});
        assert_eq!(
            carried(int.clone(), ColumnType::Int, ColumnType::Number, &[]).unwrap(),
            json!({"type": "number", "minimum": 0, "maximum": 9, "default": 3, "description": "how many"}),
            "bounds and the default carry to a wider number"
        );
        let refused = carried(int.clone(), ColumnType::Int, ColumnType::String, &[]).unwrap_err();
        assert_eq!(refused.code(), Code::MIGRATION_REFUSED);
        assert!(
            refused.diagnostic.message.contains("minimum"),
            "{}",
            refused.diagnostic.message
        );
        assert_eq!(
            carried(
                int,
                ColumnType::Int,
                ColumnType::String,
                &["minimum", "maximum"]
            )
            .unwrap(),
            json!({"type": "string", "default": "3", "description": "how many"}),
            "what does not carry is dropped only by name; the default converts"
        );

        let uuid = json!({"type": "string", "format": "uuid", "minLength": 36});
        assert_eq!(
            carried(uuid, ColumnType::Uuid, ColumnType::String, &[]).unwrap(),
            json!({"type": "string", "minLength": 36}),
            "the old type's own format goes with it"
        );

        let decimal_text = json!({"type": "string", "pattern": "^[a-z]+$"});
        assert!(
            carried(decimal_text, ColumnType::String, ColumnType::Decimal, &[]).is_err(),
            "a pattern the new type writes its own way is a conflict, not a carry"
        );

        let listed = json!({"type": "string", "enum": ["1", "2"], "default": "1"});
        assert_eq!(
            carried(listed, ColumnType::Enum, ColumnType::Int, &[]).unwrap(),
            json!({"type": "integer", "x-reldir-type": "int", "enum": [1, 2], "default": 1}),
            "listed values are converted"
        );
        assert!(
            carried(
                json!({"type": "string", "enum": ["a"]}),
                ColumnType::Enum,
                ColumnType::Int,
                &[]
            )
            .is_err(),
            "a value that does not convert does not carry"
        );
        assert!(
            carried(
                json!({"type": "string"}),
                ColumnType::String,
                ColumnType::Enum,
                &[]
            )
            .is_err()
        );
        assert!(
            carried(
                json!({"type": "string"}),
                ColumnType::String,
                ColumnType::Int,
                &["pattern"]
            )
            .is_err(),
            "dropping a rule the column does not have is a mistake to report"
        );
    }

    #[test]
    fn test2241_a_renamed_member_is_renamed_wherever_the_object_names_it() {
        let mut schema = json!({
            "properties": {"a": {}, "b": {}, "c": {}},
            "required": ["a"],
            "dependentRequired": {"b": ["a"]},
            "dependentSchemas": {"a": {"required": ["c"]}},
            "if": {"properties": {"a": {"const": 1}}, "required": ["a"]},
            "then": {"required": ["b"]},
            "allOf": [{"not": {"required": ["a", "c"]}}],
            "propertyNames": {"enum": ["a", "b", "c"]}
        });
        crate::schema::document::rename_member(schema.as_object_mut().unwrap(), "a", "z").unwrap();
        assert_eq!(
            schema,
            json!({
                "properties": {"z": {}, "b": {}, "c": {}},
                "required": ["z"],
                "dependentRequired": {"b": ["z"]},
                "dependentSchemas": {"z": {"required": ["c"]}},
                "if": {"properties": {"z": {"const": 1}}, "required": ["z"]},
                "then": {"required": ["b"]},
                "allOf": [{"not": {"required": ["z", "c"]}}],
                "propertyNames": {"enum": ["z", "b", "c"]}
            })
        );
        assert_eq!(
            schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["z", "b", "c"],
            "the member keeps its place"
        );
        let mut patterned = json!({"properties": {"a": {}}, "patternProperties": {"^z": {}}});
        assert!(
            crate::schema::document::rename_member(patterned.as_object_mut().unwrap(), "a", "zed")
                .is_err()
        );
        let mut referring = json!({"properties": {"a": {}}, "$ref": "#/$defs/x"});
        assert!(
            crate::schema::document::rename_member(referring.as_object_mut().unwrap(), "a", "b")
                .is_err()
        );
    }

    #[test]
    fn test2242_a_nullable_enum_admits_null() {
        let schema = crate::schema::document::TableBuilder::new("t")
            .column("id", subschema(&ColumnType::String, false), true)
            .column(
                "state",
                crate::schema::document::enum_subschema(&["open".into()], true),
                true,
            )
            .key(vec!["id".into()])
            .build()
            .unwrap();
        assert!(
            schema
                .validator()
                .is_valid(&json!({"id": "a", "state": null}))
        );
        assert!(
            !schema
                .validator()
                .is_valid(&json!({"id": "a", "state": "shut"}))
        );
    }
}
