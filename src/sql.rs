//! SQL over the governed directory.
//!
//! SQL runs on the mirror's typed tables, one per governed table, in SQLite's
//! dialect. What a statement may touch is enforced by SQLite's authorizer at
//! the moment it is compiled -- not guessed from its text:
//!
//! - a query reads governed tables and the `json_each` / `json_tree`
//!   table-valued functions, with any function, subquery, CTE, window or
//!   compound it likes;
//! - a mutation (`INSERT`, including `ON CONFLICT` upserts and `REPLACE`;
//!   `UPDATE`; `DELETE`; each with `RETURNING`) may also write governed tables;
//! - nothing may read reldir's own bookkeeping, attach a database, run a
//!   pragma, define or drop anything, or control a transaction.
//!
//! A mutation never writes the mirror. It runs inside a savepoint with capture
//! triggers recording every row it touches, the resulting rows are read back
//! as JSON, and the savepoint is rolled back. What the statement *would* do
//! becomes a plan of row changes, which the one mutation path turns into
//! files -- after referential actions and full validation.
//!
//! Checks and assertions are SQL too, held to a stricter rule: they must be
//! deterministic, because validity cannot depend on the clock or on chance.

use crate::{
    diagnostic::{DbError, Diagnostic, Result},
    mirror::{self, Mirror},
    plan::RowChange,
    schema::{Assertion, Check, Schema},
};
use rusqlite::{
    Connection,
    hooks::{AuthAction, AuthContext, Authorization},
};
use serde_json::{Map, Value};
use sqlparser::{ast::Statement, dialect::SQLiteDialect, parser::Parser};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug)]
pub struct SqlParam {
    pub name: Option<String>,
    pub value: Value,
}

/// What a query may consume.
#[derive(Clone, Copy, Debug)]
pub struct QueryLimits {
    pub timeout: Option<std::time::Duration>,
    pub max_rows: usize,
    /// Bytes of memory SQLite may allocate while the statement runs, including
    /// sorting and temporary storage, which are kept in memory.
    pub max_memory: u64,
}

/// Whether a statement reads or writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Read,
    Mutation,
}

/// Parse and classify one statement, refusing anything outside the supported
/// surface with the reason and where it lies.
pub fn classify(text: &str) -> Result<StatementKind> {
    let dialect = SQLiteDialect {};
    let failed = |message: String, location: Option<crate::diagnostic::Location>| {
        let mut diagnostic = Diagnostic::error(
            "QUERY_UNSUPPORTED",
            format!("SQL does not parse: {message}"),
        );
        diagnostic.source_line = location
            .as_ref()
            .and_then(|location| text.lines().nth(location.line.saturating_sub(1)))
            .map(String::from);
        diagnostic.location = location;
        DbError::from_diag(diagnostic, 4)
    };
    let mut parser = Parser::new(&dialect)
        .try_with_sql(text)
        .map_err(|error| failed(error.to_string(), sql_error_location(&error.to_string())))?;
    let statements = match parser.parse_statements() {
        Ok(statements) => statements,
        Err(error) => {
            // The parser stops at the token it could not accept; that is
            // where the fault is.
            let mut start = parser.get_current_token().span.start;
            if start.line == 0 {
                start = parser.get_previous_token().span.start;
            }
            let location = (start.line > 0).then_some(crate::diagnostic::Location {
                line: start.line as usize,
                column: start.column as usize,
            });
            let message = error.to_string();
            return Err(failed(
                message.clone(),
                sql_error_location(&message).or(location),
            ));
        }
    };
    let [statement] = statements.as_slice() else {
        return Err(DbError::new(
            "QUERY_UNSUPPORTED",
            format!(
                "exactly one SQL statement is required; {} were given",
                statements.len()
            ),
            4,
        ));
    };
    match statement {
        Statement::Query(query) => Ok(match query.body.as_ref() {
            sqlparser::ast::SetExpr::Insert(_)
            | sqlparser::ast::SetExpr::Update(_)
            | sqlparser::ast::SetExpr::Delete(_) => StatementKind::Mutation,
            _ => StatementKind::Read,
        }),
        Statement::Insert(_) | Statement::Update { .. } | Statement::Delete(_) => {
            Ok(StatementKind::Mutation)
        }
        Statement::Explain { .. } | Statement::ExplainTable { .. } => Ok(StatementKind::Read),
        other => {
            let keyword = other
                .to_string()
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            Err(DbError::from_diag(
                Diagnostic::error(
                    "QUERY_UNSUPPORTED",
                    format!(
                        "{keyword} is not supported: SQL here reads and changes rows; the structure \
                         of the database is its schemas"
                    ),
                )
                .help("change a schema with `reldir migrate`, or by editing its file"),
                4,
            ))
        }
    }
}

