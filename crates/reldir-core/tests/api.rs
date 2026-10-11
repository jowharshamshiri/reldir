//! The library API, used as an application uses it.

use reldir_core::{Code, Connection, Options};
use serde_json::{Map, Value, json};
use std::fs;
use std::path::Path;

const DIALECT: &str = "https://reldir.dev/schema/reldir-3";

fn write(root: &Path, relative: &str, value: Value) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&value).unwrap()),
    )
    .unwrap();
}

/// users <- posts, a cascade on delete; tickets numbered by a sequence.
fn folder() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let keyed = |table: &str| json!({"table": table, "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}}});
    write(
        root,
        "schema/users.json",
        json!({"$schema": DIALECT, "type": "object", "properties": {"id": {"type": "string"}, "name": {"type": "string"}, "age": {"type": "integer", "x-reldir-type": "int"}},
               "required": ["id", "name"], "additionalProperties": false, "x-reldir": keyed("users")}),
    );
    let mut posts = keyed("posts");
    posts["foreignKeys"] =
        json!([{"from": ["user_id"], "to": {"table": "users"}, "onDelete": "cascade"}]);
    write(
        root,
        "schema/posts.json",
        json!({"$schema": DIALECT, "type": "object", "properties": {"id": {"type": "string"}, "user_id": {"type": "string"}},
               "required": ["id", "user_id"], "additionalProperties": false, "x-reldir": posts}),
    );
    write(
        root,
        "schema/tickets.json",
        json!({"$schema": DIALECT, "type": "object", "properties": {"n": {"type": "integer", "x-reldir-type": "int"}, "what": {"type": "string"}},
               "required": ["n", "what"], "additionalProperties": false,
               "x-reldir": {"table": "tickets", "identity": {"source": "body", "columns": ["n"], "naming": {"rule": "reldir"}}, "generated": {"n": "sequence"}}}),
    );
    write(
        root,
        "users/ada.json",
        json!({"id": "ada", "name": "Ada", "age": 36}),
    );
    write(
        root,
        "users/bob.json",
        json!({"id": "bob", "name": "Bob", "age": 41}),
    );
    write(root, "posts/p1.json", json!({"id": "p1", "user_id": "ada"}));
    directory
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

#[test]
fn test9101_a_connection_is_shared_across_threads() {
    fn shareable<T: Send + Sync + Clone>() {}
    shareable::<Connection>();
    let directory = folder();
    let db = Connection::open(directory.path(), Options::default()).unwrap();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let db = db.clone();
            std::thread::spawn(move || db.query("SELECT count(*) AS n FROM users", &[]).unwrap())
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap().rows[0]["n"], 2);
    }
}

#[test]
fn test9102_queries_answer_columns_and_rows_and_see_outside_edits() {
    let directory = folder();
    let root = directory.path();
    let db = Connection::open(root, Options::default()).unwrap();
    let answer = db
        .query(
            "SELECT name, age FROM users WHERE age > ? ORDER BY name",
            &[json!(30)],
        )
        .unwrap();
    assert_eq!(answer.columns, ["name", "age"]);
    assert_eq!(answer.rows.len(), 2);
    assert_eq!(answer.rows[0]["name"], "Ada");
    let none = db
        .query("SELECT id, name FROM users WHERE age > 99", &[])
        .unwrap();
    assert!(none.rows.is_empty());
    assert_eq!(
        none.columns,
        ["id", "name"],
        "an empty answer still has its columns"
    );

    // Another program edits a file; the next call answers from it.
    write(
        root,
        "users/ada.json",
        json!({"id": "ada", "name": "Ada L", "age": 36}),
    );
    let named = db
        .query_with(
            "SELECT name FROM users WHERE id = :id",
            &reldir_core::api::named(&object(json!({"id": "ada"}))),
        )
        .unwrap();
    assert_eq!(named.rows[0]["name"], "Ada L");

    let error = db.query("DELETE FROM users", &[]).unwrap_err();
    assert_eq!(error.code(), Code::QUERY_UNSUPPORTED);
}

#[test]
fn test9103_changes_are_validated_and_report_what_they_wrote() {
    let directory = folder();
    let root = directory.path();
    let db = Connection::open(root, Options::default()).unwrap();

    let done = db
        .execute(
            "UPDATE users SET age = age + 1 WHERE id = ? RETURNING age",
            &[json!("bob")],
        )
        .unwrap();
    assert_eq!(done.returning, [object(json!({"age": 42}))]);
    assert!(done.revision.is_some());

    let tickets = db
        .insert(
            "tickets",
            vec![object(json!({"what": "a"})), object(json!({"what": "b"}))],
        )
        .unwrap();
    let numbers: Vec<&Value> = tickets.returning.iter().map(|row| &row["n"]).collect();
    assert_eq!(numbers, [&json!(1), &json!(2)], "generated keys come back");

    db.update(
        "users",
        &[json!("ada")],
        &object(json!({"name": "Augusta"})),
    )
    .unwrap();
    assert_eq!(
        db.get("users", &[json!("ada")]).unwrap().unwrap()["name"],
        "Augusta"
    );
    assert_eq!(db.get("users", &[json!("nobody")]).unwrap(), None);

    // A change that would leave a dangling reference is refused, and writes
    // nothing.
    let before = fs::read(root.join("posts/p1.json")).unwrap();
    let refused = db
        .execute("UPDATE posts SET user_id = 'ghost' WHERE id = 'p1'", &[])
        .unwrap_err();
    assert_eq!(refused.code(), Code::FOREIGN_KEY_VIOLATION);
    assert_eq!(fs::read(root.join("posts/p1.json")).unwrap(), before);

    let deleted = db.delete("users", &[json!("ada")]).unwrap();
    assert_eq!(deleted.rows, 2, "the user and the post that cascades");
    assert!(!root.join("posts/p1.json").exists());

    let report = db.check().unwrap();
    assert!(report.valid && report.errors.is_empty());
    let tables: Vec<(String, u64)> = db
        .tables()
        .unwrap()
        .into_iter()
        .map(|table| (table.name, table.rows))
        .collect();
    assert_eq!(
        tables,
        [
            ("posts".into(), 0),
            ("tickets".into(), 2),
            ("users".into(), 1)
        ]
    );
}

