//! The mirror: a derived relational copy of the governed directory.
//!
//! Rows live in JSON files and nowhere else; this is an index over them, kept in
//! `.db/mirror.sqlite`, rebuildable from the files at any moment and never
//! consulted to decide what a row *is*. It exists so that a command costs what
//! changed rather than what exists:
//!
//! - a **stat cache** per file (size, times, inode) with the parsed canonical
//!   row and its row-local verdict, so an unchanged file is neither re-read nor
//!   re-validated;
//! - **typed tables**, one per governed table, which SQL queries run against;
//! - every **candidate key** (primary key and each unique constraint) and every
//!   **reference edge**, so uniqueness, identity domains, foreign keys and
//!   acyclicity are answered by indexed queries over the whole database instead
//!   of by holding the whole database in memory.
//!
//! The typed tables carry no constraints: the mirror must be able to hold an
//! invalid state in order to describe it. Validity is decided by
//! [`crate::integrity`], which reads the mirror.
//!
//! A mirror that is missing, from another layout, or unreadable is discarded
//! and rebuilt from the files, and the command says so. Nothing is lost,
//! because nothing here is authoritative.

use crate::code::Code;
use crate::{
    canonical,
    diagnostic::{DbError, Diagnostic, Result},
    schema::{ColumnType, Schema},
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, types::Value as SqlValue};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
};

mod ddl;
mod files;
mod keys;
mod recorded;
mod stats;

use files::decode_diagnostics;
use keys::{constraints, signature};

pub use keys::{
    Duplicate, Edge, RowEdge, acyclic_rule, constraint_name, edges, fk_rule, index_name, key,
    quote, row_keys,
};

/// The layout this binary reads and writes. A mirror of any other layout is
/// rebuilt rather than interpreted.
pub const LAYOUT: &str = "reldir-mirror-5";

/// The constraint name a primary key is stored under in `_reldir_keys`.
pub const PRIMARY: &str = "pk";

/// Where the mirror lives.
pub fn path(root: &Path) -> PathBuf {
    root.join(".db/mirror.sqlite")
}

/// What the filesystem said about a file the last time it was read.
pub use crate::engine::fs::Stat;

/// The racy window assumed where the filesystem's timestamp granularity has
/// not been measured: two seconds, the coarsest in use (FAT's).
pub const COARSEST_GRANULARITY_NS: i64 = 2_000_000_000;

/// A row that differs from the recorded head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowChangeSince {
    pub path: String,
    /// The row's hash and its file's hash now; `None` when it is gone.
    pub after: Option<(String, String)>,
    /// Whether the recorded head holds a row at the path.
    pub recorded: bool,
}

/// One governed file as the mirror knows it.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub table: String,
    pub stat: Stat,
    /// SHA-256 of the file's bytes.
    pub raw_hash: String,
    /// When the entry was recorded, in the filesystem's clock.
    pub seen_ns: i64,
    /// SHA-256 of the canonical row, which the state root is built from.
    /// Absent when the file does not hold a row.
    pub row_hash: Option<String>,
    /// The canonical row, in schema column order.
    pub doc: Option<Map<String, Value>>,
    /// What is wrong with the file on its own.
    pub diagnostics: Vec<Diagnostic>,
}

/// What the stat pass knows about a file without its row: enough to decide
/// whether the file must be read again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    pub stat: Stat,
    /// SHA-256 of the file's bytes when last read.
    pub raw_hash: String,
    /// When it was last read, in the filesystem's clock.
    pub seen_ns: i64,
}

impl Stamp {
    /// Whether the stamp still describes a file with this stat, without
    /// reading the file.
    pub fn trusted_for(&self, stat: &Stat, granularity_ns: i64) -> bool {
        self.stat == *stat && stat.mtime_ns + granularity_ns < self.seen_ns
    }
}

/// A row ready to be written into the mirror.
pub struct Ingest<'a> {
    pub path: &'a str,
    pub table: &'a str,
    pub schema: &'a Schema,
    pub stat: Stat,
    pub raw_hash: String,
    pub seen_ns: i64,
    /// The row, when the file parsed to an object.
    pub row: Option<&'a Map<String, Value>>,
    pub diagnostics: Vec<Diagnostic>,
    /// The style the file is written in, when its bytes were read.
    pub style: Option<String>,
}

