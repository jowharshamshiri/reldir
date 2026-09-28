//! End-to-end behavior of the binary, judged through its machine contract.
//!
//! Every test runs the real binary against a real directory and reads the
//! `command_result` envelope (`--format json`), so what is asserted is what a
//! script or an agent would see.

use assert_cmd::Command;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const DIALECT: &str = "https://reldir.dev/schema/reldir-2";

fn reldir(root: &Path) -> Command {
    let mut command = Command::cargo_bin("reldir").unwrap();
    command
        .env_remove("RELDIR_DB")
        .env("NO_COLOR", "1")
        .arg("--db")
        .arg(root);
    command
}

/// Run a command with `--format json` and return (envelope, exit status).
fn run(root: &Path, args: &[&str]) -> (Value, i32) {
    let output = reldir(root)
        .args(["--format", "json"])
        .args(args)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    let envelope: Value = serde_json::from_str(&text).unwrap_or_else(|error| {
        panic!(
            "`reldir {}` did not print an envelope ({error}):\nstdout: {text}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (envelope, output.status.code().unwrap_or(-1))
}

fn ok(root: &Path, args: &[&str]) -> Value {
    let (envelope, exit) = run(root, args);
    assert_eq!(exit, 0, "`reldir {}` failed: {envelope:#}", args.join(" "));
    envelope
}

fn codes(envelope: &Value) -> Vec<String> {
    let mut out: Vec<String> = envelope["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| d["code"].as_str().unwrap_or_default().to_string())
        .collect();
    if let Some(code) = envelope["error"]["code"].as_str() {
        out.insert(0, code.to_string());
    }
    out
}

fn diagnostic<'e>(envelope: &'e Value, code: &str) -> &'e Value {
    if envelope["error"]["code"] == code {
        return &envelope["error"];
    }
    envelope["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["code"] == code)
        .unwrap_or_else(|| panic!("no {code} in {envelope:#}"))
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn write_json(root: &Path, relative: &str, value: Value) {
    write(
        root,
        relative,
        &format!("{}\n", serde_json::to_string_pretty(&value).unwrap()),
    );
}

fn read(root: &Path, relative: &str) -> String {
    fs::read_to_string(root.join(relative)).unwrap()
}

/// A table schema: `properties` as given, `id` its string key.
fn schema(table: &str, properties: Value, required: &[&str], extension: Value) -> Value {
    let mut document = json!({
        "$schema": DIALECT,
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
        "x-reldir": { "table": table, "primaryKey": ["id"] }
    });
    if let Value::Object(extra) = extension {
        for (key, value) in extra {
            document["x-reldir"][key] = value;
        }
    }
    document
}

fn pin(root: &Path, table: &str, document: Value) {
    write_json(root, &format!("schema/{table}.json"), document);
}

/// Every file under the root except reldir's own metadata, with its bytes.
fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    everything(root)
        .into_iter()
        .filter(|(path, _)| !path.starts_with(".db"))
        .collect()
}

/// Every file under the root, `.db/` included.
fn everything(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(base: &Path, directory: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(base).unwrap().to_path_buf(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// users <- posts (user_id: restrict on delete, cascade on key change), and
/// tags referenced from an array with `remove`.
fn blog() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "users",
        schema(
            "users",
            json!({"id": {"type": "string"}, "name": {"type": "string"}}),
            &["id", "name"],
            json!({}),
        ),
    );
    pin(
        root,
        "tags",
        schema(
            "tags",
            json!({"id": {"type": "string"}}),
            &["id"],
            json!({}),
        ),
    );
    pin(
        root,
        "posts",
        schema(
            "posts",
            json!({
                "id": {"type": "string"},
                "user_id": {"type": "string"},
                "title": {"type": "string", "minLength": 1},
                "tag_ids": {"type": "array", "items": {"type": "string"}}
            }),
            &["id", "user_id", "title"],
            json!({"foreignKeys": [
                {"from": ["user_id"], "to": {"table": "users"}, "onDelete": "restrict", "onUpdate": "cascade"},
                {"from": ["tag_ids[]"], "to": {"table": "tags"}, "onDelete": "remove"}
            ]}),
        ),
    );
    write_json(root, "users/ada.json", json!({"id": "ada", "name": "Ada"}));
    write_json(root, "users/bob.json", json!({"id": "bob", "name": "Bob"}));
    write_json(root, "tags/math.json", json!({"id": "math"}));
    write_json(root, "tags/logic.json", json!({"id": "logic"}));
    write_json(
        root,
        "posts/p1.json",
        json!({"id": "p1", "user_id": "ada", "title": "Engines", "tag_ids": ["math", "logic"]}),
    );
    write_json(
        root,
        "posts/p2.json",
        json!({"id": "p2", "user_id": "bob", "title": "Notes"}),
    );
    directory
}

fn revision(root: &Path) -> u64 {
    ok(root, &["status"])["revision"].as_u64().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Establishing and reading

#[test]
fn test3001_a_folder_of_json_is_adopted_queried_and_changed() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_json(root, "users/u1.json", json!({"id": "u1", "name": "Ada"}));
    write_json(root, "users/u2.json", json!({"id": "u2", "name": "Bob"}));
    let status = ok(root, &["status"]);
    assert_eq!(status["valid"], true);
    assert_eq!(status["revision"], 1, "adoption records the first revision");
    assert!(root.join(".db/schema/users.json").exists());
    assert!(
        !root.join("schema").exists(),
        "adoption infers; it declares nothing"
    );

    let rows = ok(root, &["sql", "SELECT id, name FROM users ORDER BY id"]);
    assert_eq!(rows["records"][0]["name"], "Ada");
    assert_eq!(rows["rows"], 2);

    ok(root, &["insert", "users", r#"{"id":"u3","name":"Cy"}"#]);
    ok(root, &["update", "users", "u1", r#"{"name":"Ada L."}"#]);
    ok(root, &["delete", "users", "u2"]);
    assert_eq!(
        read(root, "users/u1.json"),
        "{\n  \"id\": \"u1\",\n  \"name\": \"Ada L.\"\n}\n"
    );
    assert!(!root.join("users/u2.json").exists());
    assert_eq!(revision(root), 4);

    // An edit made outside reldir is recorded as it is observed.
    write_json(root, "users/u4.json", json!({"id": "u4", "name": "Dee"}));
    let (status, _) = run(root, &["status"]);
    assert_eq!(status["state"], "VALID_CHANGED_EXTERNALLY");
    assert_eq!(status["revision"], 5);
    let log = ok(root, &["log"]);
    assert_eq!(log["records"][0]["origin"], "external");
}

#[test]
fn test3002_an_invalid_external_edit_is_reported_and_not_recorded() {
    let directory = blog();
    let root = directory.path();
    let before = revision(root);
    write(
        root,
        "posts/p3.json",
        "{\"id\": \"p3\", \"user_id\": \"ghost\", \"title\": \"\"}\n",
    );
    let (check, exit) = run(root, &["check"]);
    assert_eq!(exit, 2);
    assert_eq!(check["valid"], false);
    let dangling = diagnostic(&check, "FOREIGN_KEY_VIOLATION");
    assert_eq!(dangling["path"], "posts/p3.json");
    assert_eq!(dangling["pointer"], "/user_id");
    assert_eq!(dangling["location"]["line"], 1);
    let short = diagnostic(&check, "SCHEMA_VIOLATION");
    assert_eq!(
        short["pointer"], "/title",
        "the empty title breaks minLength"
    );
    assert_eq!(revision(root), before, "an invalid state is never recorded");
}

#[test]
fn test3003_failed_inference_leaves_the_folder_untouched() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(root, "things/a.json", "[1, 2]\n");
    let before = everything(root);
    let (envelope, exit) = run(root, &["status"]);
    assert_eq!(exit, 8);
    assert_eq!(envelope["error"]["code"], "INFER_ROOT_NOT_OBJECT");
    assert_eq!(everything(root), before);
}

#[test]
fn test3004_an_empty_folder_answers_empty() {
    let directory = tempfile::tempdir().unwrap();
    let status = ok(directory.path(), &["status"]);
    assert_eq!(status["state"], "EMPTY");
    assert!(!directory.path().join(".db").exists());
}

/// The false green: a mistyped `--db` used to report an empty, valid database.
#[test]
fn test3005_a_named_root_that_does_not_exist_is_an_error() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("typo");
    let (envelope, exit) = run(&missing, &["check"]);
    assert_eq!(exit, 1);
    assert_eq!(envelope["error"]["code"], "PATH_NOT_FOUND");
    assert!(!missing.exists(), "and nothing is created there");

    let file = directory.path().join("file");
    fs::write(&file, "x").unwrap();
    assert_eq!(
        run(&file, &["check"]).0["error"]["code"],
        "PATH_NOT_DIRECTORY"
    );
}

#[test]
fn test3006_the_environment_names_the_root() {
    let directory = blog();
    let output = Command::cargo_bin("reldir")
        .unwrap()
        .env("RELDIR_DB", directory.path())
        .current_dir(std::env::temp_dir())
        .args(["--format", "json", "tables"])
        .output()
        .unwrap();
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["records"].as_array().unwrap().len(), 3);
}

#[test]
fn test3007_a_subdirectory_resolves_to_its_database() {
    let directory = blog();
    ok(directory.path(), &["status"]);
    let output = Command::cargo_bin("reldir")
        .unwrap()
        .env_remove("RELDIR_DB")
        .current_dir(directory.path().join("posts"))
        .args(["--format", "json", "tables"])
        .output()
        .unwrap();
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["ok"], true, "{envelope:#}");
    assert_eq!(envelope["records"].as_array().unwrap().len(), 3);
}

#[test]
fn test3008_readonly_writes_nothing_not_even_derived_state() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_json(root, "users/u1.json", json!({"id": "u1"}));
    let before = everything(root);
    let answer = ok(
        root,
        &["--readonly", "sql", "SELECT count(*) AS n FROM users"],
    );
    assert_eq!(
        answer["records"][0]["n"], 1,
        "ungoverned data answers a query"
    );
    assert_eq!(everything(root), before);

    ok(root, &["status"]);
    write_json(root, "users/u2.json", json!({"id": "u2"}));
    let before = everything(root);
    let (check, _) = run(root, &["--readonly", "check"]);
    assert!(
        codes(&check).contains(&"METADATA_STALE_READONLY".to_string()),
        "{check:#}"
    );
    assert_eq!(everything(root), before);
    let refused = run(root, &["--readonly", "delete", "users", "u1"]);
    assert_eq!(refused.0["error"]["code"], "READ_ONLY");
}

#[test]
fn test3009_no_auto_reports_what_is_missing() {
    let directory = tempfile::tempdir().unwrap();
    write_json(directory.path(), "users/u1.json", json!({"id": "u1"}));
    let (envelope, exit) = run(directory.path(), &["--no-auto", "status"]);
    assert_eq!(exit, 10);
    assert_eq!(envelope["error"]["code"], "UNINITIALIZED");
    assert!(!directory.path().join(".db").exists());
}

#[test]
fn test3010_a_new_directory_beside_a_database_is_not_adopted() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    write_json(root, "notes/n1.json", json!({"id": "n1"}));
    let (status, _) = run(root, &["status"]);
    assert!(codes(&status).contains(&"UNGOVERNED_DIRECTORY".to_string()));
    let (query, exit) = run(root, &["sql", "SELECT * FROM notes"]);
    assert_eq!(exit, 4);
    assert!(
        query["error"]["help"]
            .as_str()
            .unwrap_or_default()
            .contains("reldir infer notes --write"),
        "{query:#}"
    );
}

#[test]
fn test3011_a_clone_without_derived_state_rebuilds_it() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    fs::remove_file(root.join(".db/mirror.sqlite")).unwrap();
    let check = ok(root, &["check"]);
    assert_eq!(check["rows"], 6);
    fs::write(root.join(".db/mirror.sqlite"), b"not a database").unwrap();
    let again = ok(root, &["check"]);
    assert!(
        again["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "mirror_rebuilt"),
        "{again:#}"
    );
}

#[test]
fn test3012_only_format_2_is_read() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    fs::write(root.join(".db/format"), "format_version = 1\n").unwrap();
    let (envelope, exit) = run(root, &["status"]);
    assert_eq!(exit, 6);
    assert_eq!(envelope["error"]["code"], "FORMAT_UNSUPPORTED");
}

#[test]
fn test3013_unlabelled_metadata_is_rebuilt_only_when_nothing_is_lost() {
    let directory = blog();
    fs::create_dir_all(directory.path().join(".db/transactions")).unwrap();
    ok(directory.path(), &["status"]);

    let historic = blog();
    let root = historic.path();
    ok(root, &["status"]);
    fs::remove_file(root.join(".db/format")).unwrap();
    let (envelope, exit) = run(root, &["status"]);
    assert_eq!(exit, 6);
    assert_eq!(envelope["error"]["code"], "FORMAT_MISSING");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("provenance")
    );
    ok(root, &["--rebuild-metadata", "status"]);
}

