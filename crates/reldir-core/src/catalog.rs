//! Observing the governed directory.
//!
//! A catalog is what the binary knows about the folder at one moment: which
//! tables exist and what their schemas say, which files hold their rows, and
//! everything wrong with any of it that can be judged file by file. Rows are
//! not held in memory: they live in the [`Mirror`], which this module keeps in
//! step with the files -- re-reading only files whose size, times or inode
//! moved, and re-validating only what it re-read.
//!
//! Observation reads through a [`Source`], so the same code observes the disk
//! and the disk-with-a-planned-change-laid-over-it that a mutation is judged
//! against before anything is written.
//!
//! Every document read here is untrusted input and is parsed under the
//! configured nesting and size bounds.

use crate::code::Code;
use crate::{
    canonical,
    config::Config,
    diagnostic::{DbError, Diagnostic, Result},
    engine::fs::{Entry, Kind, Source},
    mirror::{self, Ingest, Mirror},
    schema::{Schema, SchemaFileKind},
};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    rc::Rc,
};
use unicode_normalization::UnicodeNormalization;

/// A row, as the mirror holds it.
#[derive(Debug, Clone)]
pub struct Row {
    pub table: String,
    /// Path relative to the database root, with `/` separators.
    pub relative: PathBuf,
    /// The row's members, in schema column order.
    pub value: Map<String, Value>,
}

impl Row {
    /// The file's bytes, read now, for diagnostics that point into them and for
    /// changes that must preserve them.
    pub fn raw(&self, root: &Path) -> Result<Vec<u8>> {
        let path = root.join(&self.relative);
        std::fs::read(&path).map_err(|error| DbError::io(&path, error))
    }
}

/// Where a table's schema came from.
#[derive(Debug, Clone)]
pub struct SchemaFile {
    pub kind: SchemaFileKind,
    /// Relative to the database root.
    pub relative: PathBuf,
    pub bytes: Vec<u8>,
}

/// What to tell someone holding a directory of rows the database does not
/// govern. One text, so the `UNGOVERNED_DIRECTORY` warning and the
/// `UNKNOWN_TABLE` error can never advise two different things.
fn ungoverned_help(name: &str) -> String {
    format!("run `reldir infer {name} --write`, or add it to the ignore list in .db/config")
}

const PROSPECT: &str = "reldir_prospect";

/// The most row files a change may touch and still be judged by what it
/// changes; a larger one is judged whole, once, which then costs less than
/// judging its scope twice.
const INCREMENTAL_LIMIT: usize = 4096;

/// The `_reldir_meta` key of the folder monitor's mark the mirror is current
/// with.
const MONITOR: &str = "monitor";

/// The `_reldir_meta` key under which each observation is kept.
const OBSERVATION: &str = "observation";

/// The `_reldir_meta` key of the filesystem's measured timestamp granularity.
const GRANULARITY: &str = "mtime_granularity_ns";

/// The `_reldir_meta` keys of the last verdict, and of what it judged.
const VERDICT: &str = "verdict";
const VERDICT_KEY: &str = "verdict_of";

/// A planned change, observed and judged, whose mirror changes are held until
/// it is committed or abandoned. It must be resolved one way or the other:
/// until then the mirror is inside the prospect's savepoint.
#[must_use = "a prospect holds the mirror in a savepoint until it is kept or abandoned"]
pub struct Prospect {
    pub catalog: Catalog,
    pub verdict: crate::integrity::Verdict,
}

impl Prospect {
    /// The change was committed: the observation is the new state.
    pub fn keep(self) -> Result<(Catalog, crate::integrity::Verdict)> {
        self.catalog.mirror.release(PROSPECT)?;
        Ok((self.catalog, self.verdict))
    }

    /// The change was not made: return the mirror to the state before it.
    pub fn abandon(self) -> Result<crate::integrity::Verdict> {
        self.catalog.mirror.rollback_to(PROSPECT)?;
        Ok(self.verdict)
    }
}

#[derive(Clone)]
pub struct Catalog {
    pub root: PathBuf,
    /// Top-level directories holding data that no schema governs.
    pub ungoverned: Vec<String>,
    /// Tables whose schema is a pin in `schema/`.
    pub pinned: BTreeSet<String>,
    /// Every schema that could be read.
    pub schemas: BTreeMap<String, Schema>,
    /// Where each table's schema lives, including those that could not be
    /// read -- a fault in a schema is still a fault at a path.
    pub schema_files: BTreeMap<String, SchemaFile>,
    /// Working schemas left behind by a table that now has a pin. The pin
    /// governs; these are derived copies to discard.
    pub superseded_working: Vec<String>,
    /// Faults found while observing: structure, schemas, and cross-schema
    /// rules. [`crate::integrity::validate`] adds row and set-level faults.
    pub diagnostics: Vec<Diagnostic>,
    pub warnings: Vec<Diagnostic>,
    pub mirror: Rc<Mirror>,
    /// Paths read afresh in this observation, in path order.
    pub reread: Vec<String>,
    /// Paths the mirror knew and this observation found gone.
    pub forgotten: Vec<String>,
    /// The observation examined only what the folder monitor reported
    /// changed, rather than every file.
    pub monitored: bool,
    /// Tables whose rows were all re-read because their schema changed.
    pub rebuilt_tables: Vec<String>,
    /// Whether the persistent mirror had to be discarded and rebuilt.
    pub rebuilt_mirror: bool,
}

impl Catalog {
    /// Observe the governed directory against the schemas it declares.
    pub fn observe(
        root: &Path,
        config: &Config,
        source: &dyn Source,
        mirror: Rc<Mirror>,
        rebuilt_mirror: bool,
    ) -> Result<Self> {
        crate::json::with_depth_limit(config.max_nesting_depth, || {
            let mut catalog = Self::empty(root, config, mirror, rebuilt_mirror);
            catalog.load_schemas(config, source)?;
            crate::integrity::validate_schemas(&mut catalog);
            catalog.refresh(config, source)?;
            Ok(catalog)
        })
    }

    /// Observe a folder that carries no database: its pins are read as usual,
    /// and tables nobody pinned are governed by schemas inferred in memory and
    /// never written. Rows are read and judged exactly as for a governed
    /// database, so there is one interpretation of validity regardless of where
    /// the schemas came from.
    pub fn observe_with_schemas(
        root: &Path,
        config: &Config,
        source: &dyn Source,
        inferred: BTreeMap<String, Schema>,
    ) -> Result<Self> {
        crate::json::with_depth_limit(config.max_nesting_depth, || {
            let mut catalog = Self::empty(root, config, Rc::new(Mirror::open_memory()?), false);
            catalog.load_schemas(config, source)?;
            for (table, schema) in inferred {
                if !catalog.schema_files.contains_key(&table) {
                    catalog.schemas.insert(table, schema);
                }
            }
            crate::integrity::validate_schemas(&mut catalog);
            catalog.refresh(config, source)?;
            Ok(catalog)
        })
    }

