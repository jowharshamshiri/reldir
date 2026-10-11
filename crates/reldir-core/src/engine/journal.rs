//! Writing many files as one change: the commit protocol.
//!
//! Renaming many files is not atomic on any filesystem, so a change is made
//! *recoverable* instead. Everything it will write is made durable first, then
//! one file -- the journal -- is made durable, and that is the moment the change
//! happens: before it, a crash leaves the files exactly as they were; after it,
//! recovery finishes the change from what was made durable.
//!
//! ```text
//! preflight every path, under the lock                      ─ refused: nothing written
//! stage each new file's bytes; fsync them, in parallel      ─┐
//! write the artifacts (history packs, heads); fsync         │ nothing visible:
//! fsync the transaction directory                            │ a crash discards it
//! write the journal (checksummed); fsync it and its dirs   ─┘
//! ── commit point ──
//! for each change: rename the staged file over the target,  ─┐ readers see the new
//!   or remove it -- unless the target moved since planning  │ state through the
//! fsync every directory touched                              │ journal; a crash
//! publish the artifacts; retire superseded ones; fsync       │ rolls it forward
//! remove the journal, then the transaction directory       ─┘
//! ```
//!
//! The journal ends in a SHA-256 of everything before it, so a journal torn by
//! a crash -- or never finished -- is recognised and discarded rather than
//! guessed through. Its changes name the bytes each target held when the change
//! was planned, so a target an outside editor changed after a crash is kept,
//! reported, and recorded as the outside edit it is: never overwritten.
//!
//! A failure after the commit point is never reported as if nothing happened.
//! It is `COMMITTED_RECOVERY_PENDING`: the change is made and visible, only
//! finishing it on disk is outstanding, and retrying the change would apply it
//! twice. A failure to make the journal durable that cannot be retracted is
//! `COMMIT_UNKNOWN`. Following PostgreSQL's lesson ("fsyncgate"), no fsync is
//! ever retried: a failed sync may have dropped the data it was syncing, so a
//! change whose sync failed after the commit point is finished by recovery,
//! which writes the bytes afresh before syncing them.

use crate::{
    code::Code,
    diagnostic::{DbError, Diagnostic, Result},
    engine::fs::{Durability, Fs, Kind, Source},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

/// The journal format this binary reads and writes.
const JOURNAL_VERSION: u32 = 3;
const TRAILER: &str = "#reldir-journal-end ";
const JOURNAL: &str = "journal";
/// Marks a committed transaction whose post-commit sync failed: recovery must
/// write its files afresh, not trust pages that may have been dropped.
const SYNC_FAILED: &str = "sync-failed";

/// What a file held, identified by content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Image {
    pub sha256: String,
    pub size: u64,
}

impl Image {
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            sha256: crate::canonical::hash_bytes(bytes),
            size: bytes.len() as u64,
        }
    }
}

/// One file a change writes or removes, relative to the database root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Create or replace a file. `pre` is what the file held when the change
    /// was planned, `None` for a file that did not exist.
    Write {
        path: PathBuf,
        bytes: Vec<u8>,
        pre: Option<Image>,
    },
    /// Remove a file that held `pre` when the change was planned.
    Delete { path: PathBuf, pre: Image },
}

impl Change {
    pub fn path(&self) -> &Path {
        match self {
            Self::Write { path, .. } | Self::Delete { path, .. } => path,
        }
    }
}

/// A file of reldir's own metadata a change publishes once its rows are in
/// place: a history pack, a head. Published by rename from the transaction
/// directory, so it appears whole or not at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub bytes: Vec<u8>,
    /// Where it is published, relative to the root, inside `.db/`.
    pub to: PathBuf,
}

/// A change, planned and ready to commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// How the change came about, as history records it.
    pub origin: String,
    /// The state root the change was planned against.
    pub planned_against: Option<String>,
    /// The revision recording the change, when it records one.
    pub revision: Option<String>,
    /// A caller's name for the change, so a retry is recognised as one.
    pub idempotency_key: Option<String>,
    pub changes: Vec<Change>,
    pub artifacts: Vec<Artifact>,
    /// Metadata files the change supersedes, removed after the artifacts are
    /// published: a head the new revision descends from.
    pub retire: Vec<PathBuf>,
    /// Directories the change leaves governing nothing -- a dropped table's
    /// -- removed once its changes are made, when nothing is left in them.
    pub prune: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalChange {
    /// The staged file's name in the transaction directory, for a write.
    pub n: Option<usize>,
    pub path: PathBuf,
    pub pre: Option<Image>,
    /// What the target holds once the change is made; `None` for a removal.
    pub post: Option<Image>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalArtifact {
    pub n: usize,
    pub to: PathBuf,
    pub post: Image,
}

/// What recovery needs to finish a committed change, and what a reader needs
/// to see the change before it is finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub reldir_journal: u32,
    pub id: String,
    pub origin: String,
    pub created: String,
    pub planned_against: Option<String>,
    pub revision: Option<String>,
    pub idempotency_key: Option<String>,
    pub changes: Vec<JournalChange>,
    pub artifacts: Vec<JournalArtifact>,
    pub retire: Vec<PathBuf>,
    pub prune: Vec<PathBuf>,
}

/// Where transactions are staged.
pub fn store(root: &Path) -> PathBuf {
    root.join(".db/transactions")
}

fn io(path: &Path, error: std::io::Error) -> DbError {
    DbError::io(path, error)
}

fn incomplete(message: impl Into<String>) -> DbError {
    DbError::new(Code::TRANSACTION_INCOMPLETE, message)
}

/// A transaction recovery can neither finish nor discard, named by its
/// directory, with what to do about it: no command may run while it stands,
/// since it may hold a committed change that is not yet in place.
fn unresolvable(error: DbError, root: &Path, dir: &Path) -> DbError {
    if error.code() != Code::TRANSACTION_INCOMPLETE {
        return error;
    }
    let relative = crate::catalog::relative(root, dir);
    let mut diagnostic = *error.diagnostic;
    diagnostic.path = Some(relative.clone());
    DbError::from_diag(diagnostic).with_help(format!(
        "move {} out of .db/transactions/ to let reldir run again. A journal in it lists every \
         file the change wrote, with the bytes committed for each: compare them with the files, \
         and make what is missing as an ordinary change",
        crate::catalog::slash(&relative)
    ))
}

/// Refuse a path a change may not write: one that escapes the root, or reaches
/// into reldir's metadata other than the configuration and working schemas.
pub fn safe_relative(path: &Path) -> Result<()> {
    if !plain_relative(path) || (path.starts_with(".db") && !governed_metadata(path)) {
        return Err(DbError::new(
            Code::PATH_VIOLATION,
            format!("{} is not a path a change may write", path.display()),
        ));
    }
    Ok(())
}

fn plain_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

/// `.db/config`, and the inferred contracts in `.db/schema/`: metadata that
/// is part of the database's state, written like rows.
fn governed_metadata(path: &Path) -> bool {
    path == Path::new(".db/config")
        || (path.parent() == Some(Path::new(".db/schema"))
            && path.extension().and_then(|value| value.to_str()) == Some("json"))
}

