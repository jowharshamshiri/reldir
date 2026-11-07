//! The real filesystem.
//!
//! Each platform spells a durable write differently, and the differences are
//! not cosmetic:
//!
//! - **Linux** makes data durable with `fdatasync` on the descriptor that
//!   wrote it, and directory entries with `fsync` on the directory. There is no
//!   cheaper ordering primitive, so [`Durability::Barrier`] is `Full`.
//! - **macOS** `fsync` only hands the data to the drive, whose cache may still
//!   lose it; `F_FULLFSYNC` flushes the cache too, and `F_BARRIERFSYNC` orders
//!   writes without waiting for the flush. A filesystem that does not
//!   implement the barrier answers `ENOTSUP` or `EINVAL`, and then the full
//!   flush is issued instead: a stronger sync satisfies a barrier by
//!   definition, so this is the barrier's documented meaning on such a
//!   filesystem, not a substitute for it. A failed *full* flush is never
//!   downgraded to a plain `fsync`.
//! - **Windows** flushes with `FlushFileBuffers`, which only works on a handle
//!   opened for writing -- the reason [`Fs::create_synced`] syncs on the handle
//!   that wrote. Directory entries are journaled by NTFS and renames are issued
//!   with `MOVEFILE_WRITE_THROUGH`, so [`Fs::sync_dir`] is a no-op.
//!
//! Whether a filesystem can be trusted at all is decided by [`Fs::volume`]:
//! a filesystem is `Local` only when it is recognized as one that keeps
//! exclusive advisory locks and atomic replacing renames. Anything the
//! operating system names but this module does not know is `Unsafe` -- an
//! unknown filesystem is never assumed to be safe.

use super::{
    Durability, Entry, Fs, Kind, LockGuard, Locks, Meta, Source, Stat, Volume, VolumeClass,
};
use std::{
    fs::{File, OpenOptions},
    io,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, UNIX_EPOCH},
};

#[cfg(not(any(unix, windows)))]
compile_error!("reldir's filesystem layer supports Unix and Windows only");

/// The real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct Disk;

/// The byte [`Fs::touch`] writes. Its value is irrelevant -- only the write's
/// effect on the file's mtime matters -- but it is fixed so a touch file's
/// contents are predictable.
const TOUCH_BYTE: u8 = b'.';

/// Nanoseconds in a second.
const NANOS: i64 = 1_000_000_000;

impl Source for Disk {
    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        #[cfg(unix)]
        {
            meta_of(&std::fs::symlink_metadata(path)?)
        }
        #[cfg(windows)]
        {
            windows::meta(path)
        }
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<Entry>> {
        // An entry removed between the enumeration and its stat surfaces as
        // NotFound rather than being dropped: the listing would otherwise be a
        // mixture of two moments, and the caller is the one that knows whether
        // to retry.
        let listed = std::fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
        let describe = |entry: &std::fs::DirEntry| -> io::Result<(SortKey, Entry)> {
            let name = entry.file_name();
            #[cfg(unix)]
            let meta = {
                // `DirEntry::metadata` is `fstatat(dirfd, name,
                // AT_SYMLINK_NOFOLLOW)`: the same facts as `symlink_metadata`
                // on the joined path, without resolving the directory again.
                meta_of(&entry.metadata()?)?
            };
            #[cfg(windows)]
            let meta = windows::meta(&entry.path())?;
            Ok((
                sort_key(&name),
                Entry {
                    path: entry.path(),
                    meta,
                },
            ))
        };
        // A large directory is examined by several threads at once: each
        // entry's metadata is an independent call, and on a cold cache each
        // waits on the disk.
        const PER_THREAD: usize = 2048;
        // Asking costs a read of the process's cgroup limits; the answer
        // does not change while it runs.
        static PARALLELISM: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let threads = (*PARALLELISM
            .get_or_init(|| std::thread::available_parallelism().map_or(1, |count| count.get())))
        .min(16)
        .min(listed.len().div_ceil(PER_THREAD));
        let mut entries: Vec<(SortKey, Entry)> = if threads <= 1 {
            listed.iter().map(describe).collect::<io::Result<_>>()?
        } else {
            let chunk = listed.len().div_ceil(threads);
            std::thread::scope(|scope| {
                let workers: Vec<_> = listed
                    .chunks(chunk)
                    .map(|part| {
                        scope.spawn(move || {
                            part.iter().map(describe).collect::<io::Result<Vec<_>>>()
                        })
                    })
                    .collect();
                let mut all = Vec::with_capacity(listed.len());
                for worker in workers {
                    all.extend(worker.join().expect("a metadata worker does not panic")?);
                }
                Ok::<_, io::Error>(all)
            })?
        };
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Ok(entries.into_iter().map(|(_, entry)| entry).collect())
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn read_at(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let file = File::open(path)?;
        let mut buffer = vec![0; len];
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            // `read_exact_at` fails with UnexpectedEof when the file ends
            // first, which is the contract: a short read is an error.
            file.read_exact_at(&mut buffer, offset)?;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut filled = 0;
            while filled < len {
                match file.seek_read(&mut buffer[filled..], offset + filled as u64) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!(
                                "{} ends before byte {}",
                                path.display(),
                                offset + len as u64
                            ),
                        ));
                    }
                    Ok(read) => filled += read,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(buffer)
    }
}

