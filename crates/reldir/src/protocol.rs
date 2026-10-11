//! The machine protocol, as a JSON Schema: what `--format json` prints as one
//! document and `--format jsonl` as one line each. `reldir protocol schema`
//! prints it, the drivers are checked against it, and the behaviour suite
//! validates every command's output with it.
//!
//! Every object says what it is in `type`:
//!
//! - `row` -- a row of a table, nested in `row`, so no column name can be
//!   mistaken for a member of the protocol;
//! - a record kind reldir defines (`table`, `revision`, `fix`, ...) with that
//!   kind's members;
//! - `event` -- something a command did on its own, named in `event`;
//! - `diagnostic` -- a fault, finding or note, with its registered `code`;
//! - `result` -- how the command ended, always last, carrying `protocol` and
//!   the `reldir` version that printed it.

use crate::output::PROTOCOL;
use reldir_core::code::{Code, Exit};
use serde_json::{Value, json};

/// The kinds of record commands print, beside rows.
pub const RECORD_KINDS: &[&str] = &[
    "added",
    "change",
    "dialect",
    "doc",
    "doc_topic",
    "fix",
    "fix_explanation",
    "git_setup",
    "inferred_table",
    "monitor",
    "operation",
    "plan_step",
    "protocol",
    "removed",
    "renamed",
    "restored",
    "revision",
    "revision_change",
    "schema",
    "self_update",
    "snapshot",
    "statement",
    "table",
    "table_description",
    "unrecorded_change",
    "upgrade",
    "value_change",
];

/// The kinds of event commands report.
pub const EVENT_KINDS: &[&str] = &[
    "contract_widened",
    "kept_outside_edit",
    "mirror_rebuilt",
    "new_lineage",
    "recorded",
    "recovered",
    "referential_action",
    "snapshot",
    "state_transition",
    "superseded_removed",
];

/// The protocol's JSON Schema.
pub fn schema() -> Value {
    let codes: Vec<&str> = Code::ALL.iter().map(|code| code.as_str()).collect();
    let exits: Vec<i32> = Exit::ALL.iter().map(|exit| exit.code()).collect();
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("https://reldir.dev/protocol/{PROTOCOL}"),
        "title": format!("reldir machine protocol {PROTOCOL}"),
        "description": "One `result` document (--format json), or a stream of lines each one of `line` (--format jsonl).",
        "$ref": "#/$defs/result",
        "$defs": {
            "line": {
                "description": "One line of --format jsonl.",
                "oneOf": [
                    {"$ref": "#/$defs/row"},
                    {"$ref": "#/$defs/record"},
                    {"$ref": "#/$defs/event"},
                    {"$ref": "#/$defs/diagnostic"},
                    {"$ref": "#/$defs/result"}
                ]
            },
            "row": {
                "type": "object",
                "required": ["type", "row"],
                "additionalProperties": false,
                "properties": {
                    "type": {"const": "row"},
                    "row": {"type": "object", "description": "The row, exactly as the database holds it."}
                }
            },
            "record": {
                "type": "object",
                "required": ["type"],
                "properties": {
                    "type": {"enum": RECORD_KINDS}
                }
            },
            "event": {
                "type": "object",
                "required": ["type", "event"],
                "properties": {
                    "type": {"const": "event"},
                    "event": {"enum": EVENT_KINDS}
                }
            },
            "location": {
                "type": "object",
                "required": ["line", "column"],
                "additionalProperties": false,
                "properties": {
                    "line": {"type": "integer", "minimum": 1},
                    "column": {"type": "integer", "minimum": 1}
                }
            },
            "diagnostic": {
                "type": "object",
                "required": ["type", "severity", "code", "message"],
                "additionalProperties": false,
                "properties": {
                    "type": {"const": "diagnostic"},
                    "severity": {"enum": ["error", "warning", "suggestion", "info"]},
                    "code": {"enum": codes},
                    "message": {"type": "string"},
                    "table": {"type": "string"},
                    "path": {"type": "string"},
                    "location": {"$ref": "#/$defs/location"},
                    "pointer": {"type": "string"},
                    "source_line": {"type": "string"},
                    "field": {"type": "string"},
                    "constraint": {"type": "string"},
                    "expected": {"type": "string"},
                    "observed": {"type": "string"},
                    "fixes": {"type": "array", "items": {"type": "string"}},
                    "help": {"type": "string"}
                }
            },
            "result": {
                "type": "object",
                "required": ["type", "protocol", "reldir", "command", "ok", "exit", "summary", "data"],
                "additionalProperties": false,
                "properties": {
                    "type": {"const": "result"},
                    "protocol": {"const": PROTOCOL},
                    "reldir": {"type": "string"},
                    "command": {"type": "string"},
                    "ok": {"type": "boolean"},
                    "exit": {"enum": exits},
                    "summary": {"type": "string"},
                    "data": {"type": "object", "description": "What the command reports beside its records: counts, the revision, validity."},
                    "error": {"$ref": "#/$defs/diagnostic"},
                    "records": {"type": "array", "items": {"anyOf": [{"$ref": "#/$defs/row"}, {"$ref": "#/$defs/record"}]}},
                    "diagnostics": {"type": "array", "items": {"$ref": "#/$defs/diagnostic"}},
                    "events": {"type": "array", "items": {"$ref": "#/$defs/event"}}
                }
            }
        }
    })
}
