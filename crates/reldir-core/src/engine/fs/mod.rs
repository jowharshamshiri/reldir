//! The filesystem, as the engine reads and writes it.
//!
//! Everything that observes a database reads through [`Source`], everything
//! that changes one writes through [`Fs`], and the writer lock is taken
//! through [`Locks`]. Two implementations exist:
//!
//! - [`Disk`], the real filesystem, with each platform's own spelling of a
//!   durable sync and its own way of telling whether a filesystem can be
//!   trusted;
//! - `Sim` (tests only), a deterministic in-memory filesystem that models what survives
//!   a power loss -- not only the synced state, but any state the POSIX
//!   contract permits -- and that can fail, tear or crash any operation on
//!   demand. The commit protocol is proven against it.
//!
//! Rules every implementation and every caller keeps:
//!
//! - Every byte reldir writes under the database root goes through [`Fs`].
//! - Paths are absolute. Implementations never follow symlinks for metadata.
//! - No method retries an fsync. A failed `sync_*` is reported once; callers
//!   treat dirty data as lost (fsyncgate) and rewrite before syncing again.

pub mod disk;
pub mod overlay;
/// The simulator exists for tests: no release build carries it.
#[cfg(any(test, feature = "testing"))]
pub mod sim;

pub use disk::Disk;
pub use overlay::Overlay;
#[cfg(any(test, feature = "testing"))]
pub use sim::Sim;

use std::{
    io,
    path::{Path, PathBuf},
};

/// What kind of object a path names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

/// Filesystem facts that decide whether a file changed without reading it.
/// Times are the *filesystem's* clock, nanoseconds since the Unix epoch.
/// On Windows `ctime_ns` is ChangeTime and `inode` the low 64 bits of the
/// 128-bit FileId; `device` is the volume serial number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Stat {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub inode: u64,
    pub device: u64,
}

/// Metadata of one path, not following symlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub kind: Kind,
    /// Hard-link count (Windows: NumberOfLinks).
    pub links: u64,
    pub stat: Stat,
}

/// One directory entry with its metadata. Implementations that can obtain
/// metadata during enumeration (Windows FileIdExtdDirectoryInfo) do so;
/// others stat each entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub meta: Meta,
}

/// How strongly a sync must hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Ordering: everything written before is durable before anything written
    /// after it is (macOS F_BARRIERFSYNC; elsewhere identical to `Full`).
    Barrier,
    /// The data is on stable storage when the call returns (fdatasync,
    /// F_FULLFSYNC, FlushFileBuffers on a write handle).
    Full,
}

/// Facts about the filesystem holding a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// Longest file-name component in bytes (Windows: UTF-16 units).
    pub name_max: u64,
    /// Identity of the volume; two paths on different devices cannot be
    /// renamed into one another.
    pub device: u64,
    /// The filesystem's name as the OS reports it (`ext4`, `apfs`, `NTFS`,
    /// `nfs`, ...), lowercase.
    pub kind: String,
    /// Whether this filesystem keeps the two promises the commit protocol
    /// needs (exclusive advisory locks, atomic rename replacing the target).
    pub class: VolumeClass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeClass {
    Local,
    /// Network, FUSE, sync-client or FAT-family: unsafe for writes unless
    /// configured otherwise.
    Unsafe(String),
}

