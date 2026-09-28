//! The commands, independent of how they were asked for.
//!
//! Every command is a function from a [`Context`] and its own arguments to a
//! [`Finish`], reporting what it produces through a [`Sink`]. The CLI parses
//! arguments into these calls and renders the sink; the MCP server makes the
//! same calls and returns the sink as JSON. One implementation, two front
//! ends, one contract.

pub mod health;
pub mod history;
pub mod query;
pub mod rows;
pub mod schema;
pub mod setup;

use crate::{
    catalog::Row,
    config::ResourceOverrides,
    db::{Access, Database},
    diagnostic::{DbError, Diagnostic, Result},
    output::{Finish, Sink},
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
    pub overrides: ResourceOverrides,
}

/// Whether a command intends to change anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Reads. Opens with write access where it safely can, so the persistent
    /// mirror stays current and valid external edits are recorded, and with
    /// read access where it cannot.
    Read,
    /// Changes the database.
    Write,
}

impl Context {
    pub fn root(&self) -> Result<PathBuf> {
        Ok(crate::state::resolve_root(self.db.as_deref())?.path)
    }

    /// Open the database, reporting everything opening did.
    pub fn open(&self, sink: &mut dyn Sink, intent: Intent) -> Result<Option<Database>> {
        let root = self.root()?;
        if intent == Intent::Write && self.readonly {
            return Err(DbError::new(
                "READ_ONLY",
                "this command changes the database, and --readonly forbids any write",
                1,
            ));
        }
        let access = if self.readonly {
            Access::Read
        } else if intent == Intent::Read && !safe_to_write(&root) {
            sink.note("the database is on a network or FUSE filesystem; reading without writing derived state");
            Access::Read
        } else {
            Access::Write
        };
        let opening = Opening {
            access,
            establish: !self.no_auto,
            rebuild_metadata: self.rebuild_metadata,
            dry_run: self.dry_run,
        };
        match crate::state::open(&root, opening, &self.overrides)? {
            Opened::Empty => Ok(None),
            Opened::Database { database, transitions, planned } => {
                for transition in &transitions {
                    sink.event("state_transition", transition.to_json(planned), &transition.describe(planned))?;
                }
                for event in &database.events {
                    let value = event.to_json();
                    let kind = value["kind"].as_str().unwrap_or("event").to_string();
                    sink.event(&kind, value, &event.describe())?;
                }
                Ok(Some(*database))
            }
        }
    }

    /// Open a database that must exist and have tables.
    pub fn open_database(&self, sink: &mut dyn Sink, intent: Intent) -> Result<Database> {
        self.open(sink, intent)?.ok_or_else(|| {
            DbError::from_diag(
                Diagnostic::error("UNINITIALIZED", "there is no database here: no .db/, no tables, no pins")
                    .help("create one with `reldir init`, or name one with --db"),
                10,
            )
        })
    }
}

fn safe_to_write(root: &std::path::Path) -> bool {
    match crate::probe::classify(root) {
        crate::probe::Class::Remote(_) => {
            crate::db::load_config(root).is_ok_and(|config| config.allow_remote_filesystem)
        }
        _ => true,
    }
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
    database.catalog.schemas.get(table).ok_or_else(|| database.catalog.unknown_table(table))
}

/// The row a key names. A single-column key is given as its value -- JSON, or
/// bare text for a string -- and a composite key as a JSON array.
pub fn find_row(database: &Database, table: &str, text: &str) -> Result<Row> {
    let schema = schema_of(database, table)?;
    let key = schema.primary_key();
    let candidates: Vec<Vec<Value>> = if key.len() == 1 {
        let mut out = vec![];
        if let Ok(parsed) = crate::json::parse_str(text) {
            out.push(vec![parsed]);
        }
        out.push(vec![Value::String(text.to_string())]);
        out
    } else {
        let parsed = crate::json::parse_str(text).ok().and_then(|value| value.as_array().cloned());
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
        let probe: Map<String, Value> = key.iter().cloned().zip(values).collect();
        if let Some(rendered) = crate::mirror::key(&probe, key, schema)
            && let Some(row) = database.catalog.row_by_key(table, &rendered)?
        {
            return Ok(row);
        }
    }
    Err(DbError::new("UNKNOWN_ROW", format!("{table} has no row with key {text}"), 4))
}

/// Parameters for a SQL statement: `value` for the next positional one, or
/// `name=value` for a named one; values are JSON, or bare text for a string.
pub fn parse_params(texts: &[String]) -> Result<Vec<crate::sql::SqlParam>> {
    texts
        .iter()
        .map(|text| {
            let (name, raw) = match text.split_once('=') {
                Some((name, raw)) if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
                    (Some(name.to_string()), raw)
                }
                _ => (None, text.as_str()),
            };
            let value = crate::json::parse_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
            Ok(crate::sql::SqlParam { name, value })
        })
        .collect()
}

/// Report what a commit did (or would do), and finish with it.
pub fn committed(sink: &mut dyn Sink, outcome: &crate::db::Outcome, verb: &str) -> Result<Finish> {
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
            crate::transaction::Change::Write { path, .. } => ("write", path),
            crate::transaction::Change::Delete { path } => ("delete", path),
        };
        let mut record = Map::new();
        record.insert("kind".into(), json!("change"));
        record.insert("action".into(), json!(action));
        record.insert("path".into(), json!(path));
        record.insert("planned".into(), json!(outcome.dry_run));
        sink.record(record)?;
    }
    for warning in &outcome.warnings {
        sink.diagnostic(warning)?;
    }
    let files = outcome.changes.len();
    let summary = match (outcome.dry_run, outcome.revision) {
        (true, _) => format!("would {verb}: {files} file(s); nothing was written"),
        (false, Some(revision)) => format!("{verb}: {files} file(s), revision {revision}"),
        (false, None) => "no change".to_string(),
    };
    let mut finish = Finish::ok(summary).with("files", files).with("dry_run", outcome.dry_run);
    if let Some(revision) = outcome.revision {
        finish = finish.with("revision", revision);
    }
    Ok(finish)
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
            Diagnostic::error("DECISION_REQUIRED", format!("{question} -- this needs a decision"))
                .help("pass --yes to confirm, or --dry-run to see exactly what would change"),
            9,
        ));
    }
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer).map_err(|error| DbError::io(std::path::Path::new("stdin"), error))?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
        Ok(())
    } else {
        Err(DbError::new("DECISION_REQUIRED", "declined; nothing was changed", 9))
    }
}

/// Read a whole input -- a file, or `-` for stdin -- up to a byte limit.
pub fn read_input(source: &str, limit: u64) -> Result<String> {
    use std::io::Read;
    let (reader, path): (Box<dyn Read>, PathBuf) = if source == "-" {
        (Box::new(std::io::stdin().lock()), PathBuf::from("stdin"))
    } else {
        let path = PathBuf::from(source);
        (Box::new(std::fs::File::open(&path).map_err(|error| DbError::io(&path, error))?), path)
    };
    let mut bytes = vec![];
    reader.take(limit.saturating_add(1)).read_to_end(&mut bytes).map_err(|error| DbError::io(&path, error))?;
    if bytes.len() as u64 > limit {
        return Err(DbError::new(
            "RESOURCE_LIMIT",
            format!("{} is larger than the {limit}-byte limit", path.display()),
            2,
        ));
    }
    String::from_utf8(bytes).map_err(|error| DbError::usage(format!("{} is not UTF-8: {error}", path.display())))
}
