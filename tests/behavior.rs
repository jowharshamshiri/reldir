use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;

fn db() -> Command {
    Command::cargo_bin("db").unwrap()
}

/// Declare a pinned schema.
///
/// `schema/` is the user's pin directory: jdb never creates it, so a fixture
/// that declares a schema creates it the way a user would.
fn pin(root: impl AsRef<std::path::Path>, table: &str, body: &str) {
    let root = root.as_ref();
    fs::create_dir_all(root.join("schema")).unwrap();
    fs::write(root.join(format!("schema/{table}.json")), body).unwrap();
}

fn adopted() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("users")).unwrap();
    fs::write(
        dir.path().join("users/u1.json"),
        "{\"id\":\"u1\",\"name\":\"Alice\"}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("users/u2.json"),
        "{\"id\":\"u2\",\"name\":\"Bob\"}\n",
    )
    .unwrap();
    db().args([
        "--format",
        "table",
        "init",
        dir.path().to_str().unwrap(),
        "--adopt",
    ])
    .assert()
    .success();
    dir
}

#[test]
fn test0001_adoption_query_crud_and_external_revision_are_real() {
    let dir = adopted();
    let root = dir.path().to_str().unwrap();
    db().args([
        "--db",
        root,
        "--format",
        "table",
        "sql",
        "SELECT name FROM users ORDER BY name",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("Alice").and(predicate::str::contains("Bob")));
    db().args([
        "--db",
        root,
        "--format",
        "table",
        "update",
        "users",
        "u2",
        "{\"name\":\"Robert\"}",
    ])
    .assert()
    .success();
    let bytes = fs::read_to_string(dir.path().join("users/u2.json")).unwrap();
    assert!(bytes.ends_with('\n'));
    assert!(bytes.contains("  \"name\": \"Robert\""));
    fs::write(
        dir.path().join("users/u3.json"),
        "{\"id\":\"u3\",\"name\":\"Carol\"}\n",
    )
    .unwrap();
    db().args(["--db", root, "--format", "table", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("revision 3").and(predicate::str::contains("accepted")));
}

#[test]
fn test0002_invalid_external_state_is_rejected_without_advancing_revision() {
    let dir = adopted();
    let root = dir.path().to_str().unwrap();
    fs::write(
        dir.path().join("users/wrong.json"),
        "{\"id\":\"u3\",\"name\":4}\n",
    )
    .unwrap();
    db().args(["--db", root, "--format", "table", "status"])
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("IDENTITY_MISMATCH")
                .and(predicate::str::contains("TYPE_MISMATCH")),
        );
    db().args([
        "--db",
        root,
        "--format",
        "table",
        "sql",
        "SELECT * FROM users",
    ])
    .assert()
    .code(2)
    .stderr(predicate::str::contains("IDENTITY_MISMATCH"));
}

#[test]
fn test0003_failed_adoption_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("things")).unwrap();
    fs::write(dir.path().join("things/a.json"), "[1,2]\n").unwrap();
    db().args(["init", dir.path().to_str().unwrap(), "--adopt"])
        .assert()
        .code(8)
        .stderr(predicate::str::contains("INFER_ROOT_NOT_OBJECT"));
    assert!(!dir.path().join(".db").exists());
    assert!(!dir.path().join("schema").exists());
}

#[test]
fn test0004_dry_run_does_not_mutate() {
    let dir = adopted();
    let root = dir.path().to_str().unwrap();
    let before = fs::read(dir.path().join("users/u1.json")).unwrap();
    db().args([
        "--db",
        root,
        "--format",
        "table",
        "--dry-run",
        "update",
        "users",
        "u1",
        "{\"name\":\"Changed\"}",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("would change"));
    assert_eq!(before, fs::read(dir.path().join("users/u1.json")).unwrap());
}

#[test]
fn test0005_sql_delete_executes_declared_cascade_in_one_revision() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("users")).unwrap();
    fs::create_dir(root.join("posts")).unwrap();
    pin(
        root,
        "users",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"users","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id"]}}"#,
    );
    pin(
        root,
        "posts",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"user_id":{"type":"string"}},"required":["id","user_id"],"additionalProperties":false,"x-jdb":{"table":"posts","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","user_id"],"foreignKeys":[{"columns":["user_id"],"references":{"table":"users","columns":["id"]},"onDelete":"cascade","onUpdate":"cascade"}]}}"#,
    );
    fs::write(root.join("users/u1.json"), "{\"id\":\"u1\"}\n").unwrap();
    fs::write(
        root.join("posts/p1.json"),
        "{\"id\":\"p1\",\"user_id\":\"u1\"}\n",
    )
    .unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "status",
    ])
    .assert()
    .success();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "delete",
        "users",
        "u1",
    ])
    .assert()
    .success()
    .stdout(
        predicate::str::contains("users/u1.json").and(predicate::str::contains("posts/p1.json")),
    );
    assert!(!root.join("users/u1.json").exists());
    assert!(!root.join("posts/p1.json").exists());
}

#[test]
fn test0006_direct_primary_key_update_uses_declared_cascade() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("users")).unwrap();
    fs::create_dir(root.join("posts")).unwrap();
    pin(
        root,
        "users",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"users","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id"]}}"#,
    );
    pin(
        root,
        "posts",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"user_id":{"type":"string"}},"required":["id","user_id"],"additionalProperties":false,"x-jdb":{"table":"posts","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","user_id"],"foreignKeys":[{"columns":["user_id"],"references":{"table":"users","columns":["id"]},"onDelete":"cascade","onUpdate":"cascade"}]}}"#,
    );
    fs::write(root.join("users/u1.json"), "{\"id\":\"u1\"}\n").unwrap();
    fs::write(
        root.join("posts/p1.json"),
        "{\"id\":\"p1\",\"user_id\":\"u1\"}\n",
    )
    .unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "status",
    ])
    .assert()
    .success();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "update",
        "users",
        "u1",
        "{\"id\":\"u2\"}",
    ])
    .assert()
    .success()
    .stdout(
        predicate::str::contains("users/u2.json").and(predicate::str::contains("posts/p1.json")),
    );
    assert!(!root.join("users/u1.json").exists());
    assert!(root.join("users/u2.json").exists());
    assert!(
        fs::read_to_string(root.join("posts/p1.json"))
            .unwrap()
            .contains("\"user_id\": \"u2\"")
    );
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "json",
        "diff",
        "2",
        "3",
    ])
    .assert()
    .success()
    .stdout(
        predicate::str::contains("\"kind\": \"key_change\"")
            .and(predicate::str::contains("users/u1.json"))
            .and(predicate::str::contains("users/u2.json")),
    );
}

#[test]
fn test0007_sql_dml_rejects_silent_storage_class_coercion_and_preserves_bool_output() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("items")).unwrap();
    pin(
        root,
        "items",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"count":{"type":"integer","x-jdb-type":"int"},"active":{"type":"boolean"}},"required":["id","count","active"],"additionalProperties":false,"x-jdb":{"table":"items","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","count","active"]}}"#,
    );
    fs::write(
        root.join("items/a.json"),
        "{\"id\":\"a\",\"count\":1,\"active\":true}\n",
    )
    .unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "status",
    ])
    .assert()
    .success();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "sql",
        "INSERT INTO items (id, count, active) VALUES ('a', 2, false)",
    ])
    .assert()
    .code(2)
    .stderr(predicate::str::contains("PRIMARY_KEY_VIOLATION"));
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "sql",
        "UPDATE items SET count = ? WHERE id = 'a'",
        "--param",
        "\"2\"",
    ])
    .assert()
    .code(4)
    .stderr(predicate::str::contains("TYPE_MISMATCH"));
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "sql",
        "SELECT active FROM items",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("\"active\":true"));
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "sql",
        "WITH chosen(id) AS (VALUES ('a')) UPDATE items SET count = 3 WHERE id IN (SELECT id FROM chosen)",
    ])
    .assert()
    .success();
    let row: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("items/a.json")).unwrap()).unwrap();
    assert_eq!(row["count"], 3);
}

#[test]
fn test0008_doctor_repairs_layout_without_changing_row_body() {
    let dir = adopted();
    let root = dir.path();
    let original = fs::read(root.join("users/u1.json")).unwrap();
    fs::rename(root.join("users/u1.json"), root.join("users/moved.json")).unwrap();
    let preview = db()
        .args([
            "--db",
            root.to_str().unwrap(),
            "--format",
            "json",
            "--dry-run",
            "doctor",
            "--fix",
            "--only",
            "FIX_RENAME_TO_IDENTITY",
        ])
        .output()
        .unwrap();
    assert!(preview.status.success());
    let preview: serde_json::Value = serde_json::from_slice(&preview.stdout).unwrap();
    let records = preview.as_array().unwrap();
    assert!(records.iter().any(|record| {
        record["kind"] == "doctor_fix"
            && record["paths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|path| path == "users/moved.json")
    }));
    assert!(records.iter().any(|record| record["kind"] == "doctor_diff"));
    assert!(root.join("users/moved.json").exists());
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "doctor",
        "--fix",
        "--yes",
        "--only",
        "FIX_RENAME_TO_IDENTITY",
    ])
    .assert()
    .success();
    assert_eq!(original, fs::read(root.join("users/u1.json")).unwrap());
    assert!(!root.join("users/moved.json").exists());
    assert!(
        root.join(".db/snapshots/pre-doctor-1/users/moved.json")
            .exists()
    );
}

#[test]
fn test0009_logical_hash_ignores_formatting_only_edits() {
    let dir = adopted();
    let root = dir.path();
    fs::write(
        root.join("users/u1.json"),
        "{\n  \"name\": \"Alice\",\n  \"id\": \"u1\"\n}\n",
    )
    .unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "status",
    ])
    .assert()
    .success()
    .stdout(
        predicate::str::contains("revision 1")
            .and(predicate::str::contains("external changes: none")),
    );
}

#[test]
fn test0010_named_parameters_are_bound_as_values_and_revision_diff_is_semantic() {
    let dir = adopted();
    let root = dir.path().to_str().unwrap();
    db().args([
        "--db",
        root,
        "--format",
        "json",
        "sql",
        "SELECT name FROM users WHERE id = :id",
        "--param",
        "id=\"u2\"",
    ])
    .assert()
    .success()
    .stdout(
        predicate::str::contains("\"name\": \"Bob\"")
            .and(predicate::str::contains("\"kind\": \"row\"")),
    );
    db().args([
        "--db",
        root,
        "--format",
        "table",
        "update",
        "users",
        "u2",
        "{\"name\":\"Robert\"}",
    ])
    .assert()
    .success();
    db().args(["--db", root, "--format", "jsonl", "diff", "1", "2"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("\"kind\":\"field_change\"")
                .and(predicate::str::contains("\"field\":\"name\""))
                .and(predicate::str::contains("\"old\":\"Bob\""))
                .and(predicate::str::contains("\"new\":\"Robert\"")),
        );
}

#[test]
fn test0011_snapshot_restores_authoritative_config_and_rows() {
    let dir = adopted();
    let root = dir.path();
    let original_config = fs::read(root.join(".db/config")).unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "snapshot",
        "create",
        "before",
    ])
    .assert()
    .success();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "update",
        "users",
        "u1",
        "{\"name\":\"Changed\"}",
    ])
    .assert()
    .success();
    let mut config: serde_json::Value = serde_json::from_slice(&original_config).unwrap();
    config["max_result_rows"] = serde_json::Value::from(1);
    fs::write(
        root.join(".db/config"),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "snapshot",
        "restore",
        "before",
        "--yes",
    ])
    .assert()
    .success();
    assert!(
        fs::read_to_string(root.join("users/u1.json"))
            .unwrap()
            .contains("Alice")
    );
    assert_eq!(original_config, fs::read(root.join(".db/config")).unwrap());
}

#[test]
fn test0012_declarative_migration_is_atomic_across_schema_and_rows() {
    let dir = adopted();
    let root = dir.path();
    let migration = root.join("migration.json");
    fs::write(&migration,r#"{"operations":[{"op":"rename_column","table":"users","column":"name","new":"display_name"},{"op":"add_column","table":"users","column":"active","type":"bool","default":true}]}"#).unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "migrate",
        "apply",
        migration.to_str().unwrap(),
    ])
    .assert()
    .success();
    let row: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("users/u1.json")).unwrap()).unwrap();
    assert_eq!(row["display_name"], "Alice");
    assert_eq!(row["active"], true);
    assert!(row.get("name").is_none());
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
}

/// A migration file carries a schema, and that schema is a JSON Schema document
/// like any other -- not a second grammar maintained alongside the first.
///
/// `add_table` used to embed jdb's own schema shape inside migration files,
/// which made the migration format a parallel way to spell a schema: a change
/// to the dialect would have left it behind, silently accepting documents the
/// rest of the binary had stopped understanding. It now goes through the same
/// codec as a schema file, so there is one definition of what a schema is.
#[test]
fn test0075_migrations_carry_schemas_in_the_dialect_and_refuse_any_other() {
    let dir = adopted();
    let root = dir.path();

    let accepted = root.join("add-tags.json");
    fs::write(
        &accepted,
        r#"{"operations":[{"op":"add_table","table":"tags","schema":{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"label":{"type":"string"}},"required":["id","label"],"additionalProperties":false,"x-jdb":{"table":"tags","primaryKey":["id"],"columnOrder":["id","label"]}}}]}"#,
    )
    .unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "migrate",
        "apply",
        accepted.to_str().unwrap(),
    ])
    .assert()
    .success();

    // The table exists, and the schema written for it is the dialect -- the
    // same bytes the codec would have produced for a hand-written pin.
    let written: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".db/schema/tags.json")).unwrap()).unwrap();
    assert_eq!(written["$schema"], "https://jdb.dev/schema/jdb-1");
    assert_eq!(written["x-jdb"]["table"], "tags");
    assert_eq!(written["x-jdb"]["columnOrder"][1], "label");
    assert_eq!(written["properties"]["label"]["type"], "string");

    // The old grammar is refused by name rather than quietly accepted through a
    // path the dialect does not govern.
    let refused = root.join("add-legacy.json");
    fs::write(
        &refused,
        r#"{"operations":[{"op":"add_table","table":"legacy","schema":{"table":"legacy","primary_key":["id"],"columns":{"id":{"type":"string"}}}}]}"#,
    )
    .unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "migrate",
        "apply",
        refused.to_str().unwrap(),
    ])
    .assert()
    .code(2)
    .stderr(predicate::str::contains("SCHEMA_UNSUPPORTED_KEYWORD"));
    assert!(
        !root.join(".db/schema/legacy.json").exists(),
        "a refused migration must write nothing"
    );

    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
}