/// Whether a statement deletes rows: a `DELETE`, which removes data a
/// person may need to confirm removing.
pub fn deletes(text: &str) -> bool {
    Parser::parse_sql(&SQLiteDialect {}, text)
        .map(|statements| {
            statements
                .iter()
                .any(|statement| matches!(statement, Statement::Delete(_)))
        })
        .unwrap_or(false)
}

fn sql_error_location(message: &str) -> Option<crate::diagnostic::Location> {
    let marker = "Line: ";
    let start = message.find(marker)? + marker.len();
    let rest = &message[start..];
    let (line, rest) = rest.split_once(", Column: ")?;
    let column = rest
        .split(|character: char| !character.is_ascii_digit())
        .next()?;
    Some(crate::diagnostic::Location {
        line: line.parse().ok()?,
        column: column.parse().ok()?,
    })
}

/// What an authorizer admits.
#[derive(Clone)]
struct Policy {
    /// Tables that may be read.
    readable: BTreeSet<String>,
    /// Tables that may be written.
    writable: BTreeSet<String>,
    /// Whether functions must be deterministic.
    deterministic: bool,
    /// The first refused action, to name in the error.
    refused: Arc<Mutex<Option<String>>>,
}

/// Functions whose result depends on something other than their arguments.
const NONDETERMINISTIC: &[&str] = &[
    "random",
    "randomblob",
    "changes",
    "total_changes",
    "last_insert_rowid",
    "sqlite_offset",
];

fn install(connection: &Connection, policy: &Policy) {
    let policy = policy.clone();
    connection.authorizer(Some(move |context: AuthContext<'_>| {
        if context.accessor.is_some_and(|name| name.starts_with("_reldir_capture_")) {
            return Authorization::Allow;
        }
        let decision = match context.action {
            AuthAction::Select | AuthAction::Recursive => Ok(()),
            AuthAction::Read { table_name, .. } => {
                if policy.readable.contains(table_name) || matches!(table_name, "json_each" | "json_tree") {
                    Ok(())
                } else {
                    Err(format!("reading {table_name:?}, which is not a governed table"))
                }
            }
            AuthAction::Function { function_name } => {
                if policy.deterministic && NONDETERMINISTIC.contains(&function_name.to_ascii_lowercase().as_str()) {
                    Err(format!("{function_name}() is not deterministic, and validity cannot depend on chance"))
                } else {
                    Ok(())
                }
            }
            AuthAction::Insert { table_name }
            | AuthAction::Delete { table_name }
            | AuthAction::Update { table_name, .. } => {
                if policy.writable.contains(table_name) {
                    Ok(())
                } else {
                    Err(format!("writing {table_name:?}"))
                }
            }
            AuthAction::Pragma { pragma_name, .. } => Err(format!("PRAGMA {pragma_name}")),
            AuthAction::Attach { .. } => Err("ATTACH".into()),
            AuthAction::Detach { .. } => Err("DETACH".into()),
            AuthAction::Transaction { .. } | AuthAction::Savepoint { .. } => {
                Err("transaction control; every statement is its own transaction".into())
            }
            other => Err(format!("{other:?}")),
        };
        match decision {
            Ok(()) => Authorization::Allow,
            Err(reason) => {
                let mut refused = policy.refused.lock().unwrap_or_else(|p| p.into_inner());
                if refused.is_none() {
                    *refused = Some(reason);
                }
                Authorization::Deny
            }
        }
    }));
}

fn uninstall(connection: &Connection) {
    connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
}

/// Prepare a statement under a policy, turning a refusal into a diagnostic that
/// names what was refused.
fn prepare<'c>(
    connection: &'c Connection,
    text: &str,
    policy: &Policy,
) -> Result<rusqlite::Statement<'c>> {
    install(connection, policy);
    let prepared = connection.prepare(text);
    uninstall(connection);
    prepared.map_err(|error| {
        let refused = policy
            .refused
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        match refused {
            Some(reason) => DbError::from_diag(
                Diagnostic::error(
                    "QUERY_UNSUPPORTED",
                    format!("the statement is refused: {reason}"),
                ),
                4,
            ),
            None => query_err(error),
        }
    })
}

fn policy(schemas: &BTreeMap<String, Schema>, mutation: bool, deterministic: bool) -> Policy {
    let tables: BTreeSet<String> = schemas.keys().cloned().collect();
    Policy {
        writable: if mutation {
            tables.clone()
        } else {
            BTreeSet::new()
        },
        readable: tables,
        deterministic,
        refused: Arc::new(Mutex::new(None)),
    }
}