    /// Observe the directory as a planned change would leave it, and judge it.
    /// Nothing is written: the planned bytes are read from memory, and the
    /// mirror's changes are held in a savepoint until the caller keeps them --
    /// the change was committed, so the observation is the new state and need
    /// not be made again -- or abandons them.
    pub fn prospect(&self, config: &Config, source: &dyn Source) -> Result<Prospect> {
        self.mirror.savepoint(PROSPECT)?;
        let outcome = crate::json::with_depth_limit(config.max_nesting_depth, || {
            let mut future = Self::empty(&self.root, config, Rc::clone(&self.mirror), false);
            future.load_schemas(config, source)?;
            crate::integrity::validate_schemas(&mut future);
            future.refresh(config, source)?;
            let verdict = crate::integrity::validate_through(&future, source)?;
            Ok((future, verdict))
        });
        match outcome {
            Ok((catalog, verdict)) => Ok(Prospect { catalog, verdict }),
            Err(error) => {
                self.mirror.rollback_to(PROSPECT)?;
                Err(error)
            }
        }
    }

    /// Whether a path is a row file of a table whose schema loads: what the
    /// mirror can take in on its own, without observing the folder again.
    pub(crate) fn row_file(&self, path: &Path) -> Option<(String, String)> {
        let mut parts = path.components();
        let table = parts.next()?.as_os_str().to_str()?.to_string();
        let name = parts.next()?.as_os_str().to_str()?.to_string();
        (parts.next().is_none()
            && name.ends_with(crate::naming::EXTENSION)
            && self.schemas.contains_key(&table)
            && !self.blocked(&table))
        .then_some((table, name))
    }

    /// Take planned row files into the mirror without observing the folder
    /// again -- the observation this catalog holds is current, its writer
    /// having held the lock since making it -- and return the catalog of the
    /// state they make. `None` when the change touches anything but rows of
    /// tables whose schemas load. The caller holds a savepoint around it.
    pub fn ingest_rows(
        &self,
        config: &Config,
        writes: &BTreeMap<PathBuf, Vec<u8>>,
        deletes: &BTreeSet<PathBuf>,
    ) -> Result<Option<Self>> {
        if writes
            .keys()
            .chain(deletes)
            .any(|path| self.row_file(path).is_none())
        {
            return Ok(None);
        }
        let touched: BTreeSet<&PathBuf> = writes.keys().chain(deletes).collect();
        let untouched = |diagnostic: &Diagnostic| {
            diagnostic
                .path
                .as_ref()
                .is_none_or(|path| !touched.contains(path))
        };
        crate::json::with_depth_limit(config.max_nesting_depth, || {
            let mut future = Self {
                root: self.root.clone(),
                ungoverned: self.ungoverned.clone(),
                pinned: self.pinned.clone(),
                schemas: self.schemas.clone(),
                schema_files: self.schema_files.clone(),
                superseded_working: self.superseded_working.clone(),
                diagnostics: self
                    .diagnostics
                    .iter()
                    .filter(|d| untouched(d))
                    .cloned()
                    .collect(),
                warnings: self
                    .warnings
                    .iter()
                    .filter(|d| untouched(d))
                    .cloned()
                    .collect(),
                mirror: Rc::clone(&self.mirror),
                reread: writes.keys().map(|path| slash(path)).collect(),
                forgotten: deletes.iter().map(|path| slash(path)).collect(),
                monitored: self.monitored,
                rebuilt_tables: vec![],
                rebuilt_mirror: false,
            };
            for path in deletes {
                self.mirror.remove(&slash(path))?;
            }
            let seen_ns = now_ns();
            for (path, bytes) in writes {
                let (table, name) = self.row_file(path).expect("checked above");
                let key = slash(path);
                let schema = &self.schemas[&table];
                let created = self.mirror.file(&key)?.is_none();
                let (row, diagnostics) = judge_row(&table, schema, &name, bytes, config);
                self.mirror.put(Ingest {
                    path: &key,
                    table: &table,
                    schema,
                    // Not yet on disk: the next observation re-reads it once
                    // and finds the bytes it expects. Their length is known,
                    // and is what a later change checks the file against.
                    stat: crate::engine::fs::Stat {
                        size: bytes.len() as u64,
                        ..crate::engine::fs::Stat::default()
                    },
                    raw_hash: canonical::hash_bytes(bytes),
                    seen_ns,
                    row: row.as_ref(),
                    diagnostics,
                    style: Some(style_of(bytes)),
                })?;
                if created {
                    let collides = self
                        .mirror
                        .same_folded(&table, &fold(&name), &key)?
                        .into_iter()
                        .any(|other| !touched.contains(&PathBuf::from(&other)));
                    if collides {
                        let message = format!(
                            "{name:?} would share its name with a file of {table} on a \
                             filesystem that ignores case or normalizes Unicode"
                        );
                        let collision = if config.portable_names {
                            Diagnostic::error(Code::PORTABILITY_COLLISION, message)
                        } else {
                            Diagnostic::warning(Code::PORTABILITY_COLLISION, message)
                        }
                        .at(path.clone())
                        .table(table.as_str());
                        if config.portable_names {
                            future.diagnostics.push(collision);
                        } else {
                            future.warnings.push(collision);
                        }
                    }
                }
            }
            let governed: BTreeMap<String, Schema> = self
                .schemas
                .iter()
                .filter(|(table, _)| !self.blocked(table))
                .map(|(table, schema)| (table.clone(), schema.clone()))
                .collect();
            self.mirror.tighten(&governed)?;
            self.mirror
                .set_meta(OBSERVATION, &future.observation().to_string())?;
            Ok(Some(future))
        })
    }

