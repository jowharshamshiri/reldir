//! The tools: what each is called, what it takes, what it may do, and the
//! command it runs.
//!
//! A tool's arguments are held to its input schema before anything runs; an
//! argument the schema does not name, or one of the wrong type, is refused as
//! a protocol error rather than ignored or guessed at.

use crate::{
    cli::{Command, SchemaCommand, SnapshotCommand},
    command::{Context, parse_params},
};
use reldir_core::{
    code::Code,
    diagnostic::{DbError, Result},
    migrate::Migration,
    sql::{SqlParam, Window},
    store::Hash256,
};
use rmcp::model::{Tool, ToolAnnotations};
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Rows a page holds when the caller does not say.
pub const DEFAULT_PAGE: usize = 200;
/// The most rows one page may hold.
pub const MAX_PAGE: usize = 10_000;

/// What a tool may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Reads; changes nothing.
    Read,
    /// Changes the database. A call without `plan_id` plans the change and
    /// writes nothing; the call that names the plan commits it, if the
    /// database is still in the state it was planned against.
    Planned {
        /// Whether the tool can remove or rewrite what is there.
        destructive: bool,
    },
    /// Writes beside the database's rows -- a snapshot, an exported file --
    /// in one step.
    Direct,
}

pub struct Spec {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// The arguments, as JSON Schema properties.
    pub properties: Value,
    pub required: &'static [&'static str],
    /// Further keywords of the input schema.
    pub rules: Value,
    pub effect: Effect,
}

impl Spec {
    /// The input schema: the tool's own arguments, closed, and for a planned
    /// tool the plan to commit and its confirmation.
    pub fn input_schema(&self) -> Value {
        let mut properties = self.properties.as_object().cloned().unwrap_or_default();
        if let Effect::Planned { .. } = self.effect {
            properties.insert(
                "plan_id".into(),
                json!({"type": "string", "description": "Commit the plan an earlier call with the same arguments returned. Without it, the call only plans: it validates the change and reports every file it would touch, and writes nothing."}),
            );
            properties.insert(
                "confirm".into(),
                json!({"type": "boolean", "description": "Confirms a plan that removes rows or runs referential actions, when the client cannot ask its user."}),
            );
        }
        let mut schema = json!({
            "type": "object",
            "properties": properties,
            "required": self.required,
            "additionalProperties": false,
        });
        if let (Value::Object(schema), Value::Object(rules)) = (&mut schema, &self.rules) {
            schema.extend(rules.clone());
        }
        schema
    }

    pub fn tool(&self) -> Tool {
        let annotations = match self.effect {
            Effect::Read => ToolAnnotations::new()
                .read_only(true)
                .idempotent(true)
                .open_world(false),
            Effect::Planned { destructive } => ToolAnnotations::new()
                .read_only(false)
                .destructive(destructive)
                .idempotent(false)
                .open_world(false),
            Effect::Direct => ToolAnnotations::new()
                .read_only(false)
                .destructive(false)
                .idempotent(false)
                .open_world(false),
        };
        Tool::new(self.name, self.description, object(self.input_schema()))
            .with_title(self.title)
            .with_annotations(annotations)
            .with_raw_output_schema(output_schema())
    }
}

fn object(value: Value) -> Arc<Map<String, Value>> {
    Arc::new(value.as_object().cloned().unwrap_or_default())
}

/// Every tool answers with the protocol's `result` document.
fn output_schema() -> Arc<Map<String, Value>> {
    let protocol = crate::protocol::schema();
    object(json!({
        "type": "object",
        "$ref": "#/$defs/result",
        "$defs": protocol["$defs"],
    }))
}

fn table() -> Value {
    json!({"type": "string", "description": "A table's name"})
}

fn key() -> Value {
    json!({"description": "A row's key: its value, or for a composite key an array of values in key order"})
}

fn params() -> Value {
    json!({"type": "array", "description": "Values for the statement's `?` placeholders, in order"})
}

