//! `reldir mcp`, driven by the official MCP client over stdio: the binary is
//! launched exactly as an agent's host would launch it.

use rmcp::{
    ClientHandler, ServiceExt,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientConfig, ElicitRequestParams, ElicitResult,
        ElicitationAction, ErrorData, GetPromptRequestParams, Implementation,
        ReadResourceRequestParams,
    },
    service::{RequestContext, RoleClient, RunningService},
    transport::TokioChildProcess,
};
use serde_json::{Map, Value, json};
use std::{fs, path::Path};

const DIALECT: &str = "https://reldir.dev/schema/reldir-3";

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
               "required": ["id", "name"], "additionalProperties": false, "x-reldir": {"table": "users", "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}}}}),
    );
    write(
        "schema/posts.json",
        json!({"$schema": DIALECT, "type": "object", "properties": {"id": {"type": "string"}, "user_id": {"type": "string"}},
               "required": ["id", "user_id"], "additionalProperties": false,
               "x-reldir": {"table": "posts", "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}},
                            "foreignKeys": [{"from": ["user_id"], "to": {"table": "users"}, "onDelete": "cascade"}]}}),
    );
    write("users/ada.json", json!({"id": "ada", "name": "Ada"}));
    write("users/bob.json", json!({"id": "bob", "name": "Bob"}));
    write("posts/p1.json", json!({"id": "p1", "user_id": "ada"}));
    directory
}

fn arguments(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

/// A call's answer: (the result document, whether it is an error, its text).
async fn call<C: ClientHandler>(
    client: &RunningService<RoleClient, C>,
    tool: &str,
    args: Value,
) -> (Value, bool, String) {
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments(args)))
        .await
        .unwrap_or_else(|error| panic!("{tool} failed as a protocol error: {error}"));
    let envelope = result
        .structured_content
        .clone()
        .expect("every tool answers with the result document");
    assert_eq!(result.content.len(), 1, "one line of text beside it");
    let text = result.content[0]
        .as_text()
        .map(|text| text.text.clone())
        .unwrap_or_default();
    assert!(
        !text.trim_start().starts_with('{'),
        "the text is a line to read, not the document again: {text}"
    );
    (envelope, result.is_error.unwrap_or(false), text)
}

