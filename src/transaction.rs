//! Writing many files as one change.
//!
//! Renaming many files is not atomic on any real filesystem, so a transaction
//! is made *recoverable* instead: after a crash at any instant, its state is
//! one of three, and each has one right answer.
//!
//! ```text
//! stage every new file's bytes, fsync each          ─┐
//! write the journal, fsync, rename into place         │ nothing visible changed:
//! fsync the transaction directory                     │ recovery discards it
//! write COMMITTING, fsync, fsync the directory       ─┘
//! for each change: rename a synced copy over the      ─┐ partially visible:
//!   target, or remove it; fsync its directory          │ recovery rolls it forward
//! remove COMMITTING, fsync the directory             ─┘ from the staged bytes
//! remove the transaction directory                    ─ complete
//! ```
//!
//! Every step is performed through [`Fs`], so the protocol is exercised by the
//! crash-consistency tests with a simulated crash at every operation, against
//! a model in which only fsynced data under fsynced directory entries
//! survives.

use crate::{
    diagnostic::{DbError, Diagnostic, Result},
    fs::{Fs, Kind},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

/// One file a transaction writes or removes, relative to the database root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Write { path: PathBuf, bytes: Vec<u8> },
    Delete { path: PathBuf },
}

impl Change {
    pub fn path(&self) -> &Path {
        match self {
            Self::Write { path, .. } | Self::Delete { path } => path,
        }
    }
}

/// What recovery needs to finish or discard an interrupted transaction.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    id: String,
    origin: String,
    changes: Vec<JournalChange>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalChange {
    path: PathBuf,
    stage: Option<String>,
}

fn io(path: &Path, error: std::io::Error) -> DbError {
    DbError::io(path, error)
}

fn incomplete(message: impl Into<String>) -> DbError {
    DbError::new("TRANSACTION_INCOMPLETE", message, 5)
}

/// Where transactions are staged.
pub fn store(root: &Path) -> PathBuf {
    root.join(".db/transactions")
}

/// Refuse a plan that could not be a transaction at all: a path named twice, a
/// path outside the database or inside reldir's own metadata, or more bytes
/// than the configured limit.
pub fn check_plan(changes: &[Change], max_transaction_size: u64) -> Result<()> {
    let mut paths = BTreeSet::new();
    let mut bytes: u64 = 0;
    for change in changes {
        safe_relative(change.path())?;
        if !paths.insert(change.path().to_path_buf()) {
            return Err(DbError::new(
                "MUTATION_CONFLICT",
                format!("the plan names {} more than once", change.path().display()),
                2,
            ));
        }
        if let Change::Write { bytes: written, .. } = change {
            bytes = bytes.saturating_add(written.len() as u64);
        }
    }
    if bytes > max_transaction_size {
        return Err(DbError::new(
            "RESOURCE_LIMIT",
            format!(
                "the transaction stages {bytes} bytes, beyond the {max_transaction_size} byte limit"
            ),
            2,
        ));
    }
    Ok(())
}

