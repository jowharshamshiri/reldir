---
title: CLI reference
---

# CLI reference

## Conventions

**Invocation.** `reldir <command>` runs a command. `reldir '<sql>'` runs one
query, and a bare `reldir` opens the shell — both over the current directory, so
a folder of JSON files is queryable without being set up first: prerequisites
are established as they are needed, and `--no-auto` reports what is missing
instead.

**Database discovery.** Without `--db <path>`, the binary walks up from the current
directory looking for `.db/`, like `git`. `DB_DIR` overrides. If nothing is found,
the error names every directory tried.

**Output format.** `--format table|json|jsonl|csv`; `--json` is an alias for
`--format json`. Table output is the default on a terminal, `jsonl` when
redirected. Every JSON record carries a `"kind"` field, which makes the machine
output a stable contract.

Two commands emit a document rather than records, so they ignore `--format` and
carry no `"kind"`: `reldir completions` writes a shell script, and `reldir schema
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
[Configuration]({{ site.baseurl }}/configuration).

## Setting up

### `reldir init`

```sh
reldir init ./data                      # empty database
reldir init ./data --adopt              # infer schemas for existing table directories
reldir init ./data --adopt --dry-run
reldir init ./data --track-provenance   # keep provenance in version control
```

Creates `.db/`, `.db/format`, `.db/config`, `.db/.gitignore`, and `.db/schema/`. It does not create `schema/`: that is the pin directory, made when you first pin a schema.
Adoption detects every top-level directory containing `.json` files, infers a
schema for those lacking one, validates the result, and records the first
revision with origin `import`. It reports every directory it skipped and why. If
anything fails, the whole command fails and writes nothing.

Running `init` where `.db/` already exists fails with `ALREADY_INITIALIZED`.

### `reldir inspect`

```sh
reldir inspect            # describe a directory, initialised or not
```

Useful before adopting, and on read-only or damaged directories.

## Looking at data

```sh
reldir tables                     # list tables with row counts
reldir describe users             # print a table's schema
reldir get users u1               # one row by primary key
reldir list users --where "name = 'Bob'" --order name --limit 10
```

The primary key argument is decoded against its declared type: `reldir get things 123`
finds the row whose key is the string `"123"` when the column is a `string`, and
the number `123` when it is an `int`.

## Changing data

```sh
reldir insert users '{"id":"u3","name":"Carol"}'
reldir insert users --from rows.json      # one object or an array; - for stdin
reldir update users u3 '{"name":"Caroline"}'
reldir delete users u3
```

`update` takes a patch object: only the keys you supply change.

## SQL

```sh
reldir sql 'SELECT * FROM users LIMIT 10'
reldir sql 'SELECT * FROM users WHERE id = ?' --param '"u1"'
reldir sql 'SELECT * FROM users WHERE id = :id' --param id='"u1"'
reldir sql --explain 'SELECT ...'
reldir sql --explain-analyze 'SELECT ...'
reldir explain 'SELECT ...'
reldir shell                              # interactive REPL
```

See [SQL]({{ site.baseurl }}/sql) for the supported subset.

The shell answers `.tables`, `.describe <table>`, `.status`, and `.quit`/`.exit`
in addition to SQL.

## Schemas

```sh
reldir infer                    # print proposed schemas for tables lacking one
reldir infer users              # one table
reldir infer users --write      # write .db/schema/users.json
reldir infer --all --write      # re-infer every unpinned table
reldir infer --strictness strict|balanced|loose
reldir infer users --pk id      # choose or compose the primary key: --pk a,b

reldir schema show users
reldir schema new users         # scaffold a minimal valid schema
reldir schema pin users         # declare the working schema in schema/
reldir schema restore users     # rebuild the working schema from its pin
reldir schema validate
reldir schema dialect           # print the JSON Schema dialect reldir accepts
```

`infer` never writes without `--write` and never overwrites an existing schema.

Schemas are [JSON Schema 2020-12]({{ site.baseurl }}/schemas) documents, so everything these
commands print or write is one. `reldir schema dialect` emits the dialect itself,
which is what an editor needs to complete a schema you write by hand.

## Validation and repair

```sh
reldir status                   # validity, revision, external changes
reldir check                    # full validation
reldir --readonly check         # validate and mutate no data
reldir check --strict           # lint findings also fail
reldir lint                     # how schemas could be stronger
reldir lint --strict            # exit non-zero on any finding
reldir lint --descriptions      # include missing-description suggestions
reldir doctor                   # diagnose and print a fix plan
reldir doctor --fix             # apply derived, schema, and layout fixes
reldir doctor --fix --allow-data
reldir doctor --only <CODE|FIX_ID>
reldir doctor --explain <FIX_ID>
reldir doctor --no-snapshot
```

See [Validation]({{ site.baseurl }}/validation) for the workflow.

## History

```sh
reldir diff                     # working state vs the last recorded revision
reldir diff 1 2                 # between two revisions
reldir diff users               # one table
reldir diff --schema            # schema changes only
reldir log                      # revision history
reldir show 2                   # one revision
```

Diff is semantic: it reports field changes with old and new typed values, row
additions and removals, key changes, and renames that preserve identity.

A schema change is reported the same way: columns added, removed, changed, or
reordered, and changes to the primary key, constraints, indexes, and the rest,
rather than a textual difference between two files.

## Import and export

```sh
reldir export users                          # to stdout in the selected format
reldir export users --format csv
reldir export users --format sqlite --out users.db
reldir import users --from rows.jsonl        # .jsonl or .csv
```

Import is transactional: invalid input is rejected as a whole, leaving nothing
partially applied. The `sqlite` export encoding requires `--out`.

## Maintenance

```sh
reldir reindex                  # rebuild indexes
reldir analyze                  # rebuild query statistics
reldir recover                  # resolve interrupted transactions
reldir gc                       # reclaim unneeded internal state
reldir gc --dry-run
reldir upgrade-format
reldir completions bash|zsh|fish|powershell
```

Indexes and statistics are derived state: deleting them is always safe.

## Snapshots

```sh
reldir snapshot create before-import
reldir snapshot list
reldir snapshot restore before-import
reldir snapshot delete before-import
```

Restoration is transactional and records provenance with origin
`snapshot_restore`.

## Migrations

```sh
reldir migrate add-table t --from schema.json
reldir migrate drop-table t
reldir migrate rename-table t new
reldir migrate add-column t c --type bool --default true [--nullable]
reldir migrate drop-column t c
reldir migrate rename-column t c new
reldir migrate change-type t c int [--using <sql-expr>]
reldir migrate add-constraint / drop-constraint
reldir migrate add-index / drop-index
reldir migrate apply migration.json
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