/// A schema change is reported in the terms the schema is written in.
///
/// Stored revision objects hold the semantic encoding, because that is what a
/// schema's identity is taken over. Printing it verbatim would show a reader a
/// shape that appears in no file they can edit -- snake_case keys, columns as
/// [name, definition] pairs -- so `diff` reports what changed about the table
/// instead. `db diff` promises semantic changes; for a schema those are its
/// columns and constraints, not its serialization.
#[test]
fn test0076_schema_changes_are_reported_semantically() {
    let dir = adopted();
    let root = dir.path();

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "migrate",
        "add-column",
        "users",
        "active",
        "--type",
        "bool",
        "--default",
        "true",
    ])
    .assert()
    .success();

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "diff",
        "1",
        "2",
        "--schema",
    ])
    .assert()
    .success()
    .stdout(
        predicate::str::contains("\"kind\":\"column_added\"")
            .and(predicate::str::contains("\"name\":\"active\""))
            // The column reads as the file spells it, so a reader can act on
            // the diff without translating it.
            .and(predicate::str::contains("\"type\":\"boolean\""))
            .and(predicate::str::contains("\"default\":true"))
            // None of the internal encoding may reach the reader: neither the
            // document keys the hash is taken over, nor `nullable`, nor jdb's
            // own type names, appear in any schema file.
            .and(predicate::str::contains("\"primary_key\"").not())
            .and(predicate::str::contains("additional_fields").not())
            .and(predicate::str::contains("nullable").not())
            .and(predicate::str::contains("\"type\":\"bool\"").not()),
    );

    // A constraint change is named by the key the dialect spells it with.
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "migrate",
        "add-index",
        "users",
        "active",
    ])
    .assert()
    .success();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "diff",
        "2",
        "3",
        "--schema",
    ])
    .assert()
    .success()
    .stdout(
        predicate::str::contains("\"kind\":\"schema_change\"")
            .and(predicate::str::contains("\"name\":\"indexes\"")),
    );
}

/// A fix doctor lists is a fix doctor applies.
///
/// `FIX_PIN_SCHEMA` was advertised by the plan, documented in the fix table,
/// and implemented by nothing: `--fix` printed it and then reported no
/// applicable automatic fixes. A plan that names a remedy it will not perform
/// is worse than one that stays silent, because the reader acts on it.
///
/// Pinning copies the working schema into `schema/`, so the fix can only ever
/// create a declaration. Replacing one discards something a person wrote and
/// stays an explicit `db schema pin --overwrite`.
#[test]
fn test0077_doctor_pins_an_unpinned_schema_and_never_replaces_a_declaration() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("t")).unwrap();
    fs::create_dir(root.join("u")).unwrap();
    fs::write(root.join("t/a.json"), "{\"id\":\"a\"}\n").unwrap();
    fs::write(root.join("u/b.json"), "{\"id\":\"b\"}\n").unwrap();
    db().args(["--format", "table", "init", root.to_str().unwrap(), "--adopt"])
        .assert()
        .success();

    // `u` is pinned by hand and then edited, so the declaration says something
    // the working copy does not.
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "schema",
        "pin",
        "u",
    ])
    .assert()
    .success();
    let pin = root.join("schema/u.json");
    let mut declared: serde_json::Value =
        serde_json::from_slice(&fs::read(&pin).unwrap()).unwrap();
    declared["x-jdb"]["indexes"] = serde_json::json!([["id"]]);
    fs::write(&pin, serde_json::to_vec_pretty(&declared).unwrap()).unwrap();

    // The plan offers the fix for the table that has no pin. The plan is the
    // command's answer, so it goes to stdout; lint findings are notices and go
    // to stderr, which is why the two assertions here read different streams.
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "doctor"])
        .assert()
        .stdout(predicate::str::contains("FIX_PIN_SCHEMA"));

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "doctor",
        "--fix",
        "--yes",
        "--no-snapshot",
        "--only",
        "FIX_PIN_SCHEMA",
    ])
    .assert()
    .success();

    // The unpinned table gained a declaration, byte-identical to what it
    // derived, so pinning cannot itself change what the database enforces.
    assert_eq!(
        fs::read(root.join("schema/t.json")).unwrap(),
        fs::read(root.join(".db/schema/t.json")).unwrap(),
        "the pin must be the working schema, not a re-rendering of it"
    );

    // The declaration someone wrote is untouched.
    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(&pin).unwrap()).unwrap();
    assert_eq!(
        after["x-jdb"]["indexes"],
        serde_json::json!([["id"]]),
        "an existing pin is a declaration doctor must not overwrite"
    );

    // And the finding it answered is gone.
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "lint"])
        .assert()
        .stderr(predicate::str::contains("LINT_SCHEMA_UNPINNED").not());
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
}

/// A fix selected by the id the plan prints is the fix that runs.
///
/// `doctor` filters findings by their attached fix id, so `--only
/// FIX_ADD_GENERATOR` reached nothing while a bare `--fix` applied it: the
/// finding named the remedy in the plan and carried no id to select it by. A
/// reader who copies an id off the plan must get the fix that id names.
#[test]
fn test0078_a_fix_selected_by_its_printed_id_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("t")).unwrap();
    for i in 0..25 {
        let id = format!("0192f0{i:02}-0000-7000-8000-000000000000");
        fs::write(
            root.join(format!("t/{id}.json")),
            format!("{{\"id\":\"{id}\"}}\n"),
        )
        .unwrap();
    }
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir_all(root.join("schema")).unwrap();
    fs::write(
        root.join("schema/t.json"),
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string","format":"uuid"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"t","primaryKey":["id"],"columnOrder":["id"]}}"#,
    )
    .unwrap();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "status"])
        .assert()
        .success();

    // The plan names the remedy, so that id is what a reader will pass.
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "doctor"])
        .assert()
        .stdout(predicate::str::contains("FIX_ADD_GENERATOR"));

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "doctor",
        "--fix",
        "--yes",
        "--no-snapshot",
        "--only",
        "FIX_ADD_GENERATOR",
    ])
    .assert()
    .success();

    let schema: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".db/schema/t.json")).unwrap()).unwrap();
    assert_eq!(
        schema["x-jdb"]["generated"]["id"], "uuid",
        "the fix named by the plan must be the fix that ran"
    );
}

#[test]
fn test0013_schema_errors_have_specific_codes_and_locations() {
    let dir = tempfile::tempdir().unwrap();
    db().args(["init", dir.path().to_str().unwrap()])
        .assert()
        .success();
    // A hand-written schema is a pin, and `db init` creates no pin directory.
    fs::create_dir(dir.path().join("schema")).unwrap();
    pin(
        dir.path(),
        "bad",
        "{\n  \"$schema\": \"https://jdb.dev/schema/jdb-1\",\n  \"type\": \"object\",\n  \"properties\": {\"id\": \"string\"},\n  \"x-jdb\": {\"table\": \"bad\", \"primaryKey\": [\"id\"], \"columnOrder\": [\"id\"]}\n}\n",
    );
    db().args([
        "--db",
        dir.path().to_str().unwrap(),
        "--format",
        "table",
        "status",
    ])
    .assert()
    .code(2)
    .stderr(
        predicate::str::contains("SCHEMA_COLUMN_TYPE_MISSING")
            .and(predicate::str::contains("schema/bad.json:4")),
    );
}

#[test]
fn test0014_duplicate_keys_are_rejected_before_any_mutation_is_planned() {
    let dir = adopted();
    let root = dir.path().to_str().unwrap();
    let before = fs::read_to_string(dir.path().join(".db/manifest.json")).unwrap();
    db().args([
        "--db",
        root,
        "--format",
        "table",
        "insert",
        "users",
        r#"{"id":"u3","id":"u4","name":"Carol"}"#,
    ])
    .assert()
    .code(1)
    .stderr(predicate::str::contains("duplicate object key \"id\""));
    assert!(!dir.path().join("users/u3.json").exists());
    assert!(!dir.path().join("users/u4.json").exists());
    assert_eq!(
        before,
        fs::read_to_string(dir.path().join(".db/manifest.json")).unwrap()
    );
}

#[test]
fn test0015_defaults_participate_in_identity_and_uniqueness_as_logical_values() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("items")).unwrap();
    pin(
        root,
        "items",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string","default":"fixed"}},"additionalProperties":false,"x-jdb":{"table":"items","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id"]}}"#,
    );
    fs::write(root.join("items/fixed.json"), "{}\n").unwrap();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();

    pin(
        root,
        "items",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"tag":{"type":"string","default":"same"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"items","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","tag"],"unique":[["tag"]]}}"#,
    );
    fs::write(root.join("items/a.json"), "{\"id\":\"a\"}\n").unwrap();
    fs::write(root.join("items/b.json"), "{\"id\":\"b\"}\n").unwrap();
    fs::remove_file(root.join("items/fixed.json")).unwrap();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("UNIQUE_VIOLATION"));
}

#[test]
fn test0016_check_schema_typechecking_rejects_unknown_identifiers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["init", root.to_str().unwrap()])
        .assert()
        .success();
    pin(
        root,
        "items",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"count":{"type":"integer","x-jdb-type":"int"}},"required":["id","count"],"additionalProperties":false,"x-jdb":{"table":"items","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","count"],"checks":[{"name":"positive","expr":"typo > 0"}]}}"#,
    );
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("SCHEMA_CHECK_INVALID"));
}

#[test]
fn test0017_configured_indentation_and_command_line_resource_overrides_are_enforced() {
    let dir = adopted();
    let root = dir.path();
    let config_path = root.join(".db/config");
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["indentation_width"] = serde_json::Value::from(4);
    fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "update",
        "users",
        "u1",
        r#"{"name":"Updated"}"#,
    ])
    .assert()
    .success();
    let row = fs::read_to_string(root.join("users/u1.json")).unwrap();
    assert!(row.contains("    \"id\": \"u1\""));

    config["max_json_file_size"] = serde_json::Value::from(1);
    fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("RESOURCE_LIMIT"));
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--max-json-file-size",
        "100000",
        "--format",
        "table",
        "check",
    ])
    .assert()
    .success();
}

#[test]
fn test0018_decimal_ordering_is_arbitrary_precision_and_gc_retains_history() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("numbers")).unwrap();
    pin(
        root,
        "numbers",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"amount":{"type":"string","pattern":"^-?(0|[1-9][0-9]*)(\\.[0-9]+)?$","x-jdb-type":"decimal"}},"required":["id","amount"],"additionalProperties":false,"x-jdb":{"table":"numbers","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","amount"]}}"#,
    );
    fs::write(
        root.join("numbers/a.json"),
        r#"{"id":"a","amount":"1000000000000000000000000000000000000000000"}"#,
    )
    .unwrap();
    fs::write(
        root.join("numbers/b.json"),
        r#"{"id":"b","amount":"999999999999999999999999999999999999999999"}"#,
    )
    .unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "sql",
        "SELECT id FROM numbers ORDER BY amount",
    ])
    .assert()
    .success()
    .stdout(predicate::str::starts_with("{\"id\":\"b\""));

    let fake_hash = "f".repeat(64);
    let fake = root.join(format!(".db/objects/{fake_hash}.json"));
    fs::write(&fake, "{}\n").unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "gc",
        "--dry-run",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains(&fake_hash));
    assert!(fake.exists());
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "gc"])
        .assert()
        .code(9)
        .stderr(predicate::str::contains("CONFIRMATION_REQUIRED"));
    assert!(fake.exists());
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "gc",
        "--yes",
    ])
    .assert()
    .success();
    assert!(!fake.exists());
    assert!(fs::read_dir(root.join(".db/objects")).unwrap().count() > 0);
}

