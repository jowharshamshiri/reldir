# reldir

**A relational database that lives in a folder of JSON files.**

reldir governs an ordinary folder -- one directory per table, one JSON file per
row -- as a relational database. The files stay the source of truth: people,
editors, scripts and `git` read and write them directly. reldir adds schemas,
references between rows, SQL, changes that are validated before a byte is
written, and a verified history -- and never writes a file outside the shape
the programs reading it rely on.

**[Documentation](https://jowharshamshiri.github.io/reldir/)** ·
[Getting started](https://jowharshamshiri.github.io/reldir/getting-started) ·
[Concepts](https://jowharshamshiri.github.io/reldir/concepts)

```console
$ reldir sql 'SELECT u.name, p.title FROM users u JOIN posts p ON p.user_id = u.id ORDER BY u.name'
name  | title
------+----------
Ada   | Engines
Grace | Compilers
(2 rows)
2 row(s)
$ reldir delete users ada
error[FOREIGN_KEY_VIOLATION]: deleting users/ada.json refused: posts/p1.json still references it at /user_id (posts.user_id -> users, onDelete restrict)
  --> posts/p1.json:3:14
[..]
```

## What it does

- **Adopts a folder as it is**, inferring each table's schema from its files
  and changing none of them.
- **Keeps the folder's contract**: SQL, row commands, imports, referential
  actions and repairs never write a value the table's files never held -- a new
  member, a null where there was none, a float where there were integers --
  unless you widen the contract yourself.
- **Writes only what changes**: an update rewrites the members it changes, and
  every other byte of the file stays as it was; a change across many files
  appears whole or not at all.
- **SQL** in SQLite's dialect, with multi-statement transactions.
- **JSON Schema 2020-12** on every row, plus keys, references at any depth,
  referential actions, identity domains, acyclic graphs, checks and assertions.
- **Outside edits are judged**: a valid edit is recorded, an invalid one is
  located to its line and column, and `reldir doctor` repairs it.
- **History** of every state, snapshots, and git integration: row files merge
  member by member.
- **`reldir mcp`** serves the database to AI agents; the engine embeds in
  **Python** (DB-API 2.0), **Node.js** and **Rust**.

## Install

From a clone of this repository, with Rust 1.89 or later:

```sh
cargo install --locked --path crates/reldir   # the command
pip install ./crates/reldir-py                # the Python library, with the command
```

The Node.js and Rust libraries, and what each needs, are in
[How to install reldir](https://jowharshamshiri.github.io/reldir/install).

## Try it

```sh
reldir sql 'SELECT * FROM users'   # a folder of JSON is already a database
reldir                             # a shell over it
reldir check                       # is it valid?
reldir doctor                      # what can be repaired, and how
reldir serve                       # watch it, so large folders open fast
```

## From a program

```python
import reldir

with reldir.connect("data") as db:     # commits on success, rolls back on error
    db.execute("UPDATE users SET age = age + 1 WHERE id = ?", ["ada"])
    for row in db.query("SELECT id, name FROM users ORDER BY name"):
        print(row["id"], row["name"])
```

```js
const reldir = require('@reldir/reldir');

const db = await reldir.open('data');
await db.transaction(async (tx) => {
  await tx.execute("UPDATE accounts SET balance = balance - 5 WHERE id = 'a'");
  await tx.execute("UPDATE accounts SET balance = balance + 5 WHERE id = 'b'");
});
```

```rust
use reldir_core::{Connection, Options};
use serde_json::json;

let db = Connection::open("data", Options::default())?;
let adults = db.query("SELECT name FROM users WHERE age >= ?", &[json!(18)])?;
```

## Development

```sh
cargo test                                                 # the engine and the command
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all --check
maturin develop -m crates/reldir-py/Cargo.toml && pytest crates/reldir-py/tests
(cd crates/reldir-node && npm ci && npm run build && npm test)
cargo xtask scale --rows 100000                            # the performance targets
```

## License

MIT. See [LICENSE](LICENSE).