impl Fs for Disk {
    fn create_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        // A write that fails after the file was created (ENOSPC) leaves it
        // partly written. It is not durable, and the caller -- which knows
        // whether the name matters -- owns the cleanup.
        use io::Write;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?
            .write_all(bytes)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }

    fn create_synced(&self, path: &Path, bytes: &[u8], durability: Durability) -> io::Result<()> {
        use io::Write;
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(bytes)?;
        sync(&file, durability)
    }

    fn sync_file(&self, path: &Path, durability: Durability) -> io::Result<()> {
        // A write handle: Windows refuses to flush through a read-only one,
        // and opening for writing on every platform keeps a read-only file
        // from syncing on one platform and failing on another.
        let file = OpenOptions::new().write(true).open(path)?;
        sync(&file, durability)
    }

    fn sync_files(&self, paths: &[&Path], durability: Durability) -> io::Result<()> {
        // Many files on Linux: one syncfs flushes the filesystem's dirty data
        // and commits its journal once, where a sync per file commits it per
        // file. Only where syncfs reports writeback errors (Linux 5.8 and
        // later) does it keep fdatasync's promise.
        #[cfg(target_os = "linux")]
        if paths.len() >= SYNCFS_FROM
            && flush::syncfs_reports_errors()
            && let Some(first) = paths.first()
        {
            return flush::syncfs(first);
        }
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

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            let directory = File::open(path)?;
            #[cfg(target_vendor = "apple")]
            {
                // A directory's entries live in the drive's cache after a plain
                // fsync just as file data does.
                apple::full_fsync(&directory)
            }
            #[cfg(not(target_vendor = "apple"))]
            {
                directory.sync_all()
            }
        }
        // NTFS journals directory entries itself, and every rename is issued
        // with MOVEFILE_WRITE_THROUGH; Windows has no directory flush to call.
        #[cfg(windows)]
        {
            let _ = path;
            Ok(())
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::fs::rename(from, to)
        }
        #[cfg(windows)]
        {
            windows::rename(from, to)
        }
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

    fn volume(&self, path: &Path) -> io::Result<Volume> {
        #[cfg(target_os = "linux")]
        {
            linux::volume(path)
        }
        #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
        {
            bsd::volume(path)
        }
        #[cfg(all(
            unix,
            not(any(target_os = "linux", target_vendor = "apple", target_os = "freebsd"))
        ))]
        {
            other_unix::volume(path)
        }
        #[cfg(windows)]
        {
            windows::volume(path)
        }
    }

    fn touch(&self, path: &Path) -> io::Result<i64> {
        // A write, not `utimensat(NOW)`: the point is the timestamp a *write*
        // receives, which is what every data file reldir later stats carries.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            file.write_all_at(&[TOUCH_BYTE], 0)?;
            Ok(Stat::of(&file.metadata()?)?.mtime_ns)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            loop {
                match file.seek_write(&[TOUCH_BYTE], 0) {
                    Ok(1) => break,
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            format!("{}: a one-byte write wrote nothing", path.display()),
                        ));
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(windows::meta_of_handle(&file)?.stat.mtime_ns)
        }
    }

    fn mtime_granularity_ns(&self, dir: &Path) -> io::Result<i64> {
        let probe = dir.join(probe_name());
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)?;
        let measured = measure_granularity(&file);
        drop(file);
        match (measured, std::fs::remove_file(&probe)) {
            (Ok(granularity), Ok(())) => Ok(granularity),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(removal)) => Err(io::Error::new(
                removal.kind(),
                format!(
                    "the timestamp probe {} could not be removed: {removal}",
                    probe.display()
                ),
            )),
            (Err(error), Err(removal)) => Err(io::Error::new(
                error.kind(),
                format!(
                    "{error}; the timestamp probe {} could not be removed either: {removal}",
                    probe.display()
                ),
            )),
        }
    }
}

impl Locks for Disk {
    fn try_lock(&self, path: &Path) -> io::Result<Option<LockGuard>> {
        // `File::try_lock` is `flock(LOCK_EX|LOCK_NB)` on Unix and
        // `LockFileEx` on Windows. Both belong to the open file description
        // (handle), not the process, so a second handle in the *same* process
        // is refused like any other holder: exclusion holds between threads
        // as well as between processes. The kernel releases the lock when the
        // last descriptor closes, including when the process dies.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(LockGuard(Box::new(file)))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }
}

#[cfg(unix)]
impl Stat {
    /// The change-detection facts of a file, from metadata already fetched.
    ///
    /// Times outside what an `i64` of nanoseconds can hold (before 1677 or
    /// after 2262) are refused rather than clamped: two clamped times compare
    /// equal however far apart they were, and equality is exactly what a
    /// change check trusts.
    pub fn of(metadata: &std::fs::Metadata) -> io::Result<Stat> {
        use std::os::unix::fs::MetadataExt;
        Ok(Stat {
            size: metadata.len(),
            mtime_ns: nanos(metadata.mtime(), metadata.mtime_nsec(), "modification")?,
            ctime_ns: nanos(metadata.ctime(), metadata.ctime_nsec(), "change")?,
            inode: metadata.ino(),
            device: metadata.dev(),
        })
    }
}

/// `seconds` and `nanoseconds` since the epoch as one `i64`, or an error naming
/// which timestamp is out of range.
#[cfg(unix)]
fn nanos(seconds: i64, nanoseconds: i64, which: &str) -> io::Result<i64> {
    seconds
        .checked_mul(NANOS)
        .and_then(|whole| whole.checked_add(nanoseconds))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a {which} time of {seconds}s is outside the range reldir records"),
            )
        })
}

#[cfg(unix)]
fn meta_of(metadata: &std::fs::Metadata) -> io::Result<Meta> {
    use std::os::unix::fs::MetadataExt;
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
    Ok(Meta {
        kind,
        links: metadata.nlink(),
        stat: Stat::of(metadata)?,
    })
}

/// The order [`Source::read_dir`] promises: the bytes of the name as the
/// operating system stores it. On Unix that is the name itself; on Windows it
/// is UTF-16 code units, which order supplementary characters differently from
/// the WTF-8 bytes Rust keeps an `OsStr` in.
#[cfg(unix)]
type SortKey = Vec<u8>;
#[cfg(windows)]
type SortKey = Vec<u16>;