    /// Judge a change to rows alone without observing the folder again: the
    /// changed paths are read from the planned bytes into a savepoint of the
    /// mirror, and the state they make is judged. `None` when the change
    /// touches anything but rows of tables whose schemas load -- a schema, a
    /// setting -- which is judged by observing everything again.
    ///
    /// With `previous` -- the verdict of this catalog's state -- a change to
    /// few enough rows is judged by what it can change: the findings about
    /// its paths and the keys they hold, before and after, replace those in
    /// `previous`, and the rest stand. Its cost follows the change, not the
    /// database.
    pub fn prospect_rows(
        &self,
        config: &Config,
        writes: &BTreeMap<PathBuf, Vec<u8>>,
        deletes: &BTreeSet<PathBuf>,
        source: &dyn Source,
        previous: Option<&crate::integrity::Verdict>,
    ) -> Result<Option<Prospect>> {
        if writes
            .keys()
            .chain(deletes)
            .any(|path| self.row_file(path).is_none())
        {
            return Ok(None);
        }
        let depth = config.max_nesting_depth;
        let incremental = match previous {
            Some(previous) if writes.len() + deletes.len() <= INCREMENTAL_LIMIT => {
                let scope = self.scope_of(config, writes, deletes)?;
                let before = crate::json::with_depth_limit(depth, || {
                    crate::integrity::judge(self, &crate::engine::fs::Disk, Some(&scope))
                })?;
                Some((previous, scope, before))
            }
            _ => None,
        };
        self.mirror.savepoint(PROSPECT)?;
        let outcome = (|| -> Result<(Catalog, crate::integrity::Verdict)> {
            let future = self
                .ingest_rows(config, writes, deletes)?
                .expect("checked above");
            let whole = || {
                crate::json::with_depth_limit(depth, || {
                    crate::integrity::validate_through(&future, source)
                })
            };
            let verdict = match &incremental {
                Some((previous, scope, before)) => {
                    let after = crate::json::with_depth_limit(depth, || {
                        crate::integrity::judge(&future, source, Some(scope))
                    })?;
                    match crate::integrity::revalidate(previous, before, &after) {
                        Some(verdict) => verdict,
                        None => {
                            // The verdict this change started from is not
                            // this state's: judging it whole is what is
                            // left to trust. A test that reaches here has
                            // found a defect in the scoping.
                            debug_assert!(
                                false,
                                "an incremental judgement disagreed with the verdict it began from"
                            );
                            whole()?
                        }
                    }
                }
                None => whole()?,
            };
            Ok((future, verdict))
        })();
        match outcome {
            Ok((catalog, verdict)) => Ok(Some(Prospect { catalog, verdict })),
            Err(error) => {
                self.mirror.rollback_to(PROSPECT)?;
                Err(error)
            }
        }
    }

    /// What a change to rows can change of a verdict: its paths and tables,
    /// and -- given to the mirror -- the keys those paths hold now and will
    /// hold after it.
    fn scope_of(
        &self,
        config: &Config,
        writes: &BTreeMap<PathBuf, Vec<u8>>,
        deletes: &BTreeSet<PathBuf>,
    ) -> Result<crate::integrity::Scope> {
        let mut scope = crate::integrity::Scope::default();
        for path in writes.keys().chain(deletes) {
            let (table, _) = self.row_file(path).expect("checked by the caller");
            scope.tables.insert(table);
            scope.paths.insert(slash(path));
        }
        let mut keys = self.mirror.keys_held(&scope.paths)?;
        for (path, bytes) in writes {
            let (table, name) = self.row_file(path).expect("checked by the caller");
            let schema = &self.schemas[&table];
            if let (Some(row), _) = judge_row(&table, schema, &name, bytes, config) {
                keys.extend(mirror::row_keys(&row, schema));
            }
        }
        self.mirror.set_scope(&scope.paths, &keys)?;
        Ok(scope)
    }

    fn empty(root: &Path, _config: &Config, mirror: Rc<Mirror>, rebuilt_mirror: bool) -> Self {
        Self {
            root: root.to_path_buf(),
            ungoverned: vec![],
            pinned: BTreeSet::new(),
            schemas: BTreeMap::new(),
            schema_files: BTreeMap::new(),
            superseded_working: vec![],
            diagnostics: vec![],
            warnings: vec![],
            mirror,
            reread: vec![],
            monitored: false,
            forgotten: vec![],
            rebuilt_tables: vec![],
            rebuilt_mirror,
        }
    }

    /// The `UNKNOWN_TABLE` error for a name this catalog does not govern.
    pub fn unknown_table(&self, table: &str) -> DbError {
        let diagnostic = Diagnostic::error(Code::UNKNOWN_TABLE, format!("unknown table {table:?}"));
        let diagnostic = if self.ungoverned.iter().any(|name| name == table) {
            diagnostic.at(table).help(ungoverned_help(table))
        } else {
            let nearest = self
                .schemas
                .keys()
                .min_by_key(|name| strsim::levenshtein(name, table))
                .filter(|name| strsim::levenshtein(name, table) <= 2);
            match nearest {
                Some(name) => diagnostic.help(format!("did you mean {name:?}?")),
                None => diagnostic,
            }
        };
        DbError::from_diag(diagnostic)
    }

