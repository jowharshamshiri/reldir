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

use crate::{
    canonical,
    diagnostic::{DbError, Diagnostic, Result},
    schema::{ColumnType, Schema},
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, types::Value as SqlValue};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// The layout this binary reads and writes. A mirror of any other layout is
/// rebuilt rather than interpreted.
pub const LAYOUT: &str = "reldir-mirror-1";

/// The constraint name a primary key is stored under in `_reldir_keys`.
pub const PRIMARY: &str = "pk";

/// Where the mirror lives.
pub fn path(root: &Path) -> PathBuf {
    root.join(".db/mirror.sqlite")
}

/// What the filesystem said about a file the last time it was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stat {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub inode: u64,
    pub device: u64,
}

impl Stat {
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        let mtime_ns = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |elapsed| elapsed.as_nanos() as i64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                size: metadata.len(),
                mtime_ns,
                ctime_ns: metadata.ctime() * 1_000_000_000 + metadata.ctime_nsec(),
                inode: metadata.ino(),
                device: metadata.dev(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                size: metadata.len(),
                mtime_ns,
                ctime_ns: 0,
                inode: 0,
                device: 0,
            }
        }
    }
}

/// Timestamps closer than this to the moment a file was read cannot prove the
/// file unchanged: a write in the same clock tick leaves them equal. Two
/// seconds covers the coarsest filesystems in use (FAT's two-second mtime).
const RACY_WINDOW_NS: i64 = 2_000_000_000;

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

impl FileEntry {
    /// Whether the entry still describes a file with this stat, without
    /// reading the file.
    pub fn trusted_for(&self, stat: &Stat) -> bool {
        self.stat == *stat && stat.mtime_ns + RACY_WINDOW_NS < self.seen_ns
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
}

pub struct Mirror {
    conn: Connection,
    /// Whether changes are written to `.db/mirror.sqlite` rather than held in
    /// memory for this process only.
    persistent: bool,
}

fn corrupt(error: impl std::fmt::Display) -> DbError {
    DbError::new(
        "INTERNAL_METADATA_CORRUPT",
        format!("derived mirror: {error}"),
        6,
    )
}

fn register(conn: &Connection) -> Result<()> {
    conn.create_collation("RELDIR_DECIMAL", |left, right| {
        crate::value::compare_decimal(left, right).unwrap_or_else(|| left.cmp(right))
    })
    .map_err(corrupt)
}

const INTERNAL: &str = "
CREATE TABLE IF NOT EXISTS _reldir_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS _reldir_tables (
    tbl TEXT PRIMARY KEY,
    identity TEXT NOT NULL,
    digest TEXT,
    enforced INTEGER NOT NULL DEFAULT 1
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
    trow INTEGER
);
CREATE INDEX IF NOT EXISTS _reldir_files_tbl ON _reldir_files (tbl, trow);
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
";

impl Mirror {
    /// Open the persistent mirror for writing. The caller holds the writer
    /// lock. A mirror that cannot be read, or has another layout, is replaced
    /// by an empty one; the returned flag says so, so the command can report it.
    pub fn open_persistent(root: &Path) -> Result<(Self, bool)> {
        let file = path(root);
        match Self::try_open_file(&file) {
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
        conn.execute_batch(
            "PRAGMA journal_mode=DELETE; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY; \
             PRAGMA foreign_keys=OFF;",
        )
        .map_err(corrupt)?;
        register(&conn)?;
        let mirror = Self {
            conn,
            persistent: true,
        };
        mirror.initialize()?;
        Ok(mirror)
    }

    /// An empty mirror held in memory, for folders that carry no database.
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(corrupt)?;
        conn.execute_batch("PRAGMA temp_store=MEMORY; PRAGMA foreign_keys=OFF;")
            .map_err(corrupt)?;
        register(&conn)?;
        let mirror = Self {
            conn,
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
        let mut conn = Connection::open_in_memory().map_err(corrupt)?;
        let file = path(root);
        if file.is_file()
            && let Ok(source) = Connection::open_with_flags(
                &file,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
        {
            let copied = {
                let backup = rusqlite::backup::Backup::new(&source, &mut conn);
                backup.and_then(|backup| backup.run_to_completion(256, std::time::Duration::ZERO, None))
            };
            if copied.is_err() {
                conn = Connection::open_in_memory().map_err(corrupt)?;
            }
        }
        conn.execute_batch("PRAGMA temp_store=MEMORY; PRAGMA foreign_keys=OFF;")
            .map_err(corrupt)?;
        register(&conn)?;
        let mut mirror = Self {
            conn,
            persistent: false,
        };
        if mirror.initialize().is_err() {
            // A copy of a mirror from another layout is no base to build on.
            let conn = Connection::open_in_memory().map_err(corrupt)?;
            register(&conn)?;
            mirror = Self {
                conn,
                persistent: false,
            };
            mirror.initialize()?;
        }
        Ok(mirror)
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
            .query_row("SELECT value FROM _reldir_meta WHERE key = ?1", [key], |row| row.get(0))
            .optional()
            .map_err(corrupt)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO _reldir_meta (key, value) VALUES (?1, ?2) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map(|_| ())
            .map_err(corrupt)
    }

    /// Bring the typed tables in line with a set of schemas. A table whose
    /// schema changed is emptied and recreated: its rows must be read again,
    /// because what they mean has changed. Returns the tables that were.
    pub fn sync_schemas(
        &self,
        schemas: &std::collections::BTreeMap<String, Schema>,
    ) -> Result<Vec<String>> {
        let known: Vec<(String, String)> = {
            let mut statement = self
                .conn
                .prepare("SELECT tbl, identity FROM _reldir_tables")
                .map_err(corrupt)?;
            let rows = statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(corrupt)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(corrupt)?;
            rows
        };
        let mut rebuilt = vec![];
        for (table, identity) in &known {
            match schemas.get(table) {
                Some(schema) if &signature(schema) == identity => {}
                _ => self.drop_table(table)?,
            }
        }
        for (table, schema) in schemas {
            let current = known
                .iter()
                .any(|(name, identity)| name == table && identity == &signature(schema));
            if !current {
                self.create_table(schema)?;
                rebuilt.push(table.clone());
            }
        }
        Ok(rebuilt)
    }

    fn drop_table(&self, table: &str) -> Result<()> {
        self.conn
            .execute_batch(&format!("DROP TABLE IF EXISTS {}", quote(table)))
            .map_err(corrupt)?;
        for statement in [
            "DELETE FROM _reldir_edges WHERE path IN (SELECT path FROM _reldir_files WHERE tbl = ?1)",
            "DELETE FROM _reldir_keys WHERE tbl = ?1",
            "DELETE FROM _reldir_files WHERE tbl = ?1",
            "DELETE FROM _reldir_tables WHERE tbl = ?1",
        ] {
            self.conn.execute(statement, [table]).map_err(corrupt)?;
        }
        Ok(())
    }

    fn create_table(&self, schema: &Schema) -> Result<()> {
        let table = schema.table();
        let columns: Vec<String> = schema
            .columns()
            .iter()
            .map(|(name, column)| format!("{} {}", quote(name), sqlite_type(column.kind())))
            .collect();
        self.conn
            .execute_batch(&format!(
                "CREATE TABLE {} ({})",
                quote(table),
                columns.join(", ")
            ))
            .map_err(corrupt)?;
        for columns in schema.indexes() {
            self.conn
                .execute_batch(&format!(
                    "CREATE INDEX IF NOT EXISTS {} ON {} ({})",
                    quote(&index_name(table, columns)),
                    quote(table),
                    columns.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", ")
                ))
                .map_err(corrupt)?;
        }
        self.conn
            .execute(
                "INSERT INTO _reldir_tables (tbl, identity, digest, enforced) VALUES (?1, ?2, NULL, 1)",
                params![table, signature(schema)],
            )
            .map_err(corrupt)?;
        self.key_indexes(schema, true)?;
        Ok(())
    }

    /// Index every candidate key of a table: uniquely, so `ON CONFLICT` has a
    /// constraint to name and a duplicate is refused as it is written; or not,
    /// when the observed rows already hold a duplicate the mirror must be able
    /// to describe.
    fn key_indexes(&self, schema: &Schema, unique: bool) -> Result<()> {
        let table = schema.table();
        for columns in schema.candidate_keys() {
            let name = format!("{}_key", index_name(table, columns));
            self.conn
                .execute_batch(&format!(
                    "DROP INDEX IF EXISTS {q}; CREATE {kind}INDEX {q} ON {t} ({c})",
                    q = quote(&name),
                    kind = if unique { "UNIQUE " } else { "" },
                    t = quote(table),
                    c = columns.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", ")
                ))
                .map_err(corrupt)?;
        }
        self.conn
            .execute(
                "UPDATE _reldir_tables SET enforced = ?2 WHERE tbl = ?1",
                params![table, i64::from(unique)],
            )
            .map_err(corrupt)?;
        Ok(())
    }

