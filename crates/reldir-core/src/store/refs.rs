//! Names for revisions and trees: heads, snapshot refs, quarantined heads.
//!
//! - `.db/history/heads/<revision id>` -- one empty file per tip of history.
//!   One head is the normal case; several mean history diverged (two writers
//!   each recorded a revision on the same parent) and the next writer records
//!   a merge revision with every head as a parent.
//! - `.db/history/refs/snapshots/<name>` -- a named tree, as a canonical JSON
//!   document.
//! - `.db/history/quarantine/<stamp>/` -- heads moved aside when a new lineage
//!   began over a history that could not be continued: `heads/<id>` files and
//!   a `reason.json`. Their objects stay reachable, so nothing they name is
//!   ever collected.
//!
//! Every change is made durable before the call returns, and every listing
//! refuses entries the layout does not allow rather than skipping them.

use super::{
    Hash256, Id,
    codec::push_json_string,
    error::{Corruption, Invalid, Result, StoreError},
    revision::{parse_timestamp, render_timestamp},
};
use crate::engine::fs::{Durability, Fs, Kind, Source};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Deserialize;
use std::{
    io,
    path::{Path, PathBuf},
};

fn file_name(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| StoreError::corrupt(Corruption::UnexpectedEntry, path))
}

fn sync_dir(fs: &dyn Fs, dir: &Path) -> Result<()> {
    fs.sync_dir(dir)
        .map_err(|e| StoreError::io("sync directory", dir, e))
}

/// The tips of history.
pub struct Heads;

impl Heads {
    /// The heads in `dir`, sorted. Each must be an empty regular file named by
    /// a revision id.
    pub fn list(source: &dyn Source, dir: &Path) -> Result<Vec<Id>> {
        let entries = source
            .read_dir(dir)
            .map_err(|e| StoreError::io("list heads", dir, e))?;
        let mut heads = vec![];
        for entry in entries {
            let id = Id::from_hex(file_name(&entry.path)?)
                .ok_or_else(|| StoreError::corrupt(Corruption::UnexpectedEntry, &entry.path))?;
            if entry.meta.kind != Kind::File {
                return Err(StoreError::corrupt(Corruption::NotAFile, &entry.path));
            }
            if entry.meta.stat.size != 0 {
                return Err(StoreError::corrupt(Corruption::NonEmptyHead, &entry.path));
            }
            heads.push(id);
        }
        heads.sort_unstable();
        Ok(heads)
    }

    /// Make `new` a head in place of `parents`: the new head is created and
    /// made durable first, so a crash never leaves history without a tip;
    /// then the parents' heads are removed and that made durable.
    ///
    /// Running it again after a crash finishes the job: a head already
    /// created is kept, a parent already removed is skipped, and the final
    /// directory sync runs regardless, because a removal can be visible yet
    /// not durable.
    pub fn advance(fs: &dyn Fs, dir: &Path, new: &Id, parents: &[Id]) -> Result<()> {
        if parents.contains(new) {
            return Err(StoreError::Invalid(Invalid::HeadIsParent(*new)));
        }
        let path = dir.join(new.to_hex());
        match fs.create_new(&path, b"") {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let meta = fs
                    .metadata(&path)
                    .map_err(|e| StoreError::io("inspect head", &path, e))?;
                if meta.kind != Kind::File {
                    return Err(StoreError::corrupt(Corruption::NotAFile, &path));
                }
                if meta.stat.size != 0 {
                    return Err(StoreError::corrupt(Corruption::NonEmptyHead, &path));
                }
            }
            Err(e) => return Err(StoreError::io("create head", &path, e)),
        }
        sync_dir(fs, dir)?;
        Self::remove(fs, dir, parents)
    }

    /// Remove heads -- those found to be ancestors of others, or moved to
    /// quarantine -- and make the removal durable. A head already gone is
    /// not an error: removal is what a re-run after a crash repeats.
    pub fn remove(fs: &dyn Fs, dir: &Path, heads: &[Id]) -> Result<()> {
        for head in heads {
            let path = dir.join(head.to_hex());
            match fs.remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(StoreError::io("remove head", &path, e)),
            }
        }
        sync_dir(fs, dir)
    }
}