    /// Read every schema file. A table's schema is its pin when it has one,
    /// else its working schema; a table with both is governed by the pin, and
    /// the working copy is recorded as superseded derived state.
    fn load_schemas(&mut self, config: &Config, source: &dyn Source) -> Result<()> {
        let mut candidates: BTreeMap<String, (SchemaFileKind, PathBuf)> = BTreeMap::new();
        for (kind, directory) in [
            (
                SchemaFileKind::Working,
                crate::schema_store::working_dir(&self.root),
            ),
            (
                SchemaFileKind::Pin,
                crate::schema_store::pin_dir(&self.root),
            ),
        ] {
            let relative_directory = relative(&self.root, &directory);
            match source.metadata(&directory) {
                Ok(meta) if meta.kind == Kind::Dir => {}
                Ok(_) => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            Code::NON_REGULAR_FILE,
                            format!("{} must be a real directory", relative_directory.display()),
                        )
                        .at(relative_directory),
                    );
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(DbError::io(&directory, error)),
            }
            let mut seen = BTreeSet::new();
            for Entry { path, meta } in listing(source, &directory)? {
                let relative_path = relative(&self.root, &path);
                if meta.kind != Kind::File || meta.links > 1 {
                    self.diagnostics.push(
                        Diagnostic::error(
                            Code::NON_REGULAR_FILE,
                            "schema entries must be private regular files",
                        )
                        .at(relative_path),
                    );
                    continue;
                }
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    self.diagnostics.push(
                        Diagnostic::error(
                            Code::UNEXPECTED_FILE,
                            "a schema directory holds only .json files",
                        )
                        .at(relative_path),
                    );
                    continue;
                }
                if meta.stat.size > config.max_json_file_size {
                    self.diagnostics.push(
                        Diagnostic::error(
                            Code::FILE_TOO_LARGE,
                            format!(
                                "schema exceeds the {} byte file limit",
                                config.max_json_file_size
                            ),
                        )
                        .at(relative_path),
                    );
                    continue;
                }
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let normalized: String = stem.nfc().flat_map(char::to_lowercase).collect();
                if !seen.insert(normalized) {
                    self.diagnostics.push(
                        Diagnostic::error(
                            Code::PATH_COLLISION,
                            "schema paths collide under case or Unicode normalization",
                        )
                        .at(relative_path),
                    );
                    continue;
                }
                if kind == SchemaFileKind::Pin {
                    self.pinned.insert(stem.clone());
                    if candidates.contains_key(&stem) {
                        self.superseded_working.push(stem.clone());
                    }
                }
                candidates.insert(stem, (kind, path));
            }
        }
        for (table, (kind, path)) in candidates {
            let relative_path = relative(&self.root, &path);
            let bytes = source
                .read(&path)
                .map_err(|error| DbError::io(&path, error))?;
            let spans = crate::locate::Spans::of(&bytes);
            match Schema::from_bytes(&bytes) {
                Ok(schema) if schema.table() != table => self.diagnostics.push(
                    Diagnostic::error(
                        Code::SCHEMA_TABLE_NAME_MISMATCH,
                        format!(
                            "x-reldir.table is {:?}, but the file is named for {table:?}; a schema governs \
                             the table its file is named for",
                            schema.table()
                        ),
                    )
                    .at(relative_path.clone())
                    .table(&table)
                    .pointer("/x-reldir/table")
                    .locate_in(&bytes, &spans),
                ),
                Ok(schema) => {
                    self.schemas.insert(table.clone(), schema);
                }
                Err(problems) => {
                    self.diagnostics.extend(problems.into_iter().map(|diagnostic| {
                        let mut located = diagnostic.at(relative_path.clone()).table(&table);
                        if located.location.is_none() {
                            located = located.locate_in(&bytes, &spans);
                        } else if located.source_line.is_none()
                            && let Some(location) = &located.location
                        {
                            located.source_line = std::str::from_utf8(&bytes)
                                .ok()
                                .and_then(|text| text.lines().nth(location.line.saturating_sub(1)))
                                .map(String::from);
                        }
                        located
                    }));
                }
            }
            self.schema_files.insert(
                table,
                SchemaFile {
                    kind,
                    relative: relative_path,
                    bytes,
                },
            );
        }
        Ok(())
    }

    /// Bring the mirror in line with the files.
    fn refresh(&mut self, config: &Config, source: &dyn Source) -> Result<()> {
        // Stamps that outlive this process are judged by the filesystem's own
        // clock, so the clock of the process that made them cannot vouch for
        // a file changed in the same tick it was read.
        let (seen_ns, granularity_ns) = if self.mirror.is_persistent() {
            (scan_marker(&self.root)?, self.granularity()?)
        } else {
            (now_ns(), crate::mirror::COARSEST_GRANULARITY_NS)
        };
        let ignores = config
            .ignore_set()
            .map_err(|error| DbError::new(Code::CONFIG_INVALID, error))?;
        let scan = Scan {
            config,
            ignores: &ignores,
            seen_ns,
            granularity_ns,
            source,
        };
        // Tables whose schema cannot be read are not interpreted: their rows
        // have no rules to be judged by, and judging them by stale ones would
        // report faults that are not there.
        let governed: BTreeMap<String, Schema> = self
            .schemas
            .iter()
            .filter(|(table, _)| !self.blocked(table))
            .map(|(table, schema)| (table.clone(), schema.clone()))
            .collect();
        let mirror = Rc::clone(&self.mirror);
        // Asked before anything is examined: a change made while the folder is
        // examined is then in the next answer too.
        let watched = if mirror.is_persistent() {
            self.watched()?
        } else {
            None
        };
        mirror.savepoint("reldir_refresh")?;
        let outcome = (|| -> Result<()> {
            self.rebuilt_tables = mirror.sync_schemas(&governed)?;
            let taken = match &watched {
                Some(changes) if !changes.whole && self.rebuilt_tables.is_empty() => {
                    self.take_watched(&scan, &changes.paths)?
                }
                _ => false,
            };
            self.monitored = taken;
            if !taken {
                for (table, schema) in &governed {
                    self.scan_table(table, schema, &scan)?;
                }
            }
            match &watched {
                Some(changes) => mirror.set_meta(MONITOR, &changes.now.encode())?,
                None => mirror.clear_meta(MONITOR)?,
            }
            // A duplicate that relaxed a table's key indexes may be gone now.
            mirror.tighten(&governed)?;
            if !taken {
                self.find_ungoverned(&ignores, source)?;
            }
            // The observation is kept with the rows it describes, in the same
            // transaction, so a reader answering from the mirror's last commit
            // sees one observation and its rows -- never one without the other.
            mirror.set_meta(OBSERVATION, &self.observation().to_string())?;
            Ok(())
        })();
        match outcome {
            Ok(()) => mirror.release("reldir_refresh")?,
            Err(error) => {
                mirror.rollback_to("reldir_refresh")?;
                return Err(error);
            }
        }
        Ok(())
    }

    /// What the folder monitor says changed since the mirror's mark, when a
    /// monitor runs and answers.
    fn watched(&self) -> Result<Option<crate::monitor::Changes>> {
        let Some(handle) = crate::monitor::find(&self.root) else {
            return Ok(None);
        };
        let since = self
            .mirror
            .meta(MONITOR)?
            .as_deref()
            .and_then(crate::monitor::Mark::decode);
        // A monitor that cannot be asked is one that is not there.
        Ok(handle.changes(&self.root, since).ok())
    }

    /// Take the paths a monitor reported changed, instead of examining the
    /// whole folder. `false` -- and the folder is examined whole -- unless
    /// the last examination found nothing about files themselves (no stray
    /// file, no name collision, no folder without a schema), and every path
    /// is an ordinary row file of a governed table: then the paths alone are
    /// all that can differ, and nothing about them but their rows.
    fn take_watched(&mut self, scan: &Scan<'_>, paths: &[String]) -> Result<bool> {
        let Some(stored) = self.mirror.meta(OBSERVATION)? else {
            return Ok(false);
        };
        let stored: Value = serde_json::from_str(&stored).map_err(|_| {
            DbError::new(
                Code::INTERNAL_METADATA_CORRUPT,
                "the mirror's observation is not JSON",
            )
        })?;
        let found_only_schemas = stored["diagnostics"]
            == serde_json::to_value(&self.diagnostics).unwrap_or_default()
            && stored["warnings"] == serde_json::to_value(&self.warnings).unwrap_or_default()
            && stored["ungoverned"].as_array().is_some_and(Vec::is_empty);
        if !found_only_schemas {
            return Ok(false);
        }
        for relative in paths {
            // Windows reports a table's directory as modified whenever a row
            // in it changes. The rows are reported themselves; the
            // directory's own metadata holds nothing to read.
            if self.schemas.contains_key(relative.as_str())
                && scan
                    .source
                    .metadata(&self.root.join(relative))
                    .is_ok_and(|meta| meta.kind == Kind::Dir)
            {
                continue;
            }
            let Some((table, name)) = self.row_file(Path::new(relative)) else {
                return Ok(false);
            };
            if scan.ignores.is_match(relative) || scan.ignores.is_match(&name) {
                return Ok(false);
            }
            let path = self.root.join(relative);
            let cached = self.mirror.file(relative)?;
            let meta = match scan.source.metadata(&path) {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if cached.is_some() {
                        self.mirror.remove(relative)?;
                        self.forgotten.push(relative.clone());
                    }
                    continue;
                }
                Err(error) => return Err(DbError::io(&path, error)),
            };
            if meta.kind != Kind::File
                || meta.links > 1
                || meta.stat.size > scan.config.max_json_file_size
                || !self
                    .mirror
                    .same_folded(&table, &fold(&name), relative)?
                    .is_empty()
            {
                return Ok(false);
            }
            let raw = match scan.source.read(&path) {
                Ok(raw) => raw,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if cached.is_some() {
                        self.mirror.remove(relative)?;
                        self.forgotten.push(relative.clone());
                    }
                    continue;
                }
                Err(error) => return Err(DbError::io(&path, error)),
            };
            let raw_hash = canonical::hash_bytes(&raw);
            if cached
                .as_ref()
                .is_some_and(|entry| entry.raw_hash == raw_hash)
            {
                self.mirror.touch(relative, &meta.stat, scan.seen_ns)?;
                continue;
            }
            self.reread.push(relative.clone());
            let schema = self.schemas[&table].clone();
            let (row, diagnostics) = judge_row(&table, &schema, &name, &raw, scan.config);
            self.mirror.put(Ingest {
                path: relative,
                table: &table,
                schema: &schema,
                stat: meta.stat,
                raw_hash,
                seen_ns: scan.seen_ns,
                row: row.as_ref(),
                diagnostics,
                style: Some(style_of(&raw)),
            })?;
        }
        Ok(true)
    }

    /// Top-level directories no schema governs.
    fn find_ungoverned(&mut self, ignores: &globset::GlobSet, source: &dyn Source) -> Result<()> {
        let entries = match source.metadata(&self.root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => vec![],
            _ => listing(source, &self.root)?,
        };
        for entry in entries {
            let name = entry
                .path
                .file_name()
                .and_then(|x| x.to_str())
                .unwrap_or("");
            if matches!(name, "schema" | ".db" | ".git")
                || name.starts_with('.')
                || self.schema_files.contains_key(name)
                || ignores.is_match(name)
            {
                continue;
            }
            // An empty directory holds nothing to govern: a dropped table's,
            // as a change that removes it sees it, or one made for a table
            // not begun.
            if entry.meta.kind == Kind::Dir && !listing(source, &entry.path)?.is_empty() {
                self.ungoverned.push(name.to_string());
                self.warnings.push(
                    Diagnostic::warning(
                        Code::UNGOVERNED_DIRECTORY,
                        format!("top-level directory {name:?} has no schema"),
                    )
                    .at(name)
                    .help(ungoverned_help(name)),
                );
            }
        }
        Ok(())
    }

    /// What this observation found beside the rows: which schemas govern
    /// and from where, and the faults of the folder's structure.
    fn observation(&self) -> Value {
        use base64::Engine as _;
        let files: Map<String, Value> = self
            .schema_files
            .iter()
            .map(|(table, file)| {
                (
                    table.clone(),
                    serde_json::json!({
                        "pin": file.kind == SchemaFileKind::Pin,
                        "relative": slash(&file.relative),
                        "bytes": base64::engine::general_purpose::STANDARD.encode(&file.bytes),
                    }),
                )
            })
            .collect();
        serde_json::json!({
            "pinned": self.pinned,
            "ungoverned": self.ungoverned,
            "superseded_working": self.superseded_working,
            "schemas": self.schemas.keys().collect::<Vec<_>>(),
            "schema_files": files,
            "diagnostics": self.diagnostics,
            "warnings": self.warnings,
        })
    }

    /// The database as the mirror's last commit holds it, observed by
    /// whichever writer made that commit: for a reader that will not wait
    /// for the writer now holding the lock. Nothing is read from the folder,
    /// whose files that writer may be changing.
    pub fn from_snapshot(root: &Path, mirror: Rc<Mirror>) -> Result<Self> {
        use base64::Engine as _;
        let text = mirror.meta(OBSERVATION)?.ok_or_else(|| {
            DbError::new(
                Code::INTERNAL_METADATA_CORRUPT,
                "the mirror holds no observation to answer from",
            )
        })?;
        let damaged = |what: &str| {
            DbError::new(
                Code::INTERNAL_METADATA_CORRUPT,
                format!("the mirror's observation {what}"),
            )
        };
        let record: Value = serde_json::from_str(&text).map_err(|_| damaged("is not JSON"))?;
        let strings = |key: &str| -> Result<Vec<String>> {
            serde_json::from_value(record[key].clone())
                .map_err(|_| damaged(&format!("has no {key}")))
        };
        let diagnostics = |key: &str| -> Result<Vec<Diagnostic>> {
            serde_json::from_value(record[key].clone())
                .map_err(|_| damaged(&format!("has no {key}")))
        };
        let mut schema_files = BTreeMap::new();
        for (table, file) in record["schema_files"]
            .as_object()
            .ok_or_else(|| damaged("has no schema files"))?
        {
            let bytes = file["bytes"]
                .as_str()
                .and_then(|text| base64::engine::general_purpose::STANDARD.decode(text).ok())
                .ok_or_else(|| damaged("holds a schema it cannot read back"))?;
            let relative = file["relative"]
                .as_str()
                .ok_or_else(|| damaged("holds a schema with no path"))?;
            schema_files.insert(
                table.clone(),
                SchemaFile {
                    kind: if file["pin"].as_bool() == Some(true) {
                        SchemaFileKind::Pin
                    } else {
                        SchemaFileKind::Working
                    },
                    relative: PathBuf::from(relative),
                    bytes,
                },
            );
        }
        let mut schemas = BTreeMap::new();
        for table in strings("schemas")? {
            let file = schema_files
                .get(&table)
                .ok_or_else(|| damaged("names a schema it does not hold"))?;
            let schema = Schema::from_bytes(&file.bytes)
                .map_err(|_| damaged("holds a schema that no longer loads"))?;
            schemas.insert(table, schema);
        }
        Ok(Self {
            root: root.to_path_buf(),
            ungoverned: strings("ungoverned")?,
            pinned: strings("pinned")?.into_iter().collect(),
            schemas,
            schema_files,
            superseded_working: strings("superseded_working")?,
            diagnostics: diagnostics("diagnostics")?,
            warnings: diagnostics("warnings")?,
            mirror,
            reread: vec![],
            monitored: false,
            forgotten: vec![],
            rebuilt_tables: vec![],
            rebuilt_mirror: false,
        })
    }

    /// The verdict judged of this same state before, when this observation
    /// changed nothing: no row read afresh or found gone, no table rebuilt,
    /// and the same schemas, structure and settings as when it was judged.
    pub fn earlier_verdict(&self, config: &Config) -> Result<Option<crate::integrity::Verdict>> {
        if !self.reread.is_empty()
            || !self.forgotten.is_empty()
            || !self.rebuilt_tables.is_empty()
            || self.rebuilt_mirror
        {
            return Ok(None);
        }
        let Some(key) = self.mirror.meta(VERDICT_KEY)? else {
            return Ok(None);
        };
        if key != self.verdict_key(config) {
            return Ok(None);
        }
        let Some(text) = self.mirror.meta(VERDICT)? else {
            return Ok(None);
        };
        let verdict = serde_json::from_str(&text).map_err(|error| {
            DbError::new(
                Code::INTERNAL_METADATA_CORRUPT,
                format!("derived mirror: the kept verdict does not read back: {error}"),
            )
        })?;
        Ok(Some(verdict))
    }

    /// Keep a verdict of the state this catalog observed, for the next
    /// observation that finds nothing changed.
    pub fn keep_verdict(&self, config: &Config, verdict: &crate::integrity::Verdict) -> Result<()> {
        let text = serde_json::to_string(verdict).map_err(|error| {
            DbError::new(
                Code::INTERNAL_ERROR,
                format!("a verdict did not serialize: {error}"),
            )
        })?;
        self.mirror.set_meta(VERDICT, &text)?;
        self.mirror.set_meta(VERDICT_KEY, &self.verdict_key(config))
    }

    /// What a verdict depends on beside the rows: the observation -- schemas,
    /// structure -- and the settings, hashed.
    fn verdict_key(&self, config: &Config) -> String {
        let settings = serde_json::to_string(config).unwrap_or_default();
        canonical::hash_bytes(format!("{}\n{settings}", self.observation()).as_bytes())
    }

    /// The filesystem's timestamp granularity, measured once per mirror: a
    /// file whose time is within it of a scan's marker may have been written
    /// again in the same tick, and is read again rather than trusted.
    fn granularity(&self) -> Result<i64> {
        use crate::engine::fs::Fs as _;
        if let Some(text) = self.mirror.meta(GRANULARITY)? {
            return text.parse().map_err(|_| {
                DbError::new(
                    Code::INTERNAL_METADATA_CORRUPT,
                    format!("derived mirror: {text:?} is not a timestamp granularity"),
                )
            });
        }
        let directory = cache_dir(&self.root)?;
        let measured = crate::engine::fs::Disk
            .mtime_granularity_ns(&directory)
            .map_err(|error| DbError::io(&directory, error))?;
        self.mirror.set_meta(GRANULARITY, &measured.to_string())?;
        Ok(measured)
    }

    /// Whether a table's schema has faults that stop its rows being judged.
    pub fn blocked(&self, table: &str) -> bool {
        !self.schemas.contains_key(table)
            || self.diagnostics.iter().any(|d| {
                d.table.as_deref() == Some(table) && d.code.as_str().starts_with("SCHEMA_")
            })
    }

    /// Bring the mirror's entries for one table up to date with its
    /// directory: read what changed, and forget what is gone.
    fn scan_table(&mut self, table: &str, schema: &Schema, scan: &Scan<'_>) -> Result<()> {
        let Scan {
            config,
            ignores,
            seen_ns,
            granularity_ns,
            source,
        } = *scan;
        let directory = self.root.join(table);
        match source.metadata(&directory) {
            Ok(meta) if meta.kind == Kind::Dir => {}
            Ok(_) => {
                self.forget_table_files(table)?;
                self.diagnostics.push(
                    Diagnostic::error(
                        Code::NON_REGULAR_FILE,
                        "a table's path must be a real directory",
                    )
                    .at(table)
                    .table(table),
                );
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return self.forget_table_files(table);
            }
            Err(error) => return Err(DbError::io(&directory, error)),
        }
        let entries = listing(source, &directory)?;
        let rebuilt = self.rebuilt_tables.iter().any(|t| t == table);
        // A rebuilt table starts empty, so it has no stamps to trust.
        let mut stamps = if rebuilt {
            Default::default()
        } else {
            self.mirror.stamps(table)?
        };
        let mut folded: std::collections::HashMap<String, String> =
            std::collections::HashMap::with_capacity(entries.len());
        let mut progress = crate::progress::Progress::new("scanning", entries.len());
        for Entry { path, meta } in entries {
            progress.advance();
            let relative_path = relative(&self.root, &path);
            let name = path.file_name().and_then(|x| x.to_str()).unwrap_or("");
            let key = if name.is_empty() {
                slash(&relative_path)
            } else {
                format!("{table}/{name}")
            };
            if ignores.is_match(&relative_path) || ignores.is_match(name) {
                continue;
            }
            // Two names one filesystem keeps apart and another folds into one
            // file. Here they are two rows; whether that is a fault is the
            // folder's portability decision.
            if let Some(first) = folded.insert(fold(name), name.to_string()) {
                let message = format!(
                    "{name:?} and {first:?} are one name to a filesystem that ignores case or \
                     normalizes Unicode, which would keep only one of them"
                );
                let collision = if config.portable_names {
                    Diagnostic::error(Code::PORTABILITY_COLLISION, message)
                } else {
                    Diagnostic::warning(Code::PORTABILITY_COLLISION, message)
                        .help("rename one of them before the folder is copied to macOS or Windows; `portable_names` in .db/config makes this an error")
                };
                let collision = collision.at(relative_path.clone()).table(table);
                if config.portable_names {
                    self.diagnostics.push(collision);
                } else {
                    self.warnings.push(collision);
                }
            }
            if meta.kind != Kind::File {
                self.diagnostics.push(
                    Diagnostic::error(
                        if meta.kind == Kind::Dir {
                            Code::UNEXPECTED_FILE
                        } else {
                            Code::NON_REGULAR_FILE
                        },
                        "a table directory holds only regular .json files",
                    )
                    .at(relative_path)
                    .table(table),
                );
                continue;
            }
            if meta.links > 1 {
                self.diagnostics.push(
                    Diagnostic::error(Code::NON_REGULAR_FILE, "hard-linked row files are refused")
                        .at(relative_path)
                        .table(table),
                );
                continue;
            }
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                self.diagnostics.push(
                    Diagnostic::error(
                        Code::UNEXPECTED_FILE,
                        "a table directory holds only .json files",
                    )
                    .at(relative_path)
                    .table(table)
                    .help("move it out, or add it to the ignore list in .db/config"),
                );
                continue;
            }
            if meta.stat.size > config.max_json_file_size {
                self.diagnostics.push(
                    Diagnostic::error(
                        Code::FILE_TOO_LARGE,
                        format!("file exceeds the {} byte limit", config.max_json_file_size),
                    )
                    .at(relative_path)
                    .table(table),
                );
                continue;
            }
            // What is left in `stamps` after the scan is what is gone.
            let cached = stamps.remove(&key);
            if let Some(stamp) = &cached
                && stamp.trusted_for(&meta.stat, granularity_ns)
            {
                continue;
            }
            let raw = match source.read(&path) {
                Ok(raw) => raw,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if cached.is_some() {
                        self.mirror.remove(&key)?;
                        self.forgotten.push(key);
                    }
                    continue;
                }
                Err(error) => return Err(DbError::io(&path, error)),
            };
            let raw_hash = canonical::hash_bytes(&raw);
            if let Some(stamp) = &cached
                && stamp.raw_hash == raw_hash
            {
                self.mirror.touch(&key, &meta.stat, seen_ns)?;
                continue;
            }
            self.reread.push(key.clone());
            let (row, diagnostics) = judge_row(table, schema, name, &raw, config);
            self.mirror.put(Ingest {
                path: &key,
                table,
                schema,
                stat: meta.stat,
                raw_hash,
                seen_ns,
                row: row.as_ref(),
                diagnostics,
                style: Some(style_of(&raw)),
            })?;
        }
        for gone in stamps.into_keys() {
            self.mirror.remove(&gone)?;
            self.forgotten.push(gone);
        }
        Ok(())
    }

    /// Forget every file of a table whose directory is not there to scan.
    fn forget_table_files(&mut self, table: &str) -> Result<()> {
        for path in self.mirror.paths(table)? {
            self.mirror.remove(&path)?;
            self.forgotten.push(path);
        }
        Ok(())
    }

    /// Every row of a table, in path order.
    pub fn rows(&self, table: &str) -> Result<Vec<Row>> {
        let mut out = vec![];
        self.each_row(table, |row| {
            out.push(row);
            Ok(())
        })?;
        Ok(out)
    }

    /// Visit every row of a table in path order without holding the table.
    pub fn each_row(&self, table: &str, mut visit: impl FnMut(Row) -> Result<()>) -> Result<()> {
        self.mirror.each_file(table, |entry| {
            if let Some(value) = entry.doc {
                visit(Row {
                    table: entry.table,
                    relative: PathBuf::from(entry.path),
                    value,
                })?;
            }
            Ok(())
        })
    }

    /// The row a file holds, if the mirror has read one there.
    pub fn row_at(&self, relative_path: &Path) -> Result<Option<Row>> {
        Ok(self.mirror.file(&slash(relative_path))?.and_then(|entry| {
            entry.doc.map(|value| Row {
                table: entry.table,
                relative: relative_path.to_path_buf(),
                value,
            })
        }))
    }

    /// The row a table holds under a primary key, rendered as keys are.
    pub fn row_by_key(&self, table: &str, key: &str) -> Result<Option<Row>> {
        let holders = self
            .mirror
            .holders(key, mirror::PRIMARY, &[table.to_string()])?;
        match holders.first() {
            Some((_, path)) => self.row_at(Path::new(path)),
            None => Ok(None),
        }
    }

    pub fn row_count(&self) -> Result<u64> {
        let mut total = 0;
        for table in self.schemas.keys() {
            total += self.mirror.count(table)?;
        }
        Ok(total)
    }
}

