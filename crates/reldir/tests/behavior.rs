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

const DIALECT: &str = "https://reldir.dev/schema/reldir-3";

fn reldir(root: &Path) -> Command {
    let mut command = Command::cargo_bin("reldir").unwrap();
    command
        .env_remove("RELDIR_DB")
        .env("NO_COLOR", "1")
        .arg("--db")
        .arg(root);
    command
}

/// The machine protocol's schema, as `reldir protocol schema` prints it.
fn protocol() -> &'static jsonschema::Validator {
    static PROTOCOL: std::sync::OnceLock<jsonschema::Validator> = std::sync::OnceLock::new();
    PROTOCOL.get_or_init(|| {
        let output = Command::cargo_bin("reldir")
            .unwrap()
            .args(["--format", "json", "protocol", "schema"])
            .output()
            .unwrap();
        let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
        jsonschema::validator_for(&envelope["records"][0]["document"]).unwrap()
    })
}

/// Run a command with `--format json` and return (envelope, exit status).
/// Every envelope is held to the protocol's schema.
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
    let faults: Vec<String> = protocol()
        .iter_errors(&envelope)
        .map(|error| format!("{} at {}", error, error.instance_path()))
        .collect();
    assert!(
        faults.is_empty(),
        "`reldir {}` printed an envelope outside the protocol: {faults:#?}\n{envelope:#}",
        args.join(" ")
    );
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
        "x-reldir": { "table": table, "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}} }
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
    ok(root, &["status"])["data"]["revision"]["seq"]
        .as_u64()
        .unwrap_or(0)
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
    assert_eq!(status["data"]["valid"], true);
    assert_eq!(
        status["data"]["revision"]["seq"], 1,
        "adoption records the first revision"
    );
    assert!(root.join(".db/schema/users.json").exists());
    assert!(
        !root.join("schema").exists(),
        "adoption infers; it declares nothing"
    );

    let rows = ok(root, &["sql", "SELECT id, name FROM users ORDER BY id"]);
    assert_eq!(rows["records"][0]["row"]["name"], "Ada");
    assert_eq!(rows["data"]["rows"], 2);

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
    assert_eq!(status["data"]["state"], "VALID_CHANGED_EXTERNALLY");
    assert_eq!(status["data"]["revision"]["seq"], 5);
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
    assert_eq!(check["data"]["valid"], false);
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
    assert_eq!(status["data"]["state"], "EMPTY");
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
        answer["records"][0]["row"]["n"], 1,
        "ungoverned data answers a query"
    );
    assert_eq!(everything(root), before);

    ok(root, &["status"]);
    write_json(root, "users/u2.json", json!({"id": "u2"}));
    let before = everything(root);
    let (check, _) = run(root, &["--readonly", "check"]);
    assert_eq!(
        check["data"]["state"], "VALID_UNRECORDED",
        "a valid change read-only access could not record is said to be unrecorded: {check:#}"
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
    assert_eq!(check["data"]["rows"], 6);
    for part in ["mirror.sqlite", "mirror.sqlite-wal", "mirror.sqlite-shm"] {
        fs::write(root.join(".db").join(part), b"not a database").unwrap();
    }
    let again = ok(root, &["check"]);
    assert!(
        again["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"] == "mirror_rebuilt"),
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
            .contains("history"),
        "names what would be lost: {envelope:#}"
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
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);
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
            "x-reldir": {"table": "events", "identity": {"source": "body", "columns": ["at"], "naming": {"rule": "reldir"}}}
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
        .filter(|e| e["event"] == "referential_action")
        .collect();
    assert_eq!(induced.len(), 1, "{outcome:#}");
    assert_eq!(induced[0]["action"], "remove");
    assert_eq!(induced[0]["path"], "posts/p1.json");
    let post: Value = serde_json::from_str(&read(root, "posts/p1.json")).unwrap();
    assert_eq!(post["tag_ids"], json!(["logic"]));
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);
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
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);
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
    assert_eq!(outcome["data"]["files"], 3);
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
    assert_eq!(answer["records"][0]["row"]["n"], 1);
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
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);

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
    assert_eq!(status["data"]["revision"]["seq"], before);
    assert_eq!(status["data"]["state"], "VALID");
}

#[test]
fn test3042_a_dry_run_validates_and_writes_nothing() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    let before = files(root);
    let outcome = ok(root, &["--dry-run", "delete", "tags", "math"]);
    assert_eq!(outcome["data"]["dry_run"], true);
    assert_eq!(outcome["data"]["files"], 2);
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
    assert_eq!(found["records"][0]["row"]["id"], "ada");
    let named = ok(
        root,
        &[
            "sql",
            "SELECT id FROM users WHERE name = :who",
            "--param",
            "who=\"Bob\"",
        ],
    );
    assert_eq!(named["records"][0]["row"]["id"], "bob");
    let hostile = ok(
        root,
        &[
            "sql",
            "SELECT count(*) AS n FROM users WHERE name = ?",
            "--param",
            "\"x' OR '1'='1\"",
        ],
    );
    assert_eq!(hostile["records"][0]["row"]["n"], 0);
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
    assert_eq!(typed["records"][0]["row"]["a"], "integer");
    assert_eq!(typed["records"][0]["row"]["b"], "text");
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
    assert_eq!(envelope["records"][0]["row"]["n"], 2);
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
        "SELECT * FROM sqlite_master",
        "SELECT load_extension('x')",
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
        .filter(|r| r["type"] == "row")
        .collect();
    assert_eq!(returned[0]["row"]["name"], "Ada B.");
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
            "x-reldir": {"table": "log", "identity": {"source": "body", "columns": ["n"], "naming": {"rule": "reldir"}}, "generated": {"n": "sequence", "id": "uuid"}}
        }),
    );
    ok(root, &["insert", "log", r#"[{"text":"a"},{"text":"b"}]"#]);
    let rows = ok(root, &["sql", "SELECT n, id FROM log ORDER BY n"]);
    assert_eq!(rows["records"][0]["row"]["n"], 1);
    assert_eq!(rows["records"][1]["row"]["n"], 2);
    assert_eq!(rows["records"][0]["row"]["id"].as_str().unwrap().len(), 36);
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
            "x-reldir": {"table": "counters", "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}}}
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
    assert_eq!(
        final_value["records"][0]["row"]["n"], 18,
        "every increment landed"
    );
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
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);
}

#[test]
fn test3121_the_snapshot_doctor_takes_puts_everything_back() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    fs::remove_file(root.join("tags/math.json")).unwrap();
    let post = read(root, "posts/p1.json");
    let (faulty, _) = run(root, &["check"]);
    let fault = diagnostic(&faulty, "FOREIGN_KEY_VIOLATION").clone();
    let fixed = ok(
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
    let taken = fixed["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event"] == "snapshot")
        .expect("doctor takes a snapshot before it rewrites rows");
    let name = taken["name"].as_str().unwrap();
    assert_ne!(read(root, "posts/p1.json"), post);
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);

    ok(root, &["snapshot", "restore", name, "--yes"]);
    assert_eq!(read(root, "posts/p1.json"), post, "byte for byte");
    let (restored, _) = run(root, &["check"]);
    assert_eq!(
        diagnostic(&restored, "FOREIGN_KEY_VIOLATION"),
        &fault,
        "the fault comes back with the state, and no other"
    );
}

#[test]
fn test3122_tables_are_renamed_and_dropped_with_their_folders() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    let before = revision(root);
    let math = read(root, "tags/math.json");

    let renamed = ok(
        root,
        &["migrate", "rename-table", "tags", "labels", "--yes"],
    );
    assert_eq!(revision(root), before + 1, "{renamed:#}");
    assert!(!root.join("tags").exists(), "the old folder goes");
    assert_eq!(
        read(root, "labels/math.json"),
        math,
        "rows move byte for byte"
    );
    let post: Value = serde_json::from_str(&read(root, "posts/p1.json")).unwrap();
    assert_eq!(post["tag_ids"], json!(["math", "logic"]));
    let checked = ok(root, &["check"]);
    assert_eq!(checked["data"]["valid"], true);
    assert!(
        checked["diagnostics"].as_array().unwrap().is_empty(),
        "{checked:#}"
    );
    // The reference follows the table it names.
    let (dangling, exit) = run(root, &["update", "posts", "p1", r#"{"tag_ids": ["nope"]}"#]);
    assert_ne!(exit, 0);
    assert_eq!(dangling["error"]["code"], "FOREIGN_KEY_VIOLATION");

    // A table other rows still name is not dropped from under them.
    let (refused, exit) = run(root, &["migrate", "drop-table", "users", "--yes"]);
    assert_ne!(exit, 0, "{refused:#}");
    assert!(root.join("users/ada.json").exists());

    ok(root, &["migrate", "drop-table", "posts", "--yes"]);
    assert_eq!(revision(root), before + 2);
    assert!(!root.join("posts").exists());
    let checked = ok(root, &["check"]);
    assert_eq!(checked["data"]["valid"], true);
    assert!(
        checked["diagnostics"].as_array().unwrap().is_empty(),
        "{checked:#}"
    );
    let tables = ok(root, &["tables"]);
    let names: Vec<&str> = tables["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|table| table["table"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["labels", "users"]);
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
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);
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
    let snapshots = ok(root, &["snapshot", "list"]);
    assert!(
        !snapshots["records"].as_array().unwrap().is_empty(),
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
    // Damage every pack: any one of them may hold the head's objects.
    for entry in fs::read_dir(root.join(".db/objects/pack")).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "pack")
        {
            let mut bytes = fs::read(&path).unwrap();
            let middle = bytes.len() / 2;
            bytes[middle] ^= 0xff;
            fs::write(&path, bytes).unwrap();
        }
    }
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
    assert_eq!(begun["data"]["revision"]["seq"], 1);
    assert!(
        root.join(".db/history/quarantine")
            .read_dir()
            .unwrap()
            .next()
            .is_some(),
        "the old history is kept"
    );
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);
}

#[test]
fn test3057_gc_removes_only_what_no_revision_needs() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    // A snapshot of an invalid state keeps objects no revision has: the file
    // that is not a row, and the tree that holds it.
    write(root, "users/broken.json", "{ not json");
    ok(root, &["snapshot", "create", "before-fix"]);
    fs::remove_file(root.join("users/broken.json")).unwrap();
    let kept = ok(root, &["--dry-run", "gc"]);
    assert_eq!(
        kept["data"]["dropped"], 0,
        "the snapshot keeps its objects alive"
    );
    ok(root, &["--yes", "snapshot", "delete", "before-fix"]);
    let collected = ok(root, &["gc"]);
    assert!(
        collected["data"]["dropped"].as_u64().unwrap() >= 1,
        "{collected:#}"
    );
    assert_eq!(
        ok(root, &["--dry-run", "gc"])["data"]["dropped"],
        0,
        "nothing is left to collect"
    );
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);
    assert!(!ok(root, &["log"])["records"].as_array().unwrap().is_empty());
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
        ok(root, &["check"])["data"]["valid"],
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
        "type",
        "protocol",
        "reldir",
        "command",
        "ok",
        "exit",
        "summary",
        "data",
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
    assert_eq!(lines.iter().filter(|l| l["type"] == "row").count(), 2);
    assert_eq!(lines.last().unwrap()["type"], "result");
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
    assert_eq!(answered["records"][0]["row"]["n"], 3);
    assert_eq!(answered["data"]["database_valid"], false);
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
        ok(fresh.path(), &["sql", "SELECT count(*) AS n FROM users"])["records"][0]["row"]["n"],
        2
    );
    let (duplicate, exit) = run(
        fresh.path(),
        &["import", "users", "--from", out.to_str().unwrap()],
    );
    assert_eq!(exit, 2, "{duplicate:#}");
    assert_eq!(
        ok(fresh.path(), &["sql", "SELECT count(*) AS n FROM users"])["records"][0]["row"]["n"],
        2,
        "all or nothing"
    );
}

#[test]
fn test3075_the_shell_answers_dot_commands_and_sql() {
    let directory = blog();
    let script = ".tables\nSELECT count(*) AS n\n  FROM users;\nSELECT ';' AS semi;\n.quit\n";
    let output = reldir(directory.path())
        .args(["--format", "table", "shell"])
        .write_stdin(script)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("posts") && text.contains("users"), "{text}");
    assert!(
        text.lines().any(|line| line.trim() == "2"),
        "a statement spans lines: {text}"
    );
    assert!(
        text.lines().any(|line| line.trim() == ";"),
        "a `;` inside a string does not end the statement: {text}"
    );

    // A program driving the shell gets the line protocol.
    let output = reldir(directory.path())
        .args(["--format", "jsonl", "shell"])
        .write_stdin(script)
        .output()
        .unwrap();
    let lines: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        lines
            .iter()
            .any(|line| line["type"] == "row" && line["row"]["n"] == 2)
    );
    assert_eq!(
        lines.iter().filter(|line| line["type"] == "result").count(),
        3,
        "one result per statement or dot-command"
    );
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

// ---------------------------------------------------------------------------
// Writing back only what changed