/// Commit a plan through the recoverable protocol. The caller holds the writer
/// lock and has validated the state the plan produces.
pub fn journaled(
    fs: &dyn Fs,
    root: &Path,
    id: &str,
    origin: &str,
    changes: &[Change],
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let transactions = store(root);
    let dir = transactions.join(id);
    let staged = dir.join("staged");
    fs.create_dir_all(&staged).map_err(|e| io(&staged, e))?;
    let mut journal_changes = vec![];
    for (index, change) in changes.iter().enumerate() {
        match change {
            Change::Write { path, bytes } => {
                let name = format!("{index:08}");
                let stage = staged.join(&name);
                fs.write(&stage, bytes).map_err(|e| io(&stage, e))?;
                fs.sync_file(&stage).map_err(|e| io(&stage, e))?;
                journal_changes.push(JournalChange {
                    path: path.clone(),
                    stage: Some(name),
                });
            }
            Change::Delete { path } => journal_changes.push(JournalChange {
                path: path.clone(),
                stage: None,
            }),
        }
    }
    fs.sync_dir(&staged).map_err(|e| io(&staged, e))?;
    let journal = Journal {
        id: id.to_string(),
        origin: origin.to_string(),
        changes: journal_changes,
    };
    let mut bytes = serde_json::to_vec_pretty(&journal)
        .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))?;
    bytes.push(b'\n');
    let journal_path = dir.join("journal.json");
    let journal_temp = dir.join("journal.json.tmp");
    fs.write(&journal_temp, &bytes)
        .map_err(|e| io(&journal_temp, e))?;
    fs.sync_file(&journal_temp)
        .map_err(|e| io(&journal_temp, e))?;
    fs.rename(&journal_temp, &journal_path)
        .map_err(|e| io(&journal_path, e))?;
    fs.sync_dir(&dir).map_err(|e| io(&dir, e))?;
    fs.sync_dir(&transactions)
        .map_err(|e| io(&transactions, e))?;
    let marker = dir.join("COMMITTING");
    fs.write(&marker, b"commit\n").map_err(|e| io(&marker, e))?;
    fs.sync_file(&marker).map_err(|e| io(&marker, e))?;
    fs.sync_dir(&dir).map_err(|e| io(&dir, e))?;
    apply(fs, root, &dir, &journal)?;
    finish(fs, &transactions, &dir)
}

/// Roll a journal forward. Idempotent: every step can be repeated after a
/// crash with the same result.
fn apply(fs: &dyn Fs, root: &Path, dir: &Path, journal: &Journal) -> Result<()> {
    validate_journal(dir, journal)?;
    for change in &journal.changes {
        let target = root.join(&change.path);
        let parent = target.parent().unwrap_or(root).to_path_buf();
        match &change.stage {
            Some(stage) => {
                let source = dir.join("staged").join(stage);
                let bytes = fs.read(&source).map_err(|error| {
                    incomplete(format!(
                        "transaction {} has lost staged object {stage}: {error}",
                        journal.id
                    ))
                })?;
                ensure_directory(fs, root, &parent)?;
                if fs
                    .metadata(&target)
                    .is_ok_and(|meta| meta.kind == Kind::Dir)
                {
                    fs.remove_dir_all(&target).map_err(|e| io(&target, e))?;
                }
                let temp = target.with_extension(format!("reldir-tmp-{}", journal.id));
                fs.write(&temp, &bytes).map_err(|e| io(&temp, e))?;
                fs.sync_file(&temp).map_err(|e| io(&temp, e))?;
                fs.rename(&temp, &target).map_err(|e| io(&target, e))?;
                fs.sync_dir(&parent).map_err(|e| io(&parent, e))?;
            }
            None => match fs.metadata(&target) {
                Ok(meta) => {
                    if meta.kind == Kind::Dir {
                        fs.remove_dir_all(&target).map_err(|e| io(&target, e))?;
                    } else {
                        fs.remove_file(&target).map_err(|e| io(&target, e))?;
                    }
                    fs.sync_dir(&parent).map_err(|e| io(&parent, e))?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io(&target, error)),
            },
        }
    }
    Ok(())
}

/// Take the marker off, then the transaction away.
fn finish(fs: &dyn Fs, transactions: &Path, dir: &Path) -> Result<()> {
    let marker = dir.join("COMMITTING");
    if fs.metadata(&marker).is_ok() {
        fs.remove_file(&marker).map_err(|e| io(&marker, e))?;
        fs.sync_dir(dir).map_err(|e| io(dir, e))?;
    }
    fs.remove_dir_all(dir).map_err(|e| io(dir, e))?;
    fs.sync_dir(transactions).map_err(|e| io(transactions, e))?;
    Ok(())
}

