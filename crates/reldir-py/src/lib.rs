//! `reldir._reldir`: the engine, in-process, for Python. The package around it
//! (`python/reldir`) puts a DB-API 2.0 interface on these classes; this module
//! converts values and errors and does nothing the engine does not.
//!
//! Values cross as JSON does, exactly: an integer of any size stays an
//! integer, a `Decimal` keeps its digits, and a float that is not finite is
//! refused rather than written as something it is not.

use pyo3::{
    create_exception,
    exceptions::{PyException, PyTypeError, PyValueError},
    prelude::*,
    types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple},
};
use reldir_core::{
    Changed, Code, Connection, Options, Rows, code::Exit, diagnostic::DbError, sql::SqlParam,
};
use serde_json::{Map, Number, Value};

create_exception!(
    reldir,
    Warning,
    PyException,
    "PEP 249: an important warning."
);
create_exception!(
    reldir,
    Error,
    PyException,
    "PEP 249: the base of every reldir error. `code` names the registered code, `diagnostics` every fault behind it."
);
create_exception!(
    reldir,
    InterfaceError,
    Error,
    "PEP 249: the interface was misused."
);
create_exception!(
    reldir,
    DatabaseError,
    Error,
    "PEP 249: an error of the database."
);
create_exception!(
    reldir,
    DataError,
    DatabaseError,
    "PEP 249: a value cannot be what its column says."
);
create_exception!(
    reldir,
    OperationalError,
    DatabaseError,
    "PEP 249: the database could not do it now: another writer, an outside edit, the filesystem, a decision it needs."
);
create_exception!(
    reldir,
    IntegrityError,
    DatabaseError,
    "PEP 249: the change would leave the database invalid, or break a table's contract."
);
create_exception!(
    reldir,
    InternalError,
    DatabaseError,
    "PEP 249: reldir itself failed."
);
create_exception!(
    reldir,
    ProgrammingError,
    DatabaseError,
    "PEP 249: the statement, a name or an argument is wrong."
);
create_exception!(
    reldir,
    NotSupportedError,
    DatabaseError,
    "PEP 249: what was asked is not something reldir does."
);

/// The engine's error as the PEP 249 exception it is, carrying its code,
/// exit status and diagnostics.
fn raise(py: Python<'_>, error: DbError) -> PyErr {
    let code = error.code();
    let message = format!("{}: {}", code.as_str(), error.diagnostic.message);
    let err = match code {
        Code::TYPE_MISMATCH | Code::INPUT_INVALID | Code::NOT_NULL_VIOLATION => {
            DataError::new_err(message)
        }
        Code::TRANSACTION_STATEMENT => NotSupportedError::new_err(message),
        // As sqlite3 has it: writing to a read-only database is operational.
        Code::READ_ONLY => OperationalError::new_err(message),
        _ => match code.exit() {
            Exit::Invalid => IntegrityError::new_err(message),
            Exit::Usage | Exit::Unresolved => ProgrammingError::new_err(message),
            Exit::Internal => InternalError::new_err(message),
            Exit::Ok | Exit::Findings => DatabaseError::new_err(message),
            Exit::Contention
            | Exit::Recovery
            | Exit::Untrusted
            | Exit::Inference
            | Exit::Decision
            | Exit::Uninitialized
            | Exit::UnsafeFilesystem
            | Exit::CommittedRecoveryPending
            | Exit::CommitUnknown => OperationalError::new_err(message),
        },
    };
    let value = err.value(py);
    let diagnostics: Vec<Value> = error
        .diagnostics()
        .iter()
        .map(|diagnostic| serde_json::to_value(diagnostic).unwrap_or(Value::Null))
        .collect();
    let attached = (|| -> PyResult<()> {
        value.setattr("code", code.as_str())?;
        value.setattr("exit", code.exit().code())?;
        value.setattr("diagnostics", to_py(py, &Value::Array(diagnostics))?)?;
        Ok(())
    })();
    match attached {
        Ok(()) => err,
        Err(failure) => failure,
    }
}

