# reldir

**A relational database that lives in a directory of JSON files.**

`reldir` turns an ordinary folder into a validated relational database. Rows stay
human-readable JSON files you can read, edit, `grep` and commit to Git. The
binary governs what they mean -- their schemas, the references between them, the
rules over sets of them -- answers SQL, repairs what an outside edit broke, and
records how the state changed, without taking ownership of your bytes.

📦 **[crates.io](https://crates.io/crates/reldir)** &nbsp;·&nbsp; 📖 **[Documentation](https://jowharshamshiri.github.io/reldir/)**

```console
$ reldir sql 'SELECT u.name, p.title FROM users u JOIN posts p ON p.user_id = u.id ORDER BY u.name'
name  | title
------+----------
Ada   | Engines
Grace | Compilers
(2 rows)
2 row(s)
```

## Why

A folder of JSON is transparent, diffable and editable by any tool, but nothing
stops a typo, a dangling reference or a half-finished edit from becoming
invisible state. A database enforces integrity, but the data is no longer
yours to read. reldir keeps the files authoritative and legible, and makes sure
they stay a valid database:

- **Nothing reldir writes can break the rules.** Every change -- SQL, the row
  commands, imports, migrations, repairs -- is judged as the state it would
  produce before a byte is written, and refused, with every reason located to
  the line, when that state would be invalid.
- **Anything else may edit the files.** A valid outside edit is recorded as a
  new revision; an invalid one is reported to the file, line, column and JSON
  Pointer, and `reldir doctor` repairs it least destructively first -- a row
  deleted by mistake is restored byte for byte from history.

```console
$ reldir delete users ada
error[FOREIGN_KEY_VIOLATION]: deleting users/ada.json refused: posts/p1.json still references it at /user_id (posts.user_id -> users, onDelete restrict)
  --> posts/p1.json:3:14
[..]
```

## What you get

- **JSON Schema 2020-12, all of it**, enforced on every row -- bounds,
  patterns, formats, compositions, conditionals -- plus what JSON Schema cannot
  say: primary keys, unique keys, references at any depth
  (`modules[].lessons[].lesson_ref`), identity domains shared by many tables,
  acyclic graphs, per-row SQL checks and set-level assertions.
- **Referential actions** reldir carries out itself: `restrict`, `cascade`,
  `remove` (drop the array element), `set_null`, `set_default`, on delete and on
  key change, every touched row reported.
- **SQL** in SQLite's dialect over an incrementally maintained mirror: joins,
  window functions, JSON functions; changes validated before they are written.
- **Inference** that bootstraps the strictest schemas your data supports, and a
  **lint** that proposes the references and constraints the schemas miss.
- **Crash-safe transactions**, tested by crashing at every single filesystem
  operation; **history** that verifies; **snapshots**; **concurrent writers**
  that queue rather than lose updates.
- **One machine contract**: every command speaks a `command_result` JSON
  envelope, JSON Lines, CSV or SARIF, with stable error codes and exit statuses.
- **`reldir mcp`**: the database, served to AI agents over the Model Context
  Protocol, with the same guarantees.

## Install

```sh
cargo install reldir
```

Or from a checkout: `cargo install --path . --locked`. One self-contained
executable (Rust 1.88 or later to build); no daemon, no server.

## Try it

```sh
reldir sql 'SELECT * FROM users'   # a folder of JSON is already a database
reldir                             # a shell over it
reldir check                       # is it valid?
reldir lint                        # could its schemas say more?
reldir doctor                      # what can be repaired, and how
```

## Documentation

| | |
|---|---|
| [Getting started](https://jowharshamshiri.github.io/reldir/getting-started) | Query a folder of JSON, then make it stricter |
| [Concepts](https://jowharshamshiri.github.io/reldir/concepts) | Validity, outside edits, identity, history |
| [Schemas](https://jowharshamshiri.github.io/reldir/schemas) | The dialect, references, domains, checks, assertions |
| [SQL](https://jowharshamshiri.github.io/reldir/sql) | Queries, and changing rows safely |
| [Validation](https://jowharshamshiri.github.io/reldir/validation) | `check`, `lint` and `doctor` |
| [Transactions](https://jowharshamshiri.github.io/reldir/transactions) | The commit protocol, recovery, concurrency |
| [CLI reference](https://jowharshamshiri.github.io/reldir/cli) | Every command, flag and output format |
| [Configuration](https://jowharshamshiri.github.io/reldir/configuration) | `.db/config` and limits |
| [Errors](https://jowharshamshiri.github.io/reldir/errors) | Every code and exit status |
| [On-disk format](https://jowharshamshiri.github.io/reldir/format) | Format 2, byte for byte |
| [MCP server](https://jowharshamshiri.github.io/reldir/mcp) | The database, served to AI agents |

## Python

```python
import reldir

with reldir.connect("./data") as db:
    db.execute("INSERT INTO users (id, name) VALUES (?, ?)", ["u1", "Alice"])
    for row in db.query("SELECT id, name FROM users ORDER BY name"):
        print(row["id"], row["name"])
```

A driver over the binary: parameter binding, typed exceptions carrying the
binary's diagnostics, and retries for lock contention. See
[python/README.md](python/README.md).

## Development

```sh
cargo test                      # unit tests in each module; tests/ for behavior, docs, MCP
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## License

MIT. See [LICENSE](LICENSE).
