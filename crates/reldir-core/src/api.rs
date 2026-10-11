//! The library: a folder of JSON files as a database, for applications.
//!
//! ```no_run
//! use reldir_core::{Connection, Options};
//! use serde_json::json;
//!
//! let db = Connection::open("data", Options::default())?;
//! for row in db.query("SELECT id, name FROM users WHERE age > ?", &[json!(30)])?.rows {
//!     println!("{row:?}");
//! }
//! db.execute("UPDATE users SET age = age + 1 WHERE id = ?", &[json!("ada")])?;
//! let mut tx = db.begin()?;
//! tx.execute("INSERT INTO users (id, name) VALUES ('cy', 'Cy')", &[])?;
//! tx.execute("INSERT INTO posts (id, user_id) VALUES ('p9', 'cy')", &[])?;
//! tx.commit()?;
//! # Ok::<(), reldir_core::Error>(())
//! ```
//!
//! A [`Connection`] names a database and how to use it; it holds no open
//! file between calls, so it is `Send + Sync`, cheap to clone, and every call
//! answers from the folder as it is then -- an edit another program made a
//! moment ago included. A [`Transaction`] holds the writer lock from
//! [`Connection::begin`] until it is committed, rolled back or dropped.
//!
//! Every failure is an [`Error`](crate::Error) carrying a registered [`Code`] and the
//! diagnostics behind it.

use crate::{
    catalog::Catalog,
    code::Code,
    config::ResourceOverrides,
    db::{Access, Database, Outcome, Request},
    diagnostic::{DbError, Diagnostic, Result},
    plan::RowChange,
    sql::{SqlParam, StatementKind},
    state::{Opened, Opening},
    store::revision::Origin,
};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// How a connection uses its database.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Write nothing at all, not even derived state; every change is refused.
    pub readonly: bool,
    /// Refuse to establish a database where none is yet: the folder must
    /// already have been adopted or initialised.
    pub no_establish: bool,
    /// Seconds to wait for another writer's lock before refusing.
    pub wait_seconds: Option<f64>,
    /// Seconds a query may run.
    pub timeout_seconds: Option<f64>,
    /// Rows a query may return.
    pub max_result_rows: Option<usize>,
}

/// A database in a folder.
#[derive(Debug, Clone)]
pub struct Connection {
    root: PathBuf,
    options: Options,
}

/// A query's answer: its columns, in order, and its rows.
#[derive(Debug, Clone, PartialEq)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Map<String, Value>>,
}

/// What a change did.
#[derive(Debug, Clone)]
pub struct Changed {
    /// Rows written or removed, referential actions included.
    pub rows: usize,
    /// Of those, the rows referential actions changed.
    pub induced: usize,
    /// Files written or removed.
    pub files: usize,
    /// What `RETURNING` produced, or the rows an insert or update wrote.
    pub returning: Vec<Map<String, Value>>,
    /// The revision recorded; `None` when nothing changed or history is off.
    pub revision: Option<Revision>,
    /// The change was made before under the same idempotency key, and is
    /// reported rather than made again.
    pub replayed: bool,
    /// Warnings the resulting state carries.
    pub warnings: Vec<Diagnostic>,
}

/// A recorded revision: this clone's number for it, and its id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Revision {
    pub number: u64,
    pub id: crate::store::Id,
}

/// A table, as [`Connection::tables`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    pub rows: u64,
    pub key: Vec<String>,
    /// Declared by a schema in `schema/`, rather than inferred.
    pub pinned: bool,
}

/// What [`Connection::check`] found.
#[derive(Debug, Clone)]
pub struct Report {
    pub valid: bool,
    pub errors: Vec<Diagnostic>,
    pub warnings: Vec<Diagnostic>,
}

/// One change, as [`Connection::execute`] and its relatives commit it.
#[derive(Default)]
pub struct Change<'a> {
    pub idempotency_key: Option<&'a str>,
    /// Widen tables' contracts to admit values the change writes that they
    /// never admitted, in the same change.
    pub widen: bool,
}

fn params(values: &[Value]) -> Vec<SqlParam> {
    values
        .iter()
        .map(|value| SqlParam {
            name: None,
            value: value.clone(),
        })
        .collect()
}

/// Named parameters, `:name` in the statement.
pub fn named(values: &Map<String, Value>) -> Vec<SqlParam> {
    values
        .iter()
        .map(|(name, value)| SqlParam {
            name: Some(name.clone()),
            value: value.clone(),
        })
        .collect()
}

fn no_tables() -> DbError {
    DbError::new(
        Code::UNKNOWN_TABLE,
        "there are no tables here: no rows, no schemas",
    )
}

