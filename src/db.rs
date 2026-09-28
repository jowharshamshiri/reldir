//! Opening a database, and the one path by which anything in it changes.
//!
//! **Opening** observes the directory and judges it. With write access it
//! first takes the writer lock and finishes any interrupted transaction, then
//! brings the persistent mirror in line with the files (re-reading only what
//! changed), and -- when the state is valid and differs from recorded history
//! -- records it, because accepting a valid external edit is the system's
//! central promise. With read access it takes no lock and writes nothing: the
//! mirror is copied into memory and brought up to date there.
//!
//! **Committing** is the same sequence for every writer -- SQL, the row
//! commands, import, migrations, doctor, snapshot restore:
//!
//! 1. take the writer lock, finish any interrupted transaction, re-observe;
//! 2. confirm that what the change was planned against has not moved;
//! 3. complete row changes with their referential actions;
//! 4. judge the state the change would produce, from memory, before writing;
//! 5. write it through the recoverable transaction protocol;
//! 6. re-observe and record the new revision.
//!
//! Nothing reaches the disk unless step 4 admits it, so no command can leave a
//! valid database invalid.

use crate::{
    FORMAT_VERSION,
    catalog::Catalog,
    config::{Config, ResourceOverrides},
    diagnostic::{DbError, Diagnostic, Result, Severity},
    fs::{Disk, Overlay},
    integrity::{self, Verdict},
    metadata::{self, Provenance},
    mirror::Mirror,
    plan::RowChange,
    probe,
    referential::Induced,
    schema::Schema,
    transaction::{self, Change},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

/// Whether a command may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Take no lock and write nothing, not even derived state.
    Read,
    /// Take the writer lock while opening and committing; recover, maintain the
    /// mirror, and record accepted external changes.
    Write,
}

/// What recorded history is, as far as this open could tell.
#[derive(Debug)]
pub enum History {
    /// Nothing has been recorded yet.
    Empty,
    /// The latest revision, verified to continue its predecessors.
    Head(Box<Provenance>),
    /// History cannot be read or does not verify. Reads still work; nothing can
    /// be recorded until it is repaired.
    Degraded(Box<DbError>),
}

impl History {
    pub fn head(&self) -> Option<&Provenance> {
        match self {
            Self::Head(head) => Some(head),
            _ => None,
        }
    }
}

/// Something opening the database did on its own, reported so nothing happens
/// silently.
#[derive(Debug, Clone)]
pub enum Event {
    /// Interrupted transactions were rolled forward or discarded.
    Recovered {
        completed: Vec<String>,
        discarded: Vec<String>,
    },
    /// A valid state that differed from history was recorded.
    Recorded {
        revision: u64,
        origin: String,
        changes: Vec<String>,
    },
    /// The mirror could not be read and was rebuilt from the files.
    MirrorRebuilt,
    /// Working schemas superseded by pins were removed.
    SupersededRemoved(Vec<String>),
}

impl Event {
    pub fn describe(&self) -> String {
        match self {
            Self::Recovered {
                completed,
                discarded,
            } => format!(
                "recovered interrupted transactions: {} rolled forward, {} discarded",
                completed.len(),
                discarded.len()
            ),
            Self::Recorded {
                revision,
                origin,
                changes,
            } => format!(
                "recorded revision {revision} ({origin}): {} change(s)",
                changes.len()
            ),
            Self::MirrorRebuilt => {
                "the derived mirror was unreadable and was rebuilt from the files".into()
            }
            Self::SupersededRemoved(tables) => format!(
                "removed working schemas superseded by pins: {}",
                tables.join(", ")
            ),
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Self::Recovered {
                completed,
                discarded,
            } => {
                json!({"kind": "recovered", "completed": completed, "discarded": discarded})
            }
            Self::Recorded {
                revision,
                origin,
                changes,
            } => {
                json!({"kind": "recorded", "revision": revision, "origin": origin, "changes": changes})
            }
            Self::MirrorRebuilt => json!({"kind": "mirror_rebuilt"}),
            Self::SupersededRemoved(tables) => {
                json!({"kind": "superseded_removed", "tables": tables})
            }
        }
    }
}

/// What a change must leave behind to be admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// A valid database: no errors at all.
    Valid,
    /// No fault that was not already there. For repairs, which may fix one
    /// fault of many; they can never add one.
    NoNewFaults,
}

/// How a change is to be committed.
#[derive(Debug, Clone, Copy)]
pub struct Request {
    /// The provenance origin the revision is recorded under.
    pub origin: &'static str,
    pub admission: Admission,
    /// Judge and report the change without writing it.
    pub dry_run: bool,
}

impl Request {
    pub fn internal(dry_run: bool) -> Self {
        Self {
            origin: "internal",
            admission: Admission::Valid,
            dry_run,
        }
    }
}

/// What a commit did, or -- for a dry run -- would do.
#[derive(Debug)]
pub struct Outcome {
    /// The row changes, including those referential actions induced.
    pub rows: Vec<RowChange>,
    /// Changes the engine made on the plan's behalf, and why.
    pub induced: Vec<Induced>,
    /// The file changes, in the order they are applied.
    pub changes: Vec<Change>,
    /// The revision recorded, when something was written.
    pub revision: Option<u64>,
    /// Warnings the resulting state carries.
    pub warnings: Vec<Diagnostic>,
    pub dry_run: bool,
}

/// The raw-bytes hash each path had when a change was planned, `None` for a
/// path that did not exist. A commit refuses if any of them moved.
pub type Expected = BTreeMap<PathBuf, Option<String>>;