/// The rows of one statement's `RETURNING` clause.
fn returned(envelope: &Value) -> Vec<Value> {
    envelope["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["type"] == "row")
        .map(|record| record["row"].clone())
        .collect()
}

#[test]
fn test3080_an_update_rewrites_only_the_members_it_changes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    // Written by hand: four-space indentation, members in no particular
    // order, an offset timestamp, an integer beyond 64 bits, a decomposed
    // accent, and a float spelled as a whole number.
    let original = "{\n    \"name\": \"cafe\u{301}\",\n    \"id\": \"a\",\n    \"amount\": 5,\n    \"at\": \"2024-01-01T10:00:00+02:00\",\n    \"big\": 12345678901234567890123,\n    \"ratio\": 1.0,\n    \"count\": 1\n}\n";
    write(root, "items/a.json", original);
    write(
        root,
        "items/b.json",
        "{\n    \"name\": \"tea\",\n    \"id\": \"b\",\n    \"amount\": 2,\n    \"at\": \"2024-01-02T10:00:00Z\",\n    \"big\": 1,\n    \"ratio\": 0.5,\n    \"count\": 1\n}\n",
    );
    ok(root, &["status"]);

    ok(root, &["sql", "UPDATE items SET amount = 7 WHERE id = 'a'"]);
    assert_eq!(
        read(root, "items/a.json"),
        original.replace("\"amount\": 5", "\"amount\": 7"),
        "every member the update did not change keeps its exact bytes"
    );

    // Assigning a member the value it already holds changes nothing.
    let before = files(root);
    ok(
        root,
        &[
            "sql",
            "UPDATE items SET at = '2024-01-01T08:00:00Z', ratio = 1 WHERE id = 'a'",
        ],
    );
    assert_eq!(
        files(root),
        before,
        "the same instant and the same number are no change"
    );

    // Two rows sharing a value is no reason to refuse an increment: adoption
    // does not invent a uniqueness the folder never promised.
    ok(root, &["sql", "UPDATE items SET count = count + 1"]);
    assert_eq!(
        read(root, "items/a.json"),
        original
            .replace("\"amount\": 5", "\"amount\": 7")
            .replace("\"count\": 1", "\"count\": 2")
    );
}

#[test]
fn test3081_sql_null_and_absent_follow_the_contract() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "notes",
        schema(
            "notes",
            json!({
                "id": {"type": "string"},
                "text": {"type": "string"},
                "remark": {"type": ["string", "null"]},
                "tag": {"type": "string"},
                "status": {"type": "string", "default": "open"},
                "ratio": {"type": "number", "x-reldir-type": "float"}
            }),
            &["id", "text"],
            json!({}),
        ),
    );
    write(
        root,
        "notes/a.json",
        "{\n  \"id\": \"a\",\n  \"text\": \"x\",\n  \"remark\": \"r\",\n  \"tag\": \"t\"\n}\n",
    );
    ok(root, &["status"]);

    // A member that admits null is set to null; an optional one that does
    // not is removed; `absent()` removes any member outright.
    ok(root, &["sql", "UPDATE notes SET remark = NULL, tag = NULL"]);
    assert_eq!(
        read(root, "notes/a.json"),
        "{\n  \"id\": \"a\",\n  \"text\": \"x\",\n  \"remark\": null\n}\n"
    );
    ok(root, &["sql", "UPDATE notes SET remark = absent()"]);
    assert_eq!(
        read(root, "notes/a.json"),
        "{\n  \"id\": \"a\",\n  \"text\": \"x\"\n}\n"
    );

    // A default is what a row reads when it leaves the member out; an update
    // of another member does not write it into the file.
    ok(root, &["sql", "UPDATE notes SET text = 'y'"]);
    assert_eq!(
        read(root, "notes/a.json"),
        "{\n  \"id\": \"a\",\n  \"text\": \"y\"\n}\n"
    );
    let status = ok(root, &["sql", "SELECT status FROM notes"]);
    assert_eq!(status["records"][0]["row"]["status"], "open");

    // An insert writes the members it names, a named NULL as the contract
    // reads it, and nothing it does not name.
    ok(
        root,
        &[
            "sql",
            "INSERT INTO notes (id, text, remark, tag) VALUES ('b', 'z', NULL, NULL)",
        ],
    );
    assert_eq!(
        serde_json::from_str::<Value>(&read(root, "notes/b.json")).unwrap(),
        json!({"id": "b", "text": "z", "remark": null})
    );

    // A float is written as one, whatever the SQL value looked like.
    ok(root, &["sql", "UPDATE notes SET ratio = 3 WHERE id = 'b'"]);
    assert!(
        read(root, "notes/b.json").contains("\"ratio\": 3.0"),
        "{}",
        read(root, "notes/b.json")
    );

    // A required member cannot be emptied either way.
    let (refused, exit) = run(
        root,
        &["sql", "UPDATE notes SET text = absent() WHERE id = 'b'"],
    );
    assert_ne!(exit, 0, "{refused:#}");
    let (refused, exit) = run(
        root,
        &["sql", "UPDATE notes SET text = NULL WHERE id = 'b'"],
    );
    assert_ne!(exit, 0, "{refused:#}");
}

#[test]
fn test3082_returning_sees_the_values_generated_for_a_new_row() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "log",
        json!({
            "$schema": DIALECT, "type": "object",
            "properties": {"n": {"type": "integer", "x-reldir-type": "int"}, "id": {"type": "string", "format": "uuid"}, "text": {"type": "string"}},
            "required": ["n", "id", "text"], "additionalProperties": false,
            "x-reldir": {"table": "log", "identity": {"source": "body", "columns": ["n"], "naming": {"rule": "reldir"}}, "generated": {"n": "sequence", "id": "uuid"}}
        }),
    );
    let inserted = ok(
        root,
        &[
            "sql",
            "INSERT INTO log (text) VALUES ('a'), ('b') RETURNING n, id, text",
        ],
    );
    let rows = returned(&inserted);
    assert_eq!(rows.len(), 2);
    for (expected, row) in [1, 2].into_iter().zip(&rows) {
        assert_eq!(row["n"], expected);
        let file: Value =
            serde_json::from_str(&read(root, &format!("log/{expected}.json"))).unwrap();
        assert_eq!(
            file["id"], row["id"],
            "RETURNING shows the value the file holds"
        );
        assert_eq!(file["text"], row["text"]);
    }
    let next = ok(
        root,
        &["sql", "INSERT INTO log (text) VALUES ('c') RETURNING n"],
    );
    assert_eq!(
        returned(&next)[0]["n"],
        3,
        "a sequence continues from the largest it holds"
    );
}

// ---------------------------------------------------------------------------
// The observed contract

fn events<'e>(envelope: &'e Value, kind: &str) -> Vec<&'e Value> {
    envelope["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|event| event["event"] == kind)
        .collect()
}

#[test]
fn test3083_outside_edits_beyond_an_inferred_contract_warn_and_are_never_imitated() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_json(root, "users/a.json", json!({"id": "a", "name": "Ann"}));
    write_json(root, "users/b.json", json!({"id": "b", "name": "Bob"}));
    ok(root, &["status"]);
    let contract = read(root, ".db/schema/users.json");

    // Another program writes a member the folder never had.
    write_json(
        root,
        "users/c.json",
        json!({"id": "c", "name": "Cy", "nick": "cc"}),
    );
    let status = ok(root, &["status"]);
    assert_eq!(status["data"]["valid"], true, "{status:#}");
    let exceeded = diagnostic(&status, "CONTRACT_EXCEEDED");
    assert_eq!(exceeded["path"], "users/c.json");
    assert_eq!(exceeded["pointer"], "/nick");
    assert_eq!(
        read(root, ".db/schema/users.json"),
        contract,
        "nothing widens by itself"
    );

    // reldir keeps working, and keeps what the other program wrote.
    ok(
        root,
        &["sql", "UPDATE users SET name = 'Cee' WHERE id = 'c'"],
    );
    assert_eq!(
        serde_json::from_str::<Value>(&read(root, "users/c.json")).unwrap(),
        json!({"id": "c", "name": "Cee", "nick": "cc"})
    );

    // But it does not write such a member itself.
    let before = files(root);
    let (refused, exit) = run(
        root,
        &["insert", "users", r#"{"id":"d","name":"Dee","nick":"dd"}"#],
    );
    assert_eq!(exit, 2, "{refused:#}");
    let refusal = diagnostic(&refused, "CONTRACT_WIDENING_REQUIRED");
    assert_eq!(refusal["path"], "users/d.json");
    assert!(
        refusal["help"]
            .as_str()
            .unwrap()
            .contains("declares \"nick\""),
        "the refusal says what widening would admit it: {refusal:#}"
    );
    assert_eq!(files(root), before, "a refused change writes nothing");
    let (refused, exit) = run(
        root,
        &["sql", "UPDATE users SET name = NULL WHERE id = 'a'"],
    );
    assert_eq!(exit, 2, "{refused:#}");
    assert_eq!(codes(&refused)[0], "CONTRACT_WIDENING_REQUIRED");

    // Unless asked to: the contract and the row land in one revision.
    let widened = ok(
        root,
        &[
            "--widen",
            "insert",
            "users",
            r#"{"id":"d","name":"Dee","nick":"dd"}"#,
        ],
    );
    let event = events(&widened, "contract_widened");
    assert_eq!(event.len(), 1, "{widened:#}");
    assert_eq!(event[0]["table"], "users");
    let document: Value = serde_json::from_str(&read(root, ".db/schema/users.json")).unwrap();
    assert_eq!(document["properties"]["nick"], json!({"type": "string"}));
    assert!(
        !document["required"]
            .as_array()
            .unwrap()
            .contains(&json!("nick")),
        "a member only some rows have is optional"
    );
    let status = ok(root, &["status"]);
    assert!(
        !codes(&status).contains(&"CONTRACT_EXCEEDED".to_string()),
        "the widened contract admits the outside edit too: {status:#}"
    );
    let log = ok(root, &["log"]);
    assert_eq!(log["records"][0]["origin"], "user");
}

#[test]
fn test3084_a_pinned_contract_widens_only_when_asked() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "counts",
        schema(
            "counts",
            json!({"id": {"type": "string"}, "n": {"type": "integer", "x-reldir-type": "int"}}),
            &["id", "n"],
            json!({}),
        ),
    );
    write(root, "counts/a.json", "{\"id\": \"a\", \"n\": 1}\n");
    ok(root, &["status"]);
    let (refused, exit) = run(root, &["update", "counts", "a", r#"{"n":"many"}"#]);
    assert_eq!(exit, 2, "{refused:#}");
    assert_eq!(
        codes(&refused)[0],
        "TYPE_MISMATCH",
        "a pin is held to what it says"
    );

    let pin_before = read(root, "schema/counts.json");
    ok(
        root,
        &["--widen", "update", "counts", "a", r#"{"n":"many"}"#],
    );
    let document: Value = serde_json::from_str(&read(root, "schema/counts.json")).unwrap();
    assert_eq!(
        document["properties"]["n"],
        json!({"type": ["integer", "string"]}),
        "the union outgrows the int tag"
    );
    let pin_after = read(root, "schema/counts.json");
    let edited = pin_before.find("\"n\"").unwrap();
    assert_eq!(
        pin_after[..edited],
        pin_before[..edited],
        "the pin is edited in place: what the widening does not touch keeps its bytes"
    );
    assert!(
        pin_after.contains("\"type\": [\n"),
        "in the pin's own layout:\n{pin_after}"
    );
    assert_eq!(
        read(root, "counts/a.json"),
        "{\"id\": \"a\", \"n\": \"many\"}\n"
    );
}

#[test]
fn test3085_widening_by_command_or_by_doctor_is_named_never_assumed() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_json(root, "users/a.json", json!({"id": "a", "name": "Ann"}));
    write_json(root, "users/b.json", json!({"id": "b", "name": "Bob"}));
    ok(root, &["status"]);
    write_json(
        root,
        "users/c.json",
        json!({"id": "c", "name": "Cy", "nick": 7}),
    );

    // Doctor offers the widening, and a blanket repair does not make it.
    let plan = ok(root, &["doctor"]);
    let offered: Vec<&Value> = plan["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["id"] == "FIX_WIDEN_CONTRACT")
        .collect();
    assert_eq!(offered.len(), 1, "{plan:#}");
    let contract = read(root, ".db/schema/users.json");
    ok(root, &["doctor", "--fix"]);
    assert_eq!(read(root, ".db/schema/users.json"), contract);
    ok(root, &["doctor", "--fix", "--only", "FIX_WIDEN_CONTRACT"]);
    let document: Value = serde_json::from_str(&read(root, ".db/schema/users.json")).unwrap();
    assert_eq!(
        document["properties"]["nick"],
        json!({"type": "integer", "x-reldir-type": "int"})
    );
    assert!(!codes(&ok(root, &["status"])).contains(&"CONTRACT_EXCEEDED".to_string()));
    let log = ok(root, &["log"]);
    assert_eq!(log["records"][0]["origin"], "repair");

    // A member widened as stated.
    let (refused, _) = run(
        root,
        &["sql", "UPDATE users SET name = NULL WHERE id = 'a'"],
    );
    assert_eq!(codes(&refused)[0], "CONTRACT_WIDENING_REQUIRED");
    let widened = ok(root, &["schema", "widen", "users.name", "--null"]);
    assert_eq!(events(&widened, "contract_widened").len(), 1, "{widened:#}");
    ok(
        root,
        &["sql", "UPDATE users SET name = NULL WHERE id = 'a'"],
    );
    assert_eq!(
        serde_json::from_str::<Value>(&read(root, "users/a.json")).unwrap(),
        json!({"id": "a", "name": null})
    );
    let recorded = revision(root);
    ok(root, &["schema", "widen", "users.name", "--null"]);
    assert_eq!(
        revision(root),
        recorded,
        "nothing to widen, nothing recorded"
    );

    // A table widened to admit its files.
    write_json(root, "users/d.json", json!({"id": "d", "name": ["D"]}));
    ok(root, &["schema", "widen", "users"]);
    let document: Value = serde_json::from_str(&read(root, ".db/schema/users.json")).unwrap();
    assert_eq!(
        document["properties"]["name"]["type"],
        json!(["string", "null", "array"])
    );

    // What cannot be widened by rule is refused, with the reason.
    let (refused, exit) = run(root, &["schema", "widen", "users.id", "--optional"]);
    assert_ne!(exit, 0, "{refused:#}");
    let (refused, exit) = run(root, &["schema", "widen", "users.ghost", "--null"]);
    assert_eq!(exit, 1, "{refused:#}");
}

#[test]
fn test3086_a_lost_contract_comes_back_from_history_and_a_moved_one_stays_moved() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_json(root, "users/a.json", json!({"id": "a", "name": "Ann"}));
    ok(root, &["status"]);
    let contract = read(root, ".db/schema/users.json");
    let recorded = revision(root);

    // The rows changed by hand since adoption must not reshape the contract.
    write_json(root, "users/b.json", json!({"id": "b", "name": "Bo"}));
    ok(root, &["status"]);
    fs::remove_file(root.join(".db/schema/users.json")).unwrap();
    let status = ok(root, &["status"]);
    assert_eq!(status["data"]["valid"], true, "{status:#}");
    diagnostic(&status, "CONTRACT_REDERIVED");
    assert_eq!(
        read(root, ".db/schema/users.json"),
        contract,
        "restored byte for byte"
    );
    assert_eq!(
        revision(root),
        recorded + 1,
        "only the hand-added row was recorded"
    );

    // Pinning moves the schema; history follows it.
    ok(root, &["schema", "pin", "users"]);
    assert!(!root.join(".db/schema/users.json").exists());
    let pinned = revision(root);
    assert_eq!(pinned, recorded + 2, "a schema that moved is a change");
    ok(root, &["snapshot", "create", "pinned"]);
    write_json(root, "users/c.json", json!({"id": "c", "name": "Cy"}));
    ok(root, &["snapshot", "restore", "pinned", "--yes"]);
    assert!(
        !root.join(".db/schema/users.json").exists(),
        "a snapshot holds the schema where it was, and nowhere it used to be"
    );
    assert!(!root.join("users/c.json").exists());
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);
}

