use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;

fn db() -> Command {
    Command::cargo_bin("db").unwrap()
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
fn adoption_query_crud_and_external_revision_are_real() {
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
fn invalid_external_state_is_rejected_without_advancing_revision() {
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
fn failed_adoption_writes_nothing() {
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
fn dry_run_does_not_mutate() {
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
fn sql_delete_executes_declared_cascade_in_one_revision() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("users")).unwrap();
    fs::create_dir(root.join("posts")).unwrap();
    fs::write(
        root.join("schema/users.json"),
        r#"{"table":"users","primary_key":["id"],"columns":{"id":{"type":"string"}}}"#,
    )
    .unwrap();
    fs::write(root.join("schema/posts.json"),r#"{"table":"posts","primary_key":["id"],"columns":{"id":{"type":"string"},"user_id":{"type":"string"}},"foreign_keys":[{"columns":["user_id"],"references":{"table":"users","columns":["id"]},"on_delete":"cascade","on_update":"cascade"}]}"#).unwrap();
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
fn direct_primary_key_update_uses_declared_cascade() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("users")).unwrap();
    fs::create_dir(root.join("posts")).unwrap();
    fs::write(
        root.join("schema/users.json"),
        r#"{"table":"users","primary_key":["id"],"columns":{"id":{"type":"string"}}}"#,
    )
    .unwrap();
    fs::write(root.join("schema/posts.json"),r#"{"table":"posts","primary_key":["id"],"columns":{"id":{"type":"string"},"user_id":{"type":"string"}},"foreign_keys":[{"columns":["user_id"],"references":{"table":"users","columns":["id"]},"on_delete":"cascade","on_update":"cascade"}]}"#).unwrap();
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
}

#[test]
fn sql_dml_rejects_silent_storage_class_coercion_and_preserves_bool_output() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    db().args(["--format", "table", "init", root.to_str().unwrap()])
        .assert()
        .success();
    fs::create_dir(root.join("items")).unwrap();
    fs::write(
        root.join("schema/items.json"),
        r#"{"table":"items","primary_key":["id"],"columns":{"id":{"type":"string"},"count":{"type":"int"},"active":{"type":"bool"}}}"#,
    )
    .unwrap();
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
        "UPDATE items SET count = ? WHERE id = 'a'",
        "--param",
        "\"2\"",
    ])
    .assert()
    .code(2)
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
}

#[test]
fn doctor_repairs_layout_without_changing_row_body() {
    let dir = adopted();
    let root = dir.path();
    let original = fs::read(root.join("users/u1.json")).unwrap();
    fs::rename(root.join("users/u1.json"), root.join("users/moved.json")).unwrap();
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
fn logical_hash_ignores_formatting_only_edits() {
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
fn named_parameters_are_bound_as_values_and_revision_diff_is_semantic() {
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
fn snapshot_restores_authoritative_config_and_rows() {
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
fn declarative_migration_is_atomic_across_schema_and_rows() {
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

#[test]
fn schema_errors_have_specific_codes_and_locations() {
    let dir = tempfile::tempdir().unwrap();
    db().args(["init", dir.path().to_str().unwrap()])
        .assert()
        .success();
    fs::write(
        dir.path().join("schema/bad.json"),
        "{\n  \"table\": \"bad\",\n  \"primary_key\": [\"id\"],\n  \"columns\": {\"id\": {}}\n}\n",
    )
    .unwrap();
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