// ---------------------------------------------------------------------------
// Validation: the whole of JSON Schema, and what it cannot say

/// The defect that rejected every real LCAS item: an untyped `oneOf`
/// alternative naming a required member lost its `required` on the way from
/// the pin to the rules actually applied.
#[test]
fn test3020_a_pinned_schema_is_applied_exactly_as_written() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "items",
        schema(
            "items",
            json!({
                "id": {"type": "string"},
                "feedback_rules": {"type": "array", "items": {
                    "type": "object",
                    "properties": {
                        "when_choice_index": {"type": "integer"},
                        "when_selected_set": {"type": "array", "items": {"type": "integer"}},
                        "when_missing": {"type": "boolean"},
                        "message": {"type": "string"}
                    },
                    "required": ["message"],
                    "oneOf": [
                        {"required": ["when_choice_index"]},
                        {"required": ["when_selected_set"]},
                        {"required": ["when_missing"]}
                    ]
                }}
            }),
            &["id"],
            json!({}),
        ),
    );
    write_json(
        root,
        "items/i1.json",
        json!({"id": "i1", "feedback_rules": [{"when_choice_index": 1, "message": "m"}]}),
    );
    assert_eq!(ok(root, &["check"])["valid"], true);
    write_json(
        root,
        "items/i2.json",
        json!({"id": "i2", "feedback_rules": [{"when_choice_index": 1, "when_missing": true, "message": "m"}]}),
    );
    let (check, exit) = run(root, &["check"]);
    assert_eq!(exit, 2);
    let fault = diagnostic(&check, "SCHEMA_VIOLATION");
    assert_eq!(fault["pointer"], "/feedback_rules/0");
    assert!(
        fault["message"].as_str().unwrap().contains("oneOf"),
        "{fault:#}"
    );
}

