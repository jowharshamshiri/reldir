//! A deterministic in-memory filesystem that models what survives a crash.
//!
//! # The durability model
//!
//! The simulation keeps two images of the tree -- the *live* one a process
//! sees (the page cache) and the *durable* one on stable storage -- and an
//! ordered log of the operations issued since, which are in the cache but not
//! yet on the medium. Each is one of:
//!
//! - a **data** write: a file's new contents (every write replaces the whole
//!   file, as [`Fs::write`] does);
//! - an **entry** operation on a directory: link (create, mkdir), unlink
//!   (remove, rmdir) or rename.
//!
//! [`Fs::sync_file`] moves one file's data writes to the durable image.
//! [`Fs::sync_dir`] moves one directory's entry operations there -- and every
//! earlier entry operation they depend on, because a rename is one atomic
//! operation even when it touches two directories. A file's *name* is durable
//! only through its directory, and a new directory is reachable after a crash
//! only once *its* parent is synced: entries made durable inside a directory
//! whose own creation is not are orphans that a reboot does not find.
//!
//! # What a crash leaves
//!
//! [`Sim::crash`] returns the filesystem a reboot would find, and POSIX
//! promises little about it. Following the crash-vulnerability studies of
//! Pillai et al. ("All File Systems Are Not Created Equal", OSDI '14), the
//! simulation can produce any state the contract permits, not only the
//! synced one:
//!
//! - [`CrashModel::DurableOnly`]: nothing pending persisted -- the most
//!   forgetful outcome.
//! - [`CrashModel::Prefix`]: the first `n` pending operations persisted, in
//!   order, as on a filesystem that writes back in issue order (ext4 with
//!   `data=ordered` is close).
//! - [`CrashModel::Subset`]: an arbitrary seeded subset persisted, with a
//!   data write possibly *torn* -- its first whole 4096-byte blocks written,
//!   the rest of the new size zero-filled, or only the truncation done. Data
//!   and entry operations are not ordered with respect to one another, so a
//!   rename can persist without the data of the file it publishes: the
//!   zero-length-file-after-crash behaviour of delayed allocation.
//! - [`Sim::crash_subset`] with [`Sim::pending_count`]: one exact subset, so a
//!   test can enumerate every state a short sequence can leave.
//!
//! Two constraints hold in every model, because every real filesystem keeps
//! them: a rename is atomic (both of its names change, or neither), and entry
//! operations on the same file or the same name persist in issue order -- an
//! entry operation persists only if every earlier one sharing an inode or a
//! `(directory, name)` slot with it does. Without the second, a file renamed
//! twice could surface under both names.
//!
//! # Processes
//!
//! Handles share one filesystem; each belongs to a simulated process
//! ([`Sim::process`]). [`Sim::kill`] ends a process: its later operations
//! fail and the kernel releases its locks and open handles, but the page
//! cache -- every write it made -- survives, as it does when a real process
//! dies. Only [`Sim::crash`] models losing the machine.
//!
//! # Faults and conditions
//!
//! [`Sim::fault_at`] fails, tears or crashes the n-th mutating operation, and
//! [`Fault::SyncFailsDropsData`] models Linux after a failed writeback: the
//! dirty pages are marked clean and their data is gone, so a later fsync
//! succeeds without writing it. Persistent conditions -- a name length limit,
//! a full disk, a read-only subtree, a mount boundary, Windows semantics --
//! are set once and hold until changed.
//!
//! Reads ([`Source`] and the probes `volume` and `mtime_granularity_ns`) are
//! not counted as operations for fault injection: a crash before a read is
//! the same state as a crash before the next write, and counting reads would
//! double the cost of enumerating crash points.

use super::{
    Durability, Entry, Fs, Kind, LockGuard, Locks, Meta, Source, Stat, Volume, VolumeClass,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    io,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

type Ino = u64;

/// The root directory's inode.
const ROOT: Ino = 1;

/// The unit in which unsynced data reaches the medium.
const BLOCK: usize = 4096;

const NANOS: i64 = 1_000_000_000;

/// Where the simulated clock starts: 2023-11-14T22:13:20Z. Any fixed instant
/// would do; a recent one keeps timestamps plausible in failure messages.
const EPOCH_NS: i64 = 1_700_000_000 * NANOS;

/// How far the clock moves per mutating operation. A millisecond, so a run of
/// operations spans distinct nanosecond timestamps but shares a timestamp on a
/// one-second filesystem -- the racy case change detection must handle.
const STEP_NS: i64 = 1_000_000;

/// The byte [`Fs::touch`] writes.
const TOUCH_BYTE: u8 = b'.';

/// A fault [`Sim`] injects at a chosen mutating operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The process dies before the operation takes effect. Its later
    /// operations fail and its locks are released; the page cache survives.
    Crash,
    /// The operation fails with an error of this kind and has no effect.
    Error(io::ErrorKind),
    /// The process dies partway through a write: the first half of the bytes
    /// reach the page cache, and nothing more happens. At an operation that
    /// writes no data it is a plain [`Fault::Crash`] -- there is nothing to
    /// tear.
    TornWrite,
    /// The file sync fails with EIO and the file's dirty data is discarded:
    /// the live contents revert to the last durable contents, and a later
    /// successful sync does not bring the lost data back (Linux marks the
    /// pages clean after a failed writeback). At an operation that syncs no
    /// file data it is an EIO with no effect.
    SyncFailsDropsData,
}

/// Which of the states a power loss permits [`Sim::crash`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashModel {
    /// Only what was synced survives.
    DurableOnly,
    /// A seeded, arbitrary subset of the pending operations survives, data
    /// writes possibly torn at block granularity.
    Subset(u64),
    /// The first `n` pending operations survive, whole and in order.
    Prefix(usize),
}

/// How many operations of each kind were made, for cost-budget tests. A
/// failed attempt counts: the system call was made.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// `metadata` calls.
    pub stats: u64,
    pub read_dirs: u64,
    /// `read` and `read_at` calls.
    pub reads: u64,
    pub bytes_read: u64,
    /// `create_new`, `write`, `create_synced` and `touch` calls.
    pub writes: u64,
    pub bytes_written: u64,
    /// `sync_file` and `create_synced` calls.
    pub syncs_file: u64,
    /// Of `syncs_file`, those that asked only for [`Durability::Barrier`].
    pub barriers: u64,
    pub syncs_dir: u64,
    pub renames: u64,
    /// `remove_file` calls.
    pub removes: u64,
    pub dirs_created: u64,
    pub dirs_removed: u64,
    pub volumes: u64,
    /// `mtime_granularity_ns` calls.
    pub probes: u64,
    /// `try_lock` calls.
    pub locks: u64,
}

/// One operation in the cache that has not reached the medium, as a test sees
/// it: what it does and to which path (as named when it was issued).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOp {
    pub kind: PendingKind,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingKind {
    /// A file's contents.
    Data,
    Create,
    Mkdir,
    Unlink,
    Rmdir,
    Rename {
        to: PathBuf,
    },
}

#[derive(Debug, Clone)]
enum Node {
    File {
        data: Vec<u8>,
        mtime: i64,
        ctime: i64,
    },
    Dir {
        entries: BTreeMap<OsString, Ino>,
        mtime: i64,
        ctime: i64,
    },
}

impl Node {
    fn file(at: i64) -> Self {
        Node::File {
            data: vec![],
            mtime: at,
            ctime: at,
        }
    }

    fn dir(at: i64) -> Self {
        Node::Dir {
            entries: BTreeMap::new(),
            mtime: at,
            ctime: at,
        }
    }
}

/// A change to a tree, expressed by inode so it can be applied to an image
/// in which earlier changes did or did not persist.
#[derive(Debug, Clone)]
enum Change {
    Data {
        ino: Ino,
        bytes: Vec<u8>,
    },
    Link {
        dir: Ino,
        name: OsString,
        ino: Ino,
        is_dir: bool,
    },
    Unlink {
        dir: Ino,
        name: OsString,
        ino: Ino,
        is_dir: bool,
    },
    Rename {
        from_dir: Ino,
        from_name: OsString,
        to_dir: Ino,
        to_name: OsString,
        ino: Ino,
        /// The entry the rename replaced, by the name it was stored under
        /// (which differs from `to_name` only in case, in Windows mode).
        replaced: Option<(OsString, Ino)>,
    },
}

impl Change {
    fn is_entry(&self) -> bool {
        !matches!(self, Change::Data { .. })
    }

    /// Inodes an entry operation acts on (not the directories it acts in).
    fn inodes(&self) -> Vec<Ino> {
        match self {
            Change::Data { .. } => vec![],
            Change::Link { ino, .. } | Change::Unlink { ino, .. } => vec![*ino],
            Change::Rename { ino, replaced, .. } => {
                let mut inodes = vec![*ino];
                inodes.extend(replaced.as_ref().map(|(_, old)| *old));
                inodes
            }
        }
    }

    /// `(directory, name)` slots an entry operation changes.
    fn slots(&self) -> Vec<(Ino, &OsStr)> {
        match self {
            Change::Data { .. } => vec![],
            Change::Link { dir, name, .. } | Change::Unlink { dir, name, .. } => {
                vec![(*dir, name.as_os_str())]
            }
            Change::Rename {
                from_dir,
                from_name,
                to_dir,
                to_name,
                replaced,
                ..
            } => {
                let mut slots = vec![
                    (*from_dir, from_name.as_os_str()),
                    (*to_dir, to_name.as_os_str()),
                ];
                slots.extend(
                    replaced
                        .as_ref()
                        .map(|(name, _)| (*to_dir, name.as_os_str())),
                );
                slots
            }
        }
    }

    /// Whether two entry operations must persist in issue order.
    fn conflicts(&self, other: &Change) -> bool {
        let inodes = other.inodes();
        let slots = other.slots();
        self.inodes().iter().any(|ino| inodes.contains(ino))
            || self.slots().iter().any(|slot| slots.contains(slot))
    }

    fn touches_dir(&self, directory: Ino) -> bool {
        match self {
            Change::Data { .. } => false,
            Change::Link { dir, .. } | Change::Unlink { dir, .. } => *dir == directory,
            Change::Rename {
                from_dir, to_dir, ..
            } => *from_dir == directory || *to_dir == directory,
        }
    }
}

#[derive(Debug, Clone)]
struct Op {
    /// The filesystem time the operation was stamped with.
    at: i64,
    path: PathBuf,
    to: Option<PathBuf>,
    change: Change,
}