fn revision() -> Value {
    json!({"type": ["integer", "string"], "description": "A revision: its number, or a prefix of its id"})
}

/// Every tool.
pub fn specs() -> Vec<Spec> {
    let read = Effect::Read;
    let removes = Effect::Planned { destructive: true };
    let adds = Effect::Planned { destructive: false };
    vec![
        Spec {
            name: "status",
            title: "Database status",
            description: "Whether the database is valid, what changed outside reldir and is not yet recorded, and the current revision.",
            properties: json!({}),
            required: &[],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "tables",
            title: "List tables",
            description: "The tables, with their row counts and keys.",
            properties: json!({}),
            required: &[],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "describe",
            title: "Describe a table",
            description: "A table's columns and their types, its key, and the references into and out of it.",
            properties: json!({"table": table()}),
            required: &["table"],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "schema_show",
            title: "Show a schema",
            description: "A table's schema document: JSON Schema 2020-12 in reldir's dialect.",
            properties: json!({"table": table()}),
            required: &["table"],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "get_row",
            title: "Get a row",
            description: "One row, found by its key.",
            properties: json!({"table": table(), "key": key()}),
            required: &["table", "key"],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "list_rows",
            title: "List rows",
            description: "A table's rows, optionally filtered by a SQL condition and ordered by a column (`-column` for descending).",
            properties: json!({
                "table": table(),
                "where": {"type": "string", "description": "A SQL condition over the table's columns"},
                "order": {"type": "string", "description": "A column, or -column for descending"},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_PAGE, "default": DEFAULT_PAGE}
            }),
            required: &["table"],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "query",
            title: "Run a query",
            description: "Run one read-only SQL statement (SQLite's dialect) over the tables. JSON columns are read with json_extract and json_each. Answers a page of rows at a time: when more remain, `data.next_cursor` continues from where this page ended, for as long as the database does not change.",
            properties: json!({
                "sql": {"type": "string"},
                "params": params(),
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_PAGE, "default": DEFAULT_PAGE, "description": "Rows in this page"},
                "cursor": {"type": "string", "description": "`data.next_cursor` from the previous page of the same query"},
                "allow_invalid": {"type": "boolean", "default": false, "description": "Answer even though the database is invalid"}
            }),
            required: &["sql"],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "explain",
            title: "Explain a query",
            description: "How SQLite would run a statement, without running it.",
            properties: json!({"sql": {"type": "string"}, "params": params()}),
            required: &["sql"],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "check",
            title: "Check the database",
            description: "Validate everything, and list every violation with its file, line, column and JSON Pointer.",
            properties: json!({
                "strict": {"type": "boolean", "default": false, "description": "Count lint findings as failures"},
                "deep": {"type": "boolean", "default": false, "description": "Also verify all recorded history and the mirror"}
            }),
            required: &[],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "lint",
            title: "Lint the schemas",
            description: "How the schemas could say more about the data, each finding with the fix that applies it.",
            properties: json!({"table": table()}),
            required: &[],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "infer",
            title: "Infer schemas",
            description: "The schema reldir would infer for tables from their rows; writes nothing.",
            properties: json!({"tables": {"type": "array", "items": {"type": "string"}, "description": "Tables; all when omitted"}}),
            required: &[],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "doctor_plan",
            title: "Plan repairs",
            description: "Every repair doctor could make, least destructive first.",
            properties: json!({"only": {"type": "string", "description": "A FIX_ id or a problem code"}}),
            required: &[],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "doctor_explain",
            title: "Explain a repair",
            description: "What one of doctor's fixes does, and when it applies.",
            properties: json!({"fix": {"type": "string", "description": "A FIX_ id"}}),
            required: &["fix"],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "diff",
            title: "Diff revisions",
            description: "What changed since the last recorded revision, or between two revisions.",
            properties: json!({"table": table(), "from": revision(), "to": revision(), "git": {"type": "string", "description": "A git revision -- commit, branch or tag -- to compare the files with"}, "schema_only": {"type": "boolean", "default": false}}),
            required: &[],
            rules: json!({"dependentRequired": {"from": ["to"], "to": ["from"]}, "not": {"anyOf": [{"required": ["git", "from"]}, {"required": ["git", "to"]}]}}),
            effect: read,
        },
        Spec {
            name: "log",
            title: "Revision log",
            description: "Recorded revisions, newest first.",
            properties: json!({"limit": {"type": "integer", "minimum": 1}}),
            required: &[],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "show_revision",
            title: "Show a revision",
            description: "One revision: when, how it came about, and every row and schema it changed.",
            properties: json!({"revision": revision()}),
            required: &["revision"],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "snapshot_list",
            title: "List snapshots",
            description: "The named snapshots.",
            properties: json!({}),
            required: &[],
            rules: json!({}),
            effect: read,
        },
        Spec {
            name: "mutate",
            title: "Change rows with SQL",
            description: "Plan, then commit, one INSERT, UPDATE or DELETE. Referential actions apply, and a change that would leave any row invalid, or write what a table's contract never admitted, is refused.",
            properties: json!({"sql": {"type": "string"}, "params": params()}),
            required: &["sql"],
            rules: json!({}),
            effect: removes,
        },
        Spec {
            name: "transaction",
            title: "Run a transaction",
            description: "Plan, then commit, several SQL statements as one change: each sees what the ones before it did, and the state they make together is judged once, at the end.",
            properties: json!({"statements": {"type": "array", "minItems": 1, "items": {"type": "string"}, "description": "The statements, in order, one per item"}}),
            required: &["statements"],
            rules: json!({}),
            effect: removes,
        },
        Spec {
            name: "insert_row",
            title: "Insert rows",
            description: "Plan, then commit, the insertion of one row, or an array of rows.",
            properties: json!({"table": table(), "row": {"type": ["object", "array"], "description": "A row, or an array of rows"}}),
            required: &["table", "row"],
            rules: json!({}),
            effect: adds,
        },
        Spec {
            name: "update_row",
            title: "Update a row",
            description: "Plan, then commit, setting columns of one row found by its key.",
            properties: json!({"table": table(), "key": key(), "patch": {"type": "object", "description": "The columns to set"}}),
            required: &["table", "key", "patch"],
            rules: json!({}),
            effect: removes,
        },
        Spec {
            name: "delete_row",
            title: "Delete a row",
            description: "Plan, then commit, the deletion of one row; rows that reference it follow their onDelete actions, which the plan lists.",
            properties: json!({"table": table(), "key": key()}),
            required: &["table", "key"],
            rules: json!({}),
            effect: removes,
        },
        Spec {
            name: "import",
            title: "Import rows",
            description: "Plan, then commit, inserting every row of a JSON array, JSONL or CSV file on the server's machine, as one change.",
            properties: json!({
                "table": table(),
                "path": {"type": "string", "description": "The file: .json, .jsonl or .csv"},
                "format": {"enum": ["json", "jsonl", "csv"], "description": "The file's format, when its extension does not say"},
                "mode": {"enum": ["insert", "upsert", "replace"], "default": "insert", "description": "insert: every row new; upsert: rows with a key the table holds set what they state; replace: the table becomes exactly these rows"}
            }),
            required: &["table", "path"],
            rules: json!({}),
            effect: adds,
        },
        Spec {
            name: "apply_fixes",
            title: "Apply repairs",
            description: "Plan, then commit, doctor's repairs: those it applies by default, or the one `only` names. Repairs that rewrite rows need allow_data.",
            properties: json!({"only": {"type": "string", "description": "A FIX_ id or problem code"}, "allow_data": {"type": "boolean", "default": false}}),
            required: &[],
            rules: json!({}),
            effect: removes,
        },
        Spec {
            name: "migrate",
            title: "Migrate schemas",
            description: "Plan, then commit, a migration: operations that change schemas and rewrite rows to match, as one change. `reldir docs schemas` lists the operations.",
            properties: json!({"operations": {"type": "array", "minItems": 1, "items": {"type": "object"}, "description": "Operations, each with an `op`"}}),
            required: &["operations"],
            rules: json!({}),
            effect: removes,
        },
        Spec {
            name: "schema_new",
            title: "Declare a table",
            description: "Plan, then commit, a new table with a string key `id`, declared by a schema in schema/ to edit.",
            properties: json!({"table": table()}),
            required: &["table"],
            rules: json!({}),
            effect: adds,
        },
        Spec {
            name: "schema_pin",
            title: "Pin a schema",
            description: "Plan, then commit, making a table's inferred schema its declaration in schema/.",
            properties: json!({"table": table()}),
            required: &["table"],
            rules: json!({}),
            effect: adds,
        },
        Spec {
            name: "schema_widen",
            title: "Widen a contract",
            description: "Plan, then commit, widening a table's contract: to admit what its files hold, or at one member (`users.nick`, `posts.tags[]`) as stated.",
            properties: json!({
                "target": {"type": "string", "description": "A table, or a member of one"},
                "null": {"type": "boolean", "default": false},
                "optional": {"type": "boolean", "default": false},
                "types": {"type": "array", "items": {"enum": ["null", "boolean", "integer", "number", "string", "array", "object"]}},
                "any": {"type": "boolean", "default": false}
            }),
            required: &["target"],
            rules: json!({}),
            effect: adds,
        },
        Spec {
            name: "snapshot_restore",
            title: "Restore a snapshot",
            description: "Plan, then commit, making the database what a snapshot holds, as one validated change.",
            properties: json!({"name": {"type": "string"}}),
            required: &["name"],
            rules: json!({}),
            effect: removes,
        },
        Spec {
            name: "restore_rows",
            title: "Restore rows from history",
            description: "Plan, then commit, writing row files back byte for byte as history recorded them -- a deleted row, or a row as it was before a change -- as one validated change. Each file comes from `at`, or else from the newest revision that holds it.",
            properties: json!({
                "paths": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "Row files as show_revision and diff name them: <table>/<file>.json"},
                "at": {"type": "string", "description": "A revision: its number (12 or #12) or the beginning of its id"}
            }),
            required: &["paths"],
            rules: json!({}),
            effect: removes,
        },
        Spec {
            name: "snapshot_create",
            title: "Create a snapshot",
            description: "Keep the database as it is now under a name, to restore later.",
            properties: json!({"name": {"type": "string"}}),
            required: &["name"],
            rules: json!({}),
            effect: Effect::Direct,
        },
        Spec {
            name: "export",
            title: "Export a table",
            description: "Write a table's rows -- filtered and ordered as asked -- to a new JSON, JSON lines, CSV or Parquet file on the server's machine. An existing file is never replaced.",
            properties: json!({
                "table": table(),
                "path": {"type": "string"},
                "format": {"enum": ["json", "jsonl", "csv", "parquet"], "description": "By default, the path's extension"},
                "where": {"type": "string", "description": "A SQL condition"},
                "order": {"type": "string", "description": "A column, or -column for descending"}
            }),
            required: &["table", "path"],
            rules: json!({}),
            effect: Effect::Direct,
        },
    ]
}

