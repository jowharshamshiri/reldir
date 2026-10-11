//! `reldir shell`: SQL and dot-commands, one statement at a time.
//!
//! A statement runs when SQLite would say it is complete -- a line ending in
//! `;` outside any string or comment -- so a statement can span lines and a
//! `;` inside a string does not cut it short. Each statement is its own
//! transaction, until `BEGIN` or `.begin` opens one that holds every
//! statement after it until `COMMIT` or `.commit` writes them as one change,
//! or `ROLLBACK` or `.rollback` discards them; a statement that fails inside
//! it rolls it all back. Tables, columns, keywords and
//! dot-commands complete with Tab, and history persists between sessions in
//! `$XDG_STATE_HOME/reldir/history`.

use crate::{
    cli::{Command, SchemaCommand, command_name, dispatch},
    command::{Context, parse_params, query},
    output::{Finish, Format, Terminal},
};
use reldir_core::{
    diagnostic::Result,
    sql::{Control, control},
    transaction::Transaction,
};
use rustyline::{
    Context as LineContext, Editor, Helper,
    completion::{Completer, Pair},
    highlight::Highlighter,
    hint::Hinter,
    history::DefaultHistory,
    validate::{ValidationContext, ValidationResult, Validator},
};
use std::{cell::RefCell, path::PathBuf};

const DOT_COMMANDS: &[(&str, &str)] = &[
    (".help", "this list"),
    (".tables", "the tables"),
    (".describe TABLE", "a table's columns, key and references"),
    (".schema TABLE", "a table's schema document"),
    (".status", "whether the database is valid, and its revision"),
    (".check", "every fault"),
    (".log", "recent revisions"),
    (
        ".begin",
        "open a transaction: the statements that follow are one change (also BEGIN;)",
    ),
    (
        ".commit",
        "write the open transaction as one change (also COMMIT;)",
    ),
    (".rollback", "discard the open transaction (also ROLLBACK;)"),
    (
        ".param VALUE ...",
        "values for the `?` of the statements that follow; `.param` lists them, `.param clear` forgets them",
    ),
    (
        ".explain on|off",
        "show SQLite's plan for each statement instead of running it",
    ),
    (".timer on|off", "say how long each statement took"),
    (".format FORMAT", "table, json, jsonl or csv"),
    (".quit", "leave (also .exit, or Ctrl-D)"),
];

const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "JOIN",
    "LEFT JOIN",
    "ON",
    "AS",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "LIKE",
    "BETWEEN",
    "DISTINCT",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "INSERT INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE FROM",
    "RETURNING",
    "WITH",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "EXPLAIN",
    "ON CONFLICT",
    "DO UPDATE",
    "DO NOTHING",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
];

/// What Tab completes from, refreshed as tables change.
struct Words {
    words: RefCell<Vec<String>>,
}

impl Words {
    fn refresh(&self, context: &Context) {
        let mut words: Vec<String> = KEYWORDS.iter().map(|word| word.to_string()).collect();
        words.extend(
            DOT_COMMANDS
                .iter()
                .filter_map(|(command, _)| command.split_whitespace().next())
                .map(String::from),
        );
        if let Some(database) = open_quietly(context) {
            for (table, schema) in &database.catalog.schemas {
                words.push(table.clone());
                words.extend(schema.columns().keys().cloned());
            }
        }
        words.sort();
        words.dedup();
        *self.words.borrow_mut() = words;
    }
}

/// The database, opened to read its tables -- for completion and the
/// banner -- without printing what opening did.
fn open_quietly(context: &Context) -> Option<reldir_core::db::Database> {
    let mut quiet = crate::output::Collect::default();
    context
        .open(&mut quiet, crate::command::Intent::Read)
        .ok()
        .flatten()
}

impl Completer for Words {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _: &LineContext<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let start = line[..pos]
            .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
            .map_or(0, |at| at + 1);
        let prefix = &line[start..pos];
        if prefix.is_empty() {
            return Ok((start, vec![]));
        }
        let lowered = prefix.to_lowercase();
        let candidates = self
            .words
            .borrow()
            .iter()
            .filter(|word| word.to_lowercase().starts_with(&lowered))
            .map(|word| Pair {
                display: word.clone(),
                replacement: word.clone(),
            })
            .collect();
        Ok((start, candidates))
    }
}

impl Hinter for Words {
    type Hint = String;
}

impl Highlighter for Words {}

impl Validator for Words {
    fn validate(&self, context: &mut ValidationContext<'_>) -> rustyline::Result<ValidationResult> {
        let input = context.input().trim();
        if input.is_empty() || input.starts_with('.') || reldir_core::sql::is_complete(input) {
            Ok(ValidationResult::Valid(None))
        } else {
            Ok(ValidationResult::Incomplete)
        }
    }
}