    /// Make every table's key indexes unique again once its rows allow it.
    pub fn tighten(&self, schemas: &std::collections::BTreeMap<String, Schema>) -> Result<()> {
        let relaxed: Vec<String> = {
            let mut statement = self
                .conn
                .prepare("SELECT tbl FROM _reldir_tables WHERE enforced = 0")
                .map_err(corrupt)?;
            let rows = statement
                .query_map([], |row| row.get(0))
                .map_err(corrupt)?
                .collect::<std::result::Result<Vec<String>, _>>()
                .map_err(corrupt)?;
            rows
        };
        for table in relaxed {
            let Some(schema) = schemas.get(&table) else { continue };
            let mut clean = true;
            for columns in schema.candidate_keys() {
                if !self
                    .duplicates(std::slice::from_ref(&table), &constraint_name(schema, columns), false)?
                    .is_empty()
                {
                    clean = false;
                }
            }
            if clean {
                self.key_indexes(schema, true)?;
            }
        }
        Ok(())
    }

    /// A table's digest over its rows' hashes, cached until a row changes.
    pub fn table_digest(&self, table: &str) -> Result<String> {
        let cached: Option<Option<String>> = self
            .conn
            .query_row("SELECT digest FROM _reldir_tables WHERE tbl = ?1", [table], |row| row.get(0))
            .optional()
            .map_err(corrupt)?;
        if let Some(Some(digest)) = cached {
            return Ok(digest);
        }
        let rows = self.row_hashes(table)?;
        let digest = crate::metadata::table_digest(
            rows.iter()
                .filter_map(|(path, hash)| hash.as_deref().map(|hash| (path.as_str(), hash))),
        );
        self.conn
            .execute("UPDATE _reldir_tables SET digest = ?2 WHERE tbl = ?1", params![table, digest])
            .map_err(corrupt)?;
        Ok(digest)
    }

    fn invalidate(&self, table: &str) -> Result<()> {
        self.conn
            .execute("UPDATE _reldir_tables SET digest = NULL WHERE tbl = ?1", [table])
            .map(|_| ())
            .map_err(corrupt)
    }