#[cfg(unix)]
fn sort_key(name: &std::ffi::OsStr) -> SortKey {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes().to_vec()
}

#[cfg(windows)]
fn sort_key(name: &std::ffi::OsStr) -> SortKey {
    use std::os::windows::ffi::OsStrExt;
    name.encode_wide().collect()
}

/// Sync a file through the handle that wrote it.
fn sync(file: &File, durability: Durability) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        match durability {
            Durability::Full => apple::full_fsync(file),
            Durability::Barrier => apple::barrier_fsync(file),
        }
    }
    // fdatasync: the data and the size needed to read it back, without the
    // timestamps. There is no weaker ordering-only primitive to use for a
    // barrier.
    #[cfg(all(unix, not(target_vendor = "apple")))]
    {
        let _ = durability;
        file.sync_data()
    }
    // FlushFileBuffers, through a handle opened for writing.
    #[cfg(windows)]
    {
        let _ = durability;
        file.sync_all()
    }
}

/// A name for the granularity probe that no other process or thread picks:
/// the process id separates processes, the counter separates calls within one.
fn probe_name() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        ".reldir-mtime-probe-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// The two modification times the probe asks for. Odd seconds, so a 2-second
/// filesystem must move them; sub-second parts whose every decimal digit is
/// non-zero, so truncating *or* rounding to any power of ten leaves a value
/// that is a multiple of that power and of no larger one.
const PROBES: [(u64, u32); 2] = [(1_600_000_001, 123_456_789), (1_600_000_003, 456_789_123)];

/// Granularities a filesystem can have, coarsest first.
const GRANULARITIES: [i64; 11] = [
    2 * NANOS,
    NANOS,
    100_000_000,
    10_000_000,
    1_000_000,
    100_000,
    10_000,
    1_000,
    100,
    10,
    1,
];

fn measure_granularity(file: &File) -> io::Result<i64> {
    let mut coarsest = 1;
    for (seconds, nanoseconds) in PROBES {
        let requested = UNIX_EPOCH + Duration::new(seconds, nanoseconds);
        file.set_times(std::fs::FileTimes::new().set_modified(requested))?;
        #[cfg(unix)]
        let observed = Stat::of(&file.metadata()?)?.mtime_ns;
        #[cfg(windows)]
        let observed = windows::meta_of_handle(file)?.stat.mtime_ns;
        let requested = seconds as i64 * NANOS + i64::from(nanoseconds);
        // Two probes may disagree only on a filesystem that rounds
        // inconsistently; the coarser answer is the safe one, because
        // overestimating the granularity costs a re-read while
        // underestimating it lets a same-tick change go unnoticed.
        coarsest = coarsest.max(granularity_from(requested, observed)?);
    }
    Ok(coarsest)
}

/// The granularity a filesystem showed by storing `observed` when asked for
/// `requested`: the coarsest known unit `observed` is a multiple of.
///
/// A value further from the request than the coarsest granularity can explain
/// means the filesystem did not store the time at all (some FUSE servers
/// ignore it), and no granularity can be inferred from that.
fn granularity_from(requested: i64, observed: i64) -> io::Result<i64> {
    if (observed - requested).abs() > GRANULARITIES[0] {
        return Err(io::Error::other(format!(
            "the filesystem stored a modification time of {observed}ns when asked \
             for {requested}ns; its timestamp granularity cannot be determined"
        )));
    }
    Ok(GRANULARITIES
        .into_iter()
        .find(|unit| observed % unit == 0)
        .expect("every time is a multiple of one nanosecond"))
}

/// Linux names filesystems by the magic number `statfs` reports.
#[cfg(any(target_os = "linux", test))]
mod linux_magic {
    use super::VolumeClass;

    const NETWORK: &str = "a network filesystem: advisory locks and replacing renames are \
                           not guaranteed to be coherent between the machines that share it";
    const CLUSTER: &str = "a cluster filesystem: advisory locks and replacing renames depend \
                           on the cluster's lock manager and configuration";
    const FUSE: &str = "a FUSE filesystem (including virtiofs, sshfs and sync clients): its \
                        user-space server decides whether locks exclude and renames are atomic";
    const FAT: &str = "a FAT-family filesystem: no journal, so a replacing rename is not \
                       atomic across a crash";
    const SHARED_FOLDER: &str = "a hypervisor shared folder: locks and renames are forwarded \
                                 to the host with no atomicity guarantee";

    /// The magic as the 32-bit number `linux/magic.h` defines, whatever
    /// integer type this platform's `f_type` is. On 32-bit glibc `f_type` is a
    /// signed 32-bit int, so magics with the high bit set (CIFS is 0xFF534D42)
    /// are negative there; widening them to a signed 64-bit value would
    /// sign-extend them past every constant they should match. Keeping the low
    /// 32 bits is right for every width and signedness.
    pub(super) fn magic_of(raw: i128) -> u32 {
        raw as u32
    }