#[test]
fn test0019_change_type_is_lossless_atomic_and_preserves_omitted_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("numbers")).unwrap();
    pin(
        root,
        "numbers",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"score":{"type":"string","default":"10"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"numbers","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","score"]}}"#,
    );
    fs::write(root.join("numbers/a.json"), "{\"id\":\"a\"}\n").unwrap();
    fs::write(
        root.join("numbers/b.json"),
        "{\"id\":\"b\",\"score\":\"20\"}\n",
    )
    .unwrap();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "migrate",
        "change-type",
        "numbers",
        "score",
        "int",
    ])
    .assert()
    .success();
    let schema: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".db/schema/numbers.json")).unwrap()).unwrap();
    let omitted: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("numbers/a.json")).unwrap()).unwrap();
    let explicit: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("numbers/b.json")).unwrap()).unwrap();
    assert_eq!(schema["properties"]["score"]["type"], "integer");
    assert_eq!(schema["properties"]["score"]["x-jdb-type"], "int");
    assert_eq!(schema["properties"]["score"]["default"], 10);
    assert!(omitted.get("score").is_none());
    assert_eq!(explicit["score"], 20);
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();

    let failed = tempfile::tempdir().unwrap();
    let failed_root = failed.path();
    db().args(["--format", "table", "init", failed_root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(failed_root.join("numbers")).unwrap();
    pin(
        failed_root,
        "numbers",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"score":{"type":"string"}},"required":["id","score"],"additionalProperties":false,"x-jdb":{"table":"numbers","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","score"]}}"#,
    );
    fs::write(
        failed_root.join("numbers/a.json"),
        "{\"id\":\"a\",\"score\":\"01\"}\n",
    )
    .unwrap();
    fs::write(
        failed_root.join("numbers/b.json"),
        "{\"id\":\"b\",\"score\":\"not-a-number\"}\n",
    )
    .unwrap();
    db().args([
        "--db",
        failed_root.to_str().unwrap(),
        "--format",
        "table",
        "check",
    ])
    .assert()
    .success();
    let manifest_before = fs::read(failed_root.join(".db/manifest.json")).unwrap();
    // The pin is the declaration a refused migration must leave alone. There is
    // no working copy to compare here: this database was established empty and
    // the table declared afterwards, so the pin is the only schema on disk.
    let schema_before = fs::read(failed_root.join("schema/numbers.json")).unwrap();
    db().args([
        "--db",
        failed_root.to_str().unwrap(),
        "--format",
        "table",
        "migrate",
        "change-type",
        "numbers",
        "score",
        "int",
    ])
    .assert()
    .code(2)
    .stderr(
        predicate::str::contains("numbers/a.json").and(predicate::str::contains("numbers/b.json")),
    );
    assert_eq!(
        manifest_before,
        fs::read(failed_root.join(".db/manifest.json")).unwrap()
    );
    assert_eq!(
        schema_before,
        fs::read(failed_root.join("schema/numbers.json")).unwrap(),
        "a refused migration leaves the pin as the user declared it"
    );
}

#[test]
fn test0020_snapshot_restore_removes_malformed_extra_authoritative_files() {
    let dir = adopted();
    let root = dir.path();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "snapshot",
        "create",
        "clean",
    ])
    .assert()
    .success();
    let stray = root.join("users/stray.json");
    fs::write(&stray, "not json\n").unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "snapshot",
        "restore",
        "clean",
        "--yes",
    ])
    .assert()
    .success();
    assert!(!stray.exists());
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
}

#[test]
fn test0021_provenance_objects_are_verified_and_corruption_is_exit_six() {
    let dir = adopted();
    let root = dir.path();
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".db/manifest.json")).unwrap()).unwrap();
    let hash = manifest["entries"][".db/config"]["hash"].as_str().unwrap();
    fs::write(root.join(format!(".db/objects/{hash}.json")), "{}\n").unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "status",
    ])
    .assert()
    .code(6)
    .stderr(predicate::str::contains("INTERNAL_METADATA_CORRUPT"));
}

#[test]
fn test0022_schema_type_specific_members_are_strict_and_format_errors_are_exit_six() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("bad")).unwrap();
    pin(
        root,
        "bad",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string","x-jdb-type":"int"}},"required":["id"],"x-jdb":{"table":"bad","primaryKey":["id"],"columnOrder":["id"]}}"#,
    );
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "status",
    ])
    .assert()
    .code(2)
    .stderr(predicate::str::contains(
        "does not agree with its type",
    ));

    pin(
        root,
        "bad",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"bad","schemaVersion":1,"schemaFormat":999,"primaryKey":["id"],"columnOrder":["id"]}}"#,
    );
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .code(6)
        .stderr(predicate::str::contains("FORMAT_UNSUPPORTED"));
}

#[test]
fn test0023_primary_key_arguments_are_decoded_against_the_declared_type() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("things")).unwrap();
    pin(
        root,
        "things",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"}},"required":["id","name"],"additionalProperties":false,"x-jdb":{"table":"things","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","name"]}}"#,
    );
    fs::write(
        root.join("things/123.json"),
        "{\"id\":\"123\",\"name\":\"numeric text\"}\n",
    )
    .unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "get",
        "things",
        "123",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("numeric text"));
}

#[test]
fn test0024_inference_derives_foreign_keys_and_rejects_invalid_pk_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("users")).unwrap();
    fs::create_dir(root.join("posts")).unwrap();
    // `users` is deliberately left unpinned: the point of this test is that
    // inference derives the relationship, and a pinned table is declared rather
    // than inferred.
    fs::write(root.join("users/u1.json"), "{\"id\":\"u1\"}\n").unwrap();
    fs::write(root.join("users/u2.json"), "{\"id\":\"u2\"}\n").unwrap();
    fs::write(
        root.join("posts/p1.json"),
        "{\"id\":\"p1\",\"user_id\":\"u1\"}\n",
    )
    .unwrap();
    fs::write(
        root.join("posts/p2.json"),
        "{\"id\":\"p2\",\"user_id\":\"u2\"}\n",
    )
    .unwrap();
    db().args([
        "--format",
        "table",
        "init",
        root.to_str().unwrap(),
        "--adopt",
    ])
    .assert()
    .success();
    let posts: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".db/schema/posts.json")).unwrap()).unwrap();
    let relational = &posts["x-jdb"];
    assert_eq!(relational["foreignKeys"][0]["columns"][0], "user_id");
    assert_eq!(relational["foreignKeys"][0]["references"]["table"], "users");
    assert_eq!(relational["indexes"][0][0], "user_id");
    // Inference records the relationship it found, not a prose account of
    // finding it: the schema is the evidence, and it is checkable.
    assert_eq!(
        relational["foreignKeys"][0]["references"]["columns"][0], "id",
        "the foreign key names the column it references"
    );
    assert_eq!(
        relational["foreignKeys"][0]["onDelete"], "restrict",
        "an inferred foreign key is conservative about deletion"
    );

    let invalid = tempfile::tempdir().unwrap();
    fs::create_dir(invalid.path().join("things")).unwrap();
    fs::write(invalid.path().join("things/a.json"), "{\"id\":\"a\"}\n").unwrap();
    db().args(["--format", "table", "infer", "things", "--pk", "missing"])
        .current_dir(invalid.path())
        .assert()
        .code(8)
        .stderr(
            predicate::str::contains("INFER_NO_PRIMARY_KEY")
                .and(predicate::str::contains("unknown column \"missing\"")),
        );
}

#[cfg(unix)]
#[test]
fn test0026_internal_symlinks_are_rejected_without_following_them() {
    use std::os::unix::fs::symlink;

    let dir = adopted();
    let root = dir.path();
    let outside = tempfile::tempdir().unwrap();
    let external_config = outside.path().join("config");
    let original = fs::read(root.join(".db/config")).unwrap();
    fs::write(&external_config, &original).unwrap();
    fs::remove_file(root.join(".db/config")).unwrap();
    symlink(&external_config, root.join(".db/config")).unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "status",
    ])
    .assert()
    .code(6)
    .stderr(predicate::str::contains("INTERNAL_METADATA_CORRUPT"));
    assert_eq!(original, fs::read(&external_config).unwrap());

    fs::remove_file(root.join(".db/config")).unwrap();
    fs::write(root.join(".db/config"), original).unwrap();
    let external_snapshot = outside.path().join("snapshot");
    fs::create_dir(&external_snapshot).unwrap();
    symlink(&external_snapshot, root.join(".db/snapshots/redirected")).unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "snapshot",
        "restore",
        "redirected",
        "--yes",
    ])
    .assert()
    .code(6)
    .stderr(predicate::str::contains("INTERNAL_METADATA_CORRUPT"));
    assert_eq!(fs::read_dir(&external_snapshot).unwrap().count(), 0);
}

#[test]
fn test0027_documented_flag_names_match_the_specified_cli_contract() {
    // Section 29 specifies `db list users [--where <expr>] [--order <col>]
    // [--limit n]`, and Section 39 specifies `db migrate add-column <t> <c>
    // --type <type>`. These long flags are part of the CLI contract, so a
    // derive attribute that silently renames one is a defect even though the
    // underlying operation still works under the wrong name.
    let dir = adopted();
    let root = dir.path().to_str().unwrap();

    db().args([
        "--db",
        root,
        "--format",
        "jsonl",
        "list",
        "users",
        "--where",
        "name = 'Bob'",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("Bob").and(predicate::str::contains("Alice").not()));

    db().args([
        "--db", root, "--format", "jsonl", "list", "users", "--order", "name", "--limit", "1",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("Alice").and(predicate::str::contains("Bob").not()));

    db().args([
        "--db",
        root,
        "--format",
        "table",
        "migrate",
        "add-column",
        "users",
        "active",
        "--type",
        "bool",
        "--default",
        "true",
    ])
    .assert()
    .success();

    let row: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join("users/u1.json")).unwrap()).unwrap();
    assert_eq!(row["active"], true);

    db().args(["--db", root, "--format", "table", "check"])
        .assert()
        .success();
}

/// Section 37: a RESTRICT referential action must block the mutation and report
/// it as a referential violation. SQLite implements RESTRICT with an internal
/// trigger, so the underlying failure arrives as SQLITE_CONSTRAINT_TRIGGER; it
/// must still be classified FOREIGN_KEY_VIOLATION (Section 75) with the INVALID
/// exit status (Section 51), never as a query type error.
#[test]
fn test0028_restrict_referential_action_blocks_and_reports_a_referential_violation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("users")).unwrap();
    fs::create_dir(root.join("posts")).unwrap();
    pin(
        root,
        "users",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"users","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id"]}}"#,
    );
    pin(
        root,
        "posts",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"user_id":{"type":"string"}},"required":["id","user_id"],"additionalProperties":false,"x-jdb":{"table":"posts","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","user_id"],"foreignKeys":[{"columns":["user_id"],"references":{"table":"users","columns":["id"]},"onDelete":"restrict","onUpdate":"restrict"}]}}"#,
    );
    fs::write(root.join("users/u1.json"), "{\"id\":\"u1\"}\n").unwrap();
    fs::write(
        root.join("posts/p1.json"),
        "{\"id\":\"p1\",\"user_id\":\"u1\"}\n",
    )
    .unwrap();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();

    for arguments in [
        vec!["delete", "users", "u1"],
        vec!["sql", "DELETE FROM users WHERE id = 'u1'"],
        vec!["update", "users", "u1", "{\"id\":\"u9\"}"],
    ] {
        let mut command = db();
        command.args(["--db", root.to_str().unwrap(), "--format", "table"]);
        command
            .args(&arguments)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("FOREIGN_KEY_VIOLATION"));
    }

    // The refusal must leave both the parent and the dependent row untouched.
    assert!(root.join("users/u1.json").exists());
    assert!(root.join("posts/p1.json").exists());
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
}

/// Section 37: set_null and set_default execute transactionally and every row
/// touched by the action is listed in the command output.
#[test]
fn test0029_set_null_and_set_default_actions_rewrite_dependents_in_one_revision() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("users")).unwrap();
    fs::create_dir(root.join("posts")).unwrap();
    pin(
        root,
        "users",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"users","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id"]}}"#,
    );
    pin(
        root,
        "posts",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"user_id":{"type":["string","null"]}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"posts","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","user_id"],"foreignKeys":[{"columns":["user_id"],"references":{"table":"users","columns":["id"]},"onDelete":"set_null","onUpdate":"restrict"}]}}"#,
    );
    fs::write(root.join("users/u1.json"), "{\"id\":\"u1\"}\n").unwrap();
    fs::write(
        root.join("posts/p1.json"),
        "{\"id\":\"p1\",\"user_id\":\"u1\"}\n",
    )
    .unwrap();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "delete",
        "users",
        "u1",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("posts/p1.json"));
    let row: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("posts/p1.json")).unwrap()).unwrap();
    assert_eq!(row["user_id"], serde_json::Value::Null);

    // set_default restores the declared default rather than null.
    pin(
        root,
        "posts",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"user_id":{"type":["string","null"],"default":"gone"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"posts","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","user_id"],"foreignKeys":[{"columns":["user_id"],"references":{"table":"users","columns":["id"]},"onDelete":"set_default","onUpdate":"restrict"}]}}"#,
    );
    fs::write(root.join("users/gone.json"), "{\"id\":\"gone\"}\n").unwrap();
    fs::write(root.join("users/u2.json"), "{\"id\":\"u2\"}\n").unwrap();
    fs::write(
        root.join("posts/p2.json"),
        "{\"id\":\"p2\",\"user_id\":\"u2\"}\n",
    )
    .unwrap();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "delete",
        "users",
        "u2",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("posts/p2.json"));
    let row: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("posts/p2.json")).unwrap()).unwrap();
    assert_eq!(row["user_id"], "gone");
}

