//! What can go wrong in the store, as types a caller can act on.
//!
//! Three causes are kept apart because each calls for a different response:
//! the filesystem failed ([`StoreError::Io`]: retry or report), recorded bytes
//! are not what the format allows ([`StoreError::Corrupt`] and
//! [`StoreError::CorruptObject`]: never guessed through), or the caller asked
//! for something illegal ([`StoreError::Invalid`]: a bug or bad input upstream).

use super::Id;
use std::{
    io,
    path::{Path, PathBuf},
};

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("cannot {op} {}: {source}", path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    /// A file of the store is damaged: a pack, an index, a head, a ref.
    #[error("{} is corrupt: {what}", path.display())]
    Corrupt { what: Corruption, path: PathBuf },
    /// An object's bytes are intact -- they hash to its id -- but are not a
    /// well-formed object of the kind they were read as.
    #[error("object {id} is corrupt: {what}")]
    CorruptObject { id: Id, what: Corruption },
    /// No pack holds the object.
    #[error("object {0} is not in the store")]
    Missing(Id),
    #[error("{0}")]
    Invalid(Invalid),
}

impl StoreError {
    pub(crate) fn io(op: &'static str, path: &Path, source: io::Error) -> Self {
        StoreError::Io {
            op,
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn corrupt(what: Corruption, path: &Path) -> Self {
        StoreError::Corrupt {
            what,
            path: path.to_path_buf(),
        }
    }

    /// Whether the error is an I/O failure because a path does not exist.
    pub fn is_not_found(&self) -> bool {
        matches!(self, StoreError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound)
    }
}

/// The ways recorded bytes can fail to be what the format allows.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Corruption {
    #[error("bad magic number")]
    BadMagic,
    #[error("unsupported version {0}")]
    UnsupportedVersion(u32),
    #[error("ends before its structure does")]
    Truncated,
    #[error("is {actual} bytes where its header implies {expected}")]
    WrongSize { expected: u64, actual: u64 },
    #[error("its trailing checksum does not match its contents")]
    TrailerMismatch,
    #[error("the index names a pack checksum the pack does not have")]
    PackChecksumMismatch,
    #[error("the index lists {idx} objects and the pack holds {pack}")]
    CountMismatch { idx: u64, pack: u64 },
    #[error("the fan-out table decreases at byte {byte:#04x}")]
    FanoutNotMonotonic { byte: u8 },
    #[error("entry {index} lies outside the fan-out range of its first byte")]
    FanoutMisplaced { index: u64 },
    #[error("id {id} is listed twice")]
    DuplicateId { id: Id },
    #[error("ids are not in ascending order at entry {index}")]
    UnsortedIds { index: u64 },
    #[error("object {id} lies outside the pack's entries")]
    EntryOutOfBounds { id: Id },
    #[error("the entry for {id} disagrees with its index record")]
    EntryHeaderMismatch { id: Id },
    #[error("the pack's entry {index} is not the entry its index records")]
    PackEntryMismatch { index: u64 },
    #[error("unknown object kind {0}")]
    UnknownObjectKind(u8),
    #[error("holds bytes hashing to {actual} under id {expected}")]
    HashMismatch { expected: Id, actual: Id },
    #[error("malformed or non-minimal varint")]
    MalformedVarint,
    #[error("unknown node kind {0:#04x}")]
    UnknownNodeKind(u8),
    #[error("a {found} node where a {expected} node belongs")]
    UnexpectedNode {
        expected: &'static str,
        found: &'static str,
    },
    #[error("a {0} node with nothing in it")]
    EmptyNode(&'static str),
    #[error("entries are not strictly ascending at {0:?}")]
    UnsortedEntries(String),
    #[error("a name is not UTF-8")]
    InvalidUtf8,
    #[error("illegal path or name {0:?}")]
    IllegalPath(String),
    #[error("child {index} is half empty: one of its id and identity is zero")]
    HalfEmptyChild { index: usize },
    #[error("{0} bytes follow the end of the structure")]
    TrailingBytes(usize),
    #[error("not in its one canonical encoding")]
    NonCanonical,
    #[error("not a revision: {0}")]
    Revision(String),
    #[error("not a ref: {0}")]
    Ref(String),
    #[error("an index without its pack")]
    IdxWithoutPack,
    #[error("an entry that does not belong here")]
    UnexpectedEntry,
    #[error("not a regular file")]
    NotAFile,
    #[error("not a directory")]
    NotADirectory,
    #[error("a head file is not empty")]
    NonEmptyHead,
    #[error("records identity {recorded} for a child whose identity is {computed}")]
    IdentityMismatch {
        recorded: super::Hash256,
        computed: super::Hash256,
    },
    #[error("holds {path:?}, which does not belong in this position")]
    Misplaced { path: String },
}

/// Requests the store refuses because they are illegal, not because anything
/// on disk is wrong.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Invalid {
    #[error("illegal path {0:?}")]
    Path(String),
    #[error("illegal table name {0:?}")]
    TableName(String),
    #[error("{0:?} is given twice")]
    Duplicate(String),
    #[error("{path:?} is not under table {table:?}")]
    OutsideTable { path: String, table: String },
    #[error("no table {0:?}")]
    UnknownTable(String),
    #[error("table {0:?} already exists")]
    TableExists(String),
    #[error("{0:?} is already present")]
    AlreadyPresent(String),
    #[error("{0:?} is not present")]
    NotPresent(String),
    #[error("{0:?} is both a row and an extra file")]
    RowAndExtra(String),
    #[error("{0:?} is changed twice in one delta")]
    ConflictingDelta(String),
    #[error("an object of {0} bytes exceeds the pack limit of 4 GiB")]
    ObjectTooLarge(u64),
    #[error("a pack holds at most 2^32 - 1 objects")]
    TooManyObjects,
    #[error("a pack must hold at least one object")]
    EmptyPack,
    #[error("{0:?} is not a pack name")]
    PackName(String),
    #[error("{0} already exists; a published pack is never replaced")]
    PackExists(PathBuf),
    #[error("{0:?} cannot name a snapshot: use lowercase letters, digits, '-', '_' and '.'")]
    SnapshotName(String),
    #[error("snapshot {0:?} already exists")]
    RefExists(String),
    #[error("revision {0} cannot be both the new head and one of its parents")]
    HeadIsParent(Id),
    #[error("{0} is given twice")]
    DuplicateParent(Id),
    #[error("timestamp {0} has no RFC 3339 rendering (years 0 to 9999)")]
    Timestamp(String),
    #[error("{0} exceeds the largest integer JSON represents exactly (2^53 - 1)")]
    IntegerTooLarge(u64),
}