/// A JSON value as Python holds it.
fn to_py(py: Python<'_>, value: &Value) -> PyResult<Py<PyAny>> {
    Ok(match value {
        Value::Null => py.None(),
        Value::Bool(flag) => PyBool::new(py, *flag).to_owned().into_any().unbind(),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                integer.into_pyobject(py)?.into_any().unbind()
            } else if let Some(integer) = number.as_u64() {
                integer.into_pyobject(py)?.into_any().unbind()
            } else {
                let lexeme = number.to_string();
                if lexeme.contains(['.', 'e', 'E']) {
                    let float: f64 = lexeme.parse().map_err(|_| {
                        PyValueError::new_err(format!("{lexeme} is not a number Python can hold"))
                    })?;
                    PyFloat::new(py, float).into_any().unbind()
                } else {
                    // An integer beyond 64 bits, exactly.
                    py.get_type::<PyInt>().call1((lexeme,))?.unbind()
                }
            }
        }
        Value::String(text) => PyString::new(py, text).into_any().unbind(),
        Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(to_py(py, item)?)?;
            }
            list.into_any().unbind()
        }
        Value::Object(members) => {
            let dict = PyDict::new(py);
            for (key, item) in members {
                dict.set_item(key, to_py(py, item)?)?;
            }
            dict.into_any().unbind()
        }
    })
}

fn number(text: &str) -> PyResult<Value> {
    serde_json::from_str::<Number>(text)
        .map(Value::Number)
        .map_err(|_| PyValueError::new_err(format!("{text} is not a JSON number")))
}

/// A Python value as JSON.
fn from_py(object: &Bound<'_, PyAny>) -> PyResult<Value> {
    if object.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(flag) = object.cast::<PyBool>() {
        return Ok(Value::Bool(flag.is_true()));
    }
    if object.cast::<PyInt>().is_ok() {
        return match object.extract::<i64>() {
            Ok(integer) => Ok(Value::from(integer)),
            Err(_) => number(&object.str()?.to_cow()?),
        };
    }
    if let Ok(float) = object.cast::<PyFloat>() {
        let value = float.value();
        return Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| PyValueError::new_err(format!("{value} is not a JSON number")));
    }
    if let Ok(text) = object.cast::<PyString>() {
        return Ok(Value::String(text.to_cow()?.into_owned()));
    }
    if let Ok(dict) = object.cast::<PyDict>() {
        let mut members = Map::new();
        for (key, item) in dict.iter() {
            let Ok(key) = key.cast::<PyString>() else {
                return Err(PyTypeError::new_err(format!(
                    "a JSON object's names are strings, not {}",
                    key.get_type().name()?
                )));
            };
            members.insert(key.to_cow()?.into_owned(), from_py(&item)?);
        }
        return Ok(Value::Object(members));
    }
    if object.cast::<PyList>().is_ok() || object.cast::<PyTuple>().is_ok() {
        return object
            .try_iter()?
            .map(|item| from_py(&item?))
            .collect::<PyResult<Vec<_>>>()
            .map(Value::Array);
    }
    let type_name = object.get_type().name()?.to_string();
    match type_name.as_str() {
        "Decimal" => {
            let text = object.str()?.to_cow()?.into_owned();
            number(&text)
        }
        "datetime" | "date" | "time" => Ok(Value::String(
            object.call_method0("isoformat")?.extract::<String>()?,
        )),
        other => Err(PyTypeError::new_err(format!(
            "a {other} cannot be stored: values are None, bool, int, float, Decimal, str, \
             datetime, list and dict"
        ))),
    }
}