#[test]
fn test3021_conditionals_formats_and_patterns_are_enforced() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut document = schema(
        "people",
        json!({
            "id": {"type": "string", "pattern": "^[a-z]+$"},
            "email": {"type": "string", "format": "email"},
            "role": {"type": "string", "enum": ["admin", "member"]},
            "team": {"type": "string"}
        }),
        &["id", "email", "role"],
        json!({}),
    );
    document["if"] = json!({"properties": {"role": {"const": "admin"}}});
    document["then"] = json!({"required": ["team"]});
    pin(root, "people", document);
    write_json(
        root,
        "people/ada.json",
        json!({"id": "ada", "email": "ada@example.org", "role": "member"}),
    );
    ok(root, &["check"]);

    for (row, why) in [
        (
            r#"{"id":"bob","email":"bob@example.org","role":"admin"}"#,
            "an admin without a team",
        ),
        (
            r#"{"id":"cy","email":"not an address","role":"member"}"#,
            "an email that is not one",
        ),
        (
            r#"{"id":"Dee","email":"d@example.org","role":"member"}"#,
            "an id that misses the pattern",
        ),
    ] {
        let (refused, exit) = run(root, &["insert", "people", row]);
        assert_eq!(exit, 2, "{why}: {refused:#}");
        assert!(
            refused["error"]["message"]
                .as_str()
                .unwrap()
                .contains("nothing was written"),
            "{why}"
        );
    }
    assert_eq!(
        files(root)
            .keys()
            .filter(|p| p.starts_with("people"))
            .count(),
        1
    );
}

#[test]
fn test3022_a_typo_in_a_schema_is_refused_by_name() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "t",
        schema(
            "t",
            json!({"id": {"type": "string", "minLenght": 1}}),
            &["id"],
            json!({}),
        ),
    );
    let (check, exit) = run(root, &["check"]);
    assert_eq!(exit, 2, "{check:#}");
    let fault = diagnostic(&check, "SCHEMA_UNKNOWN_KEY");
    assert!(
        fault["message"].as_str().unwrap().contains("minLength"),
        "{fault:#}"
    );
    assert_eq!(fault["path"], "schema/t.json");
    assert!(fault["location"]["line"].as_u64().is_some());
}

#[test]
fn test3023_checks_are_sql_and_must_be_deterministic() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "t",
        schema(
            "t",
            json!({"id": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}}}),
            &["id", "tags"],
            json!({"checks": [{"name": "tagged", "expr": "json_array_length(tags) > 0"}]}),
        ),
    );
    write_json(root, "t/a.json", json!({"id": "a", "tags": ["x"]}));
    ok(root, &["check"]);
    let (refused, _) = run(root, &["insert", "t", r#"{"id":"b","tags":[]}"#]);
    assert_eq!(refused["error"]["code"], "CHECK_VIOLATION");

    pin(
        root,
        "t",
        schema(
            "t",
            json!({"id": {"type": "string"}, "tags": {}}),
            &["id"],
            json!({"checks": [{"name": "fresh", "expr": "date('now') > '2000-01-01'"}]}),
        ),
    );
    let (check, _) = run(root, &["check"]);
    assert!(
        codes(&check).contains(&"SCHEMA_CHECK_INVALID".to_string()),
        "{check:#}"
    );
}

#[test]
fn test3024_timestamps_are_keys_as_instants() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "events",
        json!({
            "$schema": DIALECT, "type": "object",
            "properties": {"at": {"type": "string", "format": "date-time"}},
            "required": ["at"], "additionalProperties": false,
            "x-reldir": {"table": "events", "primaryKey": ["at"]}
        }),
    );
    ok(
        root,
        &["insert", "events", r#"{"at":"2026-01-01T10:00:00Z"}"#],
    );
    let (refused, exit) = run(
        root,
        &["insert", "events", r#"{"at":"2026-01-01T12:00:00+02:00"}"#],
    );
    assert_eq!(exit, 2, "{refused:#}");
    assert_eq!(refused["error"]["code"], "PRIMARY_KEY_VIOLATION");
}

#[test]
fn test3025_a_key_too_long_for_a_filename_is_refused_before_writing() {
    let directory = blog();
    let long = "x".repeat(300);
    let (refused, exit) = run(
        directory.path(),
        &[
            "insert",
            "users",
            &format!(r#"{{"id":"{long}","name":"L"}}"#),
        ],
    );
    assert_eq!(exit, 2);
    assert_eq!(refused["error"]["code"], "FILENAME_TOO_LONG");
}

#[test]
fn test3026_hostile_files_are_refused_without_being_followed() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    write(
        root,
        "users/dup.json",
        "{\"id\": \"dup\", \"name\": \"a\", \"name\": \"b\"}\n",
    );
    let (check, _) = run(root, &["check"]);
    assert!(
        codes(&check).contains(&"INVALID_JSON".to_string()),
        "duplicate members: {check:#}"
    );
    fs::remove_file(root.join("users/dup.json")).unwrap();

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/passwd", root.join("users/evil.json")).unwrap();
        let (check, _) = run(root, &["check"]);
        assert_eq!(
            diagnostic(&check, "NON_REGULAR_FILE")["path"],
            "users/evil.json"
        );
    }
}