    /// Rows whose hash differs from the recorded head, and recorded rows no
    /// longer present.
    pub fn row_delta(&self) -> Result<std::collections::BTreeMap<String, Option<crate::metadata::Entry>>> {
        let mut out = std::collections::BTreeMap::new();
        let mut changed = self
            .conn
            .prepare(
                "SELECT f.path, f.row_hash FROM _reldir_files f LEFT JOIN _reldir_recorded r ON r.path = f.path \
                 WHERE f.row_hash IS NOT NULL AND (r.hash IS NULL OR r.hash <> f.row_hash)",
            )
            .map_err(corrupt)?;
        for row in changed
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(corrupt)?
        {
            let (path, hash) = row.map_err(corrupt)?;
            out.insert(path, Some(crate::metadata::Entry { kind: "row".into(), hash }));
        }
        let mut removed = self
            .conn
            .prepare(
                "SELECT r.path FROM _reldir_recorded r WHERE r.kind = 'row' AND NOT EXISTS \
                 (SELECT 1 FROM _reldir_files f WHERE f.path = r.path AND f.row_hash IS NOT NULL)",
            )
            .map_err(corrupt)?;
        for row in removed.query_map([], |row| row.get::<_, String>(0)).map_err(corrupt)? {
            out.insert(row.map_err(corrupt)?, None);
        }
        Ok(out)
    }

