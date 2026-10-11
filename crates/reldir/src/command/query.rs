//! SQL: queries answered from the mirror, mutations turned into validated
//! transactions.

use super::{Context, Intent, parse_params, require_valid};
use crate::output::{Finish, Sink};
use reldir_core::code::Code;
use reldir_core::{
    db::Database,
    diagnostic::{DbError, Result},
    sql::{Control, SqlParam, StatementKind, control},
    store::revision::Origin,
    transaction::Transaction,
};
use serde_json::{Map, Value, json};
use std::path::Path;

/// The statement text: as given, or -- for `-` -- read from stdin.
pub fn statement_text(text: &str) -> Result<String> {
    if text == "-" {
        super::read_input("-", 16 * 1024 * 1024)
    } else {
        Ok(text.to_string())
    }
}

/// Run SQL: one statement, or a script of several, which is one transaction.
pub fn sql(
    context: &Context,
    sink: &mut dyn Sink,
    statement: Option<&str>,
    file: Option<&Path>,
    params: &[String],
) -> Result<Finish> {
    let text = match (statement, file) {
        (Some(text), None) => statement_text(text)?,
        (None, Some(path)) => super::read_input(&path.to_string_lossy(), 16 * 1024 * 1024)?,
        _ => unreachable!("clap requires exactly one of the statement and --file"),
    };
    let params = parse_params(params)?;
    let statements = reldir_core::sql::split_statements(&text);
    match statements.as_slice() {
        [] => Err(DbError::usage("there is no statement to run")),
        [one] => one_statement(context, sink, one, &params),
        _ => {
            if !params.is_empty() {
                return Err(DbError::usage(
                    "--param binds the `?` of one statement; a script's statements carry their \
                     own values",
                ));
            }
            script(context, sink, &statements)
        }
    }
}

fn one_statement(
    context: &Context,
    sink: &mut dyn Sink,
    text: &str,
    params: &[SqlParam],
) -> Result<Finish> {
    match reldir_core::sql::classify(text)? {
        StatementKind::Read => {
            let database = readable(context, sink)?;
            let count = reldir_core::sql::query(
                &database.catalog.mirror,
                &database.catalog.schemas,
                text,
                params,
                database.query_limits(),
                |row| sink.row(row),
            )
            .map_err(|error| reldir_core::sql::explain_unknown_table(&database.catalog, error))?;
            Ok(Finish::ok(format!("{count} row(s)"))
                .with("rows", count)
                .with("database_valid", database.is_valid()))
        }
        StatementKind::Mutation => {
            let mut database = writable(context, sink)?;
            let limits = database.query_limits();
            let (outcome, returning) =
                database.apply_with(context.request(Origin::User), |catalog| {
                    let mutation = reldir_core::sql::mutate(catalog, text, params, limits)
                        .map_err(|error| reldir_core::sql::explain_unknown_table(catalog, error))?;
                    Ok((mutation.rows, mutation.returning))
                })?;
            // A replayed change was made before, and its rows went to whoever
            // made it; it is not planned again to produce them.
            for row in returning.into_iter().flatten() {
                sink.row(row)?;
            }
            super::committed(sink, &outcome, "committed")
        }
    }
}

/// One page of a query's rows: `skip` passed over, at most `take` returned,
/// all from the state `expect_state` names when the page continues another.
/// Reports whether rows remain (`more`) and the state they were read from.
pub fn page(
    context: &Context,
    sink: &mut dyn Sink,
    text: &str,
    params: &[SqlParam],
    window: reldir_core::sql::Window,
    expect_state: Option<&str>,
) -> Result<Finish> {
    if reldir_core::sql::classify(text)? != StatementKind::Read {
        return Err(DbError::usage(
            "a page is read from a query; a change is not paged",
        ));
    }
    let database = readable(context, sink)?;
    let state = database.state_token()?;
    if let Some(expected) = expect_state
        && expected != state
    {
        return Err(DbError::from_diag(
            reldir_core::diagnostic::Diagnostic::error(
                Code::CURSOR_STALE,
                "the database changed since the first page was read, so the next page would \
                 not continue the same answer",
            )
            .help("run the query again from its first page"),
        ));
    }
    let answered = reldir_core::sql::query_window(
        &database.catalog.mirror,
        &database.catalog.schemas,
        text,
        params,
        database.query_limits(),
        window,
        |row| sink.row(row),
    )
    .map_err(|error| reldir_core::sql::explain_unknown_table(&database.catalog, error))?;
    let summary = if answered.more {
        format!("{} row(s); more remain", answered.rows)
    } else {
        format!("{} row(s)", answered.rows)
    };
    Ok(Finish::ok(summary)
        .with("rows", answered.rows)
        .with("more", answered.more)
        .with("state", state)
        .with("database_valid", database.is_valid()))
}

