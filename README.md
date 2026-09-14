# jdb

`jdb` is a filesystem-native relational JSON database. The `db` executable treats
JSON files as authoritative rows, validates them against explicit schemas, supports
SQL and direct CRUD, detects valid external edits, and commits its own edits through
a recoverable transaction journal.

```sh
cargo build --release
./target/release/db init ./data --adopt
./target/release/db --db ./data status
./target/release/db --db ./data sql 'SELECT * FROM users LIMIT 10'
```

The complete product requirements are in [`docs/spec.md`](docs/spec.md). The exact
version-1 disk representation and recovery protocol are documented in
[`docs/format-v1.md`](docs/format-v1.md).

Run the behavioral test suite with `cargo test`.
