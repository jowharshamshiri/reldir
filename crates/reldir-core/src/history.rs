//! Recorded history: how the database came to be what it is.
//!
//! Each accepted state is a *revision*: a content-addressed object naming its
//! parents, the identity of the state, and the root of a Merkle tree over it
//! ([`crate::store::tree`]). The tree holds every row by its canonical hash and
//! the id of the file's original bytes, so any recorded state can be listed,
//! diffed against another in time proportional to what differs, and put back
//! byte for byte. Revisions, tree nodes and file bytes live in packs under
//! `.db/objects/pack/`; the tips of history are empty files under
//! `.db/history/heads/`, one per head, so two clones' histories merge in git
//! without conflict and a merged history simply has two heads until the next
//! recording joins them.
//!
//! A revision is written by the same transaction as the change it records:
//! its pack and head are artifacts of the journal ([`crate::engine::journal`]),
//! published once the rows are in place. History and the rows it describes can
//! therefore never disagree after a crash.
//!
//! Provenance is not validity: a state can be valid without having been
//! recorded, and history never claims to know who made an outside edit.

use crate::{
    catalog::Catalog,
    code::Code,
    diagnostic::{DbError, Diagnostic, Result},
    engine::{fs::Disk, journal},
    store::{
        self, Hash256, Id, Layout, ObjectKind, dag,
        error::StoreError,
        node::Entry,
        objects::Store,
        pack::{PackName, PackWriter},
        refs::Heads,
        revision::{Lineage, Origin, Revision, Stats},
        tree::{self, Built, TreeBuilder, TreeUpdate},
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// The filesystem history is read through. A unit value, so a store over it
/// can live as long as the database that holds it.
static DISK: Disk = Disk;

/// A store failure, as the error a command reports.
pub fn store_error(error: StoreError) -> DbError {
    let code = match &error {
        StoreError::Io { .. } => Code::IO_ERROR,
        StoreError::Invalid(_) => Code::INTERNAL_ERROR,
        StoreError::Corrupt { .. } | StoreError::CorruptObject { .. } | StoreError::Missing(_) => {
            Code::INTERNAL_METADATA_CORRUPT
        }
    };
    let mut diagnostic = Diagnostic::error(code, format!("history: {error}"));
    if let StoreError::Io { path, .. } | StoreError::Corrupt { path, .. } = &error {
        diagnostic = diagnostic.at(path.clone());
    }
    DbError::from_diag(diagnostic)
}

/// One entry of a state that is not a row: the format, the configuration, or
/// a table's schema. The tree records its hash, and its file's bytes so that
/// putting a state back puts the file back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// The file the entry is read from, relative to the root.
    pub path: String,
    pub hash: Hash256,
    pub bytes: Vec<u8>,
}

/// How the state differs from the head it is recorded on top of.
#[derive(Debug, Clone, Default)]
pub struct Delta {
    /// `Some` when the format changed: `Some(None)` when it is gone.
    pub format: Option<Option<Recorded>>,
    pub config: Option<Option<Recorded>>,
    /// Tables whose schema changed, appeared (`Some`) or went (`None`).
    pub tables: BTreeMap<String, Option<Recorded>>,
    pub rows: Vec<RowDelta>,
}

impl Delta {
    pub fn is_empty(&self) -> bool {
        self.format.is_none()
            && self.config.is_none()
            && self.tables.is_empty()
            && self.rows.is_empty()
    }
}

/// One row's change since the head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowDelta {
    Added(Entry),
    Modified(Entry),
    Removed(String),
}

/// The whole state, for a recording that cannot build on one head: the first
/// revision, and the merge of a history with several.
#[derive(Debug, Clone, Default)]
pub struct State {
    pub format: Option<Recorded>,
    pub config: Option<Recorded>,
    pub tables: BTreeMap<String, Recorded>,
    pub rows: Vec<Entry>,
}

/// A revision made ready to commit: the objects to publish with the change it
/// records, and the heads it supersedes.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub id: Id,
    pub revision: Revision,
    pub artifacts: Vec<journal::Artifact>,
    pub retire: Vec<PathBuf>,
}

/// What a recording needs to know beyond the state.
#[derive(Debug, Clone)]
pub struct Recording<'a> {
    pub origin: Origin,
    pub transaction_id: Option<&'a str>,
    pub idempotency_key: Option<&'a str>,
    pub lineage: Option<Lineage>,
    /// A revision brought from a format 2 history: when it was made, which
    /// revision it was there, and which binary made it. A new recording is
    /// made now, by this binary.
    pub imported: Option<Imported<'a>>,
}