/// Everything wrong with one file on its own: whether it parses, whether it is
/// an object, whether it satisfies its schema, and whether its name is the one
/// its identity derives.
pub fn judge_row(
    table: &str,
    schema: &Schema,
    name: &str,
    raw: &[u8],
    config: &Config,
) -> (Option<Map<String, Value>>, Vec<Diagnostic>) {
    let mut out = vec![];
    let value = match crate::json::parse(raw) {
        Ok(value) => value,
        Err(error) => {
            let diagnostic = if crate::json::is_depth_limit(&error) {
                Diagnostic::error(
                    Code::NESTING_TOO_DEEP,
                    format!(
                        "JSON nesting exceeds the depth limit {}",
                        config.max_nesting_depth
                    ),
                )
            } else if crate::merge::has_conflict_markers(raw) {
                Diagnostic::error(
                    Code::MERGE_CONFLICT,
                    "the file holds a merge's conflict markers, so it is not JSON until they are resolved",
                )
                .help(
                    "keep one side of each conflict and delete the markers; with `reldir git setup`, \
                     later merges of row files conflict only where both sides changed one member",
                )
            } else {
                let mut diagnostic = Diagnostic::error(Code::INVALID_JSON, error.to_string());
                diagnostic.location = Some(crate::diagnostic::Location {
                    line: error.line(),
                    column: error.column(),
                });
                diagnostic.source_line = String::from_utf8_lossy(raw)
                    .lines()
                    .nth(error.line().saturating_sub(1))
                    .map(String::from);
                diagnostic
            };
            out.push(diagnostic.table(table));
            return (None, out);
        }
    };
    let Some(object) = value.as_object() else {
        out.push(
            Diagnostic::error(
                Code::ROW_ROOT_NOT_OBJECT,
                "a row file holds one JSON object",
            )
            .table(table)
            .pointer(""),
        );
        return (None, out);
    };
    let spans = crate::locate::Spans::of(raw);
    let found = schema.validator().check(&value);
    let missing: Vec<&str> = found
        .iter()
        .filter(|d| d.code == Code::ROW_MISSING_FIELD)
        .filter_map(|d| d.field.as_deref())
        .collect();
    for mut diagnostic in found.iter().cloned() {
        attach_fixes(&mut diagnostic, schema, &value, &missing);
        out.push(diagnostic.table(table).locate_in(raw, &spans));
    }
    let stem = name.strip_suffix(crate::naming::EXTENSION).unwrap_or(name);
    let mut view = object.clone();
    match schema.row_identity() {
        crate::naming::Identity::FileStem { column, kind } => {
            match schema.row_identity().stem_value(stem) {
                Some(value) => {
                    view.insert(column.clone(), value);
                }
                None => out.push(
                    Diagnostic::error(
                        Code::IDENTITY_MISMATCH,
                        format!(
                            "the file is named {name:?}, and this table's file names are its keys, \
                             which are {}",
                            match kind {
                                crate::naming::StemKind::Int => "64-bit integers",
                                crate::naming::StemKind::String => "strings",
                            }
                        ),
                    )
                    .table(table)
                    .observed(name),
                ),
            }
        }
        crate::naming::Identity::Body { .. } => match schema.row_identity().stem(object) {
            Ok(expected) if expected != stem => {
                let expected = format!("{expected}{}", crate::naming::EXTENSION);
                out.push(
                    Diagnostic::error(
                        Code::IDENTITY_MISMATCH,
                        format!(
                            "the file is named {name:?}, but the row's identity names it {expected:?}"
                        ),
                    )
                    .table(table)
                    .expected(expected.clone())
                    .observed(name)
                    .fix("FIX_RENAME_TO_IDENTITY")
                    .help(format!(
                        "rename the file to {expected}; the row itself is unaffected"
                    )),
                );
            }
            Ok(_) => {}
            Err(error) => out.push(crate::plan::naming_diagnostic(schema, &error).observed(name)),
        },
    }
    (Some(view), out)
}