/// Apply the resource limits to a connection for the duration of a statement.
fn limit(connection: &Connection, limits: &QueryLimits) -> Result<()> {
    connection
        .execute_batch(&format!(
            "PRAGMA temp_store=MEMORY; PRAGMA hard_heap_limit={};",
            limits.max_memory
        ))
        .map_err(query_err)?;
    if let Some(timeout) = limits.timeout {
        let started = std::time::Instant::now();
        connection.progress_handler(1000, Some(move || started.elapsed() >= timeout));
    }
    Ok(())
}

fn unlimit(connection: &Connection) {
    let _ = connection.execute_batch("PRAGMA hard_heap_limit=0;");
    connection.progress_handler(0, None::<fn() -> bool>);
}

/// Run a read statement, handing each result row to `emit` as it is produced.
pub fn query(
    mirror: &Mirror,
    schemas: &BTreeMap<String, Schema>,
    text: &str,
    params: &[SqlParam],
    limits: QueryLimits,
    mut emit: impl FnMut(Map<String, Value>) -> Result<()>,
) -> Result<usize> {
    if classify(text)? != StatementKind::Read {
        return Err(DbError::new(
            "QUERY_UNSUPPORTED",
            "a query must not change rows; run the mutation as its own statement",
            4,
        ));
    }
    let connection = mirror.connection();
    let policy = policy(schemas, false, false);
    let mut statement = prepare(connection, text, &policy)?;
    bind(&mut statement, params)?;
    limit(connection, &limits)?;
    let outcome = (|| -> Result<usize> {
        let names: Vec<String> = statement
            .column_names()
            .into_iter()
            .map(String::from)
            .collect();
        let declared: Vec<Option<String>> = statement
            .columns()
            .into_iter()
            .map(|column| {
                column
                    .decl_type()
                    .map(|t| t.split_whitespace().next().unwrap_or("").to_string())
            })
            .collect();
        let mut rows = statement.raw_query();
        let mut emitted = 0usize;
        while let Some(row) = rows.next().map_err(query_err)? {
            if emitted == limits.max_rows {
                return Err(DbError::new(
                    "RESOURCE_LIMIT",
                    format!(
                        "the query produces more than {} rows, the configured limit; nothing past it \
                         is returned, so no partial answer is mistaken for a whole one",
                        limits.max_rows
                    ),
                    4,
                ));
            }
            let mut object = Map::new();
            for (index, name) in names.iter().enumerate() {
                let value = mirror::from_sql(row.get_ref(index).map_err(query_err)?);
                object.insert(
                    name.clone(),
                    mirror::decode_declared(value, declared[index].as_deref())?,
                );
            }
            emit(object)?;
            emitted += 1;
        }
        Ok(emitted)
    })();
    drop(statement);
    unlimit(connection);
    outcome
}

/// A read statement's plan, as SQLite states it.
pub fn plan(
    mirror: &Mirror,
    schemas: &BTreeMap<String, Schema>,
    text: &str,
    params: &[SqlParam],
) -> Result<Vec<Map<String, Value>>> {
    classify(text)?;
    let connection = mirror.connection();
    let policy = policy(schemas, true, false);
    // The plan of a statement is taken without running it, so preparing it
    // under the mutation policy is safe: nothing is stepped.
    let mut statement = prepare(connection, &format!("EXPLAIN QUERY PLAN {text}"), &policy)?;
    bind(&mut statement, params)?;
    let mut rows = statement.raw_query();
    let mut out = vec![];
    while let Some(row) = rows.next().map_err(query_err)? {
        let mut step = Map::new();
        step.insert(
            "id".into(),
            mirror::from_sql(row.get_ref(0).map_err(query_err)?),
        );
        step.insert(
            "parent".into(),
            mirror::from_sql(row.get_ref(1).map_err(query_err)?),
        );
        step.insert(
            "detail".into(),
            mirror::from_sql(row.get_ref(3).map_err(query_err)?),
        );
        out.push(step);
    }
    Ok(out)
}

/// What a mutation would do.
#[derive(Debug, Default)]
pub struct Mutation {
    /// Every row the statement inserts, changes or deletes.
    pub rows: Vec<RowChange>,
    /// Rows the statement's `RETURNING` clause produced.
    pub returning: Vec<Map<String, Value>>,
}