    /// The recorded entries that are not rows.
    pub fn recorded_non_rows(&self) -> Result<std::collections::BTreeMap<String, crate::metadata::Entry>> {
        let mut statement = self
            .conn
            .prepare("SELECT path, kind, hash FROM _reldir_recorded WHERE kind <> 'row'")
            .map_err(corrupt)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    crate::metadata::Entry {
                        kind: row.get(1)?,
                        hash: row.get(2)?,
                    },
                ))
            })
            .map_err(corrupt)?
            .collect::<std::result::Result<std::collections::BTreeMap<_, _>, _>>()
            .map_err(corrupt)?;
        Ok(rows)
    }

    /// Every path the recorded head holds.
    pub fn recorded_paths(&self) -> Result<std::collections::BTreeSet<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT path FROM _reldir_recorded")
            .map_err(corrupt)?;
        let paths = statement
            .query_map([], |row| row.get(0))
            .map_err(corrupt)?
            .collect::<std::result::Result<_, _>>()
            .map_err(corrupt)?;
        Ok(paths)
    }

    /// Replace the recorded head wholesale.
    pub fn replace_recorded(
        &self,
        entries: &std::collections::BTreeMap<String, crate::metadata::Entry>,
        revision: Option<u64>,
        root: Option<&str>,
    ) -> Result<()> {
        self.atomically("reldir_recorded", || self.replace_recorded_inner(entries, revision, root))
    }

    /// Run `work` as one unit: all of it lands, with one flush, or none does.
    fn atomically<T>(&self, name: &str, work: impl FnOnce() -> Result<T>) -> Result<T> {
        self.savepoint(name)?;
        match work() {
            Ok(value) => {
                self.release(name)?;
                Ok(value)
            }
            Err(error) => {
                self.rollback_to(name)?;
                Err(error)
            }
        }
    }

    fn replace_recorded_inner(
        &self,
        entries: &std::collections::BTreeMap<String, crate::metadata::Entry>,
        revision: Option<u64>,
        root: Option<&str>,
    ) -> Result<()> {
        self.conn.execute("DELETE FROM _reldir_recorded", []).map_err(corrupt)?;
        for (path, entry) in entries {
            self.conn
                .execute(
                    "INSERT INTO _reldir_recorded (path, kind, hash) VALUES (?1, ?2, ?3)",
                    params![path, entry.kind, entry.hash],
                )
                .map_err(corrupt)?;
        }
        match (revision, root) {
            (Some(revision), Some(root)) => {
                self.set_meta("recorded_revision", &revision.to_string())?;
                self.set_meta("recorded_root", root)?;
            }
            _ => {
                self.conn
                    .execute(
                        "DELETE FROM _reldir_meta WHERE key IN ('recorded_revision', 'recorded_root')",
                        [],
                    )
                    .map_err(corrupt)?;
            }
        }
        Ok(())
    }

    /// Apply one recorded revision's changes to the recorded head.
    pub fn apply_recorded(
        &self,
        changes: &std::collections::BTreeMap<String, Option<crate::metadata::Entry>>,
        revision: u64,
        root: &str,
    ) -> Result<()> {
        self.atomically("reldir_recorded", || self.apply_recorded_inner(changes, revision, root))
    }

    fn apply_recorded_inner(
        &self,
        changes: &std::collections::BTreeMap<String, Option<crate::metadata::Entry>>,
        revision: u64,
        root: &str,
    ) -> Result<()> {
        for (path, entry) in changes {
            match entry {
                Some(entry) => {
                    self.conn
                        .execute(
                            "INSERT INTO _reldir_recorded (path, kind, hash) VALUES (?1, ?2, ?3) \
                             ON CONFLICT (path) DO UPDATE SET kind = excluded.kind, hash = excluded.hash",
                            params![path, entry.kind, entry.hash],
                        )
                        .map_err(corrupt)?;
                }
                None => {
                    self.conn
                        .execute("DELETE FROM _reldir_recorded WHERE path = ?1", [path])
                        .map_err(corrupt)?;
                }
            }
        }
        self.set_meta("recorded_revision", &revision.to_string())?;
        self.set_meta("recorded_root", root)
    }

    /// The mirror's record of a file, if it has one.
    pub fn file(&self, path: &str) -> Result<Option<FileEntry>> {
        self.conn
            .query_row(
                "SELECT path, tbl, size, mtime_ns, ctime_ns, inode, device, raw_hash, seen_ns, \
                 row_hash, doc, diagnostics FROM _reldir_files WHERE path = ?1",
                [path],
                file_entry,
            )
            .optional()
            .map_err(corrupt)?
            .transpose()
    }

    /// Every path the mirror knows in a table.
    pub fn paths(&self, table: &str) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT path FROM _reldir_files WHERE tbl = ?1 ORDER BY path")
            .map_err(corrupt)?;
        let paths = statement
            .query_map([table], |row| row.get(0))
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<String>, _>>()
            .map_err(corrupt)?;
        Ok(paths)
    }

    /// Visit every file of a table in path order.
    pub fn each_file(&self, table: &str, mut visit: impl FnMut(FileEntry) -> Result<()>) -> Result<()> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT path, tbl, size, mtime_ns, ctime_ns, inode, device, raw_hash, seen_ns, \
                 row_hash, doc, diagnostics FROM _reldir_files WHERE tbl = ?1 ORDER BY path",
            )
            .map_err(corrupt)?;
        let mut rows = statement.query([table]).map_err(corrupt)?;
        while let Some(row) = rows.next().map_err(corrupt)? {
            visit(file_entry(row).map_err(corrupt)??)?;
        }
        Ok(())
    }

    /// Mark an unchanged file as seen now, so it stays out of the racy window.
    pub fn touch(&self, path: &str, stat: &Stat, seen_ns: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE _reldir_files SET size = ?2, mtime_ns = ?3, ctime_ns = ?4, inode = ?5, \
                 device = ?6, seen_ns = ?7 WHERE path = ?1",
                params![
                    path,
                    stat.size as i64,
                    stat.mtime_ns,
                    stat.ctime_ns,
                    stat.inode as i64,
                    stat.device as i64,
                    seen_ns
                ],
            )
            .map(|_| ())
            .map_err(corrupt)
    }

    /// Forget a file.
    pub fn remove(&self, path: &str) -> Result<()> {
        let trow: Option<(String, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT tbl, trow FROM _reldir_files WHERE path = ?1",
                [path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(corrupt)?;
        if let Some((table, rowid)) = &trow {
            if let Some(rowid) = rowid {
                self.conn
                    .execute(&format!("DELETE FROM {} WHERE rowid = ?1", quote(table)), [rowid])
                    .map_err(corrupt)?;
            }
            self.invalidate(table)?;
        }
        for statement in [
            "DELETE FROM _reldir_edges WHERE path = ?1",
            "DELETE FROM _reldir_keys WHERE path = ?1",
            "DELETE FROM _reldir_files WHERE path = ?1",
        ] {
            self.conn.execute(statement, [path]).map_err(corrupt)?;
        }
        Ok(())
    }

    /// Record a file: its stat, its row in the typed table, its keys, and the
    /// references it makes.
    pub fn put(&self, ingest: Ingest<'_>) -> Result<()> {
        self.remove(ingest.path)?;
        let schema = ingest.schema;
        let (doc, row_hash, trow) = match ingest.row {
            Some(row) => {
                let canonical_row = canonical::canonical_row(row, schema);
                let hash = canonical::row_hash(&canonical_row);
                let names: Vec<&String> = schema.columns().keys().collect();
                let values: Vec<SqlValue> = schema
                    .columns()
                    .iter()
                    .map(|(name, column)| {
                        to_sql(
                            row.get(name).or(column.default()).unwrap_or(&Value::Null),
                            column.kind(),
                        )
                    })
                    .collect();
                let insert = format!(
                    "INSERT INTO {} ({}) VALUES ({})",
                    quote(ingest.table),
                    names.iter().map(|n| quote(n)).collect::<Vec<_>>().join(", "),
                    vec!["?"; names.len()].join(", ")
                );
                match self
                    .conn
                    .execute(&insert, rusqlite::params_from_iter(values.clone()))
                {
                    Ok(_) => {}
                    // A duplicate key in the files is a fault to report, not a
                    // row to lose: the key indexes stop being unique until the
                    // duplicate is gone.
                    Err(rusqlite::Error::SqliteFailure(failure, _))
                        if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
                    {
                        self.key_indexes(schema, false)?;
                        self.conn
                            .execute(&insert, rusqlite::params_from_iter(values))
                            .map_err(corrupt)?;
                    }
                    Err(error) => return Err(corrupt(error)),
                }
                let trow = self.conn.last_insert_rowid();
                for (constraint, columns) in constraints(schema) {
                    if let Some(key) = key(row, &columns, schema) {
                        self.conn
                            .execute(
                                "INSERT INTO _reldir_keys (tbl, cons, key, path) VALUES (?1, ?2, ?3, ?4)",
                                params![ingest.table, constraint, key, ingest.path],
                            )
                            .map_err(corrupt)?;
                    }
                }
                for edge in edges(row, schema) {
                    self.conn
                        .execute(
                            "INSERT OR IGNORE INTO _reldir_edges (path, rule, ptr, target) \
                             VALUES (?1, ?2, ?3, ?4)",
                            params![ingest.path, edge.rule, edge.pointer, edge.target],
                        )
                        .map_err(corrupt)?;
                }
                (
                    Some(serde_json::to_string(&canonical_row).map_err(corrupt)?),
                    Some(hash),
                    Some(trow),
                )
            }
            None => (None, None, None),
        };
        self.invalidate(ingest.table)?;
        let diagnostics = serde_json::to_string(&ingest.diagnostics).map_err(corrupt)?;
        self.conn
            .execute(
                "INSERT INTO _reldir_files (path, tbl, size, mtime_ns, ctime_ns, inode, device, \
                 raw_hash, seen_ns, row_hash, doc, diagnostics, trow) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    ingest.path,
                    ingest.table,
                    ingest.stat.size as i64,
                    ingest.stat.mtime_ns,
                    ingest.stat.ctime_ns,
                    ingest.stat.inode as i64,
                    ingest.stat.device as i64,
                    ingest.raw_hash,
                    ingest.seen_ns,
                    row_hash,
                    doc,
                    diagnostics,
                    trow
                ],
            )
            .map_err(corrupt)?;
        Ok(())
    }

    /// The path of the file whose typed row has this rowid.
    pub fn path_of_rowid(&self, table: &str, rowid: i64) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT path FROM _reldir_files WHERE tbl = ?1 AND trow = ?2",
                params![table, rowid],
                |row| row.get(0),
            )
            .optional()
            .map_err(corrupt)
    }

    /// The paths holding a key under a constraint in any of `tables`.
    pub fn holders(&self, key: &str, constraint: &str, tables: &[String]) -> Result<Vec<(String, String)>> {
        let placeholders = (0..tables.len()).map(|i| format!("?{}", i + 3)).collect::<Vec<_>>().join(", ");
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT tbl, path FROM _reldir_keys WHERE key = ?1 AND cons = ?2 AND tbl IN ({placeholders}) \
                 ORDER BY path"
            ))
            .map_err(corrupt)?;
        let mut arguments: Vec<SqlValue> = vec![SqlValue::Text(key.into()), SqlValue::Text(constraint.into())];
        arguments.extend(tables.iter().map(|t| SqlValue::Text(t.clone())));
        let found = statement
            .query_map(rusqlite::params_from_iter(arguments), |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(corrupt)?;
        Ok(found)
    }

    /// The references, under the given rules, that name `target`.
    pub fn referrers(&self, target: &str, rules: &[String]) -> Result<Vec<Edge>> {
        let placeholders = (0..rules.len()).map(|i| format!("?{}", i + 2)).collect::<Vec<_>>().join(", ");
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT path, rule, ptr, target FROM _reldir_edges WHERE target = ?1 AND rule IN ({placeholders}) \
                 ORDER BY path, ptr"
            ))
            .map_err(corrupt)?;
        let mut arguments: Vec<SqlValue> = vec![SqlValue::Text(target.into())];
        arguments.extend(rules.iter().map(|r| SqlValue::Text(r.clone())));
        let found = statement
            .query_map(rusqlite::params_from_iter(arguments), |row| {
                Ok(Edge {
                    path: row.get(0)?,
                    rule: row.get(1)?,
                    pointer: row.get(2)?,
                    target: row.get(3)?,
                })
            })
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(corrupt)?;
        Ok(found)
    }

    /// Every edge recorded under one rule.
    pub fn edges_of(&self, rule: &str) -> Result<Vec<Edge>> {
        let mut statement = self
            .conn
            .prepare("SELECT path, rule, ptr, target FROM _reldir_edges WHERE rule = ?1 ORDER BY path, ptr")
            .map_err(corrupt)?;
        let found = statement
            .query_map([rule], |row| {
                Ok(Edge {
                    path: row.get(0)?,
                    rule: row.get(1)?,
                    pointer: row.get(2)?,
                    target: row.get(3)?,
                })
            })
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(corrupt)?;
        Ok(found)
    }

    /// References under a rule whose target no key in the allowed tables holds.
    pub fn dangling(&self, rule: &str, constraint: &str, tables: &[String]) -> Result<Vec<Edge>> {
        let placeholders = (0..tables.len()).map(|i| format!("?{}", i + 3)).collect::<Vec<_>>().join(", ");
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT e.path, e.rule, e.ptr, e.target FROM _reldir_edges e WHERE e.rule = ?1 AND NOT EXISTS \
                 (SELECT 1 FROM _reldir_keys k WHERE k.key = e.target AND k.cons = ?2 AND k.tbl IN ({placeholders})) \
                 ORDER BY e.path, e.ptr"
            ))
            .map_err(corrupt)?;
        let mut arguments: Vec<SqlValue> = vec![SqlValue::Text(rule.into()), SqlValue::Text(constraint.into())];
        arguments.extend(tables.iter().map(|t| SqlValue::Text(t.clone())));
        let found = statement
            .query_map(rusqlite::params_from_iter(arguments), |row| {
                Ok(Edge {
                    path: row.get(0)?,
                    rule: row.get(1)?,
                    pointer: row.get(2)?,
                    target: row.get(3)?,
                })
            })
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(corrupt)?;
        Ok(found)
    }

    /// Keys held by more than one file under one constraint of one table, or --
    /// across tables -- under the primary keys of several.
    pub fn duplicates(&self, tables: &[String], constraint: &str, across: bool) -> Result<Vec<Duplicate>> {
        let placeholders = (0..tables.len()).map(|i| format!("?{}", i + 2)).collect::<Vec<_>>().join(", ");
        let having = if across { "count(DISTINCT tbl) > 1" } else { "count(*) > 1" };
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT key, group_concat(tbl || char(30) || path, char(31)) FROM \
                 (SELECT key, tbl, path FROM _reldir_keys WHERE cons = ?1 AND tbl IN ({placeholders}) ORDER BY path) \
                 GROUP BY key HAVING {having} ORDER BY key"
            ))
            .map_err(corrupt)?;
        let mut arguments: Vec<SqlValue> = vec![SqlValue::Text(constraint.into())];
        arguments.extend(tables.iter().map(|t| SqlValue::Text(t.clone())));
        let found = statement
            .query_map(rusqlite::params_from_iter(arguments), |row| {
                let key: String = row.get(0)?;
                let holders: String = row.get(1)?;
                Ok(Duplicate {
                    key,
                    holders: holders
                        .split('\u{1f}')
                        .filter_map(|entry| {
                            entry
                                .split_once('\u{1e}')
                                .map(|(table, path)| (table.to_string(), path.to_string()))
                        })
                        .collect(),
                })
            })
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(corrupt)?;
        Ok(found)
    }

    /// Files whose row-local verdict is not clean.
    pub fn files_with_diagnostics(&self) -> Result<Vec<(String, Vec<Diagnostic>)>> {
        let mut statement = self
            .conn
            .prepare("SELECT path, diagnostics FROM _reldir_files WHERE diagnostics <> '[]' ORDER BY path")
            .map_err(corrupt)?;
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(corrupt)?;
        rows.into_iter()
            .map(|(path, text)| {
                let diagnostics = decode_diagnostics(&text)?;
                Ok((path, diagnostics))
            })
            .collect()
    }

    /// The row hashes of every file in a table, in path order, for the state
    /// root.
    pub fn row_hashes(&self, table: &str) -> Result<Vec<(String, Option<String>)>> {
        let mut statement = self
            .conn
            .prepare("SELECT path, row_hash FROM _reldir_files WHERE tbl = ?1 ORDER BY path")
            .map_err(corrupt)?;
        let found = statement
            .query_map([table], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(corrupt)?;
        Ok(found)
    }

    /// Row count of a table.
    pub fn count(&self, table: &str) -> Result<u64> {
        self.conn
            .query_row("SELECT count(*) FROM _reldir_files WHERE tbl = ?1", [table], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| count as u64)
            .map_err(corrupt)
    }

    /// Rebuild SQLite's planner statistics.
    pub fn analyze(&self) -> Result<()> {
        self.conn.execute_batch("ANALYZE").map_err(corrupt)
    }

    /// SQLite's own consistency check of the mirror file.
    pub fn integrity_check(&self) -> Result<Option<String>> {
        let result: String = self
            .conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(corrupt)?;
        Ok((result != "ok").then_some(result))
    }
}