fn changed(outcome: &Outcome, returning: Vec<Map<String, Value>>) -> Changed {
    Changed {
        rows: outcome.rows.len(),
        induced: outcome.induced.len(),
        files: outcome.changes.len(),
        returning,
        revision: outcome.revision.map(|name| Revision {
            number: name.seq,
            id: name.id,
        }),
        replayed: outcome.replayed,
        warnings: outcome.warnings.clone(),
    }
}

/// The rows an insert or update wrote to its own table, as written.
fn written(outcome: &Outcome, table: &str) -> Vec<Map<String, Value>> {
    outcome
        .rows
        .iter()
        .filter(|change| change.table == table)
        .filter(|change| {
            !outcome.induced.iter().any(|induced| {
                change
                    .before
                    .as_ref()
                    .is_some_and(|before| before.relative == induced.path)
            })
        })
        .filter_map(|change| change.after.clone())
        .collect()
}

impl Connection {
    /// The database at `path`: a folder of JSON files, adopted as a
    /// database the first time it is opened unless `options` forbid it.
    pub fn open(path: impl AsRef<Path>, options: Options) -> Result<Self> {
        let root = crate::state::resolve_root(Some(path.as_ref()))?.path;
        let connection = Self { root, options };
        // Opening establishes the database, so a folder that cannot be one
        // is refused here, not at the first query.
        connection.observe()?;
        Ok(connection)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn overrides(&self) -> ResourceOverrides {
        ResourceOverrides {
            wait_seconds: self.options.wait_seconds,
            timeout_seconds: self.options.timeout_seconds,
            max_result_rows: self.options.max_result_rows,
            ..ResourceOverrides::default()
        }
    }

    fn open_as(&self, access: Access) -> Result<Option<Database>> {
        let opening = Opening {
            access,
            establish: !self.options.no_establish,
            rebuild_metadata: false,
            dry_run: false,
            ignore: vec![],
        };
        match crate::state::open(&self.root, opening, &self.overrides())? {
            Opened::Empty => Ok(None),
            Opened::Database { database, .. } => Ok(Some(*database)),
        }
    }

    /// The database as it is now, to read.
    fn observe(&self) -> Result<Option<Database>> {
        self.open_as(if self.options.readonly {
            Access::Read
        } else {
            Access::Observe
        })
    }

    /// The database as it is now, valid, held for a change.
    fn writable(&self) -> Result<Database> {
        if self.options.readonly {
            return Err(DbError::new(
                Code::READ_ONLY,
                "this connection was opened read-only",
            ));
        }
        let database = self.open_as(Access::Write)?.ok_or_else(no_tables)?;
        database.require_valid()?;
        Ok(database)
    }

    fn readable(&self) -> Result<Database> {
        let database = self.observe()?.ok_or_else(no_tables)?;
        database.require_valid()?;
        Ok(database)
    }

    fn request(change: &Change<'_>) -> Request {
        Request {
            idempotency_key: change.idempotency_key.map(String::from),
            widen: change.widen,
            ..Request::new(Origin::User, false)
        }
    }

    /// Run a read statement and collect its answer.
    pub fn query(&self, sql: &str, values: &[Value]) -> Result<Rows> {
        self.query_with(sql, &params(values))
    }

    /// Run a read statement with named or positional parameters.
    pub fn query_with(&self, sql: &str, params: &[SqlParam]) -> Result<Rows> {
        let database = self.readable()?;
        collect(&database, sql, params)
    }

    /// Run a read statement, handing over each row as it is produced.
    pub fn query_each(
        &self,
        sql: &str,
        values: &[Value],
        each: impl FnMut(Map<String, Value>) -> Result<()>,
    ) -> Result<usize> {
        let database = self.readable()?;
        crate::sql::query(
            &database.catalog.mirror,
            &database.catalog.schemas,
            sql,
            &params(values),
            database.query_limits(),
            each,
        )
        .map_err(|error| crate::sql::explain_unknown_table(&database.catalog, error))
    }

    /// Run one INSERT, UPDATE or DELETE as one validated change.
    pub fn execute(&self, sql: &str, values: &[Value]) -> Result<Changed> {
        self.execute_with(sql, &params(values), Change::default())
    }

    pub fn execute_with(
        &self,
        sql: &str,
        params: &[SqlParam],
        change: Change<'_>,
    ) -> Result<Changed> {
        if crate::sql::classify(sql)? != StatementKind::Mutation {
            return Err(DbError::usage(
                "execute changes rows; a statement that reads is a query",
            ));
        }
        let mut database = self.writable()?;
        let limits = database.query_limits();
        let (outcome, returning) = database.apply_with(Self::request(&change), |catalog| {
            let mutation = crate::sql::mutate(catalog, sql, params, limits)
                .map_err(|error| crate::sql::explain_unknown_table(catalog, error))?;
            Ok((mutation.rows, mutation.returning))
        })?;
        Ok(changed(&outcome, returning.unwrap_or_default()))
    }

    /// Run one statement once for each set of values, all as one change.
    pub fn execute_many(&self, sql: &str, rows: &[Vec<Value>]) -> Result<Changed> {
        let mut transaction = self.begin()?;
        let mut returning = vec![];
        for values in rows {
            returning.extend(transaction.execute(sql, values)?.returning);
        }
        let mut done = transaction.commit()?;
        done.returning = returning;
        Ok(done)
    }

    /// Begin a transaction: it holds the writer lock until it ends.
    pub fn begin(&self) -> Result<Transaction> {
        self.begin_with(Change::default())
    }

    pub fn begin_with(&self, change: Change<'_>) -> Result<Transaction> {
        Ok(Transaction {
            inner: self.writable()?.begin()?,
            request: Self::request(&change),
        })
    }

    /// Run `work` in a transaction, committed when it succeeds and rolled
    /// back when it fails.
    pub fn transaction<T>(
        &self,
        work: impl FnOnce(&mut Transaction) -> Result<T>,
    ) -> Result<(T, Changed)> {
        let mut transaction = self.begin()?;
        let value = work(&mut transaction)?;
        Ok((value, transaction.commit()?))
    }

    fn change_rows(
        &self,
        change: Change<'_>,
        table: &str,
        plan: impl FnOnce(&Catalog) -> Result<Vec<RowChange>>,
    ) -> Result<Changed> {
        let mut database = self.writable()?;
        let (outcome, _) =
            database.apply_with(Self::request(&change), |catalog| Ok((plan(catalog)?, ())))?;
        let rows = written(&outcome, table);
        Ok(changed(&outcome, rows))
    }

    /// Insert rows, generated columns filled; the rows as written come back.
    pub fn insert(&self, table: &str, rows: Vec<Map<String, Value>>) -> Result<Changed> {
        self.change_rows(Change::default(), table, |catalog| {
            crate::rows::insert(catalog, table, rows)
        })
    }

    /// Set columns of the row a key names.
    pub fn update(
        &self,
        table: &str,
        key: &[Value],
        patch: &Map<String, Value>,
    ) -> Result<Changed> {
        self.change_rows(Change::default(), table, |catalog| {
            Ok(vec![crate::rows::update(catalog, table, key, patch)?])
        })
    }

    /// Delete the row a key names, with its referrers' actions.
    pub fn delete(&self, table: &str, key: &[Value]) -> Result<Changed> {
        self.change_rows(Change::default(), table, |catalog| {
            Ok(vec![crate::rows::delete(catalog, table, key)?])
        })
    }

    /// Import rows as one change: inserted, merged into the rows with their
    /// keys, or replacing the table's rows, as `mode` says.
    pub fn import(
        &self,
        table: &str,
        rows: Vec<Map<String, Value>>,
        mode: crate::rows::ImportMode,
    ) -> Result<(Changed, crate::rows::Imported)> {
        let mut database = self.writable()?;
        let (outcome, counts) = database.apply_with(
            Request {
                origin: Origin::Import,
                ..Request::new(Origin::Import, false)
            },
            |catalog| crate::rows::import(catalog, table, rows, mode),
        )?;
        Ok((changed(&outcome, vec![]), counts.unwrap_or_default()))
    }

    /// The row a key names, if there is one.
    pub fn get(&self, table: &str, key: &[Value]) -> Result<Option<Map<String, Value>>> {
        let database = self.readable()?;
        Ok(crate::rows::find(&database.catalog, table, key)?.map(|row| row.value))
    }

    /// The tables.
    pub fn tables(&self) -> Result<Vec<Table>> {
        let Some(database) = self.observe()? else {
            return Ok(vec![]);
        };
        database
            .catalog
            .schemas
            .iter()
            .map(|(name, schema)| {
                Ok(Table {
                    name: name.clone(),
                    rows: database.catalog.mirror.count(name)?,
                    key: schema.primary_key().to_vec(),
                    pinned: database.catalog.pinned.contains(name),
                })
            })
            .collect()
    }

    /// Validate everything.
    pub fn check(&self) -> Result<Report> {
        let Some(database) = self.observe()? else {
            return Ok(Report {
                valid: true,
                errors: vec![],
                warnings: vec![],
            });
        };
        Ok(Report {
            valid: database.is_valid(),
            errors: database.verdict.errors.clone(),
            warnings: database.verdict.warnings.clone(),
        })
    }

    /// A name for the state the database holds now: two calls answer the
    /// same token exactly when nothing changed between them.
    pub fn state(&self) -> Result<String> {
        match self.observe()? {
            Some(database) => database.state_token(),
            None => Ok(String::new()),
        }
    }
}

fn collect(database: &Database, sql: &str, params: &[SqlParam]) -> Result<Rows> {
    let mut columns: Vec<String> = vec![];
    let mut rows = vec![];
    crate::sql::query(
        &database.catalog.mirror,
        &database.catalog.schemas,
        sql,
        params,
        database.query_limits(),
        |row| {
            if columns.is_empty() {
                columns = row.keys().cloned().collect();
            }
            rows.push(row);
            Ok(())
        },
    )
    .map_err(|error| crate::sql::explain_unknown_table(&database.catalog, error))?;
    if columns.is_empty() {
        columns = crate::sql::columns(&database.catalog.mirror, &database.catalog.schemas, sql)?;
    }
    Ok(Rows { columns, rows })
}

/// Several statements, committed as one change or not at all. Each sees what
/// the ones before it did; the state they make together is judged when it
/// commits. Dropped unfinished, it is rolled back.
pub struct Transaction {
    inner: crate::transaction::Transaction,
    request: Request,
}

impl Transaction {
    /// Run a read statement against what the transaction has done.
    pub fn query(&self, sql: &str, values: &[Value]) -> Result<Rows> {
        self.query_with(sql, &params(values))
    }