/// What an imported revision keeps of the one it was converted from.
#[derive(Debug, Clone)]
pub struct Imported<'a> {
    pub revision: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub binary_version: &'a str,
}

/// History as the head heads it.
pub struct History {
    layout: Layout,
    store: Store<'static>,
    heads: Vec<Id>,
}

impl History {
    /// Open the history under `root`. Every history directory must exist: a
    /// missing one is reported, never read as "nothing recorded".
    pub fn open(root: &Path) -> Result<Self> {
        let layout = Layout::new(root);
        let store = Store::open(&DISK, &layout.objects()).map_err(store_error)?;
        let heads = Heads::list(&DISK, &layout.heads()).map_err(store_error)?;
        Ok(Self {
            layout,
            store,
            heads,
        })
    }

    /// Create every history directory that does not exist yet.
    pub fn init(root: &Path) -> Result<()> {
        Layout::new(root).init(&DISK).map_err(store_error)
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn store(&self) -> &Store<'static> {
        &self.store
    }

    /// The tips of history: none before the first recording, one normally,
    /// several after histories kept in version control were merged.
    pub fn heads(&self) -> &[Id] {
        &self.heads
    }

    /// Look again at the heads and packs, after a commit published new ones.
    pub fn refresh(&mut self) -> Result<()> {
        self.store.refresh().map_err(store_error)?;
        self.heads = Heads::list(&DISK, &self.layout.heads()).map_err(store_error)?;
        Ok(())
    }

    pub fn revision(&self, id: &Id) -> Result<Revision> {
        store::revision::load(&self.store, id).map_err(store_error)
    }

    /// Check what every command relies on: each head's revision and root node
    /// load and agree. O(1) per head; `verify_full` checks everything.
    pub fn verify_heads(&self) -> Result<()> {
        for head in &self.heads {
            let findings = store::verify::verify_head(&self.store, head).map_err(store_error)?;
            if let Some(finding) = findings.first() {
                return Err(DbError::new(
                    Code::INTERNAL_METADATA_CORRUPT,
                    format!("history head {head} does not verify: {finding}"),
                ));
            }
        }
        Ok(())
    }

    /// Check every object of every lineage, recomputing identities from the
    /// rows up. Returns a line per finding.
    pub fn verify_full(&self) -> Result<Vec<String>> {
        let report = store::verify::verify_full(&self.store, &self.heads).map_err(store_error)?;
        Ok(report
            .findings
            .iter()
            .map(|finding| finding.to_string())
            .collect())
    }

    /// Every revision reachable from the heads, newest first.
    pub fn log(&self) -> Result<Vec<(Id, Revision)>> {
        let mut seen = BTreeSet::new();
        let mut out = vec![];
        for head in &self.heads {
            for id in dag::ancestors(&self.store, head).map_err(store_error)? {
                if seen.insert(id) {
                    out.push((id, self.revision(&id)?));
                }
            }
        }
        out.sort_by(|left, right| {
            right
                .1
                .timestamp
                .cmp(&left.1.timestamp)
                .then_with(|| right.0.cmp(&left.0))
        });
        Ok(out)
    }

    /// The rows a tree holds, in path order.
    pub fn entries(&self, tree: &Id) -> Result<Vec<Entry>> {
        tree::entries(&self.store, tree).map_err(store_error)
    }

    /// The files beside the rows a tree holds: schemas, configuration, format,
    /// and files that were not valid rows.
    pub fn extras(&self, tree: &Id) -> Result<Vec<(String, Id)>> {
        tree::extras(&self.store, tree).map_err(store_error)
    }

    /// The rows that differ between two trees.
    pub fn diff(&self, before: &Id, after: &Id) -> Result<Vec<tree::Change>> {
        tree::diff(&self.store, before, after).map_err(store_error)
    }

    /// A file's bytes, as recorded.
    pub fn blob(&self, id: &Id) -> Result<Vec<u8>> {
        Ok(self.store.get(id).map_err(store_error)?.bytes)
    }

    /// The bytes a path last held in recorded history, and the revision that
    /// recorded them: what doctor puts back when a row was deleted by mistake.
    /// The bytes of the file at `path` as a revision recorded it, if its
    /// tree holds that file.
    pub fn file_at(&self, revision: &Id, path: &str) -> Result<Option<Vec<u8>>> {
        let tree = self.revision(revision)?.tree;
        match tree::lookup(&self.store, &tree, path).map_err(store_error)? {
            Some((_, blob)) => Ok(Some(self.blob(&blob)?)),
            None => Ok(None),
        }
    }

    pub fn last_known(&self, path: &str) -> Result<Option<(Id, Vec<u8>)>> {
        for head in &self.heads {
            if let Some(found) = dag::last_known(&self.store, head, path).map_err(store_error)? {
                return Ok(Some((found.revision, self.blob(&found.blob)?)));
            }
        }
        Ok(None)
    }