impl Op {
    fn describe(&self) -> PendingOp {
        let kind = match &self.change {
            Change::Data { .. } => PendingKind::Data,
            Change::Link { is_dir: false, .. } => PendingKind::Create,
            Change::Link { is_dir: true, .. } => PendingKind::Mkdir,
            Change::Unlink { is_dir: false, .. } => PendingKind::Unlink,
            Change::Unlink { is_dir: true, .. } => PendingKind::Rmdir,
            Change::Rename { .. } => PendingKind::Rename {
                to: self.to.clone().expect("a rename records its target"),
            },
        };
        PendingOp {
            kind,
            path: self.path.clone(),
        }
    }
}

/// A tree of inodes. Inodes no entry reaches may remain in a durable image
/// (their names were removed, or never persisted); a crash prunes them.
#[derive(Debug, Clone)]
struct Image {
    nodes: BTreeMap<Ino, Node>,
}

impl Image {
    fn new(at: i64) -> Self {
        Image {
            nodes: BTreeMap::from([(ROOT, Node::dir(at))]),
        }
    }

    fn entries(&self, ino: Ino) -> Option<&BTreeMap<OsString, Ino>> {
        match self.nodes.get(&ino) {
            Some(Node::Dir { entries, .. }) => Some(entries),
            _ => None,
        }
    }

    fn data(&self, ino: Ino) -> Option<&Vec<u8>> {
        match self.nodes.get(&ino) {
            Some(Node::File { data, .. }) => Some(data),
            _ => None,
        }
    }

    /// A directory's entries for changing, stamping its times. A directory
    /// missing from a durable image (its creation did not persist) is made, so
    /// changes inside it can persist without making it reachable.
    fn dir_mut(&mut self, ino: Ino, at: i64) -> &mut BTreeMap<OsString, Ino> {
        match self.nodes.entry(ino).or_insert_with(|| Node::dir(at)) {
            Node::Dir {
                entries,
                mtime,
                ctime,
            } => {
                *mtime = at;
                *ctime = at;
                entries
            }
            Node::File { .. } => {
                unreachable!("model invariant: inode {ino} is a file, not a directory")
            }
        }
    }

    fn apply(&mut self, at: i64, change: &Change) {
        match change {
            Change::Data { ino, bytes } => {
                match self.nodes.entry(*ino).or_insert_with(|| Node::file(at)) {
                    Node::File { data, mtime, ctime } => {
                        data.clone_from(bytes);
                        *mtime = at;
                        *ctime = at;
                    }
                    Node::Dir { .. } => {
                        unreachable!("model invariant: data written to directory inode {ino}")
                    }
                }
            }
            Change::Link {
                dir,
                name,
                ino,
                is_dir,
            } => {
                self.nodes.entry(*ino).or_insert_with(|| {
                    if *is_dir {
                        Node::dir(at)
                    } else {
                        Node::file(at)
                    }
                });
                self.dir_mut(*dir, at).insert(name.clone(), *ino);
            }
            Change::Unlink { dir, name, ino, .. } => {
                let entries = self.dir_mut(*dir, at);
                if entries.get(name) == Some(ino) {
                    entries.remove(name);
                }
            }
            Change::Rename {
                from_dir,
                from_name,
                to_dir,
                to_name,
                ino,
                replaced,
            } => {
                match self.nodes.get_mut(ino) {
                    Some(Node::File { ctime, .. } | Node::Dir { ctime, .. }) => *ctime = at,
                    None => unreachable!(
                        "model invariant: inode {ino} renamed before the operation creating it"
                    ),
                }
                let source = self.dir_mut(*from_dir, at);
                if source.get(from_name) == Some(ino) {
                    source.remove(from_name);
                }
                let target = self.dir_mut(*to_dir, at);
                if let Some((name, old)) = replaced
                    && target.get(name) == Some(old)
                {
                    target.remove(name);
                }
                target.insert(to_name.clone(), *ino);
            }
        }
    }

    /// Every reachable path and its inode, parents before children.
    fn walk(&self) -> Vec<(PathBuf, Ino)> {
        let mut out = vec![];
        let mut stack = vec![(PathBuf::from("/"), ROOT)];
        while let Some((path, ino)) = stack.pop() {
            if let Some(entries) = self.entries(ino) {
                for (name, child) in entries.iter().rev() {
                    stack.push((path.join(name), *child));
                }
            }
            out.push((path, ino));
        }
        out
    }

    /// The image with every unreachable inode dropped: what a reboot (and
    /// fsck) finds.
    fn reachable(&self) -> Image {
        let keep: BTreeSet<Ino> = self.walk().into_iter().map(|(_, ino)| ino).collect();
        Image {
            nodes: self
                .nodes
                .iter()
                .filter(|(ino, _)| keep.contains(ino))
                .map(|(ino, node)| (*ino, node.clone()))
                .collect(),
        }
    }
}

/// The conditions a test sets on the simulated filesystem and its machine.
/// They survive a crash, because they describe the hardware and the mount,
/// not the running system.
#[derive(Debug, Clone)]
struct Environment {
    name_max: u64,
    space_limit: Option<u64>,
    denied: Vec<PathBuf>,
    mounts: Vec<PathBuf>,
    windows: bool,
    kind: String,
    class: VolumeClass,
    granularity_ns: i64,
}

#[derive(Debug)]
struct World {
    live: Image,
    durable: Image,
    pending: Vec<Op>,
    next_ino: Ino,
    clock_ns: i64,
    /// Mutating operations attempted so far, the index faults are placed by.
    performed: u64,
    faults: BTreeMap<u64, Fault>,
    dead: BTreeSet<u32>,
    /// Lock file inode to (owning process, guard token).
    locks: BTreeMap<Ino, (u32, u64)>,
    next_token: u64,
    /// Inodes held open (Windows mode), with the processes holding them.
    held: BTreeSet<(Ino, u32)>,
    env: Environment,
    counters: Counters,
}

impl World {
    fn new(image: Image, env: Environment, next_ino: Ino, clock_ns: i64) -> Self {
        World {
            live: image.clone(),
            durable: image,
            pending: vec![],
            next_ino,
            clock_ns,
            performed: 0,
            faults: BTreeMap::new(),
            dead: BTreeSet::new(),
            locks: BTreeMap::new(),
            next_token: 0,
            held: BTreeSet::new(),
            env,
            counters: Counters::default(),
        }
    }

    /// The time the filesystem stamps now, at its granularity.
    fn now(&self) -> i64 {
        self.clock_ns - self.clock_ns.rem_euclid(self.env.granularity_ns)
    }

    fn alloc(&mut self) -> Ino {
        let ino = self.next_ino;
        self.next_ino += 1;
        ino
    }

    fn kill(&mut self, process: u32) {
        self.dead.insert(process);
        self.locks.retain(|_, (owner, _)| *owner != process);
        self.held.retain(|(_, holder)| *holder != process);
    }

    /// Issue a change: it takes effect in the cache at once and reaches the
    /// medium later -- except an entry operation in Windows mode, which NTFS
    /// journals before the call returns.
    fn record(&mut self, path: &Path, to: Option<&Path>, change: Change) {
        let at = self.now();
        self.live.apply(at, &change);
        // A removed name's inode is gone from the live tree; there are no hard
        // links, so no other name reaches it.
        match &change {
            Change::Unlink { ino, .. } => {
                self.live.nodes.remove(ino);
            }
            Change::Rename {
                replaced: Some((_, old)),
                ..
            } => {
                self.live.nodes.remove(old);
            }
            _ => {}
        }
        if self.env.windows && change.is_entry() {
            self.durable.apply(at, &change);
        } else {
            self.pending.push(Op {
                at,
                path: path.to_path_buf(),
                to: to.map(Path::to_path_buf),
                change,
            });
        }
    }

    /// Make a file's data durable, as of now.
    fn persist_data(&mut self, ino: Ino) {
        let (synced, rest): (Vec<Op>, Vec<Op>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|op| matches!(op.change, Change::Data { ino: of, .. } if of == ino));
        for op in &synced {
            self.durable.apply(op.at, &op.change);
        }
        self.pending = rest;
    }

    /// A failed writeback: the dirty data is discarded and the cache shows
    /// what the medium holds.
    fn drop_dirty(&mut self, ino: Ino) {
        self.pending
            .retain(|op| !matches!(op.change, Change::Data { ino: of, .. } if of == ino));
        let durable = self.durable.data(ino).cloned().unwrap_or_default();
        if let Some(Node::File { data, .. }) = self.live.nodes.get_mut(&ino) {
            *data = durable;
        }
    }

    /// For each pending operation, the earlier ones that must persist before
    /// it can.
    fn dependencies(&self) -> Vec<Vec<usize>> {
        self.pending
            .iter()
            .enumerate()
            .map(|(index, op)| {
                if !op.change.is_entry() {
                    return vec![];
                }
                self.pending[..index]
                    .iter()
                    .enumerate()
                    .filter(|(_, earlier)| {
                        earlier.change.is_entry() && earlier.change.conflicts(&op.change)
                    })
                    .map(|(earlier, _)| earlier)
                    .collect()
            })
            .collect()
    }

    /// Make a directory's entry operations durable, with everything they
    /// depend on.
    fn persist_entries(&mut self, directory: Ino) {
        let dependencies = self.dependencies();
        let mut chosen = BTreeSet::new();
        let mut work: Vec<usize> = self
            .pending
            .iter()
            .enumerate()
            .filter(|(_, op)| op.change.touches_dir(directory))
            .map(|(index, _)| index)
            .collect();
        while let Some(index) = work.pop() {
            if chosen.insert(index) {
                work.extend(&dependencies[index]);
            }
        }
        let mut rest = vec![];
        for (index, op) in std::mem::take(&mut self.pending).into_iter().enumerate() {
            if chosen.contains(&index) {
                self.durable.apply(op.at, &op.change);
            } else {
                rest.push(op);
            }
        }
        self.pending = rest;
    }

