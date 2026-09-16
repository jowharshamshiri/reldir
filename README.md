# reldir

**A relational database that lives in a directory of JSON files.**

`reldir` turns an ordinary folder into a validated relational database. Rows stay
as human-readable JSON files you can read, edit, `grep`, and commit to Git. The
`reldir` binary governs their interpretation: it enforces schemas, checks referential
integrity, answers SQL, and records how the state changed, without taking
ownership of your bytes.

📦 **[crates.io](https://crates.io/crates/reldir)** &nbsp;·&nbsp; 📖 **[Documentation](https://jowharshamshiri.github.io/reldir/)**

```console
$ ls ./data
users/  posts/  comments/

$ reldir init ./data --adopt
Scanned 3 directories, 1204 JSON files.
VALID   revision 1   root 6e41f2…

$ reldir sql 'SELECT u.name, count(*) AS n
          FROM users u JOIN posts p ON p.user_id = u.id
          GROUP BY u.name ORDER BY n DESC LIMIT 3'
 name  | n
-------+---
 Alice | 41
 Bob   | 37
 Carol | 29
```

## Why

A folder of JSON files is transparent, diffable, and editable by any tool, but
nothing stops a typo from becoming invisible state. A database enforces
integrity, but the data is no longer readable on disk.

`reldir` keeps the filesystem authoritative and legible, and reports precisely when
it stops being a valid database:

```console
$ echo '{"id":"019...","user_id":"nobody","title":"x"}' > posts/broken.json
$ reldir status
INVALID   revision 1   (1 external change, 1 violation)

error[FOREIGN_KEY_VIOLATION]: posts.user_id references a row that does not exist
  --> posts/broken.json:1:24
   = constraint: posts.user_id -> users.id
   = help: run `reldir doctor` for fix options
```

Anything may edit the directory: you, your editor, a script, an agent, or `git
merge`. `reldir` judges the state it observes at each operation boundary. Valid
external edits are adopted as new revisions. Invalid ones are reported with the
file, line, and column.

## Install

```sh
cargo install reldir
```

Or build from a checkout:

```sh
cargo install --path . --locked
```

Requires a recent stable Rust toolchain (edition 2024). The result is a single
self-contained `reldir` executable, with no daemon, server, or runtime
dependencies.

## Try it

```sh
reldir init ./data --adopt     # infer schemas from existing JSON and adopt them
reldir check                   # full validation
reldir lint                    # how the schemas could be stronger
reldir doctor                  # diagnose problems and propose fixes
reldir sql 'SELECT * FROM users LIMIT 10'
```

## What you get

- **JSON files as rows**: one object per file, in canonical, diff-friendly formatting
- **Schemas as JSON Schema**: 2020-12 documents in a declared dialect, maintained in `.db/schema/` and pinnable to `schema/*.json` for version control, with 14 column types, constraints, and checks
- **Schema inference** that bootstraps the strictest schema your data supports
- **SQL**: `SELECT`/`INSERT`/`UPDATE`/`DELETE`, joins, grouping, aggregates, `EXPLAIN`
- **Referential integrity**: primary keys, unique, foreign keys, `CHECK`, cascade actions
- **Transactional writes** with a crash-recoverable journal
- **Provenance**: every accepted state transition is recorded, including external edits
- **Compiler-style diagnostics** with stable error codes and exit codes
- **Git-native**: CI-friendly validation, stable formatting, no lockfile churn

## Documentation

| Guide | |
|---|---|
| [Getting started](https://jowharshamshiri.github.io/reldir/getting-started) | Adopt a directory and run your first queries |
| [Concepts](https://jowharshamshiri.github.io/reldir/concepts) | The model: validity, external edits, provenance |
| [Schemas](https://jowharshamshiri.github.io/reldir/schemas) | The JSON Schema dialect, writing one by hand, types, constraints |
| [CLI reference](https://jowharshamshiri.github.io/reldir/cli) | Every command and flag |
| [SQL](https://jowharshamshiri.github.io/reldir/sql) | The supported subset |
| [Validation](https://jowharshamshiri.github.io/reldir/validation) | `check`, `lint`, and `doctor` |
| [Transactions](https://jowharshamshiri.github.io/reldir/transactions) | Safety, recovery, concurrency |
| [Configuration](https://jowharshamshiri.github.io/reldir/configuration) | `.db/config` and resource limits |
| [Errors](https://jowharshamshiri.github.io/reldir/errors) | Error and exit code catalogue |
| [On-disk format](https://jowharshamshiri.github.io/reldir/format-v1) | Format version 1 |

## Development

```sh
cargo build
cargo test          # unit tests in each module, behaviour tests in tests/
cargo clippy --all-targets
```

## License

MIT. See [LICENSE](LICENSE).
