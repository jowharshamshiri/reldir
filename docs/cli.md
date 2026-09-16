---
title: CLI reference
---

# CLI reference

## Conventions

**Database discovery.** Without `--db <path>`, the binary walks up from the current
directory looking for `.db/`, like `git`. `DB_DIR` overrides. If nothing is found,
the error names every directory tried.

**Output format.** `--format table|json|jsonl|csv`; `--json` is an alias for
`--format json`. Table output is the default on a terminal, `jsonl` when
redirected. Every JSON record carries a `"kind"` field, which makes the machine
output a stable contract.

Two commands emit a document rather than records, so they ignore `--format` and
carry no `"kind"`: `db completions` writes a shell script, and `db schema
dialect` writes the JSON Schema dialect. Both are artifacts you save to a file,
and a wrapper key would corrupt them.

**Mutating commands** (`insert`, `update`, `delete`, `sql` with DML, `migrate`,
`doctor --fix`, `import`, `snapshot restore`, `schema pin`, `gc`) support
`--dry-run` and `--yes`, print the affected paths and the new revision on
success, and refuse to run while the database is `INVALID`. The exceptions are
`doctor`, `recover`, and `snapshot restore`, which are how you return to a valid
state.

**Diagnostics** use compiler style: a code, a one-line message, `--> path:line:col`
with a source excerpt and caret, then `= constraint`, `= expected`, `= observed`,
and `= help` with the command to run next. Colour appears on a terminal and is
disabled by `--no-color` or a non-empty `NO_COLOR`.

### Global flags

| Flag | Effect |
|---|---|
| `--db <PATH>` | database root (env: `DB_DIR`) |
| `--readonly` | never write the rows or the schemas you pinned; derived state is left as found |
| `--format <FMT>` | `table`, `json`, `jsonl`, `csv`, `sqlite` |
| `--json` | alias for `--format json` |
| `--quiet` | suppress informational output; diagnostics, results, and exit codes are unaffected |
| `--verbose` | report scan progress from the first file rather than after one second |
| `--no-color` | disable colour |
| `--yes` | accept confirmation prompts |
| `--dry-run` | show what would happen; write nothing |
| `--rebuild-metadata` | discard an unreadable `.db/` and rebuild it from your files |
| `--no-auto` | never establish prerequisites implicitly; report what would be needed instead of creating it |
| `--timeout <SECS>` | query timeout |

Resource limits may also be overridden per invocation. See
[Configuration](configuration).

## Setting up

### `db init`

```sh
db init ./data                      # empty database
db init ./data --adopt              # infer schemas for existing table directories
db init ./data --adopt --dry-run
db init ./data --track-provenance   # keep provenance in version control
```

Creates `.db/`, `.db/format`, `.db/config`, `.db/.gitignore`, and `.db/schema/`. It does not create `schema/`: that is the pin directory, made when you first pin a schema.
Adoption detects every top-level directory containing `.json` files, infers a
schema for those lacking one, validates the result, and records the first
revision with origin `import`. It reports every directory it skipped and why. If
anything fails, the whole command fails and writes nothing.

Running `init` where `.db/` already exists fails with `ALREADY_INITIALIZED`.

### `db inspect`

```sh
db inspect            # describe a directory, initialised or not
```

Useful before adopting, and on read-only or damaged directories.

## Looking at data

```sh
db tables                     # list tables with row counts
db describe users             # print a table's schema
db get users u1               # one row by primary key
db list users --where "name = 'Bob'" --order name --limit 10
```

The primary key argument is decoded against its declared type: `db get things 123`
finds the row whose key is the string `"123"` when the column is a `string`, and
the number `123` when it is an `int`.

## Changing data

```sh
db insert users '{"id":"u3","name":"Carol"}'
db insert users --from rows.json      # one object or an array; - for stdin
db update users u3 '{"name":"Caroline"}'
db delete users u3
```

`update` takes a patch object: only the keys you supply change.

## SQL