    /// Make a revision of the state the delta describes, on top of the single
    /// head (or from nothing). With several heads -- a merged history -- or
    /// none, the whole state is built instead, from `state`, which is only
    /// asked for then. `bytes` gives a row file's bytes by path; it is asked
    /// only for files history does not hold yet.
    pub fn prepare(
        &self,
        delta: &Delta,
        state: &mut dyn FnMut() -> Result<State>,
        bytes: &mut dyn FnMut(&str) -> Result<Vec<u8>>,
        recording: Recording<'_>,
    ) -> Result<Prepared> {
        // Recording happens under the writer lock: one listing of the packs
        // answers every question about what history already holds.
        let _settled = self.store.settle().map_err(store_error)?;
        let mut pack = PackWriter::new();
        // A new lineage begins over history that may be damaged: it keeps
        // fresh copies of everything it names rather than trusting old ones.
        let fresh = recording.lineage.is_some();
        let held = |store: &Store<'_>, id: &Id| -> Result<bool> {
            Ok(!fresh && store.contains(id).map_err(store_error)?)
        };
        let blob =
            |pack: &mut PackWriter, store: &Store<'_>, id: Id, content: Vec<u8>| -> Result<()> {
                if Id::of(&content) != id {
                    return Err(DbError::new(
                        Code::CONCURRENT_MODIFICATION,
                        "a file changed while its change was being recorded; nothing was written",
                    ));
                }
                if !pack.contains(&id) && !held(store, &id)? {
                    pack.add(ObjectKind::Blob, content).map_err(store_error)?;
                }
                Ok(())
            };
        let stats;
        let built: Built = match self.heads.as_slice() {
            [head] => {
                let parent = self.revision(head)?;
                let mut update = TreeUpdate::new(&self.store, parent.tree).map_err(store_error)?;
                for (field, change) in
                    [(".db/format", &delta.format), (".db/config", &delta.config)]
                {
                    if let Some(change) = change {
                        let hash = change.as_ref().map(|recorded| recorded.hash);
                        if field == ".db/format" {
                            update.set_format(hash);
                        } else {
                            update.set_config(hash);
                        }
                        match change {
                            Some(recorded) => {
                                let id = Id::of(&recorded.bytes);
                                blob(&mut pack, &self.store, id, recorded.bytes.clone())?;
                                update.set_extra(&recorded.path, id).map_err(store_error)?;
                            }
                            None => update.remove_extra(field).map_err(store_error)?,
                        }
                    }
                }
                let parent_tables: BTreeSet<String> =
                    store::node::load_root(&self.store, &parent.tree)
                        .map_err(store_error)?
                        .tables
                        .into_iter()
                        .map(|table| table.name)
                        .collect();
                let parent_extras: BTreeSet<String> = self
                    .extras(&parent.tree)?
                    .into_iter()
                    .map(|(path, _)| path)
                    .collect();
                for (table, change) in &delta.tables {
                    // A table's schema is one file -- its pin, or its
                    // inferred contract. Wherever it is now, it is no longer
                    // at the other place, and a dropped table's is nowhere.
                    let now = change.as_ref().map(|recorded| recorded.path.as_str());
                    for home in &schema_homes(table) {
                        if Some(home.as_str()) != now && parent_extras.contains(home) {
                            update.remove_extra(home).map_err(store_error)?;
                        }
                    }
                    match change {
                        Some(recorded) => {
                            if parent_tables.contains(table) {
                                update
                                    .set_schema(table, Some(recorded.hash))
                                    .map_err(store_error)?;
                            } else {
                                update
                                    .create_table(table, Some(recorded.hash))
                                    .map_err(store_error)?;
                            }
                            let id = Id::of(&recorded.bytes);
                            blob(&mut pack, &self.store, id, recorded.bytes.clone())?;
                            update.set_extra(&recorded.path, id).map_err(store_error)?;
                        }
                        None => {
                            update.drop_table(table).map_err(store_error)?;
                        }
                    }
                }
                // Dropping a table removes every row it held: the delta's
                // removals of those rows are counted, and recorded by the drop.
                let dropped: BTreeSet<&str> = delta
                    .tables
                    .iter()
                    .filter(|(_, change)| change.is_none())
                    .map(|(table, _)| table.as_str())
                    .collect();
                let mut counts = Stats::default();
                for row in &delta.rows {
                    match row {
                        RowDelta::Removed(path)
                            if path
                                .split_once('/')
                                .is_some_and(|(table, _)| dropped.contains(table)) =>
                        {
                            counts.removed += 1;
                        }
                        RowDelta::Added(entry) | RowDelta::Modified(entry) => {
                            if !self.store.contains(&entry.blob).map_err(store_error)?
                                && !pack.contains(&entry.blob)
                            {
                                blob(&mut pack, &self.store, entry.blob, bytes(&entry.path)?)?;
                            }
                            if matches!(row, RowDelta::Added(_)) {
                                counts.added += 1;
                                update.add(entry.clone()).map_err(store_error)?;
                            } else {
                                counts.modified += 1;
                                update.modify(entry.clone()).map_err(store_error)?;
                            }
                        }
                        RowDelta::Removed(path) => {
                            counts.removed += 1;
                            update.remove(path).map_err(store_error)?;
                        }
                    }
                }
                stats = counts;
                update.finish().map_err(store_error)?
            }
            _ => {
                let whole = state()?;
                let mut builder = TreeBuilder::new(
                    whole.format.as_ref().map(|recorded| recorded.hash),
                    whole.config.as_ref().map(|recorded| recorded.hash),
                );
                for recorded in whole.format.iter().chain(&whole.config) {
                    let id = Id::of(&recorded.bytes);
                    blob(&mut pack, &self.store, id, recorded.bytes.clone())?;
                    builder.extra(&recorded.path, id).map_err(store_error)?;
                }
                for (table, recorded) in &whole.tables {
                    builder
                        .table(table, Some(recorded.hash))
                        .map_err(store_error)?;
                    let id = Id::of(&recorded.bytes);
                    blob(&mut pack, &self.store, id, recorded.bytes.clone())?;
                    builder.extra(&recorded.path, id).map_err(store_error)?;
                }
                for entry in &whole.rows {
                    if !held(&self.store, &entry.blob)? && !pack.contains(&entry.blob) {
                        blob(&mut pack, &self.store, entry.blob, bytes(&entry.path)?)?;
                    }
                    builder.insert(entry.clone()).map_err(store_error)?;
                }
                stats = if self.heads.is_empty() {
                    Stats {
                        added: whole.rows.len() as u64,
                        modified: 0,
                        removed: 0,
                    }
                } else {
                    // Joining merged heads: no one parent is the base, so the
                    // counts are what changed from the state this clone had
                    // recorded -- what the merge brought in.
                    let mut counts = Stats::default();
                    for row in &delta.rows {
                        match row {
                            RowDelta::Added(_) => counts.added += 1,
                            RowDelta::Modified(_) => counts.modified += 1,
                            RowDelta::Removed(_) => counts.removed += 1,
                        }
                    }
                    counts
                };
                builder.finish().map_err(store_error)?
            }
        };
        for object in built.objects {
            pack.add_object(object).map_err(store_error)?;
        }
        let revision = Revision {
            parents: {
                let mut parents = self.heads.clone();
                parents.sort();
                parents
            },
            state: built.identity,
            tree: built.root,
            timestamp: recording
                .imported
                .as_ref()
                .map_or_else(chrono::Utc::now, |imported| imported.timestamp),
            origin: if self.heads.len() > 1 {
                Origin::Merge
            } else {
                recording.origin
            },
            transaction_id: recording.transaction_id.map(String::from),
            idempotency_key: recording.idempotency_key.map(String::from),
            binary_version: recording
                .imported
                .as_ref()
                .map_or(crate::VERSION, |imported| imported.binary_version)
                .into(),
            stats,
            lineage: recording.lineage,
            imported: recording
                .imported
                .as_ref()
                .map(|imported| store::revision::Imported {
                    revision: imported.revision,
                }),
        };
        let id = revision.id().map_err(store_error)?;
        pack.add_object(revision.object().map_err(store_error)?)
            .map_err(store_error)?;
        let (pack_bytes, idx_bytes) = pack.encode().map_err(store_error)?;
        let name = PackName::generate();
        let relative = |path: PathBuf| -> PathBuf {
            path.strip_prefix(self.layout.db().parent().unwrap_or(Path::new("")))
                .map(Path::to_path_buf)
                .unwrap_or(path)
        };
        let packs = relative(self.layout.packs());
        let heads = relative(self.layout.heads());
        Ok(Prepared {
            id,
            artifacts: vec![
                journal::Artifact {
                    bytes: pack_bytes,
                    to: packs.join(name.pack_file()),
                },
                journal::Artifact {
                    bytes: idx_bytes,
                    to: packs.join(name.idx_file()),
                },
                journal::Artifact {
                    bytes: vec![],
                    to: heads.join(id.to_hex()),
                },
            ],
            retire: self
                .heads
                .iter()
                .map(|head| heads.join(head.to_hex()))
                .collect(),
            revision,
        })
    }

    /// Build the tree of a whole state, with `extra_files` -- files kept with
    /// it that are not rows -- and the pack that holds whatever of it history
    /// does not hold yet: what a snapshot of an unrecorded state is made of.
    pub fn build(
        &self,
        state: &State,
        extra_files: &[(String, Vec<u8>)],
        bytes: &mut dyn FnMut(&str) -> Result<Vec<u8>>,
    ) -> Result<(Built, Option<PackFiles>)> {
        let _settled = self.store.settle().map_err(store_error)?;
        let mut pack = PackWriter::new();
        let keep = |pack: &mut PackWriter, id: Id, content: &[u8]| -> Result<()> {
            if Id::of(content) != id {
                return Err(DbError::new(
                    Code::CONCURRENT_MODIFICATION,
                    "a file changed while it was being kept; nothing was written",
                ));
            }
            if !pack.contains(&id) && !self.store.contains(&id).map_err(store_error)? {
                pack.add(ObjectKind::Blob, content.to_vec())
                    .map_err(store_error)?;
            }
            Ok(())
        };
        let mut builder = TreeBuilder::new(
            state.format.as_ref().map(|recorded| recorded.hash),
            state.config.as_ref().map(|recorded| recorded.hash),
        );
        for recorded in state.format.iter().chain(&state.config) {
            let id = Id::of(&recorded.bytes);
            keep(&mut pack, id, &recorded.bytes)?;
            builder.extra(&recorded.path, id).map_err(store_error)?;
        }
        for (table, recorded) in &state.tables {
            builder
                .table(table, Some(recorded.hash))
                .map_err(store_error)?;
            let id = Id::of(&recorded.bytes);
            keep(&mut pack, id, &recorded.bytes)?;
            builder.extra(&recorded.path, id).map_err(store_error)?;
        }
        for entry in &state.rows {
            if !self.store.contains(&entry.blob).map_err(store_error)?
                && !pack.contains(&entry.blob)
            {
                let content = bytes(&entry.path)?;
                keep(&mut pack, entry.blob, &content)?;
            }
            builder.insert(entry.clone()).map_err(store_error)?;
        }
        for (path, content) in extra_files {
            let id = Id::of(content);
            keep(&mut pack, id, content)?;
            builder.extra(path, id).map_err(store_error)?;
        }
        let built = builder.finish().map_err(store_error)?;
        for object in &built.objects {
            if !self.store.contains(&object.id()).map_err(store_error)? {
                pack.add_object(object.clone()).map_err(store_error)?;
            }
        }
        if pack.is_empty() {
            return Ok((built, None));
        }
        let (pack_bytes, idx_bytes) = pack.encode().map_err(store_error)?;
        Ok((built, Some((pack_bytes, idx_bytes, PackName::generate()))))
    }

    /// Whether history holds a revision whose idempotency key is `key`: a
    /// change retried after an uncertain failure is answered by the revision
    /// that made it, not made twice.
    pub fn find_idempotent(&self, key: &str) -> Result<Option<(Id, Revision)>> {
        for (id, revision) in self.log()? {
            if revision.idempotency_key.as_deref() == Some(key) {
                return Ok(Some((id, revision)));
            }
        }
        Ok(None)
    }
}