    /// The path's names, validated: absolute, no `..`, each within the
    /// filesystem's name length limit.
    fn names(&self, path: &Path) -> io::Result<Vec<OsString>> {
        let mut components = path.components();
        if components.next() != Some(Component::RootDir) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not an absolute path", path.display()),
            ));
        }
        let mut names = vec![];
        for component in components {
            match component {
                Component::Normal(name) => {
                    if name_length(name, self.env.windows) > self.env.name_max {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidFilename,
                            format!(
                                "{}: a name is longer than the filesystem's limit of {}",
                                path.display(),
                                self.env.name_max
                            ),
                        ));
                    }
                    names.push(name.to_owned());
                }
                Component::CurDir => {}
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} is not a normalized absolute path", path.display()),
                    ));
                }
            }
        }
        Ok(names)
    }

    /// An entry of a live directory by name: exactly, or in Windows mode
    /// regardless of case. Returns the name as stored.
    fn find(&self, dir: Ino, name: &OsStr) -> Option<(OsString, Ino)> {
        let entries = self.live.entries(dir)?;
        if let Some(ino) = entries.get(name) {
            return Some((name.to_owned(), *ino));
        }
        if !self.env.windows {
            return None;
        }
        let folded = fold(name);
        entries
            .iter()
            .find(|(stored, _)| fold(stored) == folded)
            .map(|(stored, ino)| (stored.clone(), *ino))
    }

    fn walk_to(&self, names: &[OsString], path: &Path) -> io::Result<Ino> {
        let mut ino = ROOT;
        for name in names {
            if self.live.entries(ino).is_none() {
                return Err(not_a_directory(path));
            }
            ino = self.find(ino, name).ok_or_else(|| not_found(path))?.1;
        }
        Ok(ino)
    }

    fn resolve(&self, path: &Path) -> io::Result<Ino> {
        self.walk_to(&self.names(path)?, path)
    }

    /// The directory that holds `path`, and `path`'s last name.
    fn parent(&self, path: &Path) -> io::Result<(Ino, OsString)> {
        let mut names = self.names(path)?;
        let Some(name) = names.pop() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the root directory has no parent",
            ));
        };
        let dir = self.walk_to(&names, path)?;
        if self.live.entries(dir).is_none() {
            return Err(not_a_directory(path));
        }
        Ok((dir, name))
    }

    /// Whether `path` is `dir` or inside it, compared name by name (and
    /// regardless of case in Windows mode).
    fn under(&self, path: &Path, dir: &Path) -> bool {
        let names = |p: &Path| -> Vec<OsString> {
            p.components()
                .filter_map(|component| match component {
                    Component::Normal(name) if self.env.windows => Some(fold(name)),
                    Component::Normal(name) => Some(name.to_owned()),
                    _ => None,
                })
                .collect()
        };
        names(path).starts_with(&names(dir))
    }

    fn writable(&self, path: &Path) -> io::Result<()> {
        match self.env.denied.iter().find(|dir| self.under(path, dir)) {
            Some(dir) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{}: writes under {} are denied",
                    path.display(),
                    dir.display()
                ),
            )),
            None => Ok(()),
        }
    }

    /// The device holding `path`: the innermost mount boundary above it, or
    /// the root filesystem's.
    fn device(&self, path: &Path) -> u64 {
        self.env
            .mounts
            .iter()
            .enumerate()
            .filter(|(_, mount)| self.under(path, mount))
            .max_by_key(|(_, mount)| mount.components().count())
            .map_or(1, |(index, _)| index as u64 + 2)
    }

    /// In Windows mode, refuse to rename or remove a file another handle has
    /// open (without FILE_SHARE_DELETE, as reldir's own readers open files).
    fn not_held(&self, ino: Ino, path: &Path) -> io::Result<()> {
        if self.env.windows && self.held.iter().any(|(held, _)| *held == ino) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "sharing violation: {} is open in another handle (ERROR_SHARING_VIOLATION)",
                    path.display()
                ),
            ));
        }
        Ok(())
    }

    fn used(&self) -> u64 {
        self.live
            .nodes
            .values()
            .map(|node| match node {
                Node::File { data, .. } => data.len() as u64,
                Node::Dir { .. } => 0,
            })
            .sum()
    }

    /// How many bytes a file now `old` bytes long may hold before the volume
    /// is full.
    fn room(&self, old: usize) -> usize {
        match self.env.space_limit {
            None => usize::MAX,
            Some(limit) => {
                let others = self.used() - old as u64;
                usize::try_from(limit.saturating_sub(others)).unwrap_or(usize::MAX)
            }
        }
    }

    fn meta(&self, ino: Ino, path: &Path) -> Meta {
        let device = self.device(path);
        match &self.live.nodes[&ino] {
            Node::File { data, mtime, ctime } => Meta {
                kind: Kind::File,
                links: 1,
                stat: Stat {
                    size: data.len() as u64,
                    mtime_ns: *mtime,
                    ctime_ns: *ctime,
                    inode: ino,
                    device,
                },
            },
            Node::Dir {
                entries,
                mtime,
                ctime,
            } => Meta {
                kind: Kind::Dir,
                links: 2 + entries
                    .values()
                    .filter(|child| self.live.entries(**child).is_some())
                    .count() as u64,
                stat: Stat {
                    size: 0,
                    mtime_ns: *mtime,
                    ctime_ns: *ctime,
                    inode: ino,
                    device,
                },
            },
        }
    }
}

/// What a fault left for the operation to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Injected {
    Nothing,
    Torn,
    SyncFails,
}

/// Which faults an operation can suffer beyond an error or a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpClass {
    Write,
    SyncFile,
    WriteAndSync,
    Other,
}

/// A name's length as the filesystem counts it: bytes, or UTF-16 units on
/// Windows.
fn name_length(name: &OsStr, windows: bool) -> u64 {
    if windows {
        name.to_string_lossy().encode_utf16().count() as u64
    } else {
        name.as_encoded_bytes().len() as u64
    }
}

/// A name folded for case-insensitive comparison the way NTFS does it: each
/// character through an upcase table that maps one character to one character
/// (so `ß` stays `ß`, where full Unicode uppercasing would make it `SS`).
fn fold(name: &OsStr) -> OsString {
    match name.to_str() {
        Some(text) => text
            .chars()
            .map(|c| {
                let mut upper = c.to_uppercase();
                match (upper.next(), upper.next()) {
                    (Some(single), None) => single,
                    _ => c,
                }
            })
            .collect::<String>()
            .into(),
        None => name.to_owned(),
    }
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{}: no such file or directory", path.display()),
    )
}

fn not_a_directory(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotADirectory,
        format!("{}: a component is not a directory", path.display()),
    )
}

fn is_a_directory(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::IsADirectory,
        format!("{} is a directory", path.display()),
    )
}

fn eio(what: &str) -> io::Error {
    io::Error::other(format!("simulated EIO: {what}"))
}

/// SplitMix64: a small, fast generator whose output is fully determined by
/// its seed, so a crash state is reproducible from the seed in a failure
/// message.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A draw from `0..n` by multiply-shift; its bias is below 2^-32 for the
    /// small `n` used here, and it is deterministic, which is what matters.
    fn below(&mut self, n: u64) -> u64 {
        ((u128::from(self.next()) * u128::from(n)) >> 64) as u64
    }
}

/// What a crash does with one pending operation.
enum Decision {
    Skip,
    Apply,
    /// Persist a data write with these contents instead of its own.
    Partial(Vec<u8>),
}

/// A deterministic in-memory filesystem that models what survives a crash.
/// See the module documentation for the model.
#[derive(Clone)]
pub struct Sim {
    world: Arc<Mutex<World>>,
    process: u32,
}

impl std::fmt::Debug for Sim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sim")
            .field("process", &self.process)
            .finish()
    }
}

impl Default for Sim {
    fn default() -> Self {
        Self::new()
    }
}

impl Sim {
    /// An empty filesystem holding only `/`, which is durable, seen by
    /// process 0.
    pub fn new() -> Self {
        let env = Environment {
            name_max: 255,
            space_limit: None,
            denied: vec![],
            mounts: vec![],
            windows: false,
            kind: "simfs".into(),
            class: VolumeClass::Local,
            granularity_ns: 1,
        };
        Sim {
            world: Arc::new(Mutex::new(World::new(
                Image::new(EPOCH_NS),
                env,
                ROOT + 1,
                EPOCH_NS,
            ))),
            process: 0,
        }
    }