/// Work out what a mutation would do, without doing it.
pub fn mutate(
    catalog: &crate::catalog::Catalog,
    text: &str,
    params: &[SqlParam],
    limits: QueryLimits,
) -> Result<Mutation> {
    if classify(text)? != StatementKind::Mutation {
        return Err(DbError::new("QUERY_UNSUPPORTED", "not a mutation", 4));
    }
    let mirror = &catalog.mirror;
    let connection = mirror.connection();
    mirror.savepoint("reldir_mutation")?;
    let outcome = (|| -> Result<Mutation> {
        connection
            .execute_batch(
                "PRAGMA recursive_triggers=ON; \
                 CREATE TEMP TABLE IF NOT EXISTS _reldir_changes \
                 (tbl TEXT NOT NULL, op TEXT NOT NULL, old_rowid INTEGER, new_rowid INTEGER); \
                 DELETE FROM temp._reldir_changes;",
            )
            .map_err(query_err)?;
        for table in catalog.schemas.keys() {
            let quoted = mirror::quote(table);
            let literal = table.replace('\'', "''");
            connection
                .execute_batch(&format!(
                    "CREATE TEMP TRIGGER \"_reldir_capture_{table}_i\" AFTER INSERT ON main.{quoted} BEGIN \
                       INSERT INTO _reldir_changes VALUES ('{literal}', 'insert', NULL, NEW.rowid); END; \
                     CREATE TEMP TRIGGER \"_reldir_capture_{table}_u\" AFTER UPDATE ON main.{quoted} BEGIN \
                       INSERT INTO _reldir_changes VALUES ('{literal}', 'update', OLD.rowid, NEW.rowid); END; \
                     CREATE TEMP TRIGGER \"_reldir_capture_{table}_d\" AFTER DELETE ON main.{quoted} BEGIN \
                       INSERT INTO _reldir_changes VALUES ('{literal}', 'delete', OLD.rowid, NULL); END;"
                ))
                .map_err(query_err)?;
        }
        let policy = policy(&catalog.schemas, true, false);
        let mut statement = prepare(connection, text, &policy)?;
        bind(&mut statement, params)?;
        limit(connection, &limits)?;
        let names: Vec<String> = statement
            .column_names()
            .into_iter()
            .map(String::from)
            .collect();
        let declared: Vec<Option<String>> = statement
            .columns()
            .into_iter()
            .map(|column| {
                column
                    .decl_type()
                    .map(|t| t.split_whitespace().next().unwrap_or("").to_string())
            })
            .collect();
        let mut returning = vec![];
        {
            let mut rows = statement.raw_query();
            while let Some(row) = rows.next().map_err(query_err)? {
                let mut object = Map::new();
                for (index, name) in names.iter().enumerate() {
                    let value = mirror::from_sql(row.get_ref(index).map_err(query_err)?);
                    object.insert(
                        name.clone(),
                        mirror::decode_declared(value, declared[index].as_deref())?,
                    );
                }
                returning.push(object);
            }
        }
        drop(statement);
        unlimit(connection);
        let rows = captured(catalog)?;
        Ok(Mutation { rows, returning })
    })();
    unlimit(connection);
    mirror.rollback_to("reldir_mutation")?;
    outcome
}

/// The row changes the capture triggers recorded, read back as JSON.
fn captured(catalog: &crate::catalog::Catalog) -> Result<Vec<RowChange>> {
    let connection = catalog.mirror.connection();
    let mut per_table: BTreeMap<String, (BTreeSet<i64>, BTreeSet<i64>)> = BTreeMap::new();
    {
        let mut statement = connection
            .prepare(
                "SELECT tbl, op, old_rowid, new_rowid FROM temp._reldir_changes ORDER BY rowid",
            )
            .map_err(query_err)?;
        let mut rows = statement.query([]).map_err(query_err)?;
        while let Some(row) = rows.next().map_err(query_err)? {
            let table: String = row.get(0).map_err(query_err)?;
            let op: String = row.get(1).map_err(query_err)?;
            let old: Option<i64> = row.get(2).map_err(query_err)?;
            let new: Option<i64> = row.get(3).map_err(query_err)?;
            let entry = per_table.entry(table).or_default();
            match op.as_str() {
                "delete" => {
                    entry.0.extend(old);
                }
                _ => {
                    entry.1.extend(old);
                    entry.1.extend(new);
                }
            }
        }
    }
    let mut out = vec![];
    for (table, (deleted, touched)) in per_table {
        let schema = &catalog.schemas[&table];
        let names: Vec<&String> = schema.columns().keys().collect();
        let select = format!(
            "SELECT {} FROM {} WHERE rowid = ?1",
            names
                .iter()
                .map(|n| mirror::quote(n))
                .collect::<Vec<_>>()
                .join(", "),
            mirror::quote(&table)
        );
        let mut next_sequence: BTreeMap<String, i64> = BTreeMap::new();
        for rowid in deleted.union(&touched) {
            // The file this rowid held before the statement, if any: the
            // capture changed only the typed table, never the file index.
            let before_path = catalog.mirror.path_of_rowid(&table, *rowid)?;
            let before = match &before_path {
                Some(path) => catalog.row_at(std::path::Path::new(path))?,
                None => None,
            };
            let current: Option<Vec<Value>> = connection
                .query_row(&select, [rowid], |row| {
                    (0..names.len())
                        .map(|index| row.get_ref(index).map(mirror::from_sql))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .map(Some)
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })
                .map_err(query_err)?;
            let replaced = deleted.contains(rowid);
            let after = match current {
                None => None,
                Some(values) => {
                    // A rowid deleted and then reused by an insert holds a new
                    // row; it inherits nothing from the one it replaced.
                    let mut row = match (&before, replaced) {
                        (Some(before), false) => before.value.clone(),
                        _ => Map::new(),
                    };
                    let inserted = before.is_none() || replaced;
                    for ((name, column), value) in schema.columns().iter().zip(values) {
                        let decoded = mirror::decode_typed(value, column.kind())?;
                        if decoded.is_null()
                            && inserted
                            && let Some(generated) = column.generated()
                        {
                            let sequence = match next_sequence.get(name) {
                                Some(next) => *next,
                                None => sequence_start(connection, &table, name)?,
                            };
                            next_sequence.insert(name.clone(), sequence + 1);
                            row.insert(name.clone(), crate::value::generate(generated, sequence));
                        } else if decoded.is_null() && !row.contains_key(name) {
                            // An absent member stays absent rather than
                            // becoming an explicit null the row never said.
                        } else {
                            row.insert(name.clone(), decoded);
                        }
                    }
                    Some(row)
                }
            };
            match (&before, replaced, &after) {
                (Some(before), true, Some(after)) => {
                    out.push(RowChange::delete(before.clone()));
                    out.push(RowChange::insert(&table, after.clone()));
                }
                (Some(before), _, None) => out.push(RowChange::delete(before.clone())),
                (Some(before), false, Some(after)) => {
                    if &before.value != after {
                        out.push(RowChange::update(before.clone(), after.clone()));
                    }
                }
                (None, _, Some(after)) => out.push(RowChange::insert(&table, after.clone())),
                (None, _, None) => {}
            }
        }
    }
    Ok(out)
}