/// Read access to a tree.
pub trait Source: Send + Sync {
    fn metadata(&self, path: &Path) -> io::Result<Meta>;
    /// Entries of a directory, sorted by file name (byte order of the OS name).
    fn read_dir(&self, path: &Path) -> io::Result<Vec<Entry>>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// `len` bytes at `offset`; an error if the file is shorter.
    fn read_at(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>>;
}

/// Write access with the durability primitives a crash-safe protocol needs.
pub trait Fs: Source {
    /// Create a file that must not exist (O_CREAT|O_EXCL) with these bytes.
    /// Not durable until synced.
    fn create_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Create or truncate a file and write these bytes. Not durable until synced.
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Create a file that must not exist, write the bytes and sync them on the
    /// same (write) handle. One call, so Windows never syncs a read handle.
    fn create_synced(&self, path: &Path, bytes: &[u8], durability: Durability) -> io::Result<()>;
    /// Make an existing file's contents durable. Opens it for writing.
    fn sync_file(&self, path: &Path, durability: Durability) -> io::Result<()>;
    /// Make several existing files' contents durable: [`Fs::sync_file`]'s
    /// promise for each. An error means none of them is known durable. By
    /// default each is synced, several at once.
    fn sync_files(&self, paths: &[&Path], durability: Durability) -> io::Result<()> {
        let workers = paths.len().clamp(1, 16);
        let chunk = paths.len().div_ceil(workers).max(1);
        std::thread::scope(|scope| {
            let handles: Vec<_> = paths
                .chunks(chunk)
                .map(|slice| {
                    scope.spawn(move || -> io::Result<()> {
                        for path in slice {
                            self.sync_file(path, durability)?;
                        }
                        Ok(())
                    })
                })
                .collect();
            handles
                .into_iter()
                .try_for_each(|handle| handle.join().expect("a sync thread does not panic"))
        })
    }
    /// Make a directory's entries (creations, renames, removals) durable.
    /// Windows: a documented no-op (NTFS journals metadata; renames use
    /// MOVEFILE_WRITE_THROUGH).
    fn sync_dir(&self, path: &Path) -> io::Result<()>;
    /// Atomically replace `to` with `from` (same volume). Not durable until the
    /// parent directory is synced.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Create one directory; its parent must exist. AlreadyExists if present.
    fn create_dir(&self, path: &Path) -> io::Result<()>;
    /// Remove an empty directory.
    fn remove_dir(&self, path: &Path) -> io::Result<()>;
    /// Volume facts for the filesystem holding `path` (which must exist).
    fn volume(&self, path: &Path) -> io::Result<Volume>;
    /// Write one byte to `path` (creating it), and return the file's mtime as
    /// the filesystem records it: the filesystem's notion of "now", used to
    /// decide which cached stats are racy.
    fn touch(&self, path: &Path) -> io::Result<i64>;
    /// The filesystem timestamp granularity under `dir`, in nanoseconds,
    /// probed once (1 on ext4/xfs/btrfs/apfs, 100 on NTFS, 1e9 on HFS+,
    /// 2e9 on FAT).
    fn mtime_granularity_ns(&self, dir: &Path) -> io::Result<i64>;
}

/// Exclusive advisory lock management. `Disk` uses `std::fs::File::lock`;
/// `Sim` models a lock released when its simulated process dies.
pub trait Locks: Send + Sync {
    /// Try once. `Ok(None)` when another holder has it. Dropping the guard
    /// releases the lock.
    fn try_lock(&self, path: &Path) -> io::Result<Option<LockGuard>>;
}

/// Holds a lock until dropped.
pub struct LockGuard(pub Box<dyn std::any::Any + Send>);

impl std::fmt::Debug for LockGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LockGuard")
    }
}

/// Replace a file with exactly these bytes, durably: a synced sibling renamed
/// over it, then its directory synced. For reldir's own metadata, which no
/// other program reads mid-write.
pub fn replace_durably(fs: &dyn Fs, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a file needs a directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a file needs a name"))?;
    let temporary = parent.join(format!(
        ".{}.{}.new",
        name.to_string_lossy(),
        ulid::Ulid::new()
    ));
    fs.create_synced(&temporary, bytes, Durability::Full)?;
    if let Err(error) = fs.rename(&temporary, path) {
        let _ = fs.remove_file(&temporary);
        return Err(error);
    }
    fs.sync_dir(parent)
}

/// Create a directory if it is missing, and make its entry durable; refuse
/// anything but a real directory in its place.
pub fn ensure_dir(fs: &dyn Fs, path: &Path) -> io::Result<()> {
    match fs.metadata(path) {
        Ok(meta) if meta.kind == Kind::Dir => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a directory", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs.create_dir(path)?;
            match path.parent() {
                Some(parent) => fs.sync_dir(parent),
                None => Ok(()),
            }
        }
        Err(error) => Err(error),
    }
}