#[test]
fn test9104_a_transaction_reads_its_writes_and_commits_once_or_not_at_all() {
    let directory = folder();
    let root = directory.path();
    let db = Connection::open(root, Options::default()).unwrap();
    let state = db.state().unwrap();

    let mut transaction = db.begin().unwrap();
    transaction
        .execute("INSERT INTO posts (id, user_id) VALUES ('p2', 'cy')", &[])
        .unwrap();
    transaction
        .execute("INSERT INTO users (id, name) VALUES ('cy', 'Cy')", &[])
        .unwrap();
    let seen = transaction
        .query("SELECT count(*) AS n FROM posts WHERE user_id = 'cy'", &[])
        .unwrap();
    assert_eq!(seen.rows[0]["n"], 1);
    assert!(
        !root.join("users/cy.json").exists(),
        "nothing before commit"
    );
    let done = transaction.commit().unwrap();
    assert_eq!(done.rows, 2);
    assert!(root.join("users/cy.json").exists() && root.join("posts/p2.json").exists());
    assert_ne!(db.state().unwrap(), state);

    // Rolled back, or dropped: nothing written, and the lock is free again.
    let state = db.state().unwrap();
    let mut transaction = db.begin().unwrap();
    transaction.execute("DELETE FROM posts", &[]).unwrap();
    transaction.rollback().unwrap();
    {
        let mut transaction = db.begin().unwrap();
        transaction
            .execute("DELETE FROM users WHERE id = 'bob'", &[])
            .unwrap();
    }
    assert_eq!(db.state().unwrap(), state);

    // A transaction that ends invalid commits nothing.
    let mut transaction = db.begin().unwrap();
    transaction
        .execute(
            "INSERT INTO posts (id, user_id) VALUES ('p3', 'nobody')",
            &[],
        )
        .unwrap();
    let refused = transaction.commit().unwrap_err();
    assert_eq!(refused.code(), Code::FOREIGN_KEY_VIOLATION);
    assert!(!root.join("posts/p3.json").exists());

    // execute_many is one change: one bad row, and none is written.
    let refused = db
        .execute_many(
            "INSERT INTO users (id, name, age) VALUES (?, ?, ?)",
            &[
                vec![json!("dee"), json!("Dee"), json!(30)],
                vec![json!("eve"), json!("Eve"), json!("old")],
            ],
        )
        .unwrap_err();
    assert!(
        matches!(
            refused.code(),
            Code::TYPE_MISMATCH | Code::CONTRACT_WIDENING_REQUIRED
        ),
        "{refused}"
    );
    assert!(!root.join("users/dee.json").exists());
    let many = db
        .execute_many(
            "INSERT INTO users (id, name) VALUES (?, ?)",
            &[
                vec![json!("dee"), json!("Dee")],
                vec![json!("eve"), json!("Eve")],
            ],
        )
        .unwrap();
    assert_eq!(many.rows, 2);

    let (count, _) = db
        .transaction(|tx| {
            tx.execute("UPDATE users SET age = 1 WHERE age IS NULL", &[])?;
            Ok(tx
                .query("SELECT count(*) AS n FROM users WHERE age = 1", &[])?
                .rows[0]["n"]
                .clone())
        })
        .unwrap();
    assert_eq!(count, 3);
}

#[test]
fn test9105_a_readonly_connection_writes_nothing() {
    let directory = folder();
    let root = directory.path();
    let db = Connection::open(
        root,
        Options {
            readonly: true,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(
        db.query("SELECT count(*) AS n FROM users", &[])
            .unwrap()
            .rows[0]["n"],
        2
    );
    assert_eq!(
        db.execute("DELETE FROM posts", &[]).unwrap_err().code(),
        Code::READ_ONLY
    );
    assert!(db.begin().is_err());
    assert!(!root.join(".db").exists(), "not even derived state");

    let empty = tempfile::tempdir().unwrap();
    let refused = Connection::open(empty.path().join("missing"), Options::default()).unwrap_err();
    assert_eq!(refused.code(), Code::PATH_NOT_FOUND, "{refused}");
}