/// Offer exactly the repairs that would work for this row: a coercion only when
/// one exists, a default only when the column declares one, a rename only when
/// an unknown member is unambiguously a misspelt missing column.
fn attach_fixes(diagnostic: &mut Diagnostic, schema: &Schema, row: &Value, missing: &[&str]) {
    let top_column = diagnostic
        .pointer
        .as_deref()
        .map(crate::schema::path::pointer_tokens)
        .filter(|tokens| tokens.len() == 1)
        .map(|mut tokens| tokens.remove(0));
    match diagnostic.code {
        Code::TYPE_MISMATCH => {
            let coercible = top_column
                .as_deref()
                .is_some_and(|column| schema.validator().coercion(row, column).is_some());
            if !coercible {
                diagnostic.fixes.retain(|fix| fix != "FIX_COERCE_VALUE");
            }
        }
        Code::ROW_MISSING_FIELD | Code::NOT_NULL_VIOLATION => {
            let column = diagnostic
                .field
                .as_deref()
                .and_then(|name| schema.column(name));
            if column.is_some_and(|column| column.default().is_some_and(|value| !value.is_null())) {
                diagnostic.fixes.push("FIX_FILL_DEFAULT".into());
            }
        }
        Code::ROW_UNKNOWN_FIELD => {
            if let Some(unknown) = diagnostic.field.clone() {
                let mut ranked: Vec<(usize, &str)> = missing
                    .iter()
                    .map(|name| (strsim::levenshtein(&unknown, name), *name))
                    .filter(|(distance, _)| *distance <= 2)
                    .collect();
                ranked.sort();
                let unambiguous = match ranked.as_slice() {
                    [only] => Some(only.1),
                    [first, second, ..] if first.0 < second.0 => Some(first.1),
                    _ => None,
                };
                if let Some(name) = unambiguous {
                    diagnostic.expected = Some(name.to_string());
                    diagnostic.fixes.insert(0, "FIX_RENAME_FIELD".into());
                    diagnostic.help = Some(format!("did you mean {name:?}?"));
                }
            }
        }
        _ => {}
    }
}

