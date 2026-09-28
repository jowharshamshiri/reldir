//! `reldir mcp`: the database, served to AI agents over the Model Context
//! Protocol on stdio.
//!
//! Every tool is one command from [`crate::command`], run exactly as the CLI
//! runs it and answered with the same `command_result` envelope `--format
//! json` prints. Changes go through the same validated transactions: an agent
//! can no more leave the files invalid than a person can. Tools that change
//! data default to a dry run, and anything that removes data needs
//! `confirm: true`.

use crate::{
    cli::{Command, SchemaCommand},
    command::Context,
    diagnostic::{DbError, Result},
    output::{Collect, Finish},
};
use rmcp::{
    ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ErrorData, Implementation, ListResourcesResult,
        ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
        ReadResourceResult, Resource, ResourceContents, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
    },
    service::{RequestContext, RoleServer},
};
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Documentation served as resources, so an agent can read how reldir works
/// from the server it is talking to.
const DOCS: &[(&str, &str)] = &[
    ("concepts", include_str!("../docs/concepts.md")),
    ("schemas", include_str!("../docs/schemas.md")),
    ("sql", include_str!("../docs/sql.md")),
    ("validation", include_str!("../docs/validation.md")),
    ("errors", include_str!("../docs/errors.md")),
    ("cli", include_str!("../docs/cli.md")),
];

#[derive(Clone)]
pub struct Server {
    context: Arc<Context>,
}

fn object(value: Value) -> Arc<Map<String, Value>> {
    Arc::new(value.as_object().cloned().unwrap_or_default())
}

fn params_schema() -> Value {
    json!({ "type": "array", "items": {}, "description": "Values for `?` placeholders, in order" })
}