pub struct Database {
    pub root: PathBuf,
    pub config: Config,
    pub catalog: Catalog,
    pub verdict: Verdict,
    pub history: History,
    /// What opening did on its own.
    pub events: Vec<Event>,
    /// Paths whose state differs from history and was not recorded.
    pub unrecorded: Vec<String>,
    pub validation_elapsed: std::time::Duration,
    access: Access,
    overrides: ResourceOverrides,
}

impl Database {
    /// Open a database that has `.db/`.
    pub fn open(root: PathBuf, access: Access, overrides: &ResourceOverrides) -> Result<Self> {
        Self::open_as(root, access, overrides, "external")
    }

    /// Establish a database over a folder: write the layout and the schemas,
    /// then open it, recording what was adopted as an import.
    pub fn create(
        root: PathBuf,
        schemas: &BTreeMap<String, Schema>,
        track_provenance: bool,
        overrides: &ResourceOverrides,
    ) -> Result<Self> {
        init_layout(&root, track_provenance)?;
        let config = load_config(&root)?;
        for schema in schemas.values() {
            metadata::write_bytes_atomic(
                &crate::schema_store::working_path(&root, schema.table()),
                &schema.bytes(config.indentation_width),
            )?;
        }
        Self::open_as(root, Access::Write, overrides, "import")
    }

    /// Answer reads over a folder that has no `.db/`, using schemas that are
    /// never written. Nothing is persisted and nothing is recorded: this is an
    /// observation, not a revision.
    pub fn ephemeral(
        root: PathBuf,
        schemas: BTreeMap<String, Schema>,
        overrides: &ResourceOverrides,
    ) -> Result<Self> {
        let started = std::time::Instant::now();
        let config = configured(&root, overrides)?;
        let catalog = Catalog::observe_with_schemas(&root, &config, &Disk, schemas)?;
        let verdict = integrity::validate(&catalog)?;
        Ok(Self {
            root,
            config,
            catalog,
            verdict,
            history: History::Empty,
            events: vec![],
            unrecorded: vec![],
            validation_elapsed: started.elapsed(),
            access: Access::Read,
            overrides: overrides.clone(),
        })
    }

    fn open_as(
        root: PathBuf,
        access: Access,
        overrides: &ResourceOverrides,
        origin: &str,
    ) -> Result<Self> {
        let started = std::time::Instant::now();
        validate_format(&root)?;
        let mut events = vec![];
        let mut warnings = vec![];
        let (lock, mirror) = match access {
            Access::Write => {
                let waiting = configured(&root, overrides)?;
                require_safe_filesystem(&root, &waiting)?;
                let lock = acquire(&root, &waiting, crate::lock::Holder::Observing)?;
                let recovered = transaction::recover(&Disk, &root)?;
                if !recovered.completed.is_empty() || !recovered.discarded.is_empty() {
                    events.push(Event::Recovered {
                        completed: recovered.completed,
                        discarded: recovered.discarded,
                    });
                }
                let (mirror, rebuilt) = Mirror::open_persistent(&root)?;
                if rebuilt {
                    events.push(Event::MirrorRebuilt);
                }
                (Some(lock), mirror)
            }
            Access::Read => {
                match transaction::pending(&Disk, &root)? {
                    transaction::Pending::Materialising => {
                        return Err(DbError::from_diag(
                            Diagnostic::error(
                                "RECOVERY_REQUIRED",
                                "a transaction was interrupted while applying its changes, so the rows are \
                                 part old and part new; no answer drawn from them would be sound",
                            )
                            .help("run any writing command, or `reldir recover`, to roll it forward"),
                            5,
                        ));
                    }
                    transaction::Pending::Staged => warnings.push(
                        Diagnostic::warning(
                            "TRANSACTION_STAGED",
                            "a transaction is staged but has not begun applying; the rows are \
                             unaffected and it was left in place",
                        )
                        .help("any writing command, or `reldir recover`, clears it"),
                    ),
                    transaction::Pending::None => {}
                }
                (None, Mirror::open_ephemeral(&root)?)
            }
        };
        let config = configured(&root, overrides)?;
        let catalog = Catalog::observe(
            &root,
            &config,
            &Disk,
            Rc::new(mirror),
            events.iter().any(|e| matches!(e, Event::MirrorRebuilt)),
        )?;
        let mut verdict = integrity::validate(&catalog)?;
        verdict.warnings.extend(warnings);

        let history = match metadata::head(&root) {
            Ok(head) => match metadata::sync_recorded(&catalog, head.as_ref()) {
                Ok(()) => match head {
                    Some(head) => History::Head(Box::new(head)),
                    None => History::Empty,
                },
                Err(error) => History::Degraded(Box::new(error)),
            },
            Err(error) => History::Degraded(Box::new(error)),
        };
        if let History::Degraded(error) = &history {
            verdict.errors.push(
                (*error.diagnostic.clone()).help(
                    "recorded history cannot be verified. Restore .db/provenance and .db/objects from \
                     a backup, or run `reldir recover --history=new-lineage --allow-destructive` to \
                     move it aside and begin a new history from the current files",
                ),
            );
        }

        let mut database = Self {
            root,
            config,
            catalog,
            verdict,
            history,
            events,
            unrecorded: vec![],
            validation_elapsed: std::time::Duration::ZERO,
            access,
            overrides: overrides.clone(),
        };
        if access == Access::Write {
            database.remove_superseded()?;
        }
        database.adopt(origin)?;
        database.validation_elapsed = started.elapsed();
        drop(lock);
        Ok(database)
    }

