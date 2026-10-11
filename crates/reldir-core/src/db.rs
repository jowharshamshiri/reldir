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
//! 5. make the revision that records it;
//! 6. write the files and the revision together, through the journal.
//!
//! Nothing reaches the disk unless step 4 admits it, so no command can leave a
//! valid database invalid; and history is written by the same transaction as
//! the rows it records, so the two never disagree.

use crate::code::Code;
use crate::{
    FORMAT_VERSION,
    catalog::Catalog,
    config::{Config, ResourceOverrides},
    diagnostic::{DbError, Diagnostic, Result, Severity},
    engine::fs::{Disk, Overlay},
    engine::journal,
    history::{self, History},
    integrity::{self, Verdict},
    mirror::Mirror,
    plan::{FileChange, RowChange},
    referential::Induced,
    schema::Schema,
    store::{
        Id,
        revision::{Lineage, Origin, Revision},
    },
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
    /// Take no lock and write nothing, not even derived state: `--readonly`.
    /// A change committed and not yet finished on disk is seen as made.
    Read,
    /// Answer a read without waiting for a writer. When the writer lock is
    /// free it is taken just long enough to observe and record, and the read
    /// then runs on the state so observed, holding no lock; when a writer
    /// holds it, the read answers from the state as of that writer's last
    /// commit.
    Observe,
    /// Take the writer lock on opening and hold it to the end: recover,
    /// observe once, record accepted external changes, and commit.
    Write,
}

/// A revision as a person and a program refer to it: this clone's number, and
/// the id every clone shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevisionName {
    pub seq: u64,
    pub id: Id,
}

impl std::fmt::Display for RevisionName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{} ({})", self.seq, &self.id.to_hex()[..12])
    }
}

impl RevisionName {
    pub fn to_json(&self) -> Value {
        json!({"seq": self.seq, "id": self.id.to_hex()})
    }
}

/// What recorded history is, as far as this open could tell.
#[derive(Debug)]
pub enum Head {
    /// Nothing has been recorded yet.
    Empty,
    /// One latest revision, verified.
    One {
        name: RevisionName,
        revision: Box<Revision>,
    },
    /// History has several heads -- histories kept in version control were
    /// merged. The next recording joins them in a merge revision.
    Diverged(Vec<Id>),
    /// History cannot be read or does not verify. Reads still work; nothing can
    /// be recorded until it is repaired.
    Degraded(Box<DbError>),
}

impl Head {
    pub fn name(&self) -> Option<RevisionName> {
        match self {
            Self::One { name, .. } => Some(*name),
            _ => None,
        }
    }

    pub fn revision(&self) -> Option<&Revision> {
        match self {
            Self::One { revision, .. } => Some(revision),
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
        /// Files an outside editor changed after a crash, kept as edited.
        kept_outside_edits: Vec<String>,
    },
    /// A valid state that differed from history was recorded.
    Recorded {
        revision: RevisionName,
        origin: Origin,
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
                kept_outside_edits,
            } => {
                let mut text = format!(
                    "recovered interrupted transactions: {} finished, {} discarded",
                    completed.len(),
                    discarded.len()
                );
                if !kept_outside_edits.is_empty() {
                    text.push_str(&format!(
                        "; kept outside edits made after the crash to {}",
                        kept_outside_edits.join(", ")
                    ));
                }
                text
            }
            Self::Recorded {
                revision,
                origin,
                changes,
            } => format!(
                "recorded revision {revision} ({}): {} change(s)",
                origin.as_str(),
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
                kept_outside_edits,
            } => json!({"kind": "recovered", "completed": completed, "discarded": discarded,
                        "kept_outside_edits": kept_outside_edits}),
            Self::Recorded {
                revision,
                origin,
                changes,
            } => json!({"kind": "recorded", "revision": revision.to_json(),
                        "origin": origin.as_str(), "changes": changes}),
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
    /// The state with this identity, as a snapshot recorded it: a snapshot
    /// of a database with faults is put back with exactly those faults.
    /// Any other result must be valid.
    Restores(crate::store::Hash256),
}

/// How a change is to be committed.
#[derive(Debug, Clone)]
pub struct Request {
    /// How the change came about, as history records it.
    pub origin: Origin,
    pub admission: Admission,
    /// Judge and report the change without writing it.
    pub dry_run: bool,
    /// The caller's name for this change. A change whose key history already
    /// holds is not made again: its revision is reported instead, so retrying
    /// after a failure whose outcome was unknown is always safe.
    pub idempotency_key: Option<String>,
    /// Widen a table's contract, in this same change, to admit a value the
    /// change writes that it has never admitted. Without it such a change is
    /// refused.
    pub widen: bool,
    /// The state, as [`Database::state_token`] names it, the change was
    /// planned against: committing to any other is refused (PLAN_CHANGED).
    pub expect_state: Option<String>,
}

impl Request {
    pub fn new(origin: Origin, dry_run: bool) -> Self {
        Self {
            origin,
            admission: Admission::Valid,
            dry_run,
            idempotency_key: None,
            widen: false,
            expect_state: None,
        }
    }

    pub fn internal(dry_run: bool) -> Self {
        Self::new(Origin::Internal, dry_run)
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
    pub changes: Vec<FileChange>,
    /// The revision recorded, when something was written and recorded.
    pub revision: Option<RevisionName>,
    /// Whether the change was answered by an earlier revision carrying the
    /// same idempotency key, and so not made again.
    pub replayed: bool,
    /// Files an outside editor changed between planning and placing, kept as
    /// edited: the change is committed, and these edits are recorded next.
    pub kept_outside_edits: Vec<PathBuf>,
    /// Warnings the resulting state carries.
    pub warnings: Vec<Diagnostic>,
    /// Each table whose contract the change widened, as it was asked to, and
    /// the edits that did it.
    pub widened: BTreeMap<String, Vec<crate::schema::widen::Widened>>,
    pub dry_run: bool,
    /// The state the change was planned against, as
    /// [`Database::state_token`] names it; `None` for a replayed change.
    pub planned_against: Option<String>,
}

/// A commit as made: what the journal placed, and the recording it carries.
type Committing = (
    journal::Committed,
    Option<(history::Delta, history::Prepared)>,
);

/// The raw-bytes hash each path had when a change was planned, `None` for a
/// path that did not exist. A commit refuses if any of them moved.
pub type Expected = BTreeMap<PathBuf, Option<String>>;

pub struct Database {
    pub root: PathBuf,
    pub config: Config,
    pub catalog: Catalog,
    pub verdict: Verdict,
    /// Recorded history, when it could be opened.
    pub history: Option<History>,
    pub head: Head,
    /// What opening did on its own.
    pub events: Vec<Event>,
    /// Paths whose state differs from history and was not recorded.
    pub unrecorded: Vec<String>,
    /// What a command must say about how it answers, whatever it answers:
    /// a read from a snapshot, say. Not about validity, so not in the
    /// verdict.
    pub notices: Vec<Diagnostic>,
    pub validation_elapsed: std::time::Duration,
    access: Access,
    /// The writer lock, held from opening for write until the database is
    /// dropped: what was observed stays true until the change is committed.
    lock: Option<crate::engine::fs::LockGuard>,
    overrides: ResourceOverrides,
    /// Set when the answer is no longer wanted: a running query stops.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl Database {
    /// Open a database that has `.db/`.
    pub fn open(root: PathBuf, access: Access, overrides: &ResourceOverrides) -> Result<Self> {
        Self::open_as(root, access, overrides, Origin::External)
    }

    /// Open a database `reldir upgrade` has just made format 3, recording
    /// what the upgrade changed -- the format, converted schemas, rows format
    /// 2 had normalized -- as the upgrade's own revision.
    pub fn open_upgraded(root: PathBuf, overrides: &ResourceOverrides) -> Result<Self> {
        Self::open_as(root, Access::Write, overrides, Origin::Upgrade)
    }

    /// Establish a database over a folder: write the layout and the schemas,
    /// then open it, recording what was adopted as an import.
    pub fn create(
        root: PathBuf,
        schemas: &BTreeMap<String, Schema>,
        track_provenance: bool,
        config: &Config,
        overrides: &ResourceOverrides,
    ) -> Result<Self> {
        init_layout(&root, track_provenance, config)?;
        for schema in schemas.values() {
            let path = crate::schema_store::working_path(&root, schema.table());
            crate::engine::fs::replace_durably(
                &crate::engine::fs::Disk,
                &path,
                &schema.render(None)?,
            )
            .map_err(|error| DbError::io(&path, error))?;
        }
        // The first revision records what adoption inferred: the folder's
        // contract, as it was when reldir first saw it.
        Self::open_as(root, Access::Write, overrides, Origin::Inference)
    }

    /// Answer reads over a folder that has no `.db/`, using schemas and
    /// settings that are never written. Nothing is persisted and nothing is
    /// recorded: this is an observation, not a revision.
    pub fn ephemeral(
        root: PathBuf,
        schemas: BTreeMap<String, Schema>,
        settings: &Config,
        overrides: &ResourceOverrides,
    ) -> Result<Self> {
        let started = std::time::Instant::now();
        let mut config = settings.clone();
        config.apply_overrides(overrides);
        config.validate().map_err(|message| {
            DbError::new(
                Code::RESOURCE_LIMIT,
                format!("invalid command-line limit: {message}"),
            )
        })?;
        let catalog = Catalog::observe_with_schemas(&root, &config, &Disk, schemas)?;
        let verdict = integrity::validate(&catalog)?;
        Ok(Self {
            root,
            config,
            catalog,
            verdict,
            history: None,
            head: Head::Empty,
            events: vec![],
            unrecorded: vec![],
            notices: vec![],
            validation_elapsed: started.elapsed(),
            access: Access::Read,
            lock: None,
            overrides: overrides.clone(),
            cancel: None,
        })
    }