/// Where an artifact may be published: history's own directories.
fn artifact_destination(path: &Path) -> bool {
    plain_relative(path)
        && (path.starts_with(".db/objects/pack") || path.starts_with(".db/history"))
}

/// What a commit did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Committed {
    pub id: String,
    /// Targets an outside editor changed after the change was planned. The
    /// change is committed; these files keep the outside edit, which the next
    /// observation records.
    pub kept_outside_edits: Vec<PathBuf>,
}

/// Observes a commit's progress, so derived state follows it.
pub trait Observer {
    /// The journal is durable: the change has happened, and readers must see
    /// it from now on.
    fn committed(&mut self, journal: &Journal) -> Result<()>;
    /// Every file is in place and durable.
    fn applied(&mut self, journal: &Journal) -> Result<()>;
}

/// An observer for callers with no derived state to keep.
pub struct Unobserved;

impl Observer for Unobserved {
    fn committed(&mut self, _journal: &Journal) -> Result<()> {
        Ok(())
    }
    fn applied(&mut self, _journal: &Journal) -> Result<()> {
        Ok(())
    }
}

/// Bytes recorded in history, by content hash: what recovery writes a file
/// back from when the copy it staged is gone and a failed sync may have
/// dropped the one in place.
pub trait Blobs {
    fn blob(&self, sha256: &str) -> Option<Vec<u8>>;
}

/// No history to draw on.
pub struct NoBlobs;

impl Blobs for NoBlobs {
    fn blob(&self, _sha256: &str) -> Option<Vec<u8>> {
        None
    }
}