/// The next value a sequence-generated column hands out: one past the largest
/// integer the table holds there.
pub fn sequence_start(connection: &Connection, table: &str, column: &str) -> Result<i64> {
    let max: Option<i64> = connection
        .query_row(
            &format!(
                "SELECT max({}) FROM {} WHERE typeof({}) = 'integer'",
                mirror::quote(column),
                mirror::quote(table),
                mirror::quote(column)
            ),
            [],
            |row| row.get(0),
        )
        .map_err(query_err)?;
    max.unwrap_or(0).checked_add(1).ok_or_else(|| {
        DbError::new(
            "RESOURCE_LIMIT",
            format!("the sequence for {table}.{column} has reached the largest int"),
            2,
        )
    })
}

/// Compile every check and assertion against the tables they may read,
/// reporting each that cannot be compiled or is not deterministic.
pub fn compile_rules(schemas: &BTreeMap<String, Schema>) -> Vec<Diagnostic> {
    let mut out = vec![];
    let Ok(scratch) = Mirror::open_memory() else {
        return vec![Diagnostic::error(
            "INTERNAL_METADATA_CORRUPT",
            "cannot open a scratch database",
        )];
    };
    if let Err(error) = scratch.sync_schemas(schemas) {
        out.push(*error.diagnostic);
        return out;
    }
    let connection = scratch.connection();
    for (table, schema) in schemas {
        for (index, check) in schema.checks().iter().enumerate() {
            let only_this: BTreeMap<String, Schema> =
                BTreeMap::from([(table.clone(), schema.clone())]);
            let mut rule = policy(&only_this, false, true);
            rule.readable = BTreeSet::from([table.clone()]);
            let text = format!("SELECT ({}) FROM {}", check.expr(), mirror::quote(table));
            if let Some(reason) = refuses_clock(check.expr()) {
                out.push(
                    Diagnostic::error(
                        "SCHEMA_CHECK_INVALID",
                        format!("check {:?}: {reason}", check.name()),
                    )
                    .table(table)
                    .pointer(format!("/x-reldir/checks/{index}/expr")),
                );
                continue;
            }
            if let Err(error) = prepare(connection, &text, &rule) {
                out.push(
                    Diagnostic::error(
                        "SCHEMA_CHECK_INVALID",
                        format!(
                            "check {:?} is not a valid expression over {table}'s columns: {}",
                            check.name(),
                            error.diagnostic.message
                        ),
                    )
                    .table(table)
                    .pointer(format!("/x-reldir/checks/{index}/expr")),
                );
            }
        }
        for (index, assertion) in schema.assertions().iter().enumerate() {
            let at = format!("/x-reldir/assertions/{index}/query");
            if let Some(reason) = refuses_clock(assertion.query()) {
                out.push(
                    Diagnostic::error(
                        "SCHEMA_ASSERTION_INVALID",
                        format!("assertion {:?}: {reason}", assertion.name()),
                    )
                    .table(table)
                    .pointer(at),
                );
                continue;
            }
            match classify(assertion.query()) {
                Ok(StatementKind::Read) => {}
                _ => {
                    out.push(
                        Diagnostic::error(
                            "SCHEMA_ASSERTION_INVALID",
                            format!("assertion {:?} must be a single SELECT", assertion.name()),
                        )
                        .table(table)
                        .pointer(at),
                    );
                    continue;
                }
            }
            let rule = policy(schemas, false, true);
            match prepare(connection, assertion.query(), &rule) {
                Err(error) => out.push(
                    Diagnostic::error(
                        "SCHEMA_ASSERTION_INVALID",
                        format!(
                            "assertion {:?} does not compile: {}",
                            assertion.name(),
                            error.diagnostic.message
                        ),
                    )
                    .table(table)
                    .pointer(at),
                ),
                Ok(statement) => {
                    let columns: Vec<String> = statement
                        .column_names()
                        .into_iter()
                        .map(String::from)
                        .collect();
                    if columns != schema.primary_key() {
                        out.push(
                            Diagnostic::error(
                                "SCHEMA_ASSERTION_INVALID",
                                format!(
                                    "assertion {:?} returns ({}), but it names violating rows by the \
                                     table's primary key ({})",
                                    assertion.name(),
                                    columns.join(", "),
                                    schema.primary_key().join(", ")
                                ),
                            )
                            .table(table)
                            .pointer(at),
                        );
                    }
                }
            }
        }
    }
    out
}