    /// Discard working schemas a pin has replaced. They are derived copies
    /// that govern nothing, and leaving them would let a later `rm schema/x`
    /// silently revive an old schema.
    fn remove_superseded(&mut self) -> Result<()> {
        let superseded = std::mem::take(&mut self.catalog.superseded_working);
        if superseded.is_empty() {
            return Ok(());
        }
        for table in &superseded {
            let path = crate::schema_store::working_path(&self.root, table);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(DbError::io(&path, error)),
            }
        }
        metadata::sync_parent(&crate::schema_store::working_path(
            &self.root,
            &superseded[0],
        ))?;
        self.events.push(Event::SupersededRemoved(superseded));
        Ok(())
    }

    /// Record the observed state when it is valid and history does not have
    /// it; with read access, say that it was not recorded.
    fn adopt(&mut self, origin: &str) -> Result<()> {
        let (History::Head(_) | History::Empty) = &self.history else {
            return Ok(());
        };
        let pending = metadata::pending_changes(&self.catalog)?;
        if pending.is_empty() {
            return Ok(());
        }
        let summary = summarize(&pending, &self.catalog)?;
        if !self.verdict.errors.is_empty() {
            self.unrecorded = summary;
            return Ok(());
        }
        match self.access {
            Access::Write => {
                let recovered = self
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::Recovered { .. }));
                let origin = if recovered && origin == "external" {
                    "recovery"
                } else {
                    origin
                };
                let record =
                    metadata::record(&self.catalog, self.history.head(), origin, None, None)?;
                self.events.push(Event::Recorded {
                    revision: record.revision,
                    origin: origin.to_string(),
                    changes: summary,
                });
                self.history = History::Head(Box::new(record));
            }
            Access::Read => {
                self.verdict.warnings.push(
                    Diagnostic::warning(
                        "METADATA_STALE_READONLY",
                        format!(
                            "the state is valid but {} path(s) differ from recorded history; read-only \
                             access did not record them",
                            summary.len()
                        ),
                    )
                    .help("any writing command records them"),
                );
                self.unrecorded = summary;
            }
        }
        Ok(())
    }

    pub fn access(&self) -> Access {
        self.access
    }

    pub fn overrides(&self) -> &ResourceOverrides {
        &self.overrides
    }

    pub fn is_valid(&self) -> bool {
        self.verdict.errors.is_empty()
    }

    /// Refuse to go on from an invalid state, carrying every fault.
    pub fn require_valid(&self) -> Result<()> {
        match self.verdict.errors.first() {
            None => Ok(()),
            Some(first) => Err(DbError::from_diag(
                first.clone(),
                crate::diagnostic::exit_code_for_diagnostics(&self.verdict.errors),
            )
            .with_related(self.verdict.errors[1..].to_vec())),
        }
    }

    /// The hash of a file's bytes as they are now, relative to the root, for
    /// planning a change that must not race an edit.
    pub fn fingerprint(&self, relative: &Path) -> Result<Option<String>> {
        let path = self.root.join(relative);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(crate::canonical::hash_bytes(&bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(DbError::io(&path, error)),
        }
    }

    /// Apply row changes: complete them with their referential actions, and
    /// commit the files they amount to.
    pub fn apply(&mut self, rows: Vec<RowChange>, request: Request) -> Result<Outcome> {
        self.change(rows, vec![], &Expected::new(), request)
    }

    /// Plan row changes under the writer lock, against the state they will
    /// be committed to, and apply them. A statement such as
    /// `UPDATE t SET n = n + 1` is therefore computed from the current value
    /// even when another writer committed a moment before: concurrent writers
    /// queue for the lock instead of refusing each other.
    pub fn apply_with<T>(
        &mut self,
        request: Request,
        plan: impl FnOnce(&Catalog) -> Result<(Vec<RowChange>, T)>,
    ) -> Result<(Outcome, T)> {
        let mut extra = None;
        let outcome = self.transact(request, |database| {
            let (rows, value) = plan(&database.catalog)?;
            extra = Some(value);
            if rows.is_empty() {
                return Ok((vec![], vec![], vec![]));
            }
            let expansion = crate::referential::expand(&database.catalog, rows)?;
            let changes = crate::plan::render(&database.catalog, &expansion.rows)?;
            Ok((expansion.rows, expansion.induced, changes))
        })?;
        Ok((
            outcome,
            extra.expect("the plan ran, or the transaction failed"),
        ))
    }

    /// Commit file changes planned directly -- schemas, configuration, repairs.
    /// Every path in `expected` must still hold what it held when planned.
    pub fn commit(
        &mut self,
        changes: Vec<Change>,
        expected: &Expected,
        request: Request,
    ) -> Result<Outcome> {
        self.change(vec![], changes, expected, request)
    }

    /// Commit row changes and file changes as one transaction. The two must
    /// not touch the same path.
    pub fn change(
        &mut self,
        rows: Vec<RowChange>,
        files: Vec<Change>,
        expected: &Expected,
        request: Request,
    ) -> Result<Outcome> {
        self.transact(request, |database| {
            for (relative, hash) in expected {
                if &database.fingerprint(relative)? != hash {
                    return Err(moved(relative));
                }
            }
            database.confirm_rows_unmoved(&rows)?;
            let (rows, induced, mut changes) = if rows.is_empty() {
                (vec![], vec![], vec![])
            } else {
                let expansion = crate::referential::expand(&database.catalog, rows)?;
                let changes = crate::plan::render(&database.catalog, &expansion.rows)?;
                (expansion.rows, expansion.induced, changes)
            };
            changes.extend(files);
            Ok((rows, induced, changes))
        })
    }

    fn transact(
        &mut self,
        request: Request,
        plan: impl FnOnce(&mut Self) -> Result<(Vec<RowChange>, Vec<Induced>, Vec<Change>)>,
    ) -> Result<Outcome> {
        if self.access == Access::Read {
            return Err(DbError::new(
                "READ_ONLY",
                "the change was not made: this command opened the database read-only",
                1,
            ));
        }
        require_safe_filesystem(&self.root, &self.config)?;
        let lock = acquire(&self.root, &self.config, crate::lock::Holder::Committing)?;
        let recovered = transaction::recover(&Disk, &self.root)?;
        if !recovered.completed.is_empty() || !recovered.discarded.is_empty() {
            self.events.push(Event::Recovered {
                completed: recovered.completed,
                discarded: recovered.discarded,
            });
        }
        self.reobserve()?;
        if let History::Degraded(error) = &self.history {
            return Err(
                DbError::from_diag((*error.diagnostic).clone(), error.exit).with_help(
                    "no change can be recorded until history is repaired; see `reldir check`",
                ),
            );
        }
        // Anything valid that changed since opening is recorded before this
        // change, so the revision this change produces describes it alone.
        self.adopt("external")?;
        let before = self.verdict.errors.clone();
        let (rows, induced, changes) = plan(self)?;
        transaction::check_plan(&changes, self.config.max_transaction_size)?;
        let (writes, deletes) = overlay_parts(&self.root, &changes);
        let overlay = Overlay::new(&Disk, writes, deletes);
        let prospect = self.catalog.prospect(&self.config, &overlay)?;
        // Every fallible step between judging the change and committing it:
        // whatever happens, the prospect is then kept or abandoned.
        let committed = (|| -> Result<Option<String>> {
            admit(request.admission, &before, &prospect.verdict)?;
            if request.dry_run || changes.is_empty() {
                return Ok(None);
            }
            let id = uuid::Uuid::new_v4().to_string();
            transaction::journaled(&Disk, &self.root, &id, request.origin, &changes)?;
            Ok(Some(id))
        })();
        let id = match committed {
            Ok(Some(id)) => id,
            Ok(None) => {
                let prospective = prospect.abandon()?;
                drop(lock);
                return Ok(Outcome {
                    rows,
                    induced,
                    changes,
                    revision: None,
                    warnings: prospective.warnings,
                    dry_run: request.dry_run,
                });
            }
            Err(error) => {
                if let Err(rollback) = prospect.abandon() {
                    return Err(rollback.with_related(error.diagnostics()));
                }
                return Err(error);
            }
        };
        // The files now hold what the prospect observed, so its observation
        // is the new state; observing again would re-derive the same thing.
        (self.catalog, self.verdict) = prospect.keep()?;
        // A change that leaves the state history already has -- restoring what
        // an outside edit removed -- is not a new revision.
        let revision = if metadata::pending_changes(&self.catalog)?.is_empty() {
            self.history.head().map(|head| head.revision)
        } else {
            let record = metadata::record(&self.catalog, self.history.head(), request.origin, Some(&id), None)
                .map_err(|error| {
                    error.with_help(
                        "the change was written, but its revision could not be recorded; the next writing \
                         command records the state as an external change",
                    )
                })?;
            let revision = record.revision;
            self.history = History::Head(Box::new(record));
            Some(revision)
        };
        drop(lock);
        Ok(Outcome {
            rows,
            induced,
            changes,
            revision,
            warnings: self.verdict.warnings.clone(),
            dry_run: false,
        })
    }

    /// Observe the directory again, re-reading only what changed.
    pub fn reobserve(&mut self) -> Result<()> {
        let started = std::time::Instant::now();
        self.config = configured(&self.root, &self.overrides)?;
        let mirror = Rc::clone(&self.catalog.mirror);
        self.catalog = Catalog::observe(&self.root, &self.config, &Disk, mirror, false)?;
        self.verdict = integrity::validate(&self.catalog)?;
        if let History::Degraded(error) = &self.history {
            self.verdict.errors.push((*error.diagnostic).clone());
        }
        self.validation_elapsed = started.elapsed();
        Ok(())
    }

    /// Every row a plan read must still be what it read.
    fn confirm_rows_unmoved(&self, rows: &[RowChange]) -> Result<()> {
        for change in rows {
            if let Some(before) = &change.before {
                let now = self.catalog.row_at(&before.relative)?;
                let schema = self
                    .catalog
                    .schemas
                    .get(&change.table)
                    .ok_or_else(|| self.catalog.unknown_table(&change.table))?;
                let same = now.is_some_and(|row| {
                    crate::canonical::canonical_row(&row.value, schema)
                        == crate::canonical::canonical_row(&before.value, schema)
                });
                if !same {
                    return Err(moved(&before.relative));
                }
            }
        }
        Ok(())
    }

    /// Move recorded history aside and begin a new one from the current files.
    /// For history that cannot be verified: the old records are kept, whole,
    /// in `.db/provenance-quarantine/`, and the first new revision says where.
    pub fn begin_new_lineage(&mut self, reason: &str) -> Result<Provenance> {
        if self.access == Access::Read {
            return Err(DbError::new(
                "READ_ONLY",
                "a new lineage writes history; this command opened read-only",
                1,
            ));
        }
        require_safe_filesystem(&self.root, &self.config)?;
        let lock = acquire(&self.root, &self.config, crate::lock::Holder::Committing)?;
        transaction::recover(&Disk, &self.root)?;
        self.history = History::Empty;
        self.reobserve()?;
        if !self.verdict.errors.is_empty() {
            return Err(DbError::from_diag(self.verdict.errors[0].clone(), 2)
                .with_related(self.verdict.errors[1..].to_vec())
                .with_help("history can begin only from a valid state; repair the rows first with `reldir doctor`"));
        }
        let quarantined = metadata::quarantine_history(&self.root)?;
        self.catalog
            .mirror
            .replace_recorded(&BTreeMap::new(), None, None)?;
        let record = metadata::record(
            &self.catalog,
            None,
            "recovery",
            None,
            Some(metadata::Lineage {
                quarantined,
                reason: reason.to_string(),
            }),
        )?;
        self.history = History::Head(Box::new(record.clone()));
        drop(lock);
        Ok(record)
    }

    /// Remove what nothing needs: objects no revision references, and
    /// half-built snapshot copies. Returns what was (or, for a dry run, would
    /// be) removed.
    pub fn collect_garbage(&mut self, dry_run: bool) -> Result<Vec<PathBuf>> {
        if self.access == Access::Read {
            return Err(DbError::new(
                "READ_ONLY",
                "garbage collection deletes files; this command opened read-only",
                1,
            ));
        }
        let (History::Head(_) | History::Empty) = &self.history else {
            return Err(DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                "history cannot be verified, so which objects it needs is unknown; nothing was removed",
                6,
            ));
        };
        let lock = acquire(&self.root, &self.config, crate::lock::Holder::Committing)?;
        let mut referenced = BTreeSet::new();
        for revision in metadata::revisions(&self.root)? {
            for entry in metadata::load_record(&self.root, revision)?
                .changes
                .values()
                .flatten()
            {
                referenced.insert(format!("{}.json", entry.hash));
            }
        }
        let mut garbage = vec![];
        let objects = self.root.join(".db/objects");
        if objects.is_dir() {
            for entry in fs::read_dir(&objects).map_err(|error| DbError::io(&objects, error))? {
                let path = entry.map_err(|error| DbError::io(&objects, error))?.path();
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_string();
                if !referenced.contains(&name) {
                    garbage.push(path);
                }
            }
        }
        let snapshots = self.root.join(".db/snapshots");
        if snapshots.is_dir() {
            for entry in fs::read_dir(&snapshots).map_err(|error| DbError::io(&snapshots, error))? {
                let path = entry
                    .map_err(|error| DbError::io(&snapshots, error))?
                    .path();
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".creating-"))
                {
                    garbage.push(path);
                }
            }
        }
        garbage.sort();
        if !dry_run {
            for path in &garbage {
                let removed = if path.is_dir() {
                    fs::remove_dir_all(path)
                } else {
                    fs::remove_file(path)
                };
                removed.map_err(|error| DbError::io(path, error))?;
            }
            if let Some(first) = garbage.first() {
                metadata::sync_parent(first)?;
            }
        }
        drop(lock);
        Ok(garbage
            .into_iter()
            .map(|path| {
                path.strip_prefix(&self.root)
                    .map(Path::to_path_buf)
                    .unwrap_or(path)
            })
            .collect())
    }

    /// The schemas this database governs, by table.
    pub fn schemas(&self) -> &BTreeMap<String, Schema> {
        &self.catalog.schemas
    }

    /// A read's resource limits.
    pub fn query_limits(&self) -> crate::sql::QueryLimits {
        crate::sql::QueryLimits {
            timeout: self
                .config
                .timeout_seconds
                .map(std::time::Duration::from_secs),
            max_rows: self.config.max_result_rows,
            max_memory: self.config.max_query_memory,
        }
    }
}

