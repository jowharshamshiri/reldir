//! `reldir mcp`: the database, served to AI agents over the Model Context
//! Protocol on stdio.
//!
//! Every tool runs a command from [`crate::command`] exactly as the CLI runs
//! it, and answers with the protocol's `result` document as structured
//! content beside one line of text. Changes go through the same validated
//! transactions as everywhere else, in two steps: a call plans the change --
//! validates it and lists every file it would touch, writing nothing -- and
//! returns a `plan_id`; the call that passes the `plan_id` back commits
//! exactly that change, provided the database is still in the state it was
//! planned against. A plan that removes rows or runs referential actions is
//! confirmed by the user, through elicitation when the client offers it.

mod prompts;
pub mod tools;

use crate::{
    command::{Context, Intent, query},
    output::{Collect, Finish},
};
use reldir_core::{
    code::{Code, Exit},
    diagnostic::{DbError, Result},
    store::Hash256,
};
use rmcp::{
    ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ElicitRequestParams,
        ElicitationAction, ElicitationSchema, ErrorData, GetPromptRequestParams, GetPromptResponse,
        Implementation, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
        ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
        ReadResourceResult, Resource, ResourceContents, ResourceTemplate,
        ResourceUpdatedNotificationParam, ServerCapabilities, ServerConfig, SubscribeRequestParams,
        SubscriptionFilter, Tool, UnsubscribeRequestParams,
    },
    service::{Peer, RequestContext, RoleServer, SubscriptionContext, SubscriptionSink},
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tools::{Action, Effect};

/// How many plans the server keeps waiting to be committed; the oldest is
/// forgotten first.
const PLANS_KEPT: usize = 64;

/// A change planned and waiting to be committed.
#[derive(Debug, Clone)]
struct Plan {
    id: String,
    tool: String,
    /// The arguments it was planned with, as canonical JSON.
    arguments: String,
    /// The state it was planned against.
    state: String,
    /// What it removes: why committing it needs the user's confirmation.
    removes: Vec<String>,
}

/// Who is to be told when a resource changes.
#[derive(Default)]
struct Subscriptions {
    /// URIs subscribed with `resources/subscribe`, and the client to tell.
    legacy: BTreeSet<String>,
    peer: Option<Peer<RoleServer>>,
    /// Streams opened with `subscriptions/listen`.
    listeners: BTreeMap<u64, (SubscriptionSink, SubscriptionFilter)>,
}

#[derive(Clone)]
pub struct Server {
    context: Arc<Context>,
    plans: Arc<Mutex<VecDeque<Plan>>>,
    subscriptions: Arc<Mutex<Subscriptions>>,
    counter: Arc<AtomicU64>,
}

/// A tool's answer: the protocol document, and the line a person reads.
struct Answer {
    envelope: Value,
    line: String,
}

fn canonical(arguments: &Map<String, Value>) -> String {
    let mut kept = arguments.clone();
    kept.remove("plan_id");
    kept.remove("confirm");
    reldir_core::canonical::compact(&Value::Object(kept))
}

fn failure(name: &str, error: DbError) -> Value {
    Collect::default().envelope(name, &Err(error))
}

/// One line saying how the call ended.
fn line(envelope: &Value) -> String {
    let summary = envelope["summary"].as_str().unwrap_or_default();
    if envelope["ok"] == true {
        return summary.to_string();
    }
    let code = envelope["error"]["code"].as_str().unwrap_or("ERROR");
    match envelope["error"]["help"].as_str() {
        Some(help) => format!("{code}: {summary} ({help})"),
        None => format!("{code}: {summary}"),
    }
}

/// What a planned change removes: rows and files it deletes, and the
/// referential actions it runs.
fn removals(envelope: &Value) -> Vec<String> {
    let mut out = vec![];
    for record in envelope["records"].as_array().into_iter().flatten() {
        if record["type"] == "change" && record["action"] == "delete" {
            out.push(format!(
                "deletes {}",
                record["path"].as_str().unwrap_or_default()
            ));
        }
    }
    for event in envelope["events"].as_array().into_iter().flatten() {
        if event["event"] == "referential_action" {
            out.push(format!(
                "{} {} ({})",
                event["action"].as_str().unwrap_or_default(),
                event["path"].as_str().unwrap_or_default(),
                event["constraint"].as_str().unwrap_or_default()
            ));
        }
    }
    out
}