/// A revision's number in this clone, assigned in the order this clone first
/// saw it: `#12`. Numbers are for people; ids are what history is made of,
/// and are the same in every clone.
/// A pack as written: its bytes, its index's bytes, and its name.
pub type PackFiles = (Vec<u8>, Vec<u8>, PackName);

pub fn number(catalog: &Catalog, history: &History, persist: bool) -> Result<BTreeMap<Id, u64>> {
    let connection = catalog.mirror.connection();
    let corrupt = |error: rusqlite::Error| {
        DbError::new(
            Code::INTERNAL_METADATA_CORRUPT,
            format!("derived mirror: {error}"),
        )
    };
    let mut known: BTreeMap<Id, u64> = BTreeMap::new();
    {
        let mut statement = connection
            .prepare("SELECT id, seq FROM _reldir_revisions")
            .map_err(corrupt)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(corrupt)?;
        for row in rows {
            let (text, seq) = row.map_err(corrupt)?;
            let id = Id::from_hex(&text).ok_or_else(|| {
                DbError::new(
                    Code::INTERNAL_METADATA_CORRUPT,
                    format!("derived mirror: {text:?} is not a revision id"),
                )
            })?;
            known.insert(id, seq as u64);
        }
    }
    // Only revisions recorded since the last numbering lack a number, and
    // they are all newer than any that has one: walk back from the heads and
    // stop at the first numbered ancestor on each path.
    let mut unnumbered: Vec<(Id, Revision)> = vec![];
    let mut seen: BTreeSet<Id> = BTreeSet::new();
    let mut stack: Vec<Id> = history.heads().to_vec();
    while let Some(id) = stack.pop() {
        if known.contains_key(&id) || !seen.insert(id) {
            continue;
        }
        let revision = history.revision(&id)?;
        stack.extend(revision.parents.iter().copied());
        unnumbered.push((id, revision));
    }
    if unnumbered.is_empty() {
        return Ok(known);
    }
    unnumbered.sort_by(|left, right| {
        left.1
            .timestamp
            .cmp(&right.1.timestamp)
            .then_with(|| left.0.cmp(&right.0))
    });
    let first = known.values().max().copied().unwrap_or(0) + 1;
    for (next, (id, _)) in (first..).zip(unnumbered) {
        // A reader that may not write numbers what it sees for itself; the
        // next writer gives the same revisions the same numbers.
        if persist {
            connection
                .execute(
                    "INSERT INTO _reldir_revisions (id, seq) VALUES (?1, ?2)",
                    rusqlite::params![id.to_hex(), next as i64],
                )
                .map_err(corrupt)?;
        }
        known.insert(id, next);
    }
    Ok(known)
}