/// The tools, their arguments, and what they may do.
pub fn tools() -> Vec<Tool> {
    let read = || ToolAnnotations::new().read_only(true);
    let write = |destructive: bool| ToolAnnotations::new().read_only(false).destructive(destructive);
    let table = json!({ "type": "string", "description": "A table name" });
    vec![
        Tool::new("status", "Validity, unrecorded changes and the current revision.", object(json!({"type": "object"})))
            .with_annotations(read()),
        Tool::new("tables", "The tables, with row counts and keys.", object(json!({"type": "object"}))).with_annotations(read()),
        Tool::new(
            "describe",
            "A table's columns, key, and references in and out.",
            object(json!({"type": "object", "properties": {"table": table}, "required": ["table"]})),
        )
        .with_annotations(read()),
        Tool::new(
            "schema_show",
            "A table's schema document (JSON Schema in reldir's dialect).",
            object(json!({"type": "object", "properties": {"table": table}, "required": ["table"]})),
        )
        .with_annotations(read()),
        Tool::new(
            "query",
            "Run a read-only SQL query (SQLite dialect) over the tables. JSON columns are queried with json_extract / json_each.",
            object(json!({
                "type": "object",
                "properties": {
                    "sql": {"type": "string"},
                    "params": params_schema(),
                    "allow_invalid": {"type": "boolean", "description": "Answer even if the database is invalid", "default": false}
                },
                "required": ["sql"]
            })),
        )
        .with_annotations(read()),
        Tool::new(
            "mutate",
            "Run an INSERT, UPDATE or DELETE. Referential actions apply, and the change is refused if it would leave any row invalid. Dry run unless dry_run is false.",
            object(json!({
                "type": "object",
                "properties": {
                    "sql": {"type": "string"},
                    "params": params_schema(),
                    "dry_run": {"type": "boolean", "default": true},
                    "confirm": {"type": "boolean", "default": false, "description": "Required for statements that delete rows"}
                },
                "required": ["sql"]
            })),
        )
        .with_annotations(write(true)),
        Tool::new(
            "insert_row",
            "Insert one row, or an array of rows.",
            object(json!({
                "type": "object",
                "properties": {"table": table, "row": {}, "dry_run": {"type": "boolean", "default": true}},
                "required": ["table", "row"]
            })),
        )
        .with_annotations(write(false)),
        Tool::new(
            "update_row",
            "Set columns of one row, found by its key.",
            object(json!({
                "type": "object",
                "properties": {"table": table, "key": {}, "patch": {"type": "object"}, "dry_run": {"type": "boolean", "default": true}},
                "required": ["table", "key", "patch"]
            })),
        )
        .with_annotations(write(false)),
        Tool::new(
            "delete_row",
            "Delete one row; referencing rows follow their onDelete actions, which the result lists.",
            object(json!({
                "type": "object",
                "properties": {
                    "table": table, "key": {},
                    "dry_run": {"type": "boolean", "default": true},
                    "confirm": {"type": "boolean", "default": false}
                },
                "required": ["table", "key"]
            })),
        )
        .with_annotations(write(true)),
        Tool::new(
            "check",
            "Validate everything and list every violation with its file, line and JSON Pointer.",
            object(json!({"type": "object", "properties": {"strict": {"type": "boolean", "default": false}}})),
        )
        .with_annotations(read()),
        Tool::new(
            "lint",
            "How the schemas could be stronger, each finding with the fix that applies it.",
            object(json!({"type": "object", "properties": {"table": table}})),
        )
        .with_annotations(read()),
        Tool::new(
            "doctor_plan",
            "Every repair doctor could make, least destructive first.",
            object(json!({"type": "object", "properties": {"only": {"type": "string", "description": "A FIX_ id or problem code"}}})),
        )
        .with_annotations(read()),
        Tool::new(
            "apply_fixes",
            "Apply doctor's default fixes (or the one named by `only`). Fixes that rewrite rows need allow_data; fixes that remove data need confirm.",
            object(json!({
                "type": "object",
                "properties": {
                    "only": {"type": "string"},
                    "allow_data": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": true},
                    "confirm": {"type": "boolean", "default": false}
                }
            })),
        )
        .with_annotations(write(true)),
        Tool::new(
            "diff",
            "What changed since the last recorded revision, or between two revisions.",
            object(json!({
                "type": "object",
                "properties": {"table": table, "from": {"type": "integer"}, "to": {"type": "integer"}, "schema_only": {"type": "boolean"}}
            })),
        )
        .with_annotations(read()),
        Tool::new(
            "log",
            "Recorded revisions, newest first.",
            object(json!({"type": "object", "properties": {"limit": {"type": "integer", "minimum": 1}}})),
        )
        .with_annotations(read()),
    ]
}

fn text(arguments: &Map<String, Value>, name: &str) -> Result<String> {
    match arguments.get(name) {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(other) => Ok(other.to_string()),
        None => Err(DbError::usage(format!("the {name:?} argument is required"))),
    }
}

fn flag(arguments: &Map<String, Value>, name: &str, default: bool) -> Result<bool> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(other) => Err(DbError::usage(format!("{name} is a boolean, not {other}"))),
    }
}

fn key(arguments: &Map<String, Value>) -> Result<String> {
    match arguments.get("key") {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(other) => Ok(crate::canonical::compact(other)),
        None => Err(DbError::usage("the \"key\" argument is required")),
    }
}

fn params(arguments: &Map<String, Value>) -> Result<Vec<String>> {
    match arguments.get("params") {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(items)) => Ok(items.iter().map(crate::canonical::compact).collect()),
        Some(other) => Err(DbError::usage(format!("params is an array, not {other}"))),
    }
}

/// Run one tool call to its envelope.
pub fn call(base: &Context, name: &str, arguments: &Map<String, Value>) -> Value {
    let mut sink = Collect::default();
    let outcome = run_tool(base, name, arguments, &mut sink);
    sink.envelope(name, &outcome)
}

