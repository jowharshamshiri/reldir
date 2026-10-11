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

use crate::code::Code;
use crate::{
    diagnostic::{DbError, Diagnostic, Result},
    mirror::{self, Mirror},
    plan::RowChange,
    schema::{Assertion, Check, Column, ColumnType, Schema},
};
use rusqlite::{
    Connection, OptionalExtension,
    hooks::{AuthAction, AuthContext, Authorization},
    types::{Value as SqlValue, ValueRef},
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
#[derive(Clone, Debug)]
pub struct QueryLimits {
    pub timeout: Option<std::time::Duration>,
    /// Set by whoever asked for the statement when they no longer want its
    /// answer: the statement stops at its next progress check.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
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

/// Parse exactly one statement, refusing text that does not parse with the
/// reason and where it lies.
fn parse_one(text: &str) -> Result<Statement> {
    let dialect = SQLiteDialect {};
    let failed = |message: String, location: Option<crate::diagnostic::Location>| {
        let mut diagnostic = Diagnostic::error(
            Code::QUERY_UNSUPPORTED,
            format!("SQL does not parse: {message}"),
        );
        diagnostic.source_line = location
            .as_ref()
            .and_then(|location| text.lines().nth(location.line.saturating_sub(1)))
            .map(String::from);
        diagnostic.location = location;
        DbError::from_diag(diagnostic)
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
    let count = statements.len();
    let mut statements = statements.into_iter();
    match (statements.next(), statements.next()) {
        (Some(statement), None) => Ok(statement),
        _ => Err(DbError::new(
            Code::QUERY_UNSUPPORTED,
            format!("exactly one SQL statement is required; {count} were given"),
        )),
    }
}

/// Whether SQLite would call this text a complete statement: it ends with a
/// `;` that is outside every string, identifier and comment. What a shell
/// waits for before running what was typed.
pub fn is_complete(text: &str) -> bool {
    let Ok(text) = std::ffi::CString::new(text) else {
        // A NUL byte: no statement, and nothing more typed could make one.
        return true;
    };
    // SAFETY: `text` is a valid NUL-terminated string that outlives the call;
    // sqlite3_complete only reads it.
    unsafe { rusqlite::ffi::sqlite3_complete(text.as_ptr()) != 0 }
}

/// Text holding several statements, cut where SQLite would end each: at a
/// `;` outside every string, identifier and comment. The last may lack its
/// `;`. Each comes back as written, without its `;`; empty ones are dropped.
pub fn split_statements(text: &str) -> Vec<String> {
    let mut statements = vec![];
    let mut current = String::new();
    for character in text.chars() {
        current.push(character);
        if character == ';' && is_complete(&current) {
            let statement = current.trim().trim_end_matches(';').trim();
            if !statement.is_empty() {
                statements.push(statement.to_string());
            }
            current.clear();
        }
    }
    let rest = current.trim();
    if !rest.is_empty() {
        statements.push(rest.to_string());
    }
    statements
}

/// Transaction control as a script writes it: `BEGIN`, `COMMIT`, `END` or
/// `ROLLBACK`, with SQLite's optional words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Begin,
    Commit,
    Rollback,
}

/// Whether a statement is transaction control, and which.
pub fn control(statement: &str) -> Option<Control> {
    let words: Vec<String> = statement
        .split_whitespace()
        .map(|word| word.to_ascii_uppercase())
        .collect();
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["BEGIN"]
        | ["BEGIN", "TRANSACTION"]
        | ["BEGIN", "DEFERRED" | "IMMEDIATE" | "EXCLUSIVE"]
        | [
            "BEGIN",
            "DEFERRED" | "IMMEDIATE" | "EXCLUSIVE",
            "TRANSACTION",
        ] => Some(Control::Begin),
        ["COMMIT"] | ["COMMIT", "TRANSACTION"] | ["END"] | ["END", "TRANSACTION"] => {
            Some(Control::Commit)
        }
        ["ROLLBACK"] | ["ROLLBACK", "TRANSACTION"] => Some(Control::Rollback),
        _ => None,
    }
}

/// Parse and classify one statement, refusing anything outside the supported
/// surface with the reason and where it lies.
pub fn classify(text: &str) -> Result<StatementKind> {
    kind(&parse_one(text)?)
}

fn kind(statement: &Statement) -> Result<StatementKind> {
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
        Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::Savepoint { .. }
        | Statement::ReleaseSavepoint { .. } => Err(DbError::from_diag(
            Diagnostic::error(
                Code::TRANSACTION_STATEMENT,
                format!(
                    "{statement} is transaction control, and one statement alone is its own \
                     transaction"
                ),
            )
            .help(
                "a transaction is a script: several statements in one `reldir sql`, `-f FILE`, or \
                 `.begin` in the shell, optionally framed by BEGIN and COMMIT; savepoints are \
                 not supported",
            ),
        )),
        other => {
            let keyword = other
                .to_string()
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            Err(DbError::from_diag(
                Diagnostic::error(
                    Code::QUERY_UNSUPPORTED,
                    format!(
                        "{keyword} is not supported: SQL here reads and changes rows; the structure \
                         of the database is its schemas"
                    ),
                )
                .help("change a schema with `reldir migrate`, or by editing its file"),
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
    /// Whether only the functions a rule may call are allowed.
    deterministic: bool,
    /// The first refused action, to name in the error.
    refused: Arc<Mutex<Option<String>>>,
}

/// The functions a rule -- a check or an assertion -- may call: SQLite's
/// built-ins whose result depends on their arguments alone, its JSON and
/// math functions, and reldir's own. A function not listed is refused, so
/// validity can never come to depend on the clock, on chance, or on a
/// function nobody examined. The date and time functions are listed because
/// they are deterministic when told which instant; [`clock_reads`] refuses
/// each use that is not.
const RULE_FUNCTIONS: &[&str] = &[
    // Core scalars.
    "abs",
    "char",
    "coalesce",
    "concat",
    "concat_ws",
    "format",
    "glob",
    "hex",
    "ifnull",
    "iif",
    "if",
    "instr",
    "length",
    "like",
    "likelihood",
    "likely",
    "lower",
    "ltrim",
    "max",
    "min",
    "nullif",
    "octet_length",
    "printf",
    "quote",
    "replace",
    "round",
    "rtrim",
    "sign",
    "soundex",
    "substr",
    "substring",
    "trim",
    "typeof",
    "unhex",
    "unicode",
    "unlikely",
    "upper",
    "zeroblob",
    // Aggregates and windows, over the rows a query reads.
    "avg",
    "count",
    "group_concat",
    "string_agg",
    "sum",
    "total",
    "row_number",
    "rank",
    "dense_rank",
    "percent_rank",
    "cume_dist",
    "ntile",
    "lag",
    "lead",
    "first_value",
    "last_value",
    "nth_value",
    // Mathematics.
    "acos",
    "acosh",
    "asin",
    "asinh",
    "atan",
    "atan2",
    "atanh",
    "ceil",
    "ceiling",
    "cos",
    "cosh",
    "degrees",
    "exp",
    "floor",
    "ln",
    "log",
    "log10",
    "log2",
    "mod",
    "pi",
    "pow",
    "power",
    "radians",
    "sin",
    "sinh",
    "sqrt",
    "tan",
    "tanh",
    "trunc",
    // JSON.
    "json",
    "jsonb",
    "json_array",
    "jsonb_array",
    "json_array_length",
    "json_error_position",
    "json_extract",
    "jsonb_extract",
    "->",
    "->>",
    "json_insert",
    "jsonb_insert",
    "json_object",
    "jsonb_object",
    "json_patch",
    "jsonb_patch",
    "json_pretty",
    "json_remove",
    "jsonb_remove",
    "json_replace",
    "jsonb_replace",
    "json_set",
    "jsonb_set",
    "json_type",
    "json_valid",
    "json_quote",
    "json_group_array",
    "jsonb_group_array",
    "json_group_object",
    "jsonb_group_object",
    "json_each",
    "json_tree",
    // Dates and times, at a stated instant.
    "date",
    "time",
    "datetime",
    "julianday",
    "unixepoch",
    "strftime",
    "timediff",
    // reldir's exact arithmetic and normalization.
    "dec_add",
    "dec_sub",
    "dec_mul",
    "dec_cmp",
    "dec_sum",
    "nfc",
];

fn install(connection: &Connection, policy: &Policy) {
    let policy = policy.clone();
    connection.authorizer(Some(move |context: AuthContext<'_>| {
        if context
            .accessor
            .is_some_and(|name| name.starts_with("_reldir_capture_"))
        {
            return Authorization::Allow;
        }
        let decision = match context.action {
            AuthAction::Select | AuthAction::Recursive => Ok(()),
            AuthAction::Read { table_name, .. } => {
                if policy.readable.contains(table_name)
                    || matches!(table_name, "json_each" | "json_tree")
                {
                    Ok(())
                } else {
                    Err(format!(
                        "reading {table_name:?}, which is not a governed table"
                    ))
                }
            }
            AuthAction::Function { function_name } => {
                if function_name.eq_ignore_ascii_case("load_extension") {
                    Err("load_extension(): a statement never loads code into the engine".into())
                } else if policy.deterministic
                    && !RULE_FUNCTIONS.contains(&function_name.to_ascii_lowercase().as_str())
                {
                    Err(format!(
                        "{function_name}() is not a function a rule may call: validity depends on \
                         the rows alone, never on the clock or on chance"
                    ))
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
            Some(reason) => DbError::from_diag(Diagnostic::error(
                Code::QUERY_UNSUPPORTED,
                format!("the statement is refused: {reason}"),
            )),
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
    let started = std::time::Instant::now();
    let timeout = limits.timeout;
    let cancel = limits.cancel.clone();
    if timeout.is_some() || cancel.is_some() {
        connection.progress_handler(
            1000,
            Some(move || {
                timeout.is_some_and(|timeout| started.elapsed() >= timeout)
                    || cancelled(cancel.as_deref())
            }),
        );
    }
    Ok(())
}

fn cancelled(cancel: Option<&std::sync::atomic::AtomicBool>) -> bool {
    cancel.is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::Relaxed))
}

/// An interrupted statement's error: stopped because it was cancelled, or
/// because it ran past its time.
fn interrupted(error: DbError, limits: &QueryLimits) -> DbError {
    if error.code() == Code::RESOURCE_LIMIT && cancelled(limits.cancel.as_deref()) {
        return DbError::new(
            Code::RESOURCE_LIMIT,
            "the statement was cancelled by whoever asked for it",
        );
    }
    error
}

fn unlimit(connection: &Connection) {
    let _ = connection.execute_batch("PRAGMA hard_heap_limit=0;");
    connection.progress_handler(0, None::<fn() -> bool>);
}

/// Which of a query's rows to hand over: the first `skip` are passed over,
/// and at most `take` of the rest are handed on.
#[derive(Clone, Copy, Debug, Default)]
pub struct Window {
    pub skip: usize,
    pub take: Option<usize>,
}

/// How much of its answer a query handed over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Answered {
    pub rows: usize,
    /// Rows remain past the window.
    pub more: bool,
}

/// Run a read statement, handing each result row to `emit` as it is produced.
pub fn query(
    mirror: &Mirror,
    schemas: &BTreeMap<String, Schema>,
    text: &str,
    params: &[SqlParam],
    limits: QueryLimits,
    emit: impl FnMut(Map<String, Value>) -> Result<()>,
) -> Result<usize> {
    query_window(
        mirror,
        schemas,
        text,
        params,
        limits,
        Window::default(),
        emit,
    )
    .map(|answered| answered.rows)
}

/// Run a read statement, handing over the rows inside `window`.
pub fn query_window(
    mirror: &Mirror,
    schemas: &BTreeMap<String, Schema>,
    text: &str,
    params: &[SqlParam],
    limits: QueryLimits,
    window: Window,
    mut emit: impl FnMut(Map<String, Value>) -> Result<()>,
) -> Result<Answered> {
    if classify(text)? != StatementKind::Read {
        return Err(DbError::new(
            Code::QUERY_UNSUPPORTED,
            "a query must not change rows; run the mutation as its own statement",
        ));
    }
    let connection = mirror.connection();
    let policy = policy(schemas, false, false);
    let mut statement = prepare(connection, text, &policy)?;
    bind(&mut statement, params)?;
    limit(connection, &limits)?;
    let outcome = (|| -> Result<Answered> {
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
        let mut passed = 0usize;
        let mut emitted = 0usize;
        while let Some(row) = rows.next().map_err(query_err)? {
            if passed < window.skip {
                passed += 1;
                continue;
            }
            if window.take == Some(emitted) {
                return Ok(Answered {
                    rows: emitted,
                    more: true,
                });
            }
            if emitted == limits.max_rows {
                return Err(DbError::new(
                    Code::RESOURCE_LIMIT,
                    format!(
                        "the query produces more than {} rows, the configured limit; nothing past it \
                         is returned, so no partial answer is mistaken for a whole one",
                        limits.max_rows
                    ),
                ));
            }
            let mut object = Map::new();
            for (index, name) in names.iter().enumerate() {
                let value = crate::index::types::from_sql(row.get_ref(index).map_err(query_err)?);
                object.insert(
                    name.clone(),
                    crate::index::types::decode_declared(value, declared[index].as_deref())?,
                );
            }
            emit(object)?;
            emitted += 1;
        }
        Ok(Answered {
            rows: emitted,
            more: false,
        })
    })();
    drop(statement);
    unlimit(connection);
    outcome.map_err(|error| interrupted(error, &limits))
}

/// The columns a read statement answers with, in order, without running it.
pub fn columns(
    mirror: &Mirror,
    schemas: &BTreeMap<String, Schema>,
    text: &str,
) -> Result<Vec<String>> {
    if classify(text)? != StatementKind::Read {
        return Err(DbError::new(
            Code::QUERY_UNSUPPORTED,
            "only a statement that reads answers with columns",
        ));
    }
    let policy = policy(schemas, false, false);
    let statement = prepare(mirror.connection(), text, &policy)?;
    Ok(statement
        .column_names()
        .into_iter()
        .map(String::from)
        .collect())
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
            crate::index::types::from_sql(row.get_ref(0).map_err(query_err)?),
        );
        step.insert(
            "parent".into(),
            crate::index::types::from_sql(row.get_ref(1).map_err(query_err)?),
        );
        step.insert(
            "detail".into(),
            crate::index::types::from_sql(row.get_ref(3).map_err(query_err)?),
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

/// The columns an `INSERT` names, and so writes: every other column of a new
/// row is left out of its file, as a row that does not mention a member does
/// not have it.
#[derive(Debug)]
struct Inserted {
    table: String,
    /// The named columns, or `None` when the statement names none and so
    /// writes every column in order.
    named: Option<BTreeSet<String>>,
}

/// The insert a statement makes, if it makes one. Names are matched as SQLite
/// matches them, without regard to ASCII case.
fn inserted(statement: &Statement, schemas: &BTreeMap<String, Schema>) -> Option<Inserted> {
    let insert = match statement {
        Statement::Insert(insert) => insert,
        Statement::Query(query) => match query.body.as_ref() {
            sqlparser::ast::SetExpr::Insert(Statement::Insert(insert)) => insert,
            _ => return None,
        },
        _ => return None,
    };
    let sqlparser::ast::TableObject::TableName(name) = &insert.table else {
        return None;
    };
    let written = name.0.last()?.as_ident()?.value.clone();
    let (table, schema) = schemas
        .iter()
        .find(|(table, _)| table.eq_ignore_ascii_case(&written))?;
    let named = (!insert.columns.is_empty()).then(|| {
        insert
            .columns
            .iter()
            .filter_map(|ident| {
                schema
                    .columns()
                    .keys()
                    .find(|column| column.eq_ignore_ascii_case(&ident.value))
                    .cloned()
            })
            .collect()
    });
    Some(Inserted {
        table: table.clone(),
        named,
    })
}

/// Work out what a mutation would do, without doing it.
pub fn mutate(
    catalog: &crate::catalog::Catalog,
    text: &str,
    params: &[SqlParam],
    limits: QueryLimits,
) -> Result<Mutation> {
    let statement = parse_one(text)?;
    if kind(&statement)? != StatementKind::Mutation {
        return Err(DbError::new(Code::QUERY_UNSUPPORTED, "not a mutation"));
    }
    let inserted = inserted(&statement, &catalog.schemas);
    let mirror = &catalog.mirror;
    let connection = mirror.connection();
    mirror.savepoint("reldir_mutation")?;
    let outcome = (|| -> Result<Mutation> {
        connection
            .execute_batch(
                "PRAGMA recursive_triggers=ON; \
                 CREATE TEMP TABLE _reldir_changes \
                 (tbl TEXT NOT NULL, op TEXT NOT NULL, old_rowid INTEGER, new_rowid INTEGER);",
            )
            .map_err(query_err)?;
        for (ordinal, (table, schema)) in catalog.schemas.iter().enumerate() {
            let quoted = mirror::quote(table);
            let literal = table.replace('\'', "''");
            // The values each changed row held before the statement, exactly
            // as the mirror held them: a column whose value the statement
            // leaves as it was is a member the statement did not touch.
            let slots: Vec<String> = (0..schema.columns().len())
                .map(|index| format!("c{index}"))
                .collect();
            let olds: Vec<String> = schema
                .columns()
                .keys()
                .map(|name| format!("OLD.{}", mirror::quote(name)))
                .collect();
            let keep_old = format!(
                "INSERT OR IGNORE INTO _reldir_old_{ordinal} VALUES (OLD.rowid, {});",
                olds.join(", ")
            );
            connection
                .execute_batch(&format!(
                    "CREATE TEMP TABLE _reldir_old_{ordinal} (rid INTEGER PRIMARY KEY, {slots}); \
                     CREATE TEMP TRIGGER \"_reldir_capture_{ordinal}_i\" AFTER INSERT ON main.{quoted} BEGIN \
                       INSERT INTO _reldir_changes VALUES ('{literal}', 'insert', NULL, NEW.rowid); END; \
                     CREATE TEMP TRIGGER \"_reldir_capture_{ordinal}_u\" AFTER UPDATE ON main.{quoted} BEGIN \
                       {keep_old} \
                       INSERT INTO _reldir_changes VALUES ('{literal}', 'update', OLD.rowid, NEW.rowid); END; \
                     CREATE TEMP TRIGGER \"_reldir_capture_{ordinal}_d\" AFTER DELETE ON main.{quoted} BEGIN \
                       {keep_old} \
                       INSERT INTO _reldir_changes VALUES ('{literal}', 'delete', OLD.rowid, NULL); END;",
                    slots = slots.join(", ")
                ))
                .map_err(query_err)?;
            for (name, column) in schema.columns() {
                if column.generated() == Some(crate::schema::GeneratedKind::Sequence) {
                    mirror
                        .sequences()
                        .seed(table, name, sequence_start(connection, table, name)?);
                }
            }
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
                    let value =
                        crate::index::types::from_sql(row.get_ref(index).map_err(query_err)?);
                    object.insert(
                        name.clone(),
                        crate::index::types::decode_declared(value, declared[index].as_deref())?,
                    );
                }
                returning.push(object);
            }
        }
        drop(statement);
        unlimit(connection);
        let rows = captured(catalog, inserted.as_ref())?;
        Ok(Mutation { rows, returning })
    })();
    unlimit(connection);
    mirror.sequences().clear();
    mirror.rollback_to("reldir_mutation")?;
    outcome
}

/// Whether a statement left a column's value as it was: the same value as the
/// column's type compares values -- the same instant, however its offset is
/// written; the same number, however many digits spell it -- and otherwise the
/// same value of the same storage class. Reals compare by their bits, so
/// `-0.0` is not `0.0`.
fn unchanged(column: &Column, was: &SqlValue, now: &SqlValue) -> bool {
    let equal = |ordering: Option<std::cmp::Ordering>| ordering == Some(std::cmp::Ordering::Equal);
    match (was, now) {
        (SqlValue::Null, SqlValue::Null) => true,
        (SqlValue::Integer(was), SqlValue::Integer(now)) => was == now,
        (SqlValue::Real(was), SqlValue::Real(now)) => was.to_bits() == now.to_bits(),
        (SqlValue::Text(was), SqlValue::Text(now)) => {
            was == now
                || match column.kind() {
                    ColumnType::Timestamp => equal(crate::value::compare_instant(was, now)),
                    ColumnType::Decimal | ColumnType::Integer => {
                        equal(crate::value::compare_numeric(was, now))
                    }
                    _ => false,
                }
        }
        (SqlValue::Blob(was), SqlValue::Blob(now)) => was == now,
        _ => false,
    }
}

/// What a statement writing a stored value into a column makes of the member:
/// `None` removes it, `Some` sets it.
///
/// SQL has one NULL where a JSON row has two ways to hold nothing, so NULL is
/// read through the column's contract: an explicit `null` where the column
/// admits one, no member where the column is optional, and otherwise an
/// explicit `null` that validation refuses -- a required member cannot be
/// emptied either way.
fn written(column: &Column, stored: SqlValue) -> Result<Option<Value>> {
    match stored {
        SqlValue::Blob(bytes) if bytes == crate::index::types::ABSENT => Ok(None),
        SqlValue::Null if !column.nullable() && !column.required() => Ok(None),
        stored => {
            let value = crate::index::types::from_sql(ValueRef::from(&stored));
            crate::index::types::decode_typed(value, column.kind()).map(Some)
        }
    }
}

/// The row changes the capture triggers recorded, read back as JSON.
///
/// A row a statement changes keeps every member whose stored value the
/// statement left as it was exactly as its file holds it -- present or absent,
/// spelled as written. Only a member whose value changed is written, and only
/// as the column's type spells the new value. A new row holds the members the
/// insert names, the values its generated columns were given, and nothing
/// else.
fn captured(
    catalog: &crate::catalog::Catalog,
    inserted: Option<&Inserted>,
) -> Result<Vec<RowChange>> {
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
    let ordinals: BTreeMap<&String, usize> = catalog
        .schemas
        .keys()
        .enumerate()
        .map(|(ordinal, table)| (table, ordinal))
        .collect();
    let read = |sql: &str, rowid: i64, width: usize| -> Result<Option<Vec<SqlValue>>> {
        connection
            .query_row(sql, [rowid], |row| {
                (0..width)
                    .map(|index| row.get::<_, SqlValue>(index))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .optional()
            .map_err(query_err)
    };
    let mut out = vec![];
    for (table, (deleted, touched)) in per_table {
        let schema = &catalog.schemas[&table];
        let columns: Vec<(&String, &Column)> = schema.columns().iter().collect();
        let current_sql = format!(
            "SELECT {} FROM main.{} WHERE rowid = ?1",
            columns
                .iter()
                .map(|(name, _)| mirror::quote(name))
                .collect::<Vec<_>>()
                .join(", "),
            mirror::quote(&table)
        );
        let old_sql = format!(
            "SELECT {} FROM temp._reldir_old_{} WHERE rid = ?1",
            (0..columns.len())
                .map(|index| format!("c{index}"))
                .collect::<Vec<_>>()
                .join(", "),
            ordinals[&table]
        );
        for rowid in deleted.union(&touched) {
            // The file this rowid held before the statement, if any: the
            // capture changed only the typed table, never the file index.
            let before_path = catalog.mirror.path_of_rowid(&table, *rowid)?;
            let before = match &before_path {
                Some(path) => catalog.row_at(std::path::Path::new(path))?,
                None => None,
            };
            let current = read(&current_sql, *rowid, columns.len())?;
            let replaced = deleted.contains(rowid);
            let after = match (current, &before) {
                (None, _) => None,
                // A rowid deleted and then reused by an insert holds a new
                // row; it inherits nothing from the one it replaced.
                (Some(values), Some(before)) if !replaced => {
                    let old = read(&old_sql, *rowid, columns.len())?.ok_or_else(|| {
                        DbError::new(
                            Code::INTERNAL_ERROR,
                            format!("a changed {table} row's prior values were not captured"),
                        )
                    })?;
                    let mut row = before.value.clone();
                    for (((name, column), now), was) in columns.iter().zip(values).zip(old) {
                        if unchanged(column, &was, &now) {
                            continue;
                        }
                        match written(column, now)? {
                            Some(value) => {
                                row.insert((*name).clone(), value);
                            }
                            None => {
                                row.shift_remove(*name);
                            }
                        }
                    }
                    Some(row)
                }
                (Some(values), _) => {
                    let named = match inserted {
                        Some(inserted) if inserted.table == table => inserted.named.as_ref(),
                        _ => {
                            return Err(DbError::new(
                                Code::INTERNAL_ERROR,
                                format!(
                                    "a {table} row appeared that no insert of the statement made"
                                ),
                            ));
                        }
                    };
                    let mut row = Map::new();
                    for ((name, column), now) in columns.iter().zip(values) {
                        // A column the insert does not name holds what its
                        // default generated, or what an upsert later set; a
                        // NULL there is a member the row never had.
                        let named = named.is_none_or(|named| named.contains(*name));
                        if !named && matches!(now, SqlValue::Null) {
                            continue;
                        }
                        if let Some(value) = written(column, now)? {
                            row.insert((*name).clone(), value);
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
    let column = mirror::quote(column);
    let max: Option<i64> = connection
        .query_row(
            &format!(
                "SELECT {column} FROM {} WHERE typeof({column}) = 'integer' \
                 ORDER BY {column} DESC LIMIT 1",
                mirror::quote(table),
            ),
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(query_err)?;
    max.unwrap_or(0).checked_add(1).ok_or_else(|| {
        DbError::new(
            Code::RESOURCE_LIMIT,
            format!("the sequence for {table}.{column} has reached the largest int"),
        )
    })
}

/// Compile every check and assertion against the tables they may read,
/// reporting each that cannot be compiled or is not deterministic.
pub fn compile_rules(schemas: &BTreeMap<String, Schema>) -> Vec<Diagnostic> {
    let mut out = vec![];
    let Ok(scratch) = Mirror::open_memory() else {
        return vec![Diagnostic::error(
            Code::INTERNAL_METADATA_CORRUPT,
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
            let expression = match rule_expression(check.expr()) {
                Ok(expression) => expression,
                Err(reason) => {
                    out.push(
                        Diagnostic::error(
                            Code::SCHEMA_CHECK_INVALID,
                            format!("check {:?} {reason}", check.name()),
                        )
                        .table(table)
                        .pointer(format!("/x-reldir/checks/{index}/expr")),
                    );
                    continue;
                }
            };
            let text = format!("SELECT ({expression}) FROM {}", mirror::quote(table));
            if let Err(error) = prepare(connection, &text, &rule) {
                out.push(
                    Diagnostic::error(
                        Code::SCHEMA_CHECK_INVALID,
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
            let query = match rule_query(assertion.query()) {
                Ok(query) => query,
                Err(reason) => {
                    out.push(
                        Diagnostic::error(
                            Code::SCHEMA_ASSERTION_INVALID,
                            format!("assertion {:?} {reason}", assertion.name()),
                        )
                        .table(table)
                        .pointer(at),
                    );
                    continue;
                }
            };
            let rule = policy(schemas, false, true);
            match prepare(connection, &query, &rule) {
                Err(error) => out.push(
                    Diagnostic::error(
                        Code::SCHEMA_ASSERTION_INVALID,
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
                                Code::SCHEMA_ASSERTION_INVALID,
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

/// A check's expression, parsed and rendered back by reldir: the only text of
/// it that is ever run, so nothing written after the expression can change
/// what the statement around it means.
pub fn rule_expression(text: &str) -> std::result::Result<String, String> {
    let expression = parse_expression(text)?;
    if let Some(reason) = clock_reads(&expression) {
        return Err(reason);
    }
    Ok(expression.to_string())
}

/// One SQL expression, parsed and rendered back by reldir -- so text that
/// closes the expression early cannot change the statement around it.
pub fn expression(text: &str) -> std::result::Result<String, String> {
    parse_expression(text).map(|expression| expression.to_string())
}

fn parse_expression(text: &str) -> std::result::Result<sqlparser::ast::Expr, String> {
    let dialect = SQLiteDialect {};
    let mut parser = Parser::new(&dialect)
        .try_with_sql(text)
        .map_err(|error| format!("does not parse: {error}"))?;
    let expression = parser
        .parse_expr()
        .map_err(|error| format!("does not parse: {error}"))?;
    let rest = parser.peek_token();
    if rest.token != sqlparser::tokenizer::Token::EOF {
        return Err(format!("is one expression, and {} follows it", rest.token));
    }
    Ok(expression)
}

/// An assertion's query, parsed, checked to be one query, and rendered back.
pub fn rule_query(text: &str) -> std::result::Result<String, String> {
    let statement = parse_one(text).map_err(|error| error.diagnostic.message)?;
    match kind(&statement) {
        Ok(StatementKind::Read) if matches!(statement, Statement::Query(_)) => {}
        _ => return Err("must be a single SELECT".into()),
    }
    if let Some(reason) = clock_reads(&statement) {
        return Err(reason);
    }
    Ok(statement.to_string())
}

/// The first place a rule reads the clock: a keyword naming the current
/// time, or a date or time function not told which instant -- given none, or
/// `'now'`, or one computed from constants alone (`'n' || 'ow'`), which could
/// only ever be written as a literal or be the clock.
fn clock_reads<V: sqlparser::ast::Visit>(node: &V) -> Option<String> {
    use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Value as Literal};
    use std::ops::ControlFlow;
    let refusal =
        |what: &str| format!("{what} reads the clock, and validity cannot change with time");
    let literal_now = |expression: &Expr| match expression {
        Expr::Value(value) => match &value.value {
            Literal::SingleQuotedString(text) | Literal::DoubleQuotedString(text) => {
                text.trim().eq_ignore_ascii_case("now")
            }
            _ => false,
        },
        _ => false,
    };
    let names_a_column = |expression: &Expr| {
        let mut found = false;
        let _ = sqlparser::ast::visit_expressions(expression, |inner| {
            if matches!(inner, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
                found = true;
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });
        found
    };
    let mut reason = None;
    let _ = sqlparser::ast::visit_expressions(node, |expression| {
        let found = match expression {
            Expr::Function(function) => {
                let name = function
                    .name
                    .0
                    .last()
                    .and_then(|part| part.as_ident())
                    .map(|ident| ident.value.to_ascii_lowercase())
                    .unwrap_or_default();
                let at_instants: &[usize] = match name.as_str() {
                    "date" | "time" | "datetime" | "julianday" | "unixepoch" => &[0],
                    "strftime" => &[1],
                    "timediff" => &[0, 1],
                    // `current_timestamp` and friends parse as functions
                    // when written with parentheses.
                    "current_timestamp" | "current_date" | "current_time" => {
                        return {
                            reason = Some(refusal(&name));
                            ControlFlow::Break(())
                        };
                    }
                    _ => &[],
                };
                let arguments: Vec<&Expr> = match &function.args {
                    FunctionArguments::List(list) => list
                        .args
                        .iter()
                        .filter_map(|argument| match argument {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(expression)) => {
                                Some(expression)
                            }
                            _ => None,
                        })
                        .collect(),
                    _ => vec![],
                };
                at_instants
                    .iter()
                    .find_map(|index| match arguments.get(*index) {
                        None => Some(refusal(&format!("{name}() without an instant"))),
                        Some(instant) if literal_now(instant) => {
                            Some(refusal(&format!("{name}('now')")))
                        }
                        Some(Expr::Value(_)) => None,
                        Some(instant) if !names_a_column(instant) => Some(refusal(&format!(
                            "{name}() of an instant computed from constants ({instant})"
                        ))),
                        Some(_) => None,
                    })
            }
            Expr::Identifier(ident)
                if ident.quote_style.is_none()
                    && ["current_timestamp", "current_date", "current_time"]
                        .contains(&ident.value.to_ascii_lowercase().as_str()) =>
            {
                Some(refusal(&ident.value.to_ascii_lowercase()))
            }
            _ => None,
        };
        match found {
            Some(found) => {
                reason = Some(found);
                ControlFlow::Break(())
            }
            None => ControlFlow::Continue(()),
        }
    });
    reason
}

/// The files whose rows fail a check, judged under the rule policy by the
/// check as reldir renders it. A NULL result passes, as SQL's `CHECK` does.
pub fn check_violations(mirror: &Mirror, schema: &Schema, check: &Check) -> Result<Vec<String>> {
    check_violations_in(mirror, schema, check, None)
}

/// [`check_violations`], among the rows with these rowids alone when given.
pub fn check_violations_in(
    mirror: &Mirror,
    schema: &Schema,
    check: &Check,
    rowids: Option<&[i64]>,
) -> Result<Vec<String>> {
    if rowids.is_some_and(<[i64]>::is_empty) {
        return Ok(vec![]);
    }
    let connection = mirror.connection();
    let expression = rule_expression(check.expr()).map_err(|reason| {
        DbError::new(
            Code::SCHEMA_CHECK_INVALID,
            format!("check {:?} {reason}", check.name()),
        )
    })?;
    let table = schema.table().to_string();
    let only_this = BTreeMap::from([(table.clone(), schema.clone())]);
    let rule = policy(&only_this, false, true);
    let among = match rowids {
        Some(rowids) => format!(
            " AND rowid IN ({})",
            rowids
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => String::new(),
    };
    let text = format!(
        "SELECT rowid FROM {} WHERE NOT ({expression}){among}",
        mirror::quote(&table)
    );
    let mut statement = prepare(connection, &text, &rule)?;
    let rowids = statement
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(query_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(query_err)?;
    drop(statement);
    let mut paths = vec![];
    for rowid in rowids {
        paths.extend(mirror.path_of_rowid(&table, rowid)?);
    }
    paths.sort();
    Ok(paths)
}

/// The files whose rows an assertion names, judged under the rule policy by
/// the query as reldir renders it.
pub fn assertion_violations(
    mirror: &Mirror,
    schemas: &BTreeMap<String, Schema>,
    schema: &Schema,
    assertion: &Assertion,
) -> Result<Vec<String>> {
    let connection = mirror.connection();
    let query = rule_query(assertion.query()).map_err(|reason| {
        DbError::new(
            Code::SCHEMA_ASSERTION_INVALID,
            format!("assertion {:?} {reason}", assertion.name()),
        )
    })?;
    let rule = policy(schemas, false, true);
    let mut statement = prepare(connection, &query, &rule)?;
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
            let value = crate::index::types::from_sql(row.get_ref(index).map_err(query_err)?);
            values.insert(
                column.clone(),
                crate::index::types::decode_typed(value, &kind)?,
            );
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

/// An identifier as SQL must write it: bare when it is a plain name that is
/// no keyword, quoted otherwise.
fn identifier(name: &str) -> sqlparser::ast::Ident {
    let mut characters = name.chars();
    let plain = matches!(characters.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && characters.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && sqlparser::keywords::ALL_KEYWORDS
            .binary_search(&name.to_ascii_uppercase().as_str())
            .is_err();
    if plain {
        sqlparser::ast::Ident::new(name)
    } else {
        sqlparser::ast::Ident::with_quote('"', name)
    }
}

/// A check with column `old` of its table renamed to `new`, wherever the
/// expression names it -- bare or qualified by the table -- and nowhere else:
/// not in strings, not as a function's name.
pub fn rename_in_check(
    expression: &str,
    table: &str,
    old: &str,
    new: &str,
) -> std::result::Result<String, String> {
    use sqlparser::ast::Expr;
    use std::ops::ControlFlow;
    let dialect = SQLiteDialect {};
    let mut parser = Parser::new(&dialect)
        .try_with_sql(expression)
        .map_err(|error| format!("check {expression:?} does not parse: {error}"))?;
    let mut parsed = parser
        .parse_expr()
        .map_err(|error| format!("check {expression:?} does not parse: {error}"))?;
    let _ = sqlparser::ast::visit_expressions_mut(&mut parsed, |node| {
        match node {
            Expr::Identifier(ident) if ident.value.eq_ignore_ascii_case(old) => {
                *ident = identifier(new);
            }
            Expr::CompoundIdentifier(parts)
                if parts.len() == 2
                    && parts[0].value.eq_ignore_ascii_case(table)
                    && parts[1].value.eq_ignore_ascii_case(old) =>
            {
                parts[1] = identifier(new);
            }
            _ => {}
        }
        ControlFlow::<()>::Continue(())
    });
    Ok(parsed.to_string())
}

/// An assertion with column `old` of `table` renamed to `new`. A qualified
/// name is renamed when its qualifier is the table or an alias of it; a bare
/// one when the query reads the table and no other table it reads has a
/// column of that name -- and refused as ambiguous when one does. A query
/// that does not read the table is returned as it is.
pub fn rename_in_assertion(
    query: &str,
    table: &str,
    old: &str,
    new: &str,
    has_column: &dyn Fn(&str, &str) -> bool,
) -> std::result::Result<String, String> {
    use sqlparser::ast::{Expr, TableFactor, Visit, Visitor};
    use std::ops::ControlFlow;
    struct Relations(Vec<(String, Option<String>)>);
    impl Visitor for Relations {
        type Break = ();
        fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<()> {
            if let TableFactor::Table { name, alias, .. } = factor
                && let Some(last) = name.0.last().and_then(|part| part.as_ident())
            {
                self.0.push((
                    last.value.clone(),
                    alias.as_ref().map(|alias| alias.name.value.clone()),
                ));
            }
            ControlFlow::Continue(())
        }
    }
    let mut statement = parse_one(query).map_err(|error| error.diagnostic.message)?;
    let mut relations = Relations(vec![]);
    let _ = statement.visit(&mut relations);
    let reads = |name: &str| name.eq_ignore_ascii_case(table);
    if !relations.0.iter().any(|(name, _)| reads(name)) {
        return Ok(query.to_string());
    }
    let qualifiers: Vec<String> = relations
        .0
        .iter()
        .filter(|(name, _)| reads(name))
        .flat_map(|(name, alias)| [Some(name.clone()), alias.clone()])
        .flatten()
        .collect();
    let others: Vec<&String> = relations
        .0
        .iter()
        .filter(|(name, _)| !reads(name))
        .map(|(name, _)| name)
        .collect();
    let mut refusal = None;
    let _ = sqlparser::ast::visit_expressions_mut(&mut statement, |node| {
        match node {
            Expr::CompoundIdentifier(parts)
                if parts.len() >= 2
                    && parts[parts.len() - 1].value.eq_ignore_ascii_case(old)
                    && qualifiers.iter().any(|qualifier| {
                        parts[parts.len() - 2].value.eq_ignore_ascii_case(qualifier)
                    }) =>
            {
                let last = parts.len() - 1;
                parts[last] = identifier(new);
            }
            Expr::Identifier(ident) if ident.value.eq_ignore_ascii_case(old) => {
                if let Some(other) = others.iter().find(|other| has_column(other, old)) {
                    refusal = Some(format!(
                        "the query names {old:?} without saying whether it is {table}'s or \
                         {other}'s; qualify it, then rename"
                    ));
                    return ControlFlow::Break(());
                }
                *ident = identifier(new);
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
    match refusal {
        Some(reason) => Err(reason),
        None => Ok(statement.to_string()),
    }
}

fn bind(statement: &mut rusqlite::Statement<'_>, params: &[SqlParam]) -> Result<()> {
    if statement.parameter_count() != params.len() {
        return Err(DbError::new(
            Code::QUERY_TYPE_ERROR,
            format!(
                "the statement takes {} parameter(s) but {} were supplied",
                statement.parameter_count(),
                params.len()
            ),
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
                        Code::QUERY_TYPE_ERROR,
                        format!("the statement has no parameter named {name:?}"),
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
            .raw_bind_parameter(index, crate::index::types::to_sql_generic(&parameter.value))
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
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => Some(Code::PRIMARY_KEY_VIOLATION),
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => Some(Code::UNIQUE_VIOLATION),
            rusqlite::ffi::SQLITE_CONSTRAINT_NOTNULL => Some(Code::NOT_NULL_VIOLATION),
            rusqlite::ffi::SQLITE_CONSTRAINT_CHECK => Some(Code::CHECK_VIOLATION),
            _ => None,
        }
        .or(match failure.code {
            rusqlite::ffi::ErrorCode::OperationInterrupted => Some(Code::RESOURCE_LIMIT),
            rusqlite::ffi::ErrorCode::OutOfMemory => Some(Code::RESOURCE_LIMIT),
            _ => None,
        });
        if let Some(code) = code {
            let message = match code {
                Code::RESOURCE_LIMIT
                    if failure.code == rusqlite::ffi::ErrorCode::OperationInterrupted =>
                {
                    "the statement exceeded the configured timeout and was stopped".to_string()
                }
                Code::RESOURCE_LIMIT => {
                    "the statement exceeded the configured query-memory limit and was stopped"
                        .to_string()
                }
                _ => message,
            };
            return DbError::new(code, message);
        }
    }
    let code = if message.contains("no such table") {
        Code::UNKNOWN_TABLE
    } else if message.contains("no such column") {
        Code::UNKNOWN_COLUMN
    } else if message.contains("syntax error") {
        Code::QUERY_UNSUPPORTED
    } else {
        Code::QUERY_TYPE_ERROR
    };
    DbError::new(code, message)
}

/// Point an unknown table at what it probably is.
pub fn explain_unknown_table(catalog: &crate::catalog::Catalog, error: DbError) -> DbError {
    if error.diagnostic.code != Code::UNKNOWN_TABLE {
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
            "VACUUM",
        ] {
            let error = classify(refused).expect_err(refused);
            assert_eq!(error.diagnostic.code, Code::QUERY_UNSUPPORTED, "{refused}");
        }
        // Transaction control frames a script; alone, it is out of place.
        for control in ["BEGIN", "COMMIT", "ROLLBACK", "SAVEPOINT a", "RELEASE a"] {
            let error = classify(control).expect_err(control);
            assert_eq!(
                error.diagnostic.code,
                Code::TRANSACTION_STATEMENT,
                "{control}"
            );
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
            r#"{{"$schema":"{}","type":"object","properties":{{"id":{{"type":"string"}},"n":{{"type":"integer","x-reldir-type":"int"}}}},"required":["id"],"additionalProperties":false,"x-reldir":{{"table":"t","identity":{{"source":"body","columns":["id"],"naming":{{"rule":"reldir"}}}}}}}}"#,
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
            &crate::engine::fs::Disk,
            std::rc::Rc::new(Mirror::open_memory().unwrap()),
            false,
        )
        .unwrap();
        (directory, catalog)
    }

    const LIMITS: QueryLimits = QueryLimits {
        timeout: None,
        cancel: None,
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
            rename_in_check(
                "count > 0 AND count(x) > 1 AND name = 'count' AND t.count < 9",
                "t",
                "count",
                "total"
            )
            .unwrap(),
            "total > 0 AND count(x) > 1 AND name = 'count' AND t.total < 9"
        );
        assert_eq!(
            rename_in_check("\"a b\" <> ''", "t", "a b", "c").unwrap(),
            "c <> ''"
        );
        assert_eq!(
            rename_in_check("x = 1", "t", "x", "new name").unwrap(),
            "\"new name\" = 1"
        );
        assert_eq!(
            rename_in_check("x = 1", "t", "x", "select").unwrap(),
            "\"select\" = 1",
            "a keyword is quoted"
        );
        assert!(rename_in_check("\"open", "t", "a", "b").is_err());

        let has = |table: &str, column: &str| table == "orders" && column == "total";
        assert_eq!(
            rename_in_assertion(
                "SELECT c.id FROM customers AS c JOIN orders o ON o.customer = c.id WHERE c.total < 0",
                "customers",
                "total",
                "balance",
                &has,
            )
            .unwrap(),
            "SELECT c.id FROM customers AS c JOIN orders AS o ON o.customer = c.id WHERE c.balance < 0"
        );
        assert!(
            rename_in_assertion(
                "SELECT id FROM customers JOIN orders ON orders.customer = customers.id WHERE total < 0",
                "customers",
                "total",
                "balance",
                &has,
            )
            .is_err(),
            "a bare name two tables share is ambiguous"
        );
        let untouched = "SELECT id FROM orders WHERE total < 0";
        assert_eq!(
            rename_in_assertion(untouched, "customers", "total", "balance", &has).unwrap(),
            untouched
        );
    }

    #[test]
    fn test2102_clock_readers_are_not_rules() {
        for clock in [
            "created < CURRENT_TIMESTAMP",
            "created < current_date",
            "date(created) < date('now')",
            "date(created) < date(' NOW ')",
            "julianday() - julianday(due) < 30",
            "date(created) < date('n' || 'ow')",
            "strftime('%Y') = '2024'",
            "unixepoch() > 0",
            "timediff(due, 'now') IS NOT NULL",
        ] {
            assert!(rule_expression(clock).is_err(), "{clock} reads the clock");
        }
        for rule in [
            "date(start) <= date(finish)",
            "julianday(due) - julianday(created) < 30",
            "strftime('%Y', created) = '2024'",
            "date(created, '+1 day') > date(created)",
            "date('2024-01-01') < date(created)",
        ] {
            assert!(rule_expression(rule).is_ok(), "{rule} reads only the row");
        }
        let query = rule_query("SELECT id FROM t WHERE date(at) < date('now')");
        assert!(query.is_err(), "an assertion is held to the same rule");
    }

    #[test]
    fn test2106_a_cancelled_query_stops() {
        let (_directory, catalog) = catalog(&[("a.json", r#"{"id":"a"}"#)]);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let limits = QueryLimits {
            cancel: Some(std::sync::Arc::clone(&cancel)),
            ..LIMITS
        };
        let setter = {
            let cancel = std::sync::Arc::clone(&cancel);
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(100));
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            })
        };
        let started = std::time::Instant::now();
        let endless = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) \
                       SELECT max(x) AS m FROM c";
        let error = query(
            &catalog.mirror,
            &catalog.schemas,
            endless,
            &[],
            limits,
            |_| Ok(()),
        )
        .expect_err("an endless query ends only by being stopped");
        setter.join().unwrap();
        assert!(
            error.diagnostic.message.contains("cancelled"),
            "{}",
            error.diagnostic.message
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn test2107_a_window_hands_over_its_rows_and_says_whether_more_remain() {
        let (_directory, catalog) = catalog(&[
            ("a.json", r#"{"id":"a"}"#),
            ("b.json", r#"{"id":"b"}"#),
            ("c.json", r#"{"id":"c"}"#),
        ]);
        let page = |skip, take| {
            let mut ids = vec![];
            let answered = query_window(
                &catalog.mirror,
                &catalog.schemas,
                "SELECT id FROM t ORDER BY id",
                &[],
                LIMITS,
                Window { skip, take },
                |row| {
                    ids.push(row["id"].as_str().unwrap().to_string());
                    Ok(())
                },
            )
            .unwrap();
            (ids, answered.more)
        };
        assert_eq!(page(0, Some(2)), (vec!["a".into(), "b".into()], true));
        assert_eq!(page(2, Some(2)), (vec!["c".into()], false));
        assert_eq!(
            page(1, Some(2)),
            (vec!["b".into(), "c".into()], false),
            "an exact fit has no more"
        );
        assert_eq!(page(3, Some(2)), (vec![], false));
        assert_eq!(page(0, None).0.len(), 3);
    }

    #[test]
    fn test2105_a_script_is_cut_where_sqlite_ends_each_statement() {
        assert_eq!(
            split_statements(
                "BEGIN;\nINSERT INTO t VALUES ('a;b'); -- a comment; with a semicolon\n\
                 UPDATE t SET \"x;y\" = 1 /* ; */;\n\n;SELECT 1"
            ),
            [
                "BEGIN",
                "INSERT INTO t VALUES ('a;b')",
                "-- a comment; with a semicolon\nUPDATE t SET \"x;y\" = 1 /* ; */",
                "SELECT 1"
            ]
        );
        assert_eq!(control("begin transaction"), Some(Control::Begin));
        assert_eq!(control("END"), Some(Control::Commit));
        assert_eq!(control("rollback"), Some(Control::Rollback));
        assert_eq!(
            control("ROLLBACK TO keep"),
            None,
            "savepoints are not supported control"
        );
        assert_eq!(control("SELECT 1"), None);
    }

    #[test]
    fn test2104_a_check_is_one_expression_and_runs_as_reldir_renders_it() {
        assert!(
            rule_expression("x > 0) OR (1").is_err(),
            "text that closes the expression early is not one expression"
        );
        assert!(rule_expression("x > 0; DROP TABLE t").is_err());
        assert_eq!(
            rule_expression("x>0  AND y<>''").unwrap(),
            "x > 0 AND y <> ''"
        );
    }
}
