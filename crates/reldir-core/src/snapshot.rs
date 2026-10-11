//! Named states: snapshots.
//!
//! A snapshot is a name for a tree in history ([`crate::store::tree`]): every
//! row, every schema, the configuration -- and every file that was not a valid
//! row, because a snapshot taken before a repair must be able to put back what
//! the repair changed. A snapshot of a recorded valid state is a name for the
//! head's tree and costs nothing to take; one of any other state stores only
//! what history does not already hold. Snapshots live under
//! `.db/history/refs/snapshots/` and keep everything they reach alive through
//! garbage collection.
//!
//! Restoring one is an ordinary transaction, validated like any other, so a
//! restore can never leave the database invalid.

use crate::code::Code;
use crate::{
    db::{Database, Head},
    diagnostic::{DbError, Result},
    engine::{fs::Disk, journal},
    history,
    plan::FileChange,
    store::{Id, refs::SnapshotRef},
};
use std::{collections::BTreeMap, path::PathBuf};

/// Refuse a name a snapshot cannot have.
pub fn validate_name(name: &str) -> Result<()> {
    crate::store::refs::validate_snapshot_name(name).map_err(|_| {
        DbError::new(
            Code::PATH_VIOLATION,
            format!(
                "{name:?} cannot name a snapshot: use lowercase letters, digits, '-', '_' and '.', \
                 beginning with a letter or digit"
            ),
        )
    })
}

/// Every snapshot, by name.
pub fn list(database: &Database) -> Result<Vec<(String, SnapshotRef)>> {
    let history = database.history()?;
    crate::store::refs::list_snapshots(&Disk, &history.layout().snapshot_refs())
        .map_err(history::store_error)
}

pub fn exists(database: &Database, name: &str) -> Result<bool> {
    validate_name(name)?;
    let history = database.history()?;
    Ok(
        crate::store::refs::read_snapshot(&Disk, &history.layout().snapshot_refs(), name)
            .map_err(history::store_error)?
            .is_some(),
    )
}

/// Name the current state. The caller holds the writer lock.
pub fn create(database: &mut Database, name: &str) -> Result<SnapshotRef> {
    validate_name(name)?;
    if exists(database, name)? {
        return Err(DbError::new(
            Code::SNAPSHOT_EXISTS,
            format!("a snapshot named {name:?} exists"),
        ));
    }
    let valid = database.is_valid();
    let (pending, _) = history::pending(&database.catalog)?;
    let recorded = match &database.head {
        Head::One { name, revision } if valid && pending.is_empty() => {
            Some((name.id, revision.tree, revision.state))
        }
        _ => None,
    };
    let history = database.history()?;
    let refs = history.layout().snapshot_refs();
    let now = chrono::Utc::now();
    let mut artifacts = vec![];
    let snapshot = match recorded {
        Some((revision, tree, state)) => SnapshotRef {
            tree,
            state,
            revision: Some(revision),
            created: now,
            valid: true,
        },
        None => {
            let state = history::live_state(&database.catalog)?;
            let extra_files = invalid_files(database)?;
            let root = database.root.clone();
            let (built, pack) = history.build(&state, &extra_files, &mut |path| {
                let full = root.join(path);
                std::fs::read(&full).map_err(|error| DbError::io(&full, error))
            })?;
            if let Some((pack_bytes, idx_bytes, pack_name)) = pack {
                let packs = PathBuf::from(".db/objects/pack");
                artifacts.push(journal::Artifact {
                    bytes: pack_bytes,
                    to: packs.join(pack_name.pack_file()),
                });
                artifacts.push(journal::Artifact {
                    bytes: idx_bytes,
                    to: packs.join(pack_name.idx_file()),
                });
            }
            SnapshotRef {
                tree: built.root,
                state: built.identity,
                revision: database.head.name().map(|name| name.id),
                created: now,
                valid,
            }
        }
    };
    artifacts.push(journal::Artifact {
        bytes: snapshot.encode().map_err(history::store_error)?,
        to: PathBuf::from(".db/history/refs/snapshots").join(name),
    });
    let plan = journal::Plan {
        origin: crate::store::revision::Origin::Internal.as_str().into(),
        artifacts,
        ..journal::Plan::default()
    };
    journal::preflight(
        &Disk,
        &database.root,
        &plan,
        database.config.max_transaction_size,
    )?;
    journal::commit(&Disk, &database.root, &plan, &mut journal::Unobserved)?;
    let _ = refs;
    if let Some(history) = database.history.as_mut() {
        history.refresh()?;
    }
    Ok(snapshot)
}