/// Create a directory and make its entry durable in its parent, one level at
/// a time, refusing to pass through anything that is not a real directory.
fn ensure_directory(fs: &dyn Fs, root: &Path, directory: &Path) -> Result<()> {
    let relative = directory.strip_prefix(root).unwrap_or(directory);
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(DbError::new(
                "PATH_VIOLATION",
                format!("unsafe transaction path {}", directory.display()),
                5,
            ));
        };
        let parent = current.clone();
        current.push(component);
        match fs.metadata(&current) {
            Ok(meta) if meta.kind == Kind::Dir => {}
            Ok(_) => {
                return Err(DbError::new(
                    "PATH_INTERFERENCE",
                    format!(
                        "{} is no longer a directory, so the transaction cannot place its files; \
                         retrying meets the same path",
                        current.display()
                    ),
                    3,
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs.create_dir_all(&current).map_err(|e| io(&current, e))?;
                fs.sync_dir(&parent).map_err(|e| io(&parent, e))?;
            }
            Err(error) => return Err(io(&current, error)),
        }
    }
    Ok(())
}

fn validate_journal(dir: &Path, journal: &Journal) -> Result<()> {
    let directory_id = dir.file_name().and_then(|name| name.to_str());
    if uuid::Uuid::parse_str(&journal.id).is_err() || directory_id != Some(journal.id.as_str()) {
        return Err(incomplete(
            "a transaction journal's id does not match its directory",
        ));
    }
    if !matches!(
        journal.origin.as_str(),
        "internal" | "recovery" | "repair" | "migration" | "import" | "snapshot_restore"
    ) {
        return Err(incomplete(format!(
            "a transaction names the invalid origin {:?}",
            journal.origin
        )));
    }
    if journal.changes.is_empty() {
        return Err(incomplete("a transaction journal lists no changes"));
    }
    let mut paths = BTreeSet::new();
    let mut stages = BTreeSet::new();
    for change in &journal.changes {
        safe_relative(&change.path).map_err(|error| {
            incomplete(format!(
                "unsafe path in a transaction journal: {}",
                error.diagnostic.message
            ))
        })?;
        if !paths.insert(change.path.clone()) {
            return Err(incomplete(format!(
                "a transaction repeats {}",
                change.path.display()
            )));
        }
        if let Some(stage) = &change.stage {
            let stage_path = Path::new(stage);
            if stage_path.components().count() != 1
                || !matches!(stage_path.components().next(), Some(Component::Normal(_)))
                || !stages.insert(stage)
            {
                return Err(incomplete(format!(
                    "a transaction has an unsafe or repeated staged object {stage:?}"
                )));
            }
        }
    }
    Ok(())
}

/// What an unfinished transaction on disk means for someone reading the rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// Nothing unfinished.
    None,
    /// Staged, but nothing renamed: the rows are exactly what they were.
    Staged,
    /// Some changes may be applied and others not: the rows are a partial
    /// application until recovery rolls it forward.
    Materialising,
}

pub fn pending(fs: &dyn Fs, root: &Path) -> Result<Pending> {
    let transactions = store(root);
    let entries = match fs.read_dir(&transactions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Pending::None),
        Err(error) => return Err(io(&transactions, error)),
    };
    let mut state = Pending::None;
    for entry in entries {
        let meta = fs.metadata(&entry).map_err(|e| io(&entry, e))?;
        if meta.kind != Kind::Dir {
            return Err(incomplete(format!(
                "transaction entry {} is not a directory",
                entry.display()
            )));
        }
        if fs.metadata(&entry.join("COMMITTING")).is_ok() {
            return Ok(Pending::Materialising);
        }
        state = Pending::Staged;
    }
    Ok(state)
}

/// What recovery did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recovered {
    /// Transactions rolled forward.
    pub completed: Vec<String>,
    /// Transactions discarded because they never began to apply.
    pub discarded: Vec<String>,
}