/// Whether `name` may name a snapshot: lowercase letters, digits, `-`, `_`
/// and `.`, beginning with a letter or digit, not ending with `.`, at most
/// 128 bytes, and not a Windows device name. Lowercase only, so two names
/// never collide on a case-insensitive filesystem.
pub fn validate_snapshot_name(name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    let stem = name.split('.').next().unwrap_or(name);
    let reserved = matches!(stem, "con" | "prn" | "aux" | "nul")
        || stem
            .strip_prefix("com")
            .or_else(|| stem.strip_prefix("lpt"))
            .is_some_and(|n| n.len() == 1 && matches!(n.as_bytes()[0], b'1'..=b'9'));
    let legal = !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
        && !name.ends_with('.')
        && !reserved;
    if !legal {
        return Err(StoreError::Invalid(Invalid::SnapshotName(name.into())));
    }
    Ok(())
}

/// A named tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRef {
    pub tree: Id,
    pub state: Hash256,
    /// The revision the snapshot was taken at, when there was one.
    pub revision: Option<Id>,
    pub created: DateTime<Utc>,
    /// Whether the state was valid when taken: a snapshot of an invalid
    /// state is kept so it can be put back, never as a state to trust.
    pub valid: bool,
}

/// The `reldir_ref` this module reads and writes.
pub const REF_FORMAT: u64 = 3;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRef {
    reldir_ref: u64,
    kind: String,
    tree: String,
    state: String,
    revision: Option<String>,
    created: String,
    valid: bool,
}

impl SnapshotRef {
    /// `{"created","kind":"snapshot","reldir_ref":3,"revision","state","tree","valid"}`,
    /// canonical: members sorted, no whitespace.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = String::from("{\"created\":");
        push_json_string(&mut out, &render_timestamp(&self.created)?);
        out.push_str(&format!(
            ",\"kind\":\"snapshot\",\"reldir_ref\":{REF_FORMAT},\"revision\":"
        ));
        match &self.revision {
            Some(id) => push_json_string(&mut out, &id.to_hex()),
            None => out.push_str("null"),
        }
        out.push_str(&format!(
            ",\"state\":\"{}\",\"tree\":\"{}\",\"valid\":{}}}",
            self.state, self.tree, self.valid
        ));
        Ok(out.into_bytes())
    }

    /// Parse the canonical bytes; anything else is refused.
    pub fn decode(bytes: &[u8]) -> Result<SnapshotRef, Corruption> {
        let raw: RawRef =
            serde_json::from_slice(bytes).map_err(|e| Corruption::Ref(e.to_string()))?;
        if raw.reldir_ref != REF_FORMAT {
            return Err(Corruption::UnsupportedVersion(
                u32::try_from(raw.reldir_ref).unwrap_or(u32::MAX),
            ));
        }
        if raw.kind != "snapshot" {
            return Err(Corruption::Ref(format!("kind {:?}", raw.kind)));
        }
        let snapshot = SnapshotRef {
            tree: Id::from_hex(&raw.tree).ok_or_else(|| Corruption::Ref("tree".into()))?,
            state: Hash256::from_hex(&raw.state).ok_or_else(|| Corruption::Ref("state".into()))?,
            revision: match raw.revision {
                Some(text) => {
                    Some(Id::from_hex(&text).ok_or_else(|| Corruption::Ref("revision".into()))?)
                }
                None => None,
            },
            created: parse_timestamp(&raw.created)
                .ok_or_else(|| Corruption::Ref(format!("created {:?}", raw.created)))?,
            valid: raw.valid,
        };
        match snapshot.encode() {
            Ok(canonical) if canonical == bytes => Ok(snapshot),
            _ => Err(Corruption::NonCanonical),
        }
    }
}

fn exists(source: &dyn Source, path: &Path) -> Result<bool> {
    match source.metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(StoreError::io("inspect", path, e)),
    }
}

/// Record a snapshot ref: written and synced in `staging` (on the same
/// volume), then renamed into `refs_dir`, so a ref is whole or absent. The
/// caller holds the writer lock, which is what makes the existence check
/// and the rename one step.
pub fn write_snapshot(
    fs: &dyn Fs,
    staging: &Path,
    refs_dir: &Path,
    name: &str,
    snapshot: &SnapshotRef,
) -> Result<PathBuf> {
    validate_snapshot_name(name)?;
    let target = refs_dir.join(name);
    if exists(fs, &target)? {
        return Err(StoreError::Invalid(Invalid::RefExists(name.into())));
    }
    let staged = staging.join(name);
    fs.create_synced(&staged, &snapshot.encode()?, Durability::Full)
        .map_err(|e| StoreError::io("write ref", &staged, e))?;
    fs.rename(&staged, &target)
        .map_err(|e| StoreError::io("publish ref", &target, e))?;
    sync_dir(fs, refs_dir)?;
    Ok(target)
}