pub fn spec(name: &str) -> Option<Spec> {
    specs().into_iter().find(|spec| spec.name == name)
}

/// What a call runs.
pub enum Action {
    Command(Command),
    /// A page of a query.
    Page {
        sql: String,
        params: Vec<SqlParam>,
        window: Window,
        /// The state the first page was read from, when this continues it.
        expect_state: Option<String>,
        /// Names the query, so a cursor cannot continue another.
        fingerprint: String,
    },
    Script(Vec<String>),
    Migration(Migration),
}

fn string(arguments: &Map<String, Value>, name: &str) -> Result<String> {
    match arguments.get(name) {
        Some(Value::String(text)) => Ok(text.clone()),
        _ => Err(DbError::usage(format!("{name} is a required string"))),
    }
}

fn optional_string(arguments: &Map<String, Value>, name: &str) -> Option<String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .map(String::from)
}

fn flag(arguments: &Map<String, Value>, name: &str) -> bool {
    arguments
        .get(name)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn count(arguments: &Map<String, Value>, name: &str) -> Option<usize> {
    arguments
        .get(name)
        .and_then(Value::as_u64)
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
}

/// A key as the row commands take it: text as itself, anything else as JSON.
fn row_key(arguments: &Map<String, Value>) -> String {
    match arguments.get("key") {
        Some(Value::String(text)) => text.clone(),
        Some(other) => reldir_core::canonical::compact(other),
        None => unreachable!("the input schema requires a key"),
    }
}

fn revision_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// The `?` values, as the CLI's `--param` takes them.
fn param_texts(arguments: &Map<String, Value>) -> Vec<String> {
    arguments
        .get("params")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(reldir_core::canonical::compact).collect())
        .unwrap_or_default()
}