/// Finish or discard every interrupted transaction. The caller holds the
/// writer lock, so no live writer owns them.
pub fn recover(fs: &dyn Fs, root: &Path) -> Result<Recovered> {
    let transactions = store(root);
    let entries = match fs.read_dir(&transactions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Recovered::default());
        }
        Err(error) => return Err(io(&transactions, error)),
    };
    let mut outcome = Recovered::default();
    for dir in entries {
        let id = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let meta = fs.metadata(&dir).map_err(|e| io(&dir, e))?;
        if meta.kind != Kind::Dir {
            return Err(incomplete(format!(
                "transaction entry {} is not a directory",
                dir.display()
            )));
        }
        let journal_path = dir.join("journal.json");
        let committing = fs.metadata(&dir.join("COMMITTING")).is_ok();
        let journal_bytes = match fs.read(&journal_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(io(&journal_path, error)),
        };
        match (journal_bytes, committing) {
            (Some(bytes), true) => {
                let journal: Journal = crate::json::parse_as(&bytes).map_err(|error| {
                    incomplete(format!(
                        "transaction {id} was committing but its journal cannot be read ({error}); \
                         it must be resolved by hand"
                    ))
                })?;
                apply(fs, root, &dir, &journal)?;
                finish(fs, &transactions, &dir)?;
                outcome.completed.push(id);
            }
            (None, true) => {
                return Err(incomplete(format!(
                    "transaction {id} was committing but has no journal; it must be resolved by hand"
                )));
            }
            (_, false) => {
                fs.remove_dir_all(&dir).map_err(|e| io(&dir, e))?;
                fs.sync_dir(&transactions)
                    .map_err(|e| io(&transactions, e))?;
                outcome.discarded.push(id);
            }
        }
    }
    Ok(outcome)
}

