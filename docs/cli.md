---
title: CLI reference
---

# CLI reference

`reldir --help` and `reldir <command> --help` describe every command and every
argument, each with an example; this page is the map. Every command answers in
the [machine protocol]({{ site.baseurl }}/protocol) as well as for a person,
and ends with an exit status from the [error reference]({{ site.baseurl }}/errors).

With no subcommand, `reldir 'SELECT ...'` runs one statement, and bare `reldir`
on a terminal opens the [shell](#shell). Without a terminal, bare `reldir` is a
usage error rather than a wait on stdin; `reldir sql -` reads a statement from
a pipe.

## Finding the database

- `--db <DIR>` names the root exactly. It must exist and be a directory
  (`PATH_NOT_FOUND`, `PATH_NOT_DIRECTORY`): a mistyped path is an error, never
  an empty database that reports itself valid. `RELDIR_DB` does the same from
  the environment.
- Otherwise reldir walks up from the working directory to the nearest `.db/`,
  as `git` does, so every command works from inside any table directory.
- A folder of pins with no `.db/` inside another database could be a table of
  it or a database of its own; reldir refuses to guess (`ROOT_AMBIGUOUS`).
- With neither, the working directory is the candidate.

A folder with no `.db/` is adopted by the first command that needs a database:
its pins are read, a schema is inferred for every table nobody pinned, and the
first revision records what was there. No row file is changed by adoption.
`--readonly` answers from the files without writing anything, `--no-auto`
refuses (`UNINITIALIZED`, exit `10`), and `--dry-run` reports what adopting
would do. JSON files at the root belong to no table, so adoption refuses them
(`ROOT_JSON_AMBIGUOUS`) until they are moved. Once a database exists, a new
directory beside its tables is reported (`UNGOVERNED_DIRECTORY`), not adopted;
`reldir infer <dir> --write` governs it.

## Global flags

| Flag | Effect |
|---|---|
| `--db <DIR>` | the root, exactly; also `RELDIR_DB` |
| `--format <F>` | `table`, `json`, `jsonl`, `csv` or `sarif`; `table` on a terminal and `jsonl` otherwise; also `RELDIR_FORMAT` |
| `--json` | `--format json` |
| `--quiet`, `-q` | no informational prose; results, diagnostics and exit statuses are unchanged |
| `--verbose`, `-v` | progress from the first file, not after a second |
| `--no-color` | no colour; `NO_COLOR` in the environment does the same |
| `--readonly` | write nothing at all -- not rows, not history, not derived state; read a private copy of the mirror, and see a change that is committed but not yet finished on disk as made; a command that would change something is `READ_ONLY` |
| `--dry-run` | plan and validate every change, show it, write nothing |
| `--yes`, `-y` | confirm in advance; without a terminal to ask on, the only way to confirm |
| `--no-auto` | never establish a database implicitly |
| `--allow-invalid` | answer read-only queries from an invalid database, reporting its faults first |
| `--allow-destructive` | permit replacing a pin by re-inference, or beginning a new history lineage |
| `--rebuild-metadata` | permit discarding an unlabelled `.db/` that holds history |
| `--idempotency-key <KEY>` | name the change, so a retry is reported (`replayed`) rather than made twice |
| `--widen` | widen a table's contract, in the same change, to admit what the change writes |
| `--expect-state <STATE>` | commit only if the database still holds the state a dry run reported as `data.state`; otherwise `PLAN_CHANGED` |
| `--wait <S>` | seconds to wait for another writer's lock; `0` tries once; fractions allowed |
| `--timeout <S>` | seconds a query may run; fractions allowed |
| `--max-json-file-size`, `--max-nesting-depth`, `--max-query-memory`, `--max-result-rows`, `--max-transaction-size` | override a [limit]({{ site.baseurl }}/configuration) for one command |

A dry run reports the state it planned against as `data.state`. A change
given that state with `--expect-state` commits only if the database still
holds it, and so makes exactly the change the dry run showed. See
[How to retry a change without applying it twice]({{ site.baseurl }}/retry-safely).

## Looking

```sh
reldir status                 # validity, unrecorded changes, the revision
reldir tables                 # tables, row counts, keys, pins
reldir describe <table>       # columns, key, references in and out
reldir get <table> <key>      # one row; a composite key is a JSON array
reldir list <table> [--where COND] [--order [-]COLUMN] [--limit N]
reldir inspect [DIR] [--ignore PATTERN]...  # what reldir would make of a folder; writes nothing
```

```console
$ reldir status
VALID   revision #1 ([..])
$ reldir tables
table | rows | primary_key | pinned
------+------+-------------+-------
posts | 2    | ["id"]      | true
tags  | 3    | ["id"]      | true
users | 2    | ["id"]      | true
(3 rows)
3 table(s)
$ reldir get users ada
id  | name
----+-----
ada | Ada
(1 row)
$ reldir list posts --order -id
id | user_id | title     | tags
---+---------+-----------+-----------------------
p2 | grace   | Compilers | ["languages"]
p1 | ada     | Engines   | ["machines","history"]
(2 rows)
2 row(s)
```

`status` reports one of these states:

| State | Meaning |
|---|---|
| `VALID` | the files are a valid database, and history holds this state |
| `VALID_CHANGED_EXTERNALLY` | valid and different from history: recorded now, as a revision with origin `external` |
| `VALID_UNRECORDED` | valid and different from history, observed by a reader that records nothing |
| `INVALID` | one or more faults, each with its file, line, column and JSON Pointer |
| `EMPTY` | no tables, no schemas: nothing to govern |

An invalid database still answers `status`, `check`, `lint`, `doctor`, `log`,
`diff`, `recover` and `snapshot restore`. Queries refuse unless given
`--allow-invalid`; changes always refuse.

`status` also reports `monitored` in its data: whether the folder monitor
answered the observation (see [serve](#serve)).

## Changing rows

```sh
reldir insert <table> '<json object or array>'    # or --from FILE, or --from -
reldir update <table> <key> '<json object>'       # sets the columns given
reldir delete <table> <key>
```

Every change is one validated transaction: it is planned, judged as the state
it would produce, and written only if that state is valid and the change
writes nothing a table's contract never admitted (`CONTRACT_WIDENING_REQUIRED`
otherwise; see [the observed contract]({{ site.baseurl }}/concepts#the-observed-contract)). An insert fills the
generated columns it leaves out and reports each row as written.

```console
$ reldir --dry-run insert users '{"id":"alan","name":"Alan"}'
id   | name
-----+-----
alan | Alan
(1 row)

action | path            | planned
-------+-----------------+--------
write  | users/alan.json | true
(1 row)
dry run: 1 file(s) would change; nothing was written
$ reldir insert users '{"id":"alan","name":"Alan"}'
id   | name
-----+-----
alan | Alan
(1 row)

action | path            | planned
-------+-----------------+--------
write  | users/alan.json | false
(1 row)
inserted: 1 file(s), revision #2 ([..])
$ reldir update users alan '{"name":"Alan Turing"}'
id   | name
-----+------------
alan | Alan Turing
(1 row)

action | path            | planned
-------+-----------------+--------
write  | users/alan.json | false
(1 row)
updated: 1 file(s), revision #3 ([..])
$ reldir delete users ada
error[FOREIGN_KEY_VIOLATION]: deleting users/ada.json refused: posts/p1.json still references it at /user_id (posts.user_id -> users, onDelete restrict)
  --> posts/p1.json:3:14
   |
 3 |   "user_id": "ada",
   |              ^
   = at: /user_id
   = constraint: posts.user_id -> users
   = observed: ["ada"]
   = help: change or remove the references first, or declare an action that resolves them
```

A refused change writes nothing and exits `2`.

## SQL

```sh
reldir sql '<statement>' [--param VALUE ...]
reldir sql '<statement>; <statement>; ...'        # a script: one transaction
reldir sql -f script.sql
echo 'SELECT count(*) FROM users' | reldir sql -
reldir explain '<statement>'                       # SQLite's plan, without running it
```

A parameter is JSON: `--param '"Ada"'` for the next `?`, `--param 'name="Ada"'`
for `:name`. Several statements separated by `;` -- on the command line, in a
file, or on stdin -- are a script, run as one transaction: each statement sees
what the ones before it did, and the state they make together is judged once,
at the end. A script may be framed by `BEGIN` and `COMMIT`; ending it with
`ROLLBACK` runs it and writes nothing. Transaction control anywhere else is
`TRANSACTION_STATEMENT`. A script's statements carry their own values;
`--param` binds one statement's. See [SQL]({{ site.baseurl }}/sql) for the
dialect and [Transactions]({{ site.baseurl }}/transactions) for what a commit
does.

```console
$ reldir sql "UPDATE posts SET title = 'Analytical Engines' WHERE id = 'p1'; SELECT title FROM posts WHERE id = 'p1'"
title
------------------
Analytical Engines
(1 row)

action | path          | planned
-------+---------------+--------
write  | posts/p1.json | false
(1 row)
committed: 1 file(s), revision #4 ([..])
```

## Bulk rows

```sh
reldir import <table> --from FILE [--input-format json|jsonl|csv] [--mode insert|upsert|replace]
reldir export <table> [--out FILE] [--as json|jsonl|csv|parquet] [--where COND] [--order [-]COLUMN]
```

`import` reads a JSON array (or one object), JSON lines, or CSV with a header
naming columns, a row at a time, from a file or `--from -` (stdin, which needs
`--input-format`). Every row is one change. The mode says how the rows meet the
table's:

- `insert`: every row is new; a key the table holds is refused.
- `upsert`: a row whose key the table holds sets the members it states; any
  other row is inserted.
- `replace`: the table becomes exactly the imported rows; a row the import
  does not name is deleted, with its referrers' `onDelete` actions.

`export` writes a table's rows with every value exactly as their files hold
it, ordered by key or `--order`, filtered by `--where`. In JSON and JSON lines a
row's members are in its file's order, with every number spelled as the file
spells it, and a key the table takes from file names follows them; CSV and
Parquet columns are in the schema's order. With `--out` it writes a new file in the
format `--as` names or the extension says -- JSON, JSON lines, CSV or Parquet
-- which appears whole or not at all and never replaces a file that exists.
Without `--out`, the rows are the command's records, in the `--format` asked.
See [How to import and export rows in bulk]({{ site.baseurl }}/import-and-export).

```console
$ echo '{"id":"turing","name":"Alan Turing"}' > people.jsonl
$ reldir import users --from people.jsonl --mode upsert
action | path              | planned
-------+-------------------+--------
write  | users/turing.json | false
(1 row)
imported: 1 file(s), revision #5 ([..]); 1 row(s) read: 1 inserted, 0 updated, 0 deleted, 0 unchanged
$ reldir export users --out users.csv
4 row(s) to users.csv
$ cat users.csv
id,name
ada,Ada
alan,Alan Turing
grace,Grace
turing,Alan Turing
$ reldir export users --out users.csv
error[USAGE]: users.csv exists; export writes a new file and never replaces one
```

Parquet columns follow the schema: `int` as INT64, `float` and `number` as
DOUBLE (exactly, or the export is refused), `bool` as BOOLEAN, everything else
as UTF-8 text -- integers of any size and decimals as their digits, arrays and
objects as their JSON.

## Validating and repairing

```sh
reldir check [--strict] [--deep]   # every fault; exit 2 when invalid
reldir lint [TABLE] [--strict] [--descriptions]
reldir doctor [--fix] [--allow-data] [--only FIX_ID|CODE] [--explain FIX_ID] [--no-snapshot]
```

`check --strict` also fails (exit `7`) on warnings and lint findings;
`--deep` also verifies every object history holds and the mirror's own
consistency. `doctor` lists every repair it could make, least destructive
first; `--fix` applies the default repair for each problem, `--allow-data`
permits repairs that rewrite rows, and `--only` names one fix or problem. Fixes
that tighten or widen a contract apply only when named with `--only`. See
[Validation]({{ site.baseurl }}/validation) and
[How to repair an invalid database]({{ site.baseurl }}/repair-a-database).

```console
$ reldir check
VALID: 3 table(s), 9 row(s), 0 violation(s), 0 warning(s), 1 lint finding(s), [..] ms
```

## Schemas and inference

```sh
reldir schema show <table>          # the schema document
reldir schema new <table>           # declare a table with a string key `id`, as a pin
reldir schema pin <table>           # make an inferred schema the table's declaration
reldir schema widen <target> [--null] [--optional] [--type T ...] [--any]
reldir schema validate              # faults in the schemas alone
reldir schema dialect               # the dialect's meta-schema, for editors
reldir infer [TABLE ...] [--all] [--write] [--tighten WHAT] [--pk COLUMNS] [--on-schema-conflict ask|fail|compare|reinfer]
```

`schema widen users` admits what the table's files hold; `schema widen
users.nick --null --optional` widens one member; `posts.tags[]` names an
array's items. `infer` reports what it would infer and writes it with
`--write`; `--tighten enums,unique,checks,min-items` (or `all`) also states
what the data merely happens to satisfy. See [Schemas]({{ site.baseurl }}/schemas),
[How to declare a new table]({{ site.baseurl }}/declare-a-table) and
[How to write a value a table's contract does not admit]({{ site.baseurl }}/widen-a-contract).

## Migrate

```sh
reldir migrate add-table <table> --from FILE
reldir migrate drop-table <table>
reldir migrate rename-table <table> <new>
reldir migrate add-column <table> <column> --type T [--nullable] [--default JSON]
reldir migrate drop-column <table> <column>
reldir migrate rename-column <table> <column> <new>
reldir migrate change-type <table> <column> <type> [--using EXPR] [--values a,b] [--drop-constraint KEYWORD ...]
reldir migrate change-identity <table> '<identity JSON>'
reldir migrate add-constraint <table> '<constraint JSON>'
reldir migrate drop-constraint <table> <name>
reldir migrate add-index <table> <columns>
reldir migrate drop-index <table> <columns>
reldir migrate set-domain <table> [domain]
reldir migrate apply <file>         # several operations from a migration file, all or nothing
```

Each reshapes schemas and rewrites rows to match, as one change. See
[Migrations]({{ site.baseurl }}/migrations) and
[How to change the shape of a table that holds rows]({{ site.baseurl }}/change-a-schema).

## History

```sh
reldir diff [TABLE] [--schema]                # since the last revision
reldir diff --from REV --to REV               # between two revisions
reldir diff --git REV                         # the files against a git commit, branch or tag
reldir log [--limit N]
reldir show <REV>                             # `12`, `#12`, or a prefix of the id
reldir restore <PATH>... [--at REV]           # row files back as history recorded them
reldir snapshot create|list|restore|delete [NAME]
reldir recover [--history new-lineage]
reldir gc
reldir upgrade [--new-lineage]                # convert a format 2 database
```

Every change is recorded as a revision; outside edits that leave the database
valid are recorded by the next command that writes. `recover` finishes
interrupted transactions, and with `--history new-lineage` (and
`--allow-destructive`) moves unverifiable history aside -- its heads, and every
pack that fails verification -- and begins again from the files. `restore`
writes row files back byte for byte, each from the revision `--at` names or
from the newest revision that holds it, as one change judged like any other; a
path no revision holds is `UNKNOWN_ROW`. `gc` removes history objects that no revision, quarantined lineage
or snapshot reaches. `upgrade` converts a format 2 database, keeping format 2's
metadata in `.db/format-2/`. See [History and snapshots]({{ site.baseurl }}/snapshots),
[How to undo a change]({{ site.baseurl }}/undo-a-change),
[How to recover]({{ site.baseurl }}/recover) and [Format]({{ site.baseurl }}/format).

## Git

```sh
reldir git setup [--hooks]
reldir git merge-file BASE OURS THEIRS [--path P] [--labels B O T] [--marker-size N]
reldir git textconv FILE
```

`setup` routes the tables' row files through reldir's merge and diff drivers;
git runs the other two. See [Git integration]({{ site.baseurl }}/git) and
[How to merge and diff row files with git]({{ site.baseurl }}/merge-with-git).

## Serve

```sh
reldir --db ./data serve
```

`serve` watches the folder and keeps a log of the paths that change. While it
runs, every process that opens the database -- the CLI, the MCP server, the
Python and Node libraries -- reads only the paths that changed since it last
looked, instead of examining every file. Without it, every command examines
the folder whole; nothing else differs. It runs until Ctrl-C or `SIGTERM`.
See [How to keep a large folder fast]({{ site.baseurl }}/keep-a-large-folder-fast),
[Performance]({{ site.baseurl }}/performance) and
[Security]({{ site.baseurl }}/security).

## Shell

`reldir shell`, or bare `reldir` on a terminal, reads SQL a statement at a time:
a statement runs when it ends in `;` outside any string or comment, so it can
span lines. Tab completes tables, columns, keywords and dot-commands, and
history is kept in `$XDG_STATE_HOME/reldir/history`. Each statement is its own
transaction until `BEGIN;` or `.begin` opens one that holds every statement
after it -- the prompt becomes `reldir*>` -- until `COMMIT;`/`.commit` writes them
as one change or `ROLLBACK;`/`.rollback` discards them. A statement that fails
inside a transaction rolls it all back; leaving the shell rolls back an open one.

| Dot-command | Does |
|---|---|
| `.help` | list the dot-commands |
| `.tables` | the tables |
| `.describe TABLE` | a table's columns, key and references |
| `.schema TABLE` | a table's schema document |
| `.status` | validity and revision |
| `.check` | every fault |
| `.log` | recent revisions |
| `.begin`, `.commit`, `.rollback` | open, commit or discard a transaction |
| `.param VALUE ...` | values for the `?` of the statements that follow; `.param` lists them, `.param clear` forgets them |
| `.explain on\|off` | show SQLite's plan for each statement instead of running it |
| `.timer on\|off` | say how long each statement took |
| `.format FORMAT` | `table`, `json`, `jsonl` or `csv` |
| `.quit` | leave (also `.exit`, or Ctrl-D) |

## The rest

```sh
reldir init [DIR] [--adopt] [--track-provenance] [--ignore PATTERN]...   # make a folder a database
reldir analyze                                      # refresh the query planner's statistics
reldir mcp                                          # serve the database over MCP on stdio
reldir docs [TOPIC]                                 # this documentation; needs no database
reldir protocol schema                              # the machine protocol's JSON Schema
reldir completions bash|zsh|fish|elvish|powershell
```

`init` on a folder that already holds data needs `--adopt`; `--ignore` adds a
glob to the `ignore` list the new `.db/config` holds, so adoption and every
command after it leave out what it matches, and `inspect` takes the same
patterns; `--track-provenance` keeps history under version control with the rows (see
[Git integration]({{ site.baseurl }}/git#what-git-versions)). See
[MCP]({{ site.baseurl }}/mcp) for `mcp` and
[Machine protocol]({{ site.baseurl }}/protocol) for `protocol schema`.

## Output and exit statuses

`table` is for people: records as aligned tables on stdout, diagnostics
compiler-style on stderr with the source line and a caret, then a summary.
`json`, `jsonl`, `csv` and `sarif` are described in the
[machine protocol]({{ site.baseurl }}/protocol). Every code has one exit status;
the table of statuses and codes is the [error reference]({{ site.baseurl }}/errors).
