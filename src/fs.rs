//! The filesystem, as reldir reads and writes it.
//!
//! Everything that observes the governed directory reads through [`Source`],
//! and everything that commits a transaction writes through [`Fs`]. Three
//! implementations exist, each for one purpose:
//!
//! - [`Disk`], the real filesystem;
//! - [`Overlay`], the real filesystem with a planned set of changes laid over
//!   it, which is how a mutation is validated *before* it is written -- no
//!   copy of the database is made anywhere;
//! - [`Sim`], an in-memory filesystem that models what survives a crash
//!   (only fsynced data and fsynced directory entries) and can fail any
//!   operation on demand. The transaction protocol is tested against it at
//!   every point a crash can land.

use crate::mirror::Stat;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// What kind of object a path names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

/// The facts reldir needs about a path, without following a symlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub kind: Kind,
    pub len: u64,
    /// Hard-link count; more than one means the file is shared with a path
    /// the database does not govern.
    pub links: u64,
    pub stat: Stat,
}

/// Read access to a tree.
pub trait Source {
    /// Metadata of a path, not following symlinks.
    fn metadata(&self, path: &Path) -> io::Result<Meta>;
    /// The entries of a directory, as full paths, sorted.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
}

/// Write access, with the durability primitives a crash-safe protocol needs.
pub trait Fs: Source {
    /// Create or replace a file's contents. Not durable until [`Fs::sync_file`].
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Make a file's contents durable.
    fn sync_file(&self, path: &Path) -> io::Result<()>;
    /// Atomically replace `to` with `from`. Not durable until the parent
    /// directory is synced.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;
    /// Make a directory's entries -- creations, renames, removals -- durable.
    fn sync_dir(&self, path: &Path) -> io::Result<()>;
}

/// The real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct Disk;

impl Source for Disk {
    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        let metadata = std::fs::symlink_metadata(path)?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            Kind::Symlink
        } else if file_type.is_file() {
            Kind::File
        } else if file_type.is_dir() {
            Kind::Dir
        } else {
            Kind::Other
        };
        #[cfg(unix)]
        let links = {
            use std::os::unix::fs::MetadataExt;
            metadata.nlink()
        };
        #[cfg(not(unix))]
        let links = 1;
        Ok(Meta {
            kind,
            len: metadata.len(),
            links,
            stat: Stat::of(&metadata),
        })
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let mut entries = vec![];
        for entry in std::fs::read_dir(path)? {
            entries.push(entry?.path());
        }
        entries.sort();
        Ok(entries)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
}

impl Fs for Disk {
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }
    fn sync_file(&self, path: &Path) -> io::Result<()> {
        std::fs::OpenOptions::new().read(true).open(path)?.sync_all()
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }
    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(path)
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::fs::File::open(path)?.sync_all()
        }
        // Windows offers no directory handle to flush; renames are journaled
        // by NTFS itself.
        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(())
        }
    }
}

/// Make every file written under `directory`, and the directory's entries,
/// durable at once. On Linux one `syncfs` flushes the whole filesystem, which
/// costs one flush however many files were written; elsewhere each file and
/// then the directory is synced.
pub fn flush_everything_under(directory: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let handle = std::fs::File::open(directory)?;
        // SAFETY: `handle` is an open descriptor for the duration of the call.
        if unsafe { libc::syncfs(handle.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            if path.is_file() {
                Disk.sync_file(&path)?;
            }
        }
        Disk.sync_dir(directory)
    }
}

/// A tree seen through a set of planned changes.
///
/// Planned writes appear as files with fresh metadata, so no cache can mistake
/// one for the file it replaces; planned deletions vanish; directories that
/// only planned writes create appear too.
pub struct Overlay<'a> {
    base: &'a dyn Source,
    writes: BTreeMap<PathBuf, Vec<u8>>,
    deletes: BTreeSet<PathBuf>,
}

impl<'a> Overlay<'a> {
    /// `writes` and `deletes` are absolute paths.
    pub fn new(base: &'a dyn Source, writes: BTreeMap<PathBuf, Vec<u8>>, deletes: BTreeSet<PathBuf>) -> Self {
        Self { base, writes, deletes }
    }

    fn created_dir(&self, path: &Path) -> bool {
        self.writes.keys().any(|written| written.starts_with(path) && written != path)
    }
}

