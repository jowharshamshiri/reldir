//! The commands, independent of how they were asked for.
//!
//! Every command is a function from a [`Context`] and its own arguments to a
//! [`Finish`], reporting what it produces through a [`Sink`]. The CLI parses
//! arguments into these calls and renders the sink; the MCP server makes the
//! same calls and returns the sink as JSON. One implementation, two front
//! ends, one contract.

pub mod git;
pub mod health;
pub mod history;
pub mod query;
pub mod rows;
pub mod schema;
pub mod setup;
pub mod transfer;

use crate::output::{Finish, Sink};
use reldir_core::code::Code;
use reldir_core::{
    catalog::Row,
    config::ResourceOverrides,
    db::{Access, Database},
    diagnostic::{DbError, Diagnostic, Result},
    schema::Schema,
    state::{Opened, Opening},
};
use serde_json::{Map, Value, json};
use std::path::PathBuf;

/// What every command is run with.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// `--db`: the root, exactly.
    pub db: Option<PathBuf>,
    /// Write nothing at all, not even derived state.
    pub readonly: bool,
    /// Never establish a database implicitly.
    pub no_auto: bool,
    /// Authorise discarding an unlabelled `.db/` that holds history.
    pub rebuild_metadata: bool,
    /// Plan and validate every change; write none.
    pub dry_run: bool,
    /// Confirm prompts in advance.
    pub yes: bool,
    /// Answer read-only queries from an invalid database.
    pub allow_invalid: bool,
    /// Authorise operations that discard data or history.
    pub allow_destructive: bool,
    /// The caller's name for the change this command makes.
    pub idempotency_key: Option<String>,
    /// Widen contracts to admit what the change writes.
    pub widen: bool,
    /// The state a planned change must still find to be committed.
    pub expect_state: Option<String>,
    /// Set when the caller no longer wants the answer: a running query stops.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub overrides: ResourceOverrides,
}

/// Whether a command intends to change anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Reads. Observes and records under the writer lock when it is free, so
    /// the mirror stays current and valid outside edits are recorded; never
    /// waits for a writer that holds it.
    Read,
    /// Changes the database.
    Write,
}

impl Context {
    /// How a change this command makes is to be committed: under the flags
    /// the command was given, recorded with `origin`, and admitted only into
    /// a valid database.
    pub fn request(
        &self,
        origin: reldir_core::store::revision::Origin,
    ) -> reldir_core::db::Request {
        reldir_core::db::Request {
            origin,
            admission: reldir_core::db::Admission::Valid,
            dry_run: self.dry_run,
            idempotency_key: self.idempotency_key.clone(),
            widen: self.widen,
            expect_state: self.expect_state.clone(),
        }
    }

    pub fn root(&self) -> Result<PathBuf> {
        Ok(reldir_core::state::resolve_root(self.db.as_deref())?.path)
    }

    /// Open the database, reporting everything opening did.
    pub fn open(&self, sink: &mut dyn Sink, intent: Intent) -> Result<Option<Database>> {
        let root = self.root()?;
        if intent == Intent::Write && self.readonly {
            return Err(DbError::new(
                Code::READ_ONLY,
                "this command changes the database, and --readonly forbids any write",
            ));
        }
        let access = if self.readonly {
            Access::Read
        } else if intent == Intent::Read && !safe_to_write(&root) {
            sink.note("the database is on a filesystem whose locks and renames reldir cannot rely on; reading without writing derived state");
            Access::Read
        } else if intent == Intent::Read {
            Access::Observe
        } else {
            Access::Write
        };
        let opening = Opening {
            access,
            establish: !self.no_auto,
            rebuild_metadata: self.rebuild_metadata,
            dry_run: self.dry_run,
            ignore: vec![],
        };
        match reldir_core::state::open(&root, opening, &self.overrides)? {
            Opened::Empty => Ok(None),
            Opened::Database {
                database,
                transitions,
                planned,
            } => {
                for transition in &transitions {
                    sink.event(
                        "state_transition",
                        transition.to_json(planned),
                        &transition.describe(planned),
                    )?;
                }
                for event in &database.events {
                    let value = event.to_json();
                    let kind = value["kind"].as_str().unwrap_or("event").to_string();
                    sink.event(&kind, value, &event.describe())?;
                }
                for notice in &database.notices {
                    sink.diagnostic(notice)?;
                }
                let mut database = *database;
                database.cancel = self.cancel.clone();
                Ok(Some(database))
            }
        }
    }