/// The database a read answers from.
fn readable(context: &Context, sink: &mut dyn Sink) -> Result<Database> {
    let Some(database) = context.open(sink, Intent::Read)? else {
        return Err(
            DbError::new(Code::UNKNOWN_TABLE, "there are no tables here to query")
                .with_help("create a database with `reldir init`, or name one with --db"),
        );
    };
    super::gate(context, &database, sink)?;
    Ok(database)
}

/// The database a change is made to: valid, and held for writing.
fn writable(context: &Context, sink: &mut dyn Sink) -> Result<Database> {
    if context.allow_invalid {
        return Err(DbError::usage(
            "--allow-invalid answers queries; a change to an invalid database would be judged against faults it did not make",
        )
        .with_help("repair the database with `reldir doctor` first"));
    }
    let database = context.open_database(sink, Intent::Write)?;
    require_valid(&database)?;
    Ok(database)
}

/// How a script ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Commit,
    Rollback,
}

/// The statements a script runs, without the BEGIN and COMMIT or ROLLBACK
/// that may frame them; transaction control anywhere else is refused.
fn frame(statements: &[String]) -> Result<(&[String], End)> {
    let misplaced = |index: usize, why: &str| {
        DbError::new(
            Code::TRANSACTION_STATEMENT,
            format!(
                "statement {} of {}, {:?}, {why}",
                index + 1,
                statements.len(),
                statements[index]
            ),
        )
        .with_help(
            "a script is one transaction: its statements, optionally framed by BEGIN at the \
             start and COMMIT or ROLLBACK at the end",
        )
    };
    let (body, offset, end) = match control(&statements[0]) {
        Some(Control::Begin) => match statements.last().and_then(|last| control(last)) {
            Some(Control::Commit) => (&statements[1..statements.len() - 1], 1, End::Commit),
            Some(Control::Rollback) => (&statements[1..statements.len() - 1], 1, End::Rollback),
            _ => {
                return Err(misplaced(
                    0,
                    "begins a transaction that the script does not end with COMMIT or ROLLBACK",
                ));
            }
        },
        _ => (statements, 0, End::Commit),
    };
    if let Some(index) = body
        .iter()
        .position(|statement| control(statement).is_some())
    {
        return Err(misplaced(
            index + offset,
            "is transaction control in the middle of a script, which is one transaction",
        ));
    }
    Ok((body, end))
}

/// The statement a failure happened in, named in its message.
fn in_statement(mut error: DbError, index: usize, total: usize) -> DbError {
    error.diagnostic.message = format!(
        "statement {} of {total}: {}",
        index + 1,
        error.diagnostic.message
    );
    error
}

/// What one statement of a script did.
fn statement_record(
    index: usize,
    text: &str,
    rows: usize,
    changed: Option<usize>,
) -> Map<String, Value> {
    let mut record = Map::new();
    record.insert("kind".into(), json!("statement"));
    record.insert("index".into(), json!(index + 1));
    record.insert("statement".into(), json!(text));
    record.insert("rows".into(), json!(rows));
    if let Some(changed) = changed {
        record.insert("changed".into(), json!(changed));
    }
    record
}