/// Rewrites of files that exist: reported, and restorable from history.
fn overwrites(root: &std::path::Path, envelope: &Value) -> usize {
    envelope["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|record| {
            record["type"] == "change"
                && record["action"] == "write"
                && record["path"]
                    .as_str()
                    .is_some_and(|path| root.join(path).exists())
        })
        .count()
}

/// The tables and schemas a committed change touched, from its change
/// records: `(tables whose rows changed, tables whose schema changed)`.
fn touched(envelope: &Value) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut rows = BTreeSet::new();
    let mut schemas = BTreeSet::new();
    for record in envelope["records"].as_array().into_iter().flatten() {
        if record["type"] != "change" {
            continue;
        }
        let Some(path) = record["path"].as_str() else {
            continue;
        };
        let parts: Vec<&str> = path.split('/').collect();
        match parts.as_slice() {
            ["schema", file] | [".db", "schema", file] => {
                if let Some(table) = file.strip_suffix(".json") {
                    schemas.insert(table.to_string());
                }
            }
            [first, ..] if !first.starts_with('.') => {
                rows.insert(first.to_string());
            }
            _ => {}
        }
    }
    (rows, schemas)
}

impl Server {
    pub fn new(context: Context) -> Self {
        Self {
            context: Arc::new(context),
            plans: Arc::new(Mutex::new(VecDeque::new())),
            subscriptions: Arc::new(Mutex::new(Subscriptions::default())),
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Run a tool's action to its envelope, on this thread.
    fn execute(context: &Context, name: &str, action: Action) -> Value {
        let mut sink = Collect::default();
        let outcome: Result<Finish> = match action {
            Action::Command(command) => crate::cli::dispatch(context, &mut sink, command),
            Action::Script(statements) => query::script(context, &mut sink, &statements),
            Action::Migration(migration) => {
                crate::command::schema::migrate(context, &mut sink, migration)
            }
            Action::Page {
                sql,
                params,
                window,
                expect_state,
                fingerprint,
            } => query::page(
                context,
                &mut sink,
                &sql,
                &params,
                window,
                expect_state.as_deref(),
            )
            .map(|finish| {
                if finish.fields.get("more") == Some(&Value::Bool(true))
                    && let Some(state) = finish.fields.get("state").and_then(Value::as_str)
                {
                    let next = window.skip + finish.fields["rows"].as_u64().unwrap_or(0) as usize;
                    let cursor = tools::cursor(next, state, &fingerprint);
                    let summary = format!("{}; continue with cursor {cursor}", finish.summary);
                    let mut finish = finish.with("next_cursor", cursor);
                    finish.summary = summary;
                    finish
                } else {
                    finish
                }
            }),
        };
        sink.envelope(name, &outcome)
    }

    /// A planned tool's first call: run as a dry run, and keep the plan.
    fn plan(&self, context: &Context, name: &str, arguments: &Map<String, Value>) -> Value {
        let mut context = context.clone();
        // Planning answers what committing would do, so a folder that is not
        // yet a database is established first, exactly as any read would.
        if !context.readonly
            && let Ok(root) = context.root()
            && !root.join(".db").join("format").exists()
        {
            let mut quiet = Collect::default();
            if let Err(error) = context.open(&mut quiet, Intent::Read) {
                return failure(name, error);
            }
        }
        context.dry_run = true;
        let action = match tools::action(name, arguments, &mut context) {
            Ok(action) => action,
            Err(error) => return failure(name, error),
        };
        let mut envelope = Self::execute(&context, name, action);
        if envelope["ok"] != true {
            return envelope;
        }
        let Some(state) = envelope["data"]["state"].as_str().map(String::from) else {
            return failure(
                name,
                DbError::new(
                    Code::INTERNAL_ERROR,
                    format!("{name} planned a change without naming the state it planned against"),
                ),
            );
        };
        let removes = removals(&envelope);
        let rewrites = context
            .root()
            .map(|root| overwrites(&root, &envelope))
            .unwrap_or(0);
        let serial = self.counter.fetch_add(1, Ordering::Relaxed);
        let id = Hash256::of(
            format!(
                "{name}\0{}\0{state}\0{serial}\0{}",
                canonical(arguments),
                std::process::id()
            )
            .as_bytes(),
        )
        .to_hex()[..24]
            .to_string();
        let plan = Plan {
            id: id.clone(),
            tool: name.to_string(),
            arguments: canonical(arguments),
            state,
            removes: removes.clone(),
        };
        {
            let mut plans = self.plans.lock().expect("the plan list is never poisoned");
            plans.push_back(plan);
            while plans.len() > PLANS_KEPT {
                plans.pop_front();
            }
        }
        let confirm = !removes.is_empty();
        let data = envelope["data"]
            .as_object_mut()
            .expect("an envelope's data is an object");
        data.insert("plan_id".into(), json!(id));
        data.insert("removes".into(), json!(removes));
        data.insert("overwrites".into(), json!(rewrites));
        data.insert("confirmation_required".into(), json!(confirm));
        let summary = envelope["summary"].as_str().unwrap_or_default().to_string();
        envelope["summary"] = json!(format!(
            "{summary}; to commit, call {name} again with the same arguments and plan_id {id}{}",
            if confirm {
                ", which the user must confirm: it removes data"
            } else {
                ""
            }
        ));
        envelope
    }

    /// The plan a commit names, if it was made for these arguments.
    fn planned(&self, name: &str, arguments: &Map<String, Value>, id: &str) -> Result<Plan> {
        let plans = self.plans.lock().expect("the plan list is never poisoned");
        let plan = plans
            .iter()
            .find(|plan| plan.id == id)
            .cloned()
            .ok_or_else(|| {
                DbError::new(
                Code::PLAN_CHANGED,
                format!(
                    "this server holds no plan {id}: it was committed already, forgotten for newer \
                     plans, or made before the server started; nothing was written"
                ),
            )
            .with_help("plan the change again by calling the tool without plan_id")
            })?;
        if plan.tool != name || plan.arguments != canonical(arguments) {
            return Err(DbError::new(
                Code::PLAN_CHANGED,
                format!(
                    "plan {id} was made for {} with other arguments; a plan commits only the \
                     change it describes, so nothing was written",
                    plan.tool
                ),
            )
            .with_help("call the tool with exactly the arguments of the plan, or plan again"));
        }
        Ok(plan)
    }

    fn forget(&self, id: &str) {
        self.plans
            .lock()
            .expect("the plan list is never poisoned")
            .retain(|plan| plan.id != id);
    }

    /// Ask the user whether to commit a plan that removes data.
    async fn confirmed(
        &self,
        plan: &Plan,
        arguments: &Map<String, Value>,
        peer: &Peer<RoleServer>,
    ) -> Result<()> {
        if plan.removes.is_empty() || arguments.get("confirm") == Some(&Value::Bool(true)) {
            return Ok(());
        }
        let elicits = peer
            .peer_info()
            .is_some_and(|info| info.capabilities.elicitation.is_some());
        if !elicits {
            return Err(DbError::new(
                Code::DECISION_REQUIRED,
                format!(
                    "this plan removes data ({}), so committing it needs confirm: true",
                    plan.removes.join("; ")
                ),
            )
            .with_help(
                "show the plan to the user, and pass confirm: true only with their agreement",
            ));
        }
        let schema = ElicitationSchema::builder()
            .required_bool("confirm")
            .build()
            .map_err(|error| DbError::new(Code::INTERNAL_ERROR, error.to_string()))?;
        let message = format!(
            "An agent asks to commit a change to the database with {}: it {}. Commit it?",
            plan.tool,
            plan.removes.join("; ")
        );
        let answer = peer
            .create_elicitation(ElicitRequestParams::FormElicitationParams {
                meta: None,
                message,
                requested_schema: schema,
            })
            .await
            .map_err(|error| {
                DbError::new(
                    Code::DECISION_REQUIRED,
                    format!("the user could not be asked to confirm the plan: {error}"),
                )
            })?;
        let agreed = answer.action == ElicitationAction::Accept
            && answer
                .content
                .as_ref()
                .and_then(|content| content.get("confirm"))
                .and_then(Value::as_bool)
                == Some(true);
        if agreed {
            Ok(())
        } else {
            Err(DbError::new(
                Code::DECISION_REQUIRED,
                "the user did not confirm the plan; nothing was written",
            ))
        }
    }

    /// Tell subscribers what a committed change touched.
    async fn announce(&self, envelope: &Value) {
        let (rows, schemas) = touched(envelope);
        if rows.is_empty() && schemas.is_empty() {
            return;
        }
        let affected = |uri: &str| -> bool {
            if let Some(table) = uri.strip_prefix("reldir://schema/") {
                return schemas.contains(table);
            }
            if let Some(rest) = uri.strip_prefix("reldir://row/") {
                let table = rest.split('/').next().unwrap_or_default();
                return rows.contains(table) || schemas.contains(table);
            }
            false
        };
        let (legacy, peer, listeners) = {
            let subscriptions = self
                .subscriptions
                .lock()
                .expect("the subscription list is never poisoned");
            let legacy: Vec<String> = subscriptions
                .legacy
                .iter()
                .filter(|uri| affected(uri))
                .cloned()
                .collect();
            let listeners: Vec<(SubscriptionSink, SubscriptionFilter)> =
                subscriptions.listeners.values().cloned().collect();
            (legacy, subscriptions.peer.clone(), listeners)
        };
        // The set of tables, and so of resources, changes with a schema.
        let listed = !schemas.is_empty();
        if let Some(peer) = peer {
            for uri in legacy {
                let _ = peer
                    .notify_resource_updated(ResourceUpdatedNotificationParam::new(uri))
                    .await;
            }
            if listed {
                let _ = peer.notify_resource_list_changed().await;
            }
        }
        for (sink, filter) in listeners {
            for uri in filter.resource_subscriptions.iter().flatten() {
                if affected(uri) {
                    let _ = sink.notify_resource_updated(uri.clone()).await;
                }
            }
            if listed && filter.resources_list_changed == Some(true) {
                let _ = sink.notify_resource_list_changed().await;
            }
        }
    }

    /// Run a call to its answer.
    async fn answer(
        &self,
        name: &str,
        arguments: Map<String, Value>,
        request: &RequestContext<RoleServer>,
    ) -> Answer {
        let spec = tools::spec(name).expect("the caller checked the tool exists");
        let cancel = Arc::new(AtomicBool::new(false));
        let watcher = {
            let cancel = Arc::clone(&cancel);
            let token = request.ct.clone();
            tokio::spawn(async move {
                token.cancelled().await;
                cancel.store(true, Ordering::Relaxed);
            })
        };
        let mut context = (*self.context).clone();
        context.cancel = Some(cancel);
        let blocking = |work: Box<dyn FnOnce() -> Value + Send>| async move {
            // A command does blocking file I/O and holds a database that is
            // not Send, so it runs start to finish on a blocking thread.
            tokio::task::spawn_blocking(work)
                .await
                .unwrap_or_else(|error| {
                    failure(
                        "mcp",
                        DbError::new(
                            Code::INTERNAL_ERROR,
                            format!("the command panicked: {error}"),
                        ),
                    )
                })
        };
        let plan_id = arguments
            .get("plan_id")
            .and_then(Value::as_str)
            .map(String::from);
        let envelope = match (spec.effect, plan_id) {
            (Effect::Planned { .. }, None) => {
                let server = self.clone();
                let name = name.to_string();
                blocking(Box::new(move || server.plan(&context, &name, &arguments))).await
            }
            (Effect::Planned { .. }, Some(id)) => match self.planned(name, &arguments, &id) {
                Err(error) => failure(name, error),
                Ok(plan) => match self.confirmed(&plan, &arguments, &request.peer).await {
                    Err(error) => failure(name, error),
                    Ok(()) => {
                        self.forget(&plan.id);
                        context.expect_state = Some(plan.state.clone());
                        context.yes = true;
                        let name = name.to_string();
                        let envelope = blocking(Box::new(move || {
                            match tools::action(&name, &arguments, &mut context) {
                                Ok(action) => Self::execute(&context, &name, action),
                                Err(error) => failure(&name, error),
                            }
                        }))
                        .await;
                        if envelope["ok"] == true {
                            self.announce(&envelope).await;
                        }
                        envelope
                    }
                },
            },
            (Effect::Read | Effect::Direct, _) => {
                let name = name.to_string();
                blocking(Box::new(move || {
                    match tools::action(&name, &arguments, &mut context) {
                        Ok(action) => Self::execute(&context, &name, action),
                        Err(error) => failure(&name, error),
                    }
                }))
                .await
            }
        };
        watcher.abort();
        Answer {
            line: line(&envelope),
            envelope,
        }
    }

    fn resource(&self, uri: &str) -> Result<(String, &'static str)> {
        if uri == "reldir://dialect" {
            return Ok((
                serde_json::to_string_pretty(reldir_core::schema::meta::meta_schema())
                    .unwrap_or_default(),
                "application/schema+json",
            ));
        }
        if uri == "reldir://protocol" {
            return Ok((
                serde_json::to_string_pretty(&crate::protocol::schema()).unwrap_or_default(),
                "application/schema+json",
            ));
        }
        if let Some(page) = uri.strip_prefix("reldir://docs/") {
            return crate::docs::page(page)
                .map(|text| (text, "text/markdown"))
                .ok_or_else(|| {
                    DbError::new(Code::UNKNOWN_RESOURCE, format!("there is no page {page:?}"))
                });
        }
        let (name, arguments) = if let Some(table) = uri.strip_prefix("reldir://schema/") {
            ("schema_show", json!({"table": table}))
        } else if let Some(rest) = uri.strip_prefix("reldir://row/") {
            let Some((table, key)) = rest.split_once('/') else {
                return Err(DbError::new(
                    Code::UNKNOWN_RESOURCE,
                    format!("{uri} names no row: reldir://row/{{table}}/{{key}}"),
                ));
            };
            ("get_row", json!({"table": table, "key": key}))
        } else {
            return Err(DbError::new(
                Code::UNKNOWN_RESOURCE,
                format!("there is no resource {uri}"),
            ));
        };
        let mut context = (*self.context).clone();
        let arguments = arguments.as_object().cloned().unwrap_or_default();
        let action = tools::action(name, &arguments, &mut context)?;
        let envelope = Self::execute(&context, name, action);
        if envelope["ok"] != true {
            let code = envelope["error"]["code"]
                .as_str()
                .and_then(Code::parse)
                .unwrap_or(Code::INTERNAL_ERROR);
            return Err(DbError::new(
                code,
                envelope["summary"].as_str().unwrap_or_default().to_string(),
            ));
        }
        let record = &envelope["records"][0];
        let (value, mime) = match name {
            "schema_show" => (&record["document"], "application/schema+json"),
            _ => (&record["row"], "application/json"),
        };
        Ok((
            serde_json::to_string_pretty(value).unwrap_or_default(),
            mime,
        ))
    }
}

/// A failure answering a protocol request, as the protocol's error.
fn protocol_error(error: &DbError) -> ErrorData {
    let message = error.diagnostic.message.clone();
    let code = error.code();
    if matches!(
        code,
        Code::UNKNOWN_RESOURCE | Code::UNKNOWN_TABLE | Code::UNKNOWN_ROW
    ) {
        ErrorData::resource_not_found(message, None)
    } else if matches!(code.exit(), Exit::Untrusted | Exit::Internal) {
        ErrorData::internal_error(message, None)
    } else {
        ErrorData::invalid_params(message, None)
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_resources_list_changed()
                .enable_prompts()
                .build(),
        )
        .with_server_info(Implementation::new("reldir", reldir_core::VERSION))
        .with_instructions(
            "reldir governs a folder of JSON files as a relational database. Read with `query` \
             (SQLite SQL; a page at a time) and the read tools. A tool that changes anything \
             plans first: it validates the change, lists every file it would touch and returns a \
             plan_id, writing nothing; calling it again with the same arguments and that plan_id \
             commits exactly that change, unless the database has changed since. A plan that \
             removes data needs the user's confirmation. Every result is the protocol's `result` \
             document (reldir://protocol): read `ok`, `summary`, `data`, `records` and \
             `diagnostics`. `check`, `lint` and `doctor_plan` find and repair problems; \
             reldir://docs/... explains everything.",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(
            tools::specs().iter().map(tools::Spec::tool).collect(),
        ))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools::spec(name).map(|spec| spec.tool())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        let Some(spec) = tools::spec(&name) else {
            return Err(ErrorData::invalid_params(
                format!("there is no tool {name:?}"),
                None,
            ));
        };
        let arguments = request.arguments.unwrap_or_default();
        // Arguments are held to the tool's schema before anything runs: a
        // misspelt or mistyped argument is refused, never ignored.
        let schema = spec.input_schema();
        let validator = jsonschema::validator_for(&schema).map_err(|error| {
            ErrorData::internal_error(
                format!("the input schema of {name} is invalid: {error}"),
                None,
            )
        })?;
        let instance = Value::Object(arguments.clone());
        let faults: Vec<String> = validator
            .iter_errors(&instance)
            .map(|error| {
                let at = error.instance_path().to_string();
                if at.is_empty() {
                    error.to_string()
                } else {
                    format!("{at}: {error}")
                }
            })
            .collect();
        if !faults.is_empty() {
            return Err(ErrorData::invalid_params(
                format!(
                    "the arguments of {name} are not valid: {}",
                    faults.join("; ")
                ),
                Some(json!({"faults": faults})),
            ));
        }
        let answer = self.answer(&name, arguments, &context).await;
        let ok = answer.envelope["ok"] == true;
        let mut result = if ok {
            CallToolResult::success(vec![ContentBlock::text(answer.line)])
        } else {
            CallToolResult::error(vec![ContentBlock::text(answer.line)])
        };
        result.structured_content = Some(answer.envelope);
        Ok(result.into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourcesResult, ErrorData> {
        let context = Arc::clone(&self.context);
        let tables: Vec<String> = tokio::task::spawn_blocking(move || {
            let envelope = Server::execute(
                &context,
                "tables",
                Action::Command(crate::cli::Command::Tables),
            );
            if envelope["ok"] != true {
                return Err(envelope);
            }
            Ok(envelope["records"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|record| record["table"].as_str().map(String::from))
                .collect())
        })
        .await
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?
        .map_err(|envelope| ErrorData::internal_error(line(&envelope), Some(envelope)))?;
        let mut resources = vec![
            Resource::new("reldir://dialect", "dialect")
                .with_description("The JSON Schema dialect every table schema is written in")
                .with_mime_type("application/schema+json"),
            Resource::new("reldir://protocol", "protocol")
                .with_description("The JSON Schema of every tool's result")
                .with_mime_type("application/schema+json"),
        ];
        for (page, source) in crate::docs::PAGES {
            resources.push(
                Resource::new(format!("reldir://docs/{page}"), format!("docs/{page}"))
                    .with_description(crate::docs::title(source))
                    .with_mime_type("text/markdown"),
            );
        }
        for table in tables {
            resources.push(
                Resource::new(
                    format!("reldir://schema/{table}"),
                    format!("schema/{table}"),
                )
                .with_description(format!("The schema of {table}"))
                .with_mime_type("application/schema+json"),
            );
        }
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new("reldir://schema/{table}", "schema")
                .with_description("A table's schema document")
                .with_mime_type("application/schema+json"),
            ResourceTemplate::new("reldir://row/{table}/{key}", "row")
                .with_description("One row, by its key")
                .with_mime_type("application/json"),
        ]))
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
            Err(error) => Err(protocol_error(&error)),
        }
    }

    #[allow(deprecated)]
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<(), ErrorData> {
        let mut subscriptions = self
            .subscriptions
            .lock()
            .expect("the subscription list is never poisoned");
        subscriptions.legacy.insert(request.uri);
        subscriptions.peer = Some(context.peer);
        Ok(())
    }

    #[allow(deprecated)]
    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<(), ErrorData> {
        self.subscriptions
            .lock()
            .expect("the subscription list is never poisoned")
            .legacy
            .remove(&request.uri);
        Ok(())
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        let mut accepted = SubscriptionFilter::new();
        accepted.resources_list_changed = requested.resources_list_changed;
        accepted.resource_subscriptions = requested.resource_subscriptions.clone();
        Some(accepted)
    }

    async fn listen(&self, context: SubscriptionContext) -> std::result::Result<(), ErrorData> {
        let id = self.counter.fetch_add(1, Ordering::Relaxed);
        self.subscriptions
            .lock()
            .expect("the subscription list is never poisoned")
            .listeners
            .insert(id, (context.sink().clone(), context.accepted().clone()));
        context.cancelled().await;
        self.subscriptions
            .lock()
            .expect("the subscription list is never poisoned")
            .listeners
            .remove(&id);
        Ok(())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(prompts::list()))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<GetPromptResponse, ErrorData> {
        prompts::get(&request.name, request.arguments.unwrap_or_default())
            .map(Into::into)
            .map_err(|error| ErrorData::invalid_params(error.diagnostic.message.clone(), None))
    }
}

/// Serve on stdin and stdout until the client disconnects.
pub fn serve(context: Context) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            DbError::new(
                Code::IO_ERROR,
                format!("cannot start the MCP runtime: {error}"),
            )
        })?;
    runtime.block_on(async {
        let running = Server::new(context)
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|error| {
                DbError::new(
                    Code::IO_ERROR,
                    format!("the MCP session did not start: {error}"),
                )
            })?;
        running.waiting().await.map_err(|error| {
            DbError::new(
                Code::IO_ERROR,
                format!("the MCP session ended abnormally: {error}"),
            )
        })?;
        Ok(())
    })
}
