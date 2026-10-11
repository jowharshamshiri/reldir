---
title: Machine protocol
---

# Machine protocol

Every command can answer in a form a program reads: `--format json` prints one
result document, `--format jsonl` prints a stream of lines. Both are protocol
1, described exactly by the JSON Schema at the end of this page, which
`reldir protocol schema` prints and the test suite holds every command's output
to.

`--format` defaults to `table` on a terminal and `jsonl` otherwise, so a script
that pipes reldir's output gets the protocol without asking. `RELDIR_FORMAT`
sets the default for a whole environment, and `--json` is `--format json`.

## The result

A command ends with exactly one `result`. With `--format json` it is the whole
output, and carries everything the command produced:

```console
$ reldir --format json sql 'SELECT id, name FROM users ORDER BY id'
{
  "type": "result",
  "protocol": 1,
  "reldir": "1.0.0",
  "command": "sql",
  "ok": true,
  "exit": 0,
  "summary": "2 row(s)",
  "data": {
    "rows": 2,
    "database_valid": true
  },
  "records": [
    {
      "type": "row",
      "row": {
        "id": "ada",
        "name": "Ada"
      }
    },
    {
      "type": "row",
      "row": {
        "id": "grace",
        "name": "Grace"
      }
    }
  ],
  "diagnostics": [],
  "events": []
}
```

| Member | Meaning |
|---|---|
| `type` | always `"result"` |
| `protocol` | the protocol version, `1` |
| `reldir` | the version of the binary that printed it |
| `command` | the command that ran: `sql`, `check`, `insert`, ... |
| `ok` | whether it succeeded: `exit` is `0` |
| `exit` | the process's exit status; see [Errors]({{ site.baseurl }}/errors) |
| `summary` | one line for a person |
| `data` | what the command reports beside its records (below) |
| `error` | on failure, the diagnostic that stopped the command |
| `records` | rows and typed records, in the order produced |
| `diagnostics` | faults, findings and notices; on failure, every fault behind `error` |
| `events` | what the command did on its own |

## Lines

With `--format jsonl` every record, diagnostic and event is printed as its own
line the moment it is produced, and the result comes last, without the
`records`, `diagnostics` and `events` arrays the lines before it already
carried. A program can act on rows while a long query is still running.

```console
$ reldir --format jsonl sql 'SELECT 1 AS a; SELECT 2 AS b'
{"type":"row","row":{"a":1}}
{"type":"statement","index":1,"statement":"SELECT 1 AS a","rows":1}
{"type":"row","row":{"b":2}}
{"type":"statement","index":2,"statement":"SELECT 2 AS b","rows":1}
{"type":"result","protocol":1,"reldir":"1.0.0","command":"sql","ok":true,"exit":0,"summary":"2 statement(s), 2 row(s)","data":{"statements":2,"rows":2,"database_valid":true}}
```

A failure prints its diagnostics, then the result with `error`:

```console
$ reldir --format jsonl get users nobody
{"type":"diagnostic","severity":"error","code":"UNKNOWN_ROW","message":"users has no row with key nobody"}
{"type":"result","protocol":1,"reldir":"1.0.0","command":"get","ok":false,"exit":4,"summary":"users has no row with key nobody","data":{},"error":{"type":"diagnostic","severity":"error","code":"UNKNOWN_ROW","message":"users has no row with key nobody"}}
```

A reader that stops reading -- `reldir ... | head` -- stops only the printing:
a change the command was making is still made, and the exit status is the
command's own.

## What a line is

Every object says what it is in `type`.

**Rows.** A row of a table is nested under `row`: `{"type":"row","row":{...}}`.
A column may be called anything, `type` included, and is never mistaken for a
member of the protocol. The row is exactly what the database holds: absent
members are absent, numbers keep their spelling, and integers of any size are
exact.

**Typed records.** Records reldir defines name their kind in `type`, with the
kind's own members beside it:

| `type` | Printed by |
|---|---|
| `table` | `tables` |
| `table_description` | `describe` |
| `schema`, `dialect`, `protocol` | `schema show`, `schema dialect`, `protocol schema` (the document under `document`) |
| `change` | every change: `action` (`write`, `delete`), `path`, `planned` |
| `statement` | each statement of a script, after its rows: `index`, `statement`, `rows`, and `changed` for a statement that changes rows |
| `plan_step` | `explain` |
| `revision`, `revision_change` | `log`, `show` |
| `restored` | `restore`: each file's `path`, and the `revision` it comes from |
| `added`, `removed`, `renamed`, `value_change` | `diff`; each says in `change` what happened: a row `added`, `removed` or `renamed`, or a value `added`, `removed` or `changed` |
| `unrecorded_change` | `status`, for outside edits not yet recorded |
| `fix`, `fix_explanation` | `doctor` |
| `inferred_table` | `infer` |
| `operation` | `migrate` |
| `snapshot` | `snapshot create` and `snapshot list` |
| `upgrade` | `upgrade` |
| `git_setup` | `git setup` |
| `monitor` | `serve` |
| `doc`, `doc_topic` | `docs` |

**Events.** Something a command did on its own -- recorded an outside edit,
recovered an interrupted transaction, ran a referential action -- is an event:
`{"type":"event","event":"<kind>", ...}`. The kinds are `state_transition`,
`recorded`, `recovered`, `mirror_rebuilt`, `referential_action`,
`contract_widened`, `kept_outside_edit`, `superseded_removed`, `new_lineage`
and `snapshot`.

```console
$ reldir --format jsonl --dry-run sql "DELETE FROM tags WHERE id = 'history'"
{"type":"event","event":"referential_action","path":"posts/p1.json","action":"remove","constraint":"posts.tags[] -> tags","key":"[\"history\"]","because":"tags/history.json"}
{"type":"change","action":"delete","path":"tags/history.json","planned":true}
{"type":"change","action":"write","path":"posts/p1.json","planned":true}
{"type":"result","protocol":1,"reldir":"1.0.0","command":"sql","ok":true,"exit":0,"summary":"dry run: 2 file(s) would change; nothing was written","data":{"files":2,"dry_run":true,"replayed":false,"state":"[..]"}}
```

**Diagnostics.** A fault, finding or notice has `type: "diagnostic"`,
`severity` (`error`, `warning`, `suggestion`, `info`), a registered `code` and a
`message`, and -- where they apply -- `table`, `path`, `location` (`line`,
`column`), `pointer` (an RFC 6901 JSON Pointer into the file), `source_line`,
`field`, `constraint`, `expected`, `observed`, `fixes` (the doctor fixes that
apply) and `help`.

## What `data` holds

| Commands | Members |
|---|---|
| reads: `sql` (a query), `list`, `export` | `rows`; `database_valid` when the answer came from the database |
| a script of several statements | `statements`, and `rows` for a script of reads |
| `get` | `path`, `database_valid` |
| changes: `insert`, `update`, `delete`, `sql`, `import`, `migrate`, `doctor --fix`, `schema widen`, `snapshot restore`, `restore`, ... | `files`, `dry_run`, `replayed` (an idempotency key named a change already made), `revision` (`seq`, `id`) when one was recorded, `state` (the state the change was planned against; see `--expect-state` in the [CLI reference]({{ site.baseurl }}/cli)) |
| a script that ended in `ROLLBACK` | `statements`, `rolled_back` |
| `import` | also `read`, `inserted`, `updated`, `deleted`, `unchanged` |
| `export --out` | `rows`, `path` |
| `status` | `valid`, `state`, `tables`, `rows`, `violations` (a count), `monitored`, `revision`, `state_identity` |
| `check` | `valid`, `state`, `tables`, `rows`, `violations` and `warnings` (counts by code), `lint` (counts by code), `elapsed_ms` |
| `doctor` | `valid`, `fixes`, `applicable`, `manual` |

## The other formats