/// Parameters: none, a sequence for `?`, or a mapping for `:name`.
fn params(parameters: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<SqlParam>> {
    let Some(parameters) = parameters else {
        return Ok(vec![]);
    };
    if parameters.is_none() {
        return Ok(vec![]);
    }
    if let Ok(dict) = parameters.cast::<PyDict>() {
        let mut out = vec![];
        for (name, value) in dict.iter() {
            out.push(SqlParam {
                name: Some(name.extract::<String>()?),
                value: from_py(&value)?,
            });
        }
        return Ok(out);
    }
    if parameters.cast::<PyString>().is_ok() {
        return Err(PyTypeError::new_err(
            "parameters are a sequence or a mapping, not a string",
        ));
    }
    parameters
        .try_iter()?
        .map(|value| {
            Ok(SqlParam {
                name: None,
                value: from_py(&value?)?,
            })
        })
        .collect()
}

fn values(key: &Bound<'_, PyAny>) -> PyResult<Vec<Value>> {
    match from_py(key)? {
        Value::Array(values) => Ok(values),
        value => Ok(vec![value]),
    }
}

/// An answer as `(columns, rows)`, each row a tuple in column order.
fn rows(py: Python<'_>, answer: Rows) -> PyResult<(Vec<String>, Vec<Py<PyTuple>>)> {
    let mut out = Vec::with_capacity(answer.rows.len());
    for row in &answer.rows {
        let cells: Vec<Py<PyAny>> = answer
            .columns
            .iter()
            .map(|column| to_py(py, row.get(column).unwrap_or(&Value::Null)))
            .collect::<PyResult<_>>()?;
        out.push(PyTuple::new(py, cells)?.unbind());
    }
    Ok((answer.columns, out))
}

/// What a change did, as a dict.
fn changed(py: Python<'_>, done: Changed) -> PyResult<Py<PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("rows", done.rows)?;
    dict.set_item("induced", done.induced)?;
    dict.set_item("files", done.files)?;
    dict.set_item("replayed", done.replayed)?;
    let returning: Vec<Py<PyAny>> = done
        .returning
        .iter()
        .map(|row| to_py(py, &Value::Object(row.clone())))
        .collect::<PyResult<_>>()?;
    dict.set_item("returning", returning)?;
    match done.revision {
        Some(revision) => dict.set_item("revision", (revision.number, revision.id.to_hex()))?,
        None => dict.set_item("revision", py.None())?,
    }
    let warnings: Vec<Value> = done
        .warnings
        .iter()
        .map(|warning| serde_json::to_value(warning).unwrap_or(Value::Null))
        .collect();
    dict.set_item("warnings", to_py(py, &Value::Array(warnings))?)?;
    Ok(dict.unbind())
}

fn rows_argument(rows: &Bound<'_, PyAny>) -> PyResult<Vec<Map<String, Value>>> {
    match from_py(rows)? {
        Value::Object(row) => Ok(vec![row]),
        Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                Value::Object(row) => Ok(row),
                other => Err(PyTypeError::new_err(format!(
                    "a row is a dict, not {other}"
                ))),
            })
            .collect(),
        other => Err(PyTypeError::new_err(format!(
            "rows are a dict or a list of dicts, not {other}"
        ))),
    }
}

/// A value carried into [`Python::detach`], which releases the GIL and runs
/// its closure on this same thread. A transaction belongs to the thread that
/// began it and holds nothing of Python's, so it may wait for the writer
/// lock, or for the disk, without holding every other thread up.
struct Here<T>(T);

// SAFETY: `Python::detach` runs the closure on the calling thread; a `Here`
// never reaches another thread, so the value it carries never does either.
unsafe impl<T> Send for Here<T> {}

/// A database in a folder. Every call answers from the folder as it is then.
#[pyclass(frozen, module = "reldir._reldir")]
struct Database {
    inner: Connection,
}

#[pymethods]
impl Database {
    #[new]
    #[pyo3(signature = (path, *, readonly = false, establish = true, wait = None, timeout = None, max_rows = None))]
    fn new(
        py: Python<'_>,
        path: std::path::PathBuf,
        readonly: bool,
        establish: bool,
        wait: Option<f64>,
        timeout: Option<f64>,
        max_rows: Option<usize>,
    ) -> PyResult<Self> {
        let options = Options {
            readonly,
            no_establish: !establish,
            wait_seconds: wait,
            timeout_seconds: timeout,
            max_result_rows: max_rows,
        };
        py.detach(|| Connection::open(&path, options))
            .map(|inner| Self { inner })
            .map_err(|error| raise(py, error))
    }

    #[getter]
    fn root(&self) -> std::path::PathBuf {
        self.inner.root().to_path_buf()
    }