    fn open_as(
        root: PathBuf,
        access: Access,
        overrides: &ResourceOverrides,
        origin: Origin,
    ) -> Result<Self> {
        let started = std::time::Instant::now();
        validate_format(&root)?;
        let mut events = vec![];
        let waiting = configured(&root, overrides)?;
        let lock = match access {
            Access::Write => {
                require_safe_filesystem(&root, &waiting)?;
                Some(acquire(&root, &waiting, crate::lock::Holder::Observing)?)
            }
            Access::Observe => {
                require_safe_filesystem(&root, &waiting)?;
                let immediately = Config {
                    wait_seconds: 0.0,
                    ..waiting.clone()
                };
                match acquire(&root, &immediately, crate::lock::Holder::Observing) {
                    Ok(lock) => Some(lock),
                    Err(error) if error.code() == Code::LOCK_CONTENDED => {
                        if let Some(database) = Self::open_snapshot(&root, overrides)? {
                            return Ok(database);
                        }
                        // There is no committed state to answer from -- the
                        // writer is building the mirror -- so wait for it.
                        Some(acquire(&root, &waiting, crate::lock::Holder::Observing)?)
                    }
                    Err(error) => return Err(error),
                }
            }
            Access::Read => None,
        };
        // Committed changes not yet finished on disk: a writer finishes them;
        // a reader that may write nothing sees them as finished.
        let (mirror, source): (Mirror, Box<dyn crate::engine::fs::Source>) = match &lock {
            Some(_) => {
                let recovered = recover(&root)?;
                let interrupted = recovered.is_some();
                events.extend(recovered);
                let (mirror, rebuilt) = Mirror::open_persistent(&root, interrupted)?;
                if rebuilt {
                    events.push(Event::MirrorRebuilt);
                }
                (mirror, Box::new(Disk))
            }
            None => (
                Mirror::open_ephemeral(&root)?,
                Box::new(committed_view(&root)?),
            ),
        };
        let config = configured(&root, overrides)?;
        let catalog = Catalog::observe(
            &root,
            &config,
            source.as_ref(),
            Rc::new(mirror),
            events.iter().any(|e| matches!(e, Event::MirrorRebuilt)),
        )?;
        let mut verdict = match catalog.earlier_verdict(&config)? {
            Some(verdict) => verdict,
            None => {
                let verdict = integrity::validate(&catalog)?;
                if lock.is_some() {
                    catalog.keep_verdict(&config, &verdict)?;
                }
                verdict
            }
        };
        let (history, head) = load_history(&root, &catalog);
        match &head {
            Head::Degraded(error) => verdict.errors.push(
                (*error.diagnostic.clone()).help(
                    "recorded history cannot be verified. Restore .db/objects and .db/history from \
                     a backup, or run `reldir recover --history=new-lineage --allow-destructive` to \
                     move it aside and begin a new history from the current files",
                ),
            ),
            Head::Diverged(heads) => verdict.warnings.push(
                Diagnostic::warning(
                    Code::HISTORY_DIVERGED,
                    format!(
                        "history has {} heads -- histories kept in version control were merged; the \
                         next recording joins them",
                        heads.len()
                    ),
                )
                .at(".db/history/heads"),
            ),
            Head::Empty | Head::One { .. } => {}
        }
        let writes = lock.is_some();
        let mut database = Self {
            root,
            config,
            catalog,
            verdict,
            history,
            head,
            events,
            unrecorded: vec![],
            notices: vec![],
            validation_elapsed: std::time::Duration::ZERO,
            access,
            lock,
            overrides: overrides.clone(),
            cancel: None,
        };
        if writes {
            database.restore_contracts()?;
            database.remove_superseded()?;
            database.catalog.mirror.analyze_drifted()?;
        }
        database.adopt(origin)?;
        database.confirm_restorations()?;
        database.validation_elapsed = started.elapsed();
        if access == Access::Observe {
            // The read runs on what was just observed, as one snapshot, and no
            // longer holds up any writer.
            database.catalog.mirror.hold_snapshot()?;
            database.lock = None;
        }
        Ok(database)
    }

    /// A read answered from the mirror's last committed state, for when a
    /// writer holds the lock. `None` when the mirror holds no committed
    /// observation to answer from.
    fn open_snapshot(root: &Path, overrides: &ResourceOverrides) -> Result<Option<Self>> {
        let started = std::time::Instant::now();
        let Some(mirror) = Mirror::open_snapshot(root)? else {
            return Ok(None);
        };
        let mirror = Rc::new(mirror);
        let catalog = match Catalog::from_snapshot(root, Rc::clone(&mirror)) {
            Ok(catalog) => catalog,
            Err(error) if error.code() == Code::INTERNAL_METADATA_CORRUPT => return Ok(None),
            Err(error) => return Err(error),
        };
        let config = configured(root, overrides)?;
        let verdict = integrity::validate(&catalog)?;
        let history = History::open(root).ok();
        let head = match (&history, mirror.meta("recorded_head")?) {
            (Some(history), Some(text)) => match Id::from_hex(&text) {
                Some(id) => {
                    let revision = history.revision(&id)?;
                    let numbers = history::number(&catalog, history, false)?;
                    Head::One {
                        name: RevisionName {
                            seq: numbers.get(&id).copied().unwrap_or(0),
                            id,
                        },
                        revision: Box::new(revision),
                    }
                }
                None => Head::Empty,
            },
            _ => Head::Empty,
        };
        let notice = Diagnostic::warning(
            Code::SNAPSHOT_READ,
            "a writer is committing, so this answers from the database as of its last commit; \
             files changed outside reldir since then are not reflected",
        )
        .help("read again once the writer finishes, or wait for it with a writing command");
        let mut database = Self {
            root: root.to_path_buf(),
            config,
            catalog,
            verdict,
            history,
            head,
            events: vec![],
            unrecorded: vec![],
            notices: vec![notice],
            validation_elapsed: started.elapsed(),
            access: Access::Read,
            lock: None,
            overrides: overrides.clone(),
            cancel: None,
        };
        database.confirm_restorations()?;
        Ok(Some(database))
    }

    /// Hold the writer lock for a change: the lock held since opening, or
    /// taken now -- finishing interrupted transactions and observing the
    /// directory again, since another writer may have changed it. The guard
    /// returned, when the lock was taken now, releases it when dropped.
    /// A name for the state this database observed: its recorded heads, and
    /// every format, setting, schema and row that differs from them. Two
    /// observations with the same token hold the same state -- the same
    /// rows, schemas and settings, whatever their files' formatting -- so a
    /// plan or a page of results can be held to the state it was made from.
    pub fn state_token(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"reldir-observed-state-v1\0");
        match &self.head {
            Head::Empty => hasher.update(b"empty\0"),
            Head::One { name, .. } => {
                hasher.update(b"one\0");
                hasher.update(name.id.to_hex().as_bytes());
            }
            Head::Diverged(ids) => {
                hasher.update(b"diverged\0");
                let mut ids: Vec<String> = ids.iter().map(|id| id.to_hex()).collect();
                ids.sort();
                for id in ids {
                    hasher.update(id.as_bytes());
                }
            }
            Head::Degraded(_) => hasher.update(b"degraded\0"),
        }
        let (delta, _) = history::pending(&self.catalog)?;
        for (path, entry) in history::recorded_changes(&delta) {
            hasher.update([0]);
            hasher.update(path.as_bytes());
            match entry {
                Some(entry) => {
                    hasher.update([1]);
                    hasher.update(entry.kind.as_bytes());
                    hasher.update([0]);
                    hasher.update(entry.hash.as_bytes());
                }
                None => hasher.update([2]),
            }
        }
        let digest = hasher.finalize();
        Ok(digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    }

    /// The lock a change is planned under. A dry run of a database opened
    /// to read -- a folder a dry run may not adopt -- is planned against
    /// what was observed, as it writes nothing for another writer to race.
    pub(crate) fn planning_lock(
        &mut self,
        dry_run: bool,
    ) -> Result<Option<crate::engine::fs::LockGuard>> {
        if dry_run && self.access != Access::Write {
            return Ok(None);
        }
        self.writer_lock()
    }

    pub(crate) fn writer_lock(&mut self) -> Result<Option<crate::engine::fs::LockGuard>> {
        if self.access != Access::Write {
            return Err(DbError::new(
                Code::READ_ONLY,
                "the change was not made: this command opened the database to read",
            ));
        }
        if self.lock.is_some() {
            return Ok(None);
        }
        require_safe_filesystem(&self.root, &self.config)?;
        let lock = acquire(&self.root, &self.config, crate::lock::Holder::Committing)?;
        if let Some(event) = recover(&self.root)? {
            self.events.push(event);
        }
        self.reobserve()?;
        Ok(Some(lock))
    }