pub struct Mirror {
    conn: Connection,
    /// The sequences generated columns hand out while a statement is planned.
    sequences: crate::index::types::Sequences,
    /// Whether changes are written to `.db/mirror.sqlite` rather than held in
    /// memory for this process only.
    persistent: bool,
}

/// The `_reldir_meta` key recording how many rows a table had when its
/// planner statistics were gathered.
fn analyzed_key(table: &str) -> String {
    format!("analyzed_rows:{table}")
}

fn corrupt(error: impl std::fmt::Display) -> DbError {
    DbError::new(
        Code::INTERNAL_METADATA_CORRUPT,
        format!("derived mirror: {error}"),
    )
}

fn register(conn: &Connection) -> Result<crate::index::types::Sequences> {
    let sequences = crate::index::types::Sequences::default();
    crate::index::types::register(conn, &sequences)?;
    Ok(sequences)
}

const INTERNAL: &str = "
CREATE TABLE IF NOT EXISTS _reldir_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS _reldir_tables (
    tbl TEXT PRIMARY KEY,
    identity TEXT NOT NULL,
    enforced INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS _reldir_revisions (
    id TEXT PRIMARY KEY,
    seq INTEGER NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS _reldir_recorded (
    path TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    hash TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS _reldir_files (
    path TEXT PRIMARY KEY,
    tbl TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    ctime_ns INTEGER NOT NULL,
    inode INTEGER NOT NULL,
    device INTEGER NOT NULL,
    raw_hash TEXT NOT NULL,
    seen_ns INTEGER NOT NULL,
    row_hash TEXT,
    doc TEXT,
    diagnostics TEXT NOT NULL,
    trow INTEGER,
    -- A fingerprint of the style the file is written in.
    style TEXT,
    -- The file's name as a case-insensitive, normalizing filesystem compares it.
    folded TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS _reldir_files_tbl ON _reldir_files (tbl, trow);
CREATE INDEX IF NOT EXISTS _reldir_files_folded ON _reldir_files (tbl, folded);
CREATE TABLE IF NOT EXISTS _reldir_keys (
    tbl TEXT NOT NULL,
    cons TEXT NOT NULL,
    key TEXT NOT NULL,
    path TEXT NOT NULL,
    PRIMARY KEY (tbl, cons, path)
);
CREATE INDEX IF NOT EXISTS _reldir_keys_lookup ON _reldir_keys (key, cons, tbl);
CREATE INDEX IF NOT EXISTS _reldir_keys_path ON _reldir_keys (path);
CREATE TABLE IF NOT EXISTS _reldir_edges (
    path TEXT NOT NULL,
    rule TEXT NOT NULL,
    ptr TEXT NOT NULL,
    target TEXT NOT NULL,
    PRIMARY KEY (path, rule, ptr)
);
CREATE INDEX IF NOT EXISTS _reldir_edges_target ON _reldir_edges (target, rule);
CREATE INDEX IF NOT EXISTS _reldir_edges_rule ON _reldir_edges (rule);
-- Paths whose row may differ between the files and the recorded head: every
-- path absent from here holds exactly what was recorded, so what changed since
-- the last revision is found without comparing every row. The triggers keep it
-- in the same transaction as the change that unsettles a path; recording
-- settles the paths it brought into agreement. A trigger's own conflict clause
-- would be overridden by the statement that fires it (an upsert makes it an
-- abort), so each inserts only a path that is not there yet.
CREATE TABLE IF NOT EXISTS _reldir_unsettled (path TEXT PRIMARY KEY) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS _reldir_files_added AFTER INSERT ON _reldir_files
BEGIN
    INSERT INTO _reldir_unsettled (path) SELECT NEW.path
    WHERE NOT EXISTS (SELECT 1 FROM _reldir_unsettled WHERE path = NEW.path);
END;
CREATE TRIGGER IF NOT EXISTS _reldir_files_removed AFTER DELETE ON _reldir_files
BEGIN
    INSERT INTO _reldir_unsettled (path) SELECT OLD.path
    WHERE NOT EXISTS (SELECT 1 FROM _reldir_unsettled WHERE path = OLD.path);
END;
CREATE TRIGGER IF NOT EXISTS _reldir_files_changed AFTER UPDATE OF path, row_hash ON _reldir_files
BEGIN
    INSERT INTO _reldir_unsettled (path) SELECT OLD.path
    WHERE NOT EXISTS (SELECT 1 FROM _reldir_unsettled WHERE path = OLD.path);
    INSERT INTO _reldir_unsettled (path) SELECT NEW.path
    WHERE NOT EXISTS (SELECT 1 FROM _reldir_unsettled WHERE path = NEW.path);
END;
CREATE TRIGGER IF NOT EXISTS _reldir_recorded_added AFTER INSERT ON _reldir_recorded
BEGIN
    INSERT INTO _reldir_unsettled (path) SELECT NEW.path
    WHERE NOT EXISTS (SELECT 1 FROM _reldir_unsettled WHERE path = NEW.path);
END;
CREATE TRIGGER IF NOT EXISTS _reldir_recorded_removed AFTER DELETE ON _reldir_recorded
BEGIN
    INSERT INTO _reldir_unsettled (path) SELECT OLD.path
    WHERE NOT EXISTS (SELECT 1 FROM _reldir_unsettled WHERE path = OLD.path);
END;
CREATE TRIGGER IF NOT EXISTS _reldir_recorded_changed AFTER UPDATE ON _reldir_recorded
BEGIN
    INSERT INTO _reldir_unsettled (path) SELECT OLD.path
    WHERE NOT EXISTS (SELECT 1 FROM _reldir_unsettled WHERE path = OLD.path);
    INSERT INTO _reldir_unsettled (path) SELECT NEW.path
    WHERE NOT EXISTS (SELECT 1 FROM _reldir_unsettled WHERE path = NEW.path);
END;
";

impl Mirror {
    /// Open the persistent mirror for writing. The caller holds the writer
    /// lock. A mirror that cannot be read, or has another layout, is replaced
    /// by an empty one; the returned flag says so, so the command can report it.
    pub fn open_persistent(root: &Path, verify: bool) -> Result<(Self, bool)> {
        let file = path(root);
        // After an interrupted commit the mirror may hold a torn page; it is
        // derived, so one that fails SQLite's own check is rebuilt, never used.
        let opened = Self::try_open_file(&file).and_then(|mirror| match verify {
            true => match mirror.integrity_check()? {
                None => Ok(mirror),
                Some(problem) => Err(corrupt(problem)),
            },
            false => Ok(mirror),
        });
        match opened {
            Ok(mirror) => Ok((mirror, false)),
            Err(_) => {
                for suffix in ["", "-journal", "-wal", "-shm"] {
                    let stale = PathBuf::from(format!("{}{suffix}", file.display()));
                    match std::fs::remove_file(&stale) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(DbError::io(&stale, error)),
                    }
                }
                Ok((Self::try_open_file(&file)?, true))
            }
        }
    }

    fn try_open_file(file: &Path) -> Result<Self> {
        let conn = Connection::open(file).map_err(corrupt)?;
        // Write-ahead logging lets a reader keep answering from the last
        // committed state while a writer changes it. The mirror is derived,
        // so NORMAL synchronisation -- durable up to the last checkpoint --
        // is enough: a lost tail is re-read from the files. The last
        // connection to close checkpoints the log and removes it, so the next
        // process finds the whole mirror in one file and nothing to replay.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY; \
             PRAGMA foreign_keys=OFF;",
        )
        .map_err(corrupt)?;
        let sequences = register(&conn)?;
        let mirror = Self {
            conn,
            sequences,
            persistent: true,
        };
        mirror.initialize()?;
        Ok(mirror)
    }

    /// The persistent mirror opened to answer from its last committed state
    /// while a writer holds the lock: one read transaction, begun now, so
    /// every question is asked of one state. `None` when there is no mirror
    /// in this layout to read.
    pub fn open_snapshot(root: &Path) -> Result<Option<Self>> {
        let file = path(root);
        let conn = match Connection::open_with_flags(
            &file,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(conn) => conn,
            Err(_) if !file.exists() => return Ok(None),
            Err(error) => return Err(corrupt(error)),
        };
        conn.execute_batch("PRAGMA temp_store=MEMORY; PRAGMA foreign_keys=OFF;")
            .map_err(corrupt)?;
        let sequences = register(&conn)?;
        let mirror = Self {
            conn,
            sequences,
            persistent: true,
        };
        mirror.hold_snapshot()?;
        let layout: Option<String> = mirror
            .conn
            .query_row(
                "SELECT value FROM _reldir_meta WHERE key = 'layout'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap_or(None);
        if layout.as_deref() != Some(LAYOUT) {
            return Ok(None);
        }
        Ok(Some(mirror))
    }

    /// Begin a read transaction and read through it, so the mirror answers
    /// from this one state until the connection closes, whatever a writer
    /// commits meanwhile.
    pub fn hold_snapshot(&self) -> Result<()> {
        self.conn
            .execute_batch("BEGIN DEFERRED; SELECT count(*) FROM sqlite_master;")
            .map_err(corrupt)
    }

    /// An empty mirror held in memory, for folders that carry no database.
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(corrupt)?;
        conn.execute_batch("PRAGMA temp_store=MEMORY; PRAGMA foreign_keys=OFF;")
            .map_err(corrupt)?;
        let sequences = register(&conn)?;
        let mirror = Self {
            conn,
            sequences,
            persistent: false,
        };
        mirror.initialize()?;
        Ok(mirror)
    }

    /// A mirror held only in this process's memory, copied from the persistent
    /// one when it can be read. Nothing written to it reaches the disk: this is
    /// how a read-only command applies what changed since the mirror was last
    /// refreshed without writing a byte.
    pub fn open_ephemeral(root: &Path) -> Result<Self> {
        let conn = match Self::copy_persistent(root)? {
            Some(conn) => conn,
            None => Connection::open_in_memory().map_err(corrupt)?,
        };
        conn.execute_batch("PRAGMA temp_store=MEMORY; PRAGMA foreign_keys=OFF;")
            .map_err(corrupt)?;
        let sequences = register(&conn)?;
        let mut mirror = Self {
            conn,
            sequences,
            persistent: false,
        };
        if mirror.initialize().is_err() {
            // A copy of a mirror from another layout is no base to build on.
            let conn = Connection::open_in_memory().map_err(corrupt)?;
            let sequences = register(&conn)?;
            mirror = Self {
                conn,
                sequences,
                persistent: false,
            };
            mirror.initialize()?;
        }
        Ok(mirror)
    }

    /// An in-memory copy of the persistent mirror, made without opening it
    /// in place -- a reader in write-ahead mode updates the log's shared
    /// index, and a `--readonly` reader writes nothing under the root. The
    /// files are copied to a private directory and checked unchanged across
    /// the copy, so the copy is one committed state. `None` when there is
    /// no mirror, or a writer changed it meanwhile: the folder is then read
    /// afresh, which costs time and never correctness.
    fn copy_persistent(root: &Path) -> Result<Option<Connection>> {
        let file = path(root);
        let log = PathBuf::from(format!("{}-wal", file.display()));
        let stamp = |path: &Path| -> Option<(u64, std::time::SystemTime)> {
            std::fs::metadata(path)
                .ok()
                .map(|meta| (meta.len(), meta.modified().unwrap_or(std::time::UNIX_EPOCH)))
        };
        let before = (stamp(&file), stamp(&log));
        if before.0.is_none() {
            return Ok(None);
        }
        let scratch =
            tempfile::tempdir().map_err(|error| DbError::io(&std::env::temp_dir(), error))?;
        let copy = scratch.path().join("mirror.sqlite");
        if std::fs::copy(&file, &copy).is_err()
            || (before.1.is_some()
                && std::fs::copy(&log, scratch.path().join("mirror.sqlite-wal")).is_err())
        {
            return Ok(None);
        }
        if (stamp(&file), stamp(&log)) != before {
            return Ok(None);
        }
        let Ok(source) = Connection::open(&copy) else {
            return Ok(None);
        };
        let mut conn = Connection::open_in_memory().map_err(corrupt)?;
        let copied = rusqlite::backup::Backup::new(&source, &mut conn)
            .and_then(|backup| backup.run_to_completion(256, std::time::Duration::ZERO, None));
        Ok(copied.ok().map(|()| conn))
    }

    fn initialize(&self) -> Result<()> {
        let layout: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM _reldir_meta WHERE key = 'layout'",
                [],
                |row| row.get(0),
            )
            .optional()
            .or_else(|error| match error {
                rusqlite::Error::SqliteFailure(_, Some(ref message))
                    if message.contains("no such table") =>
                {
                    Ok(None)
                }
                other => Err(other),
            })
            .map_err(corrupt)?;
        match layout.as_deref() {
            Some(LAYOUT) => {}
            Some(other) => return Err(corrupt(format!("layout {other} is not {LAYOUT}"))),
            None => {
                let empty: i64 = self
                    .conn
                    .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
                    .map_err(corrupt)?;
                if empty != 0 {
                    return Err(corrupt("an unlabelled database is not a mirror"));
                }
                self.conn.execute_batch(INTERNAL).map_err(corrupt)?;
                self.conn
                    .execute(
                        "INSERT INTO _reldir_meta (key, value) VALUES ('layout', ?1)",
                        [LAYOUT],
                    )
                    .map_err(corrupt)?;
            }
        }
        Ok(())
    }

    pub fn is_persistent(&self) -> bool {
        self.persistent
    }

    /// The connection user queries run on.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// The sequences generated columns hand out while a statement is planned.
    pub fn sequences(&self) -> &crate::index::types::Sequences {
        &self.sequences
    }

    /// Begin a unit of mirror maintenance. Everything until [`Mirror::commit`]
    /// or [`Mirror::rollback`] lands together or not at all.
    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE").map_err(corrupt)
    }

    pub fn commit(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT").map_err(corrupt)
    }

    pub fn rollback(&self) -> Result<()> {
        self.conn.execute_batch("ROLLBACK").map_err(corrupt)
    }

    /// A named point inside the current unit that can be undone to, used to try
    /// a prospective state and discard it.
    pub fn savepoint(&self, name: &str) -> Result<()> {
        self.conn
            .execute_batch(&format!("SAVEPOINT {name}"))
            .map_err(corrupt)
    }

    pub fn rollback_to(&self, name: &str) -> Result<()> {
        self.conn
            .execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}"))
            .map_err(corrupt)
    }

    pub fn release(&self, name: &str) -> Result<()> {
        self.conn
            .execute_batch(&format!("RELEASE {name}"))
            .map_err(corrupt)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM _reldir_meta WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()
            .map_err(corrupt)
    }

    pub fn clear_meta(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM _reldir_meta WHERE key = ?1", [key])
            .map(|_| ())
            .map_err(corrupt)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO _reldir_meta (key, value) VALUES (?1, ?2) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value \
                 WHERE value IS NOT excluded.value",
                params![key, value],
            )
            .map(|_| ())
            .map_err(corrupt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Schema;
    use serde_json::json;

    fn schema() -> Schema {
        Schema::from_document(
            json!({
                "$schema": crate::schema::meta::DIALECT_URI,
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "email": { "type": "string" },
                    "refs": { "type": "array", "items": { "type": "string" } },
                    "at": { "type": "string", "format": "date-time" }
                },
                "required": ["id", "email", "refs"],
                "additionalProperties": false,
                "x-reldir": {
                    "table": "people",
                    "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}},
                    "unique": [["email"]],
                    "foreignKeys": [{ "from": ["refs[]"], "to": { "table": "people" } }]
                }
            }),
            None,
        )
        .unwrap()
    }

    fn row(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn test2079_planner_statistics_follow_a_table_that_grows_or_shrinks_twofold() {
        let mirror = Mirror::open_memory().unwrap();
        let schema = schema();
        let schemas = std::collections::BTreeMap::from([("people".to_string(), schema.clone())]);
        mirror.sync_schemas(&schemas).unwrap();
        let fill = |from: usize, to: usize| {
            for index in from..to {
                let data = row(
                    json!({"id": format!("p{index}"), "email": format!("{index}@"), "refs": []}),
                );
                mirror
                    .put(Ingest {
                        path: &format!("people/p{index}.json"),
                        table: "people",
                        schema: &schema,
                        stat: Stat::default(),
                        raw_hash: "h".into(),
                        seen_ns: 0,
                        row: Some(&data),
                        diagnostics: vec![],
                        style: None,
                    })
                    .unwrap();
            }
        };
        let gathered = || -> i64 {
            mirror
                .connection()
                .query_row(
                    "SELECT count(*) FROM sqlite_stat1 WHERE tbl = 'people'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0)
        };
        fill(0, 10);
        assert_eq!(
            mirror.analyze_drifted().unwrap(),
            ["people"],
            "never gathered"
        );
        assert!(gathered() > 0);
        fill(10, 100);
        assert!(
            mirror.analyze_drifted().unwrap().is_empty(),
            "within twice the floor"
        );
        fill(100, 200);
        assert_eq!(
            mirror.analyze_drifted().unwrap(),
            ["people"],
            "more than doubled"
        );
        assert!(
            mirror.analyze_drifted().unwrap().is_empty(),
            "gathered at this size"
        );
        for index in 0..150 {
            mirror.remove(&format!("people/p{index}.json")).unwrap();
        }
        assert_eq!(
            mirror.analyze_drifted().unwrap(),
            ["people"],
            "more than halved"
        );
    }

    #[test]
    fn test2070_ingest_records_keys_edges_and_typed_rows() {
        let mirror = Mirror::open_memory().unwrap();
        let schema = schema();
        let schemas = std::collections::BTreeMap::from([("people".to_string(), schema.clone())]);
        assert_eq!(
            mirror.sync_schemas(&schemas).unwrap(),
            vec!["people".to_string()]
        );
        for (id, email, refs) in [
            ("a", "x@", json!(["b"])),
            ("b", "y@", json!(["a", "ghost"])),
        ] {
            let data = row(json!({"id": id, "email": email, "refs": refs}));
            mirror
                .put(Ingest {
                    path: &format!("people/{id}.json"),
                    table: "people",
                    schema: &schema,
                    stat: Stat::default(),
                    raw_hash: "h".into(),
                    seen_ns: 0,
                    row: Some(&data),
                    diagnostics: vec![],
                    style: None,
                })
                .unwrap();
        }
        let dangling = mirror
            .dangling(
                &fk_rule("people", "fk_refs"),
                PRIMARY,
                &["people".to_string()],
            )
            .unwrap();
        assert_eq!(dangling.len(), 1);
        assert_eq!(dangling[0].path, "people/b.json");
        assert_eq!(dangling[0].pointer, "/refs/1");
        assert_eq!(dangling[0].target, "[\"ghost\"]");

        let referrers = mirror
            .referrers("[\"a\"]", &[fk_rule("people", "fk_refs")])
            .unwrap();
        assert_eq!(referrers.len(), 1);
        assert_eq!(referrers[0].path, "people/b.json");

        let count: i64 = mirror
            .connection()
            .query_row(
                "SELECT count(*) FROM people WHERE email LIKE '%@'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "typed rows are queryable");

        // Removing a file removes everything it contributed: its dangling
        // reference goes with it, and the reference to it now dangles.
        mirror.remove("people/b.json").unwrap();
        let dangling = mirror
            .dangling(&fk_rule("people", "fk_refs"), PRIMARY, &["people".into()])
            .unwrap();
        assert_eq!(dangling.len(), 1);
        assert_eq!(
            (dangling[0].path.as_str(), dangling[0].target.as_str()),
            ("people/a.json", "[\"b\"]")
        );
        assert_eq!(mirror.count("people").unwrap(), 1);
    }

    #[test]
    fn test2071_duplicates_are_found_within_and_across_tables() {
        let mirror = Mirror::open_memory().unwrap();
        let schema = schema();
        mirror
            .sync_schemas(&std::collections::BTreeMap::from([(
                "people".to_string(),
                schema.clone(),
            )]))
            .unwrap();
        for (path, id, email) in [
            ("people/a.json", "a", "same@"),
            ("people/b.json", "b", "same@"),
        ] {
            let data = row(json!({"id": id, "email": email, "refs": []}));
            mirror
                .put(Ingest {
                    path,
                    table: "people",
                    schema: &schema,
                    stat: Stat::default(),
                    raw_hash: "h".into(),
                    seen_ns: 0,
                    row: Some(&data),
                    diagnostics: vec![],
                    style: None,
                })
                .unwrap();
        }
        let duplicates = mirror
            .duplicates(
                &["people".into()],
                &constraint_name(&schema, &["email".into()]),
                false,
            )
            .unwrap();
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].holders.len(), 2);
        assert!(
            mirror
                .duplicates(&["people".into()], PRIMARY, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test2072_keys_compare_timestamps_as_instants() {
        let schema = schema();
        let offset = row(json!({"at": "2026-01-01T12:00:00+02:00"}));
        let utc = row(json!({"at": "2026-01-01T10:00:00Z"}));
        assert_eq!(
            key(&offset, &["at".into()], &schema),
            key(&utc, &["at".into()], &schema)
        );
        assert_eq!(
            key(&row(json!({})), &["at".into()], &schema),
            None,
            "null takes no part in a key"
        );
    }

    /// What `row_delta` must equal: every row compared with its record.
    fn delta_by_comparing_everything(
        mirror: &Mirror,
    ) -> std::collections::BTreeMap<String, Option<String>> {
        let rows = |sql: &str| -> std::collections::BTreeMap<String, String> {
            let mut statement = mirror.connection().prepare(sql).unwrap();
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(|row| row.unwrap())
                .collect()
        };
        let current = rows("SELECT path, row_hash FROM _reldir_files WHERE row_hash IS NOT NULL");
        let recorded = rows("SELECT path, hash FROM _reldir_recorded WHERE kind = 'row'");
        let mut delta = std::collections::BTreeMap::new();
        for (path, hash) in &current {
            if recorded.get(path) != Some(hash) {
                delta.insert(path.clone(), Some(hash.clone()));
            }
        }
        for path in recorded.keys() {
            if !current.contains_key(path) {
                delta.insert(path.clone(), None);
            }
        }
        delta
    }

    fn delta_hashes(mirror: &Mirror) -> std::collections::BTreeMap<String, Option<String>> {
        mirror
            .row_changes()
            .unwrap()
            .into_iter()
            .map(|change| (change.path, change.after.map(|(row_hash, _)| row_hash)))
            .collect()
    }

    #[derive(Debug, Clone)]
    enum Step {
        /// Write file `n` holding variant `v` of its row, or a file that is
        /// not a row.
        Put(u8, Option<u8>),
        Remove(u8),
        /// Record what changed, as a revision does.
        Record,
        /// Replace the recorded head with the current rows, as replaying
        /// history onto a rebuilt mirror does.
        ReplaceWithCurrent,
        /// Forget the recorded head.
        ReplaceWithNothing,
        /// Make these changes inside a savepoint, then roll them back.
        Abandoned(Vec<(u8, Option<u8>)>),
    }

    fn step() -> impl proptest::strategy::Strategy<Value = Step> {
        use proptest::prelude::*;
        let put = (0u8..6, proptest::option::of(0u8..3));
        prop_oneof![
            4 => put.clone().prop_map(|(n, v)| Step::Put(n, v)),
            2 => (0u8..6).prop_map(Step::Remove),
            2 => Just(Step::Record),
            1 => Just(Step::ReplaceWithCurrent),
            1 => Just(Step::ReplaceWithNothing),
            1 => proptest::collection::vec(put, 1..4).prop_map(Step::Abandoned),
        ]
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig { cases: 128, .. Default::default() })]

        /// The unsettled paths make the delta cost what changed rather than
        /// what exists; it must still be exactly the delta a comparison of
        /// every row finds, whatever order changes, records and abandoned
        /// prospects arrive in.
        #[test]
        fn test2081_the_delta_from_unsettled_paths_equals_a_full_comparison(
            steps in proptest::collection::vec(step(), 1..24)
        ) {
            let mirror = Mirror::open_memory().unwrap();
            let schema = schema();
            let schemas = std::collections::BTreeMap::from([("people".to_string(), schema.clone())]);
            mirror.sync_schemas(&schemas).unwrap();
            let put = |n: u8, variant: Option<u8>| {
                let path = format!("people/p{n}.json");
                mirror.remove(&path).unwrap();
                let data = variant.map(|v| row(json!({"id": format!("p{n}"), "email": format!("p{n}@"), "refs": vec![v.to_string()]})));
                mirror.put(Ingest {
                    path: &path,
                    table: "people",
                    schema: &schema,
                    stat: Stat::default(),
                    raw_hash: format!("{n}/{variant:?}"),
                    seen_ns: 0,
                    row: data.as_ref(),
                    diagnostics: vec![],
                style: None,
                }).unwrap();
            };
            let mut revision = 0;
            for step in steps {
                match step {
                    Step::Put(n, v) => put(n, v),
                    Step::Remove(n) => mirror.remove(&format!("people/p{n}.json")).unwrap(),
                    Step::Record => {
                        revision += 1;
                        let changes = mirror
                            .row_changes()
                            .unwrap()
                            .into_iter()
                            .map(|change| {
                                (
                                    change.path,
                                    change.after.map(|(hash, _)| crate::history::RecordedEntry {
                                        kind: "row".into(),
                                        hash,
                                    }),
                                )
                            })
                            .collect();
                        let id = crate::store::Id::of(format!("revision {revision}").as_bytes());
                        mirror.apply_recorded(&changes, &id, &id).unwrap();
                    }
                    Step::ReplaceWithCurrent => {
                        let mut entries = std::collections::BTreeMap::new();
                        let mut statement = mirror.connection()
                            .prepare("SELECT path, row_hash FROM _reldir_files WHERE row_hash IS NOT NULL").unwrap();
                        for pair in statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).unwrap() {
                            let (path, hash) = pair.unwrap();
                            entries.insert(path, crate::history::RecordedEntry { kind: "row".into(), hash });
                        }
                        drop(statement);
                        revision += 1;
                        let id = crate::store::Id::of(format!("revision {revision}").as_bytes());
                        mirror.replace_recorded(&entries, Some((&id, &id))).unwrap();
                        proptest::prop_assert!(delta_hashes(&mirror).is_empty(), "the head was just replaced with the current rows");
                    }
                    Step::ReplaceWithNothing => mirror.replace_recorded(&Default::default(), None).unwrap(),
                    Step::Abandoned(puts) => {
                        let before = delta_hashes(&mirror);
                        mirror.savepoint("abandoned").unwrap();
                        for (n, v) in puts {
                            put(n, v);
                        }
                        mirror.rollback_to("abandoned").unwrap();
                        proptest::prop_assert_eq!(delta_hashes(&mirror), before, "an abandoned change leaves no trace");
                    }
                }
                proptest::prop_assert_eq!(delta_hashes(&mirror), delta_by_comparing_everything(&mirror));
            }
        }
    }

    #[test]
    fn test2073_the_stat_cache_distrusts_timestamps_inside_the_racy_window() {
        let entry = Stamp {
            stat: Stat {
                size: 10,
                mtime_ns: 1_000,
                ..Stat::default()
            },
            raw_hash: "h".into(),
            seen_ns: 1_000 + COARSEST_GRANULARITY_NS,
        };
        assert!(
            !entry.trusted_for(&entry.stat, COARSEST_GRANULARITY_NS),
            "modified in the same window it was read"
        );
        let later = Stamp {
            seen_ns: 1_000 + COARSEST_GRANULARITY_NS + 1,
            ..entry.clone()
        };
        assert!(later.trusted_for(&later.stat, COARSEST_GRANULARITY_NS));
        assert!(
            !later.trusted_for(
                &Stat {
                    size: 11,
                    ..later.stat
                },
                COARSEST_GRANULARITY_NS
            ),
            "a changed size is a change"
        );
    }
}