fn decode_diagnostics(text: &str) -> Result<Vec<Diagnostic>> {
    let values: Vec<Value> = serde_json::from_str(text).map_err(corrupt)?;
    values.into_iter().map(|value| Diagnostic::from_json(&value).map_err(corrupt)).collect()
}

fn file_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<FileEntry>> {
    let doc: Option<String> = row.get(10)?;
    let diagnostics: String = row.get(11)?;
    Ok((|| -> Result<FileEntry> {
        Ok(FileEntry {
            path: row.get(0).map_err(corrupt)?,
            table: row.get(1).map_err(corrupt)?,
            stat: Stat {
                size: row.get::<_, i64>(2).map_err(corrupt)? as u64,
                mtime_ns: row.get(3).map_err(corrupt)?,
                ctime_ns: row.get(4).map_err(corrupt)?,
                inode: row.get::<_, i64>(5).map_err(corrupt)? as u64,
                device: row.get::<_, i64>(6).map_err(corrupt)? as u64,
            },
            raw_hash: row.get(7).map_err(corrupt)?,
            seen_ns: row.get(8).map_err(corrupt)?,
            row_hash: row.get(9).map_err(corrupt)?,
            doc: doc
                .map(|text| {
                    serde_json::from_str::<Value>(&text)
                        .map_err(corrupt)
                        .and_then(|value| match value {
                            Value::Object(map) => Ok(map),
                            _ => Err(corrupt("a mirrored row is not an object")),
                        })
                })
                .transpose()?,
            diagnostics: decode_diagnostics(&diagnostics)?,
        })
    })())
}