impl Source for Overlay<'_> {
    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        if let Some(bytes) = self.writes.get(path) {
            return Ok(Meta {
                kind: Kind::File,
                len: bytes.len() as u64,
                links: 1,
                stat: Stat {
                    size: bytes.len() as u64,
                    ..Stat::default()
                },
            });
        }
        if self.deletes.contains(path) {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        match self.base.metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound && self.created_dir(path) => Ok(Meta {
                kind: Kind::Dir,
                len: 0,
                links: 1,
                stat: Stat::default(),
            }),
            other => other,
        }
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let mut entries: BTreeSet<PathBuf> = match self.base.read_dir(path) {
            Ok(entries) => entries.into_iter().collect(),
            Err(error) if error.kind() == io::ErrorKind::NotFound && self.created_dir(path) => BTreeSet::new(),
            Err(error) => return Err(error),
        };
        entries.retain(|entry| !self.deletes.contains(entry));
        for written in self.writes.keys() {
            if let Ok(rest) = written.strip_prefix(path)
                && let Some(first) = rest.components().next()
            {
                entries.insert(path.join(first));
            }
        }
        Ok(entries.into_iter().collect())
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        if let Some(bytes) = self.writes.get(path) {
            return Ok(bytes.clone());
        }
        if self.deletes.contains(path) {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        self.base.read(path)
    }
}

/// An operation [`Sim`] can be told to fail or to crash at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Write,
    SyncFile,
    Rename,
    RemoveFile,
    CreateDir,
    RemoveDir,
    SyncDir,
}

/// A fault [`Sim`] injects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The process dies before the operation takes effect. Every later
    /// operation fails as well, and [`Sim::crashed`] yields the state a
    /// reboot would find.
    Crash,
    /// The operation fails with an I/O error and has no effect.
    Error(io::ErrorKind),
    /// The process dies partway through a write: the first half of the bytes
    /// reach the file -- and the disk, wherever the file's name is already
    /// durable -- and nothing more happens.
    Torn,
}

#[derive(Debug, Clone, Default)]
struct SimTree {
    /// Directory → entry name → node, as currently visible.
    files: BTreeMap<PathBuf, Vec<u8>>,
    dirs: BTreeSet<PathBuf>,
}

#[derive(Debug, Default)]
struct SimState {
    /// What a process sees now.
    live: SimTree,
    /// What survives a crash: file contents as last synced, and directory
    /// entries as of each directory's last sync.
    durable: SimTree,
    /// Contents written but not yet synced, by path.
    unsynced: BTreeSet<PathBuf>,
    /// Operations performed so far.
    performed: usize,
    /// Fault to inject at the n-th operation (0-based).
    fault: Option<(usize, Fault)>,
    dead: bool,
}

/// A deterministic in-memory filesystem that models durability.
///
/// A file's contents survive a crash only once synced, and a directory entry
/// (creation, rename, removal) only once its parent directory is synced -- the
/// POSIX contract a crash-safe protocol must be written against, and stricter
/// than any real filesystem, so a protocol correct here is correct there.
#[derive(Debug, Clone, Default)]
pub struct Sim {
    state: Arc<Mutex<SimState>>,
}

impl Sim {
    pub fn new() -> Self {
        let sim = Self::default();
        {
            let mut state = sim.state.lock().expect("sim lock");
            state.live.dirs.insert(PathBuf::from("/"));
            state.durable.dirs.insert(PathBuf::from("/"));
        }
        sim
    }

    /// Seed a file that is already durable.
    pub fn seed(&self, path: &Path, bytes: &[u8]) {
        let mut state = self.state.lock().expect("sim lock");
        let mut ancestor = path.parent();
        while let Some(dir) = ancestor {
            state.live.dirs.insert(dir.to_path_buf());
            state.durable.dirs.insert(dir.to_path_buf());
            ancestor = dir.parent();
        }
        state.live.files.insert(path.to_path_buf(), bytes.to_vec());
        state.durable.files.insert(path.to_path_buf(), bytes.to_vec());
    }

    /// Inject a fault at the `index`-th operation from now.
    pub fn fault_at(&self, index: usize, fault: Fault) {
        let mut state = self.state.lock().expect("sim lock");
        let at = state.performed + index;
        state.fault = Some((at, fault));
    }

    /// How many operations have been performed.
    pub fn operations(&self) -> usize {
        self.state.lock().expect("sim lock").performed
    }

    /// The filesystem a reboot after a crash would find: only what was made
    /// durable.
    pub fn crashed(&self) -> Sim {
        let state = self.state.lock().expect("sim lock");
        let durable = state.durable.clone();
        Sim {
            state: Arc::new(Mutex::new(SimState {
                live: durable.clone(),
                durable,
                ..SimState::default()
            })),
        }
    }

