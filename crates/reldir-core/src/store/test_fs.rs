//! Test-only filesystems: [`StdFs`], the [`Fs`] interface over `std::fs` in a
//! private temporary directory, and [`Faulty`], a wrapper that records every
//! mutating operation and can fail any one of them.

use crate::engine::fs::{Durability, Entry, Fs, Kind, Meta, Source, Stat, Volume, VolumeClass};
use std::{
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

/// `std::fs`, confined by convention to a temporary directory that is removed
/// when the value is dropped.
pub struct StdFs {
    dir: tempfile::TempDir,
}

impl StdFs {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("a temporary directory"),
        }
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }
}

fn meta_of(metadata: &std::fs::Metadata) -> Meta {
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
    Meta {
        kind,
        links: 1,
        stat: Stat {
            size: metadata.len(),
            ..Stat::default()
        },
    }
}

impl Source for StdFs {
    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        Ok(meta_of(&std::fs::symlink_metadata(path)?))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<Entry>> {
        let mut out = vec![];
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            out.push(Entry {
                path: entry.path(),
                meta: meta_of(&std::fs::symlink_metadata(entry.path())?),
            });
        }
        out.sort_by(|a, b| a.path.file_name().cmp(&b.path.file_name()));
        Ok(out)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn read_at(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut out = vec![0; len];
        file.read_exact(&mut out)?;
        Ok(out)
    }
}

impl Fs for StdFs {
    fn create_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?
            .write_all(bytes)
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }
    fn create_synced(&self, path: &Path, bytes: &[u8], _: Durability) -> io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()
    }
    fn sync_file(&self, path: &Path, _: Durability) -> io::Result<()> {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)?
            .sync_all()
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::File::open(path)?.sync_all()
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir(path)
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir(path)
    }
    fn volume(&self, _: &Path) -> io::Result<Volume> {
        Ok(Volume {
            name_max: 255,
            device: 0,
            kind: "test".into(),
            class: VolumeClass::Local,
        })
    }
    fn touch(&self, path: &Path) -> io::Result<i64> {
        std::fs::write(path, b"t")?;
        Ok(0)
    }
    fn mtime_granularity_ns(&self, _: &Path) -> io::Result<i64> {
        Ok(1)
    }
}

/// One mutating operation, as [`Faulty`] records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    CreateNew(PathBuf),
    Write(PathBuf),
    CreateSynced(PathBuf),
    SyncFile(PathBuf),
    SyncDir(PathBuf),
    Rename(PathBuf, PathBuf),
    RemoveFile(PathBuf),
    CreateDir(PathBuf),
    RemoveDir(PathBuf),
}

/// A filesystem that records its mutating operations and fails the one at a
/// chosen position (counting from zero), with no effect.
pub struct Faulty<'a> {
    inner: &'a dyn Fs,
    log: Mutex<Vec<Op>>,
    fail_at: Option<usize>,
}

impl<'a> Faulty<'a> {
    pub fn new(inner: &'a dyn Fs, fail_at: Option<usize>) -> Self {
        Self {
            inner,
            log: Mutex::new(vec![]),
            fail_at,
        }
    }

    pub fn log(&self) -> Vec<Op> {
        self.log.lock().unwrap().clone()
    }

    fn record(&self, op: Op) -> io::Result<()> {
        let mut log = self.log.lock().unwrap();
        let index = log.len();
        log.push(op);
        if Some(index) == self.fail_at {
            return Err(io::Error::other("injected fault"));
        }
        Ok(())
    }
}

impl Source for Faulty<'_> {
    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        self.inner.metadata(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<Entry>> {
        self.inner.read_dir(path)
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read(path)
    }
    fn read_at(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.inner.read_at(path, offset, len)
    }
}

impl Fs for Faulty<'_> {
    fn create_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.record(Op::CreateNew(path.into()))?;
        self.inner.create_new(path, bytes)
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.record(Op::Write(path.into()))?;
        self.inner.write(path, bytes)
    }
    fn create_synced(&self, path: &Path, bytes: &[u8], durability: Durability) -> io::Result<()> {
        self.record(Op::CreateSynced(path.into()))?;
        self.inner.create_synced(path, bytes, durability)
    }
    fn sync_file(&self, path: &Path, durability: Durability) -> io::Result<()> {
        self.record(Op::SyncFile(path.into()))?;
        self.inner.sync_file(path, durability)
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.record(Op::SyncDir(path.into()))?;
        self.inner.sync_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.record(Op::Rename(from.into(), to.into()))?;
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.record(Op::RemoveFile(path.into()))?;
        self.inner.remove_file(path)
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.record(Op::CreateDir(path.into()))?;
        self.inner.create_dir(path)
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.record(Op::RemoveDir(path.into()))?;
        self.inner.remove_dir(path)
    }
    fn volume(&self, path: &Path) -> io::Result<Volume> {
        self.inner.volume(path)
    }
    fn touch(&self, path: &Path) -> io::Result<i64> {
        self.inner.touch(path)
    }
    fn mtime_granularity_ns(&self, dir: &Path) -> io::Result<i64> {
        self.inner.mtime_granularity_ns(dir)
    }
}