/// Resolve what a person typed for a revision: `#12` or `12` (this clone's
/// number), or a unique prefix of its id.
pub fn resolve(numbers: &BTreeMap<Id, u64>, text: &str) -> Result<Id> {
    let unknown = || {
        DbError::new(
            Code::UNKNOWN_REVISION,
            format!("there is no revision {text:?}"),
        )
        .with_help("`reldir log` lists them, by number and by id")
    };
    let digits = text.strip_prefix('#').unwrap_or(text);
    if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) && digits.len() < 12 {
        let wanted: u64 = digits.parse().map_err(|_| unknown())?;
        return numbers
            .iter()
            .find(|(_, seq)| **seq == wanted)
            .map(|(id, _)| *id)
            .ok_or_else(unknown);
    }
    let prefix = text.to_ascii_lowercase();
    let matches: Vec<&Id> = numbers
        .keys()
        .filter(|id| prefix.len() >= 4 && id.to_hex().starts_with(&prefix))
        .collect();
    match matches.as_slice() {
        [single] => Ok(**single),
        [] => Err(unknown()),
        _ => Err(DbError::new(
            Code::UNKNOWN_REVISION,
            format!(
                "{text:?} is the beginning of {} revisions' ids",
                matches.len()
            ),
        )
        .with_help("give more of the id")),
    }
}