    /// Put back each inferred contract whose file is gone while its table's
    /// rows remain, exactly as recorded history holds it. Inferring it again
    /// from files outside programs may have changed since would move the
    /// contract they rely on; and recording the state without it would
    /// record the table as dropped. The caller holds the lock.
    fn restore_contracts(&mut self) -> Result<()> {
        if self.catalog.ungoverned.is_empty() {
            return Ok(());
        }
        let Head::One { revision, .. } = &self.head else {
            return Ok(());
        };
        let tree = revision.tree;
        let history = self.history()?;
        let extras: BTreeMap<String, crate::store::Id> =
            history.extras(&tree)?.into_iter().collect();
        let mut restored = vec![];
        for table in &self.catalog.ungoverned {
            let home = crate::catalog::slash(&crate::schema_store::working_relative(table));
            let Some(blob) = extras.get(&home) else {
                continue;
            };
            let bytes = history.blob(blob)?;
            let path = self.root.join(&home);
            crate::engine::fs::ensure_dir(
                &crate::engine::fs::Disk,
                &crate::schema_store::working_dir(&self.root),
            )
            .map_err(|error| DbError::io(&path, error))?;
            crate::engine::fs::replace_durably(&crate::engine::fs::Disk, &path, &bytes)
                .map_err(|error| DbError::io(&path, error))?;
            restored.push((table.clone(), home));
        }
        if restored.is_empty() {
            return Ok(());
        }
        self.reobserve()?;
        for (table, home) in restored {
            self.verdict.warnings.push(
                Diagnostic::warning(
                    Code::CONTRACT_REDERIVED,
                    format!(
                        "the inferred contract of {table} was missing; it was restored from \
                         recorded history"
                    ),
                )
                .table(table.as_str())
                .at(home)
                .help(
                    "keep .db/schema/ under version control with the rows; to stop governing \
                     the directory, add it to the ignore list in .db/config",
                ),
            );
        }
        Ok(())
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
        let parent = crate::schema_store::working_dir(&self.root);
        crate::engine::fs::Fs::sync_dir(&crate::engine::fs::Disk, &parent)
            .map_err(|error| DbError::io(&parent, error))?;
        self.events.push(Event::SupersededRemoved(superseded));
        Ok(())
    }

    /// Record the observed state when it is valid and history does not have
    /// it; with read access, say that it was not recorded.
    fn adopt(&mut self, origin: Origin) -> Result<()> {
        if matches!(self.head, Head::Degraded(_)) || self.history.is_none() {
            return Ok(());
        }
        let (delta, summary) = history::pending(&self.catalog)?;
        if delta.is_empty() && !matches!(self.head, Head::Diverged(_)) {
            return Ok(());
        }
        if !self.verdict.errors.is_empty() {
            self.unrecorded = summary;
            return Ok(());
        }
        match self.lock {
            Some(_) => {
                let recovered = self
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::Recovered { .. }));
                let origin = if recovered && origin == Origin::External {
                    Origin::Recovery
                } else {
                    origin
                };
                let name = self.record(&delta, origin, None, None)?;
                // Joining merged heads is recorded as a merge, whatever
                // observed it: report what history holds.
                let origin = match &self.head {
                    Head::One { revision, .. } => revision.origin,
                    _ => origin,
                };
                self.events.push(Event::Recorded {
                    revision: name,
                    origin,
                    changes: summary,
                });
            }
            // Read-only access records nothing; the state says what is
            // unrecorded (VALID_UNRECORDED), and any writing command records it.
            None => self.unrecorded = summary,
        }
        Ok(())
    }

    /// Record the observed state as a revision on its own -- an outside edit,
    /// or the first state of a new lineage. The caller holds the lock.
    fn record(
        &mut self,
        delta: &history::Delta,
        origin: Origin,
        lineage: Option<Lineage>,
        key: Option<&str>,
    ) -> Result<RevisionName> {
        let history = self.history.as_ref().ok_or_else(|| {
            DbError::new(Code::INTERNAL_ERROR, "there is no history to record in")
        })?;
        let catalog = &self.catalog;
        let root = self.root.clone();
        let prepared = history.prepare(
            delta,
            &mut || history::live_state(catalog),
            &mut |path| read_recorded(&root, catalog, path),
            history::Recording {
                origin,
                transaction_id: None,
                idempotency_key: key,
                lineage,
                imported: None,
            },
        )?;
        let plan = journal::Plan {
            origin: prepared.revision.origin.as_str().into(),
            revision: Some(prepared.id.to_hex()),
            idempotency_key: key.map(String::from),
            artifacts: prepared.artifacts.clone(),
            retire: prepared.retire.clone(),
            ..journal::Plan::default()
        };
        journal::preflight(
            &crate::engine::fs::Disk,
            &self.root,
            &plan,
            self.config.max_transaction_size,
        )?;
        journal::commit(
            &crate::engine::fs::Disk,
            &self.root,
            &plan,
            &mut journal::Unobserved,
        )?;
        self.after_recording(delta, &prepared)
    }