    /// Open a database that must exist and have tables.
    pub fn open_database(&self, sink: &mut dyn Sink, intent: Intent) -> Result<Database> {
        self.open(sink, intent)?.ok_or_else(|| {
            DbError::from_diag(
                Diagnostic::error(
                    Code::UNINITIALIZED,
                    "there is no database here: no .db/, no tables, no pins",
                )
                .help("create one with `reldir init`, or name one with --db"),
            )
        })
    }
}

fn safe_to_write(root: &std::path::Path) -> bool {
    match reldir_core::db::volume_class(root) {
        Ok(reldir_core::engine::fs::VolumeClass::Local) => true,
        // Unsafe, or not to be described: reading writes nothing unless the
        // configuration says someone decided this filesystem is safe.
        _ => reldir_core::db::load_config(root).is_ok_and(|config| config.allow_remote_filesystem),
    }
}

/// Whether a read may answer: from a valid database always; from an invalid
/// one only under `--allow-invalid`, and then after saying what is wrong.
pub fn gate(context: &Context, database: &Database, sink: &mut dyn Sink) -> Result<()> {
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

/// The answer for a folder with nothing in it.
pub fn empty() -> Finish {
    Finish::ok("no tables, no data")
        .with("valid", true)
        .with("state", "EMPTY")
        .with("tables", 0)
        .with("rows", 0)
}

/// Refuse to act on an invalid database, carrying every fault.
pub fn require_valid(database: &Database) -> Result<()> {
    database.require_valid().map_err(|error| {
        error.with_help("run `reldir check` to see every fault and `reldir doctor` for repairs")
    })
}

pub fn schema_of<'d>(database: &'d Database, table: &str) -> Result<&'d Schema> {
    database
        .catalog
        .schemas
        .get(table)
        .ok_or_else(|| database.catalog.unknown_table(table))
}

/// The key a command line names: a single-column key as its value -- JSON,
/// or bare text for a string -- and a composite key as a JSON array. The
/// values are those of the row it names, when one does.
pub fn key_values(
    catalog: &reldir_core::catalog::Catalog,
    table: &str,
    text: &str,
) -> Result<Vec<Value>> {
    let schema = reldir_core::rows::schema(catalog, table)?;
    let key = schema.primary_key();
    let candidates: Vec<Vec<Value>> = if key.len() == 1 {
        let mut out = vec![];
        if let Ok(parsed) = reldir_core::json::parse_str(text) {
            out.push(vec![parsed]);
        }
        out.push(vec![Value::String(text.to_string())]);
        out
    } else {
        let parsed = reldir_core::json::parse_str(text)
            .ok()
            .and_then(|value| value.as_array().cloned());
        match parsed {
            Some(values) if values.len() == key.len() => vec![values],
            _ => {
                return Err(DbError::usage(format!(
                    "{table} is keyed on ({}), so a key is a JSON array of {} values",
                    key.join(", "),
                    key.len()
                )));
            }
        }
    };
    for values in candidates {
        if reldir_core::rows::find(catalog, table, &values)?.is_some() {
            return Ok(values);
        }
    }
    Err(DbError::new(
        Code::UNKNOWN_ROW,
        format!("{table} has no row with key {text}"),
    ))
}

/// The row a command line's key names.
pub fn find_row(catalog: &reldir_core::catalog::Catalog, table: &str, text: &str) -> Result<Row> {
    reldir_core::rows::existing(catalog, table, &key_values(catalog, table, text)?)
}

/// Parameters for a SQL statement: `value` for the next positional one, or
/// `name=value` for a named one; values are JSON, or bare text for a string.
pub fn parse_params(texts: &[String]) -> Result<Vec<reldir_core::sql::SqlParam>> {
    texts
        .iter()
        .map(|text| {
            let (name, raw) = match text.split_once('=') {
                Some((name, raw))
                    if !name.is_empty()
                        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
                {
                    (Some(name.to_string()), raw)
                }
                _ => (None, text.as_str()),
            };
            // Always JSON: reading `7` as a number but `007` as text, or `a=b`
            // as a named parameter when text was meant, would bind a value
            // other than the one intended and the statement would quietly
            // match nothing. No JSON value begins `identifier=`, so the name
            // split cannot misread one.
            let value = reldir_core::json::parse_str(raw).map_err(|error| {
                DbError::usage(format!("--param {text:?} is not a JSON value ({error})")).with_help(format!(
                    "a parameter is JSON: text is quoted, as in --param '\"{raw}\"'; a number, true, false or null is bare"
                ))
            })?;
            Ok(reldir_core::sql::SqlParam { name, value })
        })
        .collect()
}

