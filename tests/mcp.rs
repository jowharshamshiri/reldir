//! `reldir mcp`, driven by the official MCP client over stdio: the binary is
//! launched exactly as an agent's host would launch it.

use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ReadResourceRequestParams},
    transport::TokioChildProcess,
};
use serde_json::{Map, Value, json};
use std::{fs, path::Path};

const DIALECT: &str = "https://reldir.dev/schema/reldir-2";

fn database() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let write = |relative: &str, value: Value| {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    };
    write(
        "schema/users.json",
        json!({"$schema": DIALECT, "type": "object", "properties": {"id": {"type": "string"}, "name": {"type": "string"}},
               "required": ["id", "name"], "additionalProperties": false, "x-reldir": {"table": "users", "primaryKey": ["id"]}}),
    );
    write(
        "schema/posts.json",
        json!({"$schema": DIALECT, "type": "object", "properties": {"id": {"type": "string"}, "user_id": {"type": "string"}},
               "required": ["id", "user_id"], "additionalProperties": false,
               "x-reldir": {"table": "posts", "primaryKey": ["id"],
                            "foreignKeys": [{"from": ["user_id"], "to": {"table": "users"}, "onDelete": "restrict"}]}}),
    );
    write("users/ada.json", json!({"id": "ada", "name": "Ada"}));
    write("users/bob.json", json!({"id": "bob", "name": "Bob"}));
    write("posts/p1.json", json!({"id": "p1", "user_id": "ada"}));
    directory
}

fn arguments(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

async fn call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tool: &str,
    args: Value,
) -> (Value, bool) {
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments(args)))
        .await
        .unwrap();
    let envelope = result
        .structured_content
        .clone()
        .expect("every tool answers with the envelope");
    (envelope, result.is_error.unwrap_or(false))
}

fn launch(root: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_reldir"));
    command
        .arg("--db")
        .arg(root)
        .arg("mcp")
        .env_remove("RELDIR_DB");
    command
}

#[tokio::test(flavor = "current_thread")]
async fn test5001_an_agent_reads_plans_and_changes_through_the_same_rules() {
    let directory = database();
    let root = directory.path();
    let client = ().serve(TokioChildProcess::new(launch(root)).unwrap()).await.unwrap();

    let tools: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    for expected in [
        "query",
        "mutate",
        "check",
        "doctor_plan",
        "apply_fixes",
        "insert_row",
        "delete_row",
        "diff",
        "log",
    ] {
        assert!(
            tools.contains(&expected.to_string()),
            "{expected} in {tools:?}"
        );
    }

    let (answer, failed) = call(
        &client,
        "query",
        json!({"sql": "SELECT name FROM users WHERE id = ?", "params": ["ada"]}),
    )
    .await;
    assert!(!failed);
    assert_eq!(answer["records"][0]["name"], "Ada");

    let (refused, failed) = call(&client, "query", json!({"sql": "DELETE FROM users"})).await;
    assert!(failed, "query never changes data: {refused:#}");

    // A change defaults to a dry run: planned, validated, described, not written.
    let (planned, failed) = call(
        &client,
        "insert_row",
        json!({"table": "users", "row": {"id": "cy", "name": "Cy"}}),
    )
    .await;
    assert!(!failed, "{planned:#}");
    assert_eq!(planned["dry_run"], true);
    assert!(!root.join("users/cy.json").exists());
    let (_, failed) = call(
        &client,
        "insert_row",
        json!({"table": "users", "row": {"id": "cy", "name": "Cy"}, "dry_run": false}),
    )
    .await;
    assert!(!failed);
    assert!(root.join("users/cy.json").exists());

    // A change that would break a reference is refused, as a tool error the
    // agent can read.
    let (refused, failed) = call(
        &client,
        "delete_row",
        json!({"table": "users", "key": "ada", "dry_run": false, "confirm": true}),
    )
    .await;
    assert!(failed);
    assert_eq!(refused["error"]["code"], "FOREIGN_KEY_VIOLATION");
    assert_eq!(refused["error"]["path"], "posts/p1.json");

    // Removing data needs an explicit confirmation.
    let (declined, failed) = call(
        &client,
        "delete_row",
        json!({"table": "users", "key": "bob", "dry_run": false}),
    )
    .await;
    assert!(failed);
    assert_eq!(declined["error"]["code"], "DECISION_REQUIRED");
    assert!(root.join("users/bob.json").exists());
    let (declined, _) = call(
        &client,
        "mutate",
        json!({"sql": "DELETE FROM users WHERE id = 'bob'", "dry_run": false}),
    )
    .await;
    assert_eq!(declined["error"]["code"], "DECISION_REQUIRED");

    let (check, failed) = call(&client, "check", json!({})).await;
    assert!(!failed);
    assert_eq!(check["valid"], true);

    fs::remove_file(root.join("users/ada.json")).unwrap();
    let (plan, _) = call(&client, "doctor_plan", json!({})).await;
    assert_eq!(plan["records"][0]["id"], "FIX_RESTORE_TARGET");
    let (_, failed) = call(
        &client,
        "apply_fixes",
        json!({"allow_data": true, "dry_run": false}),
    )
    .await;
    assert!(!failed);
    assert!(
        root.join("users/ada.json").exists(),
        "restored from history"
    );

    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test5002_schemas_the_dialect_and_the_docs_are_resources() {
    let directory = database();
    let client = ().serve(TokioChildProcess::new(launch(directory.path())).unwrap()).await.unwrap();
    let uris: Vec<String> = client
        .list_all_resources()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.uri.clone())
        .collect();
    for expected in [
        "reldir://dialect",
        "reldir://schema/users",
        "reldir://docs/schemas",
    ] {
        assert!(
            uris.contains(&expected.to_string()),
            "{expected} in {uris:?}"
        );
    }
    let schema = client
        .read_resource(ReadResourceRequestParams::new("reldir://schema/posts"))
        .await
        .unwrap();
    let text = serde_json::to_value(&schema.contents[0]).unwrap()["text"]
        .as_str()
        .unwrap()
        .to_string();
    let document: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(document["x-reldir"]["table"], "posts");
    assert!(
        client
            .read_resource(ReadResourceRequestParams::new("reldir://nothing"))
            .await
            .is_err()
    );
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test5003_a_readonly_session_changes_nothing() {
    let directory = database();
    let root = directory.path();
    let mut readonly = tokio::process::Command::new(env!("CARGO_BIN_EXE_reldir"));
    readonly.args(["--readonly", "--db"]).arg(root).arg("mcp");
    let client = ().serve(TokioChildProcess::new(readonly).unwrap()).await.unwrap();
    let (refused, failed) = call(
        &client,
        "insert_row",
        json!({"table": "users", "row": {"id": "cy", "name": "Cy"}, "dry_run": false}),
    )
    .await;
    assert!(failed);
    assert_eq!(refused["error"]["code"], "READ_ONLY");
    assert!(
        !root.join(".db").exists(),
        "a read-only session writes nothing at all"
    );
    client.cancel().await.unwrap();
}