/// A statement that is exactly one statement.
fn one_statement(text: &str) -> Result<String> {
    let statements = reldir_core::sql::split_statements(text);
    match statements.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(DbError::usage("the statement is empty")),
        _ => Err(DbError::usage(
            "this takes one statement; the transaction tool runs several as one change",
        )),
    }
}

/// A query's fingerprint: its text and its values.
fn fingerprint(sql: &str, params: &[String]) -> String {
    let mut text = sql.to_string();
    for param in params {
        text.push('\0');
        text.push_str(param);
    }
    Hash256::of(text.as_bytes()).to_hex()[..16].to_string()
}

/// A cursor: where the next page starts, in which state, of which query.
pub fn cursor(offset: usize, state: &str, fingerprint: &str) -> String {
    format!("c1.{offset}.{state}.{fingerprint}")
}

fn read_cursor(text: &str, expected_fingerprint: &str) -> Result<(usize, String)> {
    let malformed = || {
        DbError::usage("the cursor is not one this server gave; pass `data.next_cursor` as it came")
    };
    let mut parts = text.split('.');
    let (Some("c1"), Some(offset), Some(state), Some(fingerprint), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(malformed());
    };
    let offset: usize = offset.parse().map_err(|_| malformed())?;
    if fingerprint != expected_fingerprint {
        return Err(DbError::usage(
            "the cursor continues another query: pass it with the statement and values of the \
             page it came from",
        ));
    }
    Ok((offset, state.to_string()))
}