impl Helper for Words {}

/// Where the shell's history is kept between sessions.
fn history_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            if cfg!(windows) {
                std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
            } else {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            }
        })?;
    let directory = base.join("reldir");
    std::fs::create_dir_all(&directory).ok()?;
    Some(directory.join("history"))
}

/// What the session remembers between statements.
struct Session {
    format: Format,
    params: Vec<String>,
    explain: bool,
    timer: bool,
    /// The transaction `BEGIN` opened, until it ends.
    transaction: Option<Transaction>,
}

/// Run the shell; returns the exit status.
pub fn run(context: &Context, format: Format) -> i32 {
    let Ok(mut editor) = Editor::<Words, DefaultHistory>::new() else {
        eprintln!("the shell could not start: no terminal it can read lines from");
        return 1;
    };
    let words = Words {
        words: RefCell::new(vec![]),
    };
    words.refresh(context);
    editor.set_helper(Some(words));
    let history = history_path();
    if let Some(path) = &history {
        // A first session has no history yet; anything else that stops it
        // loading costs only the recall of earlier lines.
        let _ = editor.load_history(path);
    }
    eprintln!(
        "reldir {} -- SQL ending in `;`, or .help",
        reldir_core::VERSION
    );
    match open_quietly(context) {
        Some(database) if database.catalog.schemas.is_empty() => {
            eprintln!(
                "no tables yet: put JSON files in a folder here and reldir adopts it, or declare \
                 one with `reldir schema new <table>`"
            )
        }
        None => eprintln!(
            "no database here: put JSON files in a folder here and reldir adopts it, or name one \
             with --db"
        ),
        Some(_) => {}
    }
    let mut session = Session {
        format: if format.is_machine() && format != Format::Sarif {
            format
        } else {
            Format::Table
        },
        params: vec![],
        explain: false,
        timer: false,
        transaction: None,
    };
    let status = loop {
        let prompt = if session.transaction.is_some() {
            "reldir*> "
        } else {
            "reldir> "
        };
        let line = match editor.readline(prompt) {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(rustyline::error::ReadlineError::Eof) => break 0,
            Err(error) => {
                eprintln!("{error}");
                break 1;
            }
        };
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(input);
        if input.starts_with('.') {
            match dot(context, &mut session, input) {
                Dot::Continue => {}
                Dot::Changed => {
                    if let Some(helper) = editor.helper() {
                        helper.refresh(context);
                    }
                }
                Dot::Quit => break 0,
            }
            continue;
        }
        for statement in reldir_core::sql::split_statements(input) {
            let started = std::time::Instant::now();
            run_statement(context, &mut session, statement);
            if session.timer {
                eprintln!("({:.3}s)", started.elapsed().as_secs_f64());
            }
        }
        if let Some(helper) = editor.helper() {
            helper.refresh(context);
        }
    };
    if let Some(transaction) = session.transaction.take() {
        let statements = transaction.statements();
        match transaction.rollback() {
            Ok(_) => eprintln!(
                "the open transaction's {statements} statement(s) were rolled back; nothing was \
                 written"
            ),
            Err(error) => eprintln!("{}", error.diagnostic.message),
        }
    }
    if let Some(path) = &history
        && let Err(error) = editor.save_history(path)
    {
        eprintln!(
            "the shell's history could not be kept in {}: {error}",
            path.display()
        );
    }
    status
}

/// Run one statement: inside the open transaction, or as its own.
fn run_statement(context: &Context, session: &mut Session, statement: String) {
    match control(&statement) {
        Some(Control::Begin) => begin(context, session),
        Some(Control::Commit) => commit(context, session),
        Some(Control::Rollback) => rollback(session),
        None if session.explain => run_command(
            context,
            session.format,
            Command::Explain {
                statement,
                params: session.params.clone(),
            },
        ),
        None => match session.transaction.as_mut() {
            Some(transaction) => {
                let mut terminal = Terminal::new(session.format, "sql");
                let outcome = parse_params(&session.params).and_then(|params| {
                    query::in_transaction(transaction, &mut terminal, &statement, &params)
                });
                if outcome.is_err() {
                    // A failed statement ends the transaction: what it did
                    // before failing is undone with everything else.
                    if let Some(transaction) = session.transaction.take() {
                        let _ = transaction.rollback();
                    }
                }
                let outcome = outcome.map_err(|error| {
                    error.with_help(
                        "the transaction was rolled back, and nothing it did was written; \
                         `.begin` opens another",
                    )
                });
                terminal.finish(outcome);
            }
            None => run_command(
                context,
                session.format,
                Command::Sql {
                    statement: Some(statement),
                    file: None,
                    params: session.params.clone(),
                },
            ),
        },
    }
}