/// Section 11: every required and cross-schema grammar rule has a dedicated
/// stable code, and SCHEMA_UNKNOWN_KEY names the nearest valid key so that a
/// typo cannot become invisible state.
#[test]
fn test0030_schema_grammar_and_semantic_rules_have_dedicated_codes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("a")).unwrap();
    fs::write(root.join("a/a1.json"), "{\"id\":\"a1\"}\n").unwrap();

    let base = r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"a","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id"]}}"#;
    let cases: Vec<(&str, &str)> = vec![
        (
            "SCHEMA_UNKNOWN_KEY",
            r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"}},"required":["id"],"x-jdb":{"table":"a","primaryKey":["id"],"columnOrder":["id"],"uniqe":[["id"]]}}"#,
        ),
        (
            "SCHEMA_PK_NULLABLE",
            r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":["string","null"]}},"additionalProperties":false,"x-jdb":{"table":"a","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id"]}}"#,
        ),
        (
            "SCHEMA_COLUMN_TYPE_MISSING",
            r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":"string"},"x-jdb":{"table":"a","primaryKey":["id"],"columnOrder":["id"]}}"#,
        ),
        (
            "SCHEMA_TYPE_UNKNOWN",
            r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"nosuchtype"}},"required":["id"],"x-jdb":{"table":"a","primaryKey":["id"],"columnOrder":["id"]}}"#,
        ),
        (
            "SCHEMA_PK_COLUMN_UNKNOWN",
            r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"a","schemaVersion":1,"primaryKey":["ghost"],"columnOrder":["id"]}}"#,
        ),
        (
            "SCHEMA_DEFAULT_TYPE_MISMATCH",
            r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"n":{"type":"integer","x-jdb-type":"int","default":"text"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"a","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","n"]}}"#,
        ),
        (
            "SCHEMA_TABLE_NAME_MISMATCH",
            r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"other","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id"]}}"#,
        ),
        (
            "SCHEMA_FK_TARGET_MISSING",
            r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"b":{"type":"string"}},"required":["id","b"],"additionalProperties":false,"x-jdb":{"table":"a","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","b"],"foreignKeys":[{"columns":["b"],"references":{"table":"ghost","columns":["id"]},"onDelete":"restrict","onUpdate":"restrict"}]}}"#,
        ),
        (
            "SCHEMA_CHECK_INVALID",
            r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"a","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id"],"checks":[{"name":"c","expr":"ghost > 0"}]}}"#,
        ),
    ];
    for (code, body) in cases {
        pin(root, "a", body);
        db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
            .assert()
            .code(2)
            .stderr(predicate::str::contains(code));
    }

    // The unknown-key message must point at the intended key, not merely reject.
    pin(
        root,
        "a",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"}},"required":["id"],"x-jdb":{"table":"a","primaryKey":["id"],"columnOrder":["id"],"uniqe":[["id"]]}}"#,
    );
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unique"));

    pin(root, "a", base);
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
}

/// Section 11: foreign-key semantics are validated across schemas.
#[test]
fn test0031_cross_schema_foreign_key_rules_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("a")).unwrap();
    fs::create_dir(root.join("b")).unwrap();
    fs::write(root.join("a/a1.json"), "{\"id\":\"a1\"}\n").unwrap();
    fs::write(root.join("b/b1.json"), "{\"id\":\"b1\",\"a_id\":\"a1\"}\n").unwrap();
    pin(
        root,
        "a",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"a","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id"]}}"#,
    );

    let cases: Vec<(&str, &str)> = vec![
        (
            "SCHEMA_FK_TYPE_MISMATCH",
            r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"a_id":{"type":"integer","x-jdb-type":"int"}},"required":["id","a_id"],"additionalProperties":false,"x-jdb":{"table":"b","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","a_id"],"foreignKeys":[{"columns":["a_id"],"references":{"table":"a","columns":["id"]},"onDelete":"restrict","onUpdate":"restrict"}]}}"#,
        ),
        (
            "SCHEMA_FK_TARGET_NOT_UNIQUE",
            r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"a_id":{"type":"string"}},"required":["id","a_id"],"additionalProperties":false,"x-jdb":{"table":"b","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","a_id"],"foreignKeys":[{"columns":["a_id"],"references":{"table":"a","columns":["nope"]},"onDelete":"restrict","onUpdate":"restrict"}]}}"#,
        ),
        (
            "SCHEMA_FK_ACTION_INVALID",
            r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"a_id":{"type":"string"}},"required":["id","a_id"],"additionalProperties":false,"x-jdb":{"table":"b","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","a_id"],"foreignKeys":[{"columns":["a_id"],"references":{"table":"a","columns":["id"]},"onDelete":"set_null","onUpdate":"restrict"}]}}"#,
        ),
    ];
    for (code, body) in cases {
        pin(root, "b", body);
        db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
            .assert()
            .code(2)
            .stderr(predicate::str::contains(code));
    }

    // An all-cascade cycle is rejected (Section 11).
    pin(
        root,
        "a",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"b_id":{"type":["string","null"]}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"a","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","b_id"],"foreignKeys":[{"columns":["b_id"],"references":{"table":"b","columns":["id"]},"onDelete":"cascade","onUpdate":"cascade"}]}}"#,
    );
    pin(
        root,
        "b",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"a_id":{"type":"string"}},"required":["id","a_id"],"additionalProperties":false,"x-jdb":{"table":"b","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","a_id"],"foreignKeys":[{"columns":["a_id"],"references":{"table":"a","columns":["id"]},"onDelete":"cascade","onUpdate":"cascade"}]}}"#,
    );
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("SCHEMA_FK_CYCLE"));
}

/// Sections 10 and 16: row-level structural and relational violations each have
/// a dedicated code, and additional_fields governs unknown keys.
#[test]
fn test0032_row_structural_and_relational_violations_have_dedicated_codes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("t")).unwrap();
    let strict = r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"n":{"type":"integer","x-jdb-type":"int"}},"required":["id","n"],"additionalProperties":false,"x-jdb":{"table":"t","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","n"]}}"#;
    pin(root, "t", strict);
    fs::write(root.join("t/a.json"), "{\"id\":\"a\",\"n\":1}\n").unwrap();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();

    let cases: Vec<(&str, &str, &str)> = vec![
        (
            "ROW_UNKNOWN_FIELD",
            "b.json",
            "{\"id\":\"b\",\"n\":1,\"emial\":\"x\"}\n",
        ),
        ("ROW_MISSING_FIELD", "c.json", "{\"id\":\"c\"}\n"),
        ("ROW_ROOT_NOT_OBJECT", "d.json", "[1,2]\n"),
        ("INVALID_JSON", "e.json", "{\"id\":\"e\",,}\n"),
        (
            "NOT_NULL_VIOLATION",
            "f.json",
            "{\"id\":\"f\",\"n\":null}\n",
        ),
        ("TYPE_MISMATCH", "g.json", "{\"id\":\"g\",\"n\":\"text\"}\n"),
    ];
    for (code, name, body) in cases {
        let path = root.join("t").join(name);
        fs::write(&path, body).unwrap();
        db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
            .assert()
            .code(2)
            .stderr(predicate::str::contains(code));
        fs::remove_file(&path).unwrap();
    }

    // additional_fields: allow accepts the very key that reject refused.
    fs::write(
        root.join("t/b.json"),
        "{\"id\":\"b\",\"n\":1,\"extra\":\"x\"}\n",
    )
    .unwrap();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("ROW_UNKNOWN_FIELD"));
    pin(
        root,
        "t",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"n":{"type":"integer","x-jdb-type":"int"}},"required":["id","n"],"additionalProperties":true,"x-jdb":{"table":"t","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","n"]}}"#,
    );
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();

    // CHECK constraints are enforced over committed rows.
    fs::remove_file(root.join("t/b.json")).unwrap();
    pin(
        root,
        "t",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"n":{"type":"integer","x-jdb-type":"int"}},"required":["id","n"],"additionalProperties":false,"x-jdb":{"table":"t","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","n"],"checks":[{"name":"pos","expr":"n > 0"}]}}"#,
    );
    fs::write(root.join("t/h.json"), "{\"id\":\"h\",\"n\":-5}\n").unwrap();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("CHECK_VIOLATION"));
}

/// Section 16: primary-key uniqueness is enforced over the logical row bodies,
/// independently of which file each row happens to live in.
#[test]
fn test0033_duplicate_primary_keys_are_rejected_under_a_custom_filename_rule() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("schema")).unwrap();
    fs::create_dir(root.join("t")).unwrap();
    pin(
        root,
        "t",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"slug":{"type":"string"}},"required":["id","slug"],"additionalProperties":false,"x-jdb":{"table":"t","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","slug"],"unique":[["slug"]],"filename":["slug"]}}"#,
    );
    fs::write(root.join("t/s1.json"), "{\"id\":\"a\",\"slug\":\"s1\"}\n").unwrap();
    fs::write(root.join("t/s2.json"), "{\"id\":\"a\",\"slug\":\"s2\"}\n").unwrap();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    // Both filenames satisfy storage.filename, so the only violation is the
    // duplicated primary key, and it must name the colliding file.
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("PRIMARY_KEY_VIOLATION")
                .and(predicate::str::contains("t/s1.json"))
                .and(predicate::str::contains("IDENTITY_MISMATCH").not()),
        );
}

/// Section 12: inference fails rather than guessing, and each failure names the
/// file and the reason.
#[test]
fn test0034_inference_failures_are_specific_and_actionable() {
    let base = tempfile::tempdir().unwrap();

    // A table whose stems match no column and that has no conventional id
    // column leaves several candidates, which must be refused, not guessed.
    let ambiguous = base.path().join("ambiguous");
    fs::create_dir_all(ambiguous.join("t")).unwrap();
    fs::write(ambiguous.join("t/r0.json"), "{\"aa\":\"1\",\"bb\":\"9\"}\n").unwrap();
    fs::write(ambiguous.join("t/r1.json"), "{\"aa\":\"2\",\"bb\":\"8\"}\n").unwrap();
    db().args([
        "--db",
        ambiguous.to_str().unwrap(),
        "--format",
        "table",
        "infer",
        "t",
    ])
    .assert()
    .code(8)
    .stderr(
        predicate::str::contains("INFER_AMBIGUOUS_PRIMARY_KEY")
            .and(predicate::str::contains("aa"))
            .and(predicate::str::contains("bb")),
    );

    // No candidate at all must explain why each column was rejected.
    let none = base.path().join("none");
    fs::create_dir_all(none.join("t")).unwrap();
    fs::write(none.join("t/1.json"), "{\"aa\":\"dup\"}\n").unwrap();
    fs::write(none.join("t/2.json"), "{\"aa\":\"dup\"}\n").unwrap();
    db().args([
        "--db",
        none.to_str().unwrap(),
        "--format",
        "table",
        "infer",
        "t",
    ])
    .assert()
    .code(8)
    .stderr(
        predicate::str::contains("INFER_NO_PRIMARY_KEY")
            .and(predicate::str::contains("duplicate value")),
    );

    // Structural refusals.
    let structural = base.path().join("structural");
    fs::create_dir_all(structural.join("t")).unwrap();
    fs::write(structural.join("t/x.json"), "{\"id\":\"x\"}\n").unwrap();

    fs::create_dir(structural.join("t/nested")).unwrap();
    db().args([
        "--db",
        structural.to_str().unwrap(),
        "--format",
        "table",
        "infer",
        "t",
    ])
    .assert()
    .code(8)
    .stderr(predicate::str::contains("INFER_NESTED_DIRECTORY"));
    fs::remove_dir(structural.join("t/nested")).unwrap();

    fs::write(structural.join("t/note.txt"), "hi\n").unwrap();
    db().args([
        "--db",
        structural.to_str().unwrap(),
        "--format",
        "table",
        "infer",
        "t",
    ])
    .assert()
    .code(8)
    .stderr(predicate::str::contains("INFER_NON_JSON_FILE"));
    fs::remove_file(structural.join("t/note.txt")).unwrap();

    fs::write(structural.join("t/bad.json"), "{bad\n").unwrap();
    db().args([
        "--db",
        structural.to_str().unwrap(),
        "--format",
        "table",
        "infer",
        "t",
    ])
    .assert()
    .code(8)
    .stderr(predicate::str::contains("INFER_INVALID_JSON"));
    fs::remove_file(structural.join("t/bad.json")).unwrap();

    fs::write(structural.join("t/y.json"), "{\"id\":\"y\",\"v\":1}\n").unwrap();
    fs::write(structural.join("t/z.json"), "{\"id\":\"z\",\"v\":\"s\"}\n").unwrap();
    db().args([
        "--db",
        structural.to_str().unwrap(),
        "--format",
        "table",
        "infer",
        "t",
    ])
    .assert()
    .code(8)
    .stderr(predicate::str::contains("INFER_TYPE_CONFLICT"));
    // Loose strictness widens the conflict to json instead of failing.
    db().args([
        "--db",
        structural.to_str().unwrap(),
        "--format",
        "table",
        "infer",
        "t",
        "--strictness",
        "loose",
    ])
    .assert()
    .success()
    // A column jdb will not commit to a narrower type admits any value, which
    // JSON Schema spells as the empty schema rather than as a type name.
    .stdout(predicate::function(|stdout: &str| {
        let document: serde_json::Value = serde_json::from_str(stdout).expect("infer prints JSON");
        document["properties"]["v"] == serde_json::json!({})
    }));
    fs::remove_file(structural.join("t/y.json")).unwrap();
    fs::remove_file(structural.join("t/z.json")).unwrap();

    // An empty table directory named explicitly cannot be typed.
    let empty = base.path().join("empty");
    fs::create_dir_all(empty.join("t")).unwrap();
    fs::create_dir(empty.join("schema")).unwrap();
    db().args(["--format", "table", "init", empty.to_str().unwrap()])
        .assert()
        .success();
    db().args([
        "--db",
        empty.to_str().unwrap(),
        "--format",
        "table",
        "infer",
        "t",
    ])
    .assert()
    .code(8)
    .stderr(
        predicate::str::contains("INFER_NO_ROWS").and(predicate::str::contains("db schema new t")),
    );

    // A column that is null in every row cannot be typed under strict.
    let untyped = base.path().join("untyped");
    fs::create_dir_all(untyped.join("t")).unwrap();
    fs::write(untyped.join("t/y.json"), "{\"id\":\"y\",\"v\":null}\n").unwrap();
    db().args([
        "--db",
        untyped.to_str().unwrap(),
        "--format",
        "table",
        "infer",
        "t",
        "--strictness",
        "strict",
    ])
    .assert()
    .code(8)
    .stderr(predicate::str::contains("INFER_UNTYPED_COLUMN"));
}