// ---------------------------------------------------------------------------
// References

#[test]
fn test3030_a_referenced_row_cannot_be_deleted_and_the_refusal_says_where() {
    let directory = blog();
    let root = directory.path();
    let before = files(root);
    let (refused, exit) = run(root, &["delete", "users", "ada"]);
    assert_eq!(exit, 2);
    let fault = &refused["error"];
    assert_eq!(fault["code"], "FOREIGN_KEY_VIOLATION");
    assert_eq!(fault["path"], "posts/p1.json");
    assert_eq!(fault["pointer"], "/user_id");
    assert_eq!(fault["location"]["line"], 3);
    assert_eq!(files(root), before, "nothing was written");

    let (sql, exit) = run(root, &["sql", "DELETE FROM users WHERE id = 'ada'"]);
    assert_eq!(exit, 2, "SQL is held to the same rule: {sql:#}");
    assert_eq!(files(root), before);
}

#[test]
fn test3031_remove_takes_the_reference_out_of_the_array() {
    let directory = blog();
    let root = directory.path();
    let outcome = ok(root, &["delete", "tags", "math"]);
    let induced: Vec<&Value> = outcome["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "referential_action")
        .collect();
    assert_eq!(induced.len(), 1, "{outcome:#}");
    assert_eq!(induced[0]["action"], "remove");
    assert_eq!(induced[0]["path"], "posts/p1.json");
    let post: Value = serde_json::from_str(&read(root, "posts/p1.json")).unwrap();
    assert_eq!(post["tag_ids"], json!(["logic"]));
    assert_eq!(ok(root, &["check"])["valid"], true);
    assert_eq!(
        read(root, "posts/p2.json"),
        "{\n  \"id\": \"p2\",\n  \"user_id\": \"bob\",\n  \"title\": \"Notes\"\n}\n",
        "untouched"
    );
}

#[test]
fn test3032_a_key_change_is_carried_into_every_reference() {
    let directory = blog();
    let root = directory.path();
    ok(
        root,
        &["sql", "UPDATE users SET id = 'ada2' WHERE id = 'ada'"],
    );
    assert!(root.join("users/ada2.json").exists() && !root.join("users/ada.json").exists());
    let post: Value = serde_json::from_str(&read(root, "posts/p1.json")).unwrap();
    assert_eq!(post["user_id"], "ada2");
    assert_eq!(ok(root, &["check"])["valid"], true);
}

#[test]
fn test3033_cascade_deletes_follow_to_the_end_in_one_revision() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "nodes",
        schema(
            "nodes",
            json!({"id": {"type": "string"}, "parent": {"type": ["string", "null"]}}),
            &["id"],
            json!({"foreignKeys": [{"from": ["parent"], "to": {"table": "nodes"}, "onDelete": "cascade"}]}),
        ),
    );
    write_json(root, "nodes/a.json", json!({"id": "a", "parent": null}));
    write_json(root, "nodes/b.json", json!({"id": "b", "parent": "a"}));
    write_json(root, "nodes/c.json", json!({"id": "c", "parent": "b"}));
    write_json(root, "nodes/d.json", json!({"id": "d", "parent": null}));
    let before = revision(root);
    let outcome = ok(root, &["delete", "nodes", "a"]);
    assert_eq!(outcome["files"], 3);
    assert_eq!(revision(root), before + 1);
    assert!(root.join("nodes/d.json").exists());
    assert_eq!(
        files(root)
            .keys()
            .filter(|p| p.starts_with("nodes"))
            .count(),
        1
    );
}

#[test]
fn test3034_references_are_found_and_located_at_any_depth() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "lessons",
        schema(
            "lessons",
            json!({"id": {"type": "string"}}),
            &["id"],
            json!({}),
        ),
    );
    pin(
        root,
        "courses",
        schema(
            "courses",
            json!({"id": {"type": "string"}, "modules": {"type": "array", "items": {
                "type": "object",
                "properties": {"lessons": {"type": "array", "items": {
                    "type": "object", "properties": {"lesson_ref": {"type": "string"}}, "required": ["lesson_ref"]
                }}}
            }}}),
            &["id"],
            json!({"foreignKeys": [{"from": ["modules[].lessons[].lesson_ref"], "to": {"table": "lessons"}}]}),
        ),
    );
    write_json(root, "lessons/l1.json", json!({"id": "l1"}));
    write_json(
        root,
        "courses/c1.json",
        json!({"id": "c1", "modules": [{"lessons": [{"lesson_ref": "l1"}, {"lesson_ref": "gone"}]}]}),
    );
    let (check, exit) = run(root, &["check"]);
    assert_eq!(exit, 2);
    let fault = diagnostic(&check, "FOREIGN_KEY_VIOLATION");
    assert_eq!(fault["pointer"], "/modules/0/lessons/1/lesson_ref");
    assert_eq!(fault["location"]["line"], 10);
    // A query over such a database works, which it did not when an element
    // key was turned into SQLite DDL.
    let answer = ok(
        root,
        &[
            "--allow-invalid",
            "sql",
            "SELECT count(*) AS n FROM lessons",
        ],
    );
    assert_eq!(answer["records"][0]["n"], 1);
}

