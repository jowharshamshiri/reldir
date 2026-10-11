//! Transactions: several statements, committed as one change or not at all.
//!
//! A transaction holds the writer lock from its first statement to its end.
//! Each statement is planned against the state the statements before it left
//! -- a later `SELECT` reads an earlier `INSERT` -- and its referential
//! actions run at its end, as SQL's do. Nothing reaches a file until
//! `commit`, which judges the state all the statements make together and
//! writes it as one revision; a constraint is checked then, so a statement
//! may break it in passing as long as a later one mends it. `rollback`, an
//! error, or a transaction dropped unfinished leaves the files as they were.
//!
//! The transaction keeps, for every path a statement touched, the row there
//! when it began and the row there now. Rows changed, changed again and
//! changed back are no change at all.

use crate::{
    catalog::Row,
    code::Code,
    db::{Database, Outcome, Request},
    diagnostic::{DbError, Diagnostic, Result},
    engine::fs::LockGuard,
    plan::{FileChange, RowChange},
    referential::Induced,
    sql::{QueryLimits, SqlParam},
};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Instant,
};

/// The mirror savepoint a transaction's statements write into.
const SAVEPOINT: &str = "reldir_transaction";

/// What a path held when the transaction began, and holds now.
#[derive(Debug, Clone)]
struct Net {
    began: Option<Row>,
    now: Option<(String, Map<String, Value>)>,
}

/// What one statement did.
#[derive(Debug, Default)]
pub struct Executed {
    /// The rows it changed, referential actions included.
    pub rows: Vec<RowChange>,
    pub induced: Vec<Induced>,
    /// What its `RETURNING` clause produced.
    pub returning: Vec<Map<String, Value>>,
}

/// An open transaction. It owns the database until it is committed or rolled
/// back, which give it back.
pub struct Transaction {
    /// `None` once finished.
    database: Option<Database>,
    /// The catalog as the transaction found it, restored when it ends.
    began: crate::catalog::Catalog,
    /// The lock, when the database did not already hold it.
    _lock: Option<LockGuard>,
    net: BTreeMap<PathBuf, Net>,
    /// The bytes the transaction gave each file it wrote.
    written: BTreeMap<PathBuf, Vec<u8>>,
    /// The referential actions the statements' ends ran.
    induced: Vec<Induced>,
    started: Instant,
    statements: usize,
}

impl Database {
    /// Begin a transaction: take the writer lock, if this database does not
    /// hold it, and plan every statement until the end against what the
    /// transaction has done. A database opened to read takes no lock, and
    /// its transaction can only be committed as a dry run.
    pub fn begin(mut self) -> Result<Transaction> {
        let lock = self.planning_lock(self.access() != crate::db::Access::Write)?;
        self.catalog.mirror.savepoint(SAVEPOINT)?;
        Ok(Transaction {
            began: self.catalog.clone(),
            database: Some(self),
            _lock: lock,
            net: BTreeMap::new(),
            written: BTreeMap::new(),
            induced: vec![],
            started: Instant::now(),
            statements: 0,
        })
    }
}

impl Transaction {
    fn database(&self) -> &Database {
        self.database
            .as_ref()
            .expect("a transaction has its database until it ends")
    }

    fn database_mut(&mut self) -> &mut Database {
        self.database
            .as_mut()
            .expect("a transaction has its database until it ends")
    }

    /// The database as the transaction's statements have left it.
    pub fn view(&self) -> &Database {
        self.database()
    }

    /// How many statements have run.
    pub fn statements(&self) -> usize {
        self.statements
    }

    /// A transaction past its time is ended, not continued: it holds every
    /// other writer off.
    fn check_time(&self) -> Result<()> {
        let limit = self.database().config.transaction_timeout_seconds;
        if self.started.elapsed().as_secs_f64() > limit {
            return Err(DbError::from_diag(
                Diagnostic::error(
                    Code::TRANSACTION_EXPIRED,
                    format!(
                        "the transaction held the writer lock for more than {limit} s and was \
                         rolled back; nothing it did was written"
                    ),
                )
                .help("commit sooner, or raise transaction_timeout_seconds in .db/config"),
            ));
        }
        Ok(())
    }

    /// Run a read against what the transaction has done.
    pub fn query(
        &self,
        text: &str,
        params: &[SqlParam],
        emit: impl FnMut(Map<String, Value>) -> Result<()>,
    ) -> Result<usize> {
        self.check_time()?;
        let database = self.database();
        crate::sql::query(
            &database.catalog.mirror,
            &database.catalog.schemas,
            text,
            params,
            database.query_limits(),
            emit,
        )
    }

    /// Run a statement that changes rows.
    pub fn execute(&mut self, text: &str, params: &[SqlParam]) -> Result<Executed> {
        self.check_time()?;
        let limits: QueryLimits = self.database().query_limits();
        let mutation = crate::sql::mutate(&self.database().catalog, text, params, limits)
            .map_err(|error| crate::sql::explain_unknown_table(&self.database().catalog, error))?;
        let mut executed = self.apply(mutation.rows)?;
        executed.returning = mutation.returning;
        Ok(executed)
    }