#[test]
fn test3087_names_one_filesystem_folds_together_are_two_rows_here_and_a_warning() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    // Two names that fold together can only both exist where the filesystem
    // keeps case apart, as Linux's do and macOS's and Windows' do not by default.
    write(root, "probe/case", "");
    if root.join("probe/CASE").exists() {
        eprintln!("skipped: {} folds case", root.display());
        return;
    }
    fs::remove_dir_all(root.join("probe")).unwrap();
    write_json(root, "notes/Readme.json", json!({"text": "upper"}));
    write_json(root, "notes/readme.json", json!({"text": "lower"}));
    let status = ok(root, &["status"]);
    assert_eq!(
        status["data"]["valid"], true,
        "a valid Linux folder is adopted: {status:#}"
    );
    let warning = diagnostic(&status, "PORTABILITY_COLLISION");
    assert_eq!(warning["severity"], "warning");
    let rows = ok(root, &["sql", "SELECT text FROM notes ORDER BY text"]);
    assert_eq!(rows["data"]["rows"], 2, "both files are rows: {rows:#}");

    write(root, ".db/config", "{\"portable_names\": true}\n");
    let (status, exit) = run(root, &["check"]);
    assert_eq!(exit, 2, "{status:#}");
    assert_eq!(
        diagnostic(&status, "PORTABILITY_COLLISION")["severity"],
        "error"
    );
}

// ---------------------------------------------------------------------------
// Migrations that keep what they do not change

#[test]
fn test3088_a_renamed_column_keeps_its_place_and_every_rule_that_names_it() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "items",
        schema(
            "items",
            json!({"id": {"type": "string"}, "price": {"type": "integer", "x-reldir-type": "int"},
                   "currency": {"type": "string"}, "note": {"type": "string"}}),
            &["id"],
            json!({
                "checks": [{"name": "positive", "expr": "price > 0 AND items.price < 1000"}],
                "assertions": [{"name": "cheap", "query": "SELECT i.id FROM items AS i WHERE i.price > 500"}]
            }),
        ),
    );
    let mut document: Value = serde_json::from_str(&read(root, "schema/items.json")).unwrap();
    document["dependentRequired"] = json!({"price": ["currency"]});
    write_json(root, "schema/items.json", document);
    write(
        root,
        "items/a.json",
        "{\"id\": \"a\", \"price\": 5, \"currency\": \"EUR\", \"note\": \"n\"}\n",
    );
    ok(root, &["status"]);

    ok(
        root,
        &["migrate", "rename-column", "items", "price", "amount"],
    );
    assert_eq!(
        read(root, "items/a.json"),
        "{\"id\": \"a\", \"amount\": 5, \"currency\": \"EUR\", \"note\": \"n\"}\n",
        "the member is renamed where it stands"
    );
    let document: Value = serde_json::from_str(&read(root, "schema/items.json")).unwrap();
    assert_eq!(
        document["dependentRequired"],
        json!({"amount": ["currency"]})
    );
    assert_eq!(
        document["x-reldir"]["checks"][0]["expr"],
        "amount > 0 AND items.amount < 1000"
    );
    assert_eq!(
        document["x-reldir"]["assertions"][0]["query"],
        "SELECT i.id FROM items AS i WHERE i.amount > 500"
    );
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);
}

#[test]
fn test3089_a_type_change_keeps_the_rules_that_still_apply_or_says_which_do_not() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "items",
        schema(
            "items",
            json!({"id": {"type": "string"}, "n": {"type": "integer", "x-reldir-type": "int", "minimum": 1}}),
            &["id", "n"],
            json!({}),
        ),
    );
    write(root, "items/a.json", "{\"id\": \"a\", \"n\": 2}\n");
    ok(root, &["status"]);
    ok(root, &["migrate", "change-type", "items", "n", "number"]);
    let document: Value = serde_json::from_str(&read(root, "schema/items.json")).unwrap();
    assert_eq!(
        document["properties"]["n"],
        json!({"type": "number", "minimum": 1})
    );
    assert_eq!(
        read(root, "items/a.json"),
        "{\"id\": \"a\", \"n\": 2}\n",
        "an int is a number as written"
    );

    let (refused, exit) = run(root, &["migrate", "change-type", "items", "n", "string"]);
    assert_eq!(exit, 9, "{refused:#}");
    assert_eq!(codes(&refused)[0], "MIGRATION_REFUSED");
    ok(
        root,
        &[
            "migrate",
            "change-type",
            "items",
            "n",
            "string",
            "--drop-constraint",
            "minimum",
        ],
    );
    assert_eq!(
        read(root, "items/a.json"),
        "{\"id\": \"a\", \"n\": \"2\"}\n"
    );

    let (refused, exit) = run(root, &["migrate", "change-type", "items", "n", "enum"]);
    assert_eq!(exit, 1, "an enum needs its members: {refused:#}");
    ok(
        root,
        &[
            "migrate",
            "change-type",
            "items",
            "n",
            "enum",
            "--values",
            "1,2,3",
        ],
    );
    let document: Value = serde_json::from_str(&read(root, "schema/items.json")).unwrap();
    assert_eq!(document["properties"]["n"]["enum"], json!(["1", "2", "3"]));
}

#[test]
fn test3090_re_keying_renames_every_file_and_carries_every_reference() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    // An adopted folder keyed by file name.
    write_json(
        root,
        "people/ada.json",
        json!({"email": "ada@x.org", "name": "Ada"}),
    );
    write_json(
        root,
        "people/alan.json",
        json!({"email": "alan@x.org", "name": "Alan"}),
    );
    write_json(root, "notes/n1.json", json!({"id": "n1", "author": "ada"}));
    ok(root, &["status"]);
    let people: Value = serde_json::from_str(&read(root, ".db/schema/people.json")).unwrap();
    assert_eq!(people["x-reldir"]["identity"]["source"], "filename");
    // The reference is declared, and says to follow a new key.
    ok(
        root,
        &[
            "migrate",
            "add-constraint",
            "notes",
            r#"{"kind":"foreign_key","from":["author"],"to":{"table":"people"},"onUpdate":"cascade"}"#,
        ],
    );

    ok(
        root,
        &[
            "migrate",
            "change-identity",
            "people",
            r#"{"source":"body","columns":["email"],"naming":{"rule":"reldir"}}"#,
        ],
    );
    assert!(!root.join("people/ada.json").exists());
    assert_eq!(
        serde_json::from_str::<Value>(&read(root, "people/ada%40x.org.json")).unwrap(),
        json!({"email": "ada@x.org", "name": "Ada", "id": "ada"}),
        "what the file name held is kept in the body"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&read(root, "notes/n1.json")).unwrap(),
        json!({"id": "n1", "author": "ada@x.org"}),
        "the reference follows the new key"
    );
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);

    // Back to file names, by the column that holds them.
    ok(
        root,
        &[
            "migrate",
            "change-identity",
            "people",
            r#"{"source":"filename","column":"id","type":"string"}"#,
        ],
    );
    assert_eq!(
        serde_json::from_str::<Value>(&read(root, "people/ada.json")).unwrap(),
        json!({"email": "ada@x.org", "name": "Ada"})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&read(root, "notes/n1.json")).unwrap(),
        json!({"id": "n1", "author": "ada"})
    );
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);
}

#[test]
fn test3091_re_keying_refuses_to_strand_a_reference_that_restricts() {
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
        "notes",
        schema(
            "notes",
            json!({"id": {"type": "string"}, "author": {"type": "string"}}),
            &["id", "author"],
            json!({"foreignKeys": [{"from": ["author"], "to": {"table": "users"}}]}),
        ),
    );
    write_json(root, "users/ada.json", json!({"id": "ada", "name": "Ada"}));
    write_json(root, "notes/n1.json", json!({"id": "n1", "author": "ada"}));
    ok(root, &["status"]);
    let before = files(root);
    let (refused, exit) = run(
        root,
        &[
            "migrate",
            "change-identity",
            "users",
            r#"{"source":"body","columns":["name"],"naming":{"rule":"reldir"}}"#,
        ],
    );
    assert_eq!(exit, 2, "{refused:#}");
    assert_eq!(codes(&refused)[0], "FOREIGN_KEY_VIOLATION");
    assert_eq!(
        files(root),
        before,
        "nothing is renamed when a reference would be stranded"
    );
}

// ---------------------------------------------------------------------------
// Referential actions, judged as SQL judges them

fn teams(task_on_team_delete: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "teams",
        schema(
            "teams",
            json!({"id": {"type": "string"}}),
            &["id"],
            json!({}),
        ),
    );
    pin(
        root,
        "projects",
        schema(
            "projects",
            json!({"id": {"type": "string"}, "team": {"type": "string"}}),
            &["id", "team"],
            json!({"foreignKeys": [{"from": ["team"], "to": {"table": "teams"}, "onDelete": "cascade"}]}),
        ),
    );
    pin(
        root,
        "tasks",
        schema(
            "tasks",
            json!({"id": {"type": "string"}, "team": {"type": "string"}, "project": {"type": "string"}}),
            &["id", "team", "project"],
            json!({"foreignKeys": [
                {"name": "team", "from": ["team"], "to": {"table": "teams"}, "onDelete": task_on_team_delete},
                {"name": "project", "from": ["project"], "to": {"table": "projects"}, "onDelete": "cascade"}
            ]}),
        ),
    );
    write_json(root, "teams/t.json", json!({"id": "t"}));
    write_json(root, "projects/p.json", json!({"id": "p", "team": "t"}));
    write_json(
        root,
        "tasks/k.json",
        json!({"id": "k", "team": "t", "project": "p"}),
    );
    directory
}