#[test]
fn test3035_an_identity_domain_is_one_namespace_and_a_target() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let member = |table: &str| {
        schema(
            table,
            json!({"id": {"type": "string"}}),
            &["id"],
            json!({"identityDomain": "content"}),
        )
    };
    pin(root, "objectives", member("objectives"));
    pin(root, "knowledge", member("knowledge"));
    pin(
        root,
        "items",
        schema(
            "items",
            json!({"id": {"type": "string"}, "relations": {"type": "array", "items": {
                "type": "object",
                "properties": {"relation": {"type": "string"}, "target": {"type": "string"}},
                "required": ["relation", "target"]
            }}}),
            &["id"],
            json!({"identityDomain": "content", "foreignKeys": [{"from": ["relations[].target"], "to": {"domain": "content"}}]}),
        ),
    );
    write_json(root, "objectives/o1.json", json!({"id": "o1"}));
    write_json(root, "knowledge/k1.json", json!({"id": "k1"}));
    write_json(
        root,
        "items/i1.json",
        json!({"id": "i1", "relations": [{"relation": "assesses", "target": "o1"}, {"relation": "uses", "target": "k1"}]}),
    );
    assert_eq!(ok(root, &["check"])["valid"], true);

    let (refused, exit) = run(root, &["insert", "knowledge", r#"{"id":"o1"}"#]);
    assert_eq!(exit, 2);
    assert_eq!(refused["error"]["code"], "DOMAIN_KEY_VIOLATION");
    let (refused, _) = run(root, &["delete", "objectives", "o1"]);
    assert_eq!(refused["error"]["code"], "FOREIGN_KEY_VIOLATION");
}

#[test]
fn test3036_an_acyclic_graph_refuses_a_cycle_and_names_it() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "units",
        schema(
            "units",
            json!({"id": {"type": "string"}, "requires": {"type": "array", "items": {"type": "string"}}}),
            &["id"],
            json!({
                "foreignKeys": [{"from": ["requires[]"], "to": {"table": "units"}}],
                "acyclic": [{"name": "prerequisites", "edges": ["requires[]"]}]
            }),
        ),
    );
    write_json(root, "units/a.json", json!({"id": "a", "requires": ["b"]}));
    write_json(root, "units/b.json", json!({"id": "b", "requires": []}));
    ok(root, &["check"]);
    let (refused, exit) = run(root, &["update", "units", "b", r#"{"requires":["a"]}"#]);
    assert_eq!(exit, 2);
    assert_eq!(refused["error"]["code"], "CYCLE_VIOLATION");
    assert!(
        refused["error"]["message"].as_str().unwrap().contains("->"),
        "{refused:#}"
    );
}

#[test]
fn test3037_assertions_refuse_or_warn_as_declared() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "tasks",
        schema(
            "tasks",
            json!({"id": {"type": "string"}, "owner": {"type": "string"}, "active": {"type": "boolean"}}),
            &["id", "active"],
            json!({"assertions": [
                {"name": "one_active", "query": "SELECT id FROM tasks WHERE active AND (SELECT count(*) FROM tasks WHERE active) > 1"},
                {"name": "owned", "query": "SELECT id FROM tasks WHERE owner IS NULL", "severity": "warning", "message": "nobody owns this task"}
            ]}),
        ),
    );
    write_json(root, "tasks/a.json", json!({"id": "a", "active": true}));
    let check = ok(root, &["check"]);
    let warning = diagnostic(&check, "ASSERTION_VIOLATION");
    assert_eq!(warning["severity"], "warning");
    assert_eq!(warning["message"], "nobody owns this task");
    let (refused, exit) = run(
        root,
        &["insert", "tasks", r#"{"id":"b","owner":"x","active":true}"#],
    );
    assert_eq!(exit, 2);
    assert_eq!(refused["error"]["code"], "ASSERTION_VIOLATION");
}

// ---------------------------------------------------------------------------
// Writing

/// The incident's other half: one delete rewrote 1,074 files, reordering
/// their members and writing `null` where the standard means absence.
#[test]
fn test3040_a_change_writes_only_what_it_changes_and_keeps_each_file_as_written() {
    let directory = blog();
    let root = directory.path();
    write(
        root,
        "posts/p3.json",
        "{\n  \"title\": \"Draft\",\n  \"user_id\": \"bob\",\n  \"id\": \"p3\"\n}\n",
    );
    ok(root, &["status"]);
    let before = files(root);
    ok(root, &["delete", "tags", "logic"]);
    let after = files(root);
    let changed: Vec<&PathBuf> = before
        .keys()
        .filter(|path| before.get(*path) != after.get(*path))
        .collect();
    assert_eq!(
        changed,
        [
            &PathBuf::from("posts/p1.json"),
            &PathBuf::from("tags/logic.json")
        ]
    );

    ok(root, &["update", "posts", "p3", r#"{"title":"Final"}"#]);
    assert_eq!(
        read(root, "posts/p3.json"),
        "{\n  \"title\": \"Final\",\n  \"user_id\": \"bob\",\n  \"id\": \"p3\"\n}\n",
        "members stay where the author put them, and the absent array stays absent"
    );
}

#[test]
fn test3041_a_formatting_only_edit_is_not_a_change() {
    let directory = blog();
    let root = directory.path();
    let before = revision(root);
    write(
        root,
        "users/ada.json",
        "{ \"name\" : \"Ada\",\n\n\"id\":\"ada\" }",
    );
    let status = ok(root, &["status"]);
    assert_eq!(status["revision"], before);
    assert_eq!(status["state"], "VALID");
}

#[test]
fn test3042_a_dry_run_validates_and_writes_nothing() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    let before = files(root);
    let outcome = ok(root, &["--dry-run", "delete", "tags", "math"]);
    assert_eq!(outcome["dry_run"], true);
    assert_eq!(outcome["files"], 2);
    assert!(
        outcome["summary"]
            .as_str()
            .unwrap()
            .contains("nothing was written")
    );
    assert_eq!(files(root), before);
    let (refused, exit) = run(root, &["--dry-run", "delete", "users", "ada"]);
    assert_eq!(
        exit, 2,
        "a dry run refuses what the real run would: {refused:#}"
    );
}

#[test]
fn test3043_parameters_are_values_never_sql() {
    let directory = blog();
    let root = directory.path();
    let found = ok(
        root,
        &[
            "sql",
            "SELECT id FROM users WHERE name = ?",
            "--param",
            "\"Ada\"",
        ],
    );
    assert_eq!(found["records"][0]["id"], "ada");
    let named = ok(
        root,
        &[
            "sql",
            "SELECT id FROM users WHERE name = :who",
            "--param",
            "who=\"Bob\"",
        ],
    );
    assert_eq!(named["records"][0]["id"], "bob");
    let hostile = ok(
        root,
        &[
            "sql",
            "SELECT count(*) AS n FROM users WHERE name = ?",
            "--param",
            "\"x' OR '1'='1\"",
        ],
    );
    assert_eq!(hostile["records"][0]["n"], 0);
    // A value is JSON, never guessed: `7` is a number and `"7"` text, and
    // text left unquoted is refused rather than read as whichever it looks like.
    let typed = ok(
        root,
        &[
            "sql",
            "SELECT typeof(?) AS a, typeof(?) AS b",
            "--param",
            "7",
            "--param",
            "\"7\"",
        ],
    );
    assert_eq!(typed["records"][0]["a"], "integer");
    assert_eq!(typed["records"][0]["b"], "text");
    let (refused, exit) = run(root, &["sql", "SELECT ?", "--param", "Ada"]);
    assert_eq!(exit, 1);
    assert_eq!(refused["error"]["code"], "USAGE");
    assert!(
        refused["error"]["help"]
            .as_str()
            .unwrap()
            .contains("'\"Ada\"'"),
        "{refused:#}"
    );
}

#[test]
fn test3044_sql_is_read_from_stdin_with_a_dash() {
    let directory = blog();
    let output = reldir(directory.path())
        .args(["--format", "json", "sql", "-"])
        .write_stdin("SELECT count(*) AS n FROM posts")
        .output()
        .unwrap();
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["records"][0]["n"], 2);
}