/// Section 12: an explicit --pk override is honoured, including composite keys,
/// and a key whose values do not match the file stems is reported rather than
/// silently accepted.
#[test]
fn test0035_primary_key_overrides_are_honoured_and_checked_against_filenames() {
    let base = tempfile::tempdir().unwrap();

    let composite = base.path().join("composite");
    fs::create_dir_all(composite.join("t")).unwrap();
    fs::write(composite.join("t/x,1.json"), "{\"a\":\"x\",\"b\":\"1\"}\n").unwrap();
    fs::write(composite.join("t/x,2.json"), "{\"a\":\"x\",\"b\":\"2\"}\n").unwrap();
    db().args([
        "--db",
        composite.to_str().unwrap(),
        "--format",
        "table",
        "infer",
        "t",
        "--pk",
        "a,b",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("\"a\"").and(predicate::str::contains("\"b\"")));

    let mismatched = base.path().join("mismatched");
    fs::create_dir_all(mismatched.join("t")).unwrap();
    fs::write(
        mismatched.join("t/r0.json"),
        "{\"aa\":\"1\",\"bb\":\"9\"}\n",
    )
    .unwrap();
    fs::write(
        mismatched.join("t/r1.json"),
        "{\"aa\":\"2\",\"bb\":\"8\"}\n",
    )
    .unwrap();
    db().args([
        "--db",
        mismatched.to_str().unwrap(),
        "--format",
        "table",
        "infer",
        "t",
        "--pk",
        "bb",
    ])
    .assert()
    .stderr(predicate::str::contains("INFER_FILENAME_INCONSISTENT"));
}

/// Section 54: files and directories that are neither governed rows nor ignored
/// are surfaced rather than silently absorbed.
#[test]
fn test0036_unknown_files_and_directories_are_classified() {
    let dir = adopted();
    let root = dir.path();

    // A non-.json file inside a governed table directory.
    fs::write(root.join("users/notes.txt"), "hi\n").unwrap();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("UNEXPECTED_FILE"));
    fs::remove_file(root.join("users/notes.txt")).unwrap();

    // Editor artefacts are ignored by default.
    fs::write(root.join("users/.DS_Store"), "\n").unwrap();
    fs::write(root.join("users/scratch.json~"), "\n").unwrap();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
    fs::remove_file(root.join("users/.DS_Store")).unwrap();
    fs::remove_file(root.join("users/scratch.json~")).unwrap();

    // An ungoverned top-level directory warns, and --strict promotes it.
    fs::create_dir(root.join("junk")).unwrap();
    fs::write(root.join("junk/x.json"), "{\"a\":1}\n").unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "status",
    ])
    .assert()
    .success()
    .stderr(predicate::str::contains("UNGOVERNED_DIRECTORY"));
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "check",
        "--strict",
    ])
    .assert()
    .code(7)
    .stderr(predicate::str::contains("UNGOVERNED_DIRECTORY"));
}

/// Section 53: the binary must not rewrite a row that is logically unchanged,
/// and a formatting-only external edit must not advance the revision.
#[test]
fn test0037_logically_unchanged_rows_are_never_rewritten() {
    let dir = adopted();
    let root = dir.path();
    let reformatted = "{\n    \"id\":    \"u1\",\n\n  \"name\":\"Alice\"\n}\n";
    fs::write(root.join("users/u1.json"), reformatted).unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "status",
    ])
    .assert()
    .success();
    assert_eq!(
        reformatted,
        fs::read_to_string(root.join("users/u1.json")).unwrap(),
        "validation must not normalize formatting of an unchanged row"
    );
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
    assert_eq!(
        reformatted,
        fs::read_to_string(root.join("users/u1.json")).unwrap(),
        "check must not normalize formatting of an unchanged row"
    );
}

/// Sections 6 and 13: a schema whose primary key names a column that does not
/// exist is INVALID and must be reported as SCHEMA_PK_COLUMN_UNKNOWN. The
/// diagnostic commands must keep operating on that state rather than aborting:
/// lint analyses a declared shape, so it must decline to analyse a table whose
/// primary key does not resolve instead of indexing a missing column.
#[test]
fn test0038_unresolvable_primary_key_is_reported_not_crashed_on() {
    let dir = adopted();
    let root = dir.path();
    pin(
        root,
        "users",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"}},"required":["id","name"],"additionalProperties":false,"x-jdb":{"table":"users","schemaVersion":1,"primaryKey":["ghost"],"columnOrder":["id","name"]}}"#,
    );

    // `status` and `check` report violations on stderr and exit INVALID.
    // A panic (exit 101) or a success code would both be defects.
    for arguments in [vec!["status"], vec!["check"]] {
        let mut command = db();
        command.args(["--db", root.to_str().unwrap(), "--format", "table"]);
        command
            .args(&arguments)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("SCHEMA_PK_COLUMN_UNKNOWN"));
    }

    // `doctor` prints its plan on stdout, classifying a schema error a human
    // must resolve as a `manual` fix (Section 14).
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "doctor",
    ])
    .assert()
    .code(2)
    .stdout(
        predicate::str::contains("SCHEMA_PK_COLUMN_UNKNOWN")
            .and(predicate::str::contains("manual")),
    );

    // `lint` reports lint findings only, so it says nothing about a table it
    // declines to analyse. What must hold is that it terminates cleanly rather
    // than indexing the missing column.
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "lint"])
        .assert()
        .code(0)
        .stderr(predicate::str::contains("panicked").not());

    // The same must hold when the unresolvable key belongs to a *target* table
    // that another table's foreign-key candidate scan would inspect.
    fs::create_dir(root.join("posts")).unwrap();
    fs::write(
        root.join("posts/p1.json"),
        "{\"id\":\"p1\",\"users\":\"u1\"}\n",
    )
    .unwrap();
    pin(
        root,
        "posts",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"users":{"type":"string"}},"required":["id","users"],"additionalProperties":false,"x-jdb":{"table":"posts","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","users"]}}"#,
    );
    for arguments in [vec!["check"], vec!["lint"], vec!["doctor"]] {
        let mut command = db();
        command.args(["--db", root.to_str().unwrap(), "--format", "table"]);
        command
            .args(&arguments)
            .assert()
            .code(predicate::in_iter([0, 2, 7]))
            .stderr(predicate::str::contains("panicked").not());
    }

    // Repairing the schema restores a fully valid database.
    pin(
        root,
        "users",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"}},"required":["id","name"],"additionalProperties":false,"x-jdb":{"table":"users","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","name"]}}"#,
    );
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
}

/// Section 49: `--quiet` suppresses informational output without ever
/// suppressing diagnostics, machine-readable payload, command results, or
/// changing an exit code.
#[test]
fn test0039_quiet_suppresses_only_informational_output() {
    let dir = adopted();
    let root = dir.path().to_str().unwrap();

    // Informational confirmations disappear entirely under --quiet.
    for arguments in [
        vec!["status"],
        vec!["check"],
        vec!["doctor"],
        vec!["gc", "--dry-run"],
    ] {
        let mut loud = db();
        loud.args(["--db", root, "--format", "table"]);
        let loud = loud.args(&arguments).output().unwrap();
        assert!(
            !loud.stdout.is_empty(),
            "{arguments:?} should print informational output without --quiet"
        );

        let mut hushed = db();
        hushed.args(["--db", root, "--format", "table", "--quiet"]);
        let hushed = hushed.args(&arguments).output().unwrap();
        assert!(
            hushed.stdout.is_empty(),
            "{arguments:?} must print nothing on stdout under --quiet, got {:?}",
            String::from_utf8_lossy(&hushed.stdout)
        );
        assert_eq!(
            loud.status.code(),
            hushed.status.code(),
            "--quiet must not change the exit code of {arguments:?}"
        );
    }

    // A mutation still happens under --quiet; only its summary is suppressed.
    db().args([
        "--db",
        root,
        "--format",
        "table",
        "--quiet",
        "update",
        "users",
        "u1",
        "{\"name\":\"Quietly\"}",
    ])
    .assert()
    .success()
    .stdout(predicate::str::is_empty());
    assert!(
        fs::read_to_string(dir.path().join("users/u1.json"))
            .unwrap()
            .contains("Quietly")
    );

    // Query results are the command's payload, not informational chatter.
    db().args([
        "--db",
        root,
        "--format",
        "table",
        "--quiet",
        "sql",
        "SELECT name FROM users ORDER BY name",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("Quietly"));
    db().args([
        "--db", root, "--format", "table", "--quiet", "schema", "show", "users",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("\"table\""));

    // Diagnostics and the INVALID exit code survive --quiet.
    fs::write(
        dir.path().join("users/broken.json"),
        "{\"id\":\"broken\",\"name\":4}\n",
    )
    .unwrap();
    db().args(["--db", root, "--format", "table", "--quiet", "check"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("TYPE_MISMATCH"));
    // The machine-readable contract is never suppressed either.
    db().args(["--db", root, "--format", "json", "--quiet", "check"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("TYPE_MISMATCH"));
}

/// Section 49: human diagnostics are colourised on a TTY only, and both
/// `--no-color` and a non-empty `NO_COLOR` disable it. Tests never run on a
/// TTY, so the observable contract here is that piped output is always clean.
#[test]
fn test0040_diagnostics_are_never_colourised_off_a_terminal() {
    let dir = adopted();
    let root = dir.path().to_str().unwrap();
    fs::write(
        dir.path().join("users/broken.json"),
        "{\"id\":\"broken\",\"name\":4}\n",
    )
    .unwrap();

    for extra in [vec![], vec!["--no-color"]] {
        let mut command = db();
        command.args(["--db", root, "--format", "table"]);
        let output = command.args(&extra).arg("check").output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("TYPE_MISMATCH"),
            "diagnostic must still be reported"
        );
        assert!(
            !stderr.contains('\u{1b}'),
            "redirected output must carry no ANSI escapes, got {stderr:?}"
        );
    }

    // NO_COLOR is honoured as an environment setting.
    db().args(["--db", root, "--format", "table", "check"])
        .env("NO_COLOR", "1")
        .assert()
        .code(2)
        .stderr(predicate::str::contains('\u{1b}').not());
}

/// Section 33: the writer lock admits one binary-managed writer. Concurrent
/// writers must either serialise or fail loudly with the documented code and
/// exit status -- never interleave and never silently lose an update.
#[test]
fn test0041_concurrent_writers_never_silently_lose_an_update() {
    use std::sync::mpsc;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::create_dir(root.join("counters")).unwrap();
    fs::create_dir(root.join("schema")).unwrap();
    pin(
        &root,
        "counters",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"n":{"type":"integer","x-jdb-type":"int"}},"required":["id","n"],"additionalProperties":false,"x-jdb":{"table":"counters","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","n"]}}"#,
    );
    for index in 0..8 {
        fs::write(
            root.join(format!("counters/c{index}.json")),
            format!("{{\"id\":\"c{index}\",\"n\":0}}\n"),
        )
        .unwrap();
    }
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();

    // Eight writers race, each mutating a different row.
    let (sender, receiver) = mpsc::channel();
    let mut handles = vec![];
    for index in 0..8 {
        let root = root.clone();
        let sender = sender.clone();
        handles.push(std::thread::spawn(move || {
            let output = db()
                .args([
                    "--db",
                    root.to_str().unwrap(),
                    "--format",
                    "table",
                    "update",
                    "counters",
                    &format!("c{index}"),
                    "{\"n\":1}",
                ])
                .output()
                .unwrap();
            sender.send((index, output)).unwrap();
        }));
    }
    drop(sender);
    for handle in handles {
        handle.join().unwrap();
    }

    let mut succeeded = 0;
    for (index, output) in receiver.iter() {
        let code = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if code == 0 {
            succeeded += 1;
        } else {
            // A writer that does not win must say exactly why, with the
            // documented lock/conflict exit status (Sections 34 and 51).
            assert_eq!(
                code, 3,
                "writer {index} failed with {code} and stderr {stderr}"
            );
            assert!(
                stderr.contains("CONCURRENT_MODIFICATION"),
                "writer {index} must report the conflict: {stderr}"
            );
        }
    }
    assert!(succeeded >= 1, "at least one writer must make progress");

    // Whatever interleaving occurred, the database must be valid and every
    // committed row must hold a value that was actually written -- never a
    // partially applied or torn state.
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
    let mut mutated = 0;
    for index in 0..8 {
        let row: serde_json::Value = serde_json::from_slice(
            &fs::read(root.join(format!("counters/c{index}.json"))).unwrap(),
        )
        .unwrap();
        let n = row["n"].as_i64().unwrap();
        assert!(n == 0 || n == 1, "row c{index} holds a torn value {n}");
        if n == 1 {
            mutated += 1;
        }
    }
    assert_eq!(
        mutated, succeeded,
        "every reported success must be durable on disk"
    );
}