    fn world(&self) -> MutexGuard<'_, World> {
        self.world
            .lock()
            .expect("the simulated filesystem was poisoned by a panic in another test thread")
    }

    // ----- processes -------------------------------------------------------

    /// A handle on the same filesystem belonging to simulated process `id`.
    /// Process ids are not reused: a process that was killed stays dead, and
    /// a restarted one takes a new id.
    pub fn process(&self, id: u32) -> Sim {
        Sim {
            world: Arc::clone(&self.world),
            process: id,
        }
    }

    /// End this handle's process. Its later operations fail; its locks and
    /// open handles are released; everything it wrote stays in the cache.
    pub fn kill(&self) {
        self.world().kill(self.process);
    }

    pub fn is_alive(&self) -> bool {
        !self.world().dead.contains(&self.process)
    }

    // ----- crashes ---------------------------------------------------------

    /// The filesystem a reboot after a power loss would find, under `model`.
    /// It is a new filesystem (seen by process 0) with the same environment;
    /// locks, open handles, faults and counters do not survive.
    pub fn crash(&self, model: CrashModel) -> Sim {
        let world = self.world();
        let count = world.pending.len();
        let decisions = match model {
            CrashModel::DurableOnly => (0..count).map(|_| Decision::Skip).collect(),
            CrashModel::Prefix(n) => {
                assert!(
                    n <= count,
                    "Prefix({n}) asks for more operations than the {count} pending"
                );
                (0..count)
                    .map(|index| {
                        if index < n {
                            Decision::Apply
                        } else {
                            Decision::Skip
                        }
                    })
                    .collect()
            }
            CrashModel::Subset(seed) => subset(&world, seed),
        };
        Self::rebuild(&world, decisions)
    }

    /// How many operations are in the cache but not on the medium.
    pub fn pending_count(&self) -> usize {
        self.world().pending.len()
    }

    /// The pending operations, oldest first: bit `i` of a
    /// [`Sim::crash_subset`] mask is `pending()[i]`.
    pub fn pending(&self) -> Vec<PendingOp> {
        self.world().pending.iter().map(Op::describe).collect()
    }

    /// The state after a crash in which exactly the pending operations whose
    /// bits are set in `mask` persisted, whole. `None` when that set breaks
    /// the model's ordering constraint (an entry operation persisted without
    /// an earlier one on the same file or name), because no filesystem
    /// produces it.
    ///
    /// Panics when there are more than 64 pending operations or `mask` names
    /// one that does not exist: either is a mistake in the test.
    pub fn crash_subset(&self, mask: u64) -> Option<Sim> {
        let world = self.world();
        let count = world.pending.len();
        assert!(
            count <= 64,
            "{count} pending operations cannot be addressed by a u64 mask"
        );
        assert!(
            count == 64 || mask >> count == 0,
            "mask {mask:#b} names operations beyond the {count} pending"
        );
        let included: Vec<bool> = (0..count).map(|index| mask >> index & 1 == 1).collect();
        let dependencies = world.dependencies();
        let admissible = (0..count).all(|index| {
            !included[index] || dependencies[index].iter().all(|earlier| included[*earlier])
        });
        if !admissible {
            return None;
        }
        let decisions = included
            .into_iter()
            .map(|on| if on { Decision::Apply } else { Decision::Skip })
            .collect();
        Some(Self::rebuild(&world, decisions))
    }

    fn rebuild(world: &World, decisions: Vec<Decision>) -> Sim {
        let mut image = world.durable.clone();
        for (op, decision) in world.pending.iter().zip(decisions) {
            match decision {
                Decision::Skip => {}
                Decision::Apply => image.apply(op.at, &op.change),
                Decision::Partial(bytes) => {
                    let Change::Data { ino, .. } = op.change else {
                        unreachable!("only data writes persist partially")
                    };
                    image.apply(op.at, &Change::Data { ino, bytes });
                }
            }
        }
        Sim {
            world: Arc::new(Mutex::new(World::new(
                image.reachable(),
                world.env.clone(),
                world.next_ino,
                world.clock_ns,
            ))),
            process: 0,
        }
    }

    // ----- faults ----------------------------------------------------------

    /// Inject `fault` at the `index`-th mutating operation from now (0 is the
    /// next one), counted across every process. A later call for the same
    /// operation replaces the earlier fault.
    pub fn fault_at(&self, index: u64, fault: Fault) {
        let mut world = self.world();
        let at = world.performed + index;
        world.faults.insert(at, fault);
    }

    /// Mutating operations attempted so far, across every process.
    pub fn operations(&self) -> u64 {
        self.world().performed
    }

    // ----- conditions ------------------------------------------------------

    /// Refuse names longer than `max` bytes (UTF-16 units in Windows mode)
    /// with `InvalidFilename`, as ENAMETOOLONG.
    pub fn set_name_max(&self, max: u64) {
        self.world().env.name_max = max;
    }

    /// Fail writes that would take the files' total size past `bytes` with
    /// `StorageFull`. As on a real disk, the bytes that fit are written
    /// first.
    pub fn set_space_limit(&self, bytes: u64) {
        self.world().env.space_limit = Some(bytes);
    }

    /// Refuse every write, sync, rename, removal and directory change under
    /// `dir` with `PermissionDenied`. Reads are still allowed.
    pub fn deny_writes_under(&self, dir: &Path) {
        assert!(
            dir.is_absolute() || dir.has_root(),
            "{} is not absolute",
            dir.display()
        );
        self.world().env.denied.push(dir.to_path_buf());
    }

    /// Make `dir` the root of another device: its files report a different
    /// `Stat::device`, and renames across the boundary fail with
    /// `CrossesDevices`.
    pub fn mount_boundary(&self, dir: &Path) {
        assert!(
            dir.is_absolute() || dir.has_root(),
            "{} is not absolute",
            dir.display()
        );
        self.world().env.mounts.push(dir.to_path_buf());
    }

    /// Windows (NTFS) semantics: names are looked up regardless of case and
    /// stored as given; every create, rename and removal is durable when it
    /// returns (NTFS journals metadata, and renames are write-through), so
    /// `sync_dir` is a no-op -- but file *data* still needs a sync; and a file
    /// held open ([`Sim::hold_open`]) cannot be renamed, replaced or removed.
    ///
    /// Panics if entry operations are pending, since there is no Windows
    /// meaning for a directory change that is not yet durable.
    pub fn windows_mode(&self, on: bool) {
        let mut world = self.world();
        assert!(
            !world.pending.iter().any(|op| op.change.is_entry()),
            "switch Windows mode before any directory operation is pending"
        );
        world.env.windows = on;
    }

    /// Hold `path` open from this process, as a reader without
    /// FILE_SHARE_DELETE does. In Windows mode a rename of it, a rename over
    /// it and its removal then fail with a sharing violation; elsewhere an
    /// open file can be renamed and removed, so holding it changes nothing.
    pub fn hold_open(&self, path: &Path) -> io::Result<()> {
        let mut world = self.world();
        let ino = world.resolve(path)?;
        world.held.insert((ino, self.process));
        Ok(())
    }

    /// Close this process's hold on `path`.
    pub fn release(&self, path: &Path) -> io::Result<()> {
        let mut world = self.world();
        let ino = world.resolve(path)?;
        if !world.held.remove(&(ino, self.process)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is not held open by process {}",
                    path.display(),
                    self.process
                ),
            ));
        }
        Ok(())
    }

    /// What [`Fs::volume`] reports about the filesystem's kind and class.
    pub fn set_volume(&self, kind: &str, class: VolumeClass) {
        let mut world = self.world();
        world.env.kind = kind.to_lowercase();
        world.env.class = class;
    }

    // ----- clock -----------------------------------------------------------

    /// Stamp times at multiples of `ns` (1 on ext4, 100 on NTFS, 1e9 on
    /// HFS+, 2e9 on FAT).
    pub fn set_mtime_granularity_ns(&self, ns: i64) {
        assert!(ns > 0, "a granularity of {ns}ns is not a granularity");
        self.world().env.granularity_ns = ns;
    }

    /// Move the filesystem clock forward.
    pub fn advance_clock(&self, ns: i64) {
        assert!(ns >= 0, "the filesystem clock does not run backwards");
        self.world().clock_ns += ns;
    }

    /// Set a path's modification time, as `touch -r` does, in the cache and
    /// on the medium at once: a fixture for staging a stat, not an operation
    /// the protocol performs. Its change time becomes now.
    pub fn set_mtime(&self, path: &Path, ns: i64) -> io::Result<()> {
        let mut guard = self.world();
        let world = &mut *guard;
        let ino = world.resolve(path)?;
        let now = world.now();
        for image in [&mut world.live, &mut world.durable] {
            if let Some(Node::File { mtime, ctime, .. } | Node::Dir { mtime, ctime, .. }) =
                image.nodes.get_mut(&ino)
            {
                *mtime = ns;
                *ctime = now;
            }
        }
        Ok(())
    }

    // ----- observation -----------------------------------------------------

    pub fn counters(&self) -> Counters {
        self.world().counters
    }

    pub fn reset_counters(&self) {
        self.world().counters = Counters::default();
    }

    /// Every file's current contents, by path.
    pub fn files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let world = self.world();
        world
            .live
            .walk()
            .into_iter()
            .filter_map(|(path, ino)| world.live.data(ino).map(|data| (path, data.clone())))
            .collect()
    }

    /// Every directory, by path, `/` included.
    pub fn dirs(&self) -> BTreeSet<PathBuf> {
        let world = self.world();
        world
            .live
            .walk()
            .into_iter()
            .filter(|(_, ino)| world.live.entries(*ino).is_some())
            .map(|(path, _)| path)
            .collect()
    }

    /// Create a file, and any missing directories above it, as already
    /// durable: the starting state of a test. Panics if operations are
    /// pending (the starting state comes first) or a component is a file.
    pub fn seed(&self, path: &Path, bytes: &[u8]) {
        let mut world = self.world();
        assert!(
            world.pending.is_empty(),
            "seed describes the durable starting state; call it before any operation"
        );
        let mut names = world.names(path).expect("a valid absolute path to seed");
        let name = names.pop().expect("a file path, not the root");
        let at = world.now();
        let mut dir = ROOT;
        for component in names {
            dir = match world.find(dir, &component) {
                Some((_, ino)) => {
                    assert!(
                        world.live.entries(ino).is_some(),
                        "cannot seed {}: {} is a file",
                        path.display(),
                        component.to_string_lossy()
                    );
                    ino
                }
                None => {
                    let ino = world.alloc();
                    let link = Change::Link {
                        dir,
                        name: component,
                        ino,
                        is_dir: true,
                    };
                    world.live.apply(at, &link);
                    world.durable.apply(at, &link);
                    ino
                }
            };
        }
        let ino = match world.find(dir, &name) {
            Some((_, ino)) => {
                assert!(
                    world.live.data(ino).is_some(),
                    "cannot seed over a directory"
                );
                ino
            }
            None => {
                let ino = world.alloc();
                let link = Change::Link {
                    dir,
                    name,
                    ino,
                    is_dir: false,
                };
                world.live.apply(at, &link);
                world.durable.apply(at, &link);
                ino
            }
        };
        let data = Change::Data {
            ino,
            bytes: bytes.to_vec(),
        };
        world.live.apply(at, &data);
        world.durable.apply(at, &data);
    }

    // ----- operation plumbing ---------------------------------------------

    fn alive(&self, world: &World) -> io::Result<()> {
        if world.dead.contains(&self.process) {
            return Err(io::Error::other(format!(
                "simulated process {} has died",
                self.process
            )));
        }
        Ok(())
    }

    /// Start a mutating operation: count it, move the clock, and fire the
    /// fault placed at it.
    fn begin(&self, world: &mut World, class: OpClass) -> io::Result<Injected> {
        self.alive(world)?;
        let index = world.performed;
        world.performed += 1;
        world.clock_ns += STEP_NS;
        let crash = |world: &mut World| {
            world.kill(self.process);
            Err(io::Error::other(format!(
                "simulated crash of process {} at operation {index}",
                self.process
            )))
        };
        match world.faults.remove(&index) {
            None => Ok(Injected::Nothing),
            Some(Fault::Error(kind)) => Err(io::Error::new(
                kind,
                format!("injected fault at operation {index}"),
            )),
            Some(Fault::TornWrite) if matches!(class, OpClass::Write | OpClass::WriteAndSync) => {
                Ok(Injected::Torn)
            }
            Some(Fault::SyncFailsDropsData)
                if matches!(class, OpClass::SyncFile | OpClass::WriteAndSync) =>
            {
                Ok(Injected::SyncFails)
            }
            Some(Fault::SyncFailsDropsData) => Err(eio(&format!("injected at operation {index}"))),
            Some(Fault::Crash | Fault::TornWrite) => crash(world),
        }
    }

    /// Open `path` for writing -- creating it, or with `exclusive` requiring
    /// that it be new -- and write `bytes` as its whole contents.
    fn put(
        &self,
        world: &mut World,
        path: &Path,
        bytes: &[u8],
        exclusive: bool,
        injected: Injected,
    ) -> io::Result<Ino> {
        world.writable(path)?;
        let (dir, name) = world.parent(path)?;
        let (ino, created) = match world.find(dir, &name) {
            Some(_) if exclusive => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} already exists", path.display()),
                ));
            }
            Some((_, ino)) if world.live.data(ino).is_none() => return Err(is_a_directory(path)),
            Some((_, ino)) => (ino, false),
            None => {
                let ino = world.alloc();
                world.record(
                    path,
                    None,
                    Change::Link {
                        dir,
                        name,
                        ino,
                        is_dir: false,
                    },
                );
                (ino, true)
            }
        };
        if injected == Injected::Torn {
            let half = bytes[..bytes.len() / 2].to_vec();
            world.counters.bytes_written += half.len() as u64;
            world.record(path, None, Change::Data { ino, bytes: half });
            world.kill(self.process);
            return Err(io::Error::other(format!(
                "simulated crash of process {} during a write to {}",
                self.process,
                path.display()
            )));
        }
        let old = world.live.data(ino).map_or(0, Vec::len);
        let room = world.room(old);
        if bytes.len() > room {
            world.counters.bytes_written += room as u64;
            world.record(
                path,
                None,
                Change::Data {
                    ino,
                    bytes: bytes[..room].to_vec(),
                },
            );
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!("{}: no space left on the device", path.display()),
            ));
        }
        world.counters.bytes_written += bytes.len() as u64;
        // A new file is already empty; an empty write to it changes nothing.
        if !(created && bytes.is_empty()) {
            world.record(
                path,
                None,
                Change::Data {
                    ino,
                    bytes: bytes.to_vec(),
                },
            );
        }
        Ok(ino)
    }

    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.reads += 1;
        let ino = world.resolve(path)?;
        let data = world
            .live
            .data(ino)
            .cloned()
            .ok_or_else(|| is_a_directory(path))?;
        Ok(data)
    }
}