fn moved(relative: &Path) -> DbError {
    DbError::from_diag(
        Diagnostic::error(
            "CONCURRENT_MODIFICATION",
            format!(
                "{} changed after the change was planned; nothing was written",
                relative.display()
            ),
        )
        .at(relative)
        .help("run the command again: it will be planned against the current files"),
        3,
    )
}

/// Decide whether a prospective state may be committed.
fn admit(admission: Admission, before: &[Diagnostic], prospective: &Verdict) -> Result<()> {
    let refused: Vec<Diagnostic> = match admission {
        Admission::Valid => prospective.errors.clone(),
        Admission::NoNewFaults => {
            let known: BTreeSet<(String, Option<PathBuf>, Option<String>)> = before
                .iter()
                .map(|d| (d.code.clone(), d.path.clone(), d.pointer.clone()))
                .collect();
            prospective
                .errors
                .iter()
                .filter(|d| !known.contains(&(d.code.clone(), d.path.clone(), d.pointer.clone())))
                .cloned()
                .collect()
        }
    };
    let Some(first) = refused.first() else {
        return Ok(());
    };
    let count = refused.len();
    let lead = Diagnostic {
        message: format!(
            "the change was refused because the database would be invalid ({count} fault{}); nothing \
             was written. First: {}",
            if count == 1 { "" } else { "s" },
            first.message
        ),
        severity: Severity::Error,
        ..first.clone()
    };
    Err(DbError::from_diag(lead, 2).with_related(refused[1..].to_vec()))
}