- `table` renders records as aligned tables on stdout, one table per kind of
  record (and per statement of a script), diagnostics compiler-style on stderr
  with the source line and a caret, prose on stderr, and the summary last.
- `csv` prints records only, streamed; a query's columns are the header. Rows
  of different shapes cannot be one CSV table, and are refused rather than
  misaligned.
- `sarif` prints diagnostics as a SARIF 2.1.0 log, for `check`, `lint` and
  `doctor`; each rule's `helpUri` is the code's entry in the
  [error reference]({{ site.baseurl }}/errors).

```console
$ reldir --format csv sql 'SELECT id, name FROM users ORDER BY id'
id,name
ada,Ada
grace,Grace
```

## Stability

The protocol's version is in every result. Within protocol 1, a member never
changes meaning and is never removed; new members, record kinds, event kinds
and codes may be added, so a reader ignores what it does not know. A change
that would break a reader is a new protocol version.

Codes are a stable contract too: each keeps its meaning and its exit status.
The [error reference]({{ site.baseurl }}/errors) is generated from the same
registry the binary uses.

Arguments refused before a command runs -- a misspelt flag, a missing value --
are reported in the protocol as well: with a machine format, a result with
`USAGE` and exit `1`. A defect in reldir itself is a result with
`INTERNAL_ERROR` and exit `70`.

## The schema

`reldir protocol schema` prints it; a program can validate what it reads, or
generate types from it. The result document is `#`, one line of
`--format jsonl` is `#/$defs/line`.