fn run_tool(base: &Context, name: &str, arguments: &Map<String, Value>, sink: &mut Collect) -> Result<Finish> {
    let mut context = base.clone();
    let dry = |context: &mut Context| -> Result<()> {
        context.dry_run = flag(arguments, "dry_run", true)?;
        context.yes = flag(arguments, "confirm", false)?;
        Ok(())
    };
    let command = match name {
        "status" => Command::Status,
        "tables" => Command::Tables,
        "describe" => Command::Describe { table: text(arguments, "table")? },
        "schema_show" => Command::Schema(SchemaCommand::Show { table: text(arguments, "table")? }),
        "query" => {
            let sql = text(arguments, "sql")?;
            if crate::sql::classify(&sql)? != crate::sql::StatementKind::Read {
                return Err(DbError::usage("query runs read-only statements; use mutate to change rows"));
            }
            context.allow_invalid = flag(arguments, "allow_invalid", false)?;
            Command::Sql { statement: sql, params: params(arguments)? }
        }
        "mutate" => {
            let sql = text(arguments, "sql")?;
            if crate::sql::classify(&sql)? != crate::sql::StatementKind::Mutation {
                return Err(DbError::usage("mutate runs INSERT, UPDATE and DELETE; use query to read"));
            }
            dry(&mut context)?;
            Command::Sql { statement: sql, params: params(arguments)? }
        }
        "insert_row" => {
            dry(&mut context)?;
            let row = arguments.get("row").ok_or_else(|| DbError::usage("the \"row\" argument is required"))?;
            Command::Insert { table: text(arguments, "table")?, json: Some(row.to_string()), from: None }
        }
        "update_row" => {
            dry(&mut context)?;
            let patch = arguments.get("patch").ok_or_else(|| DbError::usage("the \"patch\" argument is required"))?;
            Command::Update { table: text(arguments, "table")?, key: key(arguments)?, patch: patch.to_string() }
        }
        "delete_row" => {
            dry(&mut context)?;
            if !context.dry_run && !context.yes {
                return Err(DbError::new("DECISION_REQUIRED", "deleting a row needs confirm: true", 9)
                    .with_help("run with dry_run first to see every row the delete touches"));
            }
            Command::Delete { table: text(arguments, "table")?, key: key(arguments)? }
        }
        "check" => Command::Check { strict: flag(arguments, "strict", false)? },
        "lint" => Command::Lint {
            table: arguments.get("table").and_then(Value::as_str).map(String::from),
            strict: false,
            descriptions: false,
        },
        "doctor_plan" => Command::Doctor {
            fix: false,
            allow_data: true,
            only: arguments.get("only").and_then(Value::as_str).map(String::from),
            explain: None,
            no_snapshot: true,
        },
        "apply_fixes" => {
            dry(&mut context)?;
            Command::Doctor {
                fix: true,
                allow_data: flag(arguments, "allow_data", false)?,
                only: arguments.get("only").and_then(Value::as_str).map(String::from),
                explain: None,
                no_snapshot: false,
            }
        }
        "diff" => Command::Diff {
            table: arguments.get("table").and_then(Value::as_str).map(String::from),
            from: arguments.get("from").and_then(Value::as_u64),
            to: arguments.get("to").and_then(Value::as_u64),
            schema: flag(arguments, "schema_only", false)?,
        },
        "log" => Command::Log { limit: arguments.get("limit").and_then(Value::as_u64).map(|n| n as usize) },
        other => return Err(DbError::usage(format!("there is no tool {other:?}"))),
    };
    crate::cli::dispatch(&context, sink, command)
}

impl Server {
    pub fn new(context: Context) -> Self {
        Self { context: Arc::new(context) }
    }

