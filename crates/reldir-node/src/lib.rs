//! The native half of `@reldir/reldir`: the engine in-process for Node.js.
//! The package's JavaScript (`index.js`) gives it its interface; this module
//! moves work off the event loop and does nothing the engine does not.
//!
//! Values cross as JSON text, which the JavaScript side writes and reads so
//! that an integer beyond 2^53 stays exact as a `BigInt`. Every call runs on
//! the libuv thread pool. A transaction belongs to a thread of its own, which
//! holds it from `begin` to its end and runs each statement sent to it, in
//! order; a transaction dropped by the garbage collector is rolled back.

use napi::{Env, Error, Result, Task, bindgen_prelude::AsyncTask};
use napi_derive::napi;
use reldir_core::{
    Changed, Connection, Options, Rows, api::Change, diagnostic::DbError, sql::SqlParam,
};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex, mpsc};

/// An engine error, as the JavaScript side rebuilds it: its code, exit
/// status, message and diagnostics, as JSON in the error's message.
fn failure(error: DbError) -> Error {
    let diagnostics: Vec<Value> = error
        .diagnostics()
        .iter()
        .map(|diagnostic| serde_json::to_value(diagnostic).unwrap_or(Value::Null))
        .collect();
    Error::from_reason(
        json!({
            "code": error.code().as_str(),
            "exit": error.exit().code(),
            "message": error.diagnostic.message,
            "diagnostics": diagnostics,
        })
        .to_string(),
    )
}

fn usage(message: impl Into<String>) -> Error {
    failure(DbError::usage(message))
}

fn parse(text: &str) -> Result<Value> {
    reldir_core::json::parse_str(text).map_err(|error| usage(format!("not JSON: {error}")))
}

/// Parameters: none, an array for `?`, or an object for `:name`.
fn params(text: &str) -> Result<Vec<SqlParam>> {
    Ok(match parse(text)? {
        Value::Null => vec![],
        Value::Array(values) => values
            .into_iter()
            .map(|value| SqlParam { name: None, value })
            .collect(),
        Value::Object(named) => named
            .into_iter()
            .map(|(name, value)| SqlParam {
                name: Some(name),
                value,
            })
            .collect(),
        other => {
            return Err(usage(format!(
                "parameters are an array or an object, not {other}"
            )));
        }
    })
}

fn key(text: &str) -> Result<Vec<Value>> {
    Ok(match parse(text)? {
        Value::Array(values) => values,
        value => vec![value],
    })
}

fn object(text: &str, what: &str) -> Result<Map<String, Value>> {
    match parse(text)? {
        Value::Object(map) => Ok(map),
        other => Err(usage(format!("{what} is an object, not {other}"))),
    }
}

fn rows_argument(text: &str) -> Result<Vec<Map<String, Value>>> {
    match parse(text)? {
        Value::Object(row) => Ok(vec![row]),
        Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                Value::Object(row) => Ok(row),
                other => Err(usage(format!("a row is an object, not {other}"))),
            })
            .collect(),
        other => Err(usage(format!(
            "rows are an object or an array of objects, not {other}"
        ))),
    }
}

fn answer(rows: Rows) -> Value {
    json!({"columns": rows.columns, "rows": rows.rows})
}

fn changed(done: Changed) -> Value {
    json!({
        "rows": done.rows,
        "induced": done.induced,
        "files": done.files,
        "returning": done.returning,
        "revision": done.revision.map(|revision| json!({"number": revision.number, "id": revision.id.to_hex()})),
        "replayed": done.replayed,
        "warnings": done.warnings,
    })
}

/// One piece of work for the thread pool.
pub struct Work(Option<Box<dyn FnOnce() -> Result<Value> + Send>>);

impl Work {
    fn new(work: impl FnOnce() -> Result<Value> + Send + 'static) -> AsyncTask<Self> {
        AsyncTask::new(Self(Some(Box::new(work))))
    }
}

impl Task for Work {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<String> {
        let work = self
            .0
            .take()
            .ok_or_else(|| Error::from_reason("the work was already done"))?;
        work().map(|value| value.to_string())
    }

    fn resolve(&mut self, _env: Env, output: String) -> Result<String> {
        Ok(output)
    }
}

/// Opening a database, off the event loop: it observes the folder.
pub struct Opening {
    path: String,
    options: Options,
}

impl Task for Opening {
    type Output = Connection;
    type JsValue = Database;

    fn compute(&mut self) -> Result<Connection> {
        Connection::open(&self.path, self.options.clone()).map_err(failure)
    }

    fn resolve(&mut self, _env: Env, inner: Connection) -> Result<Database> {
        Ok(Database {
            inner: Arc::new(inner),
        })
    }
}