/// Section 33: concurrent readers are always admitted and never blocked by one
/// another, and reading never mutates authoritative state.
#[test]
fn test0042_concurrent_readers_are_admitted_and_change_nothing() {
    let dir = adopted();
    let root = dir.path().to_path_buf();
    // Settle derived state first so the readers race over a steady database.
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
    let before = fs::read(root.join("users/u1.json")).unwrap();
    let manifest_before = fs::read(root.join(".db/manifest.json")).unwrap();

    // Section 46: a read-only invocation works from an in-memory observation
    // and writes no derived state, so any number of them may run at once.
    let mut handles = vec![];
    for _ in 0..8 {
        let root = root.clone();
        handles.push(std::thread::spawn(move || {
            db().args([
                "--db",
                root.to_str().unwrap(),
                "--readonly",
                "--format",
                "jsonl",
                "sql",
                "SELECT name FROM users ORDER BY name",
            ])
            .output()
            .unwrap()
        }));
    }
    for handle in handles {
        let output = handle.join().unwrap();
        assert!(
            output.status.success(),
            "a read-only reader must never be refused: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Alice") && stdout.contains("Bob"));
    }

    // A default invocation may record an externally observed revision and
    // refresh derived state, so it takes the writer lock. Racing several must
    // still never corrupt the database: each either answers or is refused with
    // the documented conflict code, and the answers are always correct.
    let mut handles = vec![];
    for _ in 0..8 {
        let root = root.clone();
        handles.push(std::thread::spawn(move || {
            db().args([
                "--db",
                root.to_str().unwrap(),
                "--format",
                "jsonl",
                "sql",
                "SELECT name FROM users ORDER BY name",
            ])
            .output()
            .unwrap()
        }));
    }
    let mut answered = 0;
    for handle in handles {
        let output = handle.join().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if output.status.success() {
            answered += 1;
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stdout.contains("Alice") && stdout.contains("Bob"));
        } else {
            assert_eq!(
                output.status.code(),
                Some(3),
                "a refused writer must report the documented conflict: {stderr}"
            );
            assert!(stderr.contains("CONCURRENT_MODIFICATION"), "{stderr}");
        }
    }
    assert!(answered >= 1, "at least one invocation must make progress");
    assert_eq!(before, fs::read(root.join("users/u1.json")).unwrap());
    assert_eq!(
        manifest_before,
        fs::read(root.join(".db/manifest.json")).unwrap(),
        "reads must not advance recorded state"
    );
}

/// Section 34: a mutation planned against one observed state must not commit if
/// the authoritative files changed underneath it. The conflict is reported, and
/// the external edit is preserved rather than overwritten.
#[test]
fn test0043_external_edits_during_a_mutation_are_not_overwritten() {
    let dir = adopted();
    let root = dir.path();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();

    // An external actor rewrites a row that the pending mutation does not touch.
    fs::write(
        root.join("users/u2.json"),
        "{\"id\":\"u2\",\"name\":\"ExternallyEdited\"}\n",
    )
    .unwrap();

    // The binary observes the new state, accepts it as a valid external
    // transition, and applies its own change on top without discarding it.
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "update",
        "users",
        "u1",
        "{\"name\":\"Updated\"}",
    ])
    .assert()
    .success();

    let u2 = fs::read_to_string(root.join("users/u2.json")).unwrap();
    assert!(
        u2.contains("ExternallyEdited"),
        "external edit must survive: {u2}"
    );
    let u1 = fs::read_to_string(root.join("users/u1.json")).unwrap();
    assert!(u1.contains("Updated"));
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
}

/// Section 62: the engine must not assume the database fits comfortably in
/// memory. A table with many rows still validates, queries, aggregates, and
/// mutates correctly.
#[test]
fn test0044_many_rows_validate_query_and_mutate_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("events")).unwrap();
    fs::create_dir(root.join("schema")).unwrap();
    pin(
        root,
        "events",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"integer","x-jdb-type":"int"},"bucket":{"type":"string"},"n":{"type":"integer","x-jdb-type":"int"}},"required":["id","bucket","n"],"additionalProperties":false,"x-jdb":{"table":"events","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","bucket","n"]}}"#,
    );

    let rows = 2_000usize;
    for index in 0..rows {
        fs::write(
            root.join(format!("events/{index}.json")),
            format!(
                "{{\"id\":{index},\"bucket\":\"b{}\",\"n\":{}}}\n",
                index % 4,
                index
            ),
        )
        .unwrap();
    }
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();

    // Aggregation over every row must be exact, not sampled.
    let expected_sum: i64 = (0..rows as i64).sum();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "sql",
        "SELECT count(*) AS c, sum(n) AS s FROM events",
    ])
    .assert()
    .success()
    .stdout(
        predicate::str::contains(format!("\"c\":{rows}"))
            .and(predicate::str::contains(format!("\"s\":{expected_sum}"))),
    );

    // Grouping must see every bucket.
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "sql",
        "SELECT bucket, count(*) AS c FROM events GROUP BY bucket ORDER BY bucket",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains(format!("\"c\":{}", rows / 4)));

    // A targeted mutation touches exactly one file out of many.
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "sql",
        "UPDATE events SET n = -1 WHERE id = 1999",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("events/1999.json"));
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
}

/// Section 61: resource limits are enforced rather than advisory, and exceeding
/// one is reported as RESOURCE_LIMIT instead of being silently truncated.
#[test]
fn test0045_result_row_limits_are_enforced_not_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("items")).unwrap();
    fs::create_dir(root.join("schema")).unwrap();
    pin(
        root,
        "items",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"integer","x-jdb-type":"int"}},"required":["id"],"additionalProperties":false,"x-jdb":{"table":"items","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id"]}}"#,
    );
    for index in 0..25 {
        fs::write(
            root.join(format!("items/{index}.json")),
            format!("{{\"id\":{index}}}\n"),
        )
        .unwrap();
    }
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();

    // Under the limit the query answers normally.
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--max-result-rows",
        "100",
        "--format",
        "jsonl",
        "sql",
        "SELECT id FROM items",
    ])
    .assert()
    .success();

    // Over the limit it fails loudly rather than returning a partial answer.
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--max-result-rows",
        "5",
        "--format",
        "jsonl",
        "sql",
        "SELECT id FROM items",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains("RESOURCE_LIMIT"));
}

/// Sections 57 and 61: governed content is untrusted input, and the nesting
/// limit that bounds it is the configured one. A structure deeper than the
/// limit is refused as RESOURCE_LIMIT; raising the limit above any parser
/// ceiling must actually admit the same structure, otherwise the limit is not
/// configurable in the sense the specification requires.
#[test]
fn test0046_pathological_json_is_refused_by_configured_limits() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("blobs")).unwrap();
    fs::create_dir(root.join("schema")).unwrap();
    pin(
        root,
        "blobs",
        r#"{"$schema":"https://jdb.dev/schema/jdb-1","type":"object","properties":{"id":{"type":"string"},"data":{}},"required":["id","data"],"additionalProperties":false,"x-jdb":{"table":"blobs","schemaVersion":1,"primaryKey":["id"],"columnOrder":["id","data"]}}"#,
    );
    let deep = format!(
        "{{\"id\":\"a\",\"data\":{}{}}}\n",
        "[".repeat(300),
        "]".repeat(300)
    );
    fs::write(root.join("blobs/a.json"), deep).unwrap();

    // Adoption of a structure deeper than the limit must fail, not recurse away.
    db().args([
        "--max-nesting-depth",
        "64",
        "--format",
        "table",
        "init",
        root.to_str().unwrap(),
        "--adopt",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains("RESOURCE_LIMIT"));

    // With a limit that accommodates it, the same file is ordinary data.
    db().args([
        "--max-nesting-depth",
        "512",
        "--format",
        "table",
        "init",
        root.to_str().unwrap(),
        "--adopt",
    ])
    .assert()
    .success();
}

/// Section 29: `export` writes every documented encoding, and the sqlite
/// encoding produces a database another tool can actually open and read.
#[test]
fn test0047_export_emits_every_documented_encoding() {
    let dir = adopted();
    let root = dir.path();

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "csv",
        "export",
        "users",
    ])
    .assert()
    .success()
    .stdout(
        predicate::str::contains("id,name")
            .and(predicate::str::contains("Alice"))
            .and(predicate::str::contains("Bob")),
    );

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "export",
        "users",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("\"kind\":\"row\""));

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "json",
        "export",
        "users",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("\"name\""));

    // sqlite is a file encoding, so it requires a destination rather than
    // writing a binary database to a terminal.
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "sqlite",
        "export",
        "users",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains("--out"));

    let out = root.join("export.db");
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "sqlite",
        "export",
        "users",
        "--out",
        out.to_str().unwrap(),
    ])
    .assert()
    .success();
    assert!(out.exists(), "the sqlite export must produce a file");
    // A real SQLite database begins with its documented header, so the file is
    // usable by other tools rather than merely present.
    let header = fs::read(&out).unwrap();
    assert!(
        header.starts_with(b"SQLite format 3\0"),
        "exported file is not a SQLite database"
    );
}

/// Section 29: `import` is transactional and accepts both documented input
/// encodings; invalid input is rejected as a whole rather than partly applied.
#[test]
fn test0048_import_is_transactional_across_input_encodings() {
    let dir = adopted();
    let root = dir.path();

    let jsonl = root.join("more.jsonl");
    fs::write(&jsonl, "{\"id\":\"u3\",\"name\":\"Carol\"}\n").unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "import",
        "users",
        "--from",
        jsonl.to_str().unwrap(),
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("users/u3.json"));
    assert!(root.join("users/u3.json").exists());

    let csv = root.join("more.csv");
    fs::write(&csv, "id,name\nu4,Dave\n").unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "import",
        "users",
        "--from",
        csv.to_str().unwrap(),
    ])
    .assert()
    .success();
    assert!(root.join("users/u4.json").exists());

    // A batch containing one invalid row commits nothing: the valid row in the
    // same file must not appear.
    let bad = root.join("bad.jsonl");
    fs::write(
        &bad,
        "{\"id\":\"u5\",\"name\":\"Eve\"}\n{\"id\":\"u6\",\"name\":4}\n",
    )
    .unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "import",
        "users",
        "--from",
        bad.to_str().unwrap(),
    ])
    .assert()
    .failure();
    assert!(
        !root.join("users/u5.json").exists(),
        "a rejected import must not leave a partially applied batch"
    );
    assert!(!root.join("users/u6.json").exists());

    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
}

/// Sections 35, 36, 64 and 66: derived state is rebuildable on demand, the
/// planner explains its choices, and the format command reports the current
/// version rather than silently upgrading.
#[test]
fn test0049_derived_state_planning_and_format_commands_operate() {
    let dir = adopted();
    let root = dir.path().to_str().unwrap();

    // Derived state rebuilds are always available and never change row data.
    db().args(["--db", root, "--format", "table", "analyze"])
        .assert()
        .success();
    db().args(["--db", root, "--format", "table", "reindex"])
        .assert()
        .success();
    assert!(dir.path().join(".db/indexes").is_dir());
    assert!(dir.path().join(".db/statistics").is_dir());

    // Section 64: the plan names the statement and reports measured rows when
    // analysis is requested.
    db().args([
        "--db",
        root,
        "--format",
        "jsonl",
        "sql",
        "--explain-analyze",
        "SELECT name FROM users ORDER BY name",
    ])
    .assert()
    .success()
    .stdout(
        predicate::str::contains("\"kind\":\"query_plan\"")
            .and(predicate::str::contains("actual_rows"))
            .and(predicate::str::contains("physical_plan")),
    );

    // `db explain` is the same plan without execution.
    db().args([
        "--db",
        root,
        "--format",
        "jsonl",
        "explain",
        "SELECT * FROM users",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("\"kind\":\"query_plan\""));

    // Section 66: format 1 is current, so upgrading is a no-op that says so.
    db().args(["--db", root, "--format", "table", "upgrade-format"])
        .assert()
        .success()
        .stdout(predicate::str::contains("format 1"));
}

/// Section 49: completions are generated for every documented shell.
#[test]
fn test0050_completions_are_generated_for_every_documented_shell() {
    for (shell, marker) in [
        ("bash", "_db"),
        ("zsh", "#compdef"),
        ("fish", "complete"),
        ("powershell", "Register-ArgumentCompleter"),
    ] {
        db().args(["completions", shell])
            .assert()
            .success()
            .stdout(predicate::str::contains(marker));
    }
    db().args(["completions", "nonesuch"]).assert().failure();
}

/// Section 29: the shell is a working REPL over the same relational layer,
/// answering its documented dot-commands and ordinary SQL.
#[test]
fn test0051_shell_answers_dot_commands_and_sql() {
    let dir = adopted();
    let root = dir.path().to_str().unwrap();
    let output = db()
        .args(["--db", root, "--format", "table", "shell"])
        .write_stdin(".tables\n.describe users\nSELECT name FROM users ORDER BY name;\n.quit\n")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "shell failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("users"),
        ".tables must list the table: {stdout}"
    );
    assert!(
        stdout.contains("\"primaryKey\""),
        ".describe must print the schema: {stdout}"
    );
    assert!(
        stdout.contains("Alice") && stdout.contains("Bob"),
        "SQL must execute in the shell: {stdout}"
    );
}

/// Zero-ceremony operation: a folder holding nothing is a legitimate state with
/// a truthful answer, not a fault the user must clear before asking anything.
#[test]
fn test0052_an_empty_folder_answers_rather_than_failing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    for arguments in [vec!["tables"], vec!["status"], vec!["check"]] {
        let mut command = db();
        command.args(["--db", root.to_str().unwrap(), "--format", "jsonl"]);
        command
            .args(&arguments)
            .assert()
            .success()
            .stdout(predicate::str::contains("EMPTY"));
    }

    // Reporting emptiness is an observation, so it creates nothing.
    assert!(!root.join(".db").exists(), "an answer is not a bootstrap");
    assert!(!root.join("schema").exists());
}

