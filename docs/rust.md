---
title: Rust
---

# Rust

The `reldir-core` crate is the engine itself. Applications use its
[`Connection`](#connections); the command line, the MCP server and the Python
and Node.js packages are built on the same API.

```toml
[dependencies]
reldir-core = { path = "../reldir/crates/reldir-core" }
serde_json = "1"
```

The path is the `crates/reldir-core` directory of a clone of the reldir
repository; see [How to install reldir]({{ site.baseurl }}/install).

```rust
use reldir_core::{Connection, Options};
use serde_json::json;

let db = Connection::open("data", Options::default())?;
let adults = db.query("SELECT name FROM users WHERE age >= ?", &[json!(18)])?;
for row in &adults.rows {
    println!("{}", row["name"]);
}
db.transaction(|tx| {
    tx.execute(
        "UPDATE accounts SET balance = balance - 5 WHERE id = 'a'",
        &[],
    )?;
    tx.execute(
        "UPDATE accounts SET balance = balance + 5 WHERE id = 'b'",
        &[],
    )?;
    Ok(())
})?;
```

## Connections

```text
pub struct Options {
    pub readonly: bool,                 // write nothing at all, not even derived state
    pub no_establish: bool,             // refuse a folder that is not yet a database
    pub wait_seconds: Option<f64>,      // how long to wait for another writer's lock
    pub timeout_seconds: Option<f64>,   // how long a query may run
    pub max_result_rows: Option<usize>, // how many rows a query may return
}

let db = Connection::open(path, Options::default())?;
```

`Connection::open` adopts a folder of JSON files the first time, as the
command line does, unless `no_establish` is set; a folder that cannot be a
database is refused there, not at the first query. `db.root()` is the
database's folder.

A `Connection` names a database and how to use it. It holds no file open
between calls, so it is `Send + Sync` and cheap to clone, and every call
answers from the folder as it is then -- an edit another program made a moment
ago included. Share one across threads, or clone it.

## Reading

```rust
let answer: Rows = db.query("SELECT id, name FROM users WHERE age > ?", &[json!(30)])?;
answer.columns; // Vec<String>, in order -- even when there are no rows
answer.rows; // Vec<Map<String, Value>>

let named = reldir_core::api::named(json!({"id": "ada"}).as_object().unwrap());
let one = db.query_with("SELECT name FROM users WHERE id = :id", &named)?;

let count = db.query_each("SELECT * FROM posts", &[], |row| {
    // each row as it is produced, without collecting the answer
    Ok(())
})?;
```

`query` binds positional values to `?`. `query_with` takes
`reldir_core::sql::SqlParam`s, positional or named; `api::named` makes them
from a map. A statement that changes rows is refused by every query method.

| Method | Returns |
|---|---|
| `get(table, key)` | `Option<Map>`: the row a key names |
| `tables()` | `Vec<Table>`: `name`, `rows`, `key`, `pinned` |
| `check()` | `Report`: `valid`, `errors`, `warnings` |
| `state()` | a name for the state the database holds now: equal across two calls exactly when nothing changed |

A key is `&[Value]`, one value per key column, in key order.

## Changing

Every change is validated against the state it would make before any file is
written, and is written as one recorded revision, or refused and not written
at all.

```rust
let done: Changed = db.execute(
    "UPDATE users SET age = age + 1 WHERE id = ? RETURNING age",
    &[json!("bob")],
)?;
done.rows; // rows written or removed, referential actions included
done.induced; // of those, the rows referential actions changed
done.files; // files written or removed
done.returning; // RETURNING's rows
done.revision; // Option<Revision { number, id }>
done.replayed; // made before under the same idempotency key
done.warnings; // warnings the resulting state carries
```

`execute_with(sql, params, Change { idempotency_key, widen })` names the
change and says whether it may widen contracts:

- `idempotency_key`: if history already holds a revision made under the key,
  the change is not made again and that revision is reported, with
  `replayed: true`. Retrying after an uncertain failure is always safe.
- `widen`: admit, by widening the table's contract in the same revision, a
  value the contract never admitted; without it such a change fails with
  `CONTRACT_WIDENING_REQUIRED` (see [schemas]({{ site.baseurl }}/schemas)).

`Change` implements `Default`.

| Method | Changes |
|---|---|
| `execute_many(sql, rows)` | one statement, once per set of values, all as one change |
| `insert(table, rows)` | rows, generated columns filled in; `returning` holds them as written |
| `update(table, key, patch)` | the columns `patch` names; `returning` holds the row as written |
| `delete(table, key)` | a row, with its referrers' `onDelete` actions |
| `import(table, rows, mode)` | rows as one change; returns `(Changed, Imported)` |

`import` takes a `reldir_core::rows::ImportMode`: `Insert` (every row new),
`Upsert` (a row whose key the table holds sets the members it states), or
`Replace` (the table becomes exactly these rows). `Imported` counts the rows
`inserted`, `updated`, `deleted` and `unchanged`.

## Transactions

```rust
let mut tx = db.begin()?;
tx.execute("INSERT INTO posts (id, user_id) VALUES ('p2', 'cy')", &[])?;
tx.execute("INSERT INTO users (id, name) VALUES ('cy', 'Cy')", &[])?;
let seen = tx.query("SELECT count(*) AS n FROM posts WHERE user_id = 'cy'", &[])?;
let done = tx.commit()?;
```

`begin()` (or `begin_with(Change { .. })`) takes the writer lock and holds it
until the transaction is committed, rolled back or dropped. Each statement
sees what the ones before it did; referential actions run at the end of each
statement. `commit()` judges the state all the statements make together, and
writes it as one revision -- so a statement may break a reference in passing
as long as a later one mends it. `rollback()`, an error from `commit()`, or
dropping the transaction leaves every file as it was.

`Transaction` has `query`, `query_with`, `execute`, `execute_with`, `insert`,
`update`, `delete`, `statements` (how many have run), `commit` and `rollback`.
It belongs to the thread that began it.

`Connection::transaction(work)` runs a closure in a transaction: committed when
it returns `Ok`, rolled back when it returns `Err`. It returns the closure's
value and what the commit did.

A transaction held longer than `transaction_timeout_seconds` (see
[configuration]({{ site.baseurl }}/configuration)) is ended: its next statement
fails with `TRANSACTION_EXPIRED`, and nothing it did is written.

## Errors

Every failure is a `reldir_core::Error`:

```rust
match db.execute("UPDATE posts SET user_id = 'ghost'", &[]) {
    Err(error) if error.code() == reldir_core::Code::FOREIGN_KEY_VIOLATION => {
        for diagnostic in error.diagnostics() {
            println!("{:?} {}", diagnostic.path, diagnostic.message);
        }
    }
    other => {
        other?;
    }
}
```

`code()` is the registered `Code` (see [errors]({{ site.baseurl }}/errors)),
whose `exit()` is the command line's exit status for it. `diagnostics()` is
every fault behind the failure, the lead first; each `Diagnostic` carries its
`code`, `message`, `severity`, and where known its `table`, `path`,
`location`, `pointer`, `constraint`, `expected`, `observed` and `help`, and
serializes to the [protocol]({{ site.baseurl }}/protocol)'s diagnostic.

## Below the connection

The crate's modules are the engine. An embedder that needs more than the
connection offers can use them directly:

- `db::Database` observes a folder (`db::Access` says whether to read or to
  hold it for writing) and commits changes: `apply_with`, `change`,
  `begin`, `state_token`, `collect_garbage`, `begin_new_lineage`;
- `catalog::Catalog` is one observation of the folder, and `mirror::Mirror`
  the SQLite mirror it is kept in;
- `sql` classifies, plans and runs statements; `rows` builds the row changes
  of insert, update, delete and import;
- `schema`, `integrity` and `referential` judge rows; `store` and `history`
  record them; `merge` merges three versions of a row file.

These are the engine's own interfaces: they are public so that the crate's
front ends can be built on them, and they change with the engine more readily
than `Connection` does.
