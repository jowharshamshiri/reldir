//! Format 3 history: a content-addressed object store, Merkle trees of the
//! database's state, and a DAG of revisions over them.
//!
//! Three kinds of object exist, each named by the SHA-256 of its bytes:
//!
//! - a **blob** is a file's original bytes, verbatim, so any recorded file can
//!   be put back byte for byte;
//! - a **node** is one level of a state's Merkle tree ([`node`], [`tree`]);
//! - a **revision** is one accepted state and the revisions it followed
//!   ([`revision`]).
//!
//! Objects live in immutable packs under `.db/objects/pack/` ([`pack`]), read
//! through [`Store`], which checks every object it returns against its id.
//! Tips of history are empty files under `.db/history/heads/` ([`refs`]);
//! ancestry, merge bases, reachability and the last known content of a path
//! are in [`dag`]; [`verify`] proves a history intact.
//!
//! Every byte this module writes goes through [`crate::engine::fs::Fs`], and every byte
//! it reads through [`crate::engine::fs::Source`].

mod codec;
pub mod consolidate;
pub mod dag;
pub mod error;

pub mod node;
pub mod objects;
pub mod pack;
pub mod refs;
pub mod revision;
pub mod tree;
pub mod verify;

#[cfg(test)]
pub(crate) mod test_fs;

pub use error::{Corruption, Invalid, Result, StoreError};
pub use objects::Store;

use crate::engine::fs::{Fs, Kind};
use sha2::{Digest, Sha256};
use std::{
    fmt, io,
    path::{Path, PathBuf},
};

macro_rules! digest_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub [u8; 32]);

        impl $name {
            /// Thirty-two zero bytes: the encoding of "none" wherever the
            /// format reserves a fixed-width slot for an optional digest.
            pub const ZERO: Self = Self([0; 32]);

            pub fn is_zero(&self) -> bool {
                self.0 == [0; 32]
            }

            pub fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            /// Exactly 64 lowercase hex digits; anything else -- uppercase
            /// included -- is refused, so every digest has one spelling.
            pub fn from_hex(text: &str) -> Option<Self> {
                if text.len() != 64
                    || !text
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return None;
                }
                let mut out = [0; 32];
                hex::decode_to_slice(text, &mut out).ok()?;
                Some(Self(out))
            }

            pub fn to_hex(&self) -> String {
                hex::encode(self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_hex())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.to_hex())
            }
        }
    };
}

digest_type!(
    /// An object's name: the SHA-256 of its bytes.
    Id
);

digest_type!(
    /// A digest that is not an object's name: a row's canonical hash, a
    /// schema, format or configuration hash, or a node's *identity*. Kept a
    /// separate type from [`Id`] because the tree carries both side by side,
    /// and confusing an object id with an identity is exactly the mistake
    /// that would make formatting look like a change of state.
    Hash256
);

impl Id {
    /// The id of these bytes.
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

impl Hash256 {
    /// The SHA-256 of these bytes.
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

/// The role an object was stored in.
///
/// The role is recorded in the pack beside the bytes, but it is not part of
/// the object's identity: an id names bytes, and identical bytes are one
/// object whatever role they were first stored in. That matters because a
/// blob is a file's bytes verbatim, and nothing stops a file from holding
/// exactly the bytes of a node or a revision; a pack keeps one copy, and a
/// reader that needs a node decodes the bytes as a node -- decoding, not the
/// recorded role, decides whether they are one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    Blob,
    Node,
    Revision,
}

impl ObjectKind {
    pub fn code(self) -> u8 {
        match self {
            ObjectKind::Blob => 1,
            ObjectKind::Node => 2,
            ObjectKind::Revision => 3,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(ObjectKind::Blob),
            2 => Some(ObjectKind::Node),
            3 => Some(ObjectKind::Revision),
            _ => None,
        }
    }
}

/// An object's bytes and the role they were stored in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub kind: ObjectKind,
    pub bytes: Vec<u8>,
}

impl Object {
    pub fn new(kind: ObjectKind, bytes: Vec<u8>) -> Self {
        Self { kind, bytes }
    }

    pub fn id(&self) -> Id {
        Id::of(&self.bytes)
    }
}