/// A query against ungoverned JSON succeeds with no prior lifecycle command:
/// initialization and inference are the binary's bookkeeping, not the user's.
#[test]
fn test0053_ungoverned_data_answers_a_query_without_ceremony() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("users")).unwrap();
    fs::write(
        root.join("users/u1.json"),
        "{\"id\":\"u1\",\"name\":\"Alice\"}\n",
    )
    .unwrap();
    fs::write(
        root.join("users/u2.json"),
        "{\"id\":\"u2\",\"name\":\"Bob\"}\n",
    )
    .unwrap();

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "sql",
        "SELECT count(*) AS n FROM users",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("\"n\":2"));

    // The folder is now a real database: metadata, a schema, and a first
    // revision recorded with no predecessor it cannot substantiate.
    assert!(root.join(".db/format").exists());
    assert!(root.join(".db/schema/users.json").exists());
    assert!(
        !root.join("schema").exists(),
        "adoption derives a schema; declaring one is the user's act"
    );
    assert_eq!(
        fs::read_dir(root.join(".db/provenance")).unwrap().count(),
        1,
        "adoption records exactly one initial revision"
    );

    // Establishment is reported, and it happens once: a second query finds
    // everything already in place.
    let second = db()
        .args([
            "--db",
            root.to_str().unwrap(),
            "--format",
            "jsonl",
            "sql",
            "SELECT count(*) AS n FROM users",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        !stdout.contains("state_transition"),
        "an established database is not re-established: {stdout}"
    );
}

/// `--no-auto` is the "do not change my prerequisites" posture CI wants: it
/// reports what would be needed and writes nothing.
#[test]
fn test0054_no_auto_reports_instead_of_establishing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("users")).unwrap();
    fs::write(root.join("users/u1.json"), "{\"id\":\"u1\"}\n").unwrap();

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--no-auto",
        "--format",
        "table",
        "tables",
    ])
    .assert()
    .code(10)
    .stderr(predicate::str::contains("UNINITIALIZED"));

    assert!(!root.join(".db").exists(), "--no-auto establishes nothing");
    assert!(!root.join("schema").exists());
}

/// Diagnosis observes; it does not change what it reports on. A command that
/// Diagnosis establishes what it needs and refreshes what it may. `.db/` is
/// reconstructible from the user's files, so a command that reports on a folder
/// is not barred from building the metadata it reports through -- what it must
/// not do is change the data it is describing.
#[test]
fn test0055_diagnostics_establish_and_refresh_but_never_touch_data() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("users")).unwrap();
    fs::write(root.join("users/u1.json"), "{\"id\":\"u1\"}\n").unwrap();
    let row_before = fs::read(root.join("users/u1.json")).unwrap();

    // On an ungoverned folder a diagnostic establishes rather than refusing:
    // it can describe the folder perfectly well once it has a model of it.
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
    assert!(root.join(".db/format").exists(), "check establishes what it needs");
    assert!(
        !root.join("schema").exists(),
        "establishing derives a schema; declaring one stays the user's act"
    );

    // Derived state it finds stale is rebuilt, because rebuilding it changes
    // nothing a user could observe except how fast the answer arrives.
    fs::write(root.join("users/u2.json"), "{\"id\":\"u2\"}\n").unwrap();
    fs::remove_dir_all(root.join(".db/indexes")).unwrap();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();
    assert!(
        fs::read_dir(root.join(".db/indexes")).unwrap().count() > 0,
        "a diagnosis refreshes derived state rather than reporting it stale forever"
    );

    // The data itself is untouched throughout.
    assert_eq!(
        row_before,
        fs::read(root.join("users/u1.json")).unwrap(),
        "a diagnosis never rewrites the rows it is describing"
    );
}

/// Working inside a table directory operates on the database that contains it,
/// so the user never has to explain where the root is.
#[test]
fn test0056_a_subdirectory_resolves_to_its_database_root() {
    let dir = adopted();
    let root = dir.path();

    let output = db()
        .args(["--format", "jsonl", "tables"])
        .current_dir(root.join("users"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("\"table\":\"users\""),
        "a subdirectory must resolve to the ancestor root"
    );
}

/// An explicitly named root is used exactly. Silently operating on an ancestor
/// database would act on data the user did not point at.
#[test]
fn test0057_an_explicit_root_is_not_walked_upward_from() {
    let dir = adopted();
    let root = dir.path();
    let inner = root.join("nested");
    fs::create_dir(&inner).unwrap();

    // The ancestor is a database, but the named path is not, and it holds
    // nothing -- so the answer is emptiness, not the ancestor's tables.
    db().args([
        "--db",
        inner.to_str().unwrap(),
        "--format",
        "jsonl",
        "tables",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("EMPTY"));
}

/// SQL given as a bare positional runs, so the common case is one word plus a
/// query rather than a subcommand the user has to remember.
#[test]
fn test0058_bare_sql_runs_without_a_subcommand() {
    let dir = adopted();
    let root = dir.path();

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "SELECT name FROM users ORDER BY name",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("Alice").and(predicate::str::contains("Bob")));

    // A subcommand name is still a subcommand: it must never be parsed as SQL,
    // which would silently run something the user did not write.
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "status",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("VALID"));
}

/// An established database does not gain tables implicitly. A directory dropped
/// beside it is surfaced for the user to adopt deliberately.
#[test]
fn test0059_established_databases_do_not_adopt_new_directories() {
    let dir = adopted();
    let root = dir.path();
    fs::create_dir(root.join("junk")).unwrap();
    fs::write(root.join("junk/x.json"), "{\"a\":1}\n").unwrap();

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "status",
    ])
    .assert()
    .success()
    .stderr(predicate::str::contains("UNGOVERNED_DIRECTORY"));

    assert!(
        !root.join("schema/junk.json").exists(),
        "an unrelated directory must not silently become a table"
    );
}

/// A dry run answers the question and reports what a real run would have
/// changed, without changing it. The plan is not decoration: it names the same
/// transitions the unprefixed invocation would perform.
#[test]
fn test0060_dry_run_plans_establishment_without_performing_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("users")).unwrap();
    fs::write(root.join("users/u1.json"), "{\"id\":\"u1\"}\n").unwrap();

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--dry-run",
        "--format",
        "table",
        "tables",
    ])
    .assert()
    .success()
    // The plan is a notice, and Section 48 keeps notices on stderr so stdout
    // stays free for results: a dry run remains pipeable.
    .stderr(
        predicate::str::contains("would initialize metadata").and(predicate::str::contains(
            "would infer .db/schema/users.json",
        )),
    )
    .stdout(predicate::str::contains("users"));

    assert!(!root.join(".db").exists(), "--dry-run establishes nothing");
    assert!(!root.join("schema").exists());
}

/// `--dry-run` promises what a real run would do; `--no-auto` refuses to do
/// anything. Together the refusal wins, so there is no plan to print: promising
/// work the invocation would never perform is worse than saying nothing.
#[test]
fn test0061_dry_run_with_no_auto_refuses_and_promises_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("users")).unwrap();
    fs::write(root.join("users/u1.json"), "{\"id\":\"u1\"}\n").unwrap();

    let out = db()
        .args([
            "--db",
            root.to_str().unwrap(),
            "--dry-run",
            "--no-auto",
            "--format",
            "table",
            "tables",
        ])
        .assert()
        .code(10)
        .stderr(predicate::str::contains("UNINITIALIZED"))
        .get_output()
        .clone();

    // A plan would surface on stderr alongside the refusal, so that is where
    // its absence has to be checked.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("would"),
        "a refused invocation plans nothing: {stderr}"
    );
    assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty());
    assert!(!root.join(".db").exists());
    assert!(!root.join("schema").exists());
}

/// Recall spans invocations: history belongs to the database rather than to
/// the process that typed it, so a session loads what earlier ones left and
/// leaves its own additions behind.
///
/// Driven through a pre-seeded file rather than a terminal: rustyline falls
/// back to a direct line reader when stdin is not a TTY, and that reader keeps
/// no history at all. Piping SQL therefore cannot exercise recall, so what is
/// pinned here is the surrounding contract -- the file is read, survives a
/// History belongs to a database, so a shell over a folder that has none leaves
/// no trace. Recall itself cannot be exercised here -- rustyline falls back to a
/// plain line reader off a TTY, and that reader keeps no history -- so what is
/// pinned is the boundary: where history is kept, and where it is not.
#[test]
fn test0062_shell_history_needs_a_database_to_belong_to() {
    // An ungoverned folder: the shell answers from an in-memory model and must
    // create nothing at all, history included.
    let bare = tempfile::tempdir().unwrap();
    fs::create_dir(bare.path().join("users")).unwrap();
    fs::write(bare.path().join("users/u1.json"), "{\"id\":\"u1\"}\n").unwrap();
    db().args([
        "--db",
        bare.path().to_str().unwrap(),
        "--no-auto",
        "--format",
        "table",
        "shell",
    ])
    .write_stdin("SELECT 1 AS n;\n.quit\n")
    .output()
    .unwrap();
    assert!(
        !bare.path().join(".db").exists(),
        "a shell that establishes nothing writes no history either"
    );

    // An established database keeps history under `.db/`, where Section 50
    // already excludes it from version control: recorded query text, literals
    // included, never reaches the repository.
    let dir = adopted();
    let root = dir.path().to_str().unwrap();
    let history = dir.path().join(".db/shell-history");
    fs::write(&history, "#V2\nSELECT name FROM users ORDER BY name\n").unwrap();
    let session = db()
        .args(["--db", root, "--format", "table", "shell"])
        .write_stdin("SELECT 7 AS seven;\n.quit\n")
        .output()
        .unwrap();
    assert!(
        session.status.success(),
        "a shell session with existing history must succeed: {}",
        String::from_utf8_lossy(&session.stderr)
    );
    let recorded = fs::read_to_string(&history).unwrap();
    assert!(
        recorded.contains("SELECT name FROM users"),
        "an earlier session's queries survive a later one: {recorded}"
    );
    let ignored = fs::read_to_string(dir.path().join(".db/.gitignore")).unwrap();
    assert!(ignored.contains('*') && !ignored.contains("!shell-history"));
}

/// `--readonly` promises the folder is not written, and a convenience file is
/// no exception. Pinned against a database that already has history, so the
/// assertion fails if the read-only gate is removed rather than passing
/// because nothing would have been written anyway.
#[test]
fn test0063_readonly_shell_records_no_history() {
    let dir = adopted();
    let root = dir.path().to_str().unwrap();
    let history = dir.path().join(".db/shell-history");
    fs::write(&history, "#V2\nSELECT name FROM users\n").unwrap();
    let before = fs::metadata(&history).unwrap().len();

    db().args(["--db", root, "--readonly", "--format", "table", "shell"])
        .write_stdin("SELECT 7 AS seven;\n.quit\n")
        .output()
        .unwrap();

    assert_eq!(
        fs::metadata(&history).unwrap().len(),
        before,
        "--readonly writes nothing, history included"
    );
}

/// A database this binary just created is complete, not convalescent. Indexes
/// are derived state, so establishment builds them: the first read must find
/// nothing to repair and say nothing about repairing it.
#[test]
fn test0064_establishment_leaves_derived_state_complete() {
    for (label, establish) in [
        ("adoption", true),
        ("implicit establishment on first query", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("users")).unwrap();
        fs::write(
            root.join("users/u1.json"),
            "{\"id\":\"u1\",\"name\":\"Alice\"}\n",
        )
        .unwrap();

        if establish {
            db().args([
                "--format",
                "table",
                "init",
                root.to_str().unwrap(),
                "--adopt",
            ])
            .assert()
            .success();
        } else {
            db().args([
                "--db",
                root.to_str().unwrap(),
                "--format",
                "jsonl",
                "sql",
                "SELECT count(*) AS n FROM users",
            ])
            .assert()
            .success();
        }

        // An index exists for the primary key, rather than the directory
        // standing empty until something notices.
        let built = fs::read_dir(root.join(".db/indexes")).unwrap().count();
        assert!(built > 0, "{label} builds indexes: none found");

        // The next read repairs nothing and announces nothing.
        let next = db()
            .args([
                "--db",
                root.to_str().unwrap(),
                "--format",
                "jsonl",
                "sql",
                "SELECT count(*) AS n FROM users",
            ])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&next.stderr);
        assert!(
            !stderr.contains("rebuilt stale or corrupt indexes"),
            "{label}: a freshly established database must not report a repair: {stderr}"
        );
        let stdout = String::from_utf8_lossy(&next.stdout);
        assert!(
            !stdout.contains("INDEX_STALE"),
            "{label}: derived state is current: {stdout}"
        );
    }
}

/// Section 50: after `git clone` the first command rebuilds derived state from
/// nothing but `.db/format`. Building indexes at establishment must not cost
/// that, because a clone carries no indexes to begin with.
#[test]
fn test0065_a_clone_rebuilds_derived_state_it_did_not_receive() {
    let dir = adopted();
    let root = dir.path();

    // What Git would carry: the tracked format marker, the schemas and the
    // rows. Everything else under `.db/` is ignored and absent on a fresh
    // clone.
    fs::remove_dir_all(root.join(".db/indexes")).unwrap();
    fs::remove_file(root.join(".db/manifest.json")).unwrap();

    let first = db()
        .args([
            "--db",
            root.to_str().unwrap(),
            "--format",
            "jsonl",
            "sql",
            "SELECT count(*) AS n FROM users",
        ])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "a clone answers its first query: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        String::from_utf8_lossy(&first.stdout).contains("\"n\":2"),
        "the query is answered from the cloned rows"
    );
    assert!(
        root.join(".db/indexes").exists(),
        "the first command rebuilds the indexes a clone never received"
    );

    // And having rebuilt them, it does not rebuild them again.
    let second = db()
        .args([
            "--db",
            root.to_str().unwrap(),
            "--format",
            "jsonl",
            "sql",
            "SELECT count(*) AS n FROM users",
        ])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&second.stderr).contains("rebuilt stale or corrupt indexes"),
        "derived state is rebuilt once, not on every read"
    );
}