/// Open the database in a folder. `options` is JSON: `readonly`,
/// `establish`, `wait`, `timeout`, `maxRows`.
#[napi(ts_return_type = "Promise<Database>")]
pub fn open(path: String, options: String) -> Result<AsyncTask<Opening>> {
    let options = object(&options, "options")?;
    let flag = |name: &str| options.get(name).and_then(Value::as_bool);
    let seconds = |name: &str| options.get(name).and_then(Value::as_f64);
    Ok(AsyncTask::new(Opening {
        path,
        options: Options {
            readonly: flag("readonly").unwrap_or(false),
            no_establish: !flag("establish").unwrap_or(true),
            wait_seconds: seconds("wait"),
            timeout_seconds: seconds("timeout"),
            max_result_rows: options
                .get("maxRows")
                .and_then(Value::as_u64)
                .map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
        },
    }))
}

/// A database in a folder; every call answers from the folder as it is then.
#[napi]
pub struct Database {
    inner: Arc<Connection>,
}

#[napi]
impl Database {
    #[napi(getter)]
    pub fn root(&self) -> String {
        self.inner.root().to_string_lossy().into_owned()
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn query(&self, sql: String, parameters: String) -> Result<AsyncTask<Work>> {
        let inner = Arc::clone(&self.inner);
        let params = params(&parameters)?;
        Ok(Work::new(move || {
            inner.query_with(&sql, &params).map(answer).map_err(failure)
        }))
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn execute(
        &self,
        sql: String,
        parameters: String,
        options: String,
    ) -> Result<AsyncTask<Work>> {
        let inner = Arc::clone(&self.inner);
        let params = params(&parameters)?;
        let options = object(&options, "options")?;
        let idempotency_key = options
            .get("idempotencyKey")
            .and_then(Value::as_str)
            .map(String::from);
        let widen = options
            .get("widen")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(Work::new(move || {
            inner
                .execute_with(
                    &sql,
                    &params,
                    Change {
                        idempotency_key: idempotency_key.as_deref(),
                        widen,
                    },
                )
                .map(changed)
                .map_err(failure)
        }))
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn insert(&self, table: String, rows: String) -> Result<AsyncTask<Work>> {
        let inner = Arc::clone(&self.inner);
        let rows = rows_argument(&rows)?;
        Ok(Work::new(move || {
            inner.insert(&table, rows).map(changed).map_err(failure)
        }))
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn update(
        &self,
        table: String,
        key_json: String,
        patch: String,
    ) -> Result<AsyncTask<Work>> {
        let inner = Arc::clone(&self.inner);
        let key = key(&key_json)?;
        let patch = object(&patch, "a patch")?;
        Ok(Work::new(move || {
            inner
                .update(&table, &key, &patch)
                .map(changed)
                .map_err(failure)
        }))
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn delete(&self, table: String, key_json: String) -> Result<AsyncTask<Work>> {
        let inner = Arc::clone(&self.inner);
        let key = key(&key_json)?;
        Ok(Work::new(move || {
            inner.delete(&table, &key).map(changed).map_err(failure)
        }))
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn get(&self, table: String, key_json: String) -> Result<AsyncTask<Work>> {
        let inner = Arc::clone(&self.inner);
        let key = key(&key_json)?;
        Ok(Work::new(move || {
            inner
                .get(&table, &key)
                .map(|row| row.map_or(Value::Null, Value::Object))
                .map_err(failure)
        }))
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn tables(&self) -> AsyncTask<Work> {
        let inner = Arc::clone(&self.inner);
        Work::new(move || {
            inner
                .tables()
                .map(|tables| {
                    tables
                        .into_iter()
                        .map(|table| json!({"name": table.name, "rows": table.rows, "key": table.key, "pinned": table.pinned}))
                        .collect()
                })
                .map_err(failure)
        })
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn check(&self) -> AsyncTask<Work> {
        let inner = Arc::clone(&self.inner);
        Work::new(move || {
            inner
                .check()
                .map(|report| json!({"valid": report.valid, "errors": report.errors, "warnings": report.warnings}))
                .map_err(failure)
        })
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn state(&self) -> AsyncTask<Work> {
        let inner = Arc::clone(&self.inner);
        Work::new(move || inner.state().map(Value::String).map_err(failure))
    }

    /// Begin a transaction on a thread of its own.
    #[napi(ts_return_type = "Promise<Transaction>")]
    pub fn begin(&self, options: String) -> Result<AsyncTask<Beginning>> {
        let options = object(&options, "options")?;
        Ok(AsyncTask::new(Beginning {
            inner: Arc::clone(&self.inner),
            idempotency_key: options
                .get("idempotencyKey")
                .and_then(Value::as_str)
                .map(String::from),
            widen: options
                .get("widen")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }))
    }
}

/// What a transaction's thread is asked to do.
pub enum Command {
    Query(String, Vec<SqlParam>),
    Execute(String, Vec<SqlParam>),
    Insert(String, Vec<Map<String, Value>>),
    Update(String, Vec<Value>, Map<String, Value>),
    Delete(String, Vec<Value>),
    Commit,
    Rollback,
}

type Reply = mpsc::Sender<Result<Value>>;
type Mailbox = mpsc::Sender<(Command, Reply)>;

/// The thread that holds a transaction: it runs each command in order, and
/// ends with a commit, a rollback, or -- when every handle to it is gone --
/// the rollback dropping it makes.
fn hold(mut transaction: reldir_core::Transaction, commands: mpsc::Receiver<(Command, Reply)>) {
    while let Ok((command, reply)) = commands.recv() {
        let result = match command {
            Command::Query(sql, params) => transaction.query_with(&sql, &params).map(answer),
            Command::Execute(sql, params) => transaction.execute_with(&sql, &params).map(changed),
            Command::Insert(table, rows) => transaction.insert(&table, rows).map(changed),
            Command::Update(table, key, patch) => {
                transaction.update(&table, &key, &patch).map(changed)
            }
            Command::Delete(table, key) => transaction.delete(&table, &key).map(changed),
            Command::Commit => {
                let _ = reply.send(transaction.commit().map(changed).map_err(failure));
                return;
            }
            Command::Rollback => {
                let _ = reply.send(
                    transaction
                        .rollback()
                        .map(|()| Value::Null)
                        .map_err(failure),
                );
                return;
            }
        };
        let _ = reply.send(result.map_err(failure));
    }
}

/// Beginning a transaction: its thread is started, and takes the writer
/// lock before the promise resolves.
pub struct Beginning {
    inner: Arc<Connection>,
    idempotency_key: Option<String>,
    widen: bool,
}

impl Task for Beginning {
    type Output = Mailbox;
    type JsValue = Transaction;

    fn compute(&mut self) -> Result<Mailbox> {
        let (mailbox, commands) = mpsc::channel::<(Command, Reply)>();
        let (ready, began) = mpsc::channel::<Result<()>>();
        let inner = Arc::clone(&self.inner);
        let idempotency_key = self.idempotency_key.clone();
        let widen = self.widen;
        std::thread::Builder::new()
            .name("reldir-transaction".into())
            .spawn(move || {
                let begun = inner.begin_with(Change {
                    idempotency_key: idempotency_key.as_deref(),
                    widen,
                });
                match begun {
                    Ok(transaction) => {
                        let _ = ready.send(Ok(()));
                        hold(transaction, commands);
                    }
                    Err(error) => {
                        let _ = ready.send(Err(failure(error)));
                    }
                }
            })
            .map_err(|error| {
                Error::from_reason(format!("cannot start a transaction's thread: {error}"))
            })?;
        began
            .recv()
            .map_err(|_| Error::from_reason("the transaction's thread ended before it began"))??;
        Ok(mailbox)
    }

    fn resolve(&mut self, _env: Env, mailbox: Mailbox) -> Result<Transaction> {
        Ok(Transaction {
            mailbox: Arc::new(Mutex::new(Some(mailbox))),
        })
    }
}

/// Statements committed as one change or not at all.
#[napi]
pub struct Transaction {
    mailbox: Arc<Mutex<Option<Mailbox>>>,
}

impl Transaction {
    /// Send a command to the transaction's thread, from the thread pool.
    fn send(&self, command: Command, ends: bool) -> Result<AsyncTask<Work>> {
        let mailbox = {
            let mut held = self
                .mailbox
                .lock()
                .map_err(|_| Error::from_reason("the transaction's mailbox is poisoned"))?;
            let mailbox = held
                .as_ref()
                .cloned()
                .ok_or_else(|| usage("the transaction has ended"))?;
            if ends {
                *held = None;
            }
            mailbox
        };
        Ok(Work::new(move || {
            let (reply, answer) = mpsc::channel();
            mailbox
                .send((command, reply))
                .map_err(|_| usage("the transaction has ended"))?;
            answer.recv().map_err(|_| {
                Error::from_reason("the transaction's thread ended without answering")
            })?
        }))
    }
}

#[napi]
impl Transaction {
    #[napi(getter)]
    pub fn active(&self) -> bool {
        self.mailbox
            .lock()
            .map(|held| held.is_some())
            .unwrap_or(false)
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn query(&self, sql: String, parameters: String) -> Result<AsyncTask<Work>> {
        self.send(Command::Query(sql, params(&parameters)?), false)
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn execute(&self, sql: String, parameters: String) -> Result<AsyncTask<Work>> {
        self.send(Command::Execute(sql, params(&parameters)?), false)
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn insert(&self, table: String, rows: String) -> Result<AsyncTask<Work>> {
        self.send(Command::Insert(table, rows_argument(&rows)?), false)
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn update(
        &self,
        table: String,
        key_json: String,
        patch: String,
    ) -> Result<AsyncTask<Work>> {
        self.send(
            Command::Update(table, key(&key_json)?, object(&patch, "a patch")?),
            false,
        )
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn delete(&self, table: String, key_json: String) -> Result<AsyncTask<Work>> {
        self.send(Command::Delete(table, key(&key_json)?), false)
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn commit(&self) -> Result<AsyncTask<Work>> {
        self.send(Command::Commit, true)
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn rollback(&self) -> Result<AsyncTask<Work>> {
        self.send(Command::Rollback, true)
    }
}

/// The engine's version.
#[napi]
pub fn version() -> String {
    reldir_core::VERSION.to_string()
}
