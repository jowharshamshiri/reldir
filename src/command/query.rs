//! SQL: queries answered from the mirror, mutations turned into validated
//! transactions.

use super::{Context, Intent, parse_params, require_valid};
use crate::{
    db::Request,
    diagnostic::{DbError, Result},
    output::{Finish, Sink},
    sql::StatementKind,
};
use serde_json::{Map, json};

/// The statement text: as given, or -- for `-` -- read from stdin.
pub fn statement_text(text: &str) -> Result<String> {
    if text == "-" {
        super::read_input("-", 16 * 1024 * 1024)
    } else {
        Ok(text.to_string())
    }
}

pub fn sql(context: &Context, sink: &mut dyn Sink, text: &str, params: &[String]) -> Result<Finish> {
    let text = statement_text(text)?;
    let params = parse_params(params)?;
    match crate::sql::classify(&text)? {
        StatementKind::Read => {
            let Some(database) = context.open(sink, Intent::Read)? else {
                return Err(DbError::new("UNKNOWN_TABLE", "there are no tables here to query", 4)
                    .with_help("create a database with `reldir init`, or name one with --db"));
            };
            if context.allow_invalid {
                if !database.is_valid() {
                    for diagnostic in &database.verdict.errors {
                        sink.diagnostic(diagnostic)?;
                    }
                    sink.note("answering from an INVALID database because of --allow-invalid");
                }
            } else {
                require_valid(&database)?;
            }
            let count = crate::sql::query(
                &database.catalog.mirror,
                &database.catalog.schemas,
                &text,
                &params,
                database.query_limits(),
                |row| sink.record(row),
            )
            .map_err(|error| crate::sql::explain_unknown_table(&database.catalog, error))?;
            Ok(Finish::ok(format!("{count} row(s)")).with("rows", count).with("database_valid", database.is_valid()))
        }
        StatementKind::Mutation => {
            if context.allow_invalid {
                return Err(DbError::usage(
                    "--allow-invalid answers queries; a change to an invalid database would be judged against faults it did not make",
                )
                .with_help("repair the database with `reldir doctor` first"));
            }
            let mut database = context.open_database(sink, Intent::Write)?;
            require_valid(&database)?;
            let mutation = crate::sql::mutate(&database.catalog, &text, &params, database.query_limits())
                .map_err(|error| crate::sql::explain_unknown_table(&database.catalog, error))?;
            let outcome = database.apply(mutation.rows, Request::internal(context.dry_run))?;
            for row in mutation.returning {
                let mut record = row;
                record.insert("kind".into(), json!("returning"));
                sink.record(record)?;
            }
            super::committed(sink, &outcome, "committed")
        }
    }
}

/// SQLite's plan for a statement, without running it.
pub fn explain(context: &Context, sink: &mut dyn Sink, text: &str, params: &[String]) -> Result<Finish> {
    let text = statement_text(text)?;
    let params = parse_params(params)?;
    let database = context.open_database(sink, Intent::Read)?;
    let steps = crate::sql::plan(&database.catalog.mirror, &database.catalog.schemas, &text, &params)
        .map_err(|error| crate::sql::explain_unknown_table(&database.catalog, error))?;
    let count = steps.len();
    for step in steps {
        let mut record: Map<String, serde_json::Value> = step;
        record.insert("kind".into(), json!("plan_step"));
        sink.record(record)?;
    }
    Ok(Finish::ok(format!("{count} plan step(s)")))
}