fn overlay_parts(
    root: &Path,
    changes: &[Change],
) -> (BTreeMap<PathBuf, Vec<u8>>, BTreeSet<PathBuf>) {
    let mut writes = BTreeMap::new();
    let mut deletes = BTreeSet::new();
    for change in changes {
        match change {
            Change::Write { path, bytes } => {
                writes.insert(root.join(path), bytes.clone());
            }
            Change::Delete { path } => {
                deletes.insert(root.join(path));
            }
        }
    }
    (writes, deletes)
}

/// `A path`, `M path`, `D path` for each pending change.
fn summarize(
    pending: &BTreeMap<String, Option<metadata::Entry>>,
    catalog: &Catalog,
) -> Result<Vec<String>> {
    let recorded = catalog.mirror.recorded_paths()?;
    Ok(pending
        .iter()
        .map(|(path, entry)| match (recorded.contains(path), entry) {
            (_, None) => format!("D {path}"),
            (false, Some(_)) => format!("A {path}"),
            (true, Some(_)) => format!("M {path}"),
        })
        .collect())
}

fn configured(root: &Path, overrides: &ResourceOverrides) -> Result<Config> {
    let mut config = load_config(root)?;
    config.apply_overrides(overrides);
    config.validate().map_err(|message| {
        DbError::new(
            "RESOURCE_LIMIT",
            format!("invalid command-line limit: {message}"),
            1,
        )
    })?;
    Ok(config)
}