#[test]
fn test3045_only_the_documented_sql_surface_is_admitted() {
    let directory = blog();
    let root = directory.path();
    for statement in [
        "DROP TABLE users",
        "PRAGMA table_info(users)",
        "ATTACH 'x.db' AS x",
        "SELECT * FROM _reldir_files",
        "CREATE TABLE x (a)",
    ] {
        let (refused, exit) = run(root, &["sql", statement]);
        assert_eq!(exit, 4, "{statement}: {refused:#}");
        assert_eq!(refused["error"]["code"], "QUERY_UNSUPPORTED", "{statement}");
    }
    let (located, _) = run(root, &["sql", "SELECT\n  FROM users"]);
    assert_eq!(located["error"]["location"]["line"], 2);
}

#[test]
fn test3046_upserts_and_returning_work_through_validation() {
    let directory = blog();
    let root = directory.path();
    let upsert = ok(
        root,
        &[
            "sql",
            "INSERT INTO users (id, name) VALUES ('ada', 'Ada B.') ON CONFLICT (id) DO UPDATE SET name = excluded.name RETURNING id, name",
        ],
    );
    let returned: Vec<&Value> = upsert["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["kind"] == "returning")
        .collect();
    assert_eq!(returned[0]["name"], "Ada B.");
    let user: Value = serde_json::from_str(&read(root, "users/ada.json")).unwrap();
    assert_eq!(user["name"], "Ada B.");
}

#[test]
fn test3047_generated_values_fill_what_an_insert_omits() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "log",
        json!({
            "$schema": DIALECT, "type": "object",
            "properties": {"n": {"type": "integer", "x-reldir-type": "int"}, "id": {"type": "string", "format": "uuid"}, "text": {"type": "string"}},
            "required": ["n", "id", "text"], "additionalProperties": false,
            "x-reldir": {"table": "log", "primaryKey": ["n"], "generated": {"n": "sequence", "id": "uuid"}}
        }),
    );
    ok(root, &["insert", "log", r#"[{"text":"a"},{"text":"b"}]"#]);
    let rows = ok(root, &["sql", "SELECT n, id FROM log ORDER BY n"]);
    assert_eq!(rows["records"][0]["n"], 1);
    assert_eq!(rows["records"][1]["n"], 2);
    assert_eq!(rows["records"][0]["id"].as_str().unwrap().len(), 36);
}

#[test]
fn test3048_result_limits_refuse_rather_than_truncate() {
    let directory = blog();
    let (refused, exit) = run(
        directory.path(),
        &["--max-result-rows", "1", "sql", "SELECT * FROM users"],
    );
    assert_eq!(exit, 4);
    assert_eq!(refused["error"]["code"], "RESOURCE_LIMIT");
}

#[test]
fn test3049_concurrent_writers_never_lose_an_update() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    pin(
        &root,
        "counters",
        json!({
            "$schema": DIALECT, "type": "object",
            "properties": {"id": {"type": "string"}, "n": {"type": "integer", "x-reldir-type": "int"}},
            "required": ["id", "n"], "additionalProperties": false,
            "x-reldir": {"table": "counters", "primaryKey": ["id"]}
        }),
    );
    write_json(&root, "counters/c.json", json!({"id": "c", "n": 0}));
    ok(&root, &["status"]);
    let writers: Vec<_> = (0..6)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || {
                for _ in 0..3 {
                    let (envelope, exit) =
                        run(&root, &["--wait", "60", "sql", "UPDATE counters SET n = n + 1 WHERE id = 'c'"]);
                    assert_eq!(exit, 0, "writers queue for the lock and plan against what they commit to: {envelope:#}");
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let final_value = ok(&root, &["get", "counters", "c"]);
    assert_eq!(final_value["records"][0]["n"], 18, "every increment landed");
}

// ---------------------------------------------------------------------------
// Repair

#[test]
fn test3050_a_row_deleted_by_hand_is_restored_exactly_from_history() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    let original = read(root, "users/ada.json");
    fs::remove_file(root.join("users/ada.json")).unwrap();
    let (plan, _) = run(root, &["doctor"]);
    let fixes: Vec<&str> = plan["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        fixes,
        ["FIX_RESTORE_TARGET", "FIX_ORPHAN_DELETE_ROW"],
        "least destructive first"
    );
    assert_eq!(plan["records"][0]["default"], true);

    ok(root, &["doctor", "--fix", "--allow-data", "--yes"]);
    assert_eq!(read(root, "users/ada.json"), original);
    assert_eq!(ok(root, &["check"])["valid"], true);
}

#[test]
fn test3051_an_alternative_is_chosen_by_naming_it() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    fs::remove_file(root.join("tags/math.json")).unwrap();
    ok(
        root,
        &[
            "doctor",
            "--fix",
            "--allow-data",
            "--yes",
            "--only",
            "FIX_REMOVE_REFERENCE",
        ],
    );
    let post: Value = serde_json::from_str(&read(root, "posts/p1.json")).unwrap();
    assert_eq!(post["tag_ids"], json!(["logic"]));
    assert!(!root.join("tags/math.json").exists());
    assert_eq!(ok(root, &["check"])["valid"], true);
}