    /// The name and class of the filesystem with this magic.
    pub(super) fn classify(magic: u32) -> (String, VolumeClass) {
        let local = |name: &str| (name.to_string(), VolumeClass::Local);
        let not_safe = |name: &str, why: &str| {
            (
                name.to_string(),
                VolumeClass::Unsafe(format!("{name} is {why}")),
            )
        };
        match magic {
            // ext2, ext3 and ext4 share one magic and cannot be told apart.
            0xEF53 => local("ext2/ext3/ext4"),
            0x5846_5342 => local("xfs"),
            0x9123_683E => local("btrfs"),
            0x0102_1994 => local("tmpfs"),
            0x2FC1_2FC1 => local("zfs"),
            0xF2F5_2010 => local("f2fs"),
            0xCA45_1A4E => local("bcachefs"),
            0x794C_7630 => local("overlayfs"),
            0x6969 => not_safe("nfs", NETWORK),
            0xFF53_4D42 => not_safe("cifs", NETWORK),
            0xFE53_4D42 => not_safe("smb2", NETWORK),
            0x517B => not_safe("smb", NETWORK),
            0x0102_1997 => not_safe("9p", NETWORK),
            0x5346_414F => not_safe("afs", NETWORK),
            0x564C => not_safe("ncp", NETWORK),
            0x6B41_4653 => not_safe("kafs", NETWORK),
            0x1983_0326 => not_safe("fhgfs", CLUSTER),
            0x4750_4653 => not_safe("gpfs", CLUSTER),
            0x0BD0_0BD0 => not_safe("lustre", CLUSTER),
            0x0131_11A8 => not_safe("ibrix", CLUSTER),
            0x00C3_6400 => not_safe("ceph", CLUSTER),
            // virtiofs mounts report FUSE's magic: virtiofs is FUSE over a
            // virtio transport.
            0x6573_5546 => not_safe("fuse", FUSE),
            0x786F_4256 => not_safe("vboxsf", SHARED_FOLDER),
            // vfat reports the msdos magic.
            0x4D44 => not_safe("msdos", FAT),
            0x2011_BAB0 => not_safe("exfat", FAT),
            unknown => (
                format!("0x{unknown:x}"),
                VolumeClass::Unsafe(format!("unrecognized filesystem 0x{unknown:x}")),
            ),
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{Volume, linux_magic};
    use std::{ffi::CString, io, os::unix::ffi::OsStrExt, os::unix::fs::MetadataExt, path::Path};

    pub(super) fn volume(path: &Path) -> io::Result<Volume> {
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        // SAFETY: an all-zero statfs is a valid value of a plain C struct of
        // integers; it is only an output buffer.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c_path` is a valid NUL-terminated string and `fs` a valid,
        // writable statfs; the call writes only into it.
        if unsafe { libc::statfs(c_path.as_ptr(), &mut fs) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above, for statvfs.
        let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut vfs) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // statfs and statvfs follow a symlink, so the device is read the same
        // way: the volume described is the one the path resolves into.
        let device = std::fs::metadata(path)?.dev();
        #[allow(clippy::unnecessary_cast)]
        let (kind, class) = linux_magic::classify(linux_magic::magic_of(fs.f_type as i128));
        Ok(Volume {
            #[allow(clippy::unnecessary_cast)]
            name_max: vfs.f_namemax as u64,
            device,
            kind,
            class,
        })
    }
}

/// macOS and FreeBSD name filesystems by `statfs.f_fstypename`.
#[cfg(any(target_vendor = "apple", target_os = "freebsd", test))]
mod bsd_names {
    use super::VolumeClass;

    /// The class of the filesystem `statfs` names `name` (lowercase).
    pub(super) fn classify(name: &str) -> VolumeClass {
        let why = match name {
            "apfs" | "hfs" | "ufs" | "zfs" | "tmpfs" => return VolumeClass::Local,
            "nfs" | "smbfs" | "afpfs" | "webdav" | "cifs" => {
                "a network filesystem: advisory locks and replacing renames are not \
                 guaranteed to be coherent between the machines that share it"
            }
            "msdos" | "exfat" => {
                "a FAT-family filesystem: no journal, so a replacing rename is not atomic \
                 across a crash"
            }
            fuse if fuse.contains("fuse") => {
                "a FUSE filesystem: its user-space server decides whether locks exclude \
                 and renames are atomic"
            }
            unknown => return VolumeClass::Unsafe(format!("unrecognized filesystem {unknown}")),
        };
        VolumeClass::Unsafe(format!("{name} is {why}"))
    }
}

#[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
mod bsd {
    use super::{Volume, bsd_names};
    use std::{ffi::CString, io, os::unix::ffi::OsStrExt, os::unix::fs::MetadataExt, path::Path};

    pub(super) fn volume(path: &Path) -> io::Result<Volume> {
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        // SAFETY: an all-zero statfs is a valid value of a plain C struct; it
        // is only an output buffer.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c_path` is a valid NUL-terminated string and `fs` a valid,
        // writable statfs; the call writes only into it.
        if unsafe { libc::statfs(c_path.as_ptr(), &mut fs) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let raw: Vec<u8> = fs
            .f_fstypename
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8)
            .collect();
        let kind = String::from_utf8(raw)
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("the filesystem type name is not UTF-8: {error}"),
                )
            })?
            .to_lowercase();
        Ok(Volume {
            name_max: super::unix_name_max(&c_path)?,
            device: std::fs::metadata(path)?.dev(),
            class: bsd_names::classify(&kind),
            kind,
        })
    }
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_vendor = "apple", target_os = "freebsd"))
))]
mod other_unix {
    use super::{Volume, VolumeClass};
    use std::{ffi::CString, io, os::unix::ffi::OsStrExt, os::unix::fs::MetadataExt, path::Path};

    /// On a Unix this module has no `statfs` layout for, the filesystem cannot
    /// be identified, so it is not trusted.
    pub(super) fn volume(path: &Path) -> io::Result<Volume> {
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        Ok(Volume {
            name_max: super::unix_name_max(&c_path)?,
            device: std::fs::metadata(path)?.dev(),
            kind: "unknown".into(),
            class: VolumeClass::Unsafe("unknown platform".into()),
        })
    }
}