fn acquire(root: &Path, config: &Config, holder: crate::lock::Holder) -> Result<std::fs::File> {
    let path = root.join(".db/lock");
    validate_optional_private_file(&path, "lock")?;
    crate::lock::acquire(&path, config.lock_budget(), holder)
}

/// Refuse to write where the commit protocol's two promises -- an exclusive
/// lock and an atomic rename -- cannot be verified, unless the configuration
/// says someone decided to accept that.
fn require_safe_filesystem(root: &Path, config: &Config) -> Result<()> {
    match probe::classify(root) {
        probe::Class::Remote(kind) if !config.allow_remote_filesystem => Err(DbError::from_diag(
            Diagnostic::error(
                "UNSAFE_FILESYSTEM",
                format!(
                    "{} is on a {kind} filesystem, where file locks and atomic renames are not \
                     guaranteed; writing could lose or interleave changes without any error",
                    root.display()
                ),
            )
            .help(
                "work on a local copy, or -- if this filesystem is known to honour locks and \
                 renames -- set \"allow_remote_filesystem\": true in .db/config",
            ),
            11,
        )),
        _ => Ok(()),
    }
}

/// Check `.db/format`: present, private, and this binary's version.
pub fn validate_format(root: &Path) -> Result<()> {
    let meta = root.join(".db");
    match fs::symlink_metadata(&meta) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(DbError::from_diag(
                Diagnostic::error(
                    "UNINITIALIZED",
                    format!("{} has no .db metadata", root.display()),
                )
                .help(format!("run `reldir init {}`", root.display())),
                10,
            ));
        }
        Err(error) => return Err(DbError::io(&meta, error)),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                ".db must be a real directory, not a symlink or special file",
                6,
            ));
        }
        Ok(_) => {}
    }
    let found = read_format(root)?;
    if found == FORMAT_VERSION {
        return Ok(());
    }
    Err(DbError::new(
        "FORMAT_UNSUPPORTED",
        format!("the database is format {found}; this binary reads only format {FORMAT_VERSION}"),
        6,
    ))
}

/// The format version `.db/format` declares.
pub fn read_format(root: &Path) -> Result<u32> {
    let path = root.join(".db/format");
    require_private_regular_file(&path, "format marker")?;
    let size = fs::symlink_metadata(&path)
        .map_err(|error| DbError::io(&path, error))?
        .len();
    if size > 4096 {
        return Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            ".db/format exceeds the 4096-byte format marker limit",
            6,
        ));
    }
    let text = fs::read_to_string(&path).map_err(|e| DbError::io(&path, e))?;
    text.lines()
        .find_map(|line| line.strip_prefix("format_version = "))
        .and_then(|value| value.trim().parse::<u32>().ok())
        .ok_or_else(|| {
            DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                ".db/format has no valid format_version",
                6,
            )
        })
}

pub fn load_config(root: &Path) -> Result<Config> {
    let path = root.join(".db/config");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(DbError::io(&path, error)),
        Ok(_) => {}
    }
    require_private_regular_file(&path, "configuration")?;
    let size = fs::symlink_metadata(&path)
        .map_err(|error| DbError::io(&path, error))?
        .len();
    if size > crate::config::BOOTSTRAP_MAX_CONFIG_SIZE {
        return Err(DbError::new(
            "CONFIG_INVALID",
            format!(
                ".db/config exceeds the {} byte bootstrap limit",
                crate::config::BOOTSTRAP_MAX_CONFIG_SIZE
            ),
            1,
        ));
    }
    let bytes = fs::read(&path).map_err(|e| DbError::io(&path, e))?;
    let invalid = |message: String| {
        DbError::from_diag(
            Diagnostic::error("CONFIG_INVALID", format!("invalid .db/config: {message}"))
                .at(".db/config"),
            1,
        )
    };
    let value = crate::json::with_depth_limit(crate::config::BOOTSTRAP_MAX_NESTING_DEPTH, || {
        crate::json::parse(&bytes).map_err(|error| invalid(error.to_string()))
    })?;
    let config: Config =
        serde_json::from_value(value).map_err(|error| invalid(error.to_string()))?;
    config.validate().map_err(invalid)?;
    Ok(config)
}

fn require_private_regular_file(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("cannot inspect {description} {}: {error}", path.display()),
            6,
        )
    })?;
    if !metadata.file_type().is_file() || crate::catalog::has_multiple_links(&metadata) {
        return Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!(
                "{description} {} must be a private regular file",
                path.display()
            ),
            6,
        ));
    }
    Ok(())
}