    pub fn query_with(&self, sql: &str, params: &[SqlParam]) -> Result<Rows> {
        let mut columns: Vec<String> = vec![];
        let mut rows = vec![];
        self.inner
            .query(sql, params, |row| {
                if columns.is_empty() {
                    columns = row.keys().cloned().collect();
                }
                rows.push(row);
                Ok(())
            })
            .map_err(|error| {
                crate::sql::explain_unknown_table(&self.inner.view().catalog, error)
            })?;
        if columns.is_empty() {
            let view = self.inner.view();
            columns = crate::sql::columns(&view.catalog.mirror, &view.catalog.schemas, sql)?;
        }
        Ok(Rows { columns, rows })
    }

    /// Run a statement that changes rows; its referential actions run now.
    pub fn execute(&mut self, sql: &str, values: &[Value]) -> Result<Changed> {
        self.execute_with(sql, &params(values))
    }

    pub fn execute_with(&mut self, sql: &str, params: &[SqlParam]) -> Result<Changed> {
        if crate::sql::classify(sql)? != StatementKind::Mutation {
            return Err(DbError::usage(
                "execute changes rows; a statement that reads is a query",
            ));
        }
        let executed = self.inner.execute(sql, params)?;
        Ok(Self::executed(
            executed.rows.len(),
            executed.induced.len(),
            executed.returning,
        ))
    }