/// Refuse any path that could escape the database root or reach into
/// reldir's own metadata. `.db/config` and working schemas are the only
/// metadata a transaction writes.
pub fn safe_relative(path: &Path) -> Result<()> {
    let escapes = path.is_absolute()
        || path.as_os_str().is_empty()
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir
                    | Component::RootDir
                    | Component::Prefix(_)
                    | Component::CurDir
            )
        });
    let metadata = path.starts_with(".db")
        && path != Path::new(".db/config")
        && !(path.parent() == Some(Path::new(".db/schema"))
            && path.extension().and_then(|value| value.to_str()) == Some("json"));
    if escapes || metadata {
        return Err(DbError::from_diag(
            Diagnostic::error(
                "PATH_VIOLATION",
                format!("{} is not a path a transaction may write", path.display()),
            ),
            2,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{Fault, Sim, Source};
    use std::collections::BTreeMap;

    const ROOT: &str = "/db";

    fn seeded() -> Sim {
        let sim = Sim::new();
        sim.seed(Path::new("/db/t/a.json"), b"A1");
        sim.seed(Path::new("/db/t/b.json"), b"B1");
        sim.create_dir_all(Path::new("/db/.db/transactions"))
            .unwrap();
        sim.sync_dir(Path::new("/db/.db")).unwrap();
        sim
    }

    fn plan() -> Vec<Change> {
        vec![
            Change::Write {
                path: "t/a.json".into(),
                bytes: b"A2".to_vec(),
            },
            Change::Delete {
                path: "t/b.json".into(),
            },
            Change::Write {
                path: "u/c.json".into(),
                bytes: b"C2".to_vec(),
            },
        ]
    }

    fn rows(sim: &Sim) -> BTreeMap<PathBuf, Vec<u8>> {
        sim.files()
            .into_iter()
            .filter(|(path, _)| !path.starts_with("/db/.db"))
            .collect()
    }

    const ID: &str = "0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e";

    #[test]
    fn test2130_a_complete_commit_applies_every_change() {
        let sim = seeded();
        journaled(&sim, Path::new(ROOT), ID, "internal", &plan()).unwrap();
        let after = rows(&sim);
        assert_eq!(after[Path::new("/db/t/a.json")], b"A2");
        assert!(!after.contains_key(Path::new("/db/t/b.json")));
        assert_eq!(after[Path::new("/db/u/c.json")], b"C2");
        assert!(
            sim.read_dir(&store(Path::new(ROOT))).unwrap().is_empty(),
            "nothing is left staged"
        );
        assert_eq!(rows(&sim.crashed()), after, "and all of it is durable");
    }

    /// A crash at every operation of the protocol, followed by recovery, leaves
    /// the rows exactly as they were before or exactly as the plan makes them:
    /// never a mixture.
    #[test]
    fn test2131_a_crash_anywhere_recovers_to_before_or_after() {
        let before = rows(&seeded());
        let after = {
            let sim = seeded();
            journaled(&sim, Path::new(ROOT), ID, "internal", &plan()).unwrap();
            rows(&sim)
        };
        let total = {
            let sim = seeded();
            let start = sim.operations();
            journaled(&sim, Path::new(ROOT), ID, "internal", &plan()).unwrap();
            sim.operations() - start
        };
        assert!(total > 10, "the protocol has many steps to crash in");
        let mut outcomes = BTreeSet::new();
        for at in 0..total {
            let sim = seeded();
            sim.fault_at(at, Fault::Crash);
            assert!(journaled(&sim, Path::new(ROOT), ID, "internal", &plan()).is_err());
            let rebooted = sim.crashed();
            let recovered = recover(&rebooted, Path::new(ROOT))
                .unwrap_or_else(|e| panic!("crash at {at}: {e:?}"));
            let state = rows(&rebooted);
            assert!(
                state == before || state == after,
                "a crash at operation {at} left a mixture: {state:?}"
            );
            if state == after {
                outcomes.insert("after");
                assert!(recovered.completed.len() + recovered.discarded.len() <= 1);
            } else {
                outcomes.insert("before");
            }
            assert_eq!(
                pending(&rebooted, Path::new(ROOT)).unwrap(),
                Pending::None,
                "recovery leaves nothing pending"
            );
            // Recovery is idempotent.
            assert_eq!(
                recover(&rebooted, Path::new(ROOT)).unwrap(),
                Recovered::default()
            );
        }
        assert_eq!(
            outcomes.len(),
            2,
            "some crashes land before the commit point and some after"
        );
    }

    /// A failed operation stops the protocol with an error naming the path;
    /// what is on disk is then before or recoverable, never silently mixed.
    #[test]
    fn test2132_io_errors_and_torn_writes_never_leave_a_mixed_state() {
        let before = rows(&seeded());
        let after = {
            let sim = seeded();
            journaled(&sim, Path::new(ROOT), ID, "internal", &plan()).unwrap();
            rows(&sim)
        };
        for fault in [Fault::Error(std::io::ErrorKind::StorageFull), Fault::Torn] {
            for at in 0..40 {
                let sim = seeded();
                sim.fault_at(at, fault);
                let _ = journaled(&sim, Path::new(ROOT), ID, "internal", &plan());
                let rebooted = sim.crashed();
                match recover(&rebooted, Path::new(ROOT)) {
                    Ok(_) => {
                        let state = rows(&rebooted);
                        assert!(
                            state == before || state == after,
                            "{fault:?} at {at}: {state:?}"
                        );
                    }
                    // A torn journal under a durable marker cannot be rolled
                    // forward, and recovery says so rather than guessing.
                    Err(error) => assert_eq!(error.diagnostic.code, "TRANSACTION_INCOMPLETE"),
                }
            }
        }
    }

    #[test]
    fn test2133_unsafe_paths_are_refused_before_anything_is_staged() {
        for rejected in [
            "../escape.json",
            "/abs.json",
            "t/../../x.json",
            ".db/manifest.json",
            ".db/lock",
            "./t/a.json",
            "",
        ] {
            assert!(safe_relative(Path::new(rejected)).is_err(), "{rejected:?}");
        }
        for accepted in [
            "t/a.json",
            ".db/config",
            ".db/schema/t.json",
            "schema/t.json",
        ] {
            safe_relative(Path::new(accepted)).unwrap_or_else(|_| panic!("{accepted:?}"));
        }
        let twice = vec![
            Change::Delete {
                path: "t/a.json".into(),
            },
            Change::Delete {
                path: "t/a.json".into(),
            },
        ];
        assert_eq!(
            check_plan(&twice, u64::MAX).unwrap_err().diagnostic.code,
            "MUTATION_CONFLICT"
        );
    }
}