#[test]
fn test3052_a_misnamed_file_is_renamed_and_its_body_kept() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    fs::rename(root.join("users/bob.json"), root.join("users/robert.json")).unwrap();
    let (check, _) = run(root, &["check"]);
    assert_eq!(
        diagnostic(&check, "IDENTITY_MISMATCH")["expected"],
        "bob.json"
    );
    let body = read(root, "users/robert.json");
    ok(root, &["doctor", "--fix", "--yes"]);
    assert_eq!(read(root, "users/bob.json"), body);
    assert!(
        root.join(".db/snapshots").read_dir().unwrap().count() >= 1,
        "a snapshot guards the rename"
    );
}

#[test]
fn test3053_doctor_offers_only_repairs_that_apply() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    write(
        root,
        "users/cy.json",
        "{\"id\": \"cy\", \"nmae\": \"Cy\"}\n",
    );
    let (check, _) = run(root, &["check"]);
    let unknown = diagnostic(&check, "ROW_UNKNOWN_FIELD");
    assert_eq!(unknown["fixes"][0], "FIX_RENAME_FIELD");
    assert_eq!(unknown["expected"], "name");
    ok(root, &["doctor", "--fix", "--allow-data", "--yes"]);
    let user: Value = serde_json::from_str(&read(root, "users/cy.json")).unwrap();
    assert_eq!(user, json!({"id": "cy", "name": "Cy"}));
}

#[test]
fn test3054_lint_proposes_undeclared_references_and_doctor_declares_them() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_json(root, "authors/a1.json", json!({"id": "a1"}));
    write_json(
        root,
        "books/b1.json",
        json!({"id": "b1", "written_by": "a1"}),
    );
    ok(root, &["status"]);
    let lint = ok(root, &["lint"]);
    assert_eq!(
        diagnostic(&lint, "LINT_FK_CANDIDATE")["field"],
        "written_by"
    );
    ok(root, &["doctor", "--fix", "--only", "FIX_ADD_FK", "--yes"]);
    let (refused, _) = run(root, &["delete", "authors", "a1"]);
    assert_eq!(
        refused["error"]["code"], "FOREIGN_KEY_VIOLATION",
        "the reference is now enforced"
    );
}

#[test]
fn test3055_a_snapshot_puts_everything_back() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["snapshot", "create", "before"]);
    let before = files(root);
    ok(root, &["delete", "tags", "math"]);
    ok(root, &["insert", "users", r#"{"id":"cy","name":"Cy"}"#]);
    let (declined, exit) = run(root, &["snapshot", "restore", "before"]);
    assert_eq!(
        exit, 9,
        "restoring replaces files, which needs a decision: {declined:#}"
    );
    ok(root, &["--yes", "snapshot", "restore", "before"]);
    assert_eq!(files(root), before);
}

#[test]
fn test3056_damaged_history_is_reported_and_a_new_lineage_is_a_decision() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["insert", "users", r#"{"id":"cy","name":"Cy"}"#]);
    let object = fs::read_dir(root.join(".db/objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(&object, "{}\n").unwrap();
    fs::remove_file(root.join(".db/mirror.sqlite")).unwrap();
    let (check, exit) = run(root, &["check"]);
    assert_eq!(exit, 6, "{check:#}");
    assert!(codes(&check).contains(&"INTERNAL_METADATA_CORRUPT".to_string()));
    let (declined, _) = run(root, &["recover", "--history", "new-lineage"]);
    assert_eq!(declined["error"]["code"], "DECISION_REQUIRED");
    let begun = ok(
        root,
        &["--allow-destructive", "recover", "--history", "new-lineage"],
    );
    assert_eq!(begun["revision"], 1);
    assert!(
        root.join(".db/provenance-quarantine").exists(),
        "the old history is kept"
    );
    assert_eq!(ok(root, &["check"])["valid"], true);
}

#[test]
fn test3057_gc_removes_only_what_no_revision_needs() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    write(
        root,
        ".db/objects/0000000000000000000000000000000000000000000000000000000000000000.json",
        "{}\n",
    );
    let collected = ok(root, &["gc"]);
    assert_eq!(collected["records"].as_array().unwrap().len(), 1);
    assert_eq!(ok(root, &["check"])["valid"], true);
}

// ---------------------------------------------------------------------------
// Schemas and migrations

#[test]
fn test3060_migrations_reshape_schema_and_rows_together() {
    let directory = blog();
    let root = directory.path();
    ok(
        root,
        &[
            "migrate",
            "add-column",
            "users",
            "active",
            "--type",
            "bool",
            "--default",
            "true",
        ],
    );
    let user: Value = serde_json::from_str(&read(root, "users/ada.json")).unwrap();
    assert_eq!(user["active"], true);
    ok(
        root,
        &["migrate", "rename-column", "posts", "user_id", "author_id"],
    );
    let post: Value = serde_json::from_str(&read(root, "posts/p1.json")).unwrap();
    assert_eq!(post["author_id"], "ada");
    let pinned: Value = serde_json::from_str(&read(root, "schema/posts.json")).unwrap();
    assert_eq!(
        pinned["x-reldir"]["foreignKeys"][0]["from"][0], "author_id",
        "a pinned table's migration edits its pin"
    );
    let (refused, _) = run(root, &["--yes", "migrate", "drop-table", "users"]);
    assert_eq!(
        refused["error"]["code"], "SCHEMA_FK_TARGET_MISSING",
        "{refused:#}"
    );
    assert!(root.join("users/ada.json").exists());
}

#[test]
fn test3061_a_migration_file_applies_all_or_nothing() {
    let directory = blog();
    let root = directory.path();
    let before = files(root);
    let migration = tempfile::NamedTempFile::new().unwrap();
    fs::write(
        migration.path(),
        serde_json::to_vec(&json!({"operations": [
            {"op": "add_column", "table": "users", "column": "age", "type": "int", "nullable": true},
            {"op": "change_type", "table": "users", "column": "name", "type": "int"}
        ]}))
        .unwrap(),
    )
    .unwrap();
    let (refused, exit) = run(
        root,
        &["migrate", "apply", migration.path().to_str().unwrap()],
    );
    assert_eq!(exit, 2, "{refused:#}");
    assert_eq!(refused["error"]["code"], "TYPE_MISMATCH");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("operation 2")
    );
    assert_eq!(files(root), before);
}

#[test]
fn test3062_inference_meets_an_existing_schema_by_decision() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_json(root, "users/u1.json", json!({"id": "u1", "name": "A"}));
    ok(root, &["status"]);
    ok(
        root,
        &[
            "migrate",
            "add-column",
            "users",
            "age",
            "--type",
            "string",
            "--nullable",
        ],
    );
    // The rows now say `age` is an integer; the schema says string.
    write_json(
        root,
        "users/u2.json",
        json!({"id": "u2", "name": "B", "age": 3}),
    );
    let compared = ok(
        root,
        &[
            "infer",
            "users",
            "--write",
            "--on-schema-conflict",
            "compare",
        ],
    );
    let differences: Vec<&str> = compared["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["pointer"].as_str())
        .collect();
    assert!(
        differences.iter().any(|p| p.starts_with("/properties/age")),
        "{compared:#}"
    );
    assert_eq!(compared["summary"], "compared; nothing was written");
    let (failed, _) = run(
        root,
        &["infer", "users", "--write", "--on-schema-conflict", "fail"],
    );
    assert_eq!(failed["error"]["code"], "SCHEMA_CONFLICT");
    let (asked, exit) = run(root, &["infer", "users", "--write"]);
    assert_eq!(
        exit, 9,
        "with nobody to ask, a decision is required: {asked:#}"
    );
}

#[test]
fn test3063_pinning_makes_the_inferred_schema_a_declaration() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_json(root, "users/u1.json", json!({"id": "u1"}));
    ok(root, &["schema", "pin", "users"]);
    assert!(root.join("schema/users.json").exists());
    assert!(!root.join(".db/schema/users.json").exists());
    fs::remove_dir_all(root.join(".db")).unwrap();
    assert_eq!(
        ok(root, &["check"])["valid"],
        true,
        "a pin survives the loss of .db"
    );
}