fn validate_optional_private_file(path: &Path, description: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => require_private_regular_file(path, description),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DbError::io(path, error)),
    }
}

/// The `.gitignore` that keeps derived state out of version control, and --
/// when provenance is tracked -- keeps history in it.
pub fn gitignore(track_provenance: bool) -> &'static str {
    if track_provenance {
        "*\n!.gitignore\n!format\n!config\n!provenance/\n!provenance/**\n!objects/\n!objects/**\n"
    } else {
        "*\n!.gitignore\n!format\n!config\n"
    }
}

/// Write the metadata layout of an empty database.
pub fn init_layout(root: &Path, track_provenance: bool) -> Result<()> {
    let meta = root.join(".db");
    if fs::symlink_metadata(&meta).is_ok() {
        return Err(DbError::new(
            "ALREADY_INITIALIZED",
            format!("{} is already initialized", root.display()),
            1,
        ));
    }
    fs::create_dir_all(root).map_err(|e| DbError::io(root, e))?;
    fs::create_dir(&meta).map_err(|e| DbError::io(&meta, e))?;
    // `schema/` is the user's pin directory and appears only when they pin
    // something; reldir's working schemas live in `.db/schema`.
    for directory in [
        "provenance",
        "objects",
        "transactions",
        "snapshots",
        "schema",
    ] {
        let path = meta.join(directory);
        fs::create_dir(&path).map_err(|e| DbError::io(&path, e))?;
    }
    metadata::write_bytes_atomic(&meta.join("config"), &{
        let mut bytes = serde_json::to_vec_pretty(&Config::default())
            .map_err(|error| DbError::new("INTERNAL_METADATA_CORRUPT", error.to_string(), 6))?;
        bytes.push(b'\n');
        bytes
    })?;
    metadata::write_bytes_atomic(
        &meta.join(".gitignore"),
        gitignore(track_provenance).as_bytes(),
    )?;
    // The format marker is written last: a `.db/` without one is a layout
    // that was never finished, which establishment recognises and rebuilds.
    metadata::write_bytes_atomic(
        &meta.join("format"),
        format!("format_version = {FORMAT_VERSION}\n").as_bytes(),
    )?;
    metadata::sync_parent(&meta)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn users_schema() -> String {
        format!(
            r#"{{"$schema":"{}","type":"object","properties":{{"id":{{"type":"string"}},"name":{{"type":"string"}}}},"required":["id","name"],"additionalProperties":false,"x-reldir":{{"table":"users","primaryKey":["id"]}}}}"#,
            crate::schema::meta::DIALECT_URI
        )
    }

    fn database() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        init_layout(directory.path(), false).unwrap();
        write(
            &directory.path().join(".db/schema/users.json"),
            &users_schema(),
        );
        write(
            &directory.path().join("users/u1.json"),
            "{\"id\":\"u1\",\"name\":\"A\"}\n",
        );
        directory
    }

    #[test]
    fn test1023_every_other_format_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        init_layout(directory.path(), false).unwrap();
        validate_format(directory.path()).unwrap();
        for other in [FORMAT_VERSION - 1, FORMAT_VERSION + 1] {
            fs::write(
                directory.path().join(".db/format"),
                format!("format_version = {other}\n"),
            )
            .unwrap();
            assert_eq!(
                validate_format(directory.path())
                    .unwrap_err()
                    .diagnostic
                    .code,
                "FORMAT_UNSUPPORTED"
            );
        }
        fs::write(directory.path().join(".db/format"), "garbage\n").unwrap();
        assert_eq!(
            validate_format(directory.path())
                .unwrap_err()
                .diagnostic
                .code,
            "INTERNAL_METADATA_CORRUPT"
        );
    }

    #[test]
    fn test1024_initialisation_writes_the_documented_layout() {
        let directory = tempfile::tempdir().unwrap();
        init_layout(directory.path(), false).unwrap();
        for expected in [
            ".db/format",
            ".db/config",
            ".db/.gitignore",
            ".db/schema",
            ".db/provenance",
            ".db/objects",
        ] {
            assert!(directory.path().join(expected).exists(), "{expected}");
        }
        assert!(
            !directory.path().join("schema").exists(),
            "initialisation declares nothing"
        );
        assert!(load_config(directory.path()).unwrap().validate().is_ok());
        assert_eq!(
            init_layout(directory.path(), false)
                .unwrap_err()
                .diagnostic
                .code,
            "ALREADY_INITIALIZED"
        );
    }

    #[test]
    fn test1025_tracked_provenance_keeps_history_under_version_control() {
        assert!(!gitignore(false).contains("provenance"));
        assert!(
            gitignore(true).contains("!provenance/**") && gitignore(true).contains("!objects/**")
        );
    }

    #[test]
    fn test2160_opening_for_write_records_a_valid_external_change_once() {
        let directory = database();
        let first = Database::open(
            directory.path().to_path_buf(),
            Access::Write,
            &Default::default(),
        )
        .unwrap();
        assert!(first.is_valid(), "{:?}", first.verdict.errors);
        let revision = first.history.head().unwrap().revision;
        drop(first);
        let again = Database::open(
            directory.path().to_path_buf(),
            Access::Write,
            &Default::default(),
        )
        .unwrap();
        assert_eq!(
            again.history.head().unwrap().revision,
            revision,
            "an unchanged state is not re-recorded"
        );
        write(
            &directory.path().join("users/u2.json"),
            "{\"id\":\"u2\",\"name\":\"B\"}\n",
        );
        let changed = Database::open(
            directory.path().to_path_buf(),
            Access::Write,
            &Default::default(),
        )
        .unwrap();
        assert_eq!(changed.history.head().unwrap().revision, revision + 1);
        assert!(changed.events.iter().any(|e| matches!(e, Event::Recorded { changes, .. } if changes == &vec!["A users/u2.json".to_string()])));
    }

    #[test]
    fn test2161_read_access_writes_nothing_and_says_what_it_did_not_record() {
        let directory = database();
        drop(
            Database::open(
                directory.path().to_path_buf(),
                Access::Write,
                &Default::default(),
            )
            .unwrap(),
        );
        write(
            &directory.path().join("users/u2.json"),
            "{\"id\":\"u2\",\"name\":\"B\"}\n",
        );
        let before = metadata::revisions(directory.path()).unwrap();
        let mirror_before = fs::read(crate::mirror::path(directory.path())).unwrap();
        let reader = Database::open(
            directory.path().to_path_buf(),
            Access::Read,
            &Default::default(),
        )
        .unwrap();
        assert_eq!(metadata::revisions(directory.path()).unwrap(), before);
        assert_eq!(
            fs::read(crate::mirror::path(directory.path())).unwrap(),
            mirror_before,
            "not even derived state"
        );
        assert!(
            reader
                .verdict
                .warnings
                .iter()
                .any(|w| w.code == "METADATA_STALE_READONLY")
        );
        assert_eq!(
            reader.catalog.row_count().unwrap(),
            2,
            "the reader still sees the new row"
        );
    }

    #[test]
    fn test2162_a_change_that_would_invalidate_the_database_writes_nothing() {
        let directory = database();
        let mut database = Database::open(
            directory.path().to_path_buf(),
            Access::Write,
            &Default::default(),
        )
        .unwrap();
        let revision = database.history.head().unwrap().revision;
        let bad = vec![Change::Write {
            path: "users/u9.json".into(),
            bytes: b"{\"id\":\"u9\"}\n".to_vec(),
        }];
        let error = database
            .commit(bad, &Expected::new(), Request::internal(false))
            .unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(
            error.diagnostic.message.contains("nothing was written"),
            "{}",
            error.diagnostic.message
        );
        assert!(!directory.path().join("users/u9.json").exists());
        assert_eq!(
            metadata::head(directory.path()).unwrap().unwrap().revision,
            revision
        );
    }

    #[test]
    fn test2163_a_commit_refuses_when_a_planned_path_moved() {
        let directory = database();
        let mut database = Database::open(
            directory.path().to_path_buf(),
            Access::Write,
            &Default::default(),
        )
        .unwrap();
        let expected = Expected::from([(
            PathBuf::from("users/u1.json"),
            database.fingerprint(Path::new("users/u1.json")).unwrap(),
        )]);
        write(
            &directory.path().join("users/u1.json"),
            "{\"id\":\"u1\",\"name\":\"changed\"}\n",
        );
        let change = vec![Change::Write {
            path: "users/u1.json".into(),
            bytes: b"{\"id\":\"u1\",\"name\":\"mine\"}\n".to_vec(),
        }];
        let error = database
            .commit(change, &expected, Request::internal(false))
            .unwrap_err();
        assert_eq!(error.diagnostic.code, "CONCURRENT_MODIFICATION");
        assert_eq!(error.exit_code(), 3);
        assert!(
            fs::read_to_string(directory.path().join("users/u1.json"))
                .unwrap()
                .contains("changed")
        );
    }

    #[test]
    fn test2164_a_dry_run_judges_without_writing() {
        let directory = database();
        let mut database = Database::open(
            directory.path().to_path_buf(),
            Access::Write,
            &Default::default(),
        )
        .unwrap();
        let change = vec![Change::Write {
            path: "users/u2.json".into(),
            bytes: b"{\"id\":\"u2\",\"name\":\"B\"}\n".to_vec(),
        }];
        let outcome = database
            .commit(change, &Expected::new(), Request::internal(true))
            .unwrap();
        assert!(outcome.dry_run && outcome.revision.is_none());
        assert!(!directory.path().join("users/u2.json").exists());
    }

    #[test]
    fn test2165_read_access_refuses_to_commit() {
        let directory = database();
        drop(
            Database::open(
                directory.path().to_path_buf(),
                Access::Write,
                &Default::default(),
            )
            .unwrap(),
        );
        let mut reader = Database::open(
            directory.path().to_path_buf(),
            Access::Read,
            &Default::default(),
        )
        .unwrap();
        let error = reader
            .commit(vec![], &Expected::new(), Request::internal(false))
            .unwrap_err();
        assert_eq!(error.diagnostic.code, "READ_ONLY");
    }

    #[test]
    fn test2166_repairs_may_leave_old_faults_but_never_add_one() {
        let before = vec![
            Diagnostic::error("FOREIGN_KEY_VIOLATION", "m")
                .at("a.json")
                .pointer("/x"),
        ];
        let same = Verdict {
            errors: before.clone(),
            warnings: vec![],
        };
        admit(Admission::NoNewFaults, &before, &same).unwrap();
        assert!(admit(Admission::Valid, &before, &same).is_err());
        let worse = Verdict {
            errors: vec![
                before[0].clone(),
                Diagnostic::error("TYPE_MISMATCH", "m")
                    .at("b.json")
                    .pointer(""),
            ],
            warnings: vec![],
        };
        let error = admit(Admission::NoNewFaults, &before, &worse).unwrap_err();
        assert_eq!(error.diagnostic.code, "TYPE_MISMATCH");
    }
}