fn read_ref(source: &dyn Source, path: &Path) -> Result<SnapshotRef> {
    let bytes = source
        .read(path)
        .map_err(|e| StoreError::io("read ref", path, e))?;
    SnapshotRef::decode(&bytes).map_err(|what| StoreError::corrupt(what, path))
}

/// The snapshot named `name`, if there is one.
pub fn read_snapshot(
    source: &dyn Source,
    refs_dir: &Path,
    name: &str,
) -> Result<Option<SnapshotRef>> {
    validate_snapshot_name(name)?;
    let path = refs_dir.join(name);
    match source.metadata(&path) {
        Ok(meta) if meta.kind == Kind::File => read_ref(source, &path).map(Some),
        Ok(_) => Err(StoreError::corrupt(Corruption::NotAFile, &path)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StoreError::io("inspect ref", &path, e)),
    }
}

/// Every snapshot, by name.
pub fn list_snapshots(source: &dyn Source, refs_dir: &Path) -> Result<Vec<(String, SnapshotRef)>> {
    let entries = source
        .read_dir(refs_dir)
        .map_err(|e| StoreError::io("list refs", refs_dir, e))?;
    let mut out = vec![];
    for entry in entries {
        let name = file_name(&entry.path)?;
        if validate_snapshot_name(name).is_err() {
            return Err(StoreError::corrupt(
                Corruption::UnexpectedEntry,
                &entry.path,
            ));
        }
        if entry.meta.kind != Kind::File {
            return Err(StoreError::corrupt(Corruption::NotAFile, &entry.path));
        }
        out.push((name.to_string(), read_ref(source, &entry.path)?));
    }
    Ok(out)
}

/// Forget a snapshot. Its objects become collectable unless something else
/// reaches them.
pub fn delete_snapshot(fs: &dyn Fs, refs_dir: &Path, name: &str) -> Result<()> {
    validate_snapshot_name(name)?;
    let path = refs_dir.join(name);
    match fs.remove_file(&path) {
        Ok(()) => sync_dir(fs, refs_dir),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(StoreError::Invalid(Invalid::NotPresent(name.into())))
        }
        Err(e) => Err(StoreError::io("remove ref", &path, e)),
    }
}

/// Heads moved aside, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quarantined {
    /// The directory's name under the quarantine directory.
    pub stamp: String,
    pub heads: Vec<Id>,
    pub reason: String,
    pub at: DateTime<Utc>,
}

const STAMP_FORMAT: &str = "%Y%m%dT%H%M%S%.9fZ";
const REASON_FILE: &str = "reason.json";
const HEADS_DIR: &str = "heads";
/// Packs that failed verification, set aside with the lineage that named
/// them: kept, for whoever investigates, and out of the live store.
const PACKS_DIR: &str = "packs";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReason {
    at: String,
    reason: String,
    reldir_quarantine: u64,
}

fn encode_reason(at: &DateTime<Utc>, reason: &str) -> Result<Vec<u8>> {
    let mut out = String::from("{\"at\":");
    push_json_string(&mut out, &render_timestamp(at)?);
    out.push_str(",\"reason\":");
    push_json_string(&mut out, reason);
    out.push_str(&format!(",\"reldir_quarantine\":{REF_FORMAT}}}"));
    Ok(out.into_bytes())
}

fn decode_reason(bytes: &[u8]) -> Result<(DateTime<Utc>, String), Corruption> {
    let raw: RawReason =
        serde_json::from_slice(bytes).map_err(|e| Corruption::Ref(e.to_string()))?;
    if raw.reldir_quarantine != REF_FORMAT {
        return Err(Corruption::UnsupportedVersion(
            u32::try_from(raw.reldir_quarantine).unwrap_or(u32::MAX),
        ));
    }
    let at = parse_timestamp(&raw.at).ok_or_else(|| Corruption::Ref(format!("at {:?}", raw.at)))?;
    match encode_reason(&at, &raw.reason) {
        Ok(canonical) if canonical == bytes => Ok((at, raw.reason)),
        _ => Err(Corruption::NonCanonical),
    }
}