/// A reference recorded in the mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    /// The file holding the reference.
    pub path: String,
    /// `fk:<table>/<name>` or `acyclic:<table>/<name>`.
    pub rule: String,
    /// Where in the file the referencing value sits.
    pub pointer: String,
    /// The key it names, rendered as keys are.
    pub target: String,
}

/// Files that share a key.
#[derive(Debug, Clone)]
pub struct Duplicate {
    pub key: String,
    /// (table, path) of each holder, in path order.
    pub holders: Vec<(String, String)>,
}

/// What the mirror's copy of a table depends on: the rules its rows are
/// judged by, and the order its columns are written in -- which identity
/// leaves out, because it changes how a row is written, not whether it is
/// valid.
fn signature(schema: &Schema) -> String {
    let columns: Vec<&str> = schema.columns().keys().map(String::as_str).collect();
    format!("{}:{}", schema.identity(), canonical::hash_bytes(columns.join("\u{1f}").as_bytes()))
}

/// The rule name a foreign key's edges are stored under.
pub fn fk_rule(table: &str, name: &str) -> String {
    format!("fk:{table}/{name}")
}

/// The rule name an acyclic graph's edges are stored under.
pub fn acyclic_rule(table: &str, name: &str) -> String {
    format!("acyclic:{table}/{name}")
}

/// The constraint name a column list is stored under in `_reldir_keys`.
pub fn constraint_name(schema: &Schema, columns: &[String]) -> String {
    if columns == schema.primary_key() {
        PRIMARY.into()
    } else {
        format!("unique:{}", columns.join(","))
    }
}

fn constraints(schema: &Schema) -> Vec<(String, Vec<String>)> {
    schema
        .candidate_keys()
        .map(|columns| (constraint_name(schema, columns), columns.to_vec()))
        .collect()
}

/// A key's canonical rendering: the canonical JSON array of its column values,
/// each written the way its type is compared -- a timestamp in UTC, so the same
/// instant is the same key whatever offset it was written in. Absent values
/// read as their default; a null anywhere means the row takes part in no key.
pub fn key(row: &Map<String, Value>, columns: &[String], schema: &Schema) -> Option<String> {
    let mut values = Vec::with_capacity(columns.len());
    for name in columns {
        let column = schema.column(name)?;
        let value = row.get(name).or(column.default()).cloned().unwrap_or(Value::Null);
        if value.is_null() {
            return None;
        }
        values.push(key_component(&value, column.kind()));
    }
    Some(canonical::compact(&Value::Array(values)))
}

