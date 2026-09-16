# jdb

**A relational database that lives in a directory of JSON files.**

`jdb` turns an ordinary folder into a validated relational database. Your rows stay
as human-readable JSON files that you can read, edit, `grep`, and commit to Git.
The `db` binary governs their interpretation: it enforces schemas, checks
referential integrity, answers SQL, and records how the state changed — without
ever taking ownership of your bytes.

📖 **[Documentation](https://jowharshamshiri.github.io/jdb/)**

```console
$ ls ./data
users/  posts/  comments/

$ db init ./data --adopt
Scanned 3 directories, 1204 JSON files.
VALID   revision 1   root 6e41f2…

$ db sql 'SELECT u.name, count(*) AS n
          FROM users u JOIN posts p ON p.user_id = u.id
          GROUP BY u.name ORDER BY n DESC LIMIT 3'
 name  | n
-------+---
 Alice | 41
 Bob   | 37
 Carol | 29
```

## Why

Most databases make you choose between *readable* and *relational*. A folder of
JSON files is transparent, diffable, and editable by any tool — but nothing stops
a typo from silently becoming invisible state. A real database enforces integrity
— but your data disappears into an opaque file.

`jdb` refuses the tradeoff. The filesystem stays authoritative and legible, and the
binary tells you, precisely, when it stops being a valid database:

```console
$ echo '{"id":"019...","user_id":"nobody","title":"x"}' > posts/broken.json
$ db status
INVALID   revision 1   (1 external change, 1 violation)

error[FOREIGN_KEY_VIOLATION]: posts.user_id references a row that does not exist
  --> posts/broken.json:1:24
   = constraint: posts.user_id -> users.id
   = help: run `db doctor` for fix options
```

Anything may edit the directory — you, your editor, a script, an agent, `git
merge`. `jdb` judges the state it observes at each operation boundary. Valid
external edits are adopted as legitimate new revisions; invalid ones are reported
with the file, line, and column.

## Install

```sh
cargo install --path . --locked
```

Requires a recent stable Rust toolchain (edition 2024). The result is a single
self-contained `db` executable — no daemon, no server, no runtime dependencies.

## Try it

```sh
db init ./data --adopt     # infer schemas from existing JSON and adopt them
db check                   # full validation
db lint                    # how the schemas could be stronger
db doctor                  # diagnose problems and propose fixes
db sql 'SELECT * FROM users LIMIT 10'
```

## What you get

- **JSON files as rows** — one object per file, in canonical, diff-friendly formatting
- **Schemas as JSON Schema** — 2020-12 documents in a declared dialect, maintained in `.db/schema/` and pinnable to `schema/*.json` for version control, with 14 column types, constraints, and checks
- **Schema inference** that bootstraps the strictest schema your data supports
- **SQL** — `SELECT`/`INSERT`/`UPDATE`/`DELETE`, joins, grouping, aggregates, `EXPLAIN`
- **Referential integrity** — primary keys, unique, foreign keys, `CHECK`, cascade actions
- **Transactional writes** with a crash-recoverable journal
- **Provenance** — every accepted state transition is recorded, including external edits
- **Compiler-style diagnostics** with stable error codes and exit codes
- **Git-native** — CI-friendly validation, stable formatting, no lockfile churn

## Documentation

| Guide | |
|---|---|
| [Getting started](https://jowharshamshiri.github.io/jdb/getting-started) | Adopt a directory and run your first queries |
| [Concepts](https://jowharshamshiri.github.io/jdb/concepts) | The model: validity, external edits, provenance |
| [Schemas](https://jowharshamshiri.github.io/jdb/schemas) | The JSON Schema dialect, writing one by hand, types, constraints |
| [CLI reference](https://jowharshamshiri.github.io/jdb/cli) | Every command and flag |
| [SQL](https://jowharshamshiri.github.io/jdb/sql) | The supported subset |
| [Validation](https://jowharshamshiri.github.io/jdb/validation) | `check`, `lint`, and `doctor` |
| [Transactions](https://jowharshamshiri.github.io/jdb/transactions) | Safety, recovery, concurrency |
| [Configuration](https://jowharshamshiri.github.io/jdb/configuration) | `.db/config` and resource limits |
| [Errors](https://jowharshamshiri.github.io/jdb/errors) | Error and exit code catalogue |
| [On-disk format](https://jowharshamshiri.github.io/jdb/format-v1) | Format version 1 |

## Development

```sh
cargo build
cargo test          # unit tests in each module, behaviour tests in tests/
cargo clippy --all-targets
```

## License

MIT — see [LICENSE](LICENSE).