/// What the mirror remembers the recorded head held at one path: the hash
/// a row, schema, format or configuration entry had, by kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedEntry {
    /// `format`, `config`, `schema` (keyed `schema/<table>.json` wherever the
    /// schema lives) or `row`.
    pub kind: String,
    pub hash: String,
}

/// The hash `.db/format` contributes to a state: of its trimmed text.
pub fn format_hash(text: &str) -> Hash256 {
    Hash256::of(format!("{}\n", text.trim()).as_bytes())
}

/// The hash `.db/config` contributes: of its normalized JSON, so neither its
/// formatting nor its member order is a change of state.
pub fn config_hash(bytes: &[u8]) -> Result<Hash256> {
    let value = crate::json::parse(bytes).map_err(|error| {
        DbError::new(Code::CONFIG_INVALID, format!("invalid .db/config: {error}"))
    })?;
    Ok(Hash256::of(crate::canonical::compact(&value).as_bytes()))
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(DbError::io(path, error)),
    }
}

fn hex256(text: &str) -> Result<Hash256> {
    Hash256::from_hex(text).ok_or_else(|| {
        DbError::new(
            Code::INTERNAL_METADATA_CORRUPT,
            format!("derived mirror: {text:?} is not a digest"),
        )
    })
}

fn hex_id(text: &str) -> Result<Id> {
    Id::from_hex(text).ok_or_else(|| {
        DbError::new(
            Code::INTERNAL_METADATA_CORRUPT,
            format!("derived mirror: {text:?} is not an object id"),
        )
    })
}

/// The non-row entries of the live state: format, configuration, and each
/// table's schema, with the bytes of the file each is read from.
/// The format, the configuration, and each table's schema, as recorded.
type NonRows = (
    Option<Recorded>,
    Option<Recorded>,
    BTreeMap<String, Recorded>,
);