    /// Every file's current contents.
    pub fn files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        self.state.lock().expect("sim lock").live.files.clone()
    }

    fn step(&self, op: Op) -> io::Result<Option<Fault>> {
        let mut state = self.state.lock().expect("sim lock");
        if state.dead {
            return Err(io::Error::other("the simulated process has crashed"));
        }
        let index = state.performed;
        state.performed += 1;
        match state.fault {
            Some((at, Fault::Torn)) if at == index && op == Op::Write => Ok(Some(Fault::Torn)),
            // Only a write can tear; anywhere else the same crash leaves
            // nothing half-done.
            Some((at, Fault::Crash | Fault::Torn)) if at == index => {
                state.dead = true;
                Err(io::Error::other("simulated crash"))
            }
            Some((at, Fault::Error(kind))) if at == index => Err(io::Error::from(kind)),
            _ => Ok(None),
        }
    }
}

impl Source for Sim {
    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        let state = self.state.lock().expect("sim lock");
        if let Some(bytes) = state.live.files.get(path) {
            return Ok(Meta {
                kind: Kind::File,
                len: bytes.len() as u64,
                links: 1,
                stat: Stat {
                    size: bytes.len() as u64,
                    ..Stat::default()
                },
            });
        }
        if state.live.dirs.contains(path) {
            return Ok(Meta {
                kind: Kind::Dir,
                len: 0,
                links: 1,
                stat: Stat::default(),
            });
        }
        Err(io::Error::from(io::ErrorKind::NotFound))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let state = self.state.lock().expect("sim lock");
        if !state.live.dirs.contains(path) {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        let mut out: BTreeSet<PathBuf> = BTreeSet::new();
        for candidate in state.live.files.keys().chain(state.live.dirs.iter()) {
            if candidate.parent() == Some(path) {
                out.insert(candidate.clone());
            }
        }
        Ok(out.into_iter().collect())
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let state = self.state.lock().expect("sim lock");
        state
            .live
            .files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
}

impl Fs for Sim {
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let fault = self.step(Op::Write)?;
        let mut state = self.state.lock().expect("sim lock");
        let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        if !state.live.dirs.contains(&parent) {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        if fault == Some(Fault::Torn) {
            let half = bytes[..bytes.len() / 2].to_vec();
            state.live.files.insert(path.to_path_buf(), half.clone());
            if state.durable.files.contains_key(path) {
                state.durable.files.insert(path.to_path_buf(), half);
            }
            state.dead = true;
            return Err(io::Error::other("simulated crash during a write"));
        }
        state.live.files.insert(path.to_path_buf(), bytes.to_vec());
        state.unsynced.insert(path.to_path_buf());
        Ok(())
    }

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        self.step(Op::SyncFile)?;
        let mut state = self.state.lock().expect("sim lock");
        let bytes = state
            .live
            .files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        state.unsynced.remove(path);
        // The contents are durable; whether the *name* is durable depends on
        // the directory entry, which only a directory sync publishes. Model it
        // by updating durable contents wherever the name is already durable,
        // and remembering the contents for when the entry becomes durable.
        if state.durable.files.contains_key(path) {
            state.durable.files.insert(path.to_path_buf(), bytes);
        }
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.step(Op::Rename)?;
        let mut state = self.state.lock().expect("sim lock");
        let bytes = state
            .live
            .files
            .remove(from)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        state.live.files.insert(to.to_path_buf(), bytes);
        if state.unsynced.remove(from) {
            state.unsynced.insert(to.to_path_buf());
        }
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.step(Op::RemoveFile)?;
        let mut state = self.state.lock().expect("sim lock");
        state
            .live
            .files
            .remove(path)
            .map(|_| ())
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.step(Op::CreateDir)?;
        let mut state = self.state.lock().expect("sim lock");
        let mut ancestor = Some(path);
        while let Some(dir) = ancestor {
            state.live.dirs.insert(dir.to_path_buf());
            ancestor = dir.parent();
        }
        Ok(())
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.step(Op::RemoveDir)?;
        let mut state = self.state.lock().expect("sim lock");
        state.live.files.retain(|file, _| !file.starts_with(path));
        state.live.dirs.retain(|dir| !dir.starts_with(path));
        state.unsynced.retain(|file| !file.starts_with(path));
        Ok(())
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.step(Op::SyncDir)?;
        let mut state = self.state.lock().expect("sim lock");
        // Publish this directory's entries: every live child becomes durable
        // with its synced contents (unsynced contents persist as empty -- the
        // name survives, the data does not), every durable child no longer
        // live is removed.
        let live_children: Vec<(PathBuf, Vec<u8>)> = state
            .live
            .files
            .iter()
            .filter(|(file, _)| file.parent() == Some(path))
            .map(|(file, bytes)| (file.clone(), bytes.clone()))
            .collect();
        let live_dirs: Vec<PathBuf> = state
            .live
            .dirs
            .iter()
            .filter(|dir| dir.parent() == Some(path))
            .cloned()
            .collect();
        state.durable.files.retain(|file, _| file.parent() != Some(path));
        state.durable.dirs.retain(|dir| dir.parent() != Some(path) || live_dirs.contains(dir));
        for (file, bytes) in live_children {
            let persisted = if state.unsynced.contains(&file) { vec![] } else { bytes };
            state.durable.files.insert(file, persisted);
        }
        for dir in live_dirs {
            state.durable.dirs.insert(dir);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test2090_the_overlay_shows_planned_changes_over_the_base() {
        let sim = Sim::new();
        sim.seed(Path::new("/db/t/a.json"), b"A");
        sim.seed(Path::new("/db/t/b.json"), b"B");
        let overlay = Overlay::new(
            &sim,
            BTreeMap::from([
                (PathBuf::from("/db/t/c.json"), b"C".to_vec()),
                (PathBuf::from("/db/u/x.json"), b"X".to_vec()),
            ]),
            BTreeSet::from([PathBuf::from("/db/t/a.json")]),
        );
        assert_eq!(
            overlay.read_dir(Path::new("/db/t")).unwrap(),
            vec![PathBuf::from("/db/t/b.json"), PathBuf::from("/db/t/c.json")]
        );
        assert_eq!(overlay.metadata(Path::new("/db/u")).unwrap().kind, Kind::Dir);
        assert_eq!(overlay.read(Path::new("/db/u/x.json")).unwrap(), b"X");
        assert!(overlay.read(Path::new("/db/t/a.json")).is_err());
        assert_eq!(overlay.read(Path::new("/db/t/b.json")).unwrap(), b"B");
    }

    #[test]
    fn test2091_only_synced_contents_under_synced_entries_survive_a_crash() {
        let sim = Sim::new();
        sim.create_dir_all(Path::new("/d")).unwrap();
        sim.sync_dir(Path::new("/")).unwrap();
        sim.write(Path::new("/d/unsynced"), b"lost").unwrap();
        sim.write(Path::new("/d/synced"), b"kept").unwrap();
        sim.sync_file(Path::new("/d/synced")).unwrap();
        // Neither name is durable until the directory is synced.
        assert!(sim.crashed().files().is_empty());
        sim.sync_dir(Path::new("/d")).unwrap();
        let after = sim.crashed().files();
        assert_eq!(after.get(Path::new("/d/synced")).map(Vec::as_slice), Some(&b"kept"[..]));
        assert_eq!(
            after.get(Path::new("/d/unsynced")).map(Vec::as_slice),
            Some(&b""[..]),
            "the name survived; the unsynced data did not"
        );
    }

    #[test]
    fn test2092_a_rename_is_durable_only_once_its_directory_is_synced() {
        let sim = Sim::new();
        sim.seed(Path::new("/d/row"), b"old");
        sim.write(Path::new("/d/row.tmp"), b"new").unwrap();
        sim.sync_file(Path::new("/d/row.tmp")).unwrap();
        sim.rename(Path::new("/d/row.tmp"), Path::new("/d/row")).unwrap();
        assert_eq!(sim.crashed().files()[Path::new("/d/row")], b"old");
        sim.sync_dir(Path::new("/d")).unwrap();
        assert_eq!(sim.crashed().files()[Path::new("/d/row")], b"new");
    }

    #[test]
    fn test2093_injected_faults_fail_or_crash_at_the_chosen_operation() {
        let sim = Sim::new();
        sim.create_dir_all(Path::new("/d")).unwrap();
        sim.fault_at(1, Fault::Error(io::ErrorKind::StorageFull));
        sim.write(Path::new("/d/a"), b"1").unwrap();
        assert_eq!(sim.write(Path::new("/d/b"), b"2").unwrap_err().kind(), io::ErrorKind::StorageFull);
        sim.fault_at(0, Fault::Crash);
        assert!(sim.write(Path::new("/d/c"), b"3").is_err());
        assert!(sim.write(Path::new("/d/d"), b"4").is_err(), "a crashed process does nothing more");
        let torn = Sim::new();
        torn.seed(Path::new("/d/t"), b"old!");
        torn.fault_at(0, Fault::Torn);
        assert!(torn.write(Path::new("/d/t"), b"abcd").is_err(), "a torn write is a crash");
        assert_eq!(torn.crashed().files()[Path::new("/d/t")], b"ab", "half of it reached the disk");
    }
}
