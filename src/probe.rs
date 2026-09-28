//! What kind of filesystem the database lives on.
//!
//! The commit protocol rests on two promises a local POSIX filesystem makes:
//! an advisory lock excludes every other holder, and a rename replaces its
//! target atomically. Network filesystems (NFS, SMB/CIFS) and FUSE-based sync
//! clients commonly break one or both -- a lock that is silently local to one
//! machine, a rename that is emulated as copy-then-delete -- and a database
//! that trusted them would lose updates without a word.
//!
//! So reldir asks the operating system what the filesystem is, and refuses to
//! *write* to one of those kinds unless `.db/config` sets
//! `"allow_remote_filesystem": true`: a decision someone made on purpose about
//! a filesystem they know. Reading is always allowed.

use std::path::Path;

/// A filesystem's kind, as far as the commit protocol is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Class {
    /// A local filesystem that keeps both promises.
    Local,
    /// A filesystem that may not, named as the operating system names it.
    Remote(String),
    /// The operating system would not say.
    Unknown,
}

/// Classify the filesystem holding `path`.
#[cfg(target_os = "linux")]
pub fn classify(path: &Path) -> Class {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return Class::Unknown;
    };
    let mut buffer: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid NUL-terminated string and `buffer` a valid,
    // writable statfs; the call writes only into it.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut buffer) } != 0 {
        return Class::Unknown;
    }
    // Magic numbers from linux/magic.h.
    let name = match buffer.f_type as i64 {
        0x6969 => "nfs",
        0xFF534D42 => "cifs",
        0xFE534D42 => "smb2",
        0x517B => "smb",
        0x65735546 => "fuse",
        0x01021997 => "9p",
        0x5346414F => "afs",
        0x564C => "ncp",
        0x6B414653 => "kafs",
        0x19830326 => "fhgfs",
        0x47504653 => "gpfs",
        0x0BD00BD0 => "lustre",
        0x013111A8 => "ibrix",
        0x00C36400 => "ceph",
        _ => return Class::Local,
    };
    Class::Remote(name.into())
}

/// Classify the filesystem holding `path`.
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
pub fn classify(path: &Path) -> Class {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return Class::Unknown;
    };
    let mut buffer: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut buffer) } != 0 {
        return Class::Unknown;
    }
    let raw: Vec<u8> = buffer
        .f_fstypename
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    let name = String::from_utf8_lossy(&raw).to_string();
    match name.as_str() {
        "nfs" | "smbfs" | "afpfs" | "webdav" | "cifs" | "osxfuse" | "macfuse" | "fusefs" => {
            Class::Remote(name)
        }
        name if name.starts_with("fuse") => Class::Remote(name.to_string()),
        _ => Class::Local,
    }
}

/// Classify the filesystem holding `path`.
#[cfg(windows)]
pub fn classify(path: &Path) -> Class {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut volume = vec![0u16; 1024];
    // SAFETY: both buffers are valid for their stated lengths.
    if unsafe { GetVolumePathNameW(wide.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) } == 0 {
        return Class::Unknown;
    }
    // DRIVE_REMOTE = 4.
    match unsafe { GetDriveTypeW(volume.as_ptr()) } {
        4 => Class::Remote("network drive".into()),
        0 | 1 => Class::Unknown,
        _ => Class::Local,
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    windows
)))]
pub fn classify(_path: &Path) -> Class {
    Class::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test2140_a_temporary_directory_is_local() {
        let directory = tempfile::tempdir().unwrap();
        let class = classify(directory.path());
        assert!(
            matches!(class, Class::Local | Class::Unknown),
            "the test machine's temp directory is not a network mount: {class:?}"
        );
    }
}