    /// Bring history and the mirror's copy of the head up to a revision this
    /// process just published.
    fn after_recording(
        &mut self,
        delta: &history::Delta,
        prepared: &history::Prepared,
    ) -> Result<RevisionName> {
        let history = self.history.as_mut().ok_or_else(|| {
            DbError::new(Code::INTERNAL_ERROR, "there is no history to record in")
        })?;
        history.refresh()?;
        match &self.head {
            Head::One { .. } => self.catalog.mirror.apply_recorded(
                &history::recorded_changes(delta),
                &prepared.id,
                &prepared.revision.tree,
            )?,
            _ => history::sync_recorded(
                &self.catalog,
                history,
                Some((&prepared.id, &prepared.revision)),
            )?,
        }
        let numbers = history::number(&self.catalog, history, true)?;
        let name = RevisionName {
            seq: numbers[&prepared.id],
            id: prepared.id,
        };
        self.head = Head::One {
            name,
            revision: Box::new(prepared.revision.clone()),
        };
        // A merged history's heads are joined now.
        self.verdict
            .warnings
            .retain(|warning| warning.code != Code::HISTORY_DIVERGED);
        Ok(name)
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
            Some(first) => {
                Err(DbError::from_diag(first.clone())
                    .with_related(self.verdict.errors[1..].to_vec()))
            }
        }
    }

    /// The history, or the reason it cannot be used.
    pub fn history(&self) -> Result<&History> {
        match (&self.history, &self.head) {
            (_, Head::Degraded(error)) => Err(DbError::from_diag((*error.diagnostic).clone())),
            (Some(history), _) => Ok(history),
            (None, _) => Err(DbError::new(
                Code::UNINITIALIZED,
                "this folder has no recorded history",
            )),
        }
    }

    /// The identity of the state a prospect describes, `changes` being the
    /// files it writes: what a snapshot or a revision names it by.
    fn identity_of(
        &self,
        prospect: &Catalog,
        changes: &[FileChange],
    ) -> Result<crate::store::Hash256> {
        let written: BTreeMap<String, &Vec<u8>> = changes
            .iter()
            .filter_map(|change| match change {
                FileChange::Write { path, bytes } => Some((crate::catalog::slash(path), bytes)),
                FileChange::Delete { .. } => None,
            })
            .collect();
        let (built, _) = self.history()?.build(
            &history::live_state(prospect)?,
            &[],
            &mut |path| match written.get(path) {
                Some(bytes) => Ok((*bytes).clone()),
                None => read_recorded(&self.root, prospect, path),
            },
        )?;
        Ok(built.identity)
    }

    /// This clone's number for every revision history holds.
    pub fn revision_numbers(&self) -> Result<BTreeMap<Id, u64>> {
        history::number(&self.catalog, self.history()?, self.lock.is_some())
    }

    /// What a path held when the database was last observed: for a row, the
    /// bytes the observation read; for any other file, its bytes now. The
    /// commit refuses if the file no longer holds it.
    fn pre_image(&self, relative: &Path) -> Result<Option<journal::Image>> {
        if let Some(stamp) = self.catalog.mirror.file(&crate::catalog::slash(relative))? {
            return Ok(Some(journal::Image {
                sha256: stamp.raw_hash,
                size: stamp.stat.size,
            }));
        }
        let path = self.root.join(relative);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(journal::Image::of(&bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(DbError::io(&path, error)),
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
    ///
    /// Beside the outcome comes what planning produced -- a statement's
    /// `RETURNING` rows -- or `None` when the change was not planned at all:
    /// its idempotency key named a change already made, which is reported
    /// and not made again.
    pub fn apply_with<T>(
        &mut self,
        request: Request,
        plan: impl FnOnce(&Catalog) -> Result<(Vec<RowChange>, T)>,
    ) -> Result<(Outcome, Option<T>)> {
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
        if extra.is_none() && !outcome.replayed {
            return Err(DbError::new(
                Code::INTERNAL_ERROR,
                "a change was committed without being planned",
            ));
        }
        Ok((outcome, extra))
    }

    /// Commit file changes planned directly -- schemas, configuration, repairs.
    /// Every path in `expected` must still hold what it held when planned.
    pub fn commit(
        &mut self,
        changes: Vec<FileChange>,
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
        files: Vec<FileChange>,
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

    /// Commit rows whose referential actions have already run -- a
    /// transaction's, each statement's at its end -- as one change.
    pub(crate) fn change_expanded(
        &mut self,
        rows: Vec<RowChange>,
        induced: Vec<Induced>,
        request: Request,
    ) -> Result<Outcome> {
        self.transact(request, |database| {
            database.confirm_rows_unmoved(&rows)?;
            let changes = crate::plan::render(&database.catalog, &rows)?;
            Ok((rows, induced, changes))
        })
    }

    /// The validator, which does not read history, offers `FIX_RESTORE_TARGET`
    /// for every dangling reference; a fault keeps it only where history
    /// holds the row the reference names, so the fix it names is one doctor
    /// can make.
    fn confirm_restorations(&mut self) -> Result<()> {
        let mut errors = std::mem::take(&mut self.verdict.errors);
        let mut confirmed = Ok(());
        for fault in &mut errors {
            if fault.code == Code::FOREIGN_KEY_VIOLATION
                && fault.fixes.iter().any(|fix| fix == "FIX_RESTORE_TARGET")
            {
                match crate::doctor::restoration(self, fault) {
                    Ok(Some(_)) => {}
                    Ok(None) => fault.fixes.retain(|fix| fix != "FIX_RESTORE_TARGET"),
                    Err(error) => {
                        confirmed = Err(error);
                        break;
                    }
                }
            }
        }
        self.verdict.errors = errors;
        confirmed
    }

    /// The outcome of a change already made under this request's idempotency
    /// key, if history records one: the change is reported, not made again.
    /// A command that plans its change before committing it asks this first,
    /// since a plan for a change already made may no longer apply; committing
    /// asks again under the writer lock.
    pub fn replayed(&self, request: &Request) -> Result<Option<Outcome>> {
        let (Some(key), Ok(history)) = (&request.idempotency_key, self.history()) else {
            return Ok(None);
        };
        let Some((id, _)) = history.find_idempotent(key)? else {
            return Ok(None);
        };
        let numbers = self.revision_numbers()?;
        Ok(Some(Outcome {
            rows: vec![],
            induced: vec![],
            changes: vec![],
            revision: Some(RevisionName {
                seq: numbers[&id],
                id,
            }),
            replayed: true,
            kept_outside_edits: vec![],
            warnings: self.verdict.warnings.clone(),
            widened: BTreeMap::new(),
            dry_run: request.dry_run,
            planned_against: None,
        }))
    }

    fn transact(
        &mut self,
        request: Request,
        plan: impl FnOnce(&mut Self) -> Result<(Vec<RowChange>, Vec<Induced>, Vec<FileChange>)>,
    ) -> Result<Outcome> {
        let _lock = self.planning_lock(request.dry_run)?;
        if let Some(history) = self.history.as_mut() {
            history.refresh()?;
        }
        if let Head::Degraded(error) = &self.head {
            return Err(DbError::from_diag((*error.diagnostic).clone()).with_help(
                "no change can be recorded until history is repaired; see `reldir check`",
            ));
        }
        if let Some(outcome) = self.replayed(&request)? {
            return Ok(outcome);
        }
        // Anything valid that changed since opening is recorded before this
        // change, so the revision this change produces describes it alone.
        self.adopt(Origin::External)?;
        let planned_against = self.state_token()?;
        if let Some(expected) = &request.expect_state
            && expected != &planned_against
        {
            return Err(DbError::from_diag(
                Diagnostic::error(
                    Code::PLAN_CHANGED,
                    "the database changed since this change was planned, so the plan no longer \
                     says what committing would do; nothing was written",
                )
                .help("plan the change again, and review what it would do now"),
            ));
        }
        let before = self.verdict.errors.clone();
        let faults_before: Vec<Diagnostic> = self
            .verdict
            .errors
            .iter()
            .chain(&self.verdict.warnings)
            .filter(|diagnostic| crate::contract::is_fault(diagnostic))
            .cloned()
            .collect();
        let (rows, induced, mut changes) = plan(self)?;
        let written = written_by(&self.catalog, &rows, &changes)?;
        // What each path holds now, read before any prospect moves the
        // mirror on to what it would hold.
        let mut pre_images: BTreeMap<PathBuf, Option<journal::Image>> = BTreeMap::new();
        for change in &changes {
            pre_images.insert(change.path().to_path_buf(), self.pre_image(change.path())?);
        }
        // The change is judged as the state it would produce. A value it
        // writes that the contract never admitted is refused -- or, when the
        // request says so, admitted by widening the contract in this same
        // change, and the whole is judged again.
        let mut widened: BTreeMap<String, Vec<crate::schema::widen::Widened>> = BTreeMap::new();
        let prospect = loop {
            let (writes, deletes) = overlay_parts(&self.root, &changes);
            let (relative_writes, relative_deletes) = relative_parts(&changes);
            let overlay = Overlay::new(&Disk, writes, deletes);
            // A change to rows alone is judged from the planned bytes; one
            // that touches a schema or a setting, by observing everything.
            let prospect = match self.catalog.prospect_rows(
                &self.config,
                &relative_writes,
                &relative_deletes,
                &overlay,
                Some(&self.verdict),
            )? {
                Some(prospect) => prospect,
                None => self.catalog.prospect(&self.config, &overlay)?,
            };
            let after: Vec<Diagnostic> = prospect
                .verdict
                .errors
                .iter()
                .chain(&prospect.verdict.warnings)
                .cloned()
                .collect();
            let introduced = crate::contract::introduced(&faults_before, &after, &written);
            // A pinned contract's faults are errors, which admission refuses
            // as it refuses any; an inferred one's are warnings, refused here.
            let refused: Vec<Diagnostic> = if request.widen {
                introduced
            } else {
                introduced
                    .into_iter()
                    .filter(|fault| fault.code == Code::CONTRACT_EXCEEDED)
                    .collect()
            };
            if refused.is_empty() {
                break prospect;
            }
            prospect.abandon()?;
            if !request.widen {
                return Err(self.widening_required(&refused, &changes));
            }
            if !widened.is_empty() {
                return Err(DbError::new(
                    Code::INTERNAL_ERROR,
                    format!(
                        "the widened contract still refuses a value the change writes: {}",
                        refused[0].message
                    ),
                ));
            }
            for (table, path, bytes, made) in self.widenings(&refused, &changes)? {
                pre_images.insert(path.clone(), self.pre_image(&path)?);
                changes.retain(|change| change.path() != path);
                changes.push(FileChange::Write { path, bytes });
                widened.insert(table, made);
            }
        };
        let mut journal_plan = journal::Plan {
            origin: request.origin.as_str().into(),
            idempotency_key: request.idempotency_key.clone(),
            changes: changes
                .iter()
                .map(|change| {
                    let pre = pre_images.get(change.path()).cloned().ok_or_else(|| {
                        DbError::new(
                            Code::INTERNAL_ERROR,
                            format!("{} has no pre-image", change.path().display()),
                        )
                    })?;
                    Ok(match change {
                        FileChange::Write { path, bytes } => journal::Change::Write {
                            path: path.clone(),
                            bytes: bytes.clone(),
                            pre,
                        },
                        FileChange::Delete { path } => journal::Change::Delete {
                            path: path.clone(),
                            pre: pre.ok_or_else(|| moved(path))?,
                        },
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            // A table the change leaves without a schema leaves its directory
            // governing nothing: it goes, when the change empties it.
            prune: self
                .catalog
                .schemas
                .keys()
                .filter(|table| !prospect.catalog.schemas.contains_key(*table))
                .map(PathBuf::from)
                .collect(),
            ..journal::Plan::default()
        };
        if let Err(error) = journal::preflight(
            &crate::engine::fs::Disk,
            &self.root,
            &journal_plan,
            self.config.max_transaction_size,
        ) {
            prospect.abandon()?;
            return Err(error);
        }
        // Every fallible step between judging the change and committing it:
        // whatever happens, the prospect is then kept or abandoned.
        let committed = (|| -> Result<Option<Committing>> {
            match request.admission {
                Admission::Restores(state)
                    if !prospect.verdict.errors.is_empty()
                        && self.identity_of(&prospect.catalog, &changes)? == state => {}
                Admission::Restores(_) => admit(Admission::Valid, &before, &prospect.verdict)?,
                admission => admit(admission, &before, &prospect.verdict)?,
            }
            if request.dry_run || changes.is_empty() {
                return Ok(None);
            }
            // The revision records the state the change produces, so it is
            // made from the prospect -- valid, or no worse than before -- and
            // written by the same transaction as the files.
            let recording = match (&self.history, prospect.verdict.errors.is_empty()) {
                (Some(history), true) => {
                    let (delta, _) = history::pending(&prospect.catalog)?;
                    if delta.is_empty() {
                        None
                    } else {
                        let written: BTreeMap<String, &Vec<u8>> = changes
                            .iter()
                            .filter_map(|change| match change {
                                FileChange::Write { path, bytes } => {
                                    Some((crate::catalog::slash(path), bytes))
                                }
                                FileChange::Delete { .. } => None,
                            })
                            .collect();
                        let catalog = &prospect.catalog;
                        let root = self.root.clone();
                        let prepared = history.prepare(
                            &delta,
                            &mut || history::live_state(catalog),
                            &mut |path| match written.get(path) {
                                Some(bytes) => Ok((*bytes).clone()),
                                None => read_recorded(&root, catalog, path),
                            },
                            history::Recording {
                                origin: request.origin,
                                transaction_id: None,
                                idempotency_key: request.idempotency_key.as_deref(),
                                lineage: None,
                                imported: None,
                            },
                        )?;
                        Some((delta, prepared))
                    }
                }
                _ => None,
            };
            if let Some((_, prepared)) = &recording {
                journal_plan.revision = Some(prepared.id.to_hex());
                journal_plan.artifacts = prepared.artifacts.clone();
                journal_plan.retire = prepared.retire.clone();
            }
            let committed = journal::commit(
                &crate::engine::fs::Disk,
                &self.root,
                &journal_plan,
                &mut journal::Unobserved,
            )?;
            Ok(Some((committed, recording)))
        })();
        let (committed, recording) = match committed {
            Ok(Some(done)) => done,
            Ok(None) => {
                let prospective = prospect.abandon()?;
                return Ok(Outcome {
                    rows,
                    induced,
                    changes,
                    revision: None,
                    replayed: false,
                    kept_outside_edits: vec![],
                    warnings: prospective.warnings,
                    widened,
                    dry_run: request.dry_run,
                    planned_against: Some(planned_against),
                });
            }
            Err(error) if error.code() == Code::COMMITTED_RECOVERY_PENDING => {
                // The change happened: the observation of it is the state.
                let _ = prospect.keep();
                return Err(error);
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
        self.catalog.keep_verdict(&self.config, &self.verdict)?;
        self.catalog.mirror.analyze_drifted()?;
        let revision = match &recording {
            Some((delta, prepared)) => Some(self.after_recording(delta, prepared)?),
            // A change that leaves the state history already has -- restoring
            // what an outside edit removed -- is not a new revision.
            None => self.head.name(),
        };
        Ok(Outcome {
            rows,
            induced,
            changes,
            revision,
            replayed: false,
            kept_outside_edits: committed.kept_outside_edits,
            warnings: self.verdict.warnings.clone(),
            widened,
            dry_run: false,
            planned_against: Some(planned_against),
        })
    }

    /// The refusal of a change that would write values its tables' inferred
    /// contracts have never admitted, saying what widening would admit them.
    fn widening_required(&self, faults: &[Diagnostic], changes: &[FileChange]) -> DbError {
        let proposals: BTreeMap<String, String> = match self.widenings(faults, changes) {
            Ok(widenings) => widenings
                .into_iter()
                .map(|(table, _, _, made)| {
                    let edits: Vec<String> = made.iter().map(ToString::to_string).collect();
                    (table, format!("widening would: {}", edits.join("; ")))
                })
                .collect(),
            Err(error) => faults
                .iter()
                .filter_map(|fault| fault.table.clone())
                .map(|table| {
                    (
                        table,
                        format!(
                            "the contract cannot be widened by rule ({}); edit the schema",
                            error.diagnostic.message
                        ),
                    )
                })
                .collect(),
        };
        let refusals: Vec<Diagnostic> = faults
            .iter()
            .map(|fault| {
                let table = fault.table.clone().unwrap_or_default();
                let fault_text = fault
                    .message
                    .rsplit_once(" -- outside the contract inferred for ")
                    .map_or(fault.message.as_str(), |(text, _)| text);
                Diagnostic {
                    code: Code::CONTRACT_WIDENING_REQUIRED,
                    severity: Severity::Error,
                    message: format!(
                        "the change would write what the contract of {table} has never admitted: \
                         {fault_text}; nothing was written"
                    ),
                    fixes: vec![],
                    help: Some(format!(
                        "{}. Programs reading these files rely on that contract; pass --widen to \
                         widen it as part of this change, or widen it first with `reldir schema \
                         widen`",
                        proposals
                            .get(&table)
                            .map_or("widening is a decision", String::as_str)
                    )),
                    ..fault.clone()
                }
            })
            .collect();
        DbError::from_diag(refusals[0].clone()).with_related(refusals[1..].to_vec())
    }

    /// The schema files that widen each contract just enough to admit the
    /// rows these faults are in, as the change would write them: per table,
    /// its schema's path, the new bytes, and the edits made.
    #[allow(clippy::type_complexity)]
    fn widenings(
        &self,
        faults: &[Diagnostic],
        changes: &[FileChange],
    ) -> Result<Vec<(String, PathBuf, Vec<u8>, Vec<crate::schema::widen::Widened>)>> {
        let mut bodies: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for fault in faults {
            let (Some(table), Some(path)) = (&fault.table, &fault.path) else {
                return Err(DbError::new(
                    Code::INTERNAL_ERROR,
                    format!("a contract fault names no table or file: {}", fault.message),
                ));
            };
            let bytes = changes
                .iter()
                .find_map(|change| match change {
                    FileChange::Write {
                        path: written,
                        bytes,
                    } if written == path => Some(bytes),
                    _ => None,
                })
                .ok_or_else(|| {
                    DbError::new(
                        Code::INTERNAL_ERROR,
                        format!("{} has a new fault and is not written", path.display()),
                    )
                })?;
            let body = crate::json::parse(bytes).map_err(|error| {
                DbError::new(
                    Code::INTERNAL_ERROR,
                    format!("{} as written does not parse: {error}", path.display()),
                )
            })?;
            let rows = bodies.entry(table.clone()).or_default();
            if !rows.contains(&body) {
                rows.push(body);
            }
        }
        let mut out = vec![];
        for (table, rows) in bodies {
            let (schema, file) = match (
                self.catalog.schemas.get(&table),
                self.catalog.schema_files.get(&table),
            ) {
                (Some(schema), Some(file)) => (schema, file),
                _ => return Err(self.catalog.unknown_table(&table)),
            };
            let (document, made) =
                crate::schema::widen::admit(schema.document(), &rows).map_err(|reason| {
                    DbError::from_diag(
                        Diagnostic::error(
                            Code::CONTRACT_WIDENING_REQUIRED,
                            format!("the contract of {table} cannot be widened by rule: {reason}"),
                        )
                        .table(table.as_str())
                        .at(file.relative.clone())
                        .help("edit the schema to say what it should admit, then make the change"),
                    )
                })?;
            let widened = Schema::from_document(document, None).map_err(|problems| {
                DbError::from_diag(problems.into_iter().next().unwrap_or_else(|| {
                    Diagnostic::error(Code::INTERNAL_ERROR, "a widened schema does not load")
                }))
            })?;
            let bytes = widened.render(Some(&file.bytes))?;
            out.push((table, file.relative.clone(), bytes, made));
        }
        Ok(out)
    }

    /// Observe the directory again, re-reading only what changed.
    pub fn reobserve(&mut self) -> Result<()> {
        let started = std::time::Instant::now();
        self.config = configured(&self.root, &self.overrides)?;
        let mirror = Rc::clone(&self.catalog.mirror);
        self.catalog = Catalog::observe(&self.root, &self.config, &Disk, mirror, false)?;
        self.verdict = match self.catalog.earlier_verdict(&self.config)? {
            Some(verdict) => verdict,
            None => {
                let verdict = integrity::validate(&self.catalog)?;
                self.catalog.keep_verdict(&self.config, &verdict)?;
                verdict
            }
        };
        let (history, head) = load_history(&self.root, &self.catalog);
        if let Head::Degraded(error) = &head {
            self.verdict.errors.push((*error.diagnostic).clone());
        }
        self.history = history;
        self.head = head;
        self.confirm_restorations()?;
        self.validation_elapsed = started.elapsed();
        Ok(())
    }

    /// Every row a plan read must still be what it read.
    fn confirm_rows_unmoved(&self, rows: &[RowChange]) -> Result<()> {
        for change in rows {
            if let Some(before) = &change.before {
                let now = self.catalog.row_at(&before.relative)?;
                let same = now.is_some_and(|row| {
                    crate::canonical::compact(&Value::Object(row.value))
                        == crate::canonical::compact(&Value::Object(before.value.clone()))
                });
                if !same {
                    return Err(moved(&before.relative));
                }
            }
        }
        Ok(())
    }

    /// Run `work` under the writer lock, against the files as they are once
    /// the lock is held: interrupted transactions finished, the directory
    /// observed again. For changes made outside the row path -- snapshots,
    /// history upkeep -- that must not race another writer.
    pub fn exclusive<T>(&mut self, work: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let _lock = self.writer_lock()?;
        work(self)
    }

    /// Move recorded history aside and begin a new one from the current files.
    /// For history that cannot be verified: the old heads are kept, with every
    /// object they reach, under `.db/history/quarantine/`, and the first new
    /// revision says where.
    pub fn begin_new_lineage(&mut self, reason: &str) -> Result<RevisionName> {
        let _lock = self.writer_lock()?;
        let degraded = matches!(self.head, Head::Degraded(_));
        self.verdict
            .errors
            .retain(|diagnostic| !degraded || diagnostic.code != Code::INTERNAL_METADATA_CORRUPT);
        if !self.verdict.errors.is_empty() {
            return Err(DbError::from_diag(self.verdict.errors[0].clone())
                .with_related(self.verdict.errors[1..].to_vec())
                .with_help("history can begin only from a valid state; repair the rows first with `reldir doctor`"));
        }
        let layout = crate::store::Layout::new(&self.root);
        // A pack that fails verification goes aside with the heads: out of
        // the live store, so nothing reads it again, and kept, so nothing is
        // lost. The new lineage writes fresh copies of all it names.
        let mut corrupt = vec![];
        if let Some(history) = &self.history {
            for pack in history.store().packs() {
                if let Err(error) = history.store().verify_pack(&pack) {
                    if !matches!(error, crate::store::error::StoreError::Corrupt { .. }) {
                        return Err(history::store_error(error));
                    }
                    corrupt.push(pack.name.clone());
                }
            }
        }
        let quarantined = crate::store::refs::quarantine(
            &crate::engine::fs::Disk,
            &layout.history(),
            &layout.heads(),
            &layout.quarantine(),
            reason,
            chrono::Utc::now(),
        )
        .map_err(history::store_error)?;
        crate::store::refs::set_aside_packs(
            &crate::engine::fs::Disk,
            &layout.packs(),
            &layout.quarantine(),
            &quarantined.stamp,
            &corrupt,
        )
        .map_err(history::store_error)?;
        self.history = Some(History::open(&self.root)?);
        self.head = Head::Empty;
        self.catalog
            .mirror
            .replace_recorded(&BTreeMap::new(), None)?;
        let (delta, _) = history::pending(&self.catalog)?;
        let name = self.record(
            &delta,
            Origin::Recovery,
            Some(Lineage {
                quarantined: format!(".db/history/quarantine/{}", quarantined.stamp),
                reason: reason.to_string(),
            }),
            None,
        )?;
        Ok(name)
    }

    /// Remove history objects no lineage, quarantined lineage or snapshot can
    /// reach, by rewriting the packs to hold exactly the rest.
    pub fn collect_garbage(&mut self, dry_run: bool) -> Result<GarbageReport> {
        let _lock = self.writer_lock()?;
        let history = self.history()?;
        let layout = history.layout().clone();
        let source = &crate::engine::fs::Disk;
        let mut quarantined_heads: Vec<Id> = vec![];
        for quarantined in crate::store::refs::list_quarantined(source, &layout.quarantine())
            .map_err(history::store_error)?
        {
            quarantined_heads.extend(quarantined.heads);
        }
        let trees: Vec<Id> = crate::store::refs::list_snapshots(source, &layout.snapshot_refs())
            .map_err(history::store_error)?
            .into_iter()
            .map(|(_, snapshot)| snapshot.tree)
            .collect();
        let mut keep = crate::store::dag::reachable(history.store(), history.heads(), &trees)
            .map_err(history::store_error)?;
        keep.extend(
            crate::store::dag::reachable_held(history.store(), &quarantined_heads)
                .map_err(history::store_error)?,
        );
        let stored: usize = history.store().object_records();
        let report = GarbageReport {
            kept: keep.len(),
            dropped: stored.saturating_sub(keep.len()),
            packs_before: history.store().packs().len(),
        };
        if dry_run || report.dropped == 0 {
            return Ok(report);
        }
        let staging = layout.objects().join("staging");
        crate::engine::fs::ensure_dir(&crate::engine::fs::Disk, &staging)
            .map_err(|error| DbError::io(&staging, error))?;
        crate::store::consolidate::repack(
            &crate::engine::fs::Disk,
            history.store(),
            &staging,
            &keep,
        )
        .map_err(history::store_error)?;
        crate::engine::fs::Fs::remove_dir(&crate::engine::fs::Disk, &staging)
            .map_err(|error| DbError::io(&staging, error))?;
        if let Some(history) = self.history.as_mut() {
            history.refresh()?;
        }
        Ok(report)
    }

    pub fn schemas(&self) -> &BTreeMap<String, Schema> {
        &self.catalog.schemas
    }

    /// A read's resource limits.
    pub fn query_limits(&self) -> crate::sql::QueryLimits {
        crate::sql::QueryLimits {
            timeout: self
                .config
                .timeout_seconds
                .map(std::time::Duration::from_secs_f64),
            cancel: self.cancel.clone(),
            max_rows: self.config.max_result_rows,
            max_memory: self.config.max_query_memory,
        }
    }
}

/// What garbage collection found, or -- in a dry run -- would remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GarbageReport {
    /// Objects some lineage, quarantine or snapshot reaches.
    pub kept: usize,
    /// Object records nothing reaches.
    pub dropped: usize,
    pub packs_before: usize,
}

/// Open history and judge its heads, without failing the open: history that
/// cannot be read is a fault reported with everything else.
fn load_history(root: &Path, catalog: &Catalog) -> (Option<History>, Head) {
    let history = match History::open(root) {
        Ok(history) => history,
        Err(error) => return (None, Head::Degraded(Box::new(error))),
    };
    if let Err(error) = history.verify_heads() {
        return (Some(history), Head::Degraded(Box::new(error)));
    }
    let head = match history.heads() {
        [] => match history::sync_recorded(catalog, &history, None) {
            Ok(()) => Head::Empty,
            Err(error) => Head::Degraded(Box::new(error)),
        },
        [id] => {
            let id = *id;
            let outcome = (|| -> Result<Head> {
                let revision = history.revision(&id)?;
                history::sync_recorded(catalog, &history, Some((&id, &revision)))?;
                let numbers = history::number(catalog, &history, true)?;
                Ok(Head::One {
                    name: RevisionName {
                        seq: numbers[&id],
                        id,
                    },
                    revision: Box::new(revision),
                })
            })();
            outcome.unwrap_or_else(|error| Head::Degraded(Box::new(error)))
        }
        many => Head::Diverged(many.to_vec()),
    };
    (Some(history), head)
}

/// A file's bytes for history, read now and checked against what the
/// observation recorded of it: a file that moved since is never recorded as
/// what it was.
fn read_recorded(root: &Path, catalog: &Catalog, path: &str) -> Result<Vec<u8>> {
    let full = root.join(path);
    let bytes = fs::read(&full).map_err(|error| DbError::io(&full, error))?;
    if let Some(entry) = catalog.mirror.file(path)?
        && crate::canonical::hash_bytes(&bytes) != entry.raw_hash
    {
        return Err(moved(Path::new(path)));
    }
    Ok(bytes)
}

/// The folder as a reader that may write nothing must see it: every change a
/// committed transaction makes, whether or not its files are in place yet.
fn committed_view(root: &Path) -> Result<Overlay<'static>> {
    let (pending, _) = journal::pending(&crate::engine::fs::Disk, root)?;
    let mut writes = BTreeMap::new();
    let mut deletes = BTreeSet::new();
    for transaction in &pending {
        for change in &transaction.journal.changes {
            match transaction.post(&crate::engine::fs::Disk, root, change)? {
                Some(bytes) => {
                    writes.insert(root.join(&change.path), bytes);
                }
                None => {
                    deletes.insert(root.join(&change.path));
                }
            }
        }
    }
    Ok(Overlay::new(&Disk, writes, deletes))
}

/// Finish or discard interrupted transactions; the caller holds the lock. A
/// committed transaction that cannot be finished stops every writer: its
/// change stands, and nothing may be planned on top of a state still being
/// put in place.
fn recover(root: &Path) -> Result<Option<Event>> {
    let recovered = journal::recover(
        &crate::engine::fs::Disk,
        root,
        &journal::NoBlobs,
        &mut journal::Unobserved,
    )?;
    if let Some((id, path, why)) = recovered.blocked.first() {
        return Err(DbError::from_diag(
            Diagnostic::error(
                Code::RECOVERY_BLOCKED,
                format!(
                    "committed transaction {id} cannot be finished: {} could not be placed ({why})",
                    path.display()
                ),
            )
            .at(path.clone())
            .help(
                "the change stands and readers see it; make the path writable (or move what is in \
                 its way), then run `reldir recover`",
            ),
        ));
    }
    if recovered.completed.is_empty()
        && recovered.discarded.is_empty()
        && recovered.kept_outside_edits.is_empty()
    {
        return Ok(None);
    }
    Ok(Some(Event::Recovered {
        completed: recovered
            .completed
            .into_iter()
            .map(|journal| journal.id)
            .collect(),
        discarded: recovered.discarded,
        kept_outside_edits: recovered
            .kept_outside_edits
            .iter()
            .map(|path| crate::catalog::slash(path))
            .collect(),
    }))
}

fn moved(relative: &Path) -> DbError {
    DbError::from_diag(
        Diagnostic::error(
            Code::CONCURRENT_MODIFICATION,
            format!(
                "{} changed after the change was planned; nothing was written",
                relative.display()
            ),
        )
        .at(relative)
        .help("retry: the change is planned again against the current files"),
    )
}

fn admit(admission: Admission, before: &[Diagnostic], prospective: &Verdict) -> Result<()> {
    let refused: Vec<Diagnostic> = match admission {
        Admission::Valid | Admission::Restores(_) => prospective.errors.clone(),
        Admission::NoNewFaults => {
            // A fault is the rule, the place and the value: a second check
            // failing on a row that already failed one is a new fault.
            let identify = |d: &Diagnostic| {
                (
                    d.code,
                    d.path.clone(),
                    d.pointer.clone(),
                    d.constraint.clone(),
                    d.observed.clone(),
                )
            };
            let known: BTreeSet<_> = before.iter().map(identify).collect();
            prospective
                .errors
                .iter()
                .filter(|d| !known.contains(&identify(d)))
                .cloned()
                .collect()
        }
    };
    // A refused change wrote nothing, so there is nothing for doctor to
    // repair: the faults say what the change would have broken.
    let refused: Vec<Diagnostic> = refused
        .into_iter()
        .map(|fault| Diagnostic {
            fixes: vec![],
            ..fault
        })
        .collect();
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
    Err(DbError::from_diag(lead).with_related(refused[1..].to_vec()))
}

/// The files a change writes, and where each moved row stood before.
fn written_by(
    catalog: &Catalog,
    rows: &[RowChange],
    changes: &[FileChange],
) -> Result<crate::contract::Written> {
    let mut written = crate::contract::Written::default();
    for change in changes {
        if let FileChange::Write { path, .. } = change {
            written.paths.insert(path.clone());
        }
    }
    for change in rows {
        if let (Some(before), Some(after)) = (&change.before, &change.after) {
            let schema = catalog
                .schemas
                .get(&change.table)
                .ok_or_else(|| catalog.unknown_table(&change.table))?;
            let path = crate::plan::row_path(schema, after)?;
            if path != before.relative {
                written.moved_from.insert(path, before.relative.clone());
            }
        }
    }
    Ok(written)
}

/// A change's writes and deletes, by path relative to the root.
fn relative_parts(changes: &[FileChange]) -> (BTreeMap<PathBuf, Vec<u8>>, BTreeSet<PathBuf>) {
    let mut writes = BTreeMap::new();
    let mut deletes = BTreeSet::new();
    for change in changes {
        match change {
            FileChange::Write { path, bytes } => {
                writes.insert(path.clone(), bytes.clone());
            }
            FileChange::Delete { path } => {
                deletes.insert(path.clone());
            }
        }
    }
    (writes, deletes)
}

fn overlay_parts(
    root: &Path,
    changes: &[FileChange],
) -> (BTreeMap<PathBuf, Vec<u8>>, BTreeSet<PathBuf>) {
    let mut writes = BTreeMap::new();
    let mut deletes = BTreeSet::new();
    for change in changes {
        match change {
            FileChange::Write { path, bytes } => {
                writes.insert(root.join(path), bytes.clone());
            }
            FileChange::Delete { path } => {
                deletes.insert(root.join(path));
            }
        }
    }
    (writes, deletes)
}

/// `A path`, `M path`, `D path` for each pending change.
fn configured(root: &Path, overrides: &ResourceOverrides) -> Result<Config> {
    let mut config = load_config(root)?;
    config.apply_overrides(overrides);
    config.validate().map_err(|message| {
        DbError::new(
            Code::RESOURCE_LIMIT,
            format!("invalid command-line limit: {message}"),
        )
    })?;
    Ok(config)
}

fn acquire(
    root: &Path,
    config: &Config,
    holder: crate::lock::Holder,
) -> Result<crate::engine::fs::LockGuard> {
    let path = root.join(".db/lock");
    validate_optional_private_file(&path, "lock")?;
    crate::lock::acquire(
        &crate::engine::fs::Disk,
        &path,
        config.lock_budget(),
        holder,
    )
}

/// Refuse to write where the commit protocol's two promises -- an exclusive
/// lock and an atomic rename -- cannot be verified, unless the configuration
/// says someone decided to accept that.
fn require_safe_filesystem(root: &Path, config: &Config) -> Result<()> {
    match volume_class(root)? {
        crate::engine::fs::VolumeClass::Unsafe(why) if !config.allow_remote_filesystem => {
            Err(DbError::from_diag(
                Diagnostic::error(
                    Code::UNSAFE_FILESYSTEM,
                    format!(
                        "{} is on a filesystem where file locks and atomic renames are not \
                         guaranteed ({why}); writing could lose or interleave changes without \
                         any error",
                        root.display()
                    ),
                )
                .help(
                    "work on a local copy, or -- if this filesystem is known to honour locks \
                     and renames -- set \"allow_remote_filesystem\": true in .db/config",
                ),
            ))
        }
        _ => Ok(()),
    }
}

/// Whether the filesystem holding `root` keeps the commit protocol's two
/// promises, as the operating system describes it.
pub fn volume_class(root: &Path) -> Result<crate::engine::fs::VolumeClass> {
    use crate::engine::fs::Fs as _;
    Ok(crate::engine::fs::Disk
        .volume(root)
        .map_err(|error| DbError::io(root, error))?
        .class)
}

/// Check `.db/format`: present, private, and this binary's version.
pub fn validate_format(root: &Path) -> Result<()> {
    let meta = root.join(".db");
    match fs::symlink_metadata(&meta) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(DbError::from_diag(
                Diagnostic::error(
                    Code::UNINITIALIZED,
                    format!("{} has no .db metadata", root.display()),
                )
                .help(format!("run `reldir init {}`", root.display())),
            ));
        }
        Err(error) => return Err(DbError::io(&meta, error)),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(DbError::new(
                Code::INTERNAL_METADATA_CORRUPT,
                ".db must be a real directory, not a symlink or special file",
            ));
        }
        Ok(_) => {}
    }
    let found = read_format(root)?;
    if found == FORMAT_VERSION {
        return Ok(());
    }
    if found == 2 {
        return Err(DbError::new(
            Code::FORMAT_UPGRADE_REQUIRED,
            "the database is format 2; this binary reads format 3",
        )
        .with_help(
            "run `reldir upgrade`: it converts the history and the schemas in place, and changes no row",
        ));
    }
    Err(DbError::new(
        Code::FORMAT_UNSUPPORTED,
        format!("the database is format {found}; this binary reads only format {FORMAT_VERSION}"),
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
            Code::INTERNAL_METADATA_CORRUPT,
            ".db/format exceeds the 4096-byte format marker limit",
        ));
    }
    let text = fs::read_to_string(&path).map_err(|e| DbError::io(&path, e))?;
    text.lines()
        .find_map(|line| line.strip_prefix("format_version = "))
        .and_then(|value| value.trim().parse::<u32>().ok())
        .ok_or_else(|| {
            DbError::new(
                Code::INTERNAL_METADATA_CORRUPT,
                ".db/format has no valid format_version",
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
            Code::CONFIG_INVALID,
            format!(
                ".db/config exceeds the {} byte bootstrap limit",
                crate::config::BOOTSTRAP_MAX_CONFIG_SIZE
            ),
        ));
    }
    let bytes = fs::read(&path).map_err(|e| DbError::io(&path, e))?;
    let invalid = |message: String| {
        DbError::from_diag(
            Diagnostic::error(
                Code::CONFIG_INVALID,
                format!("invalid .db/config: {message}"),
            )
            .at(".db/config"),
        )
    };
    let value = crate::json::with_depth_limit(crate::config::BOOTSTRAP_MAX_NESTING_DEPTH, || {
        crate::json::parse(&bytes).map_err(|error| invalid(error.to_string()))
    })?;
    // Settings a later release added are named as that release's, before
    // they could be mistaken for typos.
    if let Some(requires) = value.get("requires").and_then(Value::as_str)
        && let (Some(needed), Some(running)) = (
            crate::config::release(requires),
            crate::config::release(crate::VERSION),
        )
        && needed > running
    {
        return Err(DbError::from_diag(
            Diagnostic::error(
                Code::CONFIG_INVALID,
                format!(
                    ".db/config holds settings of reldir {requires}, and this is reldir {}",
                    crate::VERSION
                ),
            )
            .at(".db/config")
            .help(format!("use reldir {requires} or later with this database")),
        ));
    }
    let config: Config =
        serde_json::from_value(value).map_err(|error| invalid(error.to_string()))?;
    config.validate().map_err(invalid)?;
    Ok(config)
}

fn require_private_regular_file(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        DbError::new(
            Code::INTERNAL_METADATA_CORRUPT,
            format!("cannot inspect {description} {}: {error}", path.display()),
        )
    })?;
    if !metadata.file_type().is_file() || crate::catalog::has_multiple_links(&metadata) {
        return Err(DbError::new(
            Code::INTERNAL_METADATA_CORRUPT,
            format!(
                "{description} {} must be a private regular file",
                path.display()
            ),
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

/// The `.gitignore` that keeps derived state out of version control. The
/// format, the configuration and the inferred contracts in `.db/schema/` are
/// versioned always: a clone must interpret the folder, and hold it to the
/// shape it was adopted with, exactly as the original does. With
/// `track_provenance`, history is versioned as well -- its packs and heads are
/// named by content, so histories merge in git without conflict.
pub fn gitignore(track_provenance: bool) -> &'static str {
    if track_provenance {
        "*\n!.gitignore\n!format\n!config\n!schema/\n!schema/**\n!objects/\n!objects/pack/\n\
         !objects/pack/**\n!history/\n!history/heads/\n!history/heads/**\n!history/refs/\n\
         !history/refs/**\n!history/quarantine/\n!history/quarantine/**\n"
    } else {
        "*\n!.gitignore\n!format\n!config\n!schema/\n!schema/**\n"
    }
}

/// Write the metadata layout of an empty database, with `config` as its
/// settings.
pub fn init_layout(root: &Path, track_provenance: bool, config: &Config) -> Result<()> {
    use crate::engine::fs::{Disk as EngineDisk, Fs};
    let meta = root.join(".db");
    if fs::symlink_metadata(&meta).is_ok() {
        return Err(DbError::new(
            Code::ALREADY_INITIALIZED,
            format!("{} is already initialized", root.display()),
        ));
    }
    fs::create_dir_all(root).map_err(|e| DbError::io(root, e))?;
    let durable = |path: &Path| {
        crate::engine::fs::ensure_dir(&EngineDisk, path).map_err(|error| DbError::io(path, error))
    };
    durable(&meta)?;
    // `schema/` at the root is the user's pin directory and appears only when
    // they pin something; reldir's inferred contracts live in `.db/schema`.
    for directory in ["transactions", "schema"] {
        durable(&meta.join(directory))?;
    }
    History::init(root)?;
    let write = |name: &str, bytes: &[u8]| {
        let path = meta.join(name);
        crate::engine::fs::replace_durably(&EngineDisk, &path, bytes)
            .map_err(|error| DbError::io(&path, error))
    };
    write("config", &{
        let mut bytes = serde_json::to_vec_pretty(config)
            .map_err(|error| DbError::new(Code::INTERNAL_ERROR, error.to_string()))?;
        bytes.push(b'\n');
        bytes
    })?;
    write(".gitignore", gitignore(track_provenance).as_bytes())?;
    // The format marker is written last: a `.db/` without one is a layout
    // that was never finished, which establishment recognises and rebuilds.
    write(
        "format",
        format!("format_version = {FORMAT_VERSION}\n").as_bytes(),
    )?;
    EngineDisk
        .sync_dir(&meta)
        .map_err(|error| DbError::io(&meta, error))?;
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
            r#"{{"$schema":"{}","type":"object","properties":{{"id":{{"type":"string"}},"name":{{"type":"string"}}}},"required":["id","name"],"additionalProperties":false,"x-reldir":{{"table":"users","identity":{{"source":"body","columns":["id"],"naming":{{"rule":"reldir"}}}}}}}}"#,
            crate::schema::meta::DIALECT_URI
        )
    }

    fn database() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        init_layout(directory.path(), false, &Config::default()).unwrap();
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
        init_layout(directory.path(), false, &Config::default()).unwrap();
        validate_format(directory.path()).unwrap();
        for (other, code) in [
            (1, Code::FORMAT_UNSUPPORTED),
            (2, Code::FORMAT_UPGRADE_REQUIRED),
            (FORMAT_VERSION + 1, Code::FORMAT_UNSUPPORTED),
        ] {
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
                code,
                "format {other}"
            );
        }
        fs::write(directory.path().join(".db/format"), "garbage\n").unwrap();
        assert_eq!(
            validate_format(directory.path())
                .unwrap_err()
                .diagnostic
                .code,
            Code::INTERNAL_METADATA_CORRUPT
        );
    }

    #[test]
    fn test1024_initialisation_writes_the_documented_layout() {
        let directory = tempfile::tempdir().unwrap();
        init_layout(directory.path(), false, &Config::default()).unwrap();
        for expected in [
            ".db/format",
            ".db/config",
            ".db/.gitignore",
            ".db/schema",
            ".db/transactions",
            ".db/objects/pack",
            ".db/history/heads",
            ".db/history/refs/snapshots",
            ".db/history/quarantine",
        ] {
            assert!(directory.path().join(expected).exists(), "{expected}");
        }
        assert!(
            !directory.path().join("schema").exists(),
            "initialisation declares nothing"
        );
        assert!(load_config(directory.path()).unwrap().validate().is_ok());
        assert_eq!(
            init_layout(directory.path(), false, &Config::default())
                .unwrap_err()
                .diagnostic
                .code,
            Code::ALREADY_INITIALIZED
        );
    }

    #[test]
    fn test1025_tracked_provenance_keeps_history_under_version_control() {
        for track in [false, true] {
            assert!(
                gitignore(track).contains("!schema/**"),
                "the inferred contracts always travel with the files"
            );
        }
        assert!(!gitignore(false).contains("history"));
        assert!(
            gitignore(true).contains("!objects/pack/**")
                && gitignore(true).contains("!history/heads/**")
        );
        // A matcher that reads the file as git does: later patterns win.
        let ignored = |text: &str, path: &str| {
            let mut ignored = false;
            for line in text.lines() {
                let (negated, pattern) = match line.strip_prefix('!') {
                    Some(rest) => (true, rest),
                    None => (false, line),
                };
                let matches = pattern == "*"
                    || pattern == path
                    || pattern
                        .strip_suffix("/**")
                        .is_some_and(|prefix| path.starts_with(&format!("{prefix}/")))
                    || pattern
                        .strip_suffix('/')
                        .is_some_and(|prefix| path == prefix);
                if matches {
                    ignored = !negated;
                }
            }
            ignored
        };
        assert!(ignored(gitignore(false), "mirror.sqlite"));
        assert!(!ignored(gitignore(false), "schema/users.json"));
        assert!(ignored(gitignore(false), "objects/pack/pack-1.pack"));
        assert!(!ignored(gitignore(true), "objects/pack/pack-1.pack"));
        assert!(!ignored(gitignore(true), "history/heads/abc"));
        assert!(ignored(gitignore(true), "transactions/x/journal"));
    }

    #[test]
    fn test2167_a_file_rewritten_in_the_tick_it_was_read_is_read_again() {
        let directory = database();
        let path = directory.path().join("users/u1.json");
        let open = || {
            Database::open(
                directory.path().to_path_buf(),
                Access::Write,
                &Default::default(),
            )
            .unwrap()
        };
        drop(open());
        let stamped = fs::metadata(&path).unwrap();
        // The same size, the same modification time: on a filesystem whose
        // clock ticks coarsely, this is what a quick second write looks like.
        fs::write(&path, "{\"id\":\"u1\",\"name\":\"Z\"}\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(stamped.modified().unwrap())
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), stamped.len());
        let reopened = open();
        let row = reopened
            .catalog
            .row_at(Path::new("users/u1.json"))
            .unwrap()
            .unwrap();
        assert_eq!(
            row.value["name"], "Z",
            "a stamp made within the racy window of its file's time is not trusted"
        );
    }

    #[test]
    fn test2179_settings_of_a_later_release_name_it() {
        let directory = database();
        let root = directory.path();
        fs::write(
            root.join(".db/config"),
            r#"{"requires": "99.1.0", "setting_of_the_future": true}"#,
        )
        .unwrap();
        let refused = load_config(root).unwrap_err();
        assert_eq!(refused.code(), Code::CONFIG_INVALID);
        assert!(
            refused.diagnostic.message.contains("reldir 99.1.0"),
            "{refused}"
        );
        fs::write(
            root.join(".db/config"),
            format!(r#"{{"requires": "{}"}}"#, crate::VERSION),
        )
        .unwrap();
        load_config(root).unwrap();
        fs::write(root.join(".db/config"), r#"{"requires": "soon"}"#).unwrap();
        assert!(
            load_config(root)
                .unwrap_err()
                .diagnostic
                .message
                .contains("MAJOR.MINOR.PATCH")
        );
        fs::write(root.join(".db/config"), r#"{"typo_setting": 1}"#).unwrap();
        assert!(
            load_config(root)
                .unwrap_err()
                .diagnostic
                .message
                .contains("typo_setting")
        );
    }

    #[test]
    fn test2180_a_restore_admits_the_faults_of_the_state_it_names_and_no_other() {
        let directory = database();
        let root = directory.path();
        write(&root.join("users/u2.json"), "{\"id\":");
        let mut database =
            Database::open(root.to_path_buf(), Access::Write, &Default::default()).unwrap();
        assert!(!database.is_valid());
        database
            .exclusive(|database| crate::snapshot::create(database, "faulty").map(drop))
            .unwrap();
        let restore = crate::snapshot::restore(&database, "faulty").unwrap();
        assert!(restore.changes.is_empty(), "the folder is the snapshot");
        let restoring = |state| Request {
            admission: Admission::Restores(state),
            ..Request::new(Origin::SnapshotRestore, false)
        };
        database
            .commit(vec![], &Expected::new(), restoring(restore.state))
            .expect("the faults the snapshot holds are put back");
        let other = database
            .commit(
                vec![],
                &Expected::new(),
                restoring(crate::store::Hash256::of(b"another state")),
            )
            .expect_err("a state with faults that is not the snapshot's");
        assert_eq!(other.diagnostic.code, Code::INVALID_JSON);
    }

    #[test]
    fn test2178_gc_keeps_every_object_a_quarantined_lineage_reaches() {
        let directory = database();
        let root = directory.path();
        let mut database =
            Database::open(root.to_path_buf(), Access::Write, &Default::default()).unwrap();
        for name in ["B", "C", "D"] {
            let row = database
                .catalog
                .row_at(Path::new("users/u1.json"))
                .unwrap()
                .unwrap();
            let mut after = row.value.clone();
            after.insert("name".into(), json!(name));
            database
                .change(
                    vec![RowChange::update(row, after)],
                    vec![],
                    &Expected::new(),
                    Request::new(Origin::User, false),
                )
                .unwrap();
        }
        database.begin_new_lineage("a test begins anew").unwrap();
        database.collect_garbage(false).unwrap();
        let layout = crate::store::Layout::new(root);
        let quarantined =
            crate::store::refs::list_quarantined(&crate::engine::fs::Disk, &layout.quarantine())
                .unwrap();
        assert_eq!(quarantined.len(), 1);
        let history = History::open(root).unwrap();
        // Every revision of the quarantined lineage, back to its first, still
        // loads and proves itself.
        let mut stack: Vec<Id> = quarantined[0].heads.clone();
        let mut seen = 0;
        while let Some(id) = stack.pop() {
            let findings = crate::store::verify::verify_head(history.store(), &id).unwrap();
            assert!(findings.is_empty(), "{id:?}: {findings:?}");
            stack.extend(history.revision(&id).unwrap().parents);
            seen += 1;
        }
        assert!(
            seen >= 4,
            "the whole quarantined lineage was walked, not {seen}"
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
        let revision = first.head.name().unwrap().seq;
        drop(first);
        let again = Database::open(
            directory.path().to_path_buf(),
            Access::Write,
            &Default::default(),
        )
        .unwrap();
        assert_eq!(
            again.head.name().unwrap().seq,
            revision,
            "an unchanged state is not re-recorded"
        );
        // A database opened for write holds the writer lock until dropped.
        drop(again);
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
        assert_eq!(changed.head.name().unwrap().seq, revision + 1);
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
        let before = History::open(directory.path()).unwrap().heads().to_vec();
        let mirror_before = fs::read(crate::mirror::path(directory.path())).unwrap();
        let reader = Database::open(
            directory.path().to_path_buf(),
            Access::Read,
            &Default::default(),
        )
        .unwrap();
        assert_eq!(
            History::open(directory.path()).unwrap().heads().to_vec(),
            before
        );
        assert_eq!(
            fs::read(crate::mirror::path(directory.path())).unwrap(),
            mirror_before,
            "not even derived state"
        );
        assert_eq!(
            reader.unrecorded,
            ["A users/u2.json"],
            "what read-only access could not record is said, and left for a writer"
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
        let revision = database.head.name().unwrap().id;
        let bad = vec![FileChange::Write {
            path: "users/u9.json".into(),
            bytes: b"{\"id\":\"u9\"}\n".to_vec(),
        }];
        let error = database
            .commit(bad, &Expected::new(), Request::internal(false))
            .unwrap_err();
        assert_eq!(error.exit(), crate::code::Exit::Invalid);
        assert!(
            error.diagnostic.message.contains("nothing was written"),
            "{}",
            error.diagnostic.message
        );
        assert!(!directory.path().join("users/u9.json").exists());
        assert_eq!(
            History::open(directory.path()).unwrap().heads(),
            [revision],
            "nothing was recorded"
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
        let change = vec![FileChange::Write {
            path: "users/u1.json".into(),
            bytes: b"{\"id\":\"u1\",\"name\":\"mine\"}\n".to_vec(),
        }];
        let error = database
            .commit(change, &expected, Request::internal(false))
            .unwrap_err();
        assert_eq!(error.diagnostic.code, Code::CONCURRENT_MODIFICATION);
        assert_eq!(error.exit(), crate::code::Exit::Contention);
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
        let change = vec![FileChange::Write {
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
        assert_eq!(error.diagnostic.code, Code::READ_ONLY);
    }

    #[test]
    fn test2166_repairs_may_leave_old_faults_but_never_add_one() {
        let before = vec![
            Diagnostic::error(Code::FOREIGN_KEY_VIOLATION, "m")
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
                Diagnostic::error(Code::TYPE_MISMATCH, "m")
                    .at("b.json")
                    .pointer(""),
            ],
            warnings: vec![],
        };
        let error = admit(Admission::NoNewFaults, &before, &worse).unwrap_err();
        assert_eq!(error.diagnostic.code, Code::TYPE_MISMATCH);

        // The same row failing a second check is a second fault.
        let check = |name: &str| {
            let mut diagnostic = Diagnostic::error(Code::CHECK_VIOLATION, "c").at("c.json");
            diagnostic.constraint = Some(name.into());
            diagnostic
        };
        let one = vec![check("positive")];
        let two = Verdict {
            errors: vec![check("positive"), check("small")],
            warnings: vec![],
        };
        let error = admit(Admission::NoNewFaults, &one, &two).unwrap_err();
        assert_eq!(error.diagnostic.constraint.as_deref(), Some("small"));
    }
}