    fn executed(rows: usize, induced: usize, returning: Vec<Map<String, Value>>) -> Changed {
        Changed {
            rows,
            induced,
            files: 0,
            returning,
            revision: None,
            replayed: false,
            warnings: vec![],
        }
    }

    /// Insert rows into a table within the transaction.
    pub fn insert(&mut self, table: &str, rows: Vec<Map<String, Value>>) -> Result<Changed> {
        let changes = crate::rows::insert(&self.inner.view().catalog, table, rows)?;
        let returning = changes
            .iter()
            .filter_map(|change| change.after.clone())
            .collect();
        let executed = self.inner.apply(changes)?;
        Ok(Self::executed(
            executed.rows.len(),
            executed.induced.len(),
            returning,
        ))
    }

    /// Set columns of the row a key names, within the transaction.
    pub fn update(
        &mut self,
        table: &str,
        key: &[Value],
        patch: &Map<String, Value>,
    ) -> Result<Changed> {
        let change = crate::rows::update(&self.inner.view().catalog, table, key, patch)?;
        let returning = change.after.iter().cloned().collect();
        let executed = self.inner.apply(vec![change])?;
        Ok(Self::executed(
            executed.rows.len(),
            executed.induced.len(),
            returning,
        ))
    }

    /// Delete the row a key names, within the transaction.
    pub fn delete(&mut self, table: &str, key: &[Value]) -> Result<Changed> {
        let change = crate::rows::delete(&self.inner.view().catalog, table, key)?;
        let executed = self.inner.apply(vec![change])?;
        Ok(Self::executed(
            executed.rows.len(),
            executed.induced.len(),
            vec![],
        ))
    }

    /// How many statements have run.
    pub fn statements(&self) -> usize {
        self.inner.statements()
    }

    /// Commit everything as one change.
    pub fn commit(self) -> Result<Changed> {
        let (_, outcome) = self.inner.commit(self.request)?;
        Ok(changed(&outcome, vec![]))
    }

    /// Discard everything.
    pub fn rollback(self) -> Result<()> {
        self.inner.rollback().map(|_| ())
    }
}