```sh
db sql 'SELECT * FROM users LIMIT 10'
db sql 'SELECT * FROM users WHERE id = ?' --param '"u1"'
db sql 'SELECT * FROM users WHERE id = :id' --param id='"u1"'
db sql --explain 'SELECT ...'
db sql --explain-analyze 'SELECT ...'
db explain 'SELECT ...'
db shell                              # interactive REPL
```

See [SQL](sql) for the supported subset.

The shell answers `.tables`, `.describe <table>`, `.status`, and `.quit`/`.exit`
in addition to SQL.

## Schemas

```sh
db infer                    # print proposed schemas for tables lacking one
db infer users              # one table
db infer users --write      # write .db/schema/users.json
db infer --all --write      # re-infer every unpinned table
db infer --strictness strict|balanced|loose
db infer users --pk id      # choose or compose the primary key: --pk a,b

db schema show users
db schema new users         # scaffold a minimal valid schema
db schema pin users         # declare the working schema in schema/
db schema restore users     # rebuild the working schema from its pin
db schema validate
db schema dialect           # print the JSON Schema dialect jdb accepts
```

`infer` never writes without `--write` and never overwrites an existing schema.

Schemas are [JSON Schema 2020-12](schemas) documents, so everything these
commands print or write is one. `db schema dialect` emits the dialect itself,
which is what an editor needs to complete a schema you write by hand.

## Validation and repair

```sh
db status                   # validity, revision, external changes
db check                    # full validation
db --readonly check         # validate and mutate no data
db check --strict           # lint findings also fail
db lint                     # how schemas could be stronger
db lint --strict            # exit non-zero on any finding
db lint --descriptions      # include missing-description suggestions
db doctor                   # diagnose and print a fix plan
db doctor --fix             # apply derived, schema, and layout fixes
db doctor --fix --allow-data
db doctor --only <CODE|FIX_ID>
db doctor --explain <FIX_ID>
db doctor --no-snapshot
```

See [Validation](validation) for the workflow.

## History

```sh
db diff                     # working state vs the last recorded revision
db diff 1 2                 # between two revisions
db diff users               # one table
db diff --schema            # schema changes only
db log                      # revision history
db show 2                   # one revision
```

Diff is semantic: it reports field changes with old and new typed values, row
additions and removals, key changes, and renames that preserve identity.

A schema change is reported the same way: columns added, removed, changed, or
reordered, and changes to the primary key, constraints, indexes, and the rest,
rather than a textual difference between two files.

## Import and export

```sh
db export users                          # to stdout in the selected format
db export users --format csv
db export users --format sqlite --out users.db
db import users --from rows.jsonl        # .jsonl or .csv
```

Import is transactional: invalid input is rejected as a whole, leaving nothing
partially applied. The `sqlite` export encoding requires `--out`.

## Maintenance

```sh
db reindex                  # rebuild indexes
db analyze                  # rebuild query statistics
db recover                  # resolve interrupted transactions
db gc                       # reclaim unneeded internal state
db gc --dry-run
db upgrade-format
db completions bash|zsh|fish|powershell
```

Indexes and statistics are derived state: deleting them is always safe.

## Snapshots

```sh
db snapshot create before-import
db snapshot list
db snapshot restore before-import
db snapshot delete before-import
```

Restoration is transactional and records provenance with origin
`snapshot_restore`.

## Migrations

```sh
db migrate add-table t --from schema.json
db migrate drop-table t
db migrate rename-table t new
db migrate add-column t c --type bool --default true [--nullable]
db migrate drop-column t c
db migrate rename-column t c new
db migrate change-type t c int [--using <sql-expr>]
db migrate add-constraint / drop-constraint
db migrate add-index / drop-index
db migrate apply migration.json
```

Migrations are transactional and support `--dry-run`, which reports how many row
files would be rewritten. A `change-type` whose data cannot be converted
losslessly fails and lists the offending rows, unless `--using` supplies an
explicit conversion.

`migrate apply` takes a declarative batch:

```json
{
  "operations": [
    { "op": "rename_column", "table": "users", "column": "name", "new": "display_name" },
    { "op": "add_column", "table": "users", "column": "active", "type": "bool", "default": true }
  ]
}
```

The whole array is evaluated against one prospective state and committed as a
single transaction. Intermediate states need not validate; the final one must.