#[test]
fn test3092_no_action_is_judged_when_the_statement_is_done_and_restrict_at_once() {
    // The task names the team, and goes with its project: by the end of the
    // statement nothing names the team, which is all `no_action` asks.
    let deferred = teams("no_action");
    let root = deferred.path();
    ok(root, &["delete", "teams", "t"]);
    assert!(!root.join("tasks/k.json").exists());
    assert!(!root.join("projects/p.json").exists());

    let immediate = teams("restrict");
    let root = immediate.path();
    let (refused, exit) = run(root, &["delete", "teams", "t"]);
    assert_eq!(exit, 2, "{refused:#}");
    assert_eq!(codes(&refused)[0], "FOREIGN_KEY_VIOLATION");
    assert!(root.join("teams/t.json").exists());

    // A task that stays still blocks the delete under `no_action` too.
    let blocked = teams("no_action");
    let root = blocked.path();
    write_json(root, "projects/q.json", json!({"id": "q", "team": "u"}));
    write_json(root, "teams/u.json", json!({"id": "u"}));
    write_json(
        root,
        "tasks/k.json",
        json!({"id": "k", "team": "t", "project": "q"}),
    );
    let (refused, exit) = run(root, &["delete", "teams", "t"]);
    assert_eq!(exit, 2, "{refused:#}");
    assert_eq!(
        diagnostic(&refused, "FOREIGN_KEY_VIOLATION")["path"],
        "tasks/k.json"
    );
}

#[test]
fn test3093_a_referenced_key_set_to_null_is_a_key_change() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "users",
        schema(
            "users",
            json!({"id": {"type": "string"}, "email": {"type": ["string", "null"]}}),
            &["id"],
            json!({"unique": [["email"]]}),
        ),
    );
    pin(
        root,
        "subscriptions",
        schema(
            "subscriptions",
            json!({"id": {"type": "string"}, "email": {"type": ["string", "null"]}}),
            &["id"],
            json!({"foreignKeys": [{"from": ["email"], "to": {"table": "users"}, "columns": ["email"],
                                    "onDelete": "restrict", "onUpdate": "set_null"}]}),
        ),
    );
    write_json(root, "users/a.json", json!({"id": "a", "email": "a@x"}));
    write_json(
        root,
        "subscriptions/s.json",
        json!({"id": "s", "email": "a@x"}),
    );
    ok(
        root,
        &["sql", "UPDATE users SET email = NULL WHERE id = 'a'"],
    );
    assert_eq!(
        serde_json::from_str::<Value>(&read(root, "subscriptions/s.json")).unwrap(),
        json!({"id": "s", "email": null}),
        "the row stayed, so its reference followed onUpdate rather than onDelete"
    );
}

#[test]
fn test3094_a_reference_into_listed_tables_names_exactly_one_row() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    for table in ["posts", "pages"] {
        pin(
            root,
            table,
            schema(table, json!({"id": {"type": "string"}}), &["id"], json!({})),
        );
    }
    pin(
        root,
        "comments",
        schema(
            "comments",
            json!({"id": {"type": "string"}, "subject": {"type": "string"}}),
            &["id", "subject"],
            json!({"foreignKeys": [{"from": ["subject"], "to": {"tables": ["posts", "pages"]}}]}),
        ),
    );
    write_json(root, "posts/a.json", json!({"id": "a"}));
    write_json(root, "pages/b.json", json!({"id": "b"}));
    write_json(root, "comments/c.json", json!({"id": "c", "subject": "b"}));
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);

    write_json(root, "pages/a.json", json!({"id": "a"}));
    let (check, exit) = run(root, &["check"]);
    assert_eq!(exit, 2, "{check:#}");
    let overlap = diagnostic(&check, "SCHEMA_TARGET_KEYS_OVERLAP");
    assert_eq!(overlap["observed"], "[\"a\"]");

    fs::remove_file(root.join("pages/a.json")).unwrap();
    pin(
        root,
        "pages",
        schema(
            "pages",
            json!({"id": {"type": "integer", "x-reldir-type": "int"}}),
            &["id"],
            json!({}),
        ),
    );
    write_json(root, "pages/b.json", json!({"id": 1}));
    fs::rename(root.join("pages/b.json"), root.join("pages/1.json")).unwrap();
    let (check, _) = run(root, &["check"]);
    assert!(
        codes(&check).contains(&"SCHEMA_TARGET_KEYS_OVERLAP".to_string()),
        "keys of different types are no one namespace: {check:#}"
    );
}

#[test]
fn test3095_a_deep_check_verifies_everything_history_keeps() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["insert", "users", r#"{"id":"cy","name":"Cy"}"#]);
    ok(root, &["insert", "users", r#"{"id":"di","name":"Di"}"#]);
    // Damage only the oldest pack: objects the head still names, which no
    // ordinary command needs to read.
    let mut packs: Vec<PathBuf> = fs::read_dir(root.join(".db/objects/pack"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "pack")
        })
        .collect();
    packs.sort();
    let oldest = &packs[0];
    // A pack holds the rows' bytes first, then the tree, then the revision:
    // an early byte is a row as adoption recorded it.
    let mut bytes = fs::read(oldest).unwrap();
    bytes[48] ^= 0xff;
    fs::write(oldest, bytes).unwrap();

    let ordinary = ok(root, &["check"]);
    assert_eq!(
        ordinary["data"]["valid"], true,
        "an ordinary check reads what commands read"
    );
    let (deep, exit) = run(root, &["check", "--deep"]);
    assert_eq!(exit, 6, "{deep:#}");
    let fault = diagnostic(&deep, "INTERNAL_METADATA_CORRUPT");
    assert_eq!(fault["path"], ".db/objects");
}

// ---------------------------------------------------------------------------
// Readers and writers

#[test]
fn test3096_a_read_never_waits_for_a_writer() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    // Stand in for a writer in the middle of a commit.
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join(".db/lock"))
        .unwrap();
    lock.try_lock().unwrap();
    fs::write(
        root.join(".db/lock.holder"),
        "{\"pid\": 4242, \"command\": \"insert users\", \"since\": \"2026-01-01T00:00:00Z\"}\n",
    )
    .unwrap();

    let started = std::time::Instant::now();
    let answer = ok(
        root,
        &["--wait", "5", "sql", "SELECT id FROM users ORDER BY id"],
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "the read did not wait for the writer"
    );
    assert_eq!(answer["records"][0]["row"]["id"], "ada");
    assert_eq!(diagnostic(&answer, "SNAPSHOT_READ")["severity"], "warning");
    let status = ok(root, &["status"]);
    assert_eq!(
        status["data"]["revision"]["seq"], 1,
        "the snapshot names the revision it answers from"
    );

    let readonly = ok(
        root,
        &["--readonly", "sql", "SELECT count(*) AS n FROM users"],
    );
    assert_eq!(readonly["records"][0]["row"]["n"], 2);

    let (refused, exit) = run(
        root,
        &[
            "--wait",
            "0",
            "insert",
            "users",
            r#"{"id":"cy","name":"Cy"}"#,
        ],
    );
    assert_eq!(exit, 3, "{refused:#}");
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("4242") && message.contains("insert users"),
        "{message}"
    );

    lock.unlock().unwrap();
    let after = ok(root, &["sql", "SELECT id FROM users ORDER BY id"]);
    assert!(
        !codes(&after).contains(&"SNAPSHOT_READ".to_string()),
        "with no writer, a read observes the folder itself"
    );
}

// ---------------------------------------------------------------------------
// Upgrading from format 2