// ---------------------------------------------------------------------------
// Output

#[test]
fn test3070_failures_and_successes_share_one_envelope() {
    let directory = blog();
    let root = directory.path();
    let success = ok(root, &["get", "users", "ada"]);
    for key in [
        "kind",
        "command",
        "ok",
        "exit",
        "summary",
        "records",
        "diagnostics",
        "events",
    ] {
        assert!(success.get(key).is_some(), "{key} in {success:#}");
    }
    let (failure, exit) = run(root, &["get", "users", "nobody"]);
    assert_eq!(exit, 4);
    assert_eq!(failure["ok"], false);
    assert_eq!(failure["error"]["code"], "UNKNOWN_ROW");
}

#[test]
fn test3071_jsonl_streams_records_then_the_result() {
    let directory = blog();
    let output = reldir(directory.path())
        .args([
            "--format",
            "jsonl",
            "sql",
            "SELECT id FROM users ORDER BY id",
        ])
        .output()
        .unwrap();
    let lines: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.iter().filter(|l| l["kind"] == "row").count(), 2);
    assert_eq!(lines.last().unwrap()["kind"], "command_result");
}

#[test]
fn test3072_sarif_serves_code_scanning() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    write(
        root,
        "posts/p9.json",
        "{\"id\": \"p9\", \"user_id\": \"ghost\", \"title\": \"t\"}\n",
    );
    let output = reldir(root)
        .args(["--format", "sarif", "check"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let log: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(log["version"], "2.1.0");
    let result = &log["runs"][0]["results"][0];
    assert_eq!(result["ruleId"], "FOREIGN_KEY_VIOLATION");
    assert_eq!(
        result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
        "posts/p9.json"
    );
}

#[test]
fn test3073_an_invalid_database_answers_queries_only_when_asked() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    write(
        root,
        "posts/p9.json",
        "{\"id\": \"p9\", \"user_id\": \"ghost\", \"title\": \"t\"}\n",
    );
    let (refused, exit) = run(root, &["sql", "SELECT count(*) AS n FROM posts"]);
    assert_eq!(exit, 2);
    assert_eq!(refused["error"]["code"], "FOREIGN_KEY_VIOLATION");
    let answered = ok(
        root,
        &["--allow-invalid", "sql", "SELECT count(*) AS n FROM posts"],
    );
    assert_eq!(answered["records"][0]["n"], 3);
    assert_eq!(answered["database_valid"], false);
    assert_eq!(
        run(root, &["--allow-invalid", "sql", "DELETE FROM posts"]).1,
        1
    );
}

#[test]
fn test3074_export_and_import_round_trip() {
    let directory = blog();
    let root = directory.path();
    let scratch = tempfile::tempdir().unwrap();
    let out = scratch.path().join("users.jsonl");
    ok(root, &["export", "users", "--out", out.to_str().unwrap()]);
    let (again, _) = run(root, &["export", "users", "--out", out.to_str().unwrap()]);
    assert_eq!(again["error"]["code"], "USAGE", "export never overwrites");

    let fresh = tempfile::tempdir().unwrap();
    pin(
        fresh.path(),
        "users",
        serde_json::from_str(&read(root, "schema/users.json")).unwrap(),
    );
    ok(
        fresh.path(),
        &["import", "users", "--from", out.to_str().unwrap()],
    );
    assert_eq!(
        ok(fresh.path(), &["sql", "SELECT count(*) AS n FROM users"])["records"][0]["n"],
        2
    );
    let (duplicate, exit) = run(
        fresh.path(),
        &["import", "users", "--from", out.to_str().unwrap()],
    );
    assert_eq!(exit, 2, "{duplicate:#}");
    assert_eq!(
        ok(fresh.path(), &["sql", "SELECT count(*) AS n FROM users"])["records"][0]["n"],
        2,
        "all or nothing"
    );
}

#[test]
fn test3075_the_shell_answers_dot_commands_and_sql() {
    let directory = blog();
    let output = reldir(directory.path())
        .arg("shell")
        .write_stdin(".tables\nSELECT count(*) AS n\n  FROM users;\n.quit\n")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("posts") && text.contains("users"), "{text}");
    assert!(text.lines().any(|line| line.trim() == "2"), "{text}");
}

#[test]
fn test3076_completions_exist_for_every_shell() {
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let output = Command::cargo_bin("reldir")
            .unwrap()
            .args(["completions", shell])
            .output()
            .unwrap();
        assert!(
            output.status.success() && !output.stdout.is_empty(),
            "{shell}"
        );
    }
}

#[test]
fn test3077_the_dialect_needs_no_database() {
    let directory = tempfile::tempdir().unwrap();
    let dialect = ok(directory.path(), &["schema", "dialect"]);
    assert_eq!(dialect["records"][0]["document"]["$id"], DIALECT);
    assert!(!directory.path().join(".db").exists());
}