fn live_non_rows(catalog: &Catalog) -> Result<NonRows> {
    let format = read_optional(&catalog.root.join(".db/format"))?.map(|bytes| Recorded {
        path: ".db/format".into(),
        hash: format_hash(&String::from_utf8_lossy(&bytes)),
        bytes,
    });
    let config = match read_optional(&catalog.root.join(".db/config"))? {
        Some(bytes) => Some(Recorded {
            path: ".db/config".into(),
            hash: config_hash(&bytes)?,
            bytes,
        }),
        None => None,
    };
    let mut tables = BTreeMap::new();
    for (table, schema) in &catalog.schemas {
        let file = catalog.schema_files.get(table).ok_or_else(|| {
            DbError::new(
                Code::INTERNAL_ERROR,
                format!("table {table} has a schema but no schema file"),
            )
        })?;
        tables.insert(
            table.clone(),
            Recorded {
                path: crate::catalog::slash(&file.relative),
                hash: hex256(schema.identity())?,
                bytes: file.bytes.clone(),
            },
        );
    }
    Ok((format, config, tables))
}

/// Everything the live state holds, for a recording built from nothing.
pub fn live_state(catalog: &Catalog) -> Result<State> {
    let (format, config, tables) = live_non_rows(catalog)?;
    let mut rows = vec![];
    for table in catalog.schemas.keys() {
        for (path, row_hash, raw_hash) in catalog.mirror.row_entries(table)? {
            rows.push(Entry {
                path,
                row_hash: hex256(&row_hash)?,
                blob: hex_id(&raw_hash)?,
            });
        }
    }
    rows.sort();
    Ok(State {
        format,
        config,
        tables,
        rows,
    })
}

/// How the live state differs from what the mirror says the recorded head
/// holds, and the same change spelled `A path` / `M path` / `D path` for
/// reports. The cost follows what changed: rows are compared inside the
/// mirror through the paths it marked unsettled.
pub fn pending(catalog: &Catalog) -> Result<(Delta, Vec<String>)> {
    let mut delta = Delta::default();
    let mut summary = vec![];
    for change in catalog.mirror.row_changes()? {
        match change.after {
            Some((row_hash, raw_hash)) => {
                let entry = Entry {
                    path: change.path.clone(),
                    row_hash: hex256(&row_hash)?,
                    blob: hex_id(&raw_hash)?,
                };
                if change.recorded {
                    summary.push(format!("M {}", change.path));
                    delta.rows.push(RowDelta::Modified(entry));
                } else {
                    summary.push(format!("A {}", change.path));
                    delta.rows.push(RowDelta::Added(entry));
                }
            }
            None => {
                summary.push(format!("D {}", change.path));
                delta.rows.push(RowDelta::Removed(change.path));
            }
        }
    }
    let recorded = catalog.mirror.recorded_non_rows()?;
    let (format, config, tables) = live_non_rows(catalog)?;
    for (key, live) in [(".db/format", format), (".db/config", config)] {
        let before = recorded.get(key).map(|entry| entry.hash.clone());
        let after = live.as_ref().map(|recorded| recorded.hash.to_hex());
        if before != after {
            summary.push(format!(
                "{} {key}",
                match (&before, &after) {
                    (None, _) => "A",
                    (_, None) => "D",
                    _ => "M",
                }
            ));
            if key == ".db/format" {
                delta.format = Some(live);
            } else {
                delta.config = Some(live);
            }
        }
    }
    // A schema is recorded at the file it is read from, so pinning an
    // inferred contract unchanged is still a change: the schema moved.
    for (table, live) in &tables {
        match recorded.get(&live.path) {
            Some(entry) if entry.hash == live.hash.to_hex() => {}
            before => {
                summary.push(format!(
                    "{} {}",
                    if before.is_some() { "M" } else { "A" },
                    live.path
                ));
                delta.tables.insert(table.clone(), Some(live.clone()));
            }
        }
    }
    for key in recorded.keys() {
        let Some(table) = crate::schema_store::schema_table(Path::new(key)) else {
            continue;
        };
        match tables.get(table) {
            None => {
                summary.push(format!("D {key}"));
                delta.tables.insert(table.to_string(), None);
            }
            // Moved: the table's entry above records where it is now.
            Some(live) if live.path != *key => summary.push(format!("D {key}")),
            Some(_) => {}
        }
    }
    summary.sort_by(|left, right| left[2..].cmp(&right[2..]));
    Ok((delta, summary))
}

/// The two files a table's schema can be read from: its pin, then its
/// inferred contract. Recorded history names a schema by the one it was in.
pub fn schema_homes(table: &str) -> [String; 2] {
    [
        crate::schema_store::pin_relative(table),
        crate::catalog::slash(&crate::schema_store::working_relative(table)),
    ]
}