fn begin(context: &Context, session: &mut Session) {
    if session.transaction.is_some() {
        eprintln!("a transaction is already open; .commit or .rollback ends it");
        return;
    }
    let mut terminal = Terminal::new(session.format, "sql");
    let outcome = query::begin(context, &mut terminal).map(|transaction| {
        session.transaction = Some(transaction);
        Finish::ok(
            "transaction open: the statements that follow are one change, until .commit or \
             .rollback",
        )
    });
    terminal.finish(outcome);
}

fn commit(context: &Context, session: &mut Session) {
    let Some(transaction) = session.transaction.take() else {
        eprintln!("no transaction is open; .begin opens one");
        return;
    };
    let mut terminal = Terminal::new(session.format, "sql");
    let outcome = query::commit(context, &mut terminal, transaction);
    terminal.finish(outcome);
}

fn rollback(session: &mut Session) {
    let Some(transaction) = session.transaction.take() else {
        eprintln!("no transaction is open; .begin opens one");
        return;
    };
    let terminal = Terminal::new(session.format, "sql");
    let statements = transaction.statements();
    let outcome: Result<Finish> = transaction
        .rollback()
        .map(|_| query::rolled_back(statements));
    terminal.finish(outcome);
}

fn run_command(context: &Context, format: Format, command: Command) {
    let mut terminal = Terminal::new(format, command_name(&command));
    let outcome = dispatch(context, &mut terminal, command);
    terminal.finish(outcome);
}

enum Dot {
    Continue,
    /// The tables may have changed.
    Changed,
    Quit,
}

fn dot(context: &Context, session: &mut Session, input: &str) -> Dot {
    let mut words = input.split_whitespace();
    let name = words.next().unwrap_or_default();
    let rest: Vec<&str> = words.collect();
    let toggle = |rest: &[&str], current: bool| -> Option<bool> {
        match rest {
            ["on"] => Some(true),
            ["off"] => Some(false),
            [] => Some(!current),
            _ => None,
        }
    };
    let command = match (name, rest.as_slice()) {
        (".quit" | ".exit", _) => return Dot::Quit,
        (".begin", []) => {
            begin(context, session);
            return Dot::Continue;
        }
        (".commit", []) => {
            commit(context, session);
            return Dot::Changed;
        }
        (".rollback", []) => {
            rollback(session);
            return Dot::Continue;
        }
        (".help", _) => {
            let width = DOT_COMMANDS
                .iter()
                .map(|(command, _)| command.len())
                .max()
                .unwrap_or(0);
            for (command, what) in DOT_COMMANDS {
                eprintln!("{command:<width$}  {what}");
            }
            return Dot::Continue;
        }
        (".tables", []) => Command::Tables,
        (".describe", [table]) => Command::Describe {
            table: table.to_string(),
        },
        (".schema", [table]) => Command::Schema(SchemaCommand::Show {
            table: table.to_string(),
        }),
        (".status", []) => Command::Status,
        (".check", []) => Command::Check {
            strict: false,
            deep: false,
        },
        (".log", []) => Command::Log { limit: Some(10) },
        (".param", []) => {
            if session.params.is_empty() {
                eprintln!("no parameters; `.param VALUE ...` sets them");
            }
            for (index, value) in session.params.iter().enumerate() {
                eprintln!("?{} = {value}", index + 1);
            }
            return Dot::Continue;
        }
        (".param", ["clear"]) => {
            session.params.clear();
            return Dot::Continue;
        }
        (".param", values) => {
            // Each value is what `--param` takes: JSON, or text that is not.
            session.params = values.iter().map(|value| value.to_string()).collect();
            return Dot::Continue;
        }
        (".explain", rest) => {
            match toggle(rest, session.explain) {
                Some(explain) => session.explain = explain,
                None => eprintln!(".explain takes on or off"),
            }
            return Dot::Continue;
        }
        (".timer", rest) => {
            match toggle(rest, session.timer) {
                Some(timer) => session.timer = timer,
                None => eprintln!(".timer takes on or off"),
            }
            return Dot::Continue;
        }
        (".format", [format]) => {
            match Format::parse(format) {
                Ok(Format::Sarif) => eprintln!("the shell prints table, json, jsonl or csv"),
                Ok(format) => session.format = format,
                Err(error) => eprintln!("{}", error.diagnostic.message),
            }
            return Dot::Continue;
        }
        _ => {
            eprintln!("{input:?} is not a dot-command the shell knows; .help lists them");
            return Dot::Continue;
        }
    };
    run_command(context, session.format, command);
    Dot::Changed
}