/// The directory name a quarantine made at `at` has.
pub fn stamp(at: &DateTime<Utc>) -> String {
    at.format(STAMP_FORMAT).to_string()
}

/// Move every head aside into `quarantine_dir/<stamp>/`, with the reason.
///
/// The quarantine is built whole in `staging` (same volume), synced, and
/// renamed into place, so it is complete or absent; only then are the heads
/// removed. A crash before the removal leaves the heads in place and a
/// quarantine naming them -- running again quarantines them again under a
/// new stamp, which keeps the same objects reachable and loses nothing.
pub fn quarantine(
    fs: &dyn Fs,
    staging: &Path,
    heads_dir: &Path,
    quarantine_dir: &Path,
    reason: &str,
    at: DateTime<Utc>,
) -> Result<Quarantined> {
    let heads = Heads::list(fs, heads_dir)?;
    let reason_bytes = encode_reason(&at, reason)?;
    let stamp = stamp(&at);
    let target = quarantine_dir.join(&stamp);
    if exists(fs, &target)? {
        return Err(StoreError::io(
            "quarantine",
            &target,
            io::Error::from(io::ErrorKind::AlreadyExists),
        ));
    }
    let built = staging.join(&stamp);
    let built_heads = built.join(HEADS_DIR);
    fs.create_dir(&built)
        .map_err(|e| StoreError::io("create directory", &built, e))?;
    fs.create_dir(&built_heads)
        .map_err(|e| StoreError::io("create directory", &built_heads, e))?;
    for head in &heads {
        let path = built_heads.join(head.to_hex());
        fs.create_new(&path, b"")
            .map_err(|e| StoreError::io("create head", &path, e))?;
    }
    let reason_path = built.join(REASON_FILE);
    fs.create_synced(&reason_path, &reason_bytes, Durability::Full)
        .map_err(|e| StoreError::io("write reason", &reason_path, e))?;
    sync_dir(fs, &built_heads)?;
    sync_dir(fs, &built)?;
    fs.rename(&built, &target)
        .map_err(|e| StoreError::io("publish quarantine", &target, e))?;
    sync_dir(fs, quarantine_dir)?;
    Heads::remove(fs, heads_dir, &heads)?;
    Ok(Quarantined {
        stamp,
        heads,
        reason: reason.to_string(),
        at,
    })
}

/// Move packs out of the live store into the quarantine `stamp`, beside the
/// heads set aside there: each pack's index first, so a crash between the two
/// renames leaves a pack without an index, which the store does not read.
/// Re-running completes what a crash left.
pub fn set_aside_packs(
    fs: &dyn Fs,
    pack_dir: &Path,
    quarantine_dir: &Path,
    stamp: &str,
    packs: &[super::pack::PackName],
) -> Result<()> {
    if packs.is_empty() {
        return Ok(());
    }
    let target = quarantine_dir.join(stamp).join(PACKS_DIR);
    if !exists(fs, &target)? {
        fs.create_dir(&target)
            .map_err(|e| StoreError::io("create directory", &target, e))?;
        sync_dir(fs, &quarantine_dir.join(stamp))?;
    }
    for name in packs {
        for file in [name.idx_file(), name.pack_file()] {
            let (from, to) = (pack_dir.join(&file), target.join(&file));
            if exists(fs, &from)? {
                fs.rename(&from, &to)
                    .map_err(|e| StoreError::io("set aside pack", &to, e))?;
            }
        }
    }
    sync_dir(fs, &target)?;
    sync_dir(fs, pack_dir)
}

