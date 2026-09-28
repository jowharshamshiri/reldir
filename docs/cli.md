---
title: CLI reference
---

# CLI reference

`reldir --help` and `reldir <command> --help` describe every command and every
argument; this page is the map.

## Finding the database

- `--db <DIR>` names the root exactly. It must exist and be a directory
  (`PATH_NOT_FOUND`, `PATH_NOT_DIRECTORY`): a mistyped path is an error, never
  an empty database that reports itself valid. `RELDIR_DB` does the same from
  the environment.
- Otherwise reldir walks up from the working directory to the nearest `.db/`,
  like `git`, so every command works from inside any table directory.
- A folder of pins with no `.db/` inside another database could be a table of
  that database or a database of its own; reldir refuses to guess
  (`ROOT_AMBIGUOUS`) and asks for `--db`.
- With neither, the working directory is the candidate.

## Establishing a database on first use

A folder with no `.db/` is established by the first command that needs a
database: its pins are read, a schema is inferred for every table nobody
pinned, and the first revision records what was there (origin `import`). The
command reports what it did, and then runs. If inference fails for any table,
nothing is written.

- `--readonly` never writes: the schemas are inferred in memory and the
  question answered from the files as they are.
- `--no-auto` establishes nothing and reports `UNINITIALIZED` (exit `10`).
- `--dry-run` reports what establishing would do, and answers from memory.
- JSON files at the root belong to no table, so establishing refuses
  (`ROOT_JSON_AMBIGUOUS`) until they are moved.
- A `.db/` with no format marker that holds nothing irreplaceable is rebuilt; one
  that holds history is refused (`FORMAT_MISSING`) unless `--rebuild-metadata`
  says to discard it.

Once a database exists, a new directory beside it is not adopted on its own:
it is reported as `UNGOVERNED_DIRECTORY`, and `reldir infer <dir> --write`
governs it.

## Global flags

| Flag | Effect |
|---|---|
| `--db <DIR>` | the root, exactly (env `RELDIR_DB`) |
| `--readonly` | write nothing at all -- not rows, not history, not derived state |
| `--format <F>` | `table`, `json`, `jsonl`, `csv` or `sarif`; `table` on a terminal, `jsonl` otherwise |
| `--json` | `--format json` |
| `--dry-run` | plan and validate every change, show it, write nothing |
| `--yes`, `-y` | confirm in advance; without a terminal to ask on, the only way to confirm |
| `--no-auto` | never establish a database implicitly |
| `--allow-invalid` | answer read-only queries from an invalid database |
| `--allow-destructive` | permit replacing a pin by re-inference, or beginning a new history lineage |
| `--rebuild-metadata` | permit discarding an unlabelled `.db/` that holds history |
| `--quiet`, `-q` | no informational prose; results, diagnostics and exit statuses unchanged |
| `--verbose`, `-v` | progress from the first file |
| `--no-color` | no colour (also `NO_COLOR`) |
| `--wait <S>` | seconds to wait for the writer lock; `0` tries once |
| `--timeout <S>` | seconds a query may run |
| `--max-json-file-size`, `--max-nesting-depth`, `--max-query-memory`, `--max-result-rows`, `--max-transaction-size` | override a [limit]({{ site.baseurl }}/configuration) for one command |

## Output

Every command produces the same things -- data records, diagnostics, events
(what it did on its own: a recovery, a recorded revision, a referential action)
-- and ends with a result. Each format renders them:

- `table`: records as aligned tables on stdout, one per kind of record;
  diagnostics compiler-style on stderr, with the source line and a caret; prose
  on stderr; then a one-line summary.
- `json`: one `command_result` envelope:

  ```json
  { "kind": "command_result", "command": "check", "ok": false, "exit": 2,
    "summary": "INVALID: ...", "valid": false, "records": [], "diagnostics": [ ... ],
    "events": [] }
  ```

  A failure carries `"error"`: the diagnostic that stopped the command, with
  any further faults behind it in `diagnostics`.
- `jsonl`: each record, diagnostic and event as its own line as it happens,
  then the `command_result` line without the arrays -- for streaming.
- `csv`: records only, streamed; a query's columns are the header.
- `sarif`: diagnostics as a SARIF 2.1.0 log, for `check`, `lint` and `doctor`.

A diagnostic always has `kind`, `severity`, `code` and `message`, and -- when
they apply -- `table`, `path`, `location` (`line`, `column`), `pointer` (RFC 6901),
`source_line`, `field`, `constraint`, `expected`, `observed`, `fixes` and
`help`. Codes and exit statuses are listed in [Errors]({{ site.baseurl }}/errors).

## Setting up and looking