/// One value as it takes part in a key.
pub fn key_component(value: &Value, kind: &ColumnType) -> Value {
    match kind {
        ColumnType::Timestamp => crate::value::textual(value, kind)
            .map(Value::String)
            .unwrap_or_else(|| value.clone()),
        _ => value.clone(),
    }
}

/// A reference found in a row.
pub struct RowEdge {
    pub rule: String,
    pub pointer: String,
    pub target: String,
}

/// Every reference and acyclic edge a row makes.
pub fn edges(row: &Map<String, Value>, schema: &Schema) -> Vec<RowEdge> {
    let mut out = vec![];
    for fk in schema.foreign_keys() {
        let rule = fk_rule(schema.table(), fk.name());
        if fk.from().len() == 1 {
            let path = &fk.from()[0];
            let kind = crate::schema::document::resolve_path(schema, path)
                .map(|leaf| leaf.column.kind().clone())
                .unwrap_or(ColumnType::Json);
            for occurrence in path.occurrences(row) {
                out.push(RowEdge {
                    rule: rule.clone(),
                    pointer: occurrence.pointer,
                    target: canonical::compact(&Value::Array(vec![key_component(occurrence.value, &kind)])),
                });
            }
        } else {
            // A composite key: one value per path, none of them iterating.
            let mut values = vec![];
            let mut pointers = vec![];
            for path in fk.from() {
                let kind = crate::schema::document::resolve_path(schema, path)
                    .map(|leaf| leaf.column.kind().clone())
                    .unwrap_or(ColumnType::Json);
                match path.occurrences(row).into_iter().next() {
                    Some(occurrence) => {
                        values.push(key_component(occurrence.value, &kind));
                        pointers.push(occurrence.pointer);
                    }
                    None => {
                        values.clear();
                        break;
                    }
                }
            }
            if values.len() == fk.from().len() {
                out.push(RowEdge {
                    rule,
                    pointer: pointers[0].clone(),
                    target: canonical::compact(&Value::Array(values)),
                });
            }
        }
    }
    for graph in schema.acyclic() {
        let rule = acyclic_rule(schema.table(), graph.name());
        let kind = schema
            .column(&schema.primary_key()[0])
            .map(|c| c.kind().clone())
            .unwrap_or(ColumnType::Json);
        for path in graph.edges() {
            for occurrence in path.occurrences(row) {
                out.push(RowEdge {
                    rule: rule.clone(),
                    pointer: occurrence.pointer,
                    target: canonical::compact(&Value::Array(vec![key_component(occurrence.value, &kind)])),
                });
            }
        }
    }
    out
}

/// The SQLite declared type for a column type.
///
/// Every declared type has BLOB affinity, so SQLite stores exactly the storage
/// class it is given and never coerces a value on the way in; the declared
/// name tells [`decode_declared`] how to read a result column back.
pub fn sqlite_type(kind: &ColumnType) -> &'static str {
    match kind {
        ColumnType::Bool => "RELDIR_BLOB_BOOL",
        ColumnType::Int => "RELDIR_BLOB_I64",
        ColumnType::Float => "RELDIR_BLOB_F64",
        ColumnType::Decimal => "RELDIR_BLOB_DECIMAL COLLATE RELDIR_DECIMAL",
        ColumnType::String => "RELDIR_BLOB_STRING",
        ColumnType::Bytes => "RELDIR_BLOB_BYTES",
        ColumnType::Date => "RELDIR_BLOB_DATE",
        ColumnType::Timestamp => "RELDIR_BLOB_TIMESTAMP",
        ColumnType::Uuid => "RELDIR_BLOB_UUID",
        ColumnType::Ulid => "RELDIR_BLOB_ULID",
        ColumnType::Enum => "RELDIR_BLOB_ENUM",
        ColumnType::Array => "RELDIR_BLOB_ARRAY",
        ColumnType::Object => "RELDIR_BLOB_OBJECT",
        ColumnType::Json => "RELDIR_BLOB_JSON",
    }
}

/// A JSON value as SQLite stores it for a column type.
pub fn to_sql(value: &Value, kind: &ColumnType) -> SqlValue {
    match (kind, value) {
        (_, Value::Null) => SqlValue::Null,
        (ColumnType::Bool, Value::Bool(flag)) => SqlValue::Integer(i64::from(*flag)),
        (ColumnType::Int, Value::Number(number)) if number.as_i64().is_some() && !number.is_f64() => {
            SqlValue::Integer(number.as_i64().unwrap_or_default())
        }
        (ColumnType::Float, Value::Number(number)) => {
            number.as_f64().map(SqlValue::Real).unwrap_or(SqlValue::Null)
        }
        (ColumnType::Array | ColumnType::Object | ColumnType::Json, _) => {
            SqlValue::Text(canonical::compact(value))
        }
        (ColumnType::Timestamp, Value::String(_)) => crate::value::textual(value, kind)
            .map(SqlValue::Text)
            .unwrap_or_else(|| to_sql_generic(value)),
        _ => to_sql_generic(value),
    }
}

/// A JSON value as SQLite stores it when no column type applies, as for a
/// bound parameter.
pub fn to_sql_generic(value: &Value) -> SqlValue {
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(flag) => SqlValue::Integer(i64::from(*flag)),
        Value::Number(number) => number
            .as_i64()
            .filter(|_| !number.is_f64())
            .map(SqlValue::Integer)
            .or_else(|| number.as_f64().map(SqlValue::Real))
            .unwrap_or(SqlValue::Null),
        Value::String(text) => SqlValue::Text(text.clone()),
        other => SqlValue::Text(canonical::compact(other)),
    }
}

/// A SQLite value read back as JSON.
pub fn from_sql(value: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(integer) => Value::Number(integer.into()),
        ValueRef::Real(real) => serde_json::Number::from_f64(real).map(Value::Number).unwrap_or(Value::Null),
        ValueRef::Text(text) => Value::String(String::from_utf8_lossy(text).into()),
        ValueRef::Blob(bytes) => {
            Value::String(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes))
        }
    }
}