/// The governed files that are not valid rows, with their bytes: kept with a
/// snapshot so that putting it back puts them back.
fn invalid_files(database: &Database) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = vec![];
    for table in database.catalog.schemas.keys() {
        for path in database.catalog.mirror.paths(table)? {
            let Some(entry) = database.catalog.mirror.file(&path)? else {
                continue;
            };
            if entry.row_hash.is_none() {
                let full = database.root.join(&path);
                out.push((
                    path,
                    std::fs::read(&full).map_err(|error| DbError::io(&full, error))?,
                ));
            }
        }
    }
    Ok(out)
}

/// The files that make the database what a snapshot recorded: every file the
/// snapshot holds whose bytes differ, written; every governed file it does not
/// hold, removed. The format marker is never changed by a restore -- a
/// snapshot from another format is refused.
pub fn restore(database: &Database, name: &str) -> Result<Restore> {
    validate_name(name)?;
    let history = database.history()?;
    let snapshot =
        crate::store::refs::read_snapshot(&Disk, &history.layout().snapshot_refs(), name)
            .map_err(history::store_error)?
            .ok_or_else(|| {
                DbError::new(
                    Code::UNKNOWN_SNAPSHOT,
                    format!("there is no snapshot {name:?}"),
                )
                .with_help("`reldir snapshot list` lists them")
            })?;
    let mut wanted: BTreeMap<String, Id> = BTreeMap::new();
    for entry in history.entries(&snapshot.tree)? {
        wanted.insert(entry.path, entry.blob);
    }
    for (path, blob) in history.extras(&snapshot.tree)? {
        wanted.insert(path, blob);
    }
    if let Some(format) = wanted.remove(".db/format") {
        let current = std::fs::read(database.root.join(".db/format"))
            .map_err(|error| DbError::io(&database.root.join(".db/format"), error))?;
        if Id::of(&current) != format {
            return Err(DbError::new(
                Code::FORMAT_UNSUPPORTED,
                format!(
                    "snapshot {name:?} was taken in another format and cannot be restored here"
                ),
            ));
        }
    }
    let mut current: BTreeMap<String, Id> = BTreeMap::new();
    for table in database.catalog.schemas.keys() {
        for path in database.catalog.mirror.paths(table)? {
            if let Some(entry) = database.catalog.mirror.file(&path)?
                && let Some(id) = Id::from_hex(&entry.raw_hash)
            {
                current.insert(path, id);
            }
        }
    }
    for file in database.catalog.schema_files.values() {
        current.insert(crate::catalog::slash(&file.relative), Id::of(&file.bytes));
    }
    let config = database.root.join(".db/config");
    if let Ok(bytes) = std::fs::read(&config) {
        current.insert(".db/config".into(), Id::of(&bytes));
    }
    let mut changes = vec![];
    for (path, id) in &current {
        if !wanted.contains_key(path) {
            changes.push(FileChange::Delete {
                path: PathBuf::from(path),
            });
        } else if wanted[path] == *id {
            continue;
        }
    }
    for (path, id) in &wanted {
        if current.get(path) != Some(id) {
            changes.push(FileChange::Write {
                path: PathBuf::from(path),
                bytes: history.blob(id)?,
            });
        }
    }
    Ok(Restore {
        changes,
        state: snapshot.state,
    })
}

/// What restoring a snapshot writes, and the state it puts back.
pub struct Restore {
    pub changes: Vec<FileChange>,
    /// The identity of the snapshot's state: commit the changes with
    /// [`crate::db::Admission::Restores`] of it.
    pub state: crate::store::Hash256,
}

/// Remove a snapshot's name. What it alone kept alive is collected by the
/// next `reldir gc`. The caller holds the writer lock.
pub fn delete(database: &Database, name: &str) -> Result<()> {
    validate_name(name)?;
    if !exists(database, name)? {
        return Err(DbError::new(
            Code::UNKNOWN_SNAPSHOT,
            format!("there is no snapshot {name:?}"),
        ));
    }
    let history = database.history()?;
    crate::store::refs::delete_snapshot(&Disk, &history.layout().snapshot_refs(), name)
        .map_err(history::store_error)
}