    fn resource(&self, uri: &str) -> Result<(String, &'static str)> {
        if uri == "reldir://dialect" {
            return Ok((serde_json::to_string_pretty(crate::schema::meta::meta_schema()).unwrap_or_default(), "application/schema+json"));
        }
        if let Some(page) = uri.strip_prefix("reldir://docs/")
            && let Some((_, text)) = DOCS.iter().find(|(name, _)| *name == page)
        {
            return Ok((text.to_string(), "text/markdown"));
        }
        if let Some(table) = uri.strip_prefix("reldir://schema/") {
            let envelope = call(&self.context, "schema_show", &json!({"table": table}).as_object().cloned().unwrap_or_default());
            if envelope["ok"] == true {
                let document = &envelope["records"][0]["document"];
                return Ok((serde_json::to_string_pretty(document).unwrap_or_default(), "application/schema+json"));
            }
            return Err(DbError::new("UNKNOWN_TABLE", envelope["summary"].as_str().unwrap_or("unknown table").to_string(), 4));
        }
        Err(DbError::new("UNKNOWN_RESOURCE", format!("there is no resource {uri}"), 4))
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_resources().build())
            .with_server_info(Implementation::new("reldir", crate::VERSION))
            .with_instructions(
                "reldir governs a folder of JSON files as a relational database. Read with `query` \
                 (SQLite SQL), change with `mutate` or the row tools -- they dry-run by default and \
                 refuse any change that would leave a file invalid -- and use `check`, `lint` and \
                 `doctor_plan` to find and repair problems. Every result is a command_result envelope; \
                 read `ok`, `summary`, `records` and `diagnostics`.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools().into_iter().find(|tool| tool.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        if !tools().iter().any(|tool| tool.name == name) {
            return Err(ErrorData::invalid_params(format!("there is no tool {name:?}"), None));
        }
        let arguments = request.arguments.unwrap_or_default();
        let context = Arc::clone(&self.context);
        // A command does blocking file I/O and holds a non-Send database, so
        // it runs start to finish on a blocking thread.
        let envelope = tokio::task::spawn_blocking(move || call(&context, &name, &arguments))
            .await
            .map_err(|error| ErrorData::internal_error(format!("the command panicked: {error}"), None))?;
        let result = if envelope["ok"] == true {
            CallToolResult::structured(envelope)
        } else {
            CallToolResult::structured_error(envelope)
        };
        Ok(result.into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourcesResult, ErrorData> {
        let context = Arc::clone(&self.context);
        let tables: Vec<String> = tokio::task::spawn_blocking(move || {
            let envelope = call(&context, "tables", &Map::new());
            envelope["records"]
                .as_array()
                .map(|records| records.iter().filter_map(|r| r["table"].as_str().map(String::from)).collect())
                .unwrap_or_default()
        })
        .await
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        let mut resources = vec![
            Resource::new("reldir://dialect", "dialect")
                .with_description("The JSON Schema dialect every table schema is written in")
                .with_mime_type("application/schema+json"),
        ];
        for (page, _) in DOCS {
            resources.push(Resource::new(format!("reldir://docs/{page}"), format!("docs/{page}")).with_mime_type("text/markdown"));
        }
        for table in tables {
            resources.push(
                Resource::new(format!("reldir://schema/{table}"), format!("schema/{table}"))
                    .with_mime_type("application/schema+json"),
            );
        }
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ReadResourceResponse, ErrorData> {
        let server = self.clone();
        let uri = request.uri.clone();
        let read = tokio::task::spawn_blocking(move || server.resource(&uri))
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        match read {
            Ok((text, mime)) => Ok(ReadResourceResult::new(vec![
                ResourceContents::text(text, request.uri.clone()).with_mime_type(mime),
            ])
            .into()),
            Err(error) => Err(ErrorData::resource_not_found(error.diagnostic.message.clone(), None)),
        }
    }
}

/// Serve on stdin and stdout until the client disconnects.
pub fn serve(context: Context) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| DbError::new("IO_ERROR", format!("cannot start the MCP runtime: {error}"), 6))?;
    runtime.block_on(async {
        let running = Server::new(context)
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|error| DbError::new("IO_ERROR", format!("the MCP session did not start: {error}"), 6))?;
        running
            .waiting()
            .await
            .map_err(|error| DbError::new("IO_ERROR", format!("the MCP session ended abnormally: {error}"), 6))?;
        Ok(())
    })
}