/// Run a script as one transaction: every statement sees what the ones
/// before it did, and nothing is written unless all of them succeed and the
/// state they make together is valid.
pub fn script(context: &Context, sink: &mut dyn Sink, statements: &[String]) -> Result<Finish> {
    let (body, end) = frame(statements)?;
    let total = body.len();
    // Every statement is understood before any runs.
    let kinds = body
        .iter()
        .enumerate()
        .map(|(index, text)| {
            reldir_core::sql::classify(text).map_err(|error| in_statement(error, index, total))
        })
        .collect::<Result<Vec<_>>>()?;
    if kinds.iter().all(|kind| *kind == StatementKind::Read) {
        let database = readable(context, sink)?;
        let mut rows = 0;
        for (index, text) in body.iter().enumerate() {
            let count = reldir_core::sql::query(
                &database.catalog.mirror,
                &database.catalog.schemas,
                text,
                &[],
                database.query_limits(),
                |row| sink.row(row),
            )
            .map_err(|error| reldir_core::sql::explain_unknown_table(&database.catalog, error))
            .map_err(|error| in_statement(error, index, total))?;
            rows += count;
            sink.statement(statement_record(index, text, count, None))?;
        }
        return Ok(Finish::ok(format!("{total} statement(s), {rows} row(s)"))
            .with("statements", total)
            .with("rows", rows)
            .with("database_valid", database.is_valid()));
    }
    let mut transaction = begin(context, sink)?;
    for (index, (text, kind)) in body.iter().zip(&kinds).enumerate() {
        run_in(&mut transaction, sink, text, *kind, &[], index)
            .map_err(|error| in_statement(error, index, total))?;
    }
    match end {
        End::Rollback => {
            transaction.rollback()?;
            Ok(rolled_back(total))
        }
        End::Commit => Ok(commit(context, sink, transaction)?.with("statements", total)),
    }
}

/// Open the database for writing and begin a transaction on it.
pub fn begin(context: &Context, sink: &mut dyn Sink) -> Result<Transaction> {
    writable(context, sink)?.begin()
}

/// Run one statement inside a transaction, printing the rows it returns and
/// what it did.
pub fn in_transaction(
    transaction: &mut Transaction,
    sink: &mut dyn Sink,
    text: &str,
    params: &[SqlParam],
) -> Result<Finish> {
    let kind = reldir_core::sql::classify(text)?;
    let index = transaction.statements();
    let rows = run_in(transaction, sink, text, kind, params, index)?;
    Ok(Finish::ok(format!("{rows} row(s), in the open transaction")).with("rows", rows))
}

/// One statement of a transaction; returns how many rows it returned.
fn run_in(
    transaction: &mut Transaction,
    sink: &mut dyn Sink,
    text: &str,
    kind: StatementKind,
    params: &[SqlParam],
    index: usize,
) -> Result<usize> {
    match kind {
        StatementKind::Read => {
            let count = transaction
                .query(text, params, |row| sink.row(row))
                .map_err(|error| {
                    reldir_core::sql::explain_unknown_table(&transaction.view().catalog, error)
                })?;
            sink.statement(statement_record(index, text, count, None))?;
            Ok(count)
        }
        StatementKind::Mutation => {
            let executed = transaction.execute(text, params)?;
            let count = executed.returning.len();
            for row in executed.returning {
                sink.row(row)?;
            }
            sink.statement(statement_record(
                index,
                text,
                count,
                Some(executed.rows.len()),
            ))?;
            Ok(count)
        }
    }
}

/// Commit a transaction as one change.
pub fn commit(context: &Context, sink: &mut dyn Sink, transaction: Transaction) -> Result<Finish> {
    let (_, outcome) = transaction.commit(context.request(Origin::User))?;
    super::committed(sink, &outcome, "committed")
}

/// How a rolled-back transaction ends.
pub fn rolled_back(statements: usize) -> Finish {
    Finish::ok(format!(
        "{statements} statement(s) ran and were rolled back; nothing was written"
    ))
    .with("statements", statements)
    .with("rolled_back", true)
}

/// SQLite's plan for a statement, without running it.
pub fn explain(
    context: &Context,
    sink: &mut dyn Sink,
    text: &str,
    params: &[String],
) -> Result<Finish> {
    let text = statement_text(text)?;
    let params = parse_params(params)?;
    let database = context.open_database(sink, Intent::Read)?;
    let steps = reldir_core::sql::plan(
        &database.catalog.mirror,
        &database.catalog.schemas,
        &text,
        &params,
    )
    .map_err(|error| reldir_core::sql::explain_unknown_table(&database.catalog, error))?;
    let count = steps.len();
    for step in steps {
        let mut record: Map<String, serde_json::Value> = step;
        record.insert("kind".into(), json!("plan_step"));
        sink.record(record)?;
    }
    Ok(Finish::ok(format!("{count} plan step(s)")))
}