/// The decisions of a seeded arbitrary crash: each entry operation persists
/// with even odds if everything it depends on did; each data write persists
/// whole, not at all, truncated only, or torn after a whole number of blocks.
fn subset(world: &World, seed: u64) -> Vec<Decision> {
    let mut random = SplitMix64(seed);
    let dependencies = world.dependencies();
    let mut included = vec![false; world.pending.len()];
    let mut decisions = vec![];
    for (index, op) in world.pending.iter().enumerate() {
        let decision = match &op.change {
            Change::Data { bytes, .. } => {
                let shapes = if bytes.is_empty() { 2 } else { 4 };
                match random.below(shapes) {
                    0 => Decision::Skip,
                    1 => Decision::Apply,
                    2 => Decision::Partial(vec![]),
                    _ => {
                        let blocks = bytes.len().div_ceil(BLOCK);
                        let whole = random.below(blocks as u64) as usize * BLOCK;
                        let mut torn = bytes[..whole].to_vec();
                        torn.resize(bytes.len(), 0);
                        Decision::Partial(torn)
                    }
                }
            }
            _ => {
                let possible = dependencies[index].iter().all(|earlier| included[*earlier]);
                if possible && random.below(2) == 1 {
                    Decision::Apply
                } else {
                    Decision::Skip
                }
            }
        };
        included[index] = !matches!(decision, Decision::Skip);
        decisions.push(decision);
    }
    decisions
}

impl Source for Sim {
    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.stats += 1;
        let ino = world.resolve(path)?;
        Ok(world.meta(ino, path))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<Entry>> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.read_dirs += 1;
        let ino = world.resolve(path)?;
        let entries = world
            .live
            .entries(ino)
            .ok_or_else(|| not_a_directory(path))?;
        // A BTreeMap of names iterates in the byte order of the stored names.
        Ok(entries
            .iter()
            .map(|(name, child)| {
                let child_path = path.join(name);
                Entry {
                    meta: world.meta(*child, &child_path),
                    path: child_path,
                }
            })
            .collect())
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let data = self.read_file(path)?;
        self.world().counters.bytes_read += data.len() as u64;
        Ok(data)
    }

    fn read_at(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let data = self.read_file(path)?;
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let range = start.checked_add(len).filter(|end| *end <= data.len());
        let Some(end) = range else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "{} is {} bytes; {len} bytes at {offset} were asked for",
                    path.display(),
                    data.len()
                ),
            ));
        };
        self.world().counters.bytes_read += len as u64;
        Ok(data[start..end].to_vec())
    }
}

impl Fs for Sim {
    fn create_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.writes += 1;
        let injected = self.begin(&mut world, OpClass::Write)?;
        self.put(&mut world, path, bytes, true, injected)
            .map(|_| ())
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.writes += 1;
        let injected = self.begin(&mut world, OpClass::Write)?;
        self.put(&mut world, path, bytes, false, injected)
            .map(|_| ())
    }

    fn create_synced(&self, path: &Path, bytes: &[u8], durability: Durability) -> io::Result<()> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.writes += 1;
        world.counters.syncs_file += 1;
        if durability == Durability::Barrier {
            world.counters.barriers += 1;
        }
        let injected = self.begin(&mut world, OpClass::WriteAndSync)?;
        let ino = self.put(&mut world, path, bytes, true, injected)?;
        if injected == Injected::SyncFails {
            world.drop_dirty(ino);
            return Err(eio(&format!("syncing {}", path.display())));
        }
        world.persist_data(ino);
        Ok(())
    }

    fn sync_file(&self, path: &Path, durability: Durability) -> io::Result<()> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.syncs_file += 1;
        if durability == Durability::Barrier {
            world.counters.barriers += 1;
        }
        let injected = self.begin(&mut world, OpClass::SyncFile)?;
        world.writable(path)?;
        let ino = world.resolve(path)?;
        if world.live.data(ino).is_none() {
            return Err(is_a_directory(path));
        }
        if injected == Injected::SyncFails {
            world.drop_dirty(ino);
            return Err(eio(&format!("syncing {}", path.display())));
        }
        world.persist_data(ino);
        Ok(())
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.syncs_dir += 1;
        self.begin(&mut world, OpClass::Other)?;
        let ino = world.resolve(path)?;
        if world.live.entries(ino).is_none() {
            return Err(not_a_directory(path));
        }
        // In Windows mode every entry operation was durable when it returned.
        if !world.env.windows {
            world.persist_entries(ino);
        }
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.renames += 1;
        self.begin(&mut world, OpClass::Other)?;
        world.writable(from)?;
        world.writable(to)?;
        let (from_dir, from_name) = world.parent(from)?;
        let (from_name, ino) = world
            .find(from_dir, &from_name)
            .ok_or_else(|| not_found(from))?;
        let (to_dir, to_name) = world.parent(to)?;
        if world.device(from) != world.device(to) {
            return Err(io::Error::new(
                io::ErrorKind::CrossesDevices,
                format!(
                    "{} and {} are on different devices",
                    from.display(),
                    to.display()
                ),
            ));
        }
        let target = world.find(to_dir, &to_name);
        if let Some((stored, existing)) = &target
            && *existing == ino
        {
            // The same file under both names: POSIX does nothing, and Windows
            // changes only the case of the name.
            if world.env.windows && from_dir == to_dir && *stored != to_name {
                world.not_held(ino, from)?;
                world.record(
                    from,
                    Some(to),
                    Change::Rename {
                        from_dir,
                        from_name,
                        to_dir,
                        to_name,
                        ino,
                        replaced: None,
                    },
                );
            }
            return Ok(());
        }
        world.not_held(ino, from)?;
        if let Some((_, existing)) = &target {
            world.not_held(*existing, to)?;
        }
        let moving_dir = world.live.entries(ino).is_some();
        if let Some((_, existing)) = &target {
            match (moving_dir, world.live.entries(*existing)) {
                (true, None) => return Err(not_a_directory(to)),
                (false, Some(_)) => return Err(is_a_directory(to)),
                // MoveFileEx does not replace a directory.
                (true, Some(_)) if world.env.windows => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("{} is a directory and cannot be replaced", to.display()),
                    ));
                }
                (true, Some(entries)) if !entries.is_empty() => {
                    return Err(io::Error::new(
                        io::ErrorKind::DirectoryNotEmpty,
                        format!("{} is not empty", to.display()),
                    ));
                }
                _ => {}
            }
        }
        if moving_dir && world.under(to, from) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} cannot be moved inside itself", from.display()),
            ));
        }
        world.record(
            from,
            Some(to),
            Change::Rename {
                from_dir,
                from_name,
                to_dir,
                to_name,
                ino,
                replaced: target,
            },
        );
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.removes += 1;
        self.begin(&mut world, OpClass::Other)?;
        world.writable(path)?;
        let (dir, name) = world.parent(path)?;
        let (name, ino) = world.find(dir, &name).ok_or_else(|| not_found(path))?;
        if world.live.data(ino).is_none() {
            return Err(is_a_directory(path));
        }
        world.not_held(ino, path)?;
        world.record(
            path,
            None,
            Change::Unlink {
                dir,
                name,
                ino,
                is_dir: false,
            },
        );
        Ok(())
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.dirs_created += 1;
        self.begin(&mut world, OpClass::Other)?;
        world.writable(path)?;
        let (dir, name) = world.parent(path)?;
        if world.find(dir, &name).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", path.display()),
            ));
        }
        let ino = world.alloc();
        world.record(
            path,
            None,
            Change::Link {
                dir,
                name,
                ino,
                is_dir: true,
            },
        );
        Ok(())
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.dirs_removed += 1;
        self.begin(&mut world, OpClass::Other)?;
        world.writable(path)?;
        if world.names(path)?.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "the root directory cannot be removed",
            ));
        }
        let (dir, name) = world.parent(path)?;
        let (name, ino) = world.find(dir, &name).ok_or_else(|| not_found(path))?;
        match world.live.entries(ino) {
            None => return Err(not_a_directory(path)),
            Some(entries) if !entries.is_empty() => {
                return Err(io::Error::new(
                    io::ErrorKind::DirectoryNotEmpty,
                    format!("{} is not empty", path.display()),
                ));
            }
            Some(_) => {}
        }
        world.not_held(ino, path)?;
        world.record(
            path,
            None,
            Change::Unlink {
                dir,
                name,
                ino,
                is_dir: true,
            },
        );
        Ok(())
    }

    fn volume(&self, path: &Path) -> io::Result<Volume> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.volumes += 1;
        world.resolve(path)?;
        Ok(Volume {
            name_max: world.env.name_max,
            device: world.device(path),
            kind: world.env.kind.clone(),
            class: world.env.class.clone(),
        })
    }

    fn touch(&self, path: &Path) -> io::Result<i64> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.writes += 1;
        let injected = self.begin(&mut world, OpClass::Write)?;
        world.writable(path)?;
        let (dir, name) = world.parent(path)?;
        let ino = match world.find(dir, &name) {
            Some((_, ino)) if world.live.data(ino).is_none() => return Err(is_a_directory(path)),
            Some((_, ino)) => ino,
            None => {
                let ino = world.alloc();
                world.record(
                    path,
                    None,
                    Change::Link {
                        dir,
                        name,
                        ino,
                        is_dir: false,
                    },
                );
                ino
            }
        };
        if injected == Injected::Torn {
            // Half of one byte is none of it.
            world.kill(self.process);
            return Err(io::Error::other(format!(
                "simulated crash of process {} during a write to {}",
                self.process,
                path.display()
            )));
        }
        let mut data = world.live.data(ino).cloned().unwrap_or_default();
        if data.is_empty() && world.room(0) == 0 {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!("{}: no space left on the device", path.display()),
            ));
        }
        match data.first_mut() {
            Some(first) => *first = TOUCH_BYTE,
            None => data.push(TOUCH_BYTE),
        }
        world.counters.bytes_written += 1;
        world.record(path, None, Change::Data { ino, bytes: data });
        Ok(world.now())
    }

    fn mtime_granularity_ns(&self, dir: &Path) -> io::Result<i64> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.probes += 1;
        let ino = world.resolve(dir)?;
        if world.live.entries(ino).is_none() {
            return Err(not_a_directory(dir));
        }
        Ok(world.env.granularity_ns)
    }
}