/// `pathconf(_PC_NAME_MAX)`, distinguishing "no limit" (which reldir cannot
/// plan around, and which no real filesystem reports) from a failed call.
#[cfg(all(unix, not(target_os = "linux")))]
fn unix_name_max(c_path: &std::ffi::CStr) -> io::Result<u64> {
    errno::clear();
    // SAFETY: `c_path` is a valid NUL-terminated string; pathconf only reads it.
    let limit = unsafe { libc::pathconf(c_path.as_ptr(), libc::_PC_NAME_MAX) };
    if limit >= 0 {
        return Ok(limit as u64);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(0) {
        return Err(io::Error::other(
            "the filesystem reports no limit on file-name length",
        ));
    }
    Err(error)
}

#[cfg(all(unix, not(target_os = "linux")))]
mod errno {
    /// Reset `errno`, so a -1 that did not set it can be told from a failure.
    pub(super) fn clear() {
        #[cfg(any(
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "dragonfly"
        ))]
        // SAFETY: `__error` returns this thread's errno slot, always valid.
        unsafe {
            *libc::__error() = 0
        };
        #[cfg(any(target_os = "openbsd", target_os = "netbsd", target_os = "android"))]
        // SAFETY: `__errno` returns this thread's errno slot, always valid.
        unsafe {
            *libc::__errno() = 0
        };
        #[cfg(any(target_os = "illumos", target_os = "solaris"))]
        // SAFETY: `___errno` returns this thread's errno slot, always valid.
        unsafe {
            *libc::___errno() = 0
        };
    }
}

#[cfg(target_vendor = "apple")]
mod apple {
    use std::{fs::File, io, os::fd::AsRawFd};

    /// `F_FULLFSYNC`: the data, and the drive's cache, on stable storage.
    pub(super) fn full_fsync(file: &File) -> io::Result<()> {
        fcntl(file, libc::F_FULLFSYNC)
    }