/// A rule that reads the clock is not a rule about the data.
fn refuses_clock(text: &str) -> Option<String> {
    let lowered = text.to_ascii_lowercase();
    for keyword in ["current_timestamp", "current_date", "current_time"] {
        if lowered.contains(keyword) {
            return Some(format!(
                "{keyword} reads the clock, and validity cannot change with time"
            ));
        }
    }
    if lowered.contains("'now'") {
        return Some("'now' reads the clock, and validity cannot change with time".into());
    }
    None
}

/// The files whose rows fail a check.
pub fn check_violations(mirror: &Mirror, schema: &Schema, check: &Check) -> Result<Vec<String>> {
    let connection = mirror.connection();
    let text = format!(
        "SELECT f.path FROM {table} t JOIN _reldir_files f ON f.tbl = ?1 AND f.trow = t.rowid \
         WHERE NOT ({expr}) ORDER BY f.path",
        table = mirror::quote(schema.table()),
        expr = check.expr()
    );
    let mut statement = connection.prepare(&text).map_err(query_err)?;
    let found = statement
        .query_map([schema.table()], |row| row.get::<_, String>(0))
        .map_err(query_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(query_err)?;
    Ok(found)
}

/// The files whose rows an assertion names.
pub fn assertion_violations(
    mirror: &Mirror,
    schema: &Schema,
    assertion: &Assertion,
) -> Result<Vec<String>> {
    let connection = mirror.connection();
    let mut statement = connection.prepare(assertion.query()).map_err(query_err)?;
    let count = statement.column_count();
    let mut rows = statement.query([]).map_err(query_err)?;
    let mut keys = BTreeSet::new();
    while let Some(row) = rows.next().map_err(query_err)? {
        let mut values = Map::new();
        for (index, column) in schema.primary_key().iter().enumerate().take(count) {
            let kind = schema
                .column(column)
                .map(|c| c.kind().clone())
                .unwrap_or(crate::schema::ColumnType::Json);
            let value = mirror::from_sql(row.get_ref(index).map_err(query_err)?);
            values.insert(column.clone(), mirror::decode_typed(value, &kind)?);
        }
        if let Some(key) = mirror::key(&values, schema.primary_key(), schema) {
            keys.insert(key);
        }
    }
    let mut paths = vec![];
    for key in keys {
        for (_, path) in mirror.holders(&key, mirror::PRIMARY, &[schema.table().to_string()])? {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// Rename a column wherever an expression names it, leaving strings, function
/// names and other identifiers alone.
pub fn rename_identifier(
    expression: &str,
    old: &str,
    new: &str,
) -> std::result::Result<String, String> {
    let chars: Vec<char> = expression.chars().collect();
    let simple = |name: &str| {
        let mut characters = name.chars();
        matches!(characters.next(), Some(c) if c.is_alphabetic() || c == '_')
            && characters.all(|c| c.is_alphanumeric() || c == '_')
    };
    let render = |name: &str| {
        if simple(name) {
            name.to_string()
        } else {
            mirror::quote(name)
        }
    };
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\'' => {
                out.push('\'');
                i += 1;
                while i < chars.len() {
                    out.push(chars[i]);
                    if chars[i] == '\'' {
                        if chars.get(i + 1) == Some(&'\'') {
                            out.push('\'');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            '"' => {
                i += 1;
                let mut identifier = String::new();
                loop {
                    match chars.get(i) {
                        None => {
                            return Err(format!(
                                "unterminated quoted identifier in {expression:?}"
                            ));
                        }
                        Some('"') if chars.get(i + 1) == Some(&'"') => {
                            identifier.push('"');
                            i += 2;
                        }
                        Some('"') => {
                            i += 1;
                            break;
                        }
                        Some(c) => {
                            identifier.push(*c);
                            i += 1;
                        }
                    }
                }
                if identifier == old {
                    out.push_str(&mirror::quote(new));
                } else {
                    out.push_str(&mirror::quote(&identifier));
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let mut lookahead = i;
                while chars.get(lookahead).is_some_and(|c| c.is_whitespace()) {
                    lookahead += 1;
                }
                let is_call = chars.get(lookahead) == Some(&'(');
                if word == old && !is_call {
                    out.push_str(&render(new));
                } else {
                    out.push_str(&word);
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

fn bind(statement: &mut rusqlite::Statement<'_>, params: &[SqlParam]) -> Result<()> {
    if statement.parameter_count() != params.len() {
        return Err(DbError::new(
            "QUERY_TYPE_ERROR",
            format!(
                "the statement takes {} parameter(s) but {} were supplied",
                statement.parameter_count(),
                params.len()
            ),
            4,
        ));
    }
    let mut used = BTreeSet::new();
    let mut positional = 1;
    for parameter in params {
        let index = if let Some(name) = &parameter.name {
            [format!(":{name}"), format!("@{name}"), format!("${name}")]
                .iter()
                .find_map(|spelled| statement.parameter_index(spelled).ok().flatten())
                .ok_or_else(|| {
                    DbError::new(
                        "QUERY_TYPE_ERROR",
                        format!("the statement has no parameter named {name:?}"),
                        4,
                    )
                })?
        } else {
            while used.contains(&positional) {
                positional += 1;
            }
            let index = positional;
            positional += 1;
            index
        };
        used.insert(index);
        statement
            .raw_bind_parameter(index, mirror::to_sql_generic(&parameter.value))
            .map_err(query_err)?;
    }
    Ok(())
}

/// Classify a SQLite error: a constraint failure is a relational violation of
/// the database; running out of an allowance is a resource limit; the rest is
/// a fault in the statement.
pub fn query_err(error: rusqlite::Error) -> DbError {
    let message = error.to_string();
    if let rusqlite::Error::SqliteFailure(failure, _) = &error {
        let code = match failure.extended_code {
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => Some(("PRIMARY_KEY_VIOLATION", 2)),
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => Some(("UNIQUE_VIOLATION", 2)),
            rusqlite::ffi::SQLITE_CONSTRAINT_NOTNULL => Some(("NOT_NULL_VIOLATION", 2)),
            rusqlite::ffi::SQLITE_CONSTRAINT_CHECK => Some(("CHECK_VIOLATION", 2)),
            _ => None,
        }
        .or(match failure.code {
            rusqlite::ffi::ErrorCode::OperationInterrupted => Some(("RESOURCE_LIMIT", 4)),
            rusqlite::ffi::ErrorCode::OutOfMemory => Some(("RESOURCE_LIMIT", 4)),
            _ => None,
        });
        if let Some((code, exit)) = code {
            let message = match code {
                "RESOURCE_LIMIT"
                    if failure.code == rusqlite::ffi::ErrorCode::OperationInterrupted =>
                {
                    "the statement exceeded the configured timeout and was stopped".to_string()
                }
                "RESOURCE_LIMIT" => {
                    "the statement exceeded the configured query-memory limit and was stopped"
                        .to_string()
                }
                _ => message,
            };
            return DbError::new(code, message, exit);
        }
    }
    let code = if message.contains("no such table") {
        "UNKNOWN_TABLE"
    } else if message.contains("no such column") {
        "UNKNOWN_COLUMN"
    } else if message.contains("syntax error") {
        "QUERY_UNSUPPORTED"
    } else {
        "QUERY_TYPE_ERROR"
    };
    DbError::new(code, message, 4)
}

/// Point an unknown table at what it probably is.
pub fn explain_unknown_table(catalog: &crate::catalog::Catalog, error: DbError) -> DbError {
    if error.diagnostic.code != "UNKNOWN_TABLE" {
        return error;
    }
    match error
        .diagnostic
        .message
        .strip_prefix("no such table: ")
        .map(|name| name.strip_prefix("main.").unwrap_or(name))
    {
        Some(table) => catalog.unknown_table(table),
        None => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test2100_only_queries_and_row_mutations_are_statements() {
        assert_eq!(classify("SELECT 1").unwrap(), StatementKind::Read);
        assert_eq!(
            classify("WITH x AS (SELECT 1 AS a) SELECT a FROM x UNION SELECT 2").unwrap(),
            StatementKind::Read
        );
        assert_eq!(
            classify("INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET a = 2").unwrap(),
            StatementKind::Mutation
        );
        assert_eq!(
            classify("DELETE FROM t WHERE a = 1 RETURNING a").unwrap(),
            StatementKind::Mutation
        );
        for refused in [
            "CREATE TABLE x (a)",
            "DROP TABLE t",
            "PRAGMA foreign_keys",
            "ATTACH 'x' AS y",
            "BEGIN",
            "VACUUM",
        ] {
            let error = classify(refused).expect_err(refused);
            assert_eq!(error.diagnostic.code, "QUERY_UNSUPPORTED", "{refused}");
        }
        assert!(
            classify("SELECT 1; SELECT 2").is_err(),
            "one statement at a time"
        );
        let located = classify("SELECT\n  FROM").unwrap_err();
        assert!(
            located.diagnostic.location.is_some(),
            "a parse error points at the fault"
        );
    }

    fn catalog(rows: &[(&str, &str)]) -> (tempfile::TempDir, crate::catalog::Catalog) {
        let directory = tempfile::tempdir().unwrap();
        let schema = format!(
            r#"{{"$schema":"{}","type":"object","properties":{{"id":{{"type":"string"}},"n":{{"type":"integer","x-reldir-type":"int"}}}},"required":["id"],"additionalProperties":false,"x-reldir":{{"table":"t","primaryKey":["id"]}}}}"#,
            crate::schema::meta::DIALECT_URI
        );
        std::fs::create_dir_all(directory.path().join(".db/schema")).unwrap();
        std::fs::create_dir_all(directory.path().join("t")).unwrap();
        std::fs::write(directory.path().join(".db/schema/t.json"), schema).unwrap();
        for (name, body) in rows {
            std::fs::write(directory.path().join("t").join(name), body).unwrap();
        }
        let catalog = crate::catalog::Catalog::observe(
            directory.path(),
            &crate::config::Config::default(),
            &crate::fs::Disk,
            std::rc::Rc::new(Mirror::open_memory().unwrap()),
            false,
        )
        .unwrap();
        (directory, catalog)
    }

    const LIMITS: QueryLimits = QueryLimits {
        timeout: None,
        max_rows: 1000,
        max_memory: 1 << 28,
    };

    /// Every kind of row change a statement makes is captured, and the mirror
    /// is left exactly as it was.
    #[test]
    fn test2103_mutations_are_captured_and_never_applied_to_the_mirror() {
        let (_directory, catalog) = catalog(&[
            ("a.json", r#"{"id":"a","n":1}"#),
            ("b.json", r#"{"id":"b","n":2}"#),
        ]);
        let deleted = mutate(&catalog, "DELETE FROM t WHERE id = 'a'", &[], LIMITS).unwrap();
        assert_eq!(deleted.rows.len(), 1);
        assert!(deleted.rows[0].after.is_none());
        let updated = mutate(
            &catalog,
            "UPDATE t SET n = n + 10 RETURNING id, n",
            &[],
            LIMITS,
        )
        .unwrap();
        assert_eq!(updated.rows.len(), 2);
        assert_eq!(updated.returning.len(), 2);
        assert!(
            updated
                .rows
                .iter()
                .all(|change| change.after.as_ref().unwrap()["n"].as_i64().unwrap() > 10)
        );
        let inserted = mutate(
            &catalog,
            "INSERT INTO t (id, n) VALUES ('c', 3)",
            &[],
            LIMITS,
        )
        .unwrap();
        assert_eq!(inserted.rows.len(), 1);
        assert!(inserted.rows[0].before.is_none());
        let mut left = 0;
        query(
            &catalog.mirror,
            &catalog.schemas,
            "SELECT id FROM t",
            &[],
            LIMITS,
            |_| {
                left += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(left, 2, "the mirror still holds exactly the files");
    }

    #[test]
    fn test2101_renaming_leaves_strings_and_function_names_alone() {
        assert_eq!(
            rename_identifier(
                "count > 0 AND count(x) > 1 AND name = 'count'",
                "count",
                "total"
            )
            .unwrap(),
            "total > 0 AND count(x) > 1 AND name = 'count'"
        );
        assert_eq!(
            rename_identifier("\"a b\" <> ''", "a b", "c").unwrap(),
            "\"c\" <> ''"
        );
        assert_eq!(
            rename_identifier("x = 1", "x", "new name").unwrap(),
            "\"new name\" = 1"
        );
        assert!(rename_identifier("\"open", "a", "b").is_err());
    }

    #[test]
    fn test2102_clock_readers_are_not_rules() {
        assert!(refuses_clock("created < CURRENT_TIMESTAMP").is_some());
        assert!(refuses_clock("date(created) < date('now')").is_some());
        assert!(refuses_clock("date(start) <= date(finish)").is_none());
    }
}