/// Report what a commit did (or would do), and finish with it.
pub fn committed(
    sink: &mut dyn Sink,
    outcome: &reldir_core::db::Outcome,
    verb: &str,
) -> Result<Finish> {
    for induced in &outcome.induced {
        sink.event(
            "referential_action",
            json!({
                "path": induced.path,
                "action": induced.action.name(),
                "constraint": induced.constraint,
                "key": induced.key,
                "because": induced.because,
            }),
            &format!(
                "{} {} ({} of {} in {})",
                induced.action.name(),
                induced.path.display(),
                induced.constraint,
                induced.key,
                induced.because.display()
            ),
        )?;
    }
    for change in &outcome.changes {
        let (action, path) = match change {
            reldir_core::plan::FileChange::Write { path, .. } => ("write", path),
            reldir_core::plan::FileChange::Delete { path } => ("delete", path),
        };
        let mut record = Map::new();
        record.insert("kind".into(), json!("change"));
        record.insert("action".into(), json!(action));
        record.insert("path".into(), json!(path));
        record.insert("planned".into(), json!(outcome.dry_run));
        sink.record(record)?;
    }
    for (table, edits) in &outcome.widened {
        let edits: Vec<String> = edits.iter().map(ToString::to_string).collect();
        sink.event(
            "contract_widened",
            json!({"table": table, "edits": edits}),
            &format!("widened the contract of {table}: {}", edits.join("; ")),
        )?;
    }
    for warning in &outcome.warnings {
        sink.diagnostic(warning)?;
    }
    for path in &outcome.kept_outside_edits {
        sink.event(
            "kept_outside_edit",
            json!({"path": path}),
            &format!(
                "{} was edited outside reldir while the change was being made; the edit was kept \
                 and is recorded next",
                path.display()
            ),
        )?;
    }
    let files = outcome.changes.len();
    let summary = match (outcome.dry_run, outcome.replayed, outcome.revision) {
        (true, _, _) => format!("dry run: {files} file(s) would change; nothing was written"),
        (false, true, Some(revision)) => format!(
            "already made: revision {revision} carries this idempotency key; nothing was changed again"
        ),
        (false, _, Some(revision)) => format!("{verb}: {files} file(s), revision {revision}"),
        (false, _, None) => "no change".to_string(),
    };
    let mut finish = Finish::ok(summary)
        .with("files", files)
        .with("dry_run", outcome.dry_run)
        .with("replayed", outcome.replayed);
    if let Some(revision) = outcome.revision {
        finish = finish.with("revision", revision.to_json());
    }
    if let Some(state) = &outcome.planned_against {
        finish = finish.with("state", state.as_str());
    }
    Ok(finish)
}

/// The finish of a change already made under this command's idempotency key,
/// if there is one. A command that plans its change before committing it asks
/// this before planning, since a plan for a change already made may no longer
/// apply: a migration that dropped a column cannot drop it again.
pub fn replayed(
    context: &Context,
    sink: &mut dyn Sink,
    database: &reldir_core::db::Database,
    origin: reldir_core::store::revision::Origin,
) -> Result<Option<Finish>> {
    match database.replayed(&context.request(origin))? {
        Some(outcome) => committed(sink, &outcome, "replayed").map(Some),
        None => Ok(None),
    }
}

/// Ask before doing something that cannot be taken back. Without a terminal to
/// ask on, `--yes` is the answer; without either, the command stops with
/// `DECISION_REQUIRED` and says what it would do.
pub fn confirm(context: &Context, question: &str) -> Result<()> {
    use std::io::{BufRead, IsTerminal, Write};
    if context.yes || context.dry_run {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(DbError::from_diag(
            Diagnostic::error(
                Code::DECISION_REQUIRED,
                format!("{question} -- this needs a decision"),
            )
            .help("pass --yes to confirm, or --dry-run to see exactly what would change"),
        ));
    }
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|error| DbError::io(std::path::Path::new("stdin"), error))?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
        Ok(())
    } else {
        Err(DbError::new(
            Code::DECISION_REQUIRED,
            "declined; nothing was changed",
        ))
    }
}

/// Read a whole input -- a file, or `-` for stdin -- up to a byte limit.
pub fn read_input(source: &str, limit: u64) -> Result<String> {
    use std::io::Read;
    let (reader, path): (Box<dyn Read>, PathBuf) = if source == "-" {
        (Box::new(std::io::stdin().lock()), PathBuf::from("stdin"))
    } else {
        let path = PathBuf::from(source);
        (
            Box::new(std::fs::File::open(&path).map_err(|error| DbError::io(&path, error))?),
            path,
        )
    };
    let mut bytes = vec![];
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| DbError::io(&path, error))?;
    if bytes.len() as u64 > limit {
        return Err(DbError::new(
            Code::RESOURCE_LIMIT,
            format!("{} is larger than the {limit}-byte limit", path.display()),
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| DbError::usage(format!("{} is not UTF-8: {error}", path.display())))
}