/// Every quarantine, oldest first.
pub fn list_quarantined(source: &dyn Source, quarantine_dir: &Path) -> Result<Vec<Quarantined>> {
    let entries = source
        .read_dir(quarantine_dir)
        .map_err(|e| StoreError::io("list quarantine", quarantine_dir, e))?;
    let mut out = vec![];
    for entry in entries {
        let name = file_name(&entry.path)?;
        let parsed = NaiveDateTime::parse_from_str(name, STAMP_FORMAT)
            .ok()
            .filter(|t| t.format(STAMP_FORMAT).to_string() == name);
        if parsed.is_none() {
            return Err(StoreError::corrupt(
                Corruption::UnexpectedEntry,
                &entry.path,
            ));
        }
        if entry.meta.kind != Kind::Dir {
            return Err(StoreError::corrupt(Corruption::NotADirectory, &entry.path));
        }
        let inner = source
            .read_dir(&entry.path)
            .map_err(|e| StoreError::io("list quarantine", &entry.path, e))?;
        let names: Vec<&str> = inner
            .iter()
            .map(|e| file_name(&e.path))
            .collect::<Result<_>>()?;
        if names != [HEADS_DIR, REASON_FILE] && names != [HEADS_DIR, PACKS_DIR, REASON_FILE] {
            return Err(StoreError::corrupt(
                Corruption::UnexpectedEntry,
                &entry.path,
            ));
        }
        let reason_path = entry.path.join(REASON_FILE);
        let bytes = source
            .read(&reason_path)
            .map_err(|e| StoreError::io("read reason", &reason_path, e))?;
        let (at, reason) =
            decode_reason(&bytes).map_err(|what| StoreError::corrupt(what, &reason_path))?;
        if stamp(&at) != name {
            return Err(StoreError::corrupt(
                Corruption::Ref(format!("recorded at {at}, filed as {name}")),
                &reason_path,
            ));
        }
        out.push(Quarantined {
            stamp: name.to_string(),
            heads: Heads::list(source, &entry.path.join(HEADS_DIR))?,
            reason,
            at,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{
        Layout,
        revision::tests::at,
        test_fs::{Faulty, Op, StdFs},
    };

    fn fixture() -> (StdFs, Layout, PathBuf) {
        let (fs, layout, staging) = crate::store::objects::tests::fixture();
        (fs, layout, staging)
    }

    #[test]
    fn test8270_advance_creates_the_new_head_durably_before_removing_parents() {
        let (fs, layout, _) = fixture();
        let dir = layout.heads();
        let (a, b, c) = (Id::of(b"a"), Id::of(b"b"), Id::of(b"c"));
        Heads::advance(&fs, &dir, &a, &[]).unwrap();
        Heads::advance(&fs, &dir, &b, &[]).unwrap();
        assert_eq!(Heads::list(&fs, &dir).unwrap(), {
            let mut v = vec![a, b];
            v.sort();
            v
        });
        let faulty = Faulty::new(&fs, None);
        Heads::advance(&faulty, &dir, &c, &[a, b]).unwrap();
        let log = faulty.log();
        assert_eq!(log[0], Op::CreateNew(dir.join(c.to_hex())));
        assert_eq!(
            log[1],
            Op::SyncDir(dir.clone()),
            "the new head is durable first"
        );
        assert!(matches!(&log[2..4], [Op::RemoveFile(_), Op::RemoveFile(_)]));
        assert_eq!(log[4], Op::SyncDir(dir.clone()));
        assert_eq!(Heads::list(&fs, &dir).unwrap(), vec![c]);

        // A re-run after a crash at any point converges, and still syncs.
        let again = Faulty::new(&fs, None);
        Heads::advance(&again, &dir, &c, &[a, b]).unwrap();
        assert_eq!(again.log().last(), Some(&Op::SyncDir(dir.clone())));
        assert_eq!(Heads::list(&fs, &dir).unwrap(), vec![c]);
        assert!(matches!(
            Heads::advance(&fs, &dir, &c, &[c]),
            Err(StoreError::Invalid(Invalid::HeadIsParent(_)))
        ));
    }

    #[test]
    fn test8271_a_failure_anywhere_in_advance_never_leaves_no_head() {
        let (a, c) = (Id::of(b"a"), Id::of(b"c"));
        for fail_at in 0..5 {
            let (fs, layout, _) = fixture();
            let dir = layout.heads();
            Heads::advance(&fs, &dir, &a, &[]).unwrap();
            let faulty = Faulty::new(&fs, Some(fail_at));
            let result = Heads::advance(&faulty, &dir, &c, &[a]);
            let heads = Heads::list(&fs, &dir).unwrap();
            assert!(!heads.is_empty(), "fault at {fail_at}");
            if result.is_ok() {
                assert_eq!(heads, vec![c]);
            }
            Heads::advance(&fs, &dir, &c, &[a]).unwrap();
            assert_eq!(
                Heads::list(&fs, &dir).unwrap(),
                vec![c],
                "a re-run converges"
            );
        }
    }

    #[test]
    fn test8272_the_heads_directory_holds_only_empty_files_named_by_ids() {
        for (name, contents, expected) in [
            ("not-an-id", Some(&b""[..]), Corruption::UnexpectedEntry),
            (
                &*Id::of(b"x").to_hex().to_uppercase(),
                Some(b""),
                Corruption::UnexpectedEntry,
            ),
            (
                &*Id::of(b"x").to_hex(),
                Some(b"x"),
                Corruption::NonEmptyHead,
            ),
            (&*Id::of(b"x").to_hex(), None, Corruption::NotAFile),
        ] {
            let (fs, layout, _) = fixture();
            let path = layout.heads().join(name);
            match contents {
                Some(bytes) => std::fs::write(&path, bytes).unwrap(),
                None => std::fs::create_dir(&path).unwrap(),
            }
            let error = Heads::list(&fs, &layout.heads()).unwrap_err();
            assert!(
                matches!(&error, StoreError::Corrupt { what, path: p } if *what == expected && *p == path),
                "{name}: {error:?}"
            );
        }
    }

    fn snapshot() -> SnapshotRef {
        SnapshotRef {
            tree: Id::of(b"tree"),
            state: Hash256::of(b"state"),
            revision: Some(Id::of(b"rev")),
            created: at(1_790_000_000),
            valid: false,
        }
    }

    #[test]
    fn test8273_snapshot_refs_round_trip_canonically_and_refuse_other_spellings() {
        let bytes = snapshot().encode().unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert_eq!(
            text,
            format!(
                "{{\"created\":\"2026-09-21T14:13:20.000000000Z\",\"kind\":\"snapshot\",\"reldir_ref\":3,\"revision\":\"{}\",\"state\":\"{}\",\"tree\":\"{}\",\"valid\":false}}",
                Id::of(b"rev"),
                Hash256::of(b"state"),
                Id::of(b"tree")
            )
        );
        assert_eq!(SnapshotRef::decode(&bytes).unwrap(), snapshot());
        let none = SnapshotRef {
            revision: None,
            ..snapshot()
        };
        assert_eq!(SnapshotRef::decode(&none.encode().unwrap()).unwrap(), none);
        assert_eq!(
            SnapshotRef::decode(text.replace(",\"kind\"", " ,\"kind\"").as_bytes()).unwrap_err(),
            Corruption::NonCanonical
        );
        assert!(matches!(
            SnapshotRef::decode(text.replace("\"snapshot\"", "\"branch\"").as_bytes()).unwrap_err(),
            Corruption::Ref(_)
        ));
        assert_eq!(
            SnapshotRef::decode(
                text.replace("\"reldir_ref\":3", "\"reldir_ref\":2")
                    .as_bytes()
            )
            .unwrap_err(),
            Corruption::UnsupportedVersion(2)
        );
        assert!(matches!(
            SnapshotRef::decode(text.replace("\"valid\":false", "\"valid\":0").as_bytes())
                .unwrap_err(),
            Corruption::Ref(_)
        ));
    }

    #[test]
    fn test8274_snapshot_names_are_portable_lowercase_names() {
        for good in ["before-import", "v1.2", "a", "2026_10_11", "con-x"] {
            validate_snapshot_name(good).unwrap();
        }
        for bad in [
            "",
            "Before",
            "a/b",
            "a\\b",
            ".hidden",
            "-x",
            "x.",
            "con",
            "nul.txt",
            "com1",
            "lpt9.x",
            "a b",
            "é",
            &"a".repeat(129),
        ] {
            assert!(
                matches!(
                    validate_snapshot_name(bad),
                    Err(StoreError::Invalid(Invalid::SnapshotName(_)))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn test8275_snapshot_refs_are_written_whole_listed_and_deleted() {
        let (fs, layout, staging) = fixture();
        let dir = layout.snapshot_refs();
        write_snapshot(&fs, &staging, &dir, "one", &snapshot()).unwrap();
        assert!(matches!(
            write_snapshot(&fs, &staging, &dir, "one", &snapshot()),
            Err(StoreError::Invalid(Invalid::RefExists(_)))
        ));
        assert_eq!(read_snapshot(&fs, &dir, "one").unwrap(), Some(snapshot()));
        assert_eq!(read_snapshot(&fs, &dir, "two").unwrap(), None);
        assert_eq!(
            list_snapshots(&fs, &dir).unwrap(),
            vec![("one".to_string(), snapshot())]
        );
        assert!(
            std::fs::read_dir(&staging).unwrap().next().is_none(),
            "nothing left staged"
        );
        std::fs::write(dir.join("Bad"), b"{}").unwrap();
        assert!(matches!(
            list_snapshots(&fs, &dir),
            Err(StoreError::Corrupt {
                what: Corruption::UnexpectedEntry,
                ..
            })
        ));
        std::fs::remove_file(dir.join("Bad")).unwrap();
        std::fs::write(dir.join("torn"), b"{\"created\":").unwrap();
        assert!(matches!(
            list_snapshots(&fs, &dir),
            Err(StoreError::Corrupt {
                what: Corruption::Ref(_),
                ..
            })
        ));
        std::fs::remove_file(dir.join("torn")).unwrap();
        delete_snapshot(&fs, &dir, "one").unwrap();
        assert!(matches!(
            delete_snapshot(&fs, &dir, "one"),
            Err(StoreError::Invalid(Invalid::NotPresent(_)))
        ));
        assert!(list_snapshots(&fs, &dir).unwrap().is_empty());
    }

    #[test]
    fn test8276_quarantine_moves_heads_aside_whole_and_lists_them_back() {
        let (fs, layout, staging) = fixture();
        let (a, b) = (Id::of(b"a"), Id::of(b"b"));
        Heads::advance(&fs, &layout.heads(), &a, &[]).unwrap();
        Heads::advance(&fs, &layout.heads(), &b, &[]).unwrap();
        let when = DateTime::from_timestamp(1_790_000_000, 5).unwrap();
        let done = quarantine(
            &fs,
            &staging,
            &layout.heads(),
            &layout.quarantine(),
            "unreadable \"x\"",
            when,
        )
        .unwrap();
        assert_eq!(done.stamp, "20260921T141320.000000005Z");
        assert!(Heads::list(&fs, &layout.heads()).unwrap().is_empty());
        let listed = list_quarantined(&fs, &layout.quarantine()).unwrap();
        assert_eq!(listed, vec![done.clone()]);
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(listed[0].heads, expected);
        assert!(matches!(
            quarantine(
                &fs,
                &staging,
                &layout.heads(),
                &layout.quarantine(),
                "again",
                when
            ),
            Err(StoreError::Io { .. })
        ));
        // A stray file inside a quarantine is refused.
        std::fs::write(layout.quarantine().join(&done.stamp).join("extra"), b"").unwrap();
        assert!(matches!(
            list_quarantined(&fs, &layout.quarantine()),
            Err(StoreError::Corrupt {
                what: Corruption::UnexpectedEntry,
                ..
            })
        ));
    }

    #[test]
    fn test8277_a_failed_quarantine_leaves_the_heads_and_no_partial_quarantine() {
        let a = Id::of(b"a");
        let when = DateTime::from_timestamp(1_790_000_000, 0).unwrap();
        let mut completed = false;
        for fail_at in 0..20 {
            let (fs, layout, staging) = fixture();
            Heads::advance(&fs, &layout.heads(), &a, &[]).unwrap();
            let faulty = Faulty::new(&fs, Some(fail_at));
            let result = quarantine(
                &faulty,
                &staging,
                &layout.heads(),
                &layout.quarantine(),
                "r",
                when,
            );
            let listed = list_quarantined(&fs, &layout.quarantine()).unwrap();
            let heads = Heads::list(&fs, &layout.heads()).unwrap();
            match result {
                Ok(_) => {
                    completed = true;
                    assert_eq!(listed.len(), 1);
                }
                Err(_) => {
                    // Either nothing was published, or the quarantine is
                    // complete and the heads are still in place.
                    assert!(listed.len() <= 1, "fault at {fail_at}");
                    if listed.is_empty() {
                        assert_eq!(heads, vec![a], "fault at {fail_at}");
                    }
                    assert!(listed.len() == 1 || heads == vec![a]);
                }
            }
        }
        assert!(completed, "enough operations to reach the end");
    }
}