/// A lock held in the simulation, released when dropped -- unless the
/// process that took it died first, in which case the kernel already did.
struct SimLock {
    world: Arc<Mutex<World>>,
    ino: Ino,
    token: u64,
}

impl Drop for SimLock {
    fn drop(&mut self) {
        // A poisoned mutex means another test thread panicked; the lock must
        // still be released (a real kernel releases it on close regardless),
        // and panicking in a destructor could abort the test run.
        let mut world = match self.world.lock() {
            Ok(world) => world,
            Err(poisoned) => poisoned.into_inner(),
        };
        if world.locks.get(&self.ino).map(|(_, token)| *token) == Some(self.token) {
            world.locks.remove(&self.ino);
        }
    }
}

impl Locks for Sim {
    /// Like `flock`, a lock belongs to the handle that took it: a second
    /// attempt from the same process is refused while the first guard lives.
    fn try_lock(&self, path: &Path) -> io::Result<Option<LockGuard>> {
        let mut world = self.world();
        self.alive(&world)?;
        world.counters.locks += 1;
        self.begin(&mut world, OpClass::Other)?;
        world.writable(path)?;
        let (dir, name) = world.parent(path)?;
        let ino = match world.find(dir, &name) {
            Some((_, ino)) if world.live.data(ino).is_none() => return Err(is_a_directory(path)),
            Some((_, ino)) => ino,
            None => {
                let ino = world.alloc();
                world.record(
                    path,
                    None,
                    Change::Link {
                        dir,
                        name,
                        ino,
                        is_dir: false,
                    },
                );
                ino
            }
        };
        if world.locks.contains_key(&ino) {
            return Ok(None);
        }
        let token = world.next_token;
        world.next_token += 1;
        world.locks.insert(ino, (self.process, token));
        Ok(Some(LockGuard(Box::new(SimLock {
            world: Arc::clone(&self.world),
            ino,
            token,
        }))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Durability::Full;

    fn p(path: &str) -> &Path {
        Path::new(path)
    }

    fn tree(files: &[(&str, &[u8])]) -> BTreeMap<PathBuf, Vec<u8>> {
        files
            .iter()
            .map(|(path, bytes)| (PathBuf::from(path), bytes.to_vec()))
            .collect()
    }

    fn names(sim: &Sim, dir: &str) -> Vec<String> {
        sim.read_dir(p(dir))
            .unwrap()
            .into_iter()
            .map(|entry| {
                entry
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    /// A staged row replacing a seeded one, with an unrelated file created
    /// beside it: five pending operations.
    fn replace_and_create() -> Sim {
        let sim = Sim::new();
        sim.seed(p("/d/row"), b"old");
        sim.write(p("/d/row.tmp"), b"new").unwrap();
        sim.rename(p("/d/row.tmp"), p("/d/row")).unwrap();
        sim.write(p("/d/other"), b"o").unwrap();
        sim
    }

    #[test]
    fn test8020_only_synced_data_under_synced_entries_survives_durable_only() {
        let sim = Sim::new();
        sim.create_dir(p("/d")).unwrap();
        sim.sync_dir(p("/")).unwrap();
        sim.write(p("/d/unsynced"), b"lost").unwrap();
        sim.write(p("/d/synced"), b"kept").unwrap();
        sim.sync_file(p("/d/synced"), Full).unwrap();
        let after = sim.crash(CrashModel::DurableOnly);
        assert!(after.files().is_empty(), "no name in /d is durable yet");
        assert_eq!(
            after.dirs(),
            BTreeSet::from([PathBuf::from("/"), PathBuf::from("/d")])
        );
        sim.sync_dir(p("/d")).unwrap();
        assert_eq!(
            sim.crash(CrashModel::DurableOnly).files(),
            tree(&[("/d/synced", b"kept"), ("/d/unsynced", b"")]),
            "both names survive; only the synced data does"
        );
        assert_eq!(
            sim.read(p("/d/unsynced")).unwrap(),
            b"lost",
            "a crash image leaves the running system's cache alone"
        );
    }

    #[test]
    fn test8021_a_new_directory_is_reachable_only_once_its_parent_is_synced() {
        let sim = Sim::new();
        sim.create_dir(p("/a")).unwrap();
        sim.create_synced(p("/a/f"), b"x", Full).unwrap();
        sim.sync_dir(p("/a")).unwrap();
        let after = sim.crash(CrashModel::DurableOnly);
        assert_eq!(after.dirs(), BTreeSet::from([PathBuf::from("/")]));
        assert!(
            after.files().is_empty(),
            "/a's own entries are durable, but /a is not"
        );
        sim.sync_dir(p("/")).unwrap();
        assert_eq!(
            sim.crash(CrashModel::DurableOnly).files(),
            tree(&[("/a/f", b"x")])
        );
    }

    #[test]
    fn test8022_a_sync_makes_the_contents_durable_as_of_the_sync() {
        let sim = Sim::new();
        sim.seed(p("/d/row"), b"v1");
        sim.write(p("/d/row"), b"v2").unwrap();
        sim.sync_file(p("/d/row"), Full).unwrap();
        sim.write(p("/d/row"), b"v3").unwrap();
        assert_eq!(
            sim.crash(CrashModel::DurableOnly).files(),
            tree(&[("/d/row", b"v2")])
        );
        assert_eq!(
            sim.pending(),
            vec![PendingOp {
                kind: PendingKind::Data,
                path: PathBuf::from("/d/row")
            }]
        );
    }

    #[test]
    fn test8023_arbitrary_crashes_reach_states_durable_only_cannot_but_never_tear_a_rename() {
        let sim = Sim::new();
        sim.seed(p("/d/row"), b"old");
        sim.write(p("/d/row.tmp"), b"new").unwrap();
        sim.rename(p("/d/row.tmp"), p("/d/row")).unwrap();
        assert_eq!(
            sim.crash(CrashModel::DurableOnly).files(),
            tree(&[("/d/row", b"old")])
        );
        let mut seen = BTreeSet::new();
        for seed in 0..512 {
            let files = sim.crash(CrashModel::Subset(seed)).files();
            let row = files.get(Path::new("/d/row")).map(Vec::as_slice);
            let staged = files.contains_key(Path::new("/d/row.tmp"));
            match row {
                Some(b"old") => {}
                Some(_) => assert!(!staged, "seed {seed}: both names survived: {files:?}"),
                None => panic!("seed {seed}: the target vanished: {files:?}"),
            }
            seen.insert(files);
        }
        let zeros: &[u8] = &[0, 0, 0];
        let expected: BTreeSet<_> = [
            tree(&[("/d/row", b"old")]),
            tree(&[("/d/row", b"old"), ("/d/row.tmp", b"")]),
            tree(&[("/d/row", b"old"), ("/d/row.tmp", b"new")]),
            tree(&[("/d/row", b"old"), ("/d/row.tmp", zeros)]),
            // The rename persisted and the data did not: what delayed
            // allocation leaves when the data was never synced.
            tree(&[("/d/row", b"")]),
            tree(&[("/d/row", zeros)]),
            tree(&[("/d/row", b"new")]),
        ]
        .into();
        assert_eq!(seen, expected);
        assert_eq!(
            sim.crash(CrashModel::Subset(7)).files(),
            sim.crash(CrashModel::Subset(7)).files(),
            "a seed names one state"
        );
    }

    #[test]
    fn test8024_an_unsynced_write_persists_whole_absent_or_torn_at_block_boundaries() {
        let sim = Sim::new();
        sim.seed(p("/big"), b"");
        let bytes: Vec<u8> = (0..10_000u32).map(|i| (i % 251 + 1) as u8).collect();
        sim.write(p("/big"), &bytes).unwrap();
        let torn = |blocks: usize| {
            let mut torn = bytes[..blocks * BLOCK].to_vec();
            torn.resize(bytes.len(), 0);
            torn
        };
        let allowed: BTreeSet<Vec<u8>> = [vec![], bytes.clone(), torn(0), torn(1), torn(2)].into();
        let mut seen = BTreeSet::new();
        for seed in 0..256 {
            let content = sim.crash(CrashModel::Subset(seed)).files()[Path::new("/big")].clone();
            assert!(
                allowed.contains(&content),
                "seed {seed}: {} bytes",
                content.len()
            );
            seen.insert(content);
        }
        assert_eq!(seen, allowed);
        assert_eq!(
            sim.crash(CrashModel::Prefix(1)).files()[Path::new("/big")],
            bytes
        );
    }

    #[test]
    fn test8025_every_admissible_subset_can_be_enumerated() {
        let sim = replace_and_create();
        let kinds: Vec<PendingKind> = sim.pending().into_iter().map(|op| op.kind).collect();
        assert_eq!(
            kinds,
            [
                PendingKind::Create,
                PendingKind::Data,
                PendingKind::Rename {
                    to: PathBuf::from("/d/row")
                },
                PendingKind::Create,
                PendingKind::Data,
            ]
        );
        let count = sim.pending_count();
        let mut admissible = 0;
        let mut states = BTreeSet::new();
        for mask in 0..1u64 << count {
            if let Some(after) = sim.crash_subset(mask) {
                admissible += 1;
                states.insert(after.files());
            }
        }
        // The rename (bit 2) cannot persist without the creation of the file
        // it moves (bit 0): 8 of the 32 subsets are impossible.
        assert_eq!(admissible, 24);
        assert!(sim.crash_subset(0b00100).is_none());
        // row: old | old + staged ("" or "new") | replaced ("" or "new") = 5,
        // times other: absent | "" | "o" = 3.
        assert_eq!(states.len(), 15);
        assert_eq!(
            sim.crash_subset(0).unwrap().files(),
            sim.crash(CrashModel::DurableOnly).files()
        );
        assert_eq!(sim.crash_subset(0b11111).unwrap().files(), sim.files());
    }

    #[test]
    fn test8026_a_prefix_crash_persists_operations_in_issue_order() {
        let sim = replace_and_create();
        let expected = [
            tree(&[("/d/row", b"old")]),
            tree(&[("/d/row", b"old"), ("/d/row.tmp", b"")]),
            tree(&[("/d/row", b"old"), ("/d/row.tmp", b"new")]),
            tree(&[("/d/row", b"new")]),
            tree(&[("/d/other", b""), ("/d/row", b"new")]),
            tree(&[("/d/other", b"o"), ("/d/row", b"new")]),
        ];
        for (n, files) in expected.iter().enumerate() {
            assert_eq!(
                &sim.crash(CrashModel::Prefix(n)).files(),
                files,
                "Prefix({n})"
            );
        }
    }

    #[test]
    fn test8027_syncing_a_rename_target_persists_what_the_rename_depends_on() {
        let sim = Sim::new();
        sim.create_dir(p("/a")).unwrap();
        sim.create_dir(p("/b")).unwrap();
        sim.sync_dir(p("/")).unwrap();
        sim.create_synced(p("/a/tmp"), b"x", Full).unwrap();
        sim.write(p("/a/unrelated"), b"u").unwrap();
        sim.rename(p("/a/tmp"), p("/b/x")).unwrap();
        sim.sync_dir(p("/b")).unwrap();
        assert_eq!(
            sim.crash(CrashModel::DurableOnly).files(),
            tree(&[("/b/x", b"x")]),
            "the rename is atomic: its source's creation and removal persist with it"
        );
        let left: Vec<PendingKind> = sim.pending().into_iter().map(|op| op.kind).collect();
        assert_eq!(
            left,
            [PendingKind::Create, PendingKind::Data],
            "/a/unrelated"
        );
    }

    #[test]
    fn test8028_a_failed_sync_discards_the_dirty_data_for_good() {
        let sim = Sim::new();
        sim.seed(p("/f"), b"old");
        sim.write(p("/f"), b"new").unwrap();
        sim.fault_at(0, Fault::SyncFailsDropsData);
        let error = sim.sync_file(p("/f"), Full).unwrap_err();
        assert!(error.to_string().contains("EIO"), "{error}");
        assert_eq!(sim.read(p("/f")).unwrap(), b"old", "the cache reverted");
        sim.sync_file(p("/f"), Full).unwrap();
        assert_eq!(sim.pending_count(), 0, "nothing was left to write back");
        for model in [CrashModel::DurableOnly, CrashModel::Subset(3)] {
            assert_eq!(sim.crash(model).files(), tree(&[("/f", b"old")]));
        }
        // Rewriting after the failure, as callers must, is durable again.
        sim.write(p("/f"), b"new").unwrap();
        sim.sync_file(p("/f"), Full).unwrap();
        assert_eq!(
            sim.crash(CrashModel::DurableOnly).files(),
            tree(&[("/f", b"new")])
        );
        // A failing create_synced leaves the name with nothing behind it.
        sim.fault_at(0, Fault::SyncFailsDropsData);
        assert!(sim.create_synced(p("/g"), b"data", Full).is_err());
        assert_eq!(sim.read(p("/g")).unwrap(), b"");
        // At an operation that syncs no data it is an EIO with no effect.
        sim.fault_at(0, Fault::SyncFailsDropsData);
        assert!(sim.write(p("/h"), b"h").is_err());
        assert!(sim.metadata(p("/h")).is_err());
    }

    #[test]
    fn test8029_injected_faults_fail_crash_or_tear_the_chosen_operation() {
        let sim = Sim::new();
        sim.create_dir(p("/d")).unwrap();
        sim.fault_at(1, Fault::Error(io::ErrorKind::StorageFull));
        sim.write(p("/d/a"), b"1").unwrap();
        assert_eq!(
            sim.write(p("/d/b"), b"2").unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
        assert!(sim.metadata(p("/d/b")).is_err(), "an error has no effect");
        sim.fault_at(0, Fault::Crash);
        assert!(sim.write(p("/d/c"), b"3").is_err());
        assert!(!sim.is_alive());
        assert!(
            sim.write(p("/d/d"), b"4").is_err(),
            "a dead process does nothing"
        );
        assert!(sim.read(p("/d/a")).is_err(), "not even read");
        let restarted = sim.process(1);
        assert_eq!(
            restarted.read(p("/d/a")).unwrap(),
            b"1",
            "the cache survived"
        );
        assert!(
            restarted.metadata(p("/d/c")).is_err(),
            "the crash came first"
        );

        let torn = Sim::new();
        torn.seed(p("/t"), b"old!");
        torn.fault_at(0, Fault::TornWrite);
        assert!(torn.write(p("/t"), b"abcd").is_err());
        assert!(!torn.is_alive(), "a torn write is a crash");
        assert_eq!(torn.process(1).read(p("/t")).unwrap(), b"ab");
        assert_eq!(
            torn.crash(CrashModel::DurableOnly).files(),
            tree(&[("/t", b"old!")])
        );
        assert_eq!(
            torn.crash(CrashModel::Prefix(1)).files(),
            tree(&[("/t", b"ab")])
        );

        let elsewhere = Sim::new();
        elsewhere.seed(p("/x"), b"x");
        elsewhere.fault_at(0, Fault::TornWrite);
        assert!(elsewhere.rename(p("/x"), p("/y")).is_err());
        assert!(!elsewhere.is_alive(), "nothing to tear: a plain crash");
        assert_eq!(elsewhere.process(1).files(), tree(&[("/x", b"x")]));
    }

    #[test]
    fn test8030_locks_exclude_every_other_holder_and_die_with_their_process() {
        let sim = Sim::new();
        sim.create_dir(p("/db")).unwrap();
        let lock = p("/db/lock");
        let first = sim.process(1);
        let second = sim.process(2);
        let stale = first.try_lock(lock).unwrap().expect("uncontended");
        assert!(second.try_lock(lock).unwrap().is_none());
        assert!(
            first.try_lock(lock).unwrap().is_none(),
            "a second attempt from the same process is another holder"
        );
        first.kill();
        let taken = second
            .try_lock(lock)
            .unwrap()
            .expect("the dead holder released it");
        drop(stale);
        assert!(
            sim.process(3).try_lock(lock).unwrap().is_none(),
            "dropping a dead process's guard must not release the new holder's lock"
        );
        drop(taken);
        assert!(sim.process(3).try_lock(lock).unwrap().is_some());
        assert!(first.try_lock(lock).is_err(), "a dead process cannot lock");
    }

    #[test]
    fn test8031_windows_mode_ignores_case_and_journals_entries_but_not_data() {
        let posix = Sim::new();
        posix.write(p("/x"), b"lower").unwrap();
        posix.write(p("/X"), b"upper").unwrap();
        assert_eq!(
            names(&posix, "/"),
            ["X", "x"],
            "POSIX names are case-sensitive"
        );

        let sim = Sim::new();
        sim.windows_mode(true);
        sim.create_dir(p("/D")).unwrap();
        sim.write(p("/D/Row.json"), b"v1").unwrap();
        assert_eq!(sim.read(p("/d/row.JSON")).unwrap(), b"v1");
        assert_eq!(
            sim.create_new(p("/d/ROW.json"), b"x").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        sim.write(p("/d/ROW.JSON"), b"v2").unwrap();
        assert_eq!(names(&sim, "/D"), ["Row.json"], "the stored case is kept");
        assert_eq!(
            sim.crash(CrashModel::DurableOnly).files(),
            tree(&[("/D/Row.json", b"")]),
            "names are durable at once; data still needs a sync"
        );
        let before = sim.pending_count();
        sim.sync_dir(p("/d")).unwrap();
        assert_eq!(sim.pending_count(), before, "sync_dir is a no-op");
        sim.sync_file(p("/d/row.json"), Full).unwrap();
        assert_eq!(
            sim.crash(CrashModel::DurableOnly).files(),
            tree(&[("/D/Row.json", b"v2")])
        );
        sim.rename(p("/D/Row.json"), p("/D/ROW.json")).unwrap();
        assert_eq!(
            names(&sim, "/D"),
            ["ROW.json"],
            "a rename can change only case"
        );
        assert_eq!(
            sim.crash(CrashModel::DurableOnly).files(),
            tree(&[("/D/ROW.json", b"v2")])
        );
    }

    #[test]
    fn test8032_a_file_held_open_cannot_be_replaced_or_removed_on_windows() {
        let sim = Sim::new();
        sim.seed(p("/d/row"), b"old");
        sim.windows_mode(true);
        sim.write(p("/d/tmp"), b"new").unwrap();
        let reader = sim.process(7);
        reader.hold_open(p("/d/row")).unwrap();
        for error in [
            sim.rename(p("/d/tmp"), p("/d/row")).unwrap_err(),
            sim.remove_file(p("/d/row")).unwrap_err(),
            sim.rename(p("/d/row"), p("/d/moved")).unwrap_err(),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(error.to_string().contains("sharing violation"), "{error}");
        }
        assert_eq!(sim.read(p("/d/row")).unwrap(), b"old");
        reader.release(p("/d/row")).unwrap();
        sim.rename(p("/d/tmp"), p("/d/row")).unwrap();
        reader.hold_open(p("/d/row")).unwrap();
        reader.kill();
        sim.remove_file(p("/d/row")).unwrap();

        let posix = Sim::new();
        posix.seed(p("/d/row"), b"old");
        posix.write(p("/d/tmp"), b"new").unwrap();
        posix.process(7).hold_open(p("/d/row")).unwrap();
        posix.rename(p("/d/tmp"), p("/d/row")).unwrap();
        assert_eq!(
            posix.read(p("/d/row")).unwrap(),
            b"new",
            "POSIX renames over open files"
        );
    }

    #[test]
    fn test8033_names_longer_than_the_limit_are_refused() {
        let sim = Sim::new();
        sim.set_name_max(10);
        sim.write(p("/0123456789"), b"").unwrap();
        let long = p("/0123456789a");
        let refused = [
            sim.write(long, b"").unwrap_err(),
            sim.create_dir(long).unwrap_err(),
            sim.rename(p("/0123456789"), long).unwrap_err(),
            sim.metadata(long).unwrap_err(),
        ];
        for error in refused {
            assert_eq!(error.kind(), io::ErrorKind::InvalidFilename);
        }
        assert!(
            sim.metadata(p("/0123456789")).is_ok(),
            "the failed rename did nothing"
        );
        // Six "é" are 12 bytes but 6 UTF-16 units.
        let accented = p("/éééééé");
        assert_eq!(
            sim.write(accented, b"").unwrap_err().kind(),
            io::ErrorKind::InvalidFilename
        );
        let windows = Sim::new();
        windows.windows_mode(true);
        windows.set_name_max(10);
        windows.write(accented, b"").unwrap();
    }

    #[test]
    fn test8034_a_full_volume_writes_what_fits_then_refuses() {
        let sim = Sim::new();
        sim.set_space_limit(10);
        sim.write(p("/a"), b"12345678").unwrap();
        assert_eq!(
            sim.write(p("/b"), b"abcdefgh").unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
        assert_eq!(
            sim.read(p("/b")).unwrap(),
            b"ab",
            "the bytes that fit were written"
        );
        sim.write(p("/a"), b"ABCDEFGH").unwrap();
        assert_eq!(
            sim.write(p("/a"), b"ABCDEFGHI").unwrap_err().kind(),
            io::ErrorKind::StorageFull,
            "a rewrite is charged for what it adds beyond what it replaces"
        );
        sim.remove_file(p("/b")).unwrap();
        sim.write(p("/c"), b"cc").unwrap();
    }

    #[test]
    fn test8035_denied_subtrees_and_mount_boundaries_refuse_what_they_should() {
        let sim = Sim::new();
        sim.seed(p("/ro/f"), b"x");
        sim.seed(p("/g"), b"g");
        sim.seed(p("/mnt/f"), b"m");
        sim.deny_writes_under(p("/ro"));
        let denied = [
            sim.write(p("/ro/f"), b"y").unwrap_err(),
            sim.remove_file(p("/ro/f")).unwrap_err(),
            sim.rename(p("/ro/f"), p("/f")).unwrap_err(),
            sim.rename(p("/g"), p("/ro/g")).unwrap_err(),
            sim.create_dir(p("/ro/sub")).unwrap_err(),
            sim.sync_file(p("/ro/f"), Full).unwrap_err(),
            sim.try_lock(p("/ro/lock")).err().unwrap(),
        ];
        for error in denied {
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        }
        assert_eq!(sim.read(p("/ro/f")).unwrap(), b"x", "reads are allowed");
        sim.sync_dir(p("/ro")).unwrap();

        sim.mount_boundary(p("/mnt"));
        assert_eq!(
            sim.rename(p("/mnt/f"), p("/f")).unwrap_err().kind(),
            io::ErrorKind::CrossesDevices
        );
        assert_eq!(
            sim.rename(p("/g"), p("/mnt/g")).unwrap_err().kind(),
            io::ErrorKind::CrossesDevices
        );
        sim.rename(p("/mnt/f"), p("/mnt/h")).unwrap();
        let inside = sim.metadata(p("/mnt/h")).unwrap().stat.device;
        let outside = sim.metadata(p("/g")).unwrap().stat.device;
        assert_ne!(inside, outside);
        assert_eq!(sim.volume(p("/mnt")).unwrap().device, inside);
        assert_eq!(sim.volume(p("/")).unwrap().device, outside);
    }

    #[test]
    fn test8036_the_clock_inodes_and_times_behave_like_a_filesystem() {
        let sim = Sim::new();
        sim.set_mtime_granularity_ns(NANOS);
        assert_eq!(sim.mtime_granularity_ns(p("/")).unwrap(), NANOS);
        let first = sim.touch(p("/clock")).unwrap();
        let second = sim.touch(p("/clock")).unwrap();
        assert_eq!(first % NANOS, 0);
        assert_eq!(first, second, "two writes in one second share a timestamp");
        sim.advance_clock(NANOS);
        let third = sim.touch(p("/clock")).unwrap();
        assert_eq!(third, second + NANOS);
        assert_eq!(sim.metadata(p("/clock")).unwrap().stat.mtime_ns, third);
        assert_eq!(sim.read(p("/clock")).unwrap(), [TOUCH_BYTE]);

        sim.set_mtime_granularity_ns(1);
        sim.write(p("/a"), b"1").unwrap();
        let a = sim.metadata(p("/a")).unwrap().stat;
        sim.write(p("/a"), b"22").unwrap();
        let rewritten = sim.metadata(p("/a")).unwrap().stat;
        assert_eq!(rewritten.inode, a.inode, "a rewrite keeps the inode");
        assert!(rewritten.mtime_ns > a.mtime_ns);
        assert_eq!(rewritten.size, 2);
        sim.write(p("/b"), b"b").unwrap();
        let b = sim.metadata(p("/b")).unwrap().stat;
        assert_ne!(b.inode, a.inode);
        sim.rename(p("/b"), p("/c")).unwrap();
        let c = sim.metadata(p("/c")).unwrap().stat;
        assert_eq!(c.inode, b.inode, "a rename moves the file itself");
        assert_eq!(c.mtime_ns, b.mtime_ns);
        assert!(
            c.ctime_ns > b.ctime_ns,
            "a rename changes the inode's ctime"
        );
        sim.set_mtime(p("/c"), 42).unwrap();
        assert_eq!(sim.metadata(p("/c")).unwrap().stat.mtime_ns, 42);
    }

    #[test]
    fn test8037_counters_attribute_each_call_to_its_kind() {
        let sim = Sim::new();
        sim.create_synced(p("/a"), b"abc", Durability::Barrier)
            .unwrap();
        sim.read(p("/a")).unwrap();
        sim.read_at(p("/a"), 1, 1).unwrap();
        sim.metadata(p("/a")).unwrap();
        sim.read_dir(p("/")).unwrap();
        sim.sync_dir(p("/")).unwrap();
        sim.rename(p("/a"), p("/b")).unwrap();
        assert_eq!(
            sim.counters(),
            Counters {
                stats: 1,
                read_dirs: 1,
                reads: 2,
                bytes_read: 4,
                writes: 1,
                bytes_written: 3,
                syncs_file: 1,
                barriers: 1,
                syncs_dir: 1,
                renames: 1,
                ..Counters::default()
            }
        );
        sim.reset_counters();
        assert_eq!(sim.counters(), Counters::default());
    }

    #[test]
    fn test8038_directory_and_file_operations_refuse_what_posix_refuses() {
        let sim = Sim::new();
        sim.seed(p("/d/f"), b"0123");
        sim.seed(p("/e/g"), b"");
        sim.create_dir(p("/empty")).unwrap();
        let kind = |result: io::Result<()>| result.unwrap_err().kind();
        assert_eq!(
            kind(sim.remove_dir(p("/d"))),
            io::ErrorKind::DirectoryNotEmpty
        );
        assert_eq!(kind(sim.remove_file(p("/d"))), io::ErrorKind::IsADirectory);
        assert_eq!(
            kind(sim.remove_dir(p("/d/f"))),
            io::ErrorKind::NotADirectory
        );
        assert_eq!(kind(sim.create_dir(p("/d"))), io::ErrorKind::AlreadyExists);
        assert_eq!(kind(sim.create_dir(p("/no/sub"))), io::ErrorKind::NotFound);
        assert_eq!(
            kind(sim.write(p("/d/f/x"), b"")),
            io::ErrorKind::NotADirectory
        );
        assert_eq!(
            kind(sim.rename(p("/d"), p("/e"))),
            io::ErrorKind::DirectoryNotEmpty
        );
        assert_eq!(
            kind(sim.rename(p("/d/f"), p("/empty"))),
            io::ErrorKind::IsADirectory
        );
        assert_eq!(
            kind(sim.rename(p("/d"), p("/d/inner"))),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            kind(sim.write(p("relative"), b"")),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(kind(sim.remove_dir(p("/"))), io::ErrorKind::ResourceBusy);
        assert_eq!(
            sim.read_at(p("/d/f"), 2, 3).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(sim.read_at(p("/d/f"), 1, 2).unwrap(), b"12");
        sim.rename(p("/d"), p("/empty")).unwrap();
        assert_eq!(
            sim.read(p("/empty/f")).unwrap(),
            b"0123",
            "a directory replaced an empty one"
        );
        let root = sim.read_dir(p("/")).unwrap();
        let listed: Vec<_> = root.iter().map(|entry| entry.path.clone()).collect();
        assert_eq!(listed, [PathBuf::from("/e"), PathBuf::from("/empty")]);
        assert_eq!(root[0].meta.kind, Kind::Dir);
        assert_eq!(root[0].meta, sim.metadata(p("/e")).unwrap());
        assert_eq!(sim.metadata(p("/")).unwrap().links, 4, "two subdirectories");
    }

    #[test]
    fn test8039_a_crash_keeps_the_environment_and_forgets_the_running_system() {
        let sim = Sim::new();
        sim.set_name_max(20);
        sim.write(p("/a"), b"a").unwrap();
        let a = sim.metadata(p("/a")).unwrap().stat.inode;
        let _held = sim.try_lock(p("/lock")).unwrap().unwrap();
        sim.fault_at(2, Fault::Crash);
        let after = sim.crash(CrashModel::Subset(11));
        assert!(
            after.try_lock(p("/lock")).unwrap().is_some(),
            "locks do not survive"
        );
        for index in 0..4 {
            after.write(p(&format!("/f{index}")), b"").unwrap();
        }
        assert_eq!(
            after
                .write(p("/123456789012345678901"), b"")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidFilename
        );
        let fresh = after.metadata(p("/f0")).unwrap().stat.inode;
        assert!(fresh > a, "inode numbers are never reused");
        assert_eq!(after.counters().writes, 5);
    }
}