/// Where format 3 keeps history under a database root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    /// `root` is the database root: the directory holding `.db/`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn db(&self) -> PathBuf {
        self.root.join(".db")
    }
    pub fn objects(&self) -> PathBuf {
        self.db().join("objects")
    }
    pub fn packs(&self) -> PathBuf {
        self.objects().join("pack")
    }
    pub fn history(&self) -> PathBuf {
        self.db().join("history")
    }
    pub fn heads(&self) -> PathBuf {
        self.history().join("heads")
    }
    pub fn refs(&self) -> PathBuf {
        self.history().join("refs")
    }
    pub fn snapshot_refs(&self) -> PathBuf {
        self.refs().join("snapshots")
    }
    pub fn quarantine(&self) -> PathBuf {
        self.history().join("quarantine")
    }

    /// Create every history directory that does not exist yet, each made
    /// durable in its parent before anything is put in it. `.db/` itself must
    /// exist. A path that exists but is not a real directory is corruption,
    /// never something to write through.
    ///
    /// Readers never create these directories, and a missing one is an error
    /// to them: the absence of history is spelled by empty directories, so a
    /// directory that vanished is never mistaken for "nothing recorded".
    pub fn init(&self, fs: &dyn Fs) -> Result<()> {
        let db = self.db();
        require_dir(fs, &db)?;
        for dir in [
            self.objects(),
            self.packs(),
            self.history(),
            self.heads(),
            self.refs(),
            self.snapshot_refs(),
            self.quarantine(),
        ] {
            ensure_dir(fs, &dir)?;
        }
        Ok(())
    }
}

/// `path` is a real directory (not a symlink to one).
pub(crate) fn require_dir(source: &dyn crate::engine::fs::Source, path: &Path) -> Result<()> {
    let meta = source
        .metadata(path)
        .map_err(|e| StoreError::io("inspect", path, e))?;
    if meta.kind != Kind::Dir {
        return Err(StoreError::Corrupt {
            what: Corruption::NotADirectory,
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// Create `path` if absent and make its entry durable in its parent.
pub(crate) fn ensure_dir(fs: &dyn Fs, path: &Path) -> Result<()> {
    match fs.create_dir(path) {
        Ok(()) => {
            let parent = path.parent().ok_or_else(|| {
                StoreError::Invalid(Invalid::Path(format!("{} has no parent", path.display())))
            })?;
            fs.sync_dir(parent)
                .map_err(|e| StoreError::io("sync directory", parent, e))
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => require_dir(fs, path),
        Err(e) => Err(StoreError::io("create directory", path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_fs::StdFs;

    #[test]
    fn test8200_digests_have_exactly_one_spelling() {
        let id = Id::of(b"reldir");
        let text = id.to_hex();
        assert_eq!(Id::from_hex(&text), Some(id));
        assert_eq!(Id::from_hex(&text.to_uppercase()), None, "uppercase");
        assert_eq!(Id::from_hex(&text[1..]), None, "short");
        assert_eq!(Id::from_hex(&format!("{text}0")), None, "long");
        assert_eq!(Id::from_hex(&format!("g{}", &text[1..])), None, "non-hex");
        assert_eq!(Id::from_hex(&format!("+{}", &text[1..])), None, "sign");
        assert!(Id::ZERO.is_zero());
        assert!(!id.is_zero());
    }

    #[test]
    fn test8201_object_kind_codes_round_trip_and_unknown_codes_are_refused() {
        for kind in [ObjectKind::Blob, ObjectKind::Node, ObjectKind::Revision] {
            assert_eq!(ObjectKind::from_code(kind.code()), Some(kind));
        }
        for code in [0u8, 4, 255] {
            assert_eq!(ObjectKind::from_code(code), None);
        }
    }

    #[test]
    fn test8202_layout_init_creates_every_directory_and_refuses_a_file_in_the_way() {
        let fs = StdFs::new();
        let root = fs.root().join("db");
        std::fs::create_dir_all(root.join(".db")).unwrap();
        let layout = Layout::new(&root);
        layout.init(&fs).unwrap();
        layout.init(&fs).unwrap();
        for dir in [
            layout.packs(),
            layout.heads(),
            layout.snapshot_refs(),
            layout.quarantine(),
        ] {
            assert!(dir.is_dir(), "{}", dir.display());
        }
        let other = fs.root().join("other");
        std::fs::create_dir_all(other.join(".db/objects")).unwrap();
        std::fs::write(other.join(".db/objects/pack"), b"").unwrap();
        let error = Layout::new(&other).init(&fs).unwrap_err();
        assert!(
            matches!(&error, StoreError::Corrupt { what: Corruption::NotADirectory, path } if path.ends_with("pack")),
            "{error:?}"
        );
        let missing = Layout::new(fs.root().join("nowhere"))
            .init(&fs)
            .unwrap_err();
        assert!(matches!(missing, StoreError::Io { .. }), "{missing:?}");
    }
}
