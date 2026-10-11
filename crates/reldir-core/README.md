# reldir-core

The engine of [reldir](https://jowharshamshiri.github.io/reldir/): a folder of
JSON files governed as a relational database. Each table is a directory, each
row a JSON file, each table's rules a JSON Schema. The engine validates every
row and reference, answers SQL over the rows, and writes every change as one
validated, recorded transaction that changes only the bytes it changes.

```rust
use reldir_core::{Connection, Options};
use serde_json::json;

let db = Connection::open("data", Options::default())?;
let adults = db.query("SELECT name FROM users WHERE age >= ?", &[json!(18)])?;
db.transaction(|tx| {
    tx.execute("UPDATE accounts SET balance = balance - 5 WHERE id = 'a'", &[])?;
    tx.execute("UPDATE accounts SET balance = balance + 5 WHERE id = 'b'", &[])?;
    Ok(())
})?;
# Ok::<(), reldir_core::Error>(())
```

The `reldir` crate is the command line, shell and MCP server built on it; the
Python and Node packages embed it in-process. See the
[Rust API guide](https://jowharshamshiri.github.io/reldir/rust) and the
crate documentation.

Licensed under MIT.