    /// Change rows: a row command, or rows a statement planned. Their
    /// referential actions run now, at the end of the statement.
    pub fn apply(&mut self, rows: Vec<RowChange>) -> Result<Executed> {
        self.check_time()?;
        self.statements += 1;
        let catalog = &self.database().catalog;
        let expansion = crate::referential::expand(catalog, rows)?;
        let written = &self.written;
        let files = crate::plan::render_with(catalog, &expansion.rows, &|row: &Row| match written
            .get(&row.relative)
        {
            Some(bytes) => Ok(bytes.clone()),
            None => row.raw(&catalog.root),
        })?;
        let mut writes = BTreeMap::new();
        let mut deletes = BTreeSet::new();
        for change in &files {
            match change {
                FileChange::Write { path, bytes } => {
                    writes.insert(path.clone(), bytes.clone());
                }
                FileChange::Delete { path } => {
                    deletes.insert(path.clone());
                }
            }
        }
        // Each path keeps the row it held when the transaction began. Every
        // row the statement moves or removes leaves its path before any is
        // placed, so two rows trading paths are both kept.
        let schemas = self.database().catalog.schemas.clone();
        for before in expansion
            .rows
            .iter()
            .filter_map(|change| change.before.as_ref())
        {
            self.net
                .entry(before.relative.clone())
                .or_insert_with(|| Net {
                    began: Some(before.clone()),
                    now: None,
                })
                .now = None;
        }
        for change in &expansion.rows {
            if let Some(after) = &change.after {
                let schema = schemas
                    .get(&change.table)
                    .ok_or_else(|| self.database().catalog.unknown_table(&change.table))?;
                let path = crate::plan::row_path(schema, after)?;
                // Read before this statement's rows are taken in: a path
                // no statement touched yet holds what it held at the start.
                let began = match self.net.get(&path) {
                    Some(net) => net.began.clone(),
                    None => self.database().catalog.row_at(&path)?,
                };
                self.net.insert(
                    path,
                    Net {
                        began,
                        now: Some((change.table.clone(), after.clone())),
                    },
                );
            }
        }
        let config = self.database().config.clone();
        let future = self
            .database()
            .catalog
            .ingest_rows(&config, &writes, &deletes)?
            .ok_or_else(|| {
                DbError::new(
                    Code::INTERNAL_ERROR,
                    "a statement in a transaction changed a file that is not a row",
                )
            })?;
        self.database_mut().catalog = future;
        for (path, bytes) in writes {
            self.written.insert(path, bytes);
        }
        for path in deletes {
            self.written.remove(&path);
        }
        self.induced.extend(expansion.induced.iter().cloned());
        Ok(Executed {
            rows: expansion.rows,
            induced: expansion.induced,
            returning: vec![],
        })
    }

    /// End the transaction without writing anything.
    pub fn rollback(mut self) -> Result<Database> {
        self.unwind()?;
        Ok(self.database.take().expect("a transaction ends once"))
    }

    /// Return the mirror and the catalog to what the transaction found.
    fn unwind(&mut self) -> Result<()> {
        let began = self.began.clone();
        let database = self.database_mut();
        database.catalog.mirror.rollback_to(SAVEPOINT)?;
        database.catalog = began;
        Ok(())
    }

    /// Commit everything the statements did as one change: judged as the
    /// state it makes, guarded against new contract violations, recorded as
    /// one revision. The referential actions ran at each statement's end and
    /// are not run again; those whose row ended as it began are not reported.
    pub fn commit(mut self, request: Request) -> Result<(Database, Outcome)> {
        self.check_time()?;
        self.unwind()?;
        let rows: Vec<RowChange> = std::mem::take(&mut self.net)
            .into_values()
            .filter_map(|net| match (net.began, net.now) {
                (Some(began), Some((_, now))) if began.value == now => None,
                (None, None) => None,
                (began, now) => Some(RowChange {
                    table: began
                        .as_ref()
                        .map(|row| row.table.clone())
                        .or_else(|| now.as_ref().map(|(table, _)| table.clone()))
                        .expect("one side holds a row"),
                    before: began,
                    after: now.map(|(_, row)| row),
                }),
            })
            .collect();
        let schemas = &self.began.schemas;
        let mut touched = BTreeSet::new();
        for change in &rows {
            if let Some(before) = &change.before {
                touched.insert(before.relative.clone());
            }
            if let (Some(after), Some(schema)) = (&change.after, schemas.get(&change.table)) {
                touched.insert(crate::plan::row_path(schema, after)?);
            }
        }
        let induced: Vec<Induced> = std::mem::take(&mut self.induced)
            .into_iter()
            .filter(|induced| touched.contains(&induced.path))
            .collect();
        let mut database = self.database.take().expect("a transaction ends once");
        let outcome = database.change_expanded(rows, induced, request)?;
        Ok((database, outcome))
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        // Unfinished -- an error, a panic, an abandoned session: nothing it
        // did is written, and the mirror is left as it was found.
        if self.database.is_some() {
            let _ = self.unwind();
        }
    }
}