/// The command a tool call runs, and the context it runs in.
pub fn action(name: &str, arguments: &Map<String, Value>, context: &mut Context) -> Result<Action> {
    let command = match name {
        "status" => Command::Status,
        "tables" => Command::Tables,
        "describe" => Command::Describe {
            table: string(arguments, "table")?,
        },
        "schema_show" => Command::Schema(SchemaCommand::Show {
            table: string(arguments, "table")?,
        }),
        "get_row" => Command::Get {
            table: string(arguments, "table")?,
            key: row_key(arguments),
        },
        "list_rows" => Command::List {
            table: string(arguments, "table")?,
            filter: optional_string(arguments, "where"),
            order: optional_string(arguments, "order"),
            limit: Some(count(arguments, "limit").unwrap_or(DEFAULT_PAGE)),
        },
        "query" => {
            context.allow_invalid = flag(arguments, "allow_invalid");
            let sql = one_statement(&string(arguments, "sql")?)?;
            let texts = param_texts(arguments);
            let fingerprint = fingerprint(&sql, &texts);
            let (skip, expect_state) = match optional_string(arguments, "cursor") {
                Some(cursor) => {
                    let (offset, state) = read_cursor(&cursor, &fingerprint)?;
                    (offset, Some(state))
                }
                None => (0, None),
            };
            return Ok(Action::Page {
                params: parse_params(&texts)?,
                sql,
                window: Window {
                    skip,
                    take: Some(count(arguments, "limit").unwrap_or(DEFAULT_PAGE)),
                },
                expect_state,
                fingerprint,
            });
        }
        "explain" => Command::Explain {
            statement: one_statement(&string(arguments, "sql")?)?,
            params: param_texts(arguments),
        },
        "check" => Command::Check {
            strict: flag(arguments, "strict"),
            deep: flag(arguments, "deep"),
        },
        "lint" => Command::Lint {
            table: optional_string(arguments, "table"),
            strict: false,
            descriptions: false,
        },
        "infer" => {
            let tables: Vec<String> = arguments
                .get("tables")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            Command::Infer {
                all: tables.is_empty(),
                tables,
                write: false,
                tighten: String::new(),
                pk: vec![],
                on_schema_conflict: "compare".into(),
            }
        }
        "doctor_plan" => Command::Doctor {
            fix: false,
            allow_data: true,
            only: optional_string(arguments, "only"),
            explain: None,
            no_snapshot: true,
        },
        "doctor_explain" => Command::Doctor {
            fix: false,
            allow_data: false,
            only: None,
            explain: Some(string(arguments, "fix")?),
            no_snapshot: true,
        },
        "diff" => Command::Diff {
            table: optional_string(arguments, "table"),
            from: arguments.get("from").and_then(revision_text),
            to: arguments.get("to").and_then(revision_text),
            git: optional_string(arguments, "git"),
            schema: flag(arguments, "schema_only"),
        },
        "log" => Command::Log {
            limit: count(arguments, "limit"),
        },
        "show_revision" => Command::Show {
            revision: arguments
                .get("revision")
                .and_then(revision_text)
                .ok_or_else(|| DbError::usage("revision is a number or an id prefix"))?,
        },
        "snapshot_list" => Command::Snapshot(SnapshotCommand::List),
        "mutate" => {
            let sql = one_statement(&string(arguments, "sql")?)?;
            if reldir_core::sql::classify(&sql)? != reldir_core::sql::StatementKind::Mutation {
                return Err(DbError::usage(
                    "mutate runs INSERT, UPDATE and DELETE; query reads",
                ));
            }
            Command::Sql {
                statement: Some(sql),
                file: None,
                params: param_texts(arguments),
            }
        }
        "transaction" => {
            let statements = arguments
                .get("statements")
                .and_then(Value::as_array)
                .ok_or_else(|| DbError::usage("statements is an array of statements"))?
                .iter()
                .map(|item| {
                    item.as_str()
                        .ok_or_else(|| DbError::usage("each statement is a string"))
                        .and_then(one_statement)
                })
                .collect::<Result<Vec<_>>>()?;
            if let Some(control) = statements
                .iter()
                .find(|statement| reldir_core::sql::control(statement).is_some())
            {
                return Err(DbError::new(
                    Code::TRANSACTION_STATEMENT,
                    format!(
                        "{control:?} is transaction control; the tool's statements are the \
                         transaction, and committing its plan commits it"
                    ),
                ));
            }
            return Ok(Action::Script(statements));
        }
        "insert_row" => Command::Insert {
            table: string(arguments, "table")?,
            row: arguments.get("row").map(Value::to_string),
            from: None,
        },
        "update_row" => Command::Update {
            table: string(arguments, "table")?,
            key: row_key(arguments),
            patch: arguments
                .get("patch")
                .map(Value::to_string)
                .unwrap_or_default(),
        },
        "delete_row" => Command::Delete {
            table: string(arguments, "table")?,
            key: row_key(arguments),
        },
        "import" => Command::Import {
            table: string(arguments, "table")?,
            from: string(arguments, "path")?,
            input_format: optional_string(arguments, "format"),
            mode: optional_string(arguments, "mode").unwrap_or_else(|| "insert".into()),
        },
        "apply_fixes" => {
            context.allow_destructive = false;
            Command::Doctor {
                fix: true,
                allow_data: flag(arguments, "allow_data"),
                only: optional_string(arguments, "only"),
                explain: None,
                no_snapshot: false,
            }
        }
        "migrate" => {
            let document =
                json!({"operations": arguments.get("operations").cloned().unwrap_or(Value::Null)});
            let migration: Migration = serde_json::from_value(document)
                .map_err(|error| DbError::usage(format!("not a migration: {error}")))?;
            return Ok(Action::Migration(migration));
        }
        "schema_new" => Command::Schema(SchemaCommand::New {
            table: string(arguments, "table")?,
        }),
        "schema_pin" => Command::Schema(SchemaCommand::Pin {
            table: string(arguments, "table")?,
        }),
        "schema_widen" => Command::Schema(SchemaCommand::Widen {
            target: string(arguments, "target")?,
            null: flag(arguments, "null"),
            optional: flag(arguments, "optional"),
            types: arguments
                .get("types")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
            any: flag(arguments, "any"),
        }),
        "snapshot_restore" => Command::Snapshot(SnapshotCommand::Restore {
            name: string(arguments, "name")?,
        }),
        "restore_rows" => Command::Restore {
            paths: arguments
                .get("paths")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect(),
            at: optional_string(arguments, "at"),
        },
        "snapshot_create" => Command::Snapshot(SnapshotCommand::Create {
            name: string(arguments, "name")?,
        }),
        "export" => {
            let path = std::path::PathBuf::from(string(arguments, "path")?);
            if std::fs::symlink_metadata(&path).is_ok() {
                return Err(DbError::usage(format!(
                    "{} exists; export writes a new file and never replaces one",
                    path.display()
                )));
            }
            Command::Export {
                table: string(arguments, "table")?,
                out: Some(path),
                file_format: optional_string(arguments, "format"),
                filter: optional_string(arguments, "where"),
                order: optional_string(arguments, "order"),
            }
        }
        other => return Err(DbError::usage(format!("there is no tool {other:?}"))),
    };
    Ok(Action::Command(command))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test2290_every_tool_has_a_closed_valid_input_schema_and_a_command() {
        let mut names = std::collections::BTreeSet::new();
        for spec in specs() {
            assert!(names.insert(spec.name), "{} is listed twice", spec.name);
            let schema = spec.input_schema();
            assert!(
                jsonschema::meta::is_valid(&schema),
                "{}: {schema:#}",
                spec.name
            );
            assert!(jsonschema::validator_for(&schema).is_ok(), "{}", spec.name);
            assert_eq!(schema["additionalProperties"], false, "{}", spec.name);
            for required in spec.required {
                assert!(
                    schema["properties"].get(*required).is_some(),
                    "{} requires {required}",
                    spec.name
                );
            }
            // A tool that cannot be mapped to a command fails here, not when
            // an agent calls it.
            let mut arguments = Map::new();
            for required in spec.required {
                let value = match *required {
                    "row" | "patch" => json!({}),
                    "statements" => json!(["SELECT 1"]),
                    "operations" => json!([{"op": "drop_table", "table": "t"}]),
                    "revision" => json!(1),
                    "sql" if spec.name == "mutate" => json!("DELETE FROM t"),
                    "sql" => json!("SELECT 1"),
                    "path" if spec.name == "export" => json!("/nonexistent/reldir-export.json"),
                    _ => json!("t"),
                };
                arguments.insert(required.to_string(), value);
            }
            let mut context = Context::default();
            if let Err(error) = action(spec.name, &arguments, &mut context) {
                panic!(
                    "{} maps to no command: {}",
                    spec.name, error.diagnostic.message
                );
            }
        }
    }

    #[test]
    fn test2291_a_cursor_continues_only_its_own_query() {
        let first = fingerprint("SELECT * FROM t", &["1".into()]);
        let other = fingerprint("SELECT * FROM t", &["2".into()]);
        assert_ne!(first, other);
        let text = cursor(200, "abc", &first);
        assert_eq!(
            read_cursor(&text, &first).unwrap(),
            (200, "abc".to_string())
        );
        assert!(read_cursor(&text, &other).is_err());
        assert!(read_cursor("c1.x.abc.def", &first).is_err());
        assert!(read_cursor("garbage", &first).is_err());
    }
}