/// A directory of rows sitting beside the database, governed by nothing, is
/// the one case where "no such table" has an answer rather than just a
/// refusal. `status` and `check --strict` already name the command that fixes
/// it; the query path is where the user actually hits it.
#[test]
fn test0066_an_unknown_table_names_the_ungoverned_directory_holding_it() {
    let dir = adopted();
    let root = dir.path();

    // A new directory of rows beside an established database is an ungoverned
    // sibling: Section 42 surfaces it rather than adopting it silently.
    fs::create_dir(root.join("posts")).unwrap();
    fs::write(root.join("posts/p1.json"), "{\"id\":\"p1\"}\n").unwrap();

    let query = db()
        .args([
            "--db",
            root.to_str().unwrap(),
            "--format",
            "table",
            "sql",
            "SELECT * FROM posts",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&query.stderr);
    assert!(
        stderr.contains("UNKNOWN_TABLE"),
        "an ungoverned directory is not a table: {stderr}"
    );
    assert!(
        stderr.contains("db infer posts --write"),
        "the error names the command that governs it: {stderr}"
    );

    // The advice is the same one `status` gives, because both read the same
    // observation rather than deciding separately.
    let status = db()
        .args([
            "--db",
            root.to_str().unwrap(),
            "--format",
            "table",
            "status",
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("db infer posts --write")
            || String::from_utf8_lossy(&status.stderr).contains("db infer posts --write"),
        "status and the query path agree on the remedy"
    );

    // And taking the advice works: the table is governed and answers.
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "infer",
        "posts",
        "--write",
    ])
    .assert()
    .success();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "sql",
        "SELECT count(*) AS n FROM posts",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("\"n\":1"));
}

/// Advice is only advice where it applies. A name that matches nothing on disk
/// is a typo or a dropped table, and telling the user to infer a directory
/// that is not there would send them after nothing.
#[test]
fn test0067_an_unknown_table_with_no_directory_offers_no_inference() {
    let dir = adopted();
    let root = dir.path();

    let query = db()
        .args([
            "--db",
            root.to_str().unwrap(),
            "--format",
            "table",
            "sql",
            "SELECT * FROM ghosts",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&query.stderr);
    assert!(
        stderr.contains("UNKNOWN_TABLE"),
        "the table really is unknown: {stderr}"
    );
    assert!(
        !stderr.contains("db infer"),
        "there is no directory to infer from: {stderr}"
    );
}

/// `.db/` is disposable, but only to the extent that it can be rebuilt. An
/// unlabelled metadata directory holding nothing but derived state is rebuilt
/// without ceremony; one holding recorded history is not, because discarding it
/// destroys the only copy of something the rows do not say.
#[test]
fn test0070_unlabelled_metadata_is_rebuilt_only_when_nothing_would_be_lost() {
    // Derived state alone: rebuilt silently, and the command still answers.
    let derived = tempfile::tempdir().unwrap();
    let root = derived.path();
    fs::create_dir(root.join("a")).unwrap();
    fs::write(root.join("a/a1.json"), "{\"id\":\"a1\"}\n").unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "sql",
        "SELECT 1 AS x",
    ])
    .assert()
    .success();
    fs::remove_file(root.join(".db/format")).unwrap();
    fs::remove_dir_all(root.join(".db/provenance")).unwrap();
    fs::remove_dir_all(root.join(".db/objects")).unwrap();
    fs::remove_file(root.join(".db/config")).unwrap();

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "sql",
        "SELECT count(*) AS n FROM a",
    ])
    .assert()
    .success()
    .stdout(
        predicate::str::contains("metadata_rebuilt").and(predicate::str::contains("\"n\":1")),
    );
    assert!(root.join(".db/format").exists(), "the database is re-established");

    // Recorded history: refused, naming what would be lost, and leaving it be.
    let historic = tempfile::tempdir().unwrap();
    let root = historic.path();
    fs::create_dir(root.join("a")).unwrap();
    fs::write(root.join("a/a1.json"), "{\"id\":\"a1\"}\n").unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "sql",
        "SELECT 1 AS x",
    ])
    .assert()
    .success();
    fs::remove_file(root.join(".db/format")).unwrap();
    let revisions = fs::read_dir(root.join(".db/provenance")).unwrap().count();

    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .code(6)
        .stderr(
            predicate::str::contains("FORMAT_MISSING")
                .and(predicate::str::contains("provenance"))
                .and(predicate::str::contains("--rebuild-metadata")),
        );
    assert_eq!(
        fs::read_dir(root.join(".db/provenance")).unwrap().count(),
        revisions,
        "a refusal destroys nothing"
    );

    // The flag is the authorization, and it says what it destroyed.
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--rebuild-metadata",
        "--format",
        "jsonl",
        "sql",
        "SELECT count(*) AS n FROM a",
    ])
    .assert()
    .success()
    .stdout(
        predicate::str::contains("metadata_rebuilt")
            .and(predicate::str::contains("provenance"))
            .and(predicate::str::contains("\"n\":1")),
    );
    assert!(root.join(".db/format").exists());
}

/// A dry run promises the rebuild without performing it. Destroying history is
/// exactly the operation a user would reach for `--dry-run` to preview first.
#[test]
fn test0071_a_dry_run_promises_a_metadata_rebuild_without_performing_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("a")).unwrap();
    fs::write(root.join("a/a1.json"), "{\"id\":\"a1\"}\n").unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "sql",
        "SELECT 1 AS x",
    ])
    .assert()
    .success();
    fs::remove_file(root.join(".db/format")).unwrap();
    let revisions = fs::read_dir(root.join(".db/provenance")).unwrap().count();

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--dry-run",
        "--rebuild-metadata",
        "--format",
        "table",
        "tables",
    ])
    .assert()
    .success()
    .stderr(
        predicate::str::contains("would rebuild unreadable metadata")
            .and(predicate::str::contains("provenance")),
    );

    assert_eq!(
        fs::read_dir(root.join(".db/provenance")).unwrap().count(),
        revisions,
        "a dry run destroys nothing"
    );
    assert!(
        !root.join(".db/format").exists(),
        "a dry run establishes nothing either"
    );
}

/// Section 12: a schema change applies freely when every existing row stays
/// valid under it, and is refused when one would not. The rule is about the
/// rows, not about which direction the type moved.
#[test]
fn test0072_a_schema_change_applies_only_while_every_row_stays_valid() {
    // int -> string: every value has a faithful string form, so it applies and
    // the rows are rewritten to match.
    let widening = tempfile::tempdir().unwrap();
    let root = widening.path();
    fs::create_dir(root.join("a")).unwrap();
    fs::write(root.join("a/a1.json"), "{\"id\":\"a1\",\"n\":1}\n").unwrap();
    fs::write(root.join("a/a2.json"), "{\"id\":\"a2\",\"n\":2}\n").unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "sql",
        "SELECT 1 AS x",
    ])
    .assert()
    .success();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--yes",
        "--format",
        "table",
        "migrate",
        "change-type",
        "a",
        "n",
        "string",
    ])
    .assert()
    .success();
    let row: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("a/a1.json")).unwrap()).unwrap();
    assert_eq!(row["n"], "1", "the row is carried across, not left behind");
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();

    // string -> int where one row holds text that is not a number: refused, and
    // the row is left exactly as it was.
    let lossy = tempfile::tempdir().unwrap();
    let root = lossy.path();
    fs::create_dir(root.join("a")).unwrap();
    fs::write(root.join("a/a1.json"), "{\"id\":\"a1\",\"n\":\"not-a-number\"}\n").unwrap();
    fs::write(root.join("a/a2.json"), "{\"id\":\"a2\",\"n\":\"7\"}\n").unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
        "sql",
        "SELECT 1 AS x",
    ])
    .assert()
    .success();
    let before = fs::read(root.join("a/a1.json")).unwrap();

    db().args([
        "--db",
        root.to_str().unwrap(),
        "--yes",
        "--format",
        "table",
        "migrate",
        "change-type",
        "a",
        "n",
        "int",
    ])
    .assert()
    .code(2)
    .stderr(predicate::str::contains("TYPE_MISMATCH"));

    assert_eq!(
        before,
        fs::read(root.join("a/a1.json")).unwrap(),
        "a refused migration leaves every row untouched"
    );
}

/// Section 75: the catalogue is a contract, so a code the binary can emit and
/// no document names is a gap in that contract rather than a documentation
/// chore. Checked here so adding a code without writing it down fails the
/// suite, instead of waiting for someone to notice.
#[test]
fn test0073_every_emitted_diagnostic_code_is_documented() {
    let mut emitted = std::collections::BTreeSet::new();
    let mut sources = vec![];
    let mut stack = vec![std::path::PathBuf::from("src")];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|value| value == "rs") {
                sources.push(fs::read_to_string(&path).unwrap());
            }
        }
    }
    // Codes are written as the first argument of a diagnostic constructor, so
    // the constructor name is what identifies one.
    for text in &sources {
        // Every helper that ultimately names a code, including the ones that
        // wrap `new` -- a scan that knew only the outermost constructors would
        // miss whatever a convenience method hard-codes inside itself.
        // Every route by which a code reaches a reader. Suggestions and info
        // findings are diagnostics like any other -- omitting them hid seven
        // lint codes from this check, which then passed for them by
        // construction. `bad` and `bad_at` forward a code they are given, so
        // the literal sits at their call sites rather than inside them.
        for constructor in [
            "DbError::new(",
            "Diagnostic::error(",
            "Diagnostic::warning(",
            "Diagnostic::suggestion(",
            "Diagnostic::info(",
            "Self::new(",
            "bad(",
            "bad_at(",
        ] {
            for (index, _) in text.match_indices(constructor) {
                let rest = &text[index + constructor.len()..];
                let Some(start) = rest.find('"') else { continue };
                let Some(end) = rest[start + 1..].find('"') else {
                    continue;
                };
                let code = &rest[start + 1..start + 1 + end];
                if !code.is_empty()
                    && code
                        .chars()
                        .all(|character| character.is_ascii_uppercase() || character == '_')
                {
                    emitted.insert(code.to_string());
                }
            }
        }
    }
    assert!(
        emitted.len() > 40,
        "the scan found only {} codes, so it is not finding them",
        emitted.len()
    );

    let documentation = ["docs/errors.md", "docs/validation.md", "docs/schemas.md"]
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let undocumented: Vec<_> = emitted
        .iter()
        .filter(|code| !documentation.contains(code.as_str()))
        .collect();
    assert!(
        undocumented.is_empty(),
        "these codes are emitted but documented nowhere: {undocumented:?}"
    );

    // And the reverse. A catalogue entry for a code nothing raises describes a
    // condition that cannot occur: a reader who greps for it finds nothing and
    // cannot tell whether the fault is theirs or the documentation's.
    // `SCHEMA_MISSING` sat in the table for exactly that reason, unemitted and
    // unnoticed, because this test only ever checked one direction.
    let mut catalogued = std::collections::BTreeSet::new();
    for line in documentation.lines() {
        let cells: Vec<_> = line.split('|').map(str::trim).collect();
        if cells.len() < 3 || !cells[1].starts_with('`') {
            continue;
        }
        // A row may name several related codes: `UNKNOWN_TABLE` / `UNKNOWN_ROW`.
        for part in cells[1].split('/') {
            let code = part.trim().trim_matches('`');
            if code.len() > 3
                && !code.starts_with("FIX_")
                && code
                    .chars()
                    .all(|character| character.is_ascii_uppercase() || character == '_')
            {
                catalogued.insert(code.to_string());
            }
        }
    }
    let unreachable: Vec<_> = catalogued.difference(&emitted).collect();
    assert!(
        unreachable.is_empty(),
        "these codes are documented but nothing emits them: {unreachable:?}"
    );
}

/// Ordinals exist so a test can be named in a review, a commit, or a bug
/// report and then found. A repeated number defeats that, and the compiler
/// cannot object because module paths keep the names distinct — which is
/// exactly how eleven duplicates accumulated before anyone noticed.
#[test]
fn test0074_every_test_ordinal_is_unique() {
    fn ordinals(text: &str) -> Vec<(String, String)> {
        let mut found = vec![];
        for (index, _) in text.match_indices("fn test") {
            let rest = &text[index + "fn test".len()..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if digits.len() == 4 {
                let name: String = rest
                    .chars()
                    .take_while(|character| character.is_alphanumeric() || *character == '_')
                    .collect();
                found.push((digits, name));
            }
        }
        found
    }

    let mut sources = vec![fs::read_to_string("tests/behavior.rs").unwrap()];
    let mut stack = vec![std::path::PathBuf::from("src")];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|value| value == "rs") {
                sources.push(fs::read_to_string(&path).unwrap());
            }
        }
    }

    let mut seen: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for text in &sources {
        for (ordinal, name) in ordinals(text) {
            seen.entry(ordinal).or_default().push(name);
        }
    }
    assert!(
        seen.len() > 150,
        "the scan found only {} ordinals, so it is not finding them",
        seen.len()
    );

    let repeated: Vec<_> = seen
        .iter()
        .filter(|(_, names)| names.len() > 1)
        .map(|(ordinal, names)| format!("{ordinal}: {}", names.join(", ")))
        .collect();
    assert!(
        repeated.is_empty(),
        "these ordinals name more than one test:\n  {}",
        repeated.join("\n  ")
    );
}