/// A value read from a column of a known type, decoded to the JSON that type
/// is written as.
pub fn decode_typed(value: Value, kind: &ColumnType) -> Result<Value> {
    decode_declared(value, Some(sqlite_type(kind).split_whitespace().next().unwrap_or("")))
}

/// A result value decoded by the declared type of its source column, when it
/// has one. Expressions have none and are returned as SQLite produced them.
pub fn decode_declared(value: Value, declared: Option<&str>) -> Result<Value> {
    if value.is_null() {
        return Ok(value);
    }
    match declared {
        Some("RELDIR_BLOB_BOOL") => match value.as_i64() {
            Some(0) => Ok(Value::Bool(false)),
            Some(1) => Ok(Value::Bool(true)),
            _ => Err(DbError::new(
                "QUERY_TYPE_ERROR",
                format!("a bool column produced {value}, which is neither true nor false"),
                4,
            )),
        },
        Some(kind @ ("RELDIR_BLOB_ARRAY" | "RELDIR_BLOB_OBJECT" | "RELDIR_BLOB_JSON")) => {
            let text = value.as_str().ok_or_else(|| {
                DbError::new(
                    "QUERY_TYPE_ERROR",
                    format!("a {} column produced non-text data", kind.trim_start_matches("RELDIR_BLOB_").to_lowercase()),
                    4,
                )
            })?;
            let decoded = crate::json::parse_str(text).map_err(|error| {
                DbError::new(
                    "QUERY_TYPE_ERROR",
                    format!("a structured column produced invalid JSON: {error}"),
                    4,
                )
            })?;
            let shaped = match kind {
                "RELDIR_BLOB_ARRAY" => decoded.is_array(),
                "RELDIR_BLOB_OBJECT" => decoded.is_object(),
                _ => true,
            };
            if shaped {
                Ok(decoded)
            } else {
                Err(DbError::new(
                    "QUERY_TYPE_ERROR",
                    format!("a {} column produced the wrong JSON shape", kind.trim_start_matches("RELDIR_BLOB_").to_lowercase()),
                    4,
                ))
            }
        }
        _ => Ok(value),
    }
}

/// A quoted SQL identifier.
pub fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// The name of the index over a column list, stable across processes.
pub fn index_name(table: &str, columns: &[String]) -> String {
    let mut identity = String::new();
    for column in columns {
        identity.push_str(&column.len().to_string());
        identity.push(':');
        identity.push_str(column);
        identity.push(';');
    }
    format!("reldir_{table}_{}", &canonical::hash_bytes(identity.as_bytes())[..12])
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
                    "primaryKey": ["id"],
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
    fn test2070_ingest_records_keys_edges_and_typed_rows() {
        let mirror = Mirror::open_memory().unwrap();
        let schema = schema();
        let schemas = std::collections::BTreeMap::from([("people".to_string(), schema.clone())]);
        assert_eq!(mirror.sync_schemas(&schemas).unwrap(), vec!["people".to_string()]);
        for (id, email, refs) in [("a", "x@", json!(["b"])), ("b", "y@", json!(["a", "ghost"]))] {
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
                })
                .unwrap();
        }
        let dangling = mirror
            .dangling(&fk_rule("people", "fk_refs"), PRIMARY, &["people".to_string()])
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
            .query_row("SELECT count(*) FROM people WHERE email LIKE '%@'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2, "typed rows are queryable");

        // Removing a file removes everything it contributed: its dangling
        // reference goes with it, and the reference to it now dangles.
        mirror.remove("people/b.json").unwrap();
        let dangling = mirror.dangling(&fk_rule("people", "fk_refs"), PRIMARY, &["people".into()]).unwrap();
        assert_eq!(dangling.len(), 1);
        assert_eq!((dangling[0].path.as_str(), dangling[0].target.as_str()), ("people/a.json", "[\"b\"]"));
        assert_eq!(mirror.count("people").unwrap(), 1);
    }

    #[test]
    fn test2071_duplicates_are_found_within_and_across_tables() {
        let mirror = Mirror::open_memory().unwrap();
        let schema = schema();
        mirror
            .sync_schemas(&std::collections::BTreeMap::from([("people".to_string(), schema.clone())]))
            .unwrap();
        for (path, id, email) in [("people/a.json", "a", "same@"), ("people/b.json", "b", "same@")] {
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
                })
                .unwrap();
        }
        let duplicates = mirror
            .duplicates(&["people".into()], &constraint_name(&schema, &["email".into()]), false)
            .unwrap();
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].holders.len(), 2);
        assert!(mirror.duplicates(&["people".into()], PRIMARY, false).unwrap().is_empty());
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
        assert_eq!(key(&row(json!({})), &["at".into()], &schema), None, "null takes no part in a key");
    }

    #[test]
    fn test2073_the_stat_cache_distrusts_timestamps_inside_the_racy_window() {
        let entry = FileEntry {
            path: "t/a.json".into(),
            table: "t".into(),
            stat: Stat { size: 10, mtime_ns: 1_000, ..Stat::default() },
            raw_hash: "h".into(),
            seen_ns: 1_000 + RACY_WINDOW_NS,
            row_hash: None,
            doc: None,
            diagnostics: vec![],
        };
        assert!(!entry.trusted_for(&entry.stat), "modified in the same window it was read");
        let later = FileEntry { seen_ns: 1_000 + RACY_WINDOW_NS + 1, ..entry.clone() };
        assert!(later.trusted_for(&later.stat));
        assert!(!later.trusted_for(&Stat { size: 11, ..later.stat }), "a changed size is a change");
    }
}
