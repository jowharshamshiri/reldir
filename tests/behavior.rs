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
fn test0006_direct_primary_key_update_uses_declared_cascade() {
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

#[test]
fn test0013_schema_errors_have_specific_codes_and_locations() {
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
    fs::write(
        root.join("schema/items.json"),
        r#"{"table":"items","primary_key":["id"],"columns":{"id":{"type":"string","default":"fixed"}}}"#,
    )
    .unwrap();
    fs::write(root.join("items/fixed.json"), "{}\n").unwrap();
    db().args(["--db", root.to_str().unwrap(), "--format", "table", "check"])
        .assert()
        .success();

    fs::write(
        root.join("schema/items.json"),
        r#"{"table":"items","primary_key":["id"],"columns":{"id":{"type":"string"},"tag":{"type":"string","default":"same"}},"unique":[["tag"]]}"#,
    )
    .unwrap();
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
    fs::write(
        root.join("schema/items.json"),
        r#"{"table":"items","primary_key":["id"],"columns":{"id":{"type":"string"},"count":{"type":"int"}},"check":[{"name":"positive","expr":"typo > 0"}]}"#,
    )
    .unwrap();
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
    fs::write(
        root.join("schema/numbers.json"),
        r#"{"table":"numbers","primary_key":["id"],"columns":{"id":{"type":"string"},"amount":{"type":"decimal"}}}"#,
    )
    .unwrap();
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
    fs::write(
        root.join("schema/numbers.json"),
        r#"{"table":"numbers","primary_key":["id"],"columns":{"id":{"type":"string"},"score":{"type":"string","default":"10"}}}"#,
    )
    .unwrap();
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
        serde_json::from_slice(&fs::read(root.join("schema/numbers.json")).unwrap()).unwrap();
    let omitted: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("numbers/a.json")).unwrap()).unwrap();
    let explicit: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("numbers/b.json")).unwrap()).unwrap();
    assert_eq!(schema["columns"]["score"]["type"], "int");
    assert_eq!(schema["columns"]["score"]["default"], 10);
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
    fs::write(
        failed_root.join("schema/numbers.json"),
        r#"{"table":"numbers","primary_key":["id"],"columns":{"id":{"type":"string"},"score":{"type":"string"}}}"#,
    )
    .unwrap();
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
        fs::read(failed_root.join("schema/numbers.json")).unwrap()
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
    fs::write(
        root.join("schema/bad.json"),
        r#"{"table":"bad","primary_key":["id"],"columns":{"id":{"type":"string","values":["x"]}}}"#,
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
    .code(2)
    .stderr(predicate::str::contains(
        "values is only valid for enum columns",
    ));

    fs::write(
        root.join("schema/bad.json"),
        r#"{"table":"bad","schema_format":999,"primary_key":["id"],"columns":{"id":{"type":"string"}}}"#,
    )
    .unwrap();
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
    fs::write(
        root.join("schema/things.json"),
        r#"{"table":"things","primary_key":["id"],"columns":{"id":{"type":"string"},"name":{"type":"string"}}}"#,
    )
    .unwrap();
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
fn test0024_inference_records_foreign_key_evidence_and_rejects_invalid_pk_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("users")).unwrap();
    fs::create_dir(root.join("posts")).unwrap();
    fs::create_dir(root.join("schema")).unwrap();
    fs::write(
        root.join("schema/users.json"),
        r#"{"table":"users","primary_key":["id"],"columns":{"id":{"type":"string"}}}"#,
    )
    .unwrap();
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
        serde_json::from_slice(&fs::read(root.join("schema/posts.json")).unwrap()).unwrap();
    assert_eq!(posts["foreign_keys"][0]["columns"][0], "user_id");
    assert_eq!(posts["foreign_keys"][0]["references"]["table"], "users");
    assert_eq!(posts["indexes"][0][0], "user_id");
    assert!(
        posts["inferred"]["evidence"]["foreign_keys[0]"]
            .as_str()
            .unwrap()
            .contains("users.id")
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

#[test]
fn test0025_inferred_comparison_files_are_non_authoritative_and_replaceable() {
    let dir = adopted();
    let root = dir.path();
    let manifest_before = fs::read(root.join(".db/manifest.json")).unwrap();
    db().args([
        "--db",
        root.to_str().unwrap(),
        "--format",
        "table",
        "infer",
        "--all",
        "--write",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("revision 1"));
    let proposal = root.join("schema/users.inferred.json");
    assert!(proposal.exists());
    assert_eq!(
        manifest_before,
        fs::read(root.join(".db/manifest.json")).unwrap()
    );

    fs::write(&proposal, "not json\n").unwrap();
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
        "infer",
        "--all",
        "--write",
    ])
    .assert()
    .success();
    assert!(serde_json::from_slice::<serde_json::Value>(&fs::read(&proposal).unwrap()).is_ok());
    assert_eq!(
        manifest_before,
        fs::read(root.join(".db/manifest.json")).unwrap()
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