    /// `F_BARRIERFSYNC`: writes before it reach the medium before writes
    /// after it, without waiting for a cache flush. A filesystem that does not
    /// implement it answers ENOTSUP or EINVAL; the full flush is then issued,
    /// because a flush orders everything before it as a barrier must.
    pub(super) fn barrier_fsync(file: &File) -> io::Result<()> {
        match fcntl(file, libc::F_BARRIERFSYNC) {
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL)) => {
                full_fsync(file)
            }
            other => other,
        }
    }

    fn fcntl(file: &File, command: libc::c_int) -> io::Result<()> {
        loop {
            // SAFETY: the descriptor is open for as long as `file` is borrowed,
            // and these commands take no argument.
            if unsafe { libc::fcntl(file.as_raw_fd(), command) } != -1 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

/// Windows names filesystems by `GetVolumeInformation`, and a network drive
/// by its drive type.
#[cfg(any(windows, test))]
mod windows_names {
    use super::VolumeClass;

    /// The class of a volume whose filesystem is `name` (lowercase), on a
    /// drive that is or is not remote.
    pub(super) fn classify(name: &str, remote: bool) -> VolumeClass {
        // A mapped drive reports the *server's* filesystem name, often NTFS,
        // so the drive type is consulted first.
        if remote {
            return VolumeClass::Unsafe("network drive".into());
        }
        match name {
            "ntfs" | "refs" => VolumeClass::Local,
            "fat" | "fat32" | "exfat" => VolumeClass::Unsafe(format!(
                "{name} is a FAT-family filesystem: no journal, so a replacing rename \
                 is not atomic across a crash"
            )),
            unknown => VolumeClass::Unsafe(format!("unrecognized filesystem {unknown}")),
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::{Kind, Meta, Stat, Volume, windows_names};
    use std::{
        ffi::c_void,
        fs::{File, OpenOptions},
        io,
        os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
        path::Path,
    };
    use windows_sys::Win32::{
        Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
            FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileAttributeTagInfo,
            FileBasicInfo, FileIdInfo, FileStandardInfo, GetDriveTypeW,
            GetFileInformationByHandleEx, GetVolumeInformationByHandleW, GetVolumePathNameW,
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
        System::{
            SystemServices::{IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK},
            WindowsProgramming::DRIVE_REMOTE,
        },
    };

    /// 100-nanosecond intervals between 1601-01-01 (FILETIME's epoch) and
    /// 1970-01-01.
    const FILETIME_UNIX_OFFSET: i64 = 116_444_736_000_000_000;

    /// A FILETIME-style count of 100ns intervals since 1601 as nanoseconds
    /// since the Unix epoch.
    fn filetime_ns(ticks: i64) -> io::Result<i64> {
        ticks
            .checked_sub(FILETIME_UNIX_OFFSET)
            .and_then(|since_epoch| since_epoch.checked_mul(100))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a timestamp is outside the range reldir records",
                )
            })
    }

    /// Open a path for its attributes only, without following a reparse point.
    /// Attribute-only access is not subject to sharing modes, so this succeeds
    /// on a file another process holds open exclusively.
    fn open_attributes(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    }

    fn query<T: Default>(file: &File, class: FILE_INFO_BY_HANDLE_CLASS) -> io::Result<T> {
        let mut info = T::default();
        // SAFETY: the handle is open while `file` is borrowed, `info` is a
        // writable value of exactly the structure `class` fills, and the size
        // passed is its size.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                class,
                (&mut info as *mut T).cast::<c_void>(),
                std::mem::size_of::<T>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info)
    }

    pub(super) fn meta(path: &Path) -> io::Result<Meta> {
        meta_of_handle(&open_attributes(path)?)
    }

    /// Metadata through an open handle: ChangeTime and the 128-bit FileId are
    /// not available from `std::fs::Metadata` on stable Rust.
    pub(super) fn meta_of_handle(file: &File) -> io::Result<Meta> {
        let basic: FILE_BASIC_INFO = query(file, FileBasicInfo)?;
        let standard: FILE_STANDARD_INFO = query(file, FileStandardInfo)?;
        let id: FILE_ID_INFO = query(file, FileIdInfo)?;
        let kind = if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            let tag: FILE_ATTRIBUTE_TAG_INFO = query(file, FileAttributeTagInfo)?;
            // Only name-surrogate links are links. Other reparse points (cloud
            // placeholders, deduplicated files) are ordinary files and
            // directories to every reader.
            if matches!(
                tag.ReparseTag,
                IO_REPARSE_TAG_SYMLINK | IO_REPARSE_TAG_MOUNT_POINT
            ) {
                Kind::Symlink
            } else if basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                Kind::Dir
            } else {
                Kind::File
            }
        } else if basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            Kind::Dir
        } else {
            Kind::File
        };
        let mut low = [0u8; 8];
        low.copy_from_slice(&id.FileId.Identifier[..8]);
        Ok(Meta {
            kind,
            links: u64::from(standard.NumberOfLinks),
            stat: Stat {
                size: u64::try_from(standard.EndOfFile).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "a negative file size")
                })?,
                mtime_ns: filetime_ns(basic.LastWriteTime)?,
                ctime_ns: filetime_ns(basic.ChangeTime)?,
                inode: u64::from_le_bytes(low),
                device: id.VolumeSerialNumber,
            },
        })
    }

    /// A path in the `\\?\` form Win32 passes through unparsed, so a path
    /// longer than MAX_PATH reaches the filesystem intact. Relative paths are
    /// refused: every path reldir hands the filesystem is absolute.
    pub(super) fn verbatim(path: &Path) -> io::Result<Vec<u16>> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not an absolute path", path.display()),
            ));
        }
        // GetFullPathNameW: separators unified and `.`/`..` resolved, which a
        // verbatim path no longer gets.
        let absolute: Vec<u16> = std::path::absolute(path)?
            .as_os_str()
            .encode_wide()
            .collect();
        let prefix = |text: &str| -> Vec<u16> { text.encode_utf16().collect() };
        let mut wide =
            if absolute.starts_with(&prefix(r"\\?\")) || absolute.starts_with(&prefix(r"\\.\")) {
                absolute
            } else if absolute.starts_with(&prefix(r"\\")) {
                let mut unc = prefix(r"\\?\UNC\");
                unc.extend_from_slice(&absolute[2..]);
                unc
            } else {
                let mut drive = prefix(r"\\?\");
                drive.extend_from_slice(&absolute);
                drive
            };
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} contains a NUL character", path.display()),
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    pub(super) fn rename(from: &Path, to: &Path) -> io::Result<()> {
        let from_wide = verbatim(from)?;
        let to_wide = verbatim(to)?;
        // SAFETY: both are NUL-terminated wide strings that outlive the call.
        let ok = unsafe {
            MoveFileExW(
                from_wide.as_ptr(),
                to_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            // A sharing violation (another process holds the target open
            // without FILE_SHARE_DELETE) is returned as is: whether to wait
            // and retry is the caller's decision, not this layer's.
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn volume(path: &Path) -> io::Result<Volume> {
        let handle = open_attributes(path)?;
        let id: FILE_ID_INFO = query(&handle, FileIdInfo)?;
        let mut serial = 0u32;
        let mut max_component = 0u32;
        let mut flags = 0u32;
        // MAX_PATH + 1 is the documented maximum for a filesystem name.
        let mut name = [0u16; 261];
        // SAFETY: the handle is open, every out-pointer is a valid writable
        // location, the name buffer's length is passed with it, and the
        // volume-name buffer is null with length zero, which the API allows.
        let ok = unsafe {
            GetVolumeInformationByHandleW(
                handle.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                &mut serial,
                &mut max_component,
                &mut flags,
                name.as_mut_ptr(),
                name.len() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let length = name
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(name.len());
        let kind = String::from_utf16(&name[..length])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            .to_lowercase();
        let wide = verbatim(path)?;
        let mut root = vec![0u16; wide.len().max(261)];
        // SAFETY: `wide` is NUL-terminated and `root` is writable for the
        // length passed.
        if unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: GetVolumePathNameW wrote a NUL-terminated path into `root`.
        let remote = unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_REMOTE;
        Ok(Volume {
            name_max: u64::from(max_component),
            device: id.VolumeSerialNumber,
            class: windows_names::classify(&kind, remote),
            kind,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        // Canonical, so the absolute-path rule holds even where the temp
        // directory is reached through a symlink (macOS /var).
        let root = directory.path().canonicalize().unwrap();
        (directory, root)
    }

    #[test]
    fn test8000_create_new_refuses_an_existing_file_and_keeps_its_bytes() {
        let (_guard, root) = scratch();
        let path = root.join("row.json");
        Disk.create_new(&path, b"first").unwrap();
        let error = Disk.create_new(&path, b"second").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(Disk.read(&path).unwrap(), b"first");
    }

    #[test]
    fn test8001_create_synced_round_trips_and_refuses_an_existing_file() {
        let (_guard, root) = scratch();
        for durability in [Durability::Full, Durability::Barrier] {
            let path = root.join(format!("journal-{durability:?}"));
            Disk.create_synced(&path, b"{\"id\":1}", durability)
                .unwrap();
            assert_eq!(Disk.read(&path).unwrap(), b"{\"id\":1}");
            assert_eq!(
                Disk.create_synced(&path, b"x", durability)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::AlreadyExists
            );
            Disk.sync_file(&path, durability).unwrap();
        }
        Disk.sync_dir(&root).unwrap();
    }

    #[test]
    fn test8002_rename_replaces_the_target_atomically() {
        let (_guard, root) = scratch();
        let target = root.join("row.json");
        let staged = root.join("row.json.tmp");
        Disk.write(&target, b"old").unwrap();
        Disk.write(&staged, b"new").unwrap();
        let staged_inode = Disk.metadata(&staged).unwrap().stat.inode;
        Disk.rename(&staged, &target).unwrap();
        assert_eq!(Disk.read(&target).unwrap(), b"new");
        assert_eq!(
            Disk.metadata(&target).unwrap().stat.inode,
            staged_inode,
            "the target is the staged file itself, not a copy of it"
        );
        assert_eq!(
            Disk.metadata(&staged).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn test8003_remove_dir_refuses_a_non_empty_directory() {
        let (_guard, root) = scratch();
        let table = root.join("t");
        Disk.create_dir(&table).unwrap();
        assert_eq!(
            Disk.create_dir(&table).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        Disk.write(&table.join("a.json"), b"{}").unwrap();
        assert_eq!(
            Disk.remove_dir(&table).unwrap_err().kind(),
            io::ErrorKind::DirectoryNotEmpty
        );
        assert!(Disk.metadata(&table.join("a.json")).is_ok());
        Disk.remove_file(&table.join("a.json")).unwrap();
        Disk.remove_dir(&table).unwrap();
        assert_eq!(
            Disk.metadata(&table).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn test8004_the_temp_directory_is_a_local_volume_with_long_names() {
        let (_guard, root) = scratch();
        let volume = Disk.volume(&root).unwrap();
        assert_eq!(
            volume.class,
            VolumeClass::Local,
            "the test machine's temp directory ({}) must be recognized as local",
            volume.kind
        );
        assert!(volume.name_max >= 255, "{volume:?}");
        assert_eq!(volume.kind, volume.kind.to_lowercase());
        assert_eq!(
            volume.device,
            Disk.metadata(&root).unwrap().stat.device,
            "the volume's identity is the device its files report"
        );
    }

    #[test]
    fn test8005_touch_returns_a_clock_that_never_runs_backwards() {
        let (_guard, root) = scratch();
        let clock = root.join("clock");
        let first = Disk.touch(&clock).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let second = Disk.touch(&clock).unwrap();
        assert!(second >= first, "{second} < {first}");
        assert_eq!(Disk.read(&clock).unwrap(), [TOUCH_BYTE]);
        assert_eq!(Disk.metadata(&clock).unwrap().stat.mtime_ns, second);
        // A touch is a write: it lands within a minute of the wall clock.
        let wall = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        assert!((wall - second).abs() < 60 * NANOS, "{wall} vs {second}");
    }

    #[test]
    fn test8006_the_granularity_probe_reports_a_known_unit_and_cleans_up() {
        let (_guard, root) = scratch();
        let granularity = Disk.mtime_granularity_ns(&root).unwrap();
        assert!(
            [1, 100, NANOS, 2 * NANOS].contains(&granularity),
            "{granularity}"
        );
        assert!(
            Disk.read_dir(&root).unwrap().is_empty(),
            "the probe file must be removed"
        );
    }

    #[test]
    fn test8007_granularity_is_inferred_from_truncation_or_rounding() {
        for (seconds, nanoseconds) in PROBES {
            let requested = seconds as i64 * NANOS + i64::from(nanoseconds);
            for unit in [1, 100, 1_000, 1_000_000, NANOS, 2 * NANOS] {
                let truncated = requested - requested.rem_euclid(unit);
                assert_eq!(granularity_from(requested, truncated).unwrap(), unit);
                let rounded = (requested + unit / 2) / unit * unit;
                assert_eq!(granularity_from(requested, rounded).unwrap(), unit);
            }
        }
        // A filesystem that ignored the request says nothing about its unit.
        assert!(granularity_from(1_600_000_001 * NANOS, 1_700_000_000 * NANOS).is_err());
    }

    #[test]
    fn test8008_a_second_lock_attempt_is_refused_even_within_one_process() {
        let (_guard, root) = scratch();
        let path = root.join("lock");
        let held = Disk.try_lock(&path).unwrap().expect("an uncontended lock");
        // flock and LockFileEx belong to the open handle, so a second handle in
        // this same process is another holder and must be refused.
        assert!(Disk.try_lock(&path).unwrap().is_none());
        let from_thread = std::thread::spawn({
            let path = path.clone();
            move || Disk.try_lock(&path).unwrap().is_none()
        })
        .join()
        .unwrap();
        assert!(from_thread, "another thread is refused too");
        drop(held);
        assert!(
            Disk.try_lock(&path).unwrap().is_some(),
            "dropping the guard releases the lock"
        );
    }

    #[test]
    fn test8009_read_dir_lists_sorted_entries_with_unfollowed_metadata() {
        let (_guard, root) = scratch();
        Disk.write(&root.join("b.json"), b"bb").unwrap();
        Disk.write(&root.join("a.json"), b"a").unwrap();
        Disk.create_dir(&root.join("c")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("a.json"), root.join("d")).unwrap();
        let entries = Disk.read_dir(&root).unwrap();
        let names: Vec<_> = entries
            .iter()
            .map(|entry| entry.path.file_name().unwrap().to_owned())
            .collect();
        #[cfg(unix)]
        assert_eq!(names, ["a.json", "b.json", "c", "d"]);
        #[cfg(windows)]
        assert_eq!(names, ["a.json", "b.json", "c"]);
        assert_eq!(entries[0].meta.kind, Kind::File);
        assert_eq!(entries[0].meta.stat.size, 1);
        assert_eq!(entries[1].meta.stat.size, 2);
        assert_eq!(entries[2].meta.kind, Kind::Dir);
        #[cfg(unix)]
        assert_eq!(
            entries[3].meta.kind,
            Kind::Symlink,
            "links are not followed"
        );
        assert_eq!(
            entries[0].meta,
            Disk.metadata(&root.join("a.json")).unwrap()
        );
    }

    #[test]
    fn test8010_read_at_returns_the_range_and_refuses_a_short_file() {
        let (_guard, root) = scratch();
        let path = root.join("blob");
        Disk.write(&path, b"0123456789").unwrap();
        assert_eq!(Disk.read_at(&path, 3, 4).unwrap(), b"3456");
        assert_eq!(Disk.read_at(&path, 10, 0).unwrap(), b"");
        assert_eq!(
            Disk.read_at(&path, 8, 3).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn test8011_linux_magics_classify_whatever_the_width_of_f_type() {
        // CIFS's magic has the high bit set: on 32-bit glibc it arrives as a
        // negative i32, and sign-extending it must not lose the match.
        let negative = i128::from(0xFF53_4D42_u32 as i32);
        assert!(negative < 0);
        assert_eq!(linux_magic::magic_of(negative), 0xFF53_4D42);
        assert_eq!(linux_magic::magic_of(0xFF53_4D42), 0xFF53_4D42);
        let (kind, class) = linux_magic::classify(linux_magic::magic_of(negative));
        assert_eq!(kind, "cifs");
        assert!(matches!(class, VolumeClass::Unsafe(ref why) if why.starts_with("cifs")));
        for local in [0xEF53, 0x5846_5342, 0x9123_683E, 0x0102_1994, 0x2FC1_2FC1] {
            assert_eq!(linux_magic::classify(local).1, VolumeClass::Local);
        }
        for remote in [0x6969, 0x6573_5546, 0x4D44, 0x2011_BAB0, 0x00C3_6400] {
            assert!(matches!(
                linux_magic::classify(remote).1,
                VolumeClass::Unsafe(_)
            ));
        }
        assert_eq!(
            linux_magic::classify(0x1234_5678),
            (
                "0x12345678".to_string(),
                VolumeClass::Unsafe("unrecognized filesystem 0x12345678".into())
            ),
            "an unknown filesystem is never assumed safe"
        );
    }

    #[test]
    fn test8012_bsd_and_windows_names_classify_conservatively() {
        assert_eq!(bsd_names::classify("apfs"), VolumeClass::Local);
        assert_eq!(bsd_names::classify("hfs"), VolumeClass::Local);
        for name in [
            "nfs",
            "smbfs",
            "afpfs",
            "webdav",
            "msdos",
            "exfat",
            "macfuse",
            "fusefs.sshfs",
        ] {
            assert!(
                matches!(bsd_names::classify(name), VolumeClass::Unsafe(ref why) if why.starts_with(name)),
                "{name}"
            );
        }
        assert_eq!(
            bsd_names::classify("lifs"),
            VolumeClass::Unsafe("unrecognized filesystem lifs".into())
        );
        assert_eq!(windows_names::classify("ntfs", false), VolumeClass::Local);
        assert_eq!(windows_names::classify("refs", false), VolumeClass::Local);
        assert_eq!(
            windows_names::classify("ntfs", true),
            VolumeClass::Unsafe("network drive".into()),
            "a mapped drive is remote whatever filesystem the server reports"
        );
        for fat in ["fat", "fat32", "exfat"] {
            assert!(matches!(
                windows_names::classify(fat, false),
                VolumeClass::Unsafe(_)
            ));
        }
        assert!(matches!(
            windows_names::classify("udf", false),
            VolumeClass::Unsafe(ref why) if why == "unrecognized filesystem udf"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test8013_stat_carries_nanosecond_times_inode_and_device() {
        use std::os::unix::fs::MetadataExt;
        let (_guard, root) = scratch();
        let path = root.join("row.json");
        Disk.write(&path, b"12345").unwrap();
        let file = File::options().write(true).open(&path).unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(UNIX_EPOCH + Duration::new(1_600_000_001, 123_456_789)),
        )
        .unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        let stat = Stat::of(&metadata).unwrap();
        assert_eq!(stat.size, 5);
        assert_eq!(stat.inode, metadata.ino());
        assert_eq!(stat.device, metadata.dev());
        let granularity = Disk.mtime_granularity_ns(&root).unwrap();
        let expected = 1_600_000_001 * NANOS + 123_456_789;
        assert_eq!(stat.mtime_ns, expected - expected % granularity);
        assert!(stat.ctime_ns > stat.mtime_ns, "ctime is the kernel's now");
        // Before the epoch is a negative time, not an error or zero.
        file.set_times(
            std::fs::FileTimes::new().set_modified(UNIX_EPOCH - Duration::from_secs(10)),
        )
        .unwrap();
        let before = Stat::of(&std::fs::symlink_metadata(&path).unwrap()).unwrap();
        assert_eq!(before.mtime_ns, -10 * NANOS);
    }

    #[test]
    fn test8014_sync_file_needs_an_existing_file_and_sync_dir_a_directory() {
        let (_guard, root) = scratch();
        assert_eq!(
            Disk.sync_file(&root.join("missing"), Durability::Full)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        #[cfg(unix)]
        assert_eq!(
            Disk.sync_dir(&root.join("missing")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}

/// How many files a batch must hold before one filesystem-wide sync is
/// cheaper than a sync of each.
#[cfg(target_os = "linux")]
const SYNCFS_FROM: usize = 256;

#[cfg(target_os = "linux")]
mod flush {
    use std::{io, os::fd::AsRawFd, path::Path};

    /// Flush the whole filesystem holding `path`.
    pub fn syncfs(path: &Path) -> io::Result<()> {
        let file = std::fs::File::open(path)?;
        // SAFETY: the descriptor is this open file's, valid for the call.
        if unsafe { libc::syncfs(file.as_raw_fd()) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Whether the running kernel reports writeback errors through syncfs:
    /// Linux 5.8 and later.
    pub fn syncfs_reports_errors() -> bool {
        static REPORTS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *REPORTS.get_or_init(|| {
            let mut name: libc::utsname = unsafe { std::mem::zeroed() };
            // SAFETY: `name` is a valid, writable utsname.
            if unsafe { libc::uname(&mut name) } != 0 {
                return false;
            }
            // SAFETY: uname wrote a NUL-terminated release string.
            let release = unsafe { std::ffi::CStr::from_ptr(name.release.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let mut parts = release
                .split(|c: char| !c.is_ascii_digit())
                .filter(|part| !part.is_empty())
                .map(|part| part.parse::<u32>().unwrap_or(0));
            let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
            (major, minor) >= (5, 8)
        })
    }
}