/// A copy of a database format 2 wrote: five revisions (adoption, an insert,
/// an update, a delete, a migration), a pin, and a snapshot.
fn format_2() -> tempfile::TempDir {
    fn copy(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let target = to.join(if name == "dot-db" { ".db".into() } else { name });
            if entry.path().is_dir() {
                copy(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let directory = tempfile::tempdir().unwrap();
    copy(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/format-2"),
        directory.path(),
    );
    directory
}

#[test]
fn test3097_a_format_2_database_upgrades_with_its_history_and_snapshots() {
    let directory = format_2();
    let root = directory.path();
    let (refused, exit) = run(root, &["status"]);
    assert_eq!(exit, 6);
    assert_eq!(codes(&refused)[0], "FORMAT_UPGRADE_REQUIRED");

    let rows_before: BTreeMap<PathBuf, Vec<u8>> = files(root)
        .into_iter()
        .filter(|(path, _)| !path.starts_with("schema"))
        .collect();
    let pin_before = read(root, "schema/users.json");
    let upgraded = ok(root, &["upgrade"]);
    let report = &upgraded["records"][0];
    assert_eq!(report["revisions"], 5);
    assert_eq!(report["snapshots"], json!(["before-upgrade"]));
    assert_eq!(report["pins"], json!(["users"]));
    assert_eq!(
        report["converted"],
        json!(["posts"]),
        "a working schema a migration changed is converted as written"
    );
    assert_eq!(read(root, ".db/format").trim(), "format_version = 3");

    // No row was touched; the pin speaks the new dialect, its original kept.
    let rows_after: BTreeMap<PathBuf, Vec<u8>> = files(root)
        .into_iter()
        .filter(|(path, _)| !path.starts_with("schema"))
        .collect();
    assert_eq!(rows_after, rows_before);
    assert_eq!(read(root, ".db/format-2/pins/users.json"), pin_before);
    let pin: Value = serde_json::from_str(&read(root, "schema/users.json")).unwrap();
    assert_eq!(pin["$schema"], DIALECT);
    assert_eq!(
        pin["x-reldir"]["identity"],
        json!({"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}})
    );
    assert_eq!(
        pin["properties"]["name"]["description"],
        "what they are called"
    );
    let posts: Value = serde_json::from_str(&read(root, ".db/schema/posts.json")).unwrap();
    assert_eq!(posts["x-reldir"]["foreignKeys"][0]["to"]["table"], "users");

    // History, as format 2 recorded it, then the upgrade.
    let log = ok(root, &["log"]);
    let origins: Vec<&str> = log["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["origin"].as_str().unwrap())
        .collect();
    assert_eq!(
        origins,
        ["upgrade", "migration", "user", "user", "user", "inference"]
    );
    assert_eq!(
        log["records"][5]["imported_from"],
        json!({"format": 2, "revision": 1})
    );
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);

    // The snapshot puts the state it held back.
    write_json(root, "users/zed.json", json!({"id": "zed", "name": "Z"}));
    ok(root, &["snapshot", "restore", "before-upgrade", "--yes"]);
    assert!(!root.join("users/zed.json").exists());
    assert_eq!(read(root, ".db/format").trim(), "format_version = 3");

    let (again, exit) = run(root, &["upgrade"]);
    assert_eq!(exit, 1, "{again:#}");
}

#[test]
fn test3098_format_2_history_that_does_not_verify_is_never_converted() {
    let directory = format_2();
    let root = directory.path();
    let record = root.join(".db/provenance/00000000000000000003.json");
    let mut text = fs::read_to_string(&record).unwrap();
    text = text.replace("\"origin\": \"internal\"", "\"origin\": \"external\"");
    let tampered: Value = serde_json::from_str(&text).unwrap();
    let mut forged = tampered.clone();
    forged["new_root_hash"] = json!("0".repeat(64));
    fs::write(&record, serde_json::to_string_pretty(&forged).unwrap()).unwrap();
    let before = everything(root);
    let (refused, exit) = run(root, &["upgrade"]);
    assert_eq!(exit, 6, "{refused:#}");
    assert_eq!(codes(&refused)[0], "INTERNAL_METADATA_CORRUPT");
    let unlocked = |files: BTreeMap<PathBuf, Vec<u8>>| -> BTreeMap<PathBuf, Vec<u8>> {
        files
            .into_iter()
            .filter(|(path, _)| {
                !path
                    .to_string_lossy()
                    .replace('\\', "/")
                    .starts_with(".db/lock")
            })
            .collect()
    };
    assert_eq!(
        unlocked(everything(root)),
        unlocked(before),
        "a refused upgrade leaves the database as it was"
    );
    assert!(read(root, ".db/format").contains("= 2"));

    let begun = ok(root, &["upgrade", "--new-lineage"]);
    assert_eq!(begun["records"][0]["new_lineage"], true);
    assert_eq!(begun["data"]["revision"]["seq"], 1);
    assert!(
        root.join(".db/format-2/provenance/00000000000000000003.json")
            .exists()
    );
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);
}

// ---------------------------------------------------------------------------
// The machine protocol and the command line

#[test]
fn test3099_a_column_may_be_called_anything_the_protocol_uses() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_json(
        root,
        "things/a.json",
        json!({"id": "a", "kind": "tool", "type": "hammer"}),
    );
    let answer = ok(root, &["sql", "SELECT id, kind, type FROM things"]);
    assert_eq!(
        answer["records"][0]["row"],
        json!({"id": "a", "kind": "tool", "type": "hammer"})
    );

    let lines = reldir(root)
        .args(["--format", "jsonl", "sql", "SELECT kind, type FROM things"])
        .output()
        .unwrap();
    let first: Value = serde_json::from_str(
        String::from_utf8_lossy(&lines.stdout)
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        first,
        json!({"type": "row", "row": {"kind": "tool", "type": "hammer"}})
    );

    let csv = reldir(root)
        .args(["--format", "csv", "sql", "SELECT kind, type FROM things"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&csv.stdout),
        "kind,type\ntool,hammer\n"
    );
}

#[test]
fn test3100_arguments_are_refused_in_the_protocol_asked_for() {
    let directory = blog();
    let root = directory.path();
    let output = reldir(root)
        .args(["--format", "json", "get", "users"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["type"], "result");
    assert_eq!(envelope["error"]["code"], "USAGE");

    let output = reldir(root)
        .args(["--format", "json", "chek"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        envelope["error"]["help"]
            .as_str()
            .unwrap()
            .contains("reldir check"),
        "{envelope:#}"
    );

    // No subcommand and no terminal: nothing to open a shell for, and stdin
    // is not read for a statement nobody asked to give.
    let output = reldir(root).write_stdin("SELECT 1;").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let line: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(line["error"]["code"], "USAGE");
    let output = reldir(root)
        .args(["--format", "json", "sql", "-"])
        .write_stdin("SELECT count(*) AS n FROM users")
        .output()
        .unwrap();
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["records"][0]["row"]["n"], 2);

    let output = reldir(root)
        .env("RELDIR_FORMAT", "json")
        .args(["--timeout", "0.5", "status"])
        .output()
        .unwrap();
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        envelope["data"]["valid"], true,
        "RELDIR_FORMAT and a fractional timeout"
    );
}

#[test]
fn test3101_a_reader_that_stops_reading_does_not_stop_a_change() {
    let directory = blog();
    let root = directory.path();
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("reldir"))
        .env_remove("RELDIR_DB")
        .args(["--db", root.to_str().unwrap(), "--format", "jsonl"])
        .args([
            "migrate",
            "add-column",
            "users",
            "bio",
            "--type",
            "string",
            "--nullable",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // The reader goes away before reading anything.
    drop(child.stdout.take());
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "the migration's own status");
    let document: Value = serde_json::from_str(&read(root, "schema/users.json")).unwrap();
    assert!(
        document["properties"].get("bio").is_some(),
        "the migration was made"
    );
}

#[test]
fn test3102_an_insert_reports_the_row_it_wrote() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "log",
        json!({
            "$schema": DIALECT, "type": "object",
            "properties": {"n": {"type": "integer", "x-reldir-type": "int"}, "text": {"type": "string"}},
            "required": ["n", "text"], "additionalProperties": false,
            "x-reldir": {"table": "log", "identity": {"source": "body", "columns": ["n"], "naming": {"rule": "reldir"}}, "generated": {"n": "sequence"}}
        }),
    );
    let inserted = ok(root, &["insert", "log", r#"{"text":"a"}"#]);
    let rows: Vec<&Value> = inserted["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["type"] == "row")
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["row"], json!({"text": "a", "n": 1}));
}

/// The rows of an envelope, in order.
fn rows_of(envelope: &Value) -> Vec<Value> {
    envelope["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|record| record["type"] == "row")
        .map(|record| record["row"].clone())
        .collect()
}

#[test]
fn test3103_a_script_is_one_transaction_that_reads_its_own_writes() {
    let directory = blog();
    let root = directory.path();
    let before = revision(root);
    // The post names a user an earlier statement inserted, and the query
    // after both sees them; the references are judged at the end.
    let done = ok(
        root,
        &[
            "sql",
            "INSERT INTO posts (id, user_id, title) VALUES ('p3', 'cy', 'First'); \
             INSERT INTO users (id, name) VALUES ('cy', 'Cy'); \
             SELECT p.id, u.name FROM posts p JOIN users u ON u.id = p.user_id WHERE u.id = 'cy'",
        ],
    );
    assert_eq!(rows_of(&done), [json!({"id": "p3", "name": "Cy"})]);
    let statements: Vec<&Value> = done["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["type"] == "statement")
        .collect();
    assert_eq!(statements.len(), 3);
    assert_eq!(statements[0]["changed"], 1);
    assert_eq!(statements[2]["rows"], 1);
    assert_eq!(
        revision(root),
        before + 1,
        "one revision for the whole script"
    );
    assert!(root.join("users/cy.json").exists() && root.join("posts/p3.json").exists());
}

#[test]
fn test3104_a_script_that_fails_anywhere_writes_nothing() {
    let directory = blog();
    let root = directory.path();
    let seq = revision(root);
    let before = files(root);

    // The second statement is refused at the end: bob still has a post.
    let (refused, exit) = run(
        root,
        &[
            "sql",
            "UPDATE users SET name = 'Ada L' WHERE id = 'ada'; DELETE FROM users WHERE id = 'bob'",
        ],
    );
    assert_ne!(exit, 0);
    assert_eq!(
        refused["error"]["code"], "FOREIGN_KEY_VIOLATION",
        "{refused:#}"
    );
    assert_eq!(files(root), before, "nothing of the first statement either");

    // A statement that cannot run names itself.
    let (refused, exit) = run(
        root,
        &[
            "sql",
            "UPDATE users SET name = 'X' WHERE id = 'ada'; UPDATE nowhere SET a = 1",
        ],
    );
    assert_ne!(exit, 0);
    assert_eq!(refused["error"]["code"], "UNKNOWN_TABLE");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("statement 2 of 2:"),
        "{refused:#}"
    );

    // A statement reldir cannot run at all refuses the script before any runs.
    let (refused, _) = run(
        root,
        &[
            "sql",
            "UPDATE users SET name = 'X' WHERE id = 'ada'; DROP TABLE users",
        ],
    );
    assert_eq!(refused["error"]["code"], "QUERY_UNSUPPORTED");
    assert_eq!(files(root), before);
    assert_eq!(revision(root), seq);

    // And the writer lock went with the failures.
    ok(
        root,
        &["sql", "UPDATE users SET name = 'Ada L' WHERE id = 'ada'"],
    );
}

#[test]
fn test3105_begin_commit_and_rollback_frame_a_script_and_go_nowhere_else() {
    let directory = blog();
    let root = directory.path();
    let before = files(root);

    let rolled = ok(
        root,
        &[
            "sql",
            "BEGIN; DELETE FROM posts WHERE id = 'p2'; SELECT count(*) AS n FROM posts; ROLLBACK",
        ],
    );
    assert_eq!(
        rows_of(&rolled),
        [json!({"n": 1})],
        "the rollback's statements ran"
    );
    assert_eq!(rolled["data"]["rolled_back"], true);
    assert_eq!(files(root), before, "and wrote nothing");

    for misplaced in [
        "BEGIN; DELETE FROM posts WHERE id = 'p2'",
        "DELETE FROM posts WHERE id = 'p2'; COMMIT",
        "BEGIN; DELETE FROM posts WHERE id = 'p2'; COMMIT; DELETE FROM posts WHERE id = 'p1'; COMMIT",
        "COMMIT",
        "BEGIN",
    ] {
        let (refused, exit) = run(root, &["sql", misplaced]);
        assert_eq!(exit, 1, "{misplaced}");
        assert_eq!(
            refused["error"]["code"], "TRANSACTION_STATEMENT",
            "{misplaced}"
        );
    }
    assert_eq!(files(root), before);

    let committed = ok(
        root,
        &[
            "sql",
            "begin transaction; DELETE FROM posts WHERE id = 'p2'; end",
        ],
    );
    assert_eq!(committed["data"]["statements"], 1);
    assert!(!root.join("posts/p2.json").exists());

    let (refused, exit) = run(root, &["sql", "SELECT 1; SELECT ?", "--param", "1"]);
    assert_eq!(exit, 1);
    assert_eq!(refused["error"]["code"], "USAGE", "{refused:#}");
}

#[test]
fn test3106_rows_changed_and_changed_back_are_no_change() {
    let directory = blog();
    let root = directory.path();
    // Status first, so adopting the folder is not mistaken for the change.
    let seq = revision(root);
    let before = files(root);
    let done = ok(
        root,
        &[
            "sql",
            "UPDATE users SET name = 'Someone' WHERE id = 'ada'; \
             INSERT INTO tags (id) VALUES ('fleeting'); \
             DELETE FROM tags WHERE id = 'fleeting'; \
             UPDATE users SET name = 'Ada' WHERE id = 'ada'",
        ],
    );
    assert_eq!(done["data"]["files"], 0, "{done:#}");
    assert_eq!(revision(root), seq, "no revision for no change");
    assert_eq!(files(root), before);
}

#[test]
fn test3107_a_key_change_in_a_script_carries_its_references_once() {
    let directory = blog();
    let root = directory.path();
    let done = ok(
        root,
        &[
            "sql",
            "UPDATE users SET id = 'lovelace' WHERE id = 'ada'; \
             SELECT id FROM posts WHERE user_id = 'lovelace'; \
             DELETE FROM tags WHERE id = 'math'",
        ],
    );
    assert_eq!(
        rows_of(&done),
        [json!({"id": "p1"})],
        "the cascade ran at the statement's end"
    );
    assert!(root.join("users/lovelace.json").exists() && !root.join("users/ada.json").exists());
    let post: Value = serde_json::from_str(&read(root, "posts/p1.json")).unwrap();
    assert_eq!(post["user_id"], "lovelace");
    assert_eq!(
        post["tag_ids"],
        json!(["logic"]),
        "both statements' actions, in one file"
    );
    let actions: Vec<&Value> = done["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["event"] == "referential_action")
        .collect();
    assert_eq!(actions.len(), 2, "each action reported once: {actions:#?}");
}

#[test]
fn test3108_a_script_comes_from_a_file_or_stdin_and_honours_dry_run() {
    let directory = blog();
    let root = directory.path();
    let script = root.join("..").join(format!(
        "{}-script.sql",
        root.file_name().unwrap().to_string_lossy()
    ));
    fs::write(
        &script,
        "-- move a post; a ';' in a comment or 'a ; string' cuts nothing\n\
         UPDATE posts SET user_id = 'bob', title = 'a ; b' WHERE id = 'p1';\n\
         SELECT title FROM posts WHERE id = 'p1';\n",
    )
    .unwrap();
    let before = files(root);
    let planned = ok(root, &["--dry-run", "sql", "-f", script.to_str().unwrap()]);
    assert_eq!(rows_of(&planned), [json!({"title": "a ; b"})]);
    assert_eq!(planned["data"]["dry_run"], true);
    assert_eq!(files(root), before, "a dry run writes nothing");

    ok(root, &["sql", "-f", script.to_str().unwrap()]);
    let post: Value = serde_json::from_str(&read(root, "posts/p1.json")).unwrap();
    assert_eq!(post["user_id"], "bob");
    fs::remove_file(&script).unwrap();

    let output = reldir(root)
        .args(["--format", "json", "sql", "-"])
        .write_stdin("SELECT count(*) AS n FROM users;\nSELECT count(*) AS n FROM posts;\n")
        .output()
        .unwrap();
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rows_of(&envelope), [json!({"n": 2}), json!({"n": 2})]);

    let (refused, exit) = run(root, &["sql", "SELECT 1", "-f", "x.sql"]);
    assert_eq!(exit, 1);
    assert_eq!(refused["error"]["code"], "USAGE");
}

#[test]
fn test3109_a_transaction_past_its_time_is_rolled_back() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    write(
        root,
        ".db/config",
        "{\"transaction_timeout_seconds\": 0.000001}\n",
    );
    let before = files(root);
    let (refused, exit) = run(
        root,
        &[
            "sql",
            "UPDATE users SET name = 'X' WHERE id = 'ada'; SELECT 1",
        ],
    );
    assert_ne!(exit, 0);
    assert_eq!(
        refused["error"]["code"], "TRANSACTION_EXPIRED",
        "{refused:#}"
    );
    assert_eq!(files(root), before);
}

#[test]
fn test3110_the_shell_holds_a_transaction_until_it_ends() {
    let directory = blog();
    let root = directory.path();
    let shell = |script: &str| -> Vec<Value> {
        let output = reldir(root)
            .args(["--format", "jsonl", "shell"])
            .write_stdin(script.to_string())
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    let before = files(root);
    // Rolled back by hand, and by leaving with one open.
    shell(
        "BEGIN;\nDELETE FROM posts WHERE id = 'p2';\nSELECT count(*) AS n FROM posts;\n.rollback\n",
    );
    shell(".begin\nDELETE FROM posts WHERE id = 'p2';\n.quit\n");
    assert_eq!(files(root), before);

    // A failed statement ends the transaction with everything it did.
    let lines = shell(
        ".begin\nDELETE FROM posts WHERE id = 'p2';\nUPDATE nowhere SET a = 1;\n\
         DELETE FROM users WHERE id = 'bob';\n",
    );
    assert_eq!(files(root), before, "{lines:#?}");
    let refused = lines.iter().rfind(|line| line["type"] == "result").unwrap();
    assert_eq!(
        refused["error"]["code"], "FOREIGN_KEY_VIOLATION",
        "the statement after the failure ran on its own, against p2 restored: {lines:#?}"
    );

    let lines = shell(
        ".begin\nDELETE FROM posts WHERE id = 'p2';\nSELECT count(*) AS n FROM posts;\n\
         DELETE FROM users WHERE id = 'bob';\nCOMMIT;\n",
    );
    assert!(
        lines
            .iter()
            .any(|line| line["type"] == "row" && line["row"]["n"] == 1),
        "the transaction reads its own writes: {lines:#?}"
    );
    assert!(!root.join("posts/p2.json").exists() && !root.join("users/bob.json").exists());
}

#[test]
fn test3111_rows_that_trade_keys_in_a_script_are_two_writes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(root, "slots/1.json", "{\"id\":1,\"label\":\"one\"}\n");
    write(root, "slots/2.json", "{\"id\":2,\"label\":\"two\"}\n");
    ok(root, &["status"]);
    let done = ok(
        root,
        &[
            "sql",
            "UPDATE slots SET id = 10 WHERE id = 1; UPDATE slots SET id = 1 WHERE id = 2; \
             UPDATE slots SET id = 2 WHERE id = 10",
        ],
    );
    assert_eq!(
        done["data"]["files"], 2,
        "the stop at 10 is not a change: {done:#}"
    );
    assert_eq!(read(root, "slots/1.json"), "{\"id\":1,\"label\":\"two\"}\n");
    assert_eq!(read(root, "slots/2.json"), "{\"id\":2,\"label\":\"one\"}\n");
    assert!(!root.join("slots/10.json").exists());
}

#[test]
fn test3112_a_retried_change_with_its_key_is_reported_not_made_again() {
    let directory = blog();
    let root = directory.path();
    let statement = "UPDATE posts SET title = title || '!' WHERE id = 'p2'";
    let first = ok(root, &["--idempotency-key", "retry-1", "sql", statement]);
    assert_eq!(first["data"]["replayed"], false);
    let again = ok(root, &["--idempotency-key", "retry-1", "sql", statement]);
    assert_eq!(again["data"]["replayed"], true, "{again:#}");
    assert_eq!(again["data"]["revision"], first["data"]["revision"]);
    let post: Value = serde_json::from_str(&read(root, "posts/p2.json")).unwrap();
    assert_eq!(post["title"], "Notes!", "made once");

    let inserted = ok(
        root,
        &[
            "--idempotency-key",
            "retry-2",
            "insert",
            "tags",
            r#"{"id":"new"}"#,
        ],
    );
    let again = ok(
        root,
        &[
            "--idempotency-key",
            "retry-2",
            "insert",
            "tags",
            r#"{"id":"new"}"#,
        ],
    );
    assert_eq!(again["data"]["replayed"], true);
    assert_eq!(again["data"]["revision"], inserted["data"]["revision"]);
}

/// Run git in `root`, with this build's `reldir` first on PATH -- the merge
/// and diff drivers `reldir git setup` configures run it by name.
fn git(root: &Path, args: &[&str]) -> std::process::Output {
    let binary = assert_cmd::cargo::cargo_bin("reldir");
    let path = std::env::join_paths(
        std::iter::once(binary.parent().unwrap().to_path_buf()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )),
    )
    .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("PATH", path)
        .env_remove("RELDIR_DB")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap()
}

fn git_ok(root: &Path, args: &[&str]) -> String {
    let output = git(root, args);
    assert!(
        output.status.success(),
        "git {}: {}{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn test3113_git_merges_rows_member_by_member_and_marks_only_real_conflicts() {
    let directory = blog();
    let root = directory.path();
    git_ok(root, &["init", "-q", "-b", "main"]);
    let setup = ok(root, &["git", "setup", "--hooks"]);
    assert_eq!(setup["records"][0]["type"], "git_setup");
    let attributes = read(root, ".gitattributes");
    assert!(
        attributes.contains("/users/*.json merge=reldir diff=reldir"),
        "{attributes}"
    );
    assert!(git_ok(root, &["config", "merge.reldir.driver"]).contains("git merge-file"));
    // Running it again changes nothing.
    ok(root, &["git", "setup", "--hooks"]);
    assert_eq!(read(root, ".gitattributes"), attributes);
    git_ok(root, &["add", "-A"]);
    git_ok(root, &["commit", "-q", "-m", "start"]);

    // Two branches change different members of one row: a clean merge.
    git_ok(root, &["checkout", "-q", "-b", "rename"]);
    write(
        root,
        "users/ada.json",
        "{\n  \"id\": \"ada\",\n  \"name\": \"Ada Lovelace\"\n}\n",
    );
    git_ok(root, &["commit", "-q", "-am", "rename"]);
    git_ok(root, &["checkout", "-q", "main"]);
    ok(
        root,
        &[
            "migrate",
            "add-column",
            "users",
            "born",
            "--type",
            "int",
            "--nullable",
        ],
    );
    ok(root, &["update", "users", "ada", r#"{"born": 1815}"#]);
    git_ok(root, &["add", "-A"]);
    git_ok(root, &["commit", "-q", "-m", "born"]);
    git_ok(root, &["merge", "-q", "--no-edit", "rename"]);
    let merged: Value = serde_json::from_str(&read(root, "users/ada.json")).unwrap();
    assert_eq!(
        merged,
        json!({"id": "ada", "name": "Ada Lovelace", "born": 1815})
    );
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);

    // Both change the same member: the conflict is marked around it alone,
    // and reldir names it.
    git_ok(root, &["checkout", "-q", "-b", "left"]);
    ok(root, &["update", "users", "bob", r#"{"name": "Robert"}"#]);
    git_ok(root, &["commit", "-q", "-am", "left"]);
    git_ok(root, &["checkout", "-q", "main"]);
    ok(
        root,
        &[
            "update",
            "users",
            "bob",
            r#"{"name": "Bobby", "born": 1990}"#,
        ],
    );
    git_ok(root, &["commit", "-q", "-am", "right"]);
    let failed = git(root, &["merge", "-q", "--no-edit", "left"]);
    assert!(!failed.status.success(), "the merge conflicts");
    let text = read(root, "users/bob.json");
    assert_eq!(text.matches("<<<<<<<").count(), 1, "{text}");
    // The markers name the sides: by git's labels, or -- from a git that
    // leaves its placeholders unexpanded -- by their roles.
    for marker in ["<<<<<<< ", "||||||| ", ">>>>>>> "] {
        let label = text
            .split(marker)
            .nth(1)
            .and_then(|rest| rest.lines().next());
        assert!(
            label.is_some_and(|label| !label.is_empty() && !label.starts_with('%')),
            "{marker:?} names its side: {text}"
        );
    }
    assert!(
        text.contains("1990"),
        "a member only one side changed is merged: {text}"
    );
    let (checked, exit) = run(root, &["check"]);
    assert_ne!(exit, 0);
    let conflict = diagnostic(&checked, "MERGE_CONFLICT");
    assert_eq!(conflict["path"], "users/bob.json");
    git_ok(root, &["merge", "--abort"]);

    // A row diff against a git revision.
    ok(root, &["update", "users", "ada", r#"{"name": "Countess"}"#]);
    let diff = ok(root, &["diff", "--git", "HEAD"]);
    let changes: Vec<&Value> = diff["records"].as_array().unwrap().iter().collect();
    assert_eq!(changes.len(), 1, "{diff:#}");
    assert_eq!(changes[0]["path"], "users/ada.json");
    assert_eq!(changes[0]["pointer"], "/name");
    let (unknown, exit) = run(root, &["diff", "--git", "no-such-branch"]);
    assert_eq!(exit, 1);
    assert_eq!(unknown["error"]["code"], "USAGE");

    // git diff names the member that changed, through the text conversion.
    let shown = git_ok(root, &["diff", "--", "users/ada.json"]);
    assert!(
        shown.contains("-/name = \"Ada Lovelace\"") && shown.contains("+/name = \"Countess\""),
        "{shown}"
    );
}

#[test]
fn test3114_imports_stream_every_format_and_commit_once() {
    let directory = blog();
    let root = directory.path();
    let input = tempfile::tempdir().unwrap();
    // Commas and brackets inside strings and nested values do not end an
    // element; each element is read with a row file's strictness.
    let array = input.path().join("tags.json");
    fs::write(
        &array,
        r#" [ {"id": "a,b"}, {"id": "c]d"} ,{"id":"e\"[{"} ] "#,
    )
    .unwrap();
    let done = ok(root, &["import", "tags", "--from", array.to_str().unwrap()]);
    assert_eq!(done["data"]["inserted"], 3);
    assert_eq!(revision(root), revision(root), "stable");
    for (name, body) in [
        ("lines.jsonl", "{\"id\": \"l1\"}\n\n{\"id\": \"l2\"}\n"),
        ("cells.csv", "id\nc1\nc2\n"),
    ] {
        let path = input.path().join(name);
        fs::write(&path, body).unwrap();
        assert_eq!(
            ok(root, &["import", "tags", "--from", path.to_str().unwrap()])["data"]["inserted"],
            2
        );
    }
    let output = reldir(root)
        .args([
            "--format",
            "json",
            "import",
            "tags",
            "--from",
            "-",
            "--input-format",
            "jsonl",
        ])
        .write_stdin("{\"id\": \"s1\"}\n")
        .output()
        .unwrap();
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["data"]["inserted"], 1, "{envelope:#}");
    let (refused, exit) = run(root, &["import", "tags", "--from", "-"]);
    assert_eq!(exit, 1);
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--input-format")
    );

    // One bad element, and nothing of the import is written.
    let before = files(root);
    let bad = input.path().join("bad.json");
    fs::write(&bad, r#"[{"id": "fine"}, {"id": "dup", "id": "twice"}]"#).unwrap();
    let (refused, _) = run(root, &["import", "tags", "--from", bad.to_str().unwrap()]);
    assert_eq!(refused["error"]["code"], "INPUT_INVALID", "{refused:#}");
    let unclosed = input.path().join("unclosed.json");
    fs::write(&unclosed, r#"[{"id": "x"}"#).unwrap();
    let (refused, _) = run(
        root,
        &["import", "tags", "--from", unclosed.to_str().unwrap()],
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not closed")
    );
    assert_eq!(files(root), before);
}

#[test]
fn test3115_upsert_and_replace_meet_the_table_by_key() {
    let directory = blog();
    let root = directory.path();
    let input = tempfile::tempdir().unwrap();
    let upsert = input.path().join("users.jsonl");
    fs::write(
        &upsert,
        "{\"id\": \"ada\", \"name\": \"Ada Lovelace\"}\n{\"id\": \"bob\", \"name\": \"Bob\"}\n{\"id\": \"cy\", \"name\": \"Cy\"}\n",
    )
    .unwrap();
    let (refused, _) = run(
        root,
        &["import", "users", "--from", upsert.to_str().unwrap()],
    );
    assert_eq!(
        refused["error"]["code"], "PRIMARY_KEY_VIOLATION",
        "insert refuses a key the table holds"
    );
    let done = ok(
        root,
        &[
            "import",
            "users",
            "--from",
            upsert.to_str().unwrap(),
            "--mode",
            "upsert",
        ],
    );
    assert_eq!(
        (
            &done["data"]["inserted"],
            &done["data"]["updated"],
            &done["data"]["unchanged"]
        ),
        (&json!(1), &json!(1), &json!(1))
    );
    assert_eq!(
        serde_json::from_str::<Value>(&read(root, "users/ada.json")).unwrap()["name"],
        "Ada Lovelace"
    );

    // The posts table becomes exactly the import: p2 goes.
    let replace = input.path().join("posts.json");
    fs::write(&replace, r#"[{"id": "p1", "user_id": "ada", "title": "Engines", "tag_ids": ["math", "logic"]}, {"id": "p3", "user_id": "cy", "title": "Third"}]"#).unwrap();
    let done = ok(
        root,
        &[
            "import",
            "posts",
            "--from",
            replace.to_str().unwrap(),
            "--mode",
            "replace",
        ],
    );
    assert_eq!(
        (
            &done["data"]["inserted"],
            &done["data"]["deleted"],
            &done["data"]["unchanged"]
        ),
        (&json!(1), &json!(1), &json!(1))
    );
    assert!(!root.join("posts/p2.json").exists() && root.join("posts/p3.json").exists());

    let twice = input.path().join("twice.jsonl");
    fs::write(&twice, "{\"id\": \"x\"}\n{\"id\": \"x\"}\n").unwrap();
    let (refused, _) = run(
        root,
        &[
            "import",
            "tags",
            "--from",
            twice.to_str().unwrap(),
            "--mode",
            "upsert",
        ],
    );
    assert_eq!(refused["error"]["code"], "INPUT_INVALID");
}

#[test]
fn test3116_exports_are_exact_filtered_ordered_and_never_replace_a_file() {
    let directory = blog();
    let root = directory.path();
    let out = tempfile::tempdir().unwrap();
    let path = |name: &str| out.path().join(name);

    let lines = path("posts.jsonl");
    ok(
        root,
        &[
            "export",
            "posts",
            "--out",
            lines.to_str().unwrap(),
            "--order",
            "-id",
        ],
    );
    let rows: Vec<Value> = fs::read_to_string(&lines)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows[0]["id"], "p2");
    assert!(
        rows[0].get("tag_ids").is_none(),
        "an absent member stays absent"
    );
    assert_eq!(rows[1]["tag_ids"], json!(["math", "logic"]));

    let json = path("ada.json");
    let done = ok(
        root,
        &[
            "export",
            "posts",
            "--out",
            json.to_str().unwrap(),
            "--where",
            "user_id = 'ada'",
        ],
    );
    assert_eq!(done["data"]["rows"], 1);
    let array: Value = serde_json::from_str(&fs::read_to_string(&json).unwrap()).unwrap();
    assert_eq!(array.as_array().unwrap().len(), 1);

    let csv = path("users.csv");
    ok(root, &["export", "users", "--out", csv.to_str().unwrap()]);
    assert_eq!(
        fs::read_to_string(&csv).unwrap(),
        "id,name\nada,Ada\nbob,Bob\n"
    );

    let (refused, exit) = run(root, &["export", "users", "--out", csv.to_str().unwrap()]);
    assert_eq!(exit, 1);
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("never replaces")
    );

    let parquet = path("posts.parquet");
    ok(
        root,
        &["export", "posts", "--out", parquet.to_str().unwrap()],
    );
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        fs::File::open(&parquet).unwrap(),
    )
    .unwrap()
    .build()
    .unwrap();
    use arrow_array::Array as _;
    let mut ids = vec![];
    let mut tags = vec![];
    for batch in reader {
        let batch = batch.unwrap();
        let id = batch
            .column_by_name("id")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow_array::StringArray>()
            .unwrap()
            .clone();
        let tag = batch
            .column_by_name("tag_ids")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow_array::StringArray>()
            .unwrap()
            .clone();
        for index in 0..batch.num_rows() {
            ids.push(id.value(index).to_string());
            tags.push((!tag.is_null(index)).then(|| tag.value(index).to_string()));
        }
    }
    assert_eq!(ids, ["p1", "p2"]);
    assert_eq!(tags, [Some("[\"math\",\"logic\"]".to_string()), None]);

    // Rows printed rather than written follow --format.
    let printed = ok(root, &["export", "users"]);
    assert_eq!(printed["records"].as_array().unwrap().len(), 2);
    let (refused, _) = run(root, &["export", "users", "--as", "csv"]);
    assert_eq!(
        refused["error"]["code"], "USAGE",
        "--as names a file's format"
    );
}

/// A `reldir serve` running for a test, stopped when it ends.
struct Monitor(std::process::Child);

impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn monitor(root: &Path) -> Monitor {
    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("reldir"))
        .env_remove("RELDIR_DB")
        .args(["--db", root.to_str().unwrap(), "--format", "jsonl", "serve"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Started once the handle it publishes names it: one a stopped monitor
    // left behind names another process.
    let handle = root.join(".db/serve.json");
    let started = std::time::Instant::now();
    loop {
        let published = fs::read(&handle)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|handle| handle["pid"] == child.id());
        if published {
            break;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "the monitor did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Monitor(child)
}

#[test]
fn test3117_a_monitored_folder_is_read_by_what_changed_and_read_right() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    let _monitor = monitor(root);
    // The first observation with the monitor examines everything, and marks
    // where its log stands.
    ok(root, &["status"]);
    let quiet = ok(root, &["status"]);
    assert_eq!(quiet["data"]["monitored"], true, "{quiet:#}");

    // Outside edits and additions are seen, through the monitor.
    write_json(
        root,
        "users/ada.json",
        json!({"id": "ada", "name": "Ada L"}),
    );
    write_json(root, "users/cy.json", json!({"id": "cy", "name": "Cy"}));
    let after = ok(root, &["status"]);
    assert_eq!(after["data"]["monitored"], true, "{after:#}");
    assert_eq!(
        ok(root, &["sql", "SELECT name FROM users WHERE id = 'ada'"])["records"][0]["row"]["name"],
        "Ada L"
    );
    assert_eq!(
        ok(root, &["sql", "SELECT count(*) AS n FROM users"])["records"][0]["row"]["n"],
        3
    );
    // And removals: the post's reference now dangles.
    fs::remove_file(root.join("tags/logic.json")).unwrap();
    let (broken, _) = run(root, &["check"]);
    assert_eq!(
        broken["data"]["monitored"],
        Value::Null,
        "check reports faults, not how it observed"
    );
    assert_eq!(
        diagnostic(&broken, "FOREIGN_KEY_VIOLATION")["path"],
        "posts/p1.json",
        "the removal was seen"
    );
    write_json(root, "tags/logic.json", json!({"id": "logic"}));
    assert_eq!(ok(root, &["status"])["data"]["valid"], true);

    // A change reldir makes itself is reported back, and read once.
    ok(root, &["update", "users", "bob", r#"{"name": "Robert"}"#]);
    assert_eq!(
        ok(root, &["get", "users", "bob"])["records"][0]["row"]["name"],
        "Robert"
    );

    // A file that is not a plain row -- a new directory beside the tables --
    // is examined the slow way, and found.
    fs::create_dir_all(root.join("notes")).unwrap();
    write_json(root, "notes/n1.json", json!({"id": "n1"}));
    let found = ok(root, &["status"]);
    assert_eq!(
        found["data"]["monitored"], false,
        "anything but rows is examined whole"
    );
    assert_eq!(ok(root, &["check"])["data"]["valid"], true);
}

#[test]
fn test3118_without_its_monitor_the_folder_is_examined_whole() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    let first = monitor(root);
    ok(root, &["status"]);
    drop(first);
    // The handle it left names a port no one listens on.
    write_json(
        root,
        "users/ada.json",
        json!({"id": "ada", "name": "Changed while unwatched"}),
    );
    let status = ok(root, &["status"]);
    assert_eq!(status["data"]["monitored"], false);
    assert_eq!(
        ok(root, &["sql", "SELECT name FROM users WHERE id = 'ada'"])["records"][0]["row"]["name"],
        "Changed while unwatched"
    );
    // A monitor started afresh begins another log: its first answer is
    // "examine everything".
    let _again = monitor(root);
    write_json(root, "users/bob.json", json!({"id": "bob", "name": "Seen"}));
    assert_eq!(ok(root, &["status"])["data"]["monitored"], false);
    assert_eq!(ok(root, &["status"])["data"]["monitored"], true);
    assert_eq!(
        ok(root, &["sql", "SELECT name FROM users WHERE id = 'bob'"])["records"][0]["row"]["name"],
        "Seen"
    );
}

#[test]
fn test3119_a_recursive_schema_is_judged_to_any_depth_and_promptly() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    pin(
        root,
        "trees",
        json!({
            "$schema": DIALECT, "type": "object",
            "properties": {"id": {"type": "string"}, "root": {"$ref": "#/$defs/node"}},
            "required": ["id", "root"], "additionalProperties": false,
            "$defs": {"node": {
                "type": "object",
                "properties": {"label": {"type": "string"}, "children": {"type": "array", "items": {"$ref": "#/$defs/node"}}},
                "required": ["label"], "additionalProperties": false
            }},
            "x-reldir": {"table": "trees", "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}}}
        }),
    );
    let mut deep = json!({"label": "leaf"});
    for level in 0..40 {
        deep = json!({"label": format!("l{level}"), "children": [deep]});
    }
    write_json(root, "trees/deep.json", json!({"id": "deep", "root": deep}));
    write_json(
        root,
        "trees/bad.json",
        json!({"id": "bad", "root": {"label": "x", "children": [{"label": "y", "children": [{"nolabel": 1}]}]}}),
    );
    let started = std::time::Instant::now();
    let (checked, exit) = run(root, &["check"]);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "a recursive schema must not hang"
    );
    assert_eq!(exit, 2);
    let fault = diagnostic(&checked, "SCHEMA_VIOLATION");
    assert_eq!(fault["path"], "trees/bad.json");
    assert_eq!(fault["pointer"], "/root/children/0/children/0", "{fault:#}");
}

#[test]
fn test3120_concurrent_transactions_and_readers_see_only_whole_states() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    pin(
        &root,
        "accounts",
        json!({
            "$schema": DIALECT, "type": "object",
            "properties": {"id": {"type": "string"}, "balance": {"type": "integer", "x-reldir-type": "int"}},
            "required": ["id", "balance"], "additionalProperties": false,
            "x-reldir": {"table": "accounts", "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}},
                         "checks": [{"name": "solvent", "expr": "balance >= 0"}]}
        }),
    );
    for account in ["a", "b", "c", "d"] {
        write_json(
            &root,
            &format!("accounts/{account}.json"),
            json!({"id": account, "balance": 100}),
        );
    }
    ok(&root, &["status"]);
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..2)
        .map(|_| {
            let (root, done) = (root.clone(), std::sync::Arc::clone(&done));
            std::thread::spawn(move || {
                let mut reads = 0;
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    let answer = ok(
                        &root,
                        &[
                            "sql",
                            "SELECT sum(balance) AS total, count(*) AS n FROM accounts",
                        ],
                    );
                    let row = &answer["records"][0]["row"];
                    assert_eq!(
                        (&row["total"], &row["n"]),
                        (&json!(400), &json!(4)),
                        "a read saw half a transfer"
                    );
                    reads += 1;
                }
                reads
            })
        })
        .collect();
    let pairs = [("a", "b"), ("b", "c"), ("c", "d"), ("d", "a")];
    let writers: Vec<_> = pairs
        .into_iter()
        .enumerate()
        .map(|(index, (from, to))| {
            let root = root.clone();
            std::thread::spawn(move || {
                let mut made = 0;
                for round in 0..4 {
                    let amount = 1 + (index + round) % 7;
                    let script = format!(
                        "BEGIN; UPDATE accounts SET balance = balance - {amount} WHERE id = '{from}'; \
                         UPDATE accounts SET balance = balance + {amount} WHERE id = '{to}'; COMMIT"
                    );
                    let (envelope, exit) = run(&root, &["--wait", "60", "sql", &script]);
                    assert_eq!(exit, 0, "{envelope:#}");
                    made += 1;
                }
                made
            })
        })
        .collect();
    let made: usize = writers
        .into_iter()
        .map(|writer| writer.join().unwrap())
        .sum();
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let reads: usize = readers
        .into_iter()
        .map(|reader| reader.join().unwrap())
        .sum();
    assert!(reads > 0);
    assert_eq!(made, 16);
    let total = ok(
        &root,
        &["sql", "SELECT sum(balance) AS total FROM accounts"],
    );
    assert_eq!(total["records"][0]["row"]["total"], 400);
    // One revision per transaction, after the first.
    assert_eq!(revision(&root), 17);
}

#[test]
fn test3130_an_export_holds_each_value_as_its_file_spells_it() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(
        root,
        "items/x1.json",
        r#"{"name": "b", "id": "x1", "price": 1.50, "big": 12345678901234567890123, "nested": {"z": 1, "a": 2.0}}"#,
    );
    ok(root, &["init", "--adopt"]);
    let out = tempfile::tempdir().unwrap();
    let exported = |name: &str| {
        let path = out.path().join(name);
        ok(root, &["export", "items", "--out", path.to_str().unwrap()]);
        fs::read_to_string(path).unwrap()
    };
    let row = r#"{"name":"b","id":"x1","price":1.50,"big":12345678901234567890123,"nested":{"z":1,"a":2.0}}"#;
    assert_eq!(exported("items.jsonl"), format!("{row}\n"));
    assert_eq!(exported("items.json"), format!("[\n  {row}\n]\n"));
    assert_eq!(
        exported("items.csv"),
        "name,id,price,big,nested\nb,x1,1.50,12345678901234567890123,\"{\"\"z\"\":1,\"\"a\"\":2.0}\"\n"
    );
}

#[test]
fn test3150_a_planned_change_retried_under_its_key_is_reported_not_planned_again() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    write(
        root,
        "../drop-name.json",
        r#"{"operations": [{"op": "drop_column", "table": "users", "column": "name"}]}"#,
    );
    let migration = root.parent().unwrap().join("drop-name.json");
    let migration = migration.to_str().unwrap();
    let retried = |args: &[&str]| {
        let first = ok(root, args);
        let (before, head) = (files(root), revision(root));
        let again = ok(root, args);
        assert_eq!(again["data"]["replayed"], true, "{again:#}");
        assert_eq!(again["data"]["revision"], first["data"]["revision"]);
        assert_eq!(files(root), before, "a replay changes no file");
        assert_eq!(revision(root), head, "a replay records nothing");
    };
    retried(&[
        "--yes",
        "--idempotency-key",
        "m1",
        "migrate",
        "apply",
        migration,
    ]);
    retried(&[
        "--idempotency-key",
        "m2",
        "migrate",
        "add-index",
        "posts",
        "title",
    ]);
    retried(&["--idempotency-key", "s1", "schema", "new", "notes"]);
    write_json(root, "labels/red.json", json!({"id": "red"}));
    retried(&["--idempotency-key", "i1", "infer", "labels", "--write"]);
    retried(&["--idempotency-key", "p1", "schema", "pin", "labels"]);
    retried(&[
        "--idempotency-key",
        "w1",
        "schema",
        "widen",
        "posts.title",
        "--null",
    ]);
    ok(root, &["snapshot", "create", "before"]);
    write_json(root, "tags/art.json", json!({"id": "art"}));
    retried(&[
        "--yes",
        "--idempotency-key",
        "r1",
        "snapshot",
        "restore",
        "before",
    ]);
}

#[test]
fn test3151_merged_heads_are_joined_as_a_merge_and_counted_from_this_clone() {
    let directory = blog();
    let ours = directory.path();
    ok(ours, &["init", "--adopt", "--track-provenance"]);
    let theirs_directory = tempfile::tempdir().unwrap();
    let theirs = theirs_directory.path();
    for (path, bytes) in everything(ours) {
        fs::create_dir_all(theirs.join(&path).parent().unwrap()).unwrap();
        fs::write(theirs.join(&path), bytes).unwrap();
    }
    write_json(theirs, "tags/art.json", json!({"id": "art"}));
    ok(theirs, &["status"]);
    write_json(ours, "tags/music.json", json!({"id": "music"}));
    ok(ours, &["status"]);
    // What a git merge of tracked history does: both clones' heads and
    // objects, and both clones' rows.
    for (path, bytes) in everything(theirs) {
        let history = path.starts_with(".db/history") || path.starts_with(".db/objects");
        if history || path == Path::new("tags/art.json") {
            fs::write(ours.join(&path), bytes).unwrap();
        }
    }
    let (read, _) = run(ours, &["--readonly", "status"]);
    assert!(
        codes(&read).contains(&"HISTORY_DIVERGED".to_string()),
        "{read:#}"
    );
    let joined = ok(ours, &["status"]);
    let recorded = joined["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event"] == "recorded")
        .unwrap_or_else(|| panic!("nothing was recorded: {joined:#}"));
    assert_eq!(recorded["origin"], "merge", "{joined:#}");
    assert_eq!(
        joined["data"]["state"], "VALID_CHANGED_EXTERNALLY",
        "{joined:#}"
    );
    assert!(
        !codes(&joined).contains(&"HISTORY_DIVERGED".to_string()),
        "the heads are joined: {joined:#}"
    );
    let log = ok(ours, &["log", "--limit", "1"]);
    let merge = &log["records"][0];
    assert_eq!(merge["origin"], "merge");
    assert_eq!(merge["parents"].as_array().unwrap().len(), 2);
    assert_eq!(
        merge["rows"],
        json!({"added": 1, "modified": 0, "removed": 0})
    );
}

#[test]
fn test3131_a_table_description_reads_as_tables_and_carries_as_a_document() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    let output = reldir(root)
        .args(["--quiet", "--format", "table", "describe", "users"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    assert!(
        lines[0].starts_with("table | schema_file")
            && lines[2].starts_with("users | schema/users.json"),
        "the table itself is one record:\n{text}"
    );
    let columns = lines
        .iter()
        .position(|line| line.starts_with("name | type"))
        .unwrap_or_else(|| panic!("the columns are a table:\n{text}"));
    assert!(lines[columns + 2].starts_with("id   | string"));
    let referrers = lines
        .iter()
        .position(|line| line.starts_with("table | constraint"))
        .unwrap_or_else(|| panic!("what references the table is a table:\n{text}"));
    assert!(lines[referrers + 2].starts_with("posts | posts.user_id -> users"));
    assert!(!text.contains('{'), "nothing is printed as JSON:\n{text}");

    let described = ok(root, &["describe", "users"]);
    let document = &described["records"][0];
    assert_eq!(document["type"], "table_description");
    assert_eq!(document["document"]["columns"][1]["name"], "name");
    assert_eq!(document["document"]["referenced_by"][0]["table"], "posts");
}

#[test]
fn test3132_doctor_fix_never_reports_success_over_faults_it_left() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    fs::remove_file(root.join("users/bob.json")).unwrap();

    // The one repair rewrites a row, which --fix alone is not allowed to do.
    let (refused, exit) = run(root, &["doctor", "--fix"]);
    assert_eq!(exit, 9, "{refused:#}");
    assert_eq!(refused["error"]["code"], "DECISION_REQUIRED");
    assert!(
        refused["error"]["help"]
            .as_str()
            .unwrap()
            .contains("--allow-data"),
        "{refused:#}"
    );
    assert!(!root.join("users/bob.json").exists());

    // A repair that is allowed is made, and the faults it leaves still say
    // the database is invalid.
    fs::rename(root.join("users/ada.json"), root.join("users/zed.json")).unwrap();
    let (repaired, exit) = run(root, &["doctor", "--fix"]);
    assert_eq!(exit, 2, "{repaired:#}");
    assert_eq!(repaired["data"]["applied"], 1);
    assert_eq!(repaired["data"]["valid"], false);
    assert!(root.join("users/ada.json").exists());

    let (repaired, exit) = run(root, &["doctor", "--fix", "--allow-data"]);
    assert_eq!(exit, 0, "{repaired:#}");
    assert_eq!(repaired["data"]["valid"], true);
}

#[test]
fn test3152_a_fault_names_a_restoration_only_when_history_holds_the_row() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    let fixes = |envelope: &Value, path: &str| -> Vec<String> {
        envelope["diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(std::iter::once(&envelope["error"]))
            .find(|d| d["code"] == "FOREIGN_KEY_VIOLATION" && d["path"] == path)
            .unwrap_or_else(|| panic!("no fault at {path}: {envelope:#}"))["fixes"]
            .as_array()
            .map(|fixes| {
                fixes
                    .iter()
                    .map(|f| f.as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    // A refused change wrote nothing: there is nothing for doctor to repair.
    let (refused, exit) = run(
        root,
        &[
            "insert",
            "posts",
            r#"{"id":"p3","user_id":"zed","title":"Ghost"}"#,
        ],
    );
    assert_ne!(exit, 0);
    assert_eq!(fixes(&refused, "posts/p3.json"), Vec::<String>::new());
    // A reference to a row history never held cannot be restored.
    write_json(
        root,
        "posts/p3.json",
        json!({"id": "p3", "user_id": "zed", "title": "Ghost"}),
    );
    let (never, _) = run(root, &["check"]);
    assert_eq!(fixes(&never, "posts/p3.json"), ["FIX_ORPHAN_DELETE_ROW"]);
    fs::remove_file(root.join("posts/p3.json")).unwrap();
    // A reference to a row history held can be.
    fs::remove_file(root.join("users/bob.json")).unwrap();
    let (gone, _) = run(root, &["check"]);
    assert_eq!(
        fixes(&gone, "posts/p2.json"),
        ["FIX_RESTORE_TARGET", "FIX_ORPHAN_DELETE_ROW"]
    );
    let doctor = ok(root, &["doctor"]);
    let offered: Vec<&str> = doctor["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|fix| fix["id"].as_str())
        .collect();
    assert_eq!(offered, ["FIX_RESTORE_TARGET", "FIX_ORPHAN_DELETE_ROW"]);
}

#[test]
fn test3153_adoption_leaves_out_what_it_is_told_to_ignore() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_json(root, "books/b1.json", json!({"id": "b1", "title": "Dune"}));
    write(root, "books/README.txt", "the books\n");
    write_json(root, "notes.json", json!({"draft": true}));
    let before = files(root);
    let (refused, exit) = run(root, &["init", "--adopt"]);
    assert_ne!(exit, 0);
    assert_eq!(codes(&refused)[0], "ROOT_JSON_AMBIGUOUS", "{refused:#}");
    let (refused, _) = run(root, &["init", "--adopt", "--ignore", "notes.json"]);
    let fault = diagnostic(&refused, "INFER_NON_JSON_FILE");
    assert_eq!(fault["path"], "books/README.txt");
    assert!(
        fault["help"]
            .as_str()
            .unwrap()
            .contains("--ignore books/README.txt"),
        "{fault:#}"
    );
    assert!(
        !root.join(".db").exists(),
        "a refused adoption leaves no .db/"
    );
    let (bad, exit) = run(root, &["init", "--adopt", "--ignore", "books/[README"]);
    assert_eq!(exit, 1, "{bad:#}");
    ok(
        root,
        &[
            "init",
            "--adopt",
            "--ignore",
            "books/README.txt",
            "--ignore",
            "notes.json",
        ],
    );
    assert_eq!(files(root), before, "adoption changes no file");
    let config: Value = serde_json::from_str(&read(root, ".db/config")).unwrap();
    let ignore = config["ignore"].as_array().unwrap();
    assert!(ignore.contains(&json!("books/README.txt")) && ignore.contains(&json!("notes.json")));
    let checked = ok(root, &["check"]);
    assert_eq!(checked["data"]["valid"], true, "{checked:#}");
    assert!(codes(&checked).is_empty(), "{checked:#}");
}

#[test]
fn test3154_a_row_is_restored_as_history_recorded_it() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["insert", "users", r#"{"id":"cy","name":"Cy"}"#]);
    let first = read(root, "users/cy.json");
    let named = revision(root);
    ok(root, &["update", "users", "cy", r#"{"name":"Cyrus"}"#]);
    let second = read(root, "users/cy.json");
    ok(root, &["delete", "users", "cy"]);
    assert!(!root.join("users/cy.json").exists());
    // By default, from the newest revision that holds the file.
    let restored = ok(root, &["restore", "users/cy.json"]);
    assert_eq!(read(root, "users/cy.json"), second, "{restored:#}");
    assert_eq!(restored["records"][0]["type"], "restored");
    // From a named revision, byte for byte.
    let named = named.to_string();
    ok(root, &["restore", "users/cy.json", "--at", &named]);
    assert_eq!(read(root, "users/cy.json"), first);
    // Nothing to do is not a change.
    let head = revision(root);
    let again = ok(root, &["restore", "users/cy.json", "--at", &named]);
    assert_eq!(revision(root), head, "{again:#}");
    // A path history never held, or that is no row file, is refused.
    let (unknown, _) = run(root, &["restore", "users/zed.json"]);
    assert_eq!(codes(&unknown)[0], "UNKNOWN_ROW");
    let (outside, exit) = run(root, &["restore", "../users/cy.json"]);
    assert_eq!((codes(&outside)[0].as_str(), exit), ("USAGE", 1));
    // A restored row is judged like any other change.
    ok(root, &["delete", "posts", "p2"]);
    ok(root, &["delete", "users", "bob"]);
    let (dangling, exit) = run(root, &["restore", "posts/p2.json"]);
    assert_ne!(exit, 0);
    assert_eq!(codes(&dangling)[0], "FOREIGN_KEY_VIOLATION");
    ok(root, &["restore", "users/bob.json", "posts/p2.json"]);
    assert!(root.join("posts/p2.json").exists());
}

#[test]
fn test3155_a_new_lineage_sets_a_corrupt_pack_aside_and_history_verifies_again() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    ok(root, &["insert", "users", r#"{"id":"cy","name":"Cy"}"#]);
    let packs = root.join(".db/objects/pack");
    let pack = fs::read_dir(&packs)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "pack"))
        .unwrap();
    let mut bytes = fs::read(&pack).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    fs::write(&pack, bytes).unwrap();
    let (deep, _) = run(root, &["check", "--deep"]);
    assert!(
        codes(&deep).contains(&"INTERNAL_METADATA_CORRUPT".to_string()),
        "{deep:#}"
    );
    ok(
        root,
        &["--allow-destructive", "recover", "--history", "new-lineage"],
    );
    // The corrupt pack is out of the live store, kept beside the old heads.
    assert!(!pack.exists());
    let quarantine = fs::read_dir(root.join(".db/history/quarantine"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(
        quarantine
            .join("packs")
            .join(pack.file_name().unwrap())
            .exists()
    );
    let deep = ok(root, &["check", "--deep"]);
    assert_eq!(deep["data"]["valid"], true, "{deep:#}");
    ok(root, &["gc"]);
    ok(root, &["check", "--deep"]);
    ok(root, &["update", "users", "cy", r#"{"name":"Cyrus"}"#]);
}

#[test]
fn test3156_a_transaction_recovery_cannot_resolve_is_named_with_what_to_do() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    let stranded = root.join(".db/transactions/01ARZ3NDEKTSV4RRFFQ69G5FAV");
    fs::create_dir_all(stranded.join("left-behind")).unwrap();
    let (refused, exit) = run(root, &["insert", "users", r#"{"id":"cy","name":"Cy"}"#]);
    assert_ne!(exit, 0);
    let fault = diagnostic(&refused, "TRANSACTION_INCOMPLETE");
    assert_eq!(
        fault["path"], ".db/transactions/01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "{fault:#}"
    );
    assert!(
        fault["help"]
            .as_str()
            .unwrap()
            .contains("move .db/transactions/01ARZ3NDEKTSV4RRFFQ69G5FAV out"),
        "{fault:#}"
    );
    assert!(!root.join("users/cy.json").exists());
    // Doing what the help says lets reldir run again.
    fs::rename(&stranded, root.join("stranded-transaction")).unwrap();
    fs::remove_dir_all(root.join("stranded-transaction")).unwrap();
    ok(root, &["insert", "users", r#"{"id":"cy","name":"Cy"}"#]);
}

#[test]
fn test3157_a_table_directory_changing_is_no_reason_to_examine_the_folder_whole() {
    let directory = blog();
    let root = directory.path();
    ok(root, &["status"]);
    let _monitor = monitor(root);
    ok(root, &["status"]);
    assert_eq!(ok(root, &["status"])["data"]["monitored"], true);

    // Windows reports a table's directory as changed beside every row that
    // changes in it; Linux reports it when the directory's own times change.
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    fs::File::open(root.join("users"))
        .unwrap()
        .set_modified(later)
        .unwrap();
    write_json(root, "users/cy.json", json!({"id": "cy", "name": "Cy"}));
    let after = ok(root, &["status"]);
    assert_eq!(after["data"]["monitored"], true, "{after:#}");
    assert_eq!(
        ok(root, &["sql", "SELECT count(*) AS n FROM users"])["records"][0]["row"]["n"],
        3
    );

    // A directory that is no table is still examined the slow way.
    fs::create_dir(root.join("drafts")).unwrap();
    assert_eq!(ok(root, &["status"])["data"]["monitored"], false);
}