/// Check a plan against the files before anything is written. Every refusal
/// here means nothing was written.
pub fn preflight(fs: &dyn Fs, root: &Path, plan: &Plan, max_bytes: u64) -> Result<()> {
    if plan.changes.is_empty() && plan.artifacts.is_empty() && plan.prune.is_empty() {
        return Ok(());
    }
    if crate::store::revision::Origin::parse(&plan.origin).is_none() {
        return Err(DbError::new(
            Code::INTERNAL_ERROR,
            format!("a change names the unknown origin {:?}", plan.origin),
        ));
    }
    let mut paths = BTreeSet::new();
    let mut bytes: u64 = 0;
    for change in &plan.changes {
        safe_relative(change.path())?;
        if !paths.insert(change.path().to_path_buf()) {
            return Err(DbError::new(
                Code::MUTATION_CONFLICT,
                format!(
                    "the change names {} more than once",
                    change.path().display()
                ),
            ));
        }
        if let Change::Write { bytes: written, .. } = change {
            bytes = bytes.saturating_add(written.len() as u64);
        }
    }
    for artifact in &plan.artifacts {
        if !artifact_destination(&artifact.to) || !paths.insert(artifact.to.clone()) {
            return Err(DbError::new(
                Code::INTERNAL_ERROR,
                format!(
                    "an artifact cannot be published to {}",
                    artifact.to.display()
                ),
            ));
        }
        bytes = bytes.saturating_add(artifact.bytes.len() as u64);
    }
    for retired in &plan.retire {
        if !artifact_destination(retired) {
            return Err(DbError::new(
                Code::INTERNAL_ERROR,
                format!("{} is not a file a change may retire", retired.display()),
            ));
        }
    }
    for directory in &plan.prune {
        if !prunable(directory) {
            return Err(DbError::new(
                Code::INTERNAL_ERROR,
                format!(
                    "{} is not a directory a change may prune",
                    directory.display()
                ),
            ));
        }
    }
    if bytes > max_bytes {
        return Err(DbError::new(
            Code::RESOURCE_LIMIT,
            format!("the change writes {bytes} bytes, beyond the {max_bytes} byte limit"),
        ));
    }
    // Transactions are staged inside `.db/`, so every target must be on its
    // device for a staged file to be renamed into place.
    // A dry run may plan for a folder not yet established: its `.db/` will
    // be made inside the root, on the root's device.
    let metadata = root.join(".db");
    let staging_device = match fs.volume(&metadata) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs.volume(root).map_err(|error| io(root, error))?
        }
        other => other.map_err(|error| io(&metadata, error))?,
    }
    .device;
    let mut volumes = BTreeMap::new();
    let targets = plan
        .changes
        .iter()
        .map(|change| (change.path(), Some(change)))
        .chain(
            plan.artifacts
                .iter()
                .map(|artifact| (artifact.to.as_path(), None)),
        );
    for (relative, change) in targets {
        let target = root.join(relative);
        // Walk down from the root: every existing ancestor must be a real
        // directory, and the first missing one -- created after the commit
        // point -- must be creatable where it will be.
        let mut existing = root.to_path_buf();
        let mut missing: Vec<&std::ffi::OsStr> = vec![];
        let components: Vec<_> = relative.components().collect();
        for component in &components[..components.len() - 1] {
            let Component::Normal(name) = component else {
                unreachable!("safe paths have only normal components")
            };
            if !missing.is_empty() {
                missing.push(name);
                continue;
            }
            let next = existing.join(name);
            match fs.metadata(&next) {
                Ok(meta) if meta.kind == Kind::Dir => existing = next,
                Ok(_) => {
                    return Err(DbError::new(
                        Code::PATH_INTERFERENCE,
                        format!(
                            "{} is not a directory, so {} cannot be placed; nothing was written",
                            next.display(),
                            relative.display()
                        ),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing.push(name),
                Err(error) => return Err(io(&next, error)),
            }
        }
        let volume = match volumes.get(&existing) {
            Some(volume) => volume,
            None => {
                let volume = fs.volume(&existing).map_err(|error| io(&existing, error))?;
                volumes.entry(existing.clone()).or_insert(volume)
            }
        };
        if volume.device != staging_device {
            return Err(DbError::new(
                Code::CROSS_DEVICE,
                format!(
                    "{} is on another device than .db/, so a change cannot be renamed into it \
                     atomically",
                    existing.display()
                ),
            ));
        }
        let name = relative.file_name().unwrap_or_default();
        for component in missing.iter().chain(std::iter::once(&name)) {
            if component.len() as u64 > volume.name_max {
                return Err(DbError::new(
                    Code::FILENAME_TOO_LONG,
                    format!(
                        "{:?} is {} bytes, longer than the {} this filesystem allows; nothing was \
                         written",
                        component,
                        component.len(),
                        volume.name_max
                    ),
                ));
            }
        }
        if missing.is_empty() {
            match fs.metadata(&target) {
                Ok(meta) if meta.kind == Kind::File => {}
                Ok(_) => {
                    return Err(DbError::new(
                        Code::PATH_INTERFERENCE,
                        format!(
                            "{} is not a regular file, so the change cannot replace it; nothing \
                             was written",
                            relative.display()
                        ),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io(&target, error)),
            }
        }
        // The planned pre-image must still be what the target holds: a file
        // that moved since the change was planned would otherwise be
        // overwritten by a change computed from what it used to say.
        if let Some(change) = change {
            let expected = match change {
                Change::Write { pre, .. } => pre.clone(),
                Change::Delete { pre, .. } => Some(pre.clone()),
            };
            if current_image(fs, &target)? != expected {
                return Err(DbError::new(
                    Code::CONCURRENT_MODIFICATION,
                    format!(
                        "{} changed after the change was planned; nothing was written, and \
                         retrying plans it again",
                        relative.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// What a file holds now, by content.
fn current_image(source: &dyn Source, path: &Path) -> Result<Option<Image>> {
    match source.read(path) {
        Ok(bytes) => Ok(Some(Image::of(&bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io(path, error)),
    }
}

/// The journal's bytes: canonical JSON, then the trailer checksumming it.
fn encode(journal: &Journal) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(journal).expect("a journal serializes");
    bytes.push(b'\n');
    let trailer = format!("{TRAILER}{}\n", crate::canonical::hash_bytes(&bytes));
    bytes.extend_from_slice(trailer.as_bytes());
    bytes
}

/// Read a journal's bytes back. `Ok(None)` when the trailer is absent or does
/// not match: the journal was never finished, so its change never happened.
/// An error when a finished journal cannot be interpreted -- a committed
/// change this binary cannot read, which must not be guessed through.
fn decode(bytes: &[u8]) -> Result<Option<Journal>> {
    let Some(at) = bytes
        .windows(TRAILER.len())
        .rposition(|window| window == TRAILER.as_bytes())
    else {
        return Ok(None);
    };
    let (body, trailer) = bytes.split_at(at);
    let Ok(trailer) = std::str::from_utf8(&trailer[TRAILER.len()..]) else {
        return Ok(None);
    };
    if trailer.trim_end_matches('\n') != crate::canonical::hash_bytes(body)
        || !trailer.ends_with('\n')
    {
        return Ok(None);
    }
    let journal: Journal = crate::json::parse_as(body).map_err(|error| {
        incomplete(format!(
            "a committed transaction's journal cannot be interpreted: {error}"
        ))
    })?;
    if journal.reldir_journal != JOURNAL_VERSION {
        return Err(incomplete(format!(
            "a committed transaction was written by journal format {}, which this binary does not \
             read; finish it with the binary that wrote it",
            journal.reldir_journal
        )));
    }
    validate(&journal)?;
    Ok(Some(journal))
}

fn validate(journal: &Journal) -> Result<()> {
    if ulid::Ulid::from_string(&journal.id).is_err() {
        return Err(incomplete(format!(
            "a committed transaction has the malformed id {:?}",
            journal.id
        )));
    }
    let mut paths = BTreeSet::new();
    let mut staged = BTreeSet::new();
    for change in &journal.changes {
        safe_relative(&change.path).map_err(|_| {
            incomplete(format!(
                "a committed transaction names the unsafe path {}",
                change.path.display()
            ))
        })?;
        let consistent = match (change.n, &change.post) {
            (Some(n), Some(_)) => staged.insert(n),
            (None, None) => change.pre.is_some(),
            _ => false,
        };
        if !paths.insert(change.path.clone()) || !consistent {
            return Err(incomplete(format!(
                "a committed transaction describes {} inconsistently",
                change.path.display()
            )));
        }
    }
    for artifact in &journal.artifacts {
        if !artifact_destination(&artifact.to)
            || !staged.insert(artifact.n)
            || !paths.insert(artifact.to.clone())
        {
            return Err(incomplete(format!(
                "a committed transaction publishes {} inconsistently",
                artifact.to.display()
            )));
        }
    }
    if journal
        .retire
        .iter()
        .any(|path| !artifact_destination(path))
    {
        return Err(incomplete(
            "a committed transaction retires a path it may not",
        ));
    }
    if journal
        .prune
        .iter()
        .any(|path| !prunable(path) || paths.contains(path))
    {
        return Err(incomplete(
            "a committed transaction prunes a directory it may not",
        ));
    }
    Ok(())
}

/// A directory a change may prune: a top-level directory of the folder.
fn prunable(path: &Path) -> bool {
    plain_relative(path)
        && path.components().count() == 1
        && !path.starts_with(".db")
        && !path.starts_with("schema")
}

fn staged_name(n: usize) -> String {
    n.to_string()
}

/// Commit a plan. The caller holds the writer lock and has judged the state
/// the plan produces; `preflight` has accepted the plan.
pub fn commit(
    fs: &dyn Fs,
    root: &Path,
    plan: &Plan,
    observer: &mut dyn Observer,
) -> Result<Committed> {
    let id = ulid::Ulid::new().to_string();
    if plan.changes.is_empty() && plan.artifacts.is_empty() && plan.prune.is_empty() {
        return Ok(Committed {
            id,
            kept_outside_edits: vec![],
        });
    }
    let transactions = store(root);
    ensure_dir(fs, &transactions)?;
    let dir = transactions.join(&id);
    fs.create_dir(&dir).map_err(|error| io(&dir, error))?;
    let staged = (|| -> Result<Journal> {
        let mut changes = vec![];
        let mut writes = vec![];
        let mut next = 0;
        for change in &plan.changes {
            match change {
                Change::Write { path, bytes, pre } => {
                    writes.push((dir.join(staged_name(next)), bytes.as_slice()));
                    changes.push(JournalChange {
                        n: Some(next),
                        path: path.clone(),
                        pre: pre.clone(),
                        post: Some(Image::of(bytes)),
                    });
                    next += 1;
                }
                Change::Delete { path, pre } => changes.push(JournalChange {
                    n: None,
                    path: path.clone(),
                    pre: Some(pre.clone()),
                    post: None,
                }),
            }
        }
        let mut artifacts = vec![];
        for artifact in &plan.artifacts {
            writes.push((dir.join(staged_name(next)), artifact.bytes.as_slice()));
            artifacts.push(JournalArtifact {
                n: next,
                to: artifact.to.clone(),
                post: Image::of(&artifact.bytes),
            });
            next += 1;
        }
        stage_all(fs, &writes)?;
        fs.sync_dir(&dir).map_err(|error| io(&dir, error))?;
        Ok(Journal {
            reldir_journal: JOURNAL_VERSION,
            id: id.clone(),
            origin: plan.origin.clone(),
            created: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            planned_against: plan.planned_against.clone(),
            revision: plan.revision.clone(),
            idempotency_key: plan.idempotency_key.clone(),
            changes,
            artifacts,
            retire: plan.retire.clone(),
            prune: plan.prune.clone(),
        })
    })();
    let journal = match staged {
        Ok(journal) => journal,
        Err(error) => {
            // Nothing is visible: take the staging away. A failure to do so
            // leaves a directory without a journal, which recovery discards.
            let _ = discard(fs, &transactions, &dir);
            return Err(error);
        }
    };
    let journal_path = dir.join(JOURNAL);
    let durable = fs
        .create_synced(&journal_path, &encode(&journal), Durability::Full)
        .and_then(|()| fs.sync_dir(&dir))
        .and_then(|()| fs.sync_dir(&transactions));
    if let Err(error) = durable {
        // The journal may or may not have reached the disk. Retract it; if
        // even that cannot be made durable, nobody can say whether the change
        // happened, and saying anything else would be a guess.
        let retracted = match fs.remove_file(&journal_path) {
            Ok(()) => fs.sync_dir(&dir),
            Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => fs.sync_dir(&dir),
            Err(other) => Err(other),
        };
        return Err(match retracted {
            Ok(()) => {
                let _ = discard(fs, &transactions, &dir);
                io(&journal_path, error)
            }
            Err(_) => DbError::from_diag(
                Diagnostic::error(
                    Code::COMMIT_UNKNOWN,
                    format!(
                        "making transaction {id} durable failed ({error}) and could not be \
                         retracted; whether the change was made is unknown"
                    ),
                )
                .help(
                    "run `reldir recover`: it finishes the change if its journal reached the disk \
                     and discards it otherwise; then look before retrying, or retry with the same \
                     --idempotency-key",
                ),
            ),
        });
    }
    // The change has happened. Every failure from here on says so.
    let pending = |reason: String| {
        DbError::from_diag(
            Diagnostic::error(
                Code::COMMITTED_RECOVERY_PENDING,
                format!("transaction {id} is committed, but {reason}"),
            )
            .help(
                "the change is made and every reader sees it; the next writing command or \
                 `reldir recover` finishes it on disk. Do not retry the change.",
            ),
        )
    };
    observer.committed(&journal).map_err(|error| {
        pending(format!(
            "derived state could not follow it: {}",
            error.diagnostic.message
        ))
    })?;
    let applied = apply(fs, root, &dir, &journal, false, false, &NoBlobs);
    let report = match applied {
        Ok(report) => report,
        Err(Failure::Sync(error)) => {
            let _ = fs.create_synced(&dir.join(SYNC_FAILED), b"", Durability::Full);
            return Err(pending(format!(
                "syncing it failed: {}",
                error.diagnostic.message
            )));
        }
        Err(Failure::Blocked(blocked)) => {
            return Err(pending(format!(
                "{} could not be placed: {}",
                blocked.0.display(),
                blocked.1
            )));
        }
        Err(Failure::Other(error)) => return Err(pending(error.diagnostic.message.clone())),
    };
    observer.applied(&journal).map_err(|error| {
        pending(format!(
            "derived state could not follow it: {}",
            error.diagnostic.message
        ))
    })?;
    finish(fs, &transactions, &dir, &journal).map_err(|error| {
        pending(format!(
            "its staging could not be removed: {}",
            error.diagnostic.message
        ))
    })?;
    Ok(Committed {
        id,
        kept_outside_edits: report.kept,
    })
}

/// Stage files and make them durable together, so a large change costs
/// about one flush rather than one per file.
fn stage_all(fs: &dyn Fs, writes: &[(PathBuf, &[u8])]) -> Result<()> {
    for (path, bytes) in writes {
        fs.create_new(path, bytes)
            .map_err(|error| io(path, error))?;
    }
    let paths: Vec<&Path> = writes.iter().map(|(path, _)| path.as_path()).collect();
    match paths.first() {
        Some(first) => fs
            .sync_files(&paths, Durability::Full)
            .map_err(|error| io(first, error)),
        None => Ok(()),
    }
}

/// Why finishing a committed change stopped.
enum Failure {
    /// A sync failed: the data it covered may be lost, and must be written
    /// afresh before it is synced again.
    Sync(DbError),
    /// One target could not be placed, for a reason a retry meets again.
    Blocked((PathBuf, String)),
    Other(DbError),
}

struct Applied {
    kept: Vec<PathBuf>,
}

/// Finish a committed change: put every file in place, make it durable, and
/// publish the artifacts. Idempotent: a crash anywhere in it is recovered by
/// running it again. With `rewrite`, every target is written afresh from the
/// bytes the change committed, because a failed sync may have dropped them.
fn apply(
    fs: &dyn Fs,
    root: &Path,
    dir: &Path,
    journal: &Journal,
    recovering: bool,
    rewrite: bool,
    blobs: &dyn Blobs,
) -> std::result::Result<Applied, Failure> {
    let mut kept = vec![];
    let mut touched: BTreeSet<PathBuf> = BTreeSet::new();
    if recovering {
        // An interrupted run may have created a directory without making its
        // entry durable. Before anything is placed in one, every existing
        // directory between the root and each target is made durable.
        let mut ancestors = BTreeSet::new();
        for path in journal
            .changes
            .iter()
            .map(|change| &change.path)
            .chain(journal.artifacts.iter().map(|artifact| &artifact.to))
        {
            let mut current = root.join(path);
            while let Some(parent) = current.parent() {
                if !parent.starts_with(root) {
                    break;
                }
                if fs.metadata(parent).is_ok_and(|meta| meta.kind == Kind::Dir) {
                    ancestors.insert(parent.to_path_buf());
                }
                if parent == root {
                    break;
                }
                current = parent.to_path_buf();
            }
        }
        sync_all(fs, &ancestors)?;
    }
    let blocked = |path: &Path, error: std::io::Error| {
        Failure::Blocked((path.to_path_buf(), error.to_string()))
    };
    for change in &journal.changes {
        let target = root.join(&change.path);
        let parent = target.parent().unwrap_or(root).to_path_buf();
        // Every changed directory is synced, whether this run or one a crash
        // or a failure interrupted put the file there: a rename that is in
        // place is not durable until its directory is.
        if fs
            .metadata(&parent)
            .is_ok_and(|meta| meta.kind == Kind::Dir)
        {
            touched.insert(parent.clone());
        }
        let now = current_image(fs, &target).map_err(Failure::Other)?;
        match (&change.post, change.n) {
            (Some(post), Some(n)) => {
                let staged = dir.join(staged_name(n));
                if now.as_ref() == Some(post) {
                    if rewrite {
                        rewrite_in_place(fs, dir, &target, post, &staged, blobs)?;
                        touched.insert(parent);
                    }
                    continue;
                }
                if now != change.pre {
                    // Edited outside reldir after the change was planned:
                    // the change is committed, and the edit made after it is
                    // kept. Recording it is the next observation's job.
                    kept.push(change.path.clone());
                    continue;
                }
                create_parents(fs, root, &parent, &mut touched)?;
                match fs.rename(&staged, &target) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        // The staged copy is gone but the target does not hold
                        // the committed bytes: write them from history.
                        let bytes = blobs.blob(&post.sha256).ok_or_else(|| {
                            Failure::Other(incomplete(format!(
                                "transaction {} lost its staged copy of {} and history has no \
                                 copy of it",
                                journal.id,
                                change.path.display()
                            )))
                        })?;
                        place_fresh(fs, dir, &target, &bytes, n)?;
                    }
                    Err(error) => return Err(blocked(&target, error)),
                }
                touched.insert(parent);
            }
            (None, None) => {
                if now.is_none() {
                    continue;
                }
                if now != change.pre {
                    kept.push(change.path.clone());
                    continue;
                }
                match fs.remove_file(&target) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(blocked(&target, error)),
                }
                touched.insert(parent);
            }
            _ => unreachable!("validated journals pair a staged file with its image"),
        }
    }
    for directory in &journal.prune {
        let target = root.join(directory);
        let entries = match fs.read_dir(&target) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(blocked(&target, error)),
        };
        // Whatever is left in it -- a file the change did not govern -- is
        // not the change's to remove: the directory stays.
        if !entries.is_empty() {
            continue;
        }
        match fs.remove_dir(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(blocked(&target, error)),
        }
        // Its removal is made durable by syncing the root; it has nothing
        // left to sync itself.
        touched.remove(&target);
        touched.insert(root.to_path_buf());
    }
    sync_all(fs, &touched)?;
    let mut published: BTreeSet<PathBuf> = BTreeSet::new();
    for artifact in &journal.artifacts {
        let to = root.join(&artifact.to);
        let parent = to.parent().unwrap_or(root).to_path_buf();
        if fs
            .metadata(&parent)
            .is_ok_and(|meta| meta.kind == Kind::Dir)
        {
            published.insert(parent.clone());
        }
        if current_image(fs, &to).map_err(Failure::Other)?.as_ref() == Some(&artifact.post) {
            continue;
        }
        create_parents(fs, root, &parent, &mut published)?;
        fs.rename(&dir.join(staged_name(artifact.n)), &to)
            .map_err(|error| blocked(&to, error))?;
        published.insert(parent);
    }
    for retired in &journal.retire {
        let path = root.join(retired);
        match fs.remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(blocked(&path, error)),
        }
        let parent = path.parent().unwrap_or(root).to_path_buf();
        if fs
            .metadata(&parent)
            .is_ok_and(|meta| meta.kind == Kind::Dir)
        {
            published.insert(parent);
        }
    }
    sync_all(fs, &published)?;
    Ok(Applied { kept })
}

/// Write `bytes` beside `target` and rename it over, durably.
fn place_fresh(
    fs: &dyn Fs,
    dir: &Path,
    target: &Path,
    bytes: &[u8],
    n: usize,
) -> std::result::Result<(), Failure> {
    let fresh = dir.join(format!("{}.fresh", staged_name(n)));
    match fs.remove_file(&fresh) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Failure::Blocked((fresh, error.to_string()))),
    }
    fs.create_synced(&fresh, bytes, Durability::Full)
        .map_err(|error| Failure::Sync(io(&fresh, error)))?;
    fs.rename(&fresh, target)
        .map_err(|error| Failure::Blocked((target.to_path_buf(), error.to_string())))
}

/// After a failed sync, write a target that holds the committed bytes afresh,
/// so the bytes synced are ones this process wrote rather than pages a failed
/// writeback may have marked clean without storing.
fn rewrite_in_place(
    fs: &dyn Fs,
    dir: &Path,
    target: &Path,
    post: &Image,
    staged: &Path,
    blobs: &dyn Blobs,
) -> std::result::Result<(), Failure> {
    let bytes = match fs.read(target) {
        Ok(bytes) if Image::of(&bytes) == *post => bytes,
        _ => blobs.blob(&post.sha256).ok_or_else(|| {
            Failure::Other(incomplete(format!(
                "{} must be written again after a failed sync, and no copy of its bytes survives",
                target.display()
            )))
        })?,
    };
    let n = staged
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.parse().ok())
        .unwrap_or(usize::MAX);
    place_fresh(fs, dir, target, &bytes, n)
}

/// Create the missing directories between the root and `directory`, each made
/// durable in its parent before anything is placed inside it: a file renamed
/// into a directory whose own creation a crash lost would be lost with it.
fn create_parents(
    fs: &dyn Fs,
    root: &Path,
    directory: &Path,
    touched: &mut BTreeSet<PathBuf>,
) -> std::result::Result<(), Failure> {
    let relative = directory.strip_prefix(root).unwrap_or(directory);
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let parent = current.clone();
        current.push(component);
        match fs.metadata(&current) {
            Ok(meta) if meta.kind == Kind::Dir => {}
            Ok(_) => {
                return Err(Failure::Blocked((
                    current,
                    "a file stands where a directory is needed".into(),
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs.create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(Failure::Blocked((current, error.to_string()))),
                }
                fs.sync_dir(&parent)
                    .map_err(|error| Failure::Sync(io(&parent, error)))?;
                touched.insert(parent);
            }
            Err(error) => return Err(Failure::Blocked((current, error.to_string()))),
        }
    }
    Ok(())
}

fn sync_all(fs: &dyn Fs, directories: &BTreeSet<PathBuf>) -> std::result::Result<(), Failure> {
    for directory in directories {
        fs.sync_dir(directory)
            .map_err(|error| Failure::Sync(io(directory, error)))?;
    }
    Ok(())
}

fn ensure_dir(fs: &dyn Fs, path: &Path) -> Result<()> {
    match fs.metadata(path) {
        Ok(meta) if meta.kind == Kind::Dir => Ok(()),
        Ok(_) => Err(DbError::new(
            Code::INTERNAL_METADATA_CORRUPT,
            format!("{} must be a directory", path.display()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs.create_dir(path).map_err(|error| io(path, error))?;
            let parent = path.parent().unwrap_or(path);
            fs.sync_dir(parent).map_err(|error| io(parent, error))
        }
        Err(error) => Err(io(path, error)),
    }
}

/// Take a finished transaction away: the journal first, so a crash midway
/// leaves a directory without one, which recovery discards.
fn finish(fs: &dyn Fs, transactions: &Path, dir: &Path, journal: &Journal) -> Result<()> {
    match fs.remove_file(&dir.join(JOURNAL)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io(dir, error)),
    }
    let _ = journal;
    discard(fs, transactions, dir)
}

/// Remove a transaction directory that holds no journal: staged files no
/// commit reached, or the leftovers of a finished one.
fn discard(fs: &dyn Fs, transactions: &Path, dir: &Path) -> Result<()> {
    let entries = match fs.read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io(dir, error)),
    };
    for entry in entries {
        if entry.meta.kind != Kind::File {
            return Err(incomplete(format!(
                "the transaction holds {}, which no transaction stages; look at it before \
                 recovery removes the transaction",
                entry.path.file_name().map_or_else(
                    || entry.path.display().to_string(),
                    |name| { name.to_string_lossy().into_owned() }
                )
            )));
        }
        match fs.remove_file(&entry.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(&entry.path, error)),
        }
    }
    match fs.remove_dir(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io(dir, error)),
    }
    let _ = fs.sync_dir(transactions);
    Ok(())
}

/// A committed transaction not yet finished on disk, as a reader sees it.
#[derive(Debug, Clone)]
pub struct Pending {
    pub dir: PathBuf,
    pub journal: Journal,
}

impl Pending {
    /// What a changed path holds once the change is finished: `Some(bytes)`
    /// for a write, `None` for a removal. Read from the staged copy while it is
    /// there, and from the target once it holds the committed bytes.
    pub fn post(
        &self,
        source: &dyn Source,
        root: &Path,
        change: &JournalChange,
    ) -> Result<Option<Vec<u8>>> {
        let (Some(post), Some(n)) = (&change.post, change.n) else {
            return Ok(None);
        };
        for candidate in [self.dir.join(staged_name(n)), root.join(&change.path)] {
            match source.read(&candidate) {
                Ok(bytes) if Image::of(&bytes) == *post => return Ok(Some(bytes)),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io(&candidate, error)),
            }
        }
        Err(incomplete(format!(
            "transaction {} committed {}, and neither its staged copy nor the file holds the \
             committed bytes",
            self.journal.id,
            change.path.display()
        )))
    }
}

/// Things in `.db/transactions` that are not transactions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Strays(pub Vec<PathBuf>);

/// The committed transactions not yet finished, for readers that must see
/// them, and anything in the staging area that is not a transaction.
pub fn pending(source: &dyn Source, root: &Path) -> Result<(Vec<Pending>, Strays)> {
    let transactions = store(root);
    let entries = match source.read_dir(&transactions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((vec![], Strays::default()));
        }
        Err(error) => return Err(io(&transactions, error)),
    };
    let mut out = vec![];
    let mut strays = Strays::default();
    for entry in entries {
        let is_transaction = entry.meta.kind == Kind::Dir
            && entry
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| ulid::Ulid::from_string(name).is_ok());
        if !is_transaction {
            strays.0.push(entry.path);
            continue;
        }
        let path = entry.path.join(JOURNAL);
        match source.read(&path) {
            Ok(bytes) => {
                if let Some(journal) = decode(&bytes)? {
                    out.push(Pending {
                        dir: entry.path,
                        journal,
                    });
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(&path, error)),
        }
    }
    Ok((out, strays))
}