/// The entries the mirror's recorded set holds for a delta, once recorded.
pub fn recorded_changes(delta: &Delta) -> BTreeMap<String, Option<RecordedEntry>> {
    let mut out = BTreeMap::new();
    for (key, change) in [(".db/format", &delta.format), (".db/config", &delta.config)] {
        if let Some(change) = change {
            out.insert(
                key.to_string(),
                change.as_ref().map(|recorded| RecordedEntry {
                    kind: key.trim_start_matches(".db/").into(),
                    hash: recorded.hash.to_hex(),
                }),
            );
        }
    }
    for (table, change) in &delta.tables {
        for home in schema_homes(table) {
            out.insert(home, None);
        }
        if let Some(recorded) = change {
            out.insert(
                recorded.path.clone(),
                Some(RecordedEntry {
                    kind: "schema".into(),
                    hash: recorded.hash.to_hex(),
                }),
            );
        }
    }
    for row in &delta.rows {
        match row {
            RowDelta::Added(entry) | RowDelta::Modified(entry) => {
                out.insert(
                    entry.path.clone(),
                    Some(RecordedEntry {
                        kind: "row".into(),
                        hash: entry.row_hash.to_hex(),
                    }),
                );
            }
            RowDelta::Removed(path) => {
                out.insert(path.clone(), None);
            }
        }
    }
    out
}

/// Bring the mirror's copy of the recorded head in line with history: by the
/// difference between the tree it describes and the head's, when it still
/// describes one this store holds, or else wholesale.
pub fn sync_recorded(
    catalog: &Catalog,
    history: &History,
    head: Option<(&Id, &Revision)>,
) -> Result<()> {
    let known = catalog.mirror.meta("recorded_head")?;
    let wanted = head.map(|(id, _)| id.to_hex());
    if known == wanted {
        return Ok(());
    }
    let Some((id, revision)) = head else {
        return catalog.mirror.replace_recorded(&BTreeMap::new(), None);
    };
    let previous_tree = catalog
        .mirror
        .meta("recorded_tree")?
        .and_then(|text| Id::from_hex(&text))
        .filter(|tree| history.store.contains(tree).unwrap_or(false));
    let root = store::node::load_root(&history.store, &revision.tree).map_err(store_error)?;
    let mut non_rows: BTreeMap<String, RecordedEntry> = BTreeMap::new();
    if let Some(format) = root.format {
        non_rows.insert(
            ".db/format".into(),
            RecordedEntry {
                kind: "format".into(),
                hash: format.to_hex(),
            },
        );
    }
    if let Some(config) = root.config {
        non_rows.insert(
            ".db/config".into(),
            RecordedEntry {
                kind: "config".into(),
                hash: config.to_hex(),
            },
        );
    }
    let schema_identities = |table: &store::node::TableRef| -> Result<Option<Hash256>> {
        Ok(store::node::load_table(&history.store, table)
            .map_err(store_error)?
            .schema)
    };
    let extras: BTreeSet<String> = history
        .extras(&revision.tree)?
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    for table in &root.tables {
        if let Some(identity) = schema_identities(table)? {
            let home = schema_homes(&table.name)
                .into_iter()
                .find(|home| extras.contains(home))
                .ok_or_else(|| {
                    DbError::new(
                        Code::INTERNAL_METADATA_CORRUPT,
                        format!(
                            "recorded history holds a schema for {} and not the file it was read from",
                            table.name
                        ),
                    )
                })?;
            non_rows.insert(
                home,
                RecordedEntry {
                    kind: "schema".into(),
                    hash: identity.to_hex(),
                },
            );
        }
    }
    match previous_tree {
        Some(previous) => {
            let mut changes: BTreeMap<String, Option<RecordedEntry>> = BTreeMap::new();
            for change in history.diff(&previous, &revision.tree)? {
                changes.insert(
                    change.path,
                    change.after.map(|(row_hash, _)| RecordedEntry {
                        kind: "row".into(),
                        hash: row_hash.to_hex(),
                    }),
                );
            }
            for (key, entry) in catalog.mirror.recorded_non_rows()? {
                if non_rows.get(&key) != Some(&entry) {
                    changes.insert(key.clone(), non_rows.get(&key).cloned());
                }
            }
            for (key, entry) in &non_rows {
                changes
                    .entry(key.clone())
                    .or_insert_with(|| Some(entry.clone()));
            }
            catalog.mirror.apply_recorded(&changes, id, &revision.tree)
        }
        None => {
            let mut entries = non_rows;
            for entry in history.entries(&revision.tree)? {
                entries.insert(
                    entry.path,
                    RecordedEntry {
                        kind: "row".into(),
                        hash: entry.row_hash.to_hex(),
                    },
                );
            }
            catalog
                .mirror
                .replace_recorded(&entries, Some((id, &revision.tree)))
        }
    }
}