/// What one observation reads every table with.
#[derive(Clone, Copy)]
struct Scan<'a> {
    config: &'a Config,
    ignores: &'a globset::GlobSet,
    /// The filesystem's time when the observation began.
    seen_ns: i64,
    /// How finely the filesystem's timestamps tell moments apart.
    granularity_ns: i64,
    source: &'a dyn Source,
}

/// A directory's entries, each with its metadata. An entry removed between
/// the listing and its examination fails the listing -- it would otherwise
/// mix two moments -- so it is taken again, a few times, before the churn is
/// reported as the failure it is.
fn listing(source: &dyn Source, directory: &Path) -> Result<Vec<Entry>> {
    let mut attempts = 0;
    loop {
        match source.read_dir(directory) {
            Ok(entries) => return Ok(entries),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && attempts < 3
                    && source.metadata(directory).is_ok() =>
            {
                attempts += 1;
            }
            Err(error) => return Err(DbError::io(directory, error)),
        }
    }
}

/// Where a scan's marker and probes are written.
fn cache_dir(root: &Path) -> Result<PathBuf> {
    let directory = root.join(".db").join("cache");
    crate::engine::fs::ensure_dir(&crate::engine::fs::Disk, &directory)
        .map_err(|error| DbError::io(&directory, error))?;
    Ok(directory)
}