<!-- generated:schema -->
```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://reldir.dev/protocol/1",
  "title": "reldir machine protocol 1",
  "description": "One `result` document (--format json), or a stream of lines each one of `line` (--format jsonl).",
  "$ref": "#/$defs/result",
  "$defs": {
    "line": {
      "description": "One line of --format jsonl.",
      "oneOf": [
        {
          "$ref": "#/$defs/row"
        },
        {
          "$ref": "#/$defs/record"
        },
        {
          "$ref": "#/$defs/event"
        },
        {
          "$ref": "#/$defs/diagnostic"
        },
        {
          "$ref": "#/$defs/result"
        }
      ]
    },
    "row": {
      "type": "object",
      "required": [
        "type",
        "row"
      ],
      "additionalProperties": false,
      "properties": {
        "type": {
          "const": "row"
        },
        "row": {
          "type": "object",
          "description": "The row, exactly as the database holds it."
        }
      }
    },
    "record": {
      "type": "object",
      "required": [
        "type"
      ],
      "properties": {
        "type": {
          "enum": [
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
            "snapshot",
            "statement",
            "table",
            "table_description",
            "unrecorded_change",
            "upgrade",
            "value_change"
          ]
        }
      }
    },
    "event": {
      "type": "object",
      "required": [
        "type",
        "event"
      ],
      "properties": {
        "type": {
          "const": "event"
        },
        "event": {
          "enum": [
            "contract_widened",
            "kept_outside_edit",
            "mirror_rebuilt",
            "new_lineage",
            "recorded",
            "recovered",
            "referential_action",
            "snapshot",
            "state_transition",
            "superseded_removed"
          ]
        }
      }
    },
    "location": {
      "type": "object",
      "required": [
        "line",
        "column"
      ],
      "additionalProperties": false,
      "properties": {
        "line": {
          "type": "integer",
          "minimum": 1
        },
        "column": {
          "type": "integer",
          "minimum": 1
        }
      }
    },
    "diagnostic": {
      "type": "object",
      "required": [
        "type",
        "severity",
        "code",
        "message"
      ],
      "additionalProperties": false,
      "properties": {
        "type": {
          "const": "diagnostic"
        },
        "severity": {
          "enum": [
            "error",
            "warning",
            "suggestion",
            "info"
          ]
        },
        "code": {
          "enum": [
            "PATH_NOT_FOUND",
            "PATH_NOT_DIRECTORY",
            "ROOT_AMBIGUOUS",
            "ROOT_JSON_AMBIGUOUS",
            "UNINITIALIZED",
            "ALREADY_INITIALIZED",
            "FORMAT_UNSUPPORTED",
            "FORMAT_MISSING",
            "FORMAT_UPGRADE_REQUIRED",
            "CONFIG_INVALID",
            "UNSAFE_FILESYSTEM",
            "READ_ONLY",
            "USAGE",
            "INPUT_INVALID",
            "IO_ERROR",
            "UNEXPECTED_FILE",
            "UNGOVERNED_DIRECTORY",
            "NON_REGULAR_FILE",
            "PATH_COLLISION",
            "PORTABILITY_COLLISION",
            "PATH_VIOLATION",
            "FILENAME_TOO_LONG",
            "FILENAME_RULE_UNDETERMINED",
            "CROSS_DEVICE",
            "SCHEMA_INVALID_JSON",
            "SCHEMA_MISSING_REQUIRED",
            "SCHEMA_UNKNOWN_KEY",
            "SCHEMA_UNKNOWN_FORMAT",
            "SCHEMA_GRAMMAR",
            "SCHEMA_REF_EXTERNAL",
            "SCHEMA_TABLE_NAME_MISMATCH",
            "SCHEMA_INVALID_TABLE_NAME",
            "SCHEMA_TYPE_UNKNOWN",
            "SCHEMA_TYPE_TAG_CONFLICT",
            "SCHEMA_COLUMN_UNKNOWN",
            "SCHEMA_COLUMN_EXISTS",
            "SCHEMA_TABLE_EXISTS",
            "SCHEMA_CONSTRAINT_EXISTS",
            "SCHEMA_IDENTITY_INVALID",
            "SCHEMA_DEFAULT_TYPE_MISMATCH",
            "SCHEMA_CONSTRAINT_NAME_DUPLICATE",
            "SCHEMA_REFERENCE_PATH_INVALID",
            "SCHEMA_FK_TARGET_MISSING",
            "SCHEMA_FK_TARGET_INVALID",
            "SCHEMA_FK_TARGET_NOT_UNIQUE",
            "SCHEMA_TARGET_KEYS_OVERLAP",
            "SCHEMA_FK_TYPE_MISMATCH",
            "SCHEMA_FK_ACTION_INVALID",
            "SCHEMA_KEY_NOT_SCALAR",
            "SCHEMA_DOMAIN_KEY_INVALID",
            "SCHEMA_ACYCLIC_INVALID",
            "SCHEMA_CHECK_INVALID",
            "SCHEMA_ASSERTION_INVALID",
            "SCHEMA_CONFLICT",
            "CONTRACT_WIDENING_REQUIRED",
            "CONTRACT_EXCEEDED",
            "CONTRACT_REDERIVED",
            "INVALID_JSON",
            "MERGE_CONFLICT",
            "ROW_ROOT_NOT_OBJECT",
            "SCHEMA_VIOLATION",
            "TYPE_MISMATCH",
            "ROW_MISSING_FIELD",
            "ROW_UNKNOWN_FIELD",
            "NOT_NULL_VIOLATION",
            "IDENTITY_MISMATCH",
            "KEY_COLLISION",
            "FILE_TOO_LARGE",
            "NESTING_TOO_DEEP",
            "PRIMARY_KEY_VIOLATION",
            "UNIQUE_VIOLATION",
            "DOMAIN_KEY_VIOLATION",
            "FOREIGN_KEY_VIOLATION",
            "CYCLE_VIOLATION",
            "CHECK_VIOLATION",
            "ASSERTION_VIOLATION",
            "MUTATION_CONFLICT",
            "MIGRATION_REFUSED",
            "RESOURCE_LIMIT",
            "DECISION_REQUIRED",
            "QUERY_UNSUPPORTED",
            "QUERY_TYPE_ERROR",
            "UNKNOWN_TABLE",
            "UNKNOWN_COLUMN",
            "UNKNOWN_ROW",
            "UNKNOWN_CONSTRAINT",
            "UNKNOWN_REVISION",
            "UNKNOWN_SNAPSHOT",
            "UNKNOWN_RESOURCE",
            "CURSOR_STALE",
            "LOCK_CONTENDED",
            "CONCURRENT_MODIFICATION",
            "PATH_INTERFERENCE",
            "PLAN_CHANGED",
            "TRANSACTION_INCOMPLETE",
            "RECOVERY_BLOCKED",
            "COMMITTED_RECOVERY_PENDING",
            "COMMIT_UNKNOWN",
            "TRANSACTION_EXPIRED",
            "TRANSACTION_STATEMENT",
            "SNAPSHOT_READ",
            "INTERNAL_METADATA_CORRUPT",
            "HISTORY_DIVERGED",
            "SNAPSHOT_EXISTS",
            "INFER_NO_ROWS",
            "INFER_INVALID_JSON",
            "INFER_ROOT_NOT_OBJECT",
            "INFER_NON_JSON_FILE",
            "INFER_NESTED_DIRECTORY",
            "INFER_NON_REGULAR_FILE",
            "INFER_KEY_INVALID",
            "INFER_FILENAME_INCONSISTENT",
            "LINT_SCHEMA_UNPINNED",
            "LINT_NULLABLE_NEVER_NULL",
            "LINT_WIDER_TYPE",
            "LINT_ENUM_CANDIDATE",
            "LINT_UNIQUE_CANDIDATE",
            "LINT_CHECK_CANDIDATE",
            "LINT_MIN_ITEMS_CANDIDATE",
            "LINT_FK_CANDIDATE",
            "LINT_DOMAIN_CANDIDATE",
            "LINT_FK_ACTION_DEFAULTED",
            "LINT_PK_NOT_GENERATED",
            "LINT_COLUMN_NEVER_POPULATED",
            "LINT_INCONSISTENT_PRESENCE",
            "LINT_ADDITIONAL_FIELDS_ALLOWED",
            "LINT_NON_CANONICAL_FORMATTING",
            "LINT_NO_DESCRIPTION",
            "INTERNAL_ERROR"
          ]
        },
        "message": {
          "type": "string"
        },
        "table": {
          "type": "string"
        },
        "path": {
          "type": "string"
        },
        "location": {
          "$ref": "#/$defs/location"
        },
        "pointer": {
          "type": "string"
        },
        "source_line": {
          "type": "string"
        },
        "field": {
          "type": "string"
        },
        "constraint": {
          "type": "string"
        },
        "expected": {
          "type": "string"
        },
        "observed": {
          "type": "string"
        },
        "fixes": {
          "type": "array",
          "items": {
            "type": "string"
          }
        },
        "help": {
          "type": "string"
        }
      }
    },
    "result": {
      "type": "object",
      "required": [
        "type",
        "protocol",
        "reldir",
        "command",
        "ok",
        "exit",
        "summary",
        "data"
      ],
      "additionalProperties": false,
      "properties": {
        "type": {
          "const": "result"
        },
        "protocol": {
          "const": 1
        },
        "reldir": {
          "type": "string"
        },
        "command": {
          "type": "string"
        },
        "ok": {
          "type": "boolean"
        },
        "exit": {
          "enum": [
            0,
            1,
            2,
            3,
            4,
            5,
            6,
            7,
            8,
            9,
            10,
            11,
            12,
            13,
            70
          ]
        },
        "summary": {
          "type": "string"
        },
        "data": {
          "type": "object",
          "description": "What the command reports beside its records: counts, the revision, validity."
        },
        "error": {
          "$ref": "#/$defs/diagnostic"
        },
        "records": {
          "type": "array",
          "items": {
            "anyOf": [
              {
                "$ref": "#/$defs/row"
              },
              {
                "$ref": "#/$defs/record"
              }
            ]
          }
        },
        "diagnostics": {
          "type": "array",
          "items": {
            "$ref": "#/$defs/diagnostic"
          }
        },
        "events": {
          "type": "array",
          "items": {
            "$ref": "#/$defs/event"
          }
        }
      }
    }
  }
}
```
<!-- end:schema -->
