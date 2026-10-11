//! The examples of the Rust guide (`docs/rust.md`), compiled with the
//! crate's tests. Each region from `// example` to `// end` is one rust
//! block of the page, verbatim; `tests/guide.rs` holds the page to them.

#![allow(
    unused,
    path_statements,
    clippy::no_effect,
    clippy::unnecessary_operation
)]

use reldir_core::{Changed, Connection, Error, Rows};
use serde_json::json;

fn example_1() -> Result<(), Error> {
    // example
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
    // end
    Ok(())
}

fn example_2(db: &Connection) -> Result<(), Error> {
    // example
    let answer: Rows = db.query("SELECT id, name FROM users WHERE age > ?", &[json!(30)])?;
    answer.columns; // Vec<String>, in order -- even when there are no rows
    answer.rows; // Vec<Map<String, Value>>

    let named = reldir_core::api::named(json!({"id": "ada"}).as_object().unwrap());
    let one = db.query_with("SELECT name FROM users WHERE id = :id", &named)?;

    let count = db.query_each("SELECT * FROM posts", &[], |row| {
        // each row as it is produced, without collecting the answer
        Ok(())
    })?;
    // end
    Ok(())
}

fn example_3(db: &Connection) -> Result<(), Error> {
    // example
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
    // end
    Ok(())
}

fn example_4(db: &Connection) -> Result<(), Error> {
    // example
    let mut tx = db.begin()?;
    tx.execute("INSERT INTO posts (id, user_id) VALUES ('p2', 'cy')", &[])?;
    tx.execute("INSERT INTO users (id, name) VALUES ('cy', 'Cy')", &[])?;
    let seen = tx.query("SELECT count(*) AS n FROM posts WHERE user_id = 'cy'", &[])?;
    let done = tx.commit()?;
    // end
    Ok(())
}

fn example_5(db: &Connection) -> Result<(), Error> {
    // example
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
    // end
    Ok(())
}

fn main() {
    // Compiled, not run: the examples assume a database in `data/`.
}