    /// `"read"` or `"change"`; raises for a statement reldir does not run.
    fn kind(&self, py: Python<'_>, sql: &str) -> PyResult<&'static str> {
        match reldir_core::sql::classify(sql) {
            Ok(reldir_core::sql::StatementKind::Read) => Ok("read"),
            Ok(reldir_core::sql::StatementKind::Mutation) => Ok("change"),
            Err(error) => Err(raise(py, error)),
        }
    }

    /// Run a read statement: `(columns, rows)`.
    #[pyo3(signature = (sql, parameters = None))]
    fn query(
        &self,
        py: Python<'_>,
        sql: &str,
        parameters: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<(Vec<String>, Vec<Py<PyTuple>>)> {
        let params = params(parameters)?;
        let answer = py
            .detach(|| self.inner.query_with(sql, &params))
            .map_err(|error| raise(py, error))?;
        rows(py, answer)
    }

    /// Run one change and commit it.
    #[pyo3(signature = (sql, parameters = None, *, idempotency_key = None, widen = false))]
    fn execute(
        &self,
        py: Python<'_>,
        sql: &str,
        parameters: Option<&Bound<'_, PyAny>>,
        idempotency_key: Option<String>,
        widen: bool,
    ) -> PyResult<Py<PyDict>> {
        let params = params(parameters)?;
        let done = py
            .detach(|| {
                self.inner.execute_with(
                    sql,
                    &params,
                    reldir_core::api::Change {
                        idempotency_key: idempotency_key.as_deref(),
                        widen,
                    },
                )
            })
            .map_err(|error| raise(py, error))?;
        changed(py, done)
    }

    /// Insert rows -- a dict, or a list of dicts -- and commit.
    fn insert(&self, py: Python<'_>, table: &str, rows: &Bound<'_, PyAny>) -> PyResult<Py<PyDict>> {
        let rows = rows_argument(rows)?;
        let done = py
            .detach(|| self.inner.insert(table, rows))
            .map_err(|error| raise(py, error))?;
        changed(py, done)
    }

    /// Set columns of the row a key names, and commit.
    fn update(
        &self,
        py: Python<'_>,
        table: &str,
        key: &Bound<'_, PyAny>,
        patch: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyDict>> {
        let key = values(key)?;
        let Value::Object(patch) = from_py(patch)? else {
            return Err(PyTypeError::new_err(
                "a patch is a dict of the columns to set",
            ));
        };
        let done = py
            .detach(|| self.inner.update(table, &key, &patch))
            .map_err(|error| raise(py, error))?;
        changed(py, done)
    }

    /// Delete the row a key names, and commit.
    fn delete(&self, py: Python<'_>, table: &str, key: &Bound<'_, PyAny>) -> PyResult<Py<PyDict>> {
        let key = values(key)?;
        let done = py
            .detach(|| self.inner.delete(table, &key))
            .map_err(|error| raise(py, error))?;
        changed(py, done)
    }

    /// The row a key names, or None.
    fn get(&self, py: Python<'_>, table: &str, key: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let key = values(key)?;
        let row = py
            .detach(|| self.inner.get(table, &key))
            .map_err(|error| raise(py, error))?;
        match row {
            Some(row) => to_py(py, &Value::Object(row)),
            None => Ok(py.None()),
        }
    }

    /// The tables: a list of dicts with name, rows, key and pinned.
    fn tables(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let tables = py
            .detach(|| self.inner.tables())
            .map_err(|error| raise(py, error))?;
        let listed: Vec<Value> = tables
            .into_iter()
            .map(|table| {
                serde_json::json!({"name": table.name, "rows": table.rows, "key": table.key, "pinned": table.pinned})
            })
            .collect();
        to_py(py, &Value::Array(listed))
    }

    /// Validate everything: a dict with valid, errors and warnings.
    fn check(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let report = py
            .detach(|| self.inner.check())
            .map_err(|error| raise(py, error))?;
        to_py(
            py,
            &serde_json::json!({
                "valid": report.valid,
                "errors": report.errors,
                "warnings": report.warnings,
            }),
        )
    }

    /// A name for the state the database holds now.
    fn state(&self, py: Python<'_>) -> PyResult<String> {
        py.detach(|| self.inner.state())
            .map_err(|error| raise(py, error))
    }

    /// Begin a transaction, which holds the writer lock until it ends.
    #[pyo3(signature = (*, idempotency_key = None, widen = false))]
    fn begin(
        &self,
        py: Python<'_>,
        idempotency_key: Option<String>,
        widen: bool,
    ) -> PyResult<Transaction> {
        let inner = &self.inner;
        // Waiting for another writer's lock must not hold the GIL that
        // writer needs to finish.
        py.detach(|| {
            Here(inner.begin_with(reldir_core::api::Change {
                idempotency_key: idempotency_key.as_deref(),
                widen,
            }))
        })
        .0
        .map(|inner| Transaction { inner: Some(inner) })
        .map_err(|error| raise(py, error))
    }
}

/// Statements that are committed as one change or not at all. It belongs to
/// the thread that began it.
#[pyclass(unsendable, module = "reldir._reldir")]
struct Transaction {
    inner: Option<reldir_core::Transaction>,
}

impl Transaction {
    fn open(&mut self) -> PyResult<&mut reldir_core::Transaction> {
        self.inner
            .as_mut()
            .ok_or_else(|| InterfaceError::new_err("the transaction has ended"))
    }
}

#[pymethods]
impl Transaction {
    /// Whether it is still open.
    #[getter]
    fn active(&self) -> bool {
        self.inner.is_some()
    }

    #[getter]
    fn statements(&self) -> usize {
        self.inner.as_ref().map_or(0, |inner| inner.statements())
    }

    #[pyo3(signature = (sql, parameters = None))]
    fn query(
        &mut self,
        py: Python<'_>,
        sql: &str,
        parameters: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<(Vec<String>, Vec<Py<PyTuple>>)> {
        let params = params(parameters)?;
        let open = Here(self.open()?);
        let answer = py
            .detach(move || {
                let open = open;
                Here(open.0.query_with(sql, &params))
            })
            .0
            .map_err(|error| raise(py, error))?;
        rows(py, answer)
    }

    #[pyo3(signature = (sql, parameters = None))]
    fn execute(
        &mut self,
        py: Python<'_>,
        sql: &str,
        parameters: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyDict>> {
        let params = params(parameters)?;
        let open = Here(self.open()?);
        let done = py
            .detach(move || {
                let open = open;
                Here(open.0.execute_with(sql, &params))
            })
            .0
            .map_err(|error| raise(py, error))?;
        changed(py, done)
    }

    fn insert(
        &mut self,
        py: Python<'_>,
        table: &str,
        rows: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyDict>> {
        let rows = rows_argument(rows)?;
        let open = Here(self.open()?);
        let done = py
            .detach(move || {
                let open = open;
                Here(open.0.insert(table, rows))
            })
            .0
            .map_err(|error| raise(py, error))?;
        changed(py, done)
    }

    fn update(
        &mut self,
        py: Python<'_>,
        table: &str,
        key: &Bound<'_, PyAny>,
        patch: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyDict>> {
        let key = values(key)?;
        let Value::Object(patch) = from_py(patch)? else {
            return Err(PyTypeError::new_err(
                "a patch is a dict of the columns to set",
            ));
        };
        let open = Here(self.open()?);
        let done = py
            .detach(move || {
                let open = open;
                Here(open.0.update(table, &key, &patch))
            })
            .0
            .map_err(|error| raise(py, error))?;
        changed(py, done)
    }

    fn delete(
        &mut self,
        py: Python<'_>,
        table: &str,
        key: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyDict>> {
        let key = values(key)?;
        let open = Here(self.open()?);
        let done = py
            .detach(move || {
                let open = open;
                Here(open.0.delete(table, &key))
            })
            .0
            .map_err(|error| raise(py, error))?;
        changed(py, done)
    }

    /// Commit everything as one change.
    fn commit(&mut self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        let inner = self
            .inner
            .take()
            .ok_or_else(|| InterfaceError::new_err("the transaction has ended"))?;
        let inner = Here(inner);
        let done = py
            .detach(move || {
                let inner = inner;
                Here(inner.0.commit())
            })
            .0
            .map_err(|error| raise(py, error))?;
        changed(py, done)
    }

    /// Discard everything.
    fn rollback(&mut self, py: Python<'_>) -> PyResult<()> {
        match self.inner.take() {
            Some(inner) => {
                let inner = Here(inner);
                py.detach(move || {
                    let inner = inner;
                    Here(inner.0.rollback())
                })
                .0
                .map_err(|error| raise(py, error))
            }
            None => Ok(()),
        }
    }
}

/// The `reldir` command, run in this process with these arguments (the
/// program's name first); returns its exit status.
#[pyfunction]
fn cli_main(arguments: Vec<std::ffi::OsString>) -> i32 {
    reldir::main(arguments)
}

#[pymodule]
fn _reldir(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    module.add("__version__", reldir_core::VERSION)?;
    module.add_class::<Database>()?;
    module.add_class::<Transaction>()?;
    module.add_function(wrap_pyfunction!(cli_main, module)?)?;
    for (name, class) in [
        ("Warning", py.get_type::<Warning>()),
        ("Error", py.get_type::<Error>()),
        ("InterfaceError", py.get_type::<InterfaceError>()),
        ("DatabaseError", py.get_type::<DatabaseError>()),
        ("DataError", py.get_type::<DataError>()),
        ("OperationalError", py.get_type::<OperationalError>()),
        ("IntegrityError", py.get_type::<IntegrityError>()),
        ("InternalError", py.get_type::<InternalError>()),
        ("ProgrammingError", py.get_type::<ProgrammingError>()),
        ("NotSupportedError", py.get_type::<NotSupportedError>()),
    ] {
        module.add(name, class)?;
    }
    Ok(())
}