/// What recovery did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recovered {
    /// Committed transactions finished, with their journals.
    pub completed: Vec<Journal>,
    /// Transactions discarded because they never committed.
    pub discarded: Vec<String>,
    /// Targets edited outside reldir after a committed change, kept as edited.
    pub kept_outside_edits: Vec<PathBuf>,
    /// Committed transactions that could not be finished, with why. Their
    /// journals stay; readers keep seeing their changes.
    pub blocked: Vec<(String, PathBuf, String)>,
    pub strays: Strays,
}

/// Finish every committed transaction and discard every other. The caller
/// holds the writer lock, so no live writer owns them.
pub fn recover(
    fs: &dyn Fs,
    root: &Path,
    blobs: &dyn Blobs,
    observer: &mut dyn Observer,
) -> Result<Recovered> {
    let transactions = store(root);
    let entries = match fs.read_dir(&transactions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Recovered::default());
        }
        Err(error) => return Err(io(&transactions, error)),
    };
    let mut outcome = Recovered::default();
    for entry in entries {
        let name = entry
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        if entry.meta.kind != Kind::Dir || ulid::Ulid::from_string(&name).is_err() {
            outcome.strays.0.push(entry.path);
            continue;
        }
        let dir = entry.path;
        let journal = match fs.read(&dir.join(JOURNAL)) {
            Ok(bytes) => decode(&bytes).map_err(|error| unresolvable(error, root, &dir))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(io(&dir, error)),
        };
        let Some(journal) = journal else {
            discard(fs, &transactions, &dir).map_err(|error| unresolvable(error, root, &dir))?;
            outcome.discarded.push(name);
            continue;
        };
        if journal.id != name {
            return Err(unresolvable(
                incomplete(format!(
                    "transaction directory {name} holds the journal of {}",
                    journal.id
                )),
                root,
                &dir,
            ));
        }
        let rewrite = fs.metadata(&dir.join(SYNC_FAILED)).is_ok();
        observer.committed(&journal)?;
        match apply(fs, root, &dir, &journal, true, rewrite, blobs) {
            Ok(applied) => {
                observer.applied(&journal)?;
                finish(fs, &transactions, &dir, &journal)?;
                outcome.kept_outside_edits.extend(applied.kept);
                outcome.completed.push(journal);
            }
            Err(Failure::Blocked((path, why))) => outcome.blocked.push((name, path, why)),
            Err(Failure::Sync(error)) => {
                let _ = fs.create_synced(&dir.join(SYNC_FAILED), b"", Durability::Full);
                outcome
                    .blocked
                    .push((name, dir.clone(), error.diagnostic.message.clone()));
            }
            Err(Failure::Other(error)) => return Err(unresolvable(error, root, &dir)),
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::fs::{
        Sim,
        sim::{CrashModel, Fault},
    };
    use std::collections::BTreeMap;

    const ROOT: &str = "/db";

    fn seeded() -> Sim {
        let sim = Sim::new();
        sim.seed(Path::new("/db/t/a.json"), b"A1");
        sim.seed(Path::new("/db/t/b.json"), b"B1");
        sim.seed(Path::new("/db/.db/format"), b"format_version = 3\n");
        sim
    }

    fn plan() -> Plan {
        Plan {
            origin: "internal".into(),
            changes: vec![
                Change::Write {
                    path: "t/a.json".into(),
                    bytes: b"A2".to_vec(),
                    pre: Some(Image::of(b"A1")),
                },
                Change::Delete {
                    path: "t/b.json".into(),
                    pre: Image::of(b"B1"),
                },
                Change::Write {
                    path: "u/c.json".into(),
                    bytes: b"C2".to_vec(),
                    pre: None,
                },
            ],
            artifacts: vec![Artifact {
                bytes: b"pack".to_vec(),
                to: ".db/objects/pack/pack-1.pack".into(),
            }],
            ..Plan::default()
        }
    }

    fn rows(files: BTreeMap<PathBuf, Vec<u8>>) -> BTreeMap<PathBuf, Vec<u8>> {
        files
            .into_iter()
            .filter(|(path, _)| !path.starts_with("/db/.db/transactions"))
            .collect()
    }

    fn run(sim: &Sim) -> Result<Committed> {
        preflight(sim, Path::new(ROOT), &plan(), u64::MAX)?;
        commit(sim, Path::new(ROOT), &plan(), &mut Unobserved)
    }

    #[test]
    fn test8040_a_complete_commit_applies_every_change_durably() {
        let sim = seeded();
        let committed = run(&sim).unwrap();
        assert!(committed.kept_outside_edits.is_empty());
        let after = rows(sim.files());
        assert_eq!(after[Path::new("/db/t/a.json")], b"A2");
        assert!(!after.contains_key(Path::new("/db/t/b.json")));
        assert_eq!(after[Path::new("/db/u/c.json")], b"C2");
        assert_eq!(
            after[Path::new("/db/.db/objects/pack/pack-1.pack")],
            b"pack"
        );
        assert!(
            sim.read_dir(&store(Path::new(ROOT))).unwrap().is_empty(),
            "nothing is left staged"
        );
        assert_eq!(
            rows(sim.crash(CrashModel::DurableOnly).files()),
            after,
            "and all of it is durable"
        );
    }

    /// Power is cut at every operation of the protocol, under every crash
    /// model the simulator offers -- any subset of the unsynced operations may
    /// survive -- and recovery always leaves the files exactly as they were or
    /// exactly as the change makes them: never a mixture, never a torn file.
    #[test]
    fn test8041_a_crash_anywhere_recovers_to_before_or_after() {
        let before = rows(seeded().files());
        let after = {
            let sim = seeded();
            run(&sim).unwrap();
            rows(sim.files())
        };
        let total = {
            let sim = seeded();
            let start = sim.operations();
            run(&sim).unwrap();
            sim.operations() - start
        };
        assert!(total > 10, "the protocol was exercised: {total} operations");
        let mut outcomes = BTreeSet::new();
        for at in 0..=total {
            for seed in 0..12u64 {
                let sim = seeded();
                sim.fault_at(at, Fault::Crash);
                let _ = run(&sim);
                let rebooted = match seed {
                    0 => sim.crash(CrashModel::DurableOnly),
                    seed => sim.crash(CrashModel::Subset(seed)),
                };
                let recovered = recover(&rebooted, Path::new(ROOT), &NoBlobs, &mut Unobserved)
                    .unwrap_or_else(|error| panic!("recovery after a crash at {at}: {error}"));
                assert!(recovered.blocked.is_empty(), "{recovered:?}");
                let state = rows(rebooted.files());
                assert!(
                    state == before || state == after,
                    "a crash at operation {at} (seed {seed}) left a mixture: {state:?}"
                );
                assert_eq!(
                    rows(rebooted.crash(CrashModel::DurableOnly).files()),
                    state,
                    "what recovery leaves is durable"
                );
                outcomes.insert(state == after);
            }
        }
        assert_eq!(
            outcomes.len(),
            2,
            "crashes land on both sides of the commit point"
        );
        // Wherever few enough operations are pending, every subset of them a
        // filesystem could persist is tried, not a sample.
        let mut exhaustive = 0;
        for at in 0..=total {
            let sim = seeded();
            sim.fault_at(at, Fault::Crash);
            let _ = run(&sim);
            let pending_ops = sim.pending_count();
            if pending_ops > 12 {
                continue;
            }
            for mask in 0..(1u64 << pending_ops) {
                let Some(rebooted) = sim.crash_subset(mask) else {
                    continue;
                };
                recover(&rebooted, Path::new(ROOT), &NoBlobs, &mut Unobserved)
                    .unwrap_or_else(|error| panic!("crash at {at}, subset {mask:b}: {error}"));
                let state = rows(rebooted.files());
                assert!(
                    state == before || state == after,
                    "crash at {at}, subset {mask:b}: {state:?}"
                );
                exhaustive += 1;
            }
        }
        assert!(exhaustive > 100, "subsets were enumerated: {exhaustive}");
    }

    /// A dropped table's directory goes with its last row, and a crash
    /// anywhere leaves it either as it was or gone; a directory something
    /// else is left in stays.
    #[test]
    fn test8051_a_directory_the_change_empties_is_pruned_and_no_other() {
        let seeded = || {
            let sim = Sim::new();
            sim.seed(Path::new("/db/t/a.json"), b"A1");
            sim.seed(Path::new("/db/v/a.json"), b"V1");
            sim.seed(Path::new("/db/v/kept.json"), b"K1");
            sim.seed(Path::new("/db/.db/format"), b"format_version = 3\n");
            sim
        };
        let plan = Plan {
            origin: "migration".into(),
            changes: vec![
                Change::Delete {
                    path: "t/a.json".into(),
                    pre: Image::of(b"A1"),
                },
                Change::Delete {
                    path: "v/a.json".into(),
                    pre: Image::of(b"V1"),
                },
            ],
            prune: vec!["t".into(), "v".into()],
            ..Plan::default()
        };
        let run = |sim: &Sim| -> Result<Committed> {
            preflight(sim, Path::new(ROOT), &plan, u64::MAX)?;
            commit(sim, Path::new(ROOT), &plan, &mut Unobserved)
        };
        let shape = |sim: &Sim| {
            (
                rows(sim.files()),
                sim.metadata(Path::new("/db/t")).is_ok(),
                sim.metadata(Path::new("/db/v")).is_ok(),
            )
        };
        let before = shape(&seeded());
        let sim = seeded();
        run(&sim).unwrap();
        let after = shape(&sim);
        assert!(!after.1, "the emptied directory is gone");
        assert!(after.2, "the directory still holding a file stays");
        assert_eq!(after.0[Path::new("/db/v/kept.json")], b"K1");
        assert_eq!(shape(&sim.crash(CrashModel::DurableOnly)), after);

        let total = {
            let sim = seeded();
            let start = sim.operations();
            run(&sim).unwrap();
            sim.operations() - start
        };
        for at in 0..=total {
            for seed in 0..6u64 {
                let sim = seeded();
                sim.fault_at(at, Fault::Crash);
                let _ = run(&sim);
                let rebooted = match seed {
                    0 => sim.crash(CrashModel::DurableOnly),
                    seed => sim.crash(CrashModel::Subset(seed)),
                };
                recover(&rebooted, Path::new(ROOT), &NoBlobs, &mut Unobserved)
                    .unwrap_or_else(|error| panic!("recovery after a crash at {at}: {error}"));
                let state = shape(&rebooted);
                assert!(
                    state == before || state == after,
                    "a crash at {at} (seed {seed}) left {state:?}"
                );
            }
        }
        // Only a top-level directory of the folder is ever pruned.
        for refused in [".db", "schema", "t/sub", "../t"] {
            let plan = Plan {
                origin: "migration".into(),
                prune: vec![refused.into()],
                ..Plan::default()
            };
            assert!(
                preflight(&seeded(), Path::new(ROOT), &plan, u64::MAX).is_err(),
                "{refused}"
            );
        }
    }

    /// A crash during recovery is recovered from too.
    #[test]
    fn test8042_a_crash_during_recovery_is_recovered() {
        let after = {
            let sim = seeded();
            run(&sim).unwrap();
            rows(sim.files())
        };
        // Crash just after the commit point: the journal is durable, nothing
        // applied.
        let committed_at = {
            let sim = seeded();
            let mut point = 0;
            for at in 0..200 {
                let probe = seeded();
                probe.fault_at(at, Fault::Crash);
                let _ = run(&probe);
                let rebooted = probe.crash(CrashModel::DurableOnly);
                if pending(&rebooted, Path::new(ROOT)).unwrap().0.len() == 1 {
                    point = at;
                    break;
                }
            }
            drop(sim);
            point
        };
        assert!(committed_at > 0);
        let sim = seeded();
        sim.fault_at(committed_at, Fault::Crash);
        let _ = run(&sim);
        let first = sim.crash(CrashModel::DurableOnly);
        let steps = {
            let probe = first.crash(CrashModel::DurableOnly);
            let start = probe.operations();
            recover(&probe, Path::new(ROOT), &NoBlobs, &mut Unobserved).unwrap();
            probe.operations() - start
        };
        for at in 0..=steps {
            let attempt = first.crash(CrashModel::DurableOnly);
            attempt.fault_at(at, Fault::Crash);
            let _ = recover(&attempt, Path::new(ROOT), &NoBlobs, &mut Unobserved);
            let rebooted = attempt.crash(CrashModel::Subset(at));
            recover(&rebooted, Path::new(ROOT), &NoBlobs, &mut Unobserved).unwrap();
            assert_eq!(rows(rebooted.files()), after, "crash at recovery step {at}");
        }
    }

    /// A key whose file name the filesystem cannot hold is refused before
    /// anything is written, never after the commit point, where it would
    /// strand the transaction.
    #[test]
    fn test8043_an_unplaceable_name_is_refused_before_the_commit_point() {
        let sim = seeded();
        sim.set_name_max(64);
        let long = format!("t/{}.json", "k".repeat(70));
        let plan = Plan {
            origin: "internal".into(),
            changes: vec![Change::Write {
                path: long.into(),
                bytes: b"{}".to_vec(),
                pre: None,
            }],
            ..Plan::default()
        };
        let error = preflight(&sim, Path::new(ROOT), &plan, u64::MAX).unwrap_err();
        assert_eq!(error.code(), Code::FILENAME_TOO_LONG);
        // A name that fits, even with the staging prefix of the old protocol
        // well past the limit, commits: staged names are short.
        let fitting = Plan {
            origin: "internal".into(),
            changes: vec![Change::Write {
                path: format!("t/{}.json", "k".repeat(59)).into(),
                bytes: b"{}".to_vec(),
                pre: None,
            }],
            ..Plan::default()
        };
        preflight(&sim, Path::new(ROOT), &fitting, u64::MAX).unwrap();
        commit(&sim, Path::new(ROOT), &fitting, &mut Unobserved).unwrap();
        assert!(pending(&sim, Path::new(ROOT)).unwrap().0.is_empty());
    }

    /// A directory or symlink where a file belongs is refused before writing,
    /// and is never removed.
    #[test]
    fn test8044_interference_is_refused_and_nothing_is_removed() {
        let sim = seeded();
        sim.create_dir(Path::new("/db/u")).unwrap();
        sim.create_dir(Path::new("/db/u/c.json")).unwrap();
        sim.create_new(Path::new("/db/u/c.json/precious"), b"keep")
            .unwrap();
        let error = preflight(&sim, Path::new(ROOT), &plan(), u64::MAX).unwrap_err();
        assert_eq!(error.code(), Code::PATH_INTERFERENCE);
        assert_eq!(sim.files()[Path::new("/db/u/c.json/precious")], b"keep");
    }

    #[test]
    fn test8045_a_target_that_moved_since_planning_is_refused() {
        let sim = seeded();
        sim.write(Path::new("/db/t/a.json"), b"edited").unwrap();
        let error = preflight(&sim, Path::new(ROOT), &plan(), u64::MAX).unwrap_err();
        assert_eq!(error.code(), Code::CONCURRENT_MODIFICATION);
    }

    /// After a crash, a file an outside editor changed before recovery ran is
    /// kept as edited and reported, never overwritten.
    #[test]
    fn test8046_an_outside_edit_after_a_crash_is_kept() {
        let mut committed_at = None;
        for at in 0..200 {
            let probe = seeded();
            probe.fault_at(at, Fault::Crash);
            let _ = run(&probe);
            let rebooted = probe.crash(CrashModel::DurableOnly);
            if pending(&rebooted, Path::new(ROOT)).unwrap().0.len() == 1 {
                committed_at = Some(at);
                break;
            }
        }
        let sim = seeded();
        sim.fault_at(committed_at.unwrap(), Fault::Crash);
        let _ = run(&sim);
        let rebooted = sim.crash(CrashModel::DurableOnly);
        rebooted
            .write(Path::new("/db/t/a.json"), b"outside")
            .unwrap();
        let recovered = recover(&rebooted, Path::new(ROOT), &NoBlobs, &mut Unobserved).unwrap();
        assert_eq!(recovered.kept_outside_edits, [PathBuf::from("t/a.json")]);
        assert_eq!(rebooted.files()[Path::new("/db/t/a.json")], b"outside");
        assert_eq!(rebooted.files()[Path::new("/db/u/c.json")], b"C2");
    }

    /// A sync that fails after the commit point is reported as committed --
    /// retrying would apply the change twice -- and recovery finishes it by
    /// writing the bytes afresh rather than re-syncing pages a failed writeback
    /// may have dropped.
    #[test]
    fn test8047_a_failed_sync_after_commit_says_committed_and_recovery_rewrites() {
        let total = {
            let sim = seeded();
            let start = sim.operations();
            run(&sim).unwrap();
            sim.operations() - start
        };
        let mut saw_pending = false;
        for at in 0..total {
            let sim = seeded();
            sim.fault_at(at, Fault::SyncFailsDropsData);
            match run(&sim) {
                Ok(_) => continue,
                Err(error) if error.code() == Code::COMMITTED_RECOVERY_PENDING => {
                    saw_pending = true;
                    recover(&sim, Path::new(ROOT), &NoBlobs, &mut Unobserved).unwrap();
                    let state = rows(sim.files());
                    assert_eq!(state[Path::new("/db/u/c.json")], b"C2");
                    assert_eq!(
                        rows(sim.crash(CrashModel::DurableOnly).files()),
                        state,
                        "after recovery the change is durable (sync failed at operation {at}: {error})"
                    );
                }
                Err(error) => {
                    // Before the commit point: nothing happened.
                    assert_ne!(error.code(), Code::COMMIT_UNKNOWN, "{error}");
                    assert_eq!(rows(sim.files())[Path::new("/db/t/a.json")], b"A1");
                }
            }
        }
        assert!(
            saw_pending,
            "some sync failure landed after the commit point"
        );
    }

    /// A journal torn by a crash, or never finished, is not a commit.
    #[test]
    fn test8048_only_a_journal_whose_checksum_holds_is_a_commit() {
        let journal = Journal {
            reldir_journal: JOURNAL_VERSION,
            id: ulid::Ulid::new().to_string(),
            origin: "internal".into(),
            created: "2026-01-01T00:00:00.000Z".into(),
            planned_against: None,
            revision: None,
            idempotency_key: None,
            changes: vec![],
            artifacts: vec![],
            retire: vec![],
            prune: vec![],
        };
        let bytes = encode(&journal);
        assert_eq!(decode(&bytes).unwrap(), Some(journal));
        for cut in [0, 10, bytes.len() / 2, bytes.len() - 1] {
            assert_eq!(decode(&bytes[..cut]).unwrap(), None, "cut at {cut}");
        }
        let mut flipped = bytes.clone();
        flipped[5] ^= 1;
        assert_eq!(decode(&flipped).unwrap(), None);
    }

    /// Strays in the staging area are reported, never treated as
    /// transactions and never a reason to stop reading.
    #[test]
    fn test8049_strays_in_the_staging_area_are_reported() {
        let sim = seeded();
        sim.create_dir(Path::new("/db/.db/transactions")).unwrap();
        sim.create_new(Path::new("/db/.db/transactions/.DS_Store"), b"x")
            .unwrap();
        let (pending, strays) = pending(&sim, Path::new(ROOT)).unwrap();
        assert!(pending.is_empty());
        assert_eq!(strays.0, [PathBuf::from("/db/.db/transactions/.DS_Store")]);
        let recovered = recover(&sim, Path::new(ROOT), &NoBlobs, &mut Unobserved).unwrap();
        assert_eq!(recovered.strays, strays);
    }

    /// A reader overlaying a committed, unfinished change sees exactly its
    /// post-state bytes.
    #[test]
    fn test8050_a_reader_sees_a_committed_change_before_it_is_finished() {
        let mut committed_at = None;
        for at in 0..200 {
            let probe = seeded();
            probe.fault_at(at, Fault::Crash);
            let _ = run(&probe);
            if pending(&probe.crash(CrashModel::DurableOnly), Path::new(ROOT))
                .unwrap()
                .0
                .len()
                == 1
            {
                committed_at = Some(at);
                break;
            }
        }
        let sim = seeded();
        sim.fault_at(committed_at.unwrap(), Fault::Crash);
        let _ = run(&sim);
        let rebooted = sim.crash(CrashModel::DurableOnly);
        let (pending, _) = pending(&rebooted, Path::new(ROOT)).unwrap();
        let change = pending[0]
            .journal
            .changes
            .iter()
            .find(|change| change.path == Path::new("u/c.json"))
            .unwrap();
        assert_eq!(
            pending[0].post(&rebooted, Path::new(ROOT), change).unwrap(),
            Some(b"C2".to_vec())
        );
    }
}