```sh
reldir init [DIR] [--adopt] [--track-provenance]
reldir inspect [DIR]          # what reldir would make of a folder; writes nothing
reldir status                 # validity, unrecorded changes, the revision
reldir tables                 # tables, row counts, keys, pins, domains
reldir describe <table>       # columns, key, references in and out
reldir get <table> <key>      # one row; a composite key is a JSON array
reldir list <table> [--where COND] [--order [-]COLUMN] [--limit N]
```

`init` on a folder that already holds data needs `--adopt`, and
`--track-provenance` keeps history under version control too.

## Changing rows

```sh
reldir insert <table> '<json object or array>'    # or --from FILE, or --from -
reldir update <table> <key> '<json object>'       # sets the columns given
reldir delete <table> <key>
reldir import <table> --from FILE                  # .json, .jsonl/.ndjson or .csv
reldir export <table> [--out FILE]                 # JSON Lines, or CSV for a .csv name
```

An insert fills generated columns (`uuid`, `ulid`, `now`, `sequence`) it
omits. An import is one transaction: all of it lands, or none. `export` never
overwrites a file.

## SQL

```sh
reldir sql '<statement>' [--param V]...    # or reldir '<statement>', or reldir sql -
reldir explain '<statement>'
reldir shell
```

See [SQL]({{ site.baseurl }}/sql).

## Health

```sh
reldir check [--strict]
reldir lint [TABLE] [--strict] [--descriptions]
reldir doctor [--fix] [--allow-data] [--only FIX_ID|CODE] [--explain FIX_ID] [--no-snapshot]
```

See [Validation]({{ site.baseurl }}/validation).

## Schemas

```sh
reldir schema show <table>
reldir schema new <table>       # declare a table as a pin: a string key `id`, ready to edit
reldir schema pin <table>       # make an inferred schema the table's declaration
reldir schema validate          # schema faults alone, without judging rows
reldir schema dialect           # the dialect, for editors
reldir infer [TABLE]... [--all] [--write] [--pk COLS] [--strictness strict|balanced|loose]
             [--on-schema-conflict ask|fail|compare|reinfer]
```

`infer` prints without `--write`. When a table already has a schema that the
inferred one differs from, `--on-schema-conflict` decides: `ask` (the default)
asks on a terminal and otherwise stops with `DECISION_REQUIRED` (exit `9`);
`fail` stops with `SCHEMA_CONFLICT`; `compare` reports the differences and
writes nothing; `reinfer` replaces the schema -- and replacing a pin, which
someone wrote, also needs `--allow-destructive`.

## Migrate

Structural changes, each validated as the state it produces together with every
row it reshapes:

```sh
reldir migrate add-table <table> --from schema.json
reldir migrate drop-table <table>
reldir migrate rename-table <table> <new>
reldir migrate add-column <table> <column> --type TYPE [--nullable] [--default JSON]
reldir migrate drop-column <table> <column>
reldir migrate rename-column <table> <column> <new>
reldir migrate change-type <table> <column> <type> [--using SQL]
reldir migrate add-constraint <table> '<json>'
reldir migrate drop-constraint <table> <name>
reldir migrate add-index <table> <columns>
reldir migrate drop-index <table> <columns>
reldir migrate set-domain <table> [DOMAIN]
reldir migrate apply <file>
```

Renaming a table moves its directory and every reference to it; renaming a
column rewrites its rows, its constraints and every reference to it. A column
that admits no null needs a `--default` on a table with rows, and the default is
written into every existing row. `change-type` converts each value only when
nothing is lost, or computes it with the `--using` expression. A constraint is
written as the dialect writes it, tagged with its `kind`: `unique`,
`foreign_key`, `check`, `acyclic` or `assertion`.

Dropping a table or a column asks for confirmation. A pinned table's migration
edits its pin.

A migration file is a list run as one change -- the intermediate states need
not be valid, the final one must:

```json
{
  "operations": [
    { "op": "rename_column", "table": "users", "column": "name", "new": "display_name" },
    { "op": "add_column", "table": "users", "column": "active", "type": "bool", "default": true },
    { "op": "add_constraint", "table": "posts",
      "definition": { "kind": "foreign_key", "from": ["user_id"], "to": { "table": "users" }, "onDelete": "cascade" } }
  ]
}
```

## History and upkeep

```sh
reldir log [--limit N]
reldir show <revision>
reldir diff [TABLE] [--from A --to B] [--schema]    # since the last revision, or between two
reldir snapshot create|list|restore|delete <name>
reldir recover [--history new-lineage]
reldir gc
reldir analyze
reldir completions bash|zsh|fish|elvish|powershell
reldir mcp
```

`diff` reports each row added, removed or renamed, and each changed value by
its JSON Pointer, old and new. `recover --history new-lineage` moves history
that does not verify to `.db/provenance-quarantine/` and begins a new one from
the current files (it needs `--allow-destructive`). `gc` removes history objects
no revision references. `analyze` refreshes the query planner's statistics.
`reldir mcp` serves the database to AI agents; see [MCP]({{ site.baseurl }}/mcp).