/// An in-memory filesystem, for property tests that build many stores. It
/// models visibility only, not durability: syncs are accepted and do nothing.
#[derive(Default)]
pub struct MemFs {
    state: Mutex<MemState>,
}

#[derive(Default)]
struct MemState {
    files: std::collections::BTreeMap<PathBuf, Vec<u8>>,
    dirs: std::collections::BTreeSet<PathBuf>,
}

impl MemFs {
    /// A filesystem holding the given directories and their ancestors.
    pub fn with_dirs(dirs: &[&Path]) -> Self {
        let fs = Self::default();
        {
            let mut state = fs.state.lock().unwrap();
            for dir in dirs {
                for ancestor in dir.ancestors() {
                    state.dirs.insert(ancestor.to_path_buf());
                }
            }
        }
        fs
    }

    fn not_found() -> io::Error {
        io::Error::from(io::ErrorKind::NotFound)
    }

    fn parent_exists(state: &MemState, path: &Path) -> io::Result<()> {
        match path.parent() {
            Some(parent) if state.dirs.contains(parent) => Ok(()),
            _ => Err(Self::not_found()),
        }
    }
}

impl Source for MemFs {
    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        let state = self.state.lock().unwrap();
        if let Some(bytes) = state.files.get(path) {
            return Ok(Meta {
                kind: Kind::File,
                links: 1,
                stat: Stat {
                    size: bytes.len() as u64,
                    ..Stat::default()
                },
            });
        }
        if state.dirs.contains(path) {
            return Ok(Meta {
                kind: Kind::Dir,
                links: 1,
                stat: Stat::default(),
            });
        }
        Err(Self::not_found())
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<Entry>> {
        let children: Vec<PathBuf> = {
            let state = self.state.lock().unwrap();
            if !state.dirs.contains(path) {
                return Err(Self::not_found());
            }
            state
                .files
                .keys()
                .chain(state.dirs.iter())
                .filter(|p| p.parent() == Some(path))
                .cloned()
                .collect()
        };
        let mut out = vec![];
        for child in children {
            out.push(Entry {
                meta: self.metadata(&child)?,
                path: child,
            });
        }
        out.sort_by(|a, b| a.path.file_name().cmp(&b.path.file_name()));
        Ok(out)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let state = self.state.lock().unwrap();
        state.files.get(path).cloned().ok_or_else(Self::not_found)
    }

    fn read_at(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let bytes = self.read(path)?;
        let eof = || io::Error::from(io::ErrorKind::UnexpectedEof);
        let start = usize::try_from(offset).map_err(|_| eof())?;
        let end = start.checked_add(len).ok_or_else(eof)?;
        bytes.get(start..end).map(<[u8]>::to_vec).ok_or_else(eof)
    }
}

impl Fs for MemFs {
    fn create_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        Self::parent_exists(&state, path)?;
        if state.files.contains_key(path) || state.dirs.contains(path) {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        state.files.insert(path.into(), bytes.to_vec());
        Ok(())
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        Self::parent_exists(&state, path)?;
        state.files.insert(path.into(), bytes.to_vec());
        Ok(())
    }
    fn create_synced(&self, path: &Path, bytes: &[u8], _: Durability) -> io::Result<()> {
        self.create_new(path, bytes)
    }
    fn sync_file(&self, path: &Path, _: Durability) -> io::Result<()> {
        self.read(path).map(|_| ())
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        match self.state.lock().unwrap().dirs.contains(path) {
            true => Ok(()),
            false => Err(Self::not_found()),
        }
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        Self::parent_exists(&state, to)?;
        let bytes = state.files.remove(from).ok_or_else(Self::not_found)?;
        state.files.insert(to.into(), bytes);
        Ok(())
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state
            .files
            .remove(path)
            .map(|_| ())
            .ok_or_else(Self::not_found)
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        Self::parent_exists(&state, path)?;
        if state.files.contains_key(path) || state.dirs.contains(path) {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        state.dirs.insert(path.into());
        Ok(())
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state
            .files
            .keys()
            .chain(state.dirs.iter())
            .any(|p| p.parent() == Some(path))
        {
            return Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty));
        }
        match state.dirs.remove(path) {
            true => Ok(()),
            false => Err(Self::not_found()),
        }
    }
    fn volume(&self, _: &Path) -> io::Result<Volume> {
        Ok(Volume {
            name_max: 255,
            device: 0,
            kind: "memory".into(),
            class: VolumeClass::Local,
        })
    }
    fn touch(&self, path: &Path) -> io::Result<i64> {
        self.write(path, b"t")?;
        Ok(0)
    }
    fn mtime_granularity_ns(&self, _: &Path) -> io::Result<i64> {
        Ok(1)
    }
}