/// Plan a change, then commit the plan.
async fn plan_and_commit<C: ClientHandler>(
    client: &RunningService<RoleClient, C>,
    tool: &str,
    args: Value,
) -> Value {
    let (plan, failed, _) = call(client, tool, args.clone()).await;
    assert!(!failed, "{tool} did not plan: {plan:#}");
    let mut commit = args;
    commit["plan_id"] = plan["data"]["plan_id"].clone();
    let (done, failed, _) = call(client, tool, commit).await;
    assert!(!failed, "{tool} did not commit: {done:#}");
    done
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
async fn test5001_a_change_is_planned_then_committed_exactly_as_planned() {
    let directory = database();
    let root = directory.path();
    let client = ().serve(TokioChildProcess::new(launch(root)).unwrap()).await.unwrap();

    let tools = client.list_all_tools().await.unwrap();
    let tool = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("no tool {name}"))
    };
    for name in [
        "query",
        "mutate",
        "transaction",
        "check",
        "doctor_plan",
        "apply_fixes",
        "insert_row",
        "update_row",
        "delete_row",
        "get_row",
        "list_rows",
        "explain",
        "diff",
        "log",
        "show_revision",
        "migrate",
        "schema_widen",
        "snapshot_create",
        "snapshot_restore",
        "restore_rows",
        "import",
        "export",
        "doctor_explain",
        "infer",
    ] {
        let tool = tool(name);
        assert!(tool.output_schema.is_some(), "{name} declares its output");
        assert!(tool.title.is_some(), "{name} has a title");
    }
    let hints = |name: &str| tool(name).annotations.clone().unwrap();
    assert_eq!(hints("query").read_only_hint, Some(true));
    for name in [
        "update_row",
        "delete_row",
        "mutate",
        "apply_fixes",
        "transaction",
        "restore_rows",
    ] {
        assert_eq!(hints(name).destructive_hint, Some(true), "{name}");
    }
    assert_eq!(hints("insert_row").destructive_hint, Some(false));
    assert_eq!(hints("check").open_world_hint, Some(false));

    // Planning writes nothing, and says how to commit.
    let (plan, failed, text) = call(
        &client,
        "insert_row",
        json!({"table": "users", "row": {"id": "cy", "name": "Cy"}}),
    )
    .await;
    assert!(!failed, "{plan:#}");
    assert_eq!(plan["data"]["dry_run"], true);
    assert_eq!(plan["data"]["confirmation_required"], false);
    let plan_id = plan["data"]["plan_id"].as_str().unwrap().to_string();
    assert!(text.contains(&plan_id), "{text}");
    assert!(!root.join("users/cy.json").exists());

    // A plan commits only for the arguments it was made with.
    let (refused, failed, _) = call(
        &client,
        "insert_row",
        json!({"table": "users", "row": {"id": "cy", "name": "Not Cy"}, "plan_id": plan_id}),
    )
    .await;
    assert!(failed);
    assert_eq!(refused["error"]["code"], "PLAN_CHANGED");
    let (done, failed, _) = call(
        &client,
        "insert_row",
        json!({"table": "users", "row": {"id": "cy", "name": "Cy"}, "plan_id": plan_id}),
    )
    .await;
    assert!(!failed, "{done:#}");
    assert_eq!(done["data"]["dry_run"], false);
    assert!(root.join("users/cy.json").exists());
    // And once.
    let (again, _, _) = call(
        &client,
        "insert_row",
        json!({"table": "users", "row": {"id": "cy", "name": "Cy"}, "plan_id": plan_id}),
    )
    .await;
    assert_eq!(again["error"]["code"], "PLAN_CHANGED");

    // A plan is held to the state it was made against.
    let (plan, _, _) = call(
        &client,
        "update_row",
        json!({"table": "users", "key": "bob", "patch": {"name": "Robert"}}),
    )
    .await;
    assert_eq!(plan["data"]["overwrites"], 1);
    fs::write(
        root.join("users/bob.json"),
        "{\"id\": \"bob\", \"name\": \"Bobby\"}\n",
    )
    .unwrap();
    let (stale, failed, _) = call(
        &client,
        "update_row",
        json!({"table": "users", "key": "bob", "patch": {"name": "Robert"}, "plan_id": plan["data"]["plan_id"]}),
    )
    .await;
    assert!(failed);
    assert_eq!(stale["error"]["code"], "PLAN_CHANGED");
    assert!(
        fs::read_to_string(root.join("users/bob.json"))
            .unwrap()
            .contains("Bobby")
    );

    // What a plan removes is decided from the plan, however the statement is
    // spelled, and committing it needs a confirmation.
    let sql =
        "WITH gone AS (SELECT 'ada' AS id) DELETE FROM users WHERE id IN (SELECT id FROM gone)";
    let (plan, failed, text) = call(&client, "mutate", json!({"sql": sql})).await;
    assert!(!failed, "{plan:#}");
    assert_eq!(plan["data"]["confirmation_required"], true);
    let removes: Vec<String> = serde_json::from_value(plan["data"]["removes"].clone()).unwrap();
    assert!(
        removes
            .iter()
            .any(|reason| reason.contains("users/ada.json")),
        "{removes:?}"
    );
    assert!(
        removes
            .iter()
            .any(|reason| reason.contains("posts/p1.json")),
        "the cascade: {removes:?}"
    );
    assert!(text.contains("confirm"), "{text}");
    let (declined, failed, _) = call(
        &client,
        "mutate",
        json!({"sql": sql, "plan_id": plan["data"]["plan_id"]}),
    )
    .await;
    assert!(failed);
    assert_eq!(declined["error"]["code"], "DECISION_REQUIRED");
    assert!(root.join("users/ada.json").exists());
    let (done, failed, _) = call(
        &client,
        "mutate",
        json!({"sql": sql, "plan_id": plan["data"]["plan_id"], "confirm": true}),
    )
    .await;
    assert!(!failed, "{done:#}");
    assert!(!root.join("users/ada.json").exists() && !root.join("posts/p1.json").exists());

    // A query never changes anything.
    let (refused, failed, _) = call(&client, "query", json!({"sql": "DELETE FROM users"})).await;
    assert!(failed, "{refused:#}");

    let (check, failed, _) = call(&client, "check", json!({})).await;
    assert!(!failed);
    assert_eq!(check["data"]["valid"], true);

    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test5002_arguments_are_held_to_the_tool_schema() {
    let directory = database();
    let client = ().serve(TokioChildProcess::new(launch(directory.path())).unwrap()).await.unwrap();
    for (tool, args) in [
        ("query", json!({"sql": "SELECT 1", "limt": 5})),
        ("query", json!({"sql": 7})),
        ("diff", json!({"from": 1})),
        (
            "insert_row",
            json!({"table": "users", "row": {"id": "x"}, "dry_run": false}),
        ),
        ("check", json!({"strict": "yes"})),
    ] {
        let error = client
            .call_tool(
                CallToolRequestParams::new(tool.to_string())
                    .with_arguments(arguments(args.clone())),
            )
            .await
            .expect_err(&format!("{tool} {args}"));
        assert!(error.to_string().contains("not valid"), "{tool}: {error}");
    }
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test5003_a_query_answers_a_page_at_a_time_from_one_state() {
    let directory = database();
    let root = directory.path();
    for n in 0..7 {
        fs::write(
            root.join(format!("users/u{n}.json")),
            format!("{{\"id\": \"u{n}\", \"name\": \"U{n}\"}}\n"),
        )
        .unwrap();
    }
    let client = ().serve(TokioChildProcess::new(launch(root)).unwrap()).await.unwrap();
    let sql = "SELECT id FROM users WHERE id LIKE ? ORDER BY id";
    let mut seen = vec![];
    let mut cursor: Option<String> = None;
    loop {
        let mut args = json!({"sql": sql, "params": ["u%"], "limit": 3});
        if let Some(cursor) = &cursor {
            args["cursor"] = json!(cursor);
        }
        let (page, failed, _) = call(&client, "query", args).await;
        assert!(!failed, "{page:#}");
        for record in page["records"].as_array().unwrap() {
            seen.push(record["row"]["id"].as_str().unwrap().to_string());
        }
        match page["data"]["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
    }
    assert_eq!(seen, (0..7).map(|n| format!("u{n}")).collect::<Vec<_>>());

    // A cursor continues only its own query, and only in its own state.
    let (first, _, _) = call(
        &client,
        "query",
        json!({"sql": sql, "params": ["u%"], "limit": 3}),
    )
    .await;
    let next = first["data"]["next_cursor"].as_str().unwrap().to_string();
    let (other, failed, _) = call(
        &client,
        "query",
        json!({"sql": sql, "params": ["x%"], "limit": 3, "cursor": next}),
    )
    .await;
    assert!(failed);
    assert_eq!(other["error"]["code"], "USAGE");
    fs::write(
        root.join("users/u9.json"),
        "{\"id\": \"u9\", \"name\": \"U9\"}\n",
    )
    .unwrap();
    let (stale, failed, _) = call(
        &client,
        "query",
        json!({"sql": sql, "params": ["u%"], "limit": 3, "cursor": next}),
    )
    .await;
    assert!(failed);
    assert_eq!(stale["error"]["code"], "CURSOR_STALE");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test5004_transactions_migrations_and_repairs_go_through_plans() {
    let directory = database();
    let root = directory.path();
    let client = ().serve(TokioChildProcess::new(launch(root)).unwrap()).await.unwrap();

    let done = plan_and_commit(
        &client,
        "transaction",
        json!({"statements": [
            "INSERT INTO posts (id, user_id) VALUES ('p2', 'dee')",
            "INSERT INTO users (id, name) VALUES ('dee', 'Dee')"
        ]}),
    )
    .await;
    assert_eq!(done["data"]["statements"], 2);
    assert!(root.join("posts/p2.json").exists() && root.join("users/dee.json").exists());

    let (refused, failed, _) = call(
        &client,
        "transaction",
        json!({"statements": ["BEGIN", "DELETE FROM posts"]}),
    )
    .await;
    assert!(failed);
    assert_eq!(refused["error"]["code"], "TRANSACTION_STATEMENT");

    plan_and_commit(
        &client,
        "migrate",
        json!({"operations": [{"op": "add_column", "table": "users", "column": "bio", "type": "string", "nullable": true}]}),
    )
    .await;
    let schema: Value =
        serde_json::from_str(&fs::read_to_string(root.join("schema/users.json")).unwrap()).unwrap();
    assert!(schema["properties"].get("bio").is_some());

    // A row a post still references, deleted outside reldir, is a fault;
    // doctor restores it from history, through a plan.
    fs::remove_file(root.join("users/ada.json")).unwrap();
    let (plan, _, _) = call(&client, "doctor_plan", json!({})).await;
    assert_eq!(plan["records"][0]["id"], "FIX_RESTORE_TARGET", "{plan:#}");
    plan_and_commit(&client, "apply_fixes", json!({"allow_data": true})).await;
    assert!(
        root.join("users/ada.json").exists(),
        "restored from history"
    );

    let (row, failed, _) = call(&client, "get_row", json!({"table": "users", "key": "dee"})).await;
    assert!(!failed);
    assert_eq!(row["records"][0]["row"]["name"], "Dee");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test5005_schemas_rows_the_dialect_docs_and_prompts_are_served() {
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
        "reldir://protocol",
        "reldir://schema/users",
        "reldir://docs/schemas",
    ] {
        assert!(
            uris.contains(&expected.to_string()),
            "{expected} in {uris:?}"
        );
    }
    let templates: Vec<String> = client
        .list_all_resource_templates()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.uri_template.clone())
        .collect();
    assert!(
        templates.contains(&"reldir://row/{table}/{key}".to_string()),
        "{templates:?}"
    );

    let text = |result: rmcp::model::ReadResourceResult| {
        serde_json::to_value(&result.contents[0]).unwrap()["text"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let schema = client
        .read_resource(ReadResourceRequestParams::new("reldir://schema/posts"))
        .await
        .unwrap();
    let document: Value = serde_json::from_str(&text(schema)).unwrap();
    assert_eq!(document["x-reldir"]["table"], "posts");
    let row = client
        .read_resource(ReadResourceRequestParams::new("reldir://row/users/ada"))
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&text(row)).unwrap()["name"],
        "Ada"
    );
    let docs = client
        .read_resource(ReadResourceRequestParams::new("reldir://docs/sql"))
        .await
        .unwrap();
    assert!(!text(docs).contains("{{"), "site markup is rendered away");
    for missing in [
        "reldir://nothing",
        "reldir://row/users/nobody",
        "reldir://schema/nowhere",
    ] {
        let error = client
            .read_resource(ReadResourceRequestParams::new(missing))
            .await
            .expect_err(missing);
        assert!(
            error.to_string().to_lowercase().contains("not found")
                || error.to_string().contains("-32002"),
            "{missing}: {error}"
        );
    }

    let prompts: Vec<String> = client
        .list_all_prompts()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(
        prompts,
        ["explore-database", "design-schema", "repair-database"]
    );
    let prompt = client
        .get_prompt(
            GetPromptRequestParams::new("design-schema")
                .with_arguments(arguments(json!({"table": "invoices"}))),
        )
        .await
        .unwrap();
    let message = serde_json::to_value(&prompt.messages[0]).unwrap();
    assert!(message.to_string().contains("invoices"));
    assert!(
        client
            .get_prompt(GetPromptRequestParams::new("design-schema"))
            .await
            .is_err(),
        "a required argument is required"
    );
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test5006_a_readonly_session_changes_nothing() {
    let directory = database();
    let root = directory.path();
    let mut readonly = tokio::process::Command::new(env!("CARGO_BIN_EXE_reldir"));
    readonly.args(["--readonly", "--db"]).arg(root).arg("mcp");
    let client = ().serve(TokioChildProcess::new(readonly).unwrap()).await.unwrap();
    let (refused, failed, _) = call(
        &client,
        "insert_row",
        json!({"table": "users", "row": {"id": "cy", "name": "Cy"}}),
    )
    .await;
    assert!(failed);
    assert_eq!(refused["error"]["code"], "READ_ONLY");
    let (answer, failed, _) = call(
        &client,
        "query",
        json!({"sql": "SELECT count(*) AS n FROM users"}),
    )
    .await;
    assert!(!failed);
    assert_eq!(answer["records"][0]["row"]["n"], 2);
    assert!(
        !root.join(".db").exists(),
        "a read-only session writes nothing at all"
    );
    client.cancel().await.unwrap();
}

/// A client whose user answers every confirmation as `agree` says.
#[derive(Clone)]
struct User {
    agree: bool,
}

impl ClientHandler for User {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::builder().enable_elicitation().build(),
            Implementation::new("test-user", "1"),
        )
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        let ElicitRequestParams::FormElicitationParams { message, .. } = request else {
            return Ok(ElicitResult::new(ElicitationAction::Decline));
        };
        assert!(
            message.contains("users/bob.json"),
            "the user is told what is removed: {message}"
        );
        Ok(if self.agree {
            ElicitResult::new(ElicitationAction::Accept).with_content(json!({"confirm": true}))
        } else {
            ElicitResult::new(ElicitationAction::Decline)
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test5007_a_client_that_can_ask_its_user_asks_them() {
    for agree in [false, true] {
        let directory = database();
        let root = directory.path();
        let client = User { agree }
            .serve(TokioChildProcess::new(launch(root)).unwrap())
            .await
            .unwrap();
        let args = json!({"table": "users", "key": "bob"});
        let (plan, _, _) = call(&client, "delete_row", args.clone()).await;
        let mut commit = args;
        commit["plan_id"] = plan["data"]["plan_id"].clone();
        let (answer, failed, _) = call(&client, "delete_row", commit).await;
        assert_eq!(!failed, agree, "{answer:#}");
        assert_eq!(root.join("users/bob.json").exists(), !agree);
        if !agree {
            assert_eq!(answer["error"]["code"], "DECISION_REQUIRED");
        }
        client.cancel().await.unwrap();
    }
}

/// A client that passes on every resource notification it hears.
#[derive(Clone)]
struct Listener {
    heard: tokio::sync::mpsc::UnboundedSender<String>,
}

impl ClientHandler for Listener {
    async fn on_resource_updated(
        &self,
        params: rmcp::model::ResourceUpdatedNotificationParam,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) {
        let _ = self.heard.send(format!("updated {}", params.uri));
    }

    async fn on_resource_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) {
        let _ = self.heard.send("list changed".into());
    }
}

// `resources/subscribe` is what clients that negotiate 2025-11-25 -- every
// client that opens with `initialize` -- subscribe with.
#[allow(deprecated)]
#[tokio::test(flavor = "current_thread")]
async fn test5008_subscribers_hear_of_the_changes_a_commit_makes() {
    let directory = database();
    let root = directory.path();
    let (sender, mut heard) = tokio::sync::mpsc::unbounded_channel();
    let client = Listener { heard: sender }
        .serve(TokioChildProcess::new(launch(root)).unwrap())
        .await
        .unwrap();
    for uri in [
        "reldir://row/users/bob",
        "reldir://row/users/ada",
        "reldir://schema/posts",
    ] {
        client
            .subscribe(rmcp::model::SubscribeRequestParams::new(uri))
            .await
            .unwrap();
    }
    client
        .unsubscribe(rmcp::model::UnsubscribeRequestParams::new(
            "reldir://row/users/ada",
        ))
        .await
        .unwrap();
    let next = |heard: &mut tokio::sync::mpsc::UnboundedReceiver<String>| {
        let mut received = vec![];
        while let Ok(message) = heard.try_recv() {
            received.push(message);
        }
        received.sort();
        received
    };

    plan_and_commit(
        &client,
        "update_row",
        json!({"table": "users", "key": "bob", "patch": {"name": "Robert"}}),
    )
    .await;
    // Notifications arrive before the reply that follows them.
    call(&client, "status", json!({})).await;
    assert_eq!(next(&mut heard), ["updated reldir://row/users/bob"]);

    // A plan is not a change.
    call(
        &client,
        "update_row",
        json!({"table": "users", "key": "bob", "patch": {"name": "Bert"}}),
    )
    .await;
    call(&client, "status", json!({})).await;
    assert!(next(&mut heard).is_empty());

    // A schema change is a change to the list of resources, too.
    plan_and_commit(
        &client,
        "migrate",
        json!({"operations": [{"op": "add_column", "table": "posts", "column": "title", "type": "string", "nullable": true}]}),
    )
    .await;
    call(&client, "status", json!({})).await;
    assert_eq!(
        next(&mut heard),
        ["list changed", "updated reldir://schema/posts"]
    );
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test5009_an_agent_brings_a_deleted_row_back_from_history() {
    let directory = database();
    let root = directory.path();
    let client = ().serve(TokioChildProcess::new(launch(root)).unwrap()).await.unwrap();
    let bob = fs::read(root.join("users/bob.json")).unwrap();
    let (plan, failed, _) = call(
        &client,
        "delete_row",
        json!({"table": "users", "key": "bob"}),
    )
    .await;
    assert!(!failed, "{plan:#}");
    let (_, failed, _) = call(
        &client,
        "delete_row",
        json!({"table": "users", "key": "bob", "plan_id": plan["data"]["plan_id"], "confirm": true}),
    )
    .await;
    assert!(!failed);
    assert!(!root.join("users/bob.json").exists());

    let (plan, failed, _) = call(
        &client,
        "restore_rows",
        json!({"paths": ["users/bob.json"]}),
    )
    .await;
    assert!(!failed, "{plan:#}");
    assert_eq!(plan["data"]["dry_run"], true);
    assert!(
        !root.join("users/bob.json").exists(),
        "a plan writes nothing"
    );
    let (done, failed, _) = call(
        &client,
        "restore_rows",
        json!({"paths": ["users/bob.json"], "plan_id": plan["data"]["plan_id"], "confirm": true}),
    )
    .await;
    assert!(!failed, "{done:#}");
    assert_eq!(fs::read(root.join("users/bob.json")).unwrap(), bob);
}