/// The filesystem's time now, read from a marker file written for the
/// purpose: what a stamp made by this observation is compared against later
/// (git's answer to "racy" timestamps).
fn scan_marker(root: &Path) -> Result<i64> {
    use crate::engine::fs::Fs as _;
    let marker = cache_dir(root)?.join("scan-marker");
    crate::engine::fs::Disk
        .touch(&marker)
        .map_err(|error| DbError::io(&marker, error))
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as i64)
}

fn relative(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

/// Whether a file has other names. A hard-linked metadata or row file could be
/// changed through a path reldir does not govern, so it is refused.
#[cfg(unix)]
pub fn has_multiple_links(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() > 1
}

#[cfg(not(unix))]
pub fn has_multiple_links(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// A relative path with `/` separators, as the mirror and provenance name it.
/// A file name folded as case-insensitive, normalising filesystems compare
/// names: two names that fold alike are one file on some of them.
/// The style a file is written in, as the mirror keeps it: a fingerprint of
/// the detected style, short enough to keep for every file.
pub(crate) fn style_of(raw: &[u8]) -> String {
    style_fingerprint(&crate::json_edit::Style::detect(raw))
}

/// A style's fingerprint: equal exactly when the styles are.
pub fn style_fingerprint(style: &crate::json_edit::Style) -> String {
    let text = serde_json::to_string(style).expect("a style serializes");
    crate::canonical::hash_bytes(text.as_bytes())[..16].to_string()
}

pub(crate) fn fold(name: &str) -> String {
    if name.is_ascii() {
        // NFC is the identity on ASCII, and lowercasing it is byte-wise.
        name.to_ascii_lowercase()
    } else {
        name.nfc().flat_map(char::to_lowercase).collect()
    }
}

pub fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
