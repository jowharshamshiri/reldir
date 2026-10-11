//! The nodes of a state's Merkle tree, their one encoding each, and their
//! identities.
//!
//! A state is a root over tables; a table is 64 mids of 64 leaves; a row lives
//! in the leaf its path's bucket names (the first 12 bits of SHA-256 of the
//! path: six for the mid, six for the leaf). Every node begins `\0RDN3` and a
//! kind byte:
//!
//! ```text
//! L leaf    entries by path: varint:len path  32:row_hash  32:blob
//! M mid     64 × ( 32:child id  32:child identity )      all-zero = empty
//! T table   32:schema identity (zero = none)  64 × ( 32:mid id  32:mid identity )
//! R root    32:format  32:config  32:extras id (zero = none)
//!           tables by name: varint:len name  32:table id  32:table identity
//! X extras  entries by path: varint:len path  32:blob
//! ```
//!
//! Each node has two digests. Its **object id** is the SHA-256 of its bytes,
//! and covers everything -- blob ids included, so a tree restores files byte
//! for byte. Its **identity** covers only what decides the logical state:
//!
//! ```text
//! leaf   SHA-256("reldir-leaf-v3\0"  ‖ each entry: path ‖ 0x00 ‖ row_hash)
//! mid    SHA-256("reldir-mid-v3\0"   ‖ 64 child identities)
//! table  SHA-256("reldir-table-v3\0" ‖ schema ‖ 64 mid identities)
//! root   SHA-256("reldir-state-v3\0" ‖ format ‖ config ‖ each table: name ‖ 0x00 ‖ identity)
//! ```
//!
//! An empty child contributes 32 zero bytes, so an empty table's identity is
//! SHA-256 of the table prefix, its schema and 2048 zero bytes. Blob ids and
//! the extras node are outside every identity: reformatting a file, or a
//! file that is not a row, is never a change of state. Paths and names hold
//! no NUL, so each concatenation above parses one way only.
//!
//! Decoding accepts exactly the bytes encoding produces -- sorted, unique,
//! no empty leaf or mid stored, no trailing bytes -- so one structure has one
//! object id.

use super::{
    Hash256, Id, Store,
    codec::{Cursor, put_varint},
    error::{Corruption, Result, StoreError},
};
use sha2::{Digest, Sha256};

pub const MAGIC: &[u8; 5] = b"\0RDN3";
/// Children per mid, and mids per table.
pub const FANOUT: usize = 64;

const KIND_LEAF: u8 = b'L';
const KIND_MID: u8 = b'M';
const KIND_TABLE: u8 = b'T';
const KIND_ROOT: u8 = b'R';
const KIND_EXTRAS: u8 = b'X';

/// One row as the tree records it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Entry {
    /// Relative to the database root, `/`-separated: `<table>/<file>`.
    pub path: String,
    /// The SHA-256 of the row's canonical form.
    pub row_hash: Hash256,
    /// The id of the file's original bytes.
    pub blob: Id,
}

/// A non-empty child of a mid or table: its object id and its identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Child {
    pub id: Id,
    pub identity: Hash256,
}

pub type Children = [Option<Child>; FANOUT];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    /// Strictly ascending by path; never empty.
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mid {
    /// At least one is present.
    pub children: Children,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableNode {
    pub schema: Option<Hash256>,
    pub mids: Children,
}

/// A table as the root records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub name: String,
    pub id: Id,
    pub identity: Hash256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub format: Option<Hash256>,
    pub config: Option<Hash256>,
    pub extras: Option<Id>,
    /// Strictly ascending by name.
    pub tables: Vec<TableRef>,
}

/// Files kept with a state that are not valid rows, by path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extras {
    /// Strictly ascending by path; never empty.
    pub entries: Vec<(String, Id)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Leaf(Leaf),
    Mid(Mid),
    Table(TableNode),
    Root(Root),
    Extras(Extras),
}

/// Whether `path` may name a file in a tree: relative, `/`-separated, no
/// empty, `.` or `..` component, no NUL (which the identities use as a
/// separator), no `\` (a separator on Windows, where it could climb out of
/// the root on restore), and no `:` in its first component (where Windows
/// would read a drive).
pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\0', '\\'])
        && path
            .split('/')
            .all(|component| !matches!(component, "" | "." | ".."))
        && !path
            .split('/')
            .next()
            .is_some_and(|first| first.contains(':'))
}

/// Whether `name` may name a table: a single valid path component.
pub fn valid_table_name(name: &str) -> bool {
    valid_path(name) && !name.contains(['/', ':'])
}

/// The table a row path belongs to: its first component, when what follows
/// is non-empty.
pub fn table_of(path: &str) -> Option<&str> {
    let (table, rest) = path.split_once('/')?;
    (!rest.is_empty()).then_some(table)
}

/// The (mid, leaf) a path's row lives in.
pub fn bucket(path: &str) -> (usize, usize) {
    let digest = Sha256::digest(path.as_bytes());
    let bits = (usize::from(digest[0]) << 4) | (usize::from(digest[1]) >> 4);
    (bits >> 6, bits & (FANOUT - 1))
}

fn header(kind: u8) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.push(kind);
    out
}

fn put_str(out: &mut Vec<u8>, text: &str) {
    put_varint(out, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

fn put_children(out: &mut Vec<u8>, children: &Children) {
    for child in children {
        match child {
            Some(child) => {
                out.extend_from_slice(&child.id.0);
                out.extend_from_slice(&child.identity.0);
            }
            None => out.extend_from_slice(&[0; 64]),
        }
    }
}

fn opt_hash(value: Option<Hash256>) -> [u8; 32] {
    value.map_or([0; 32], |h| h.0)
}

fn children_identities(hasher: &mut Sha256, children: &Children) {
    for child in children {
        hasher.update(child.map_or([0; 32], |c| c.identity.0));
    }
}

/// The identity of a leaf holding these (path, row hash) pairs, in order.
pub fn leaf_identity<'a>(entries: impl IntoIterator<Item = (&'a str, &'a Hash256)>) -> Hash256 {
    let mut hasher = Sha256::new();
    hasher.update(b"reldir-leaf-v3\0");
    for (path, row_hash) in entries {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(row_hash.0);
    }
    Hash256(hasher.finalize().into())
}

/// The identity of a table with no rows.
pub fn empty_table_identity(schema: Option<Hash256>) -> Hash256 {
    TableNode {
        schema,
        mids: [None; FANOUT],
    }
    .identity()
}

impl Leaf {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = header(KIND_LEAF);
        for entry in &self.entries {
            put_str(&mut out, &entry.path);
            out.extend_from_slice(&entry.row_hash.0);
            out.extend_from_slice(&entry.blob.0);
        }
        out
    }

    pub fn identity(&self) -> Hash256 {
        leaf_identity(self.entries.iter().map(|e| (e.path.as_str(), &e.row_hash)))
    }

    pub fn find(&self, path: &str) -> Option<&Entry> {
        self.entries
            .binary_search_by(|e| e.path.as_str().cmp(path))
            .ok()
            .map(|at| &self.entries[at])
    }
}

impl Mid {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = header(KIND_MID);
        put_children(&mut out, &self.children);
        out
    }

    pub fn identity(&self) -> Hash256 {
        let mut hasher = Sha256::new();
        hasher.update(b"reldir-mid-v3\0");
        children_identities(&mut hasher, &self.children);
        Hash256(hasher.finalize().into())
    }
}

impl TableNode {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = header(KIND_TABLE);
        out.extend_from_slice(&opt_hash(self.schema));
        put_children(&mut out, &self.mids);
        out
    }

    pub fn identity(&self) -> Hash256 {
        let mut hasher = Sha256::new();
        hasher.update(b"reldir-table-v3\0");
        hasher.update(opt_hash(self.schema));
        children_identities(&mut hasher, &self.mids);
        Hash256(hasher.finalize().into())
    }
}

impl Root {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = header(KIND_ROOT);
        out.extend_from_slice(&opt_hash(self.format));
        out.extend_from_slice(&opt_hash(self.config));
        out.extend_from_slice(&self.extras.map_or([0; 32], |id| id.0));
        for table in &self.tables {
            put_str(&mut out, &table.name);
            out.extend_from_slice(&table.id.0);
            out.extend_from_slice(&table.identity.0);
        }
        out
    }

    /// The identity of the state, from the table identities this root records.
    pub fn identity(&self) -> Hash256 {
        state_identity(
            self.format,
            self.config,
            self.tables.iter().map(|t| (t.name.as_str(), &t.identity)),
        )
    }

    pub fn table(&self, name: &str) -> Option<&TableRef> {
        self.tables
            .binary_search_by(|t| t.name.as_str().cmp(name))
            .ok()
            .map(|at| &self.tables[at])
    }
}

/// The identity of a state with these parts, tables in name order.
pub fn state_identity<'a>(
    format: Option<Hash256>,
    config: Option<Hash256>,
    tables: impl IntoIterator<Item = (&'a str, &'a Hash256)>,
) -> Hash256 {
    let mut hasher = Sha256::new();
    hasher.update(b"reldir-state-v3\0");
    hasher.update(opt_hash(format));
    hasher.update(opt_hash(config));
    for (name, identity) in tables {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(identity.0);
    }
    Hash256(hasher.finalize().into())
}

impl Extras {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = header(KIND_EXTRAS);
        for (path, blob) in &self.entries {
            put_str(&mut out, path);
            out.extend_from_slice(&blob.0);
        }
        out
    }
}

impl Node {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Node::Leaf(n) => n.encode(),
            Node::Mid(n) => n.encode(),
            Node::Table(n) => n.encode(),
            Node::Root(n) => n.encode(),
            Node::Extras(n) => n.encode(),
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Node::Leaf(_) => "leaf",
            Node::Mid(_) => "mid",
            Node::Table(_) => "table",
            Node::Root(_) => "root",
            Node::Extras(_) => "extras",
        }
    }

    /// Decode exactly the bytes [`Node::encode`] produces.
    pub fn decode(bytes: &[u8]) -> Result<Node, Corruption> {
        let mut cursor = Cursor::new(bytes);
        if &cursor.array::<5>().map_err(|_| Corruption::BadMagic)? != MAGIC {
            return Err(Corruption::BadMagic);
        }
        let node = match cursor.u8()? {
            KIND_LEAF => {
                let mut entries: Vec<Entry> = vec![];
                while !cursor.is_empty() {
                    let path = decode_path(&mut cursor, valid_path)?;
                    if entries.last().is_some_and(|last| last.path >= path) {
                        return Err(Corruption::UnsortedEntries(path));
                    }
                    let row_hash = Hash256(cursor.array()?);
                    let blob = Id(cursor.array()?);
                    entries.push(Entry {
                        path,
                        row_hash,
                        blob,
                    });
                }
                if entries.is_empty() {
                    return Err(Corruption::EmptyNode("leaf"));
                }
                Node::Leaf(Leaf { entries })
            }
            KIND_MID => {
                let children = decode_children(&mut cursor)?;
                if children.iter().all(Option::is_none) {
                    return Err(Corruption::EmptyNode("mid"));
                }
                Node::Mid(Mid { children })
            }
            KIND_TABLE => {
                let schema = decode_opt_hash(&mut cursor)?;
                let mids = decode_children(&mut cursor)?;
                Node::Table(TableNode { schema, mids })
            }
            KIND_ROOT => {
                let format = decode_opt_hash(&mut cursor)?;
                let config = decode_opt_hash(&mut cursor)?;
                let extras = Id(cursor.array()?);
                let extras = (!extras.is_zero()).then_some(extras);
                let mut tables: Vec<TableRef> = vec![];
                while !cursor.is_empty() {
                    let name = decode_path(&mut cursor, valid_table_name)?;
                    if tables.last().is_some_and(|last| last.name >= name) {
                        return Err(Corruption::UnsortedEntries(name));
                    }
                    let id = Id(cursor.array()?);
                    let identity = Hash256(cursor.array()?);
                    if id.is_zero() || identity.is_zero() {
                        return Err(Corruption::HalfEmptyChild {
                            index: tables.len(),
                        });
                    }
                    tables.push(TableRef { name, id, identity });
                }
                Node::Root(Root {
                    format,
                    config,
                    extras,
                    tables,
                })
            }
            KIND_EXTRAS => {
                let mut entries: Vec<(String, Id)> = vec![];
                while !cursor.is_empty() {
                    let path = decode_path(&mut cursor, valid_path)?;
                    if entries.last().is_some_and(|(last, _)| *last >= path) {
                        return Err(Corruption::UnsortedEntries(path));
                    }
                    entries.push((path, Id(cursor.array()?)));
                }
                if entries.is_empty() {
                    return Err(Corruption::EmptyNode("extras"));
                }
                Node::Extras(Extras { entries })
            }
            other => return Err(Corruption::UnknownNodeKind(other)),
        };
        cursor.finish()?;
        Ok(node)
    }
}

fn decode_path(cursor: &mut Cursor<'_>, valid: fn(&str) -> bool) -> Result<String, Corruption> {
    let bytes = cursor.length_prefixed()?;
    let text = std::str::from_utf8(bytes).map_err(|_| Corruption::InvalidUtf8)?;
    if !valid(text) {
        return Err(Corruption::IllegalPath(text.to_string()));
    }
    Ok(text.to_string())
}

fn decode_opt_hash(cursor: &mut Cursor<'_>) -> Result<Option<Hash256>, Corruption> {
    let hash = Hash256(cursor.array()?);
    Ok((!hash.is_zero()).then_some(hash))
}

fn decode_children(cursor: &mut Cursor<'_>) -> Result<Children, Corruption> {
    let mut children = [None; FANOUT];
    for (index, slot) in children.iter_mut().enumerate() {
        let id = Id(cursor.array()?);
        let identity = Hash256(cursor.array()?);
        *slot = match (id.is_zero(), identity.is_zero()) {
            (true, true) => None,
            (false, false) => Some(Child { id, identity }),
            _ => return Err(Corruption::HalfEmptyChild { index }),
        };
    }
    Ok(children)
}

/// Read and decode a node.
pub fn load(store: &Store<'_>, id: &Id) -> Result<Node> {
    let object = store.get(id)?;
    Node::decode(&object.bytes).map_err(|what| StoreError::CorruptObject { id: *id, what })
}

fn unexpected(id: &Id, expected: &'static str, node: &Node) -> StoreError {
    StoreError::CorruptObject {
        id: *id,
        what: Corruption::UnexpectedNode {
            expected,
            found: node.kind_name(),
        },
    }
}

macro_rules! typed_loader {
    ($(#[$doc:meta])* $name:ident, $variant:ident, $type:ty, $label:literal) => {
        $(#[$doc])*
        pub fn $name(store: &Store<'_>, id: &Id) -> Result<$type> {
            match load(store, id)? {
                Node::$variant(node) => Ok(node),
                other => Err(unexpected(id, $label, &other)),
            }
        }
    };
}

typed_loader!(
    /// Read a root node.
    load_root, Root, Root, "root"
);
typed_loader!(
    /// Read an extras node.
    load_extras, Extras, Extras, "extras"
);
typed_loader!(load_table_node, Table, TableNode, "table");
typed_loader!(load_mid_node, Mid, Mid, "mid");
typed_loader!(load_leaf_node, Leaf, Leaf, "leaf");

fn mismatch(id: &Id, recorded: Hash256, computed: Hash256) -> StoreError {
    StoreError::CorruptObject {
        id: *id,
        what: Corruption::IdentityMismatch { recorded, computed },
    }
}

/// Read a table node and prove it has the identity its root records.
pub fn load_table(store: &Store<'_>, table: &TableRef) -> Result<TableNode> {
    let node = load_table_node(store, &table.id)?;
    let computed = node.identity();
    if computed != table.identity {
        return Err(mismatch(&table.id, table.identity, computed));
    }
    Ok(node)
}

/// Read a mid and prove it has the identity its table records.
pub fn load_mid(store: &Store<'_>, child: &Child) -> Result<Mid> {
    let node = load_mid_node(store, &child.id)?;
    let computed = node.identity();
    if computed != child.identity {
        return Err(mismatch(&child.id, child.identity, computed));
    }
    Ok(node)
}

/// Read a leaf and prove it has the identity its mid records and that every
/// row in it belongs to table `table`, bucket (`mid`, `leaf`).
pub fn load_leaf(
    store: &Store<'_>,
    child: &Child,
    table: &str,
    mid: usize,
    leaf: usize,
) -> Result<Leaf> {
    let node = load_leaf_node(store, &child.id)?;
    if let Some(entry) = node
        .entries
        .iter()
        .find(|e| !placed(&e.path, table, mid, leaf))
    {
        return Err(StoreError::CorruptObject {
            id: child.id,
            what: Corruption::Misplaced {
                path: entry.path.clone(),
            },
        });
    }
    let computed = node.identity();
    if computed != child.identity {
        return Err(mismatch(&child.id, child.identity, computed));
    }
    Ok(node)
}

/// Whether a row at `path` belongs in table `table` at bucket (`mid`, `leaf`).
pub fn placed(path: &str, table: &str, mid: usize, leaf: usize) -> bool {
    table_of(path) == Some(table) && bucket(path) == (mid, leaf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn entry(path: &str, row: u8, blob: u8) -> Entry {
        Entry {
            path: path.into(),
            row_hash: Hash256::of(&[row]),
            blob: Id::of(&[blob]),
        }
    }

    fn child(n: u8) -> Child {
        Child {
            id: Id::of(&[n, 1]),
            identity: Hash256::of(&[n, 2]),
        }
    }

    fn samples() -> Vec<Node> {
        let mut children = [None; FANOUT];
        children[3] = Some(child(3));
        children[63] = Some(child(63));
        vec![
            Node::Leaf(Leaf {
                entries: vec![entry("t/a.json", 1, 1), entry("t/b.json", 2, 2)],
            }),
            Node::Mid(Mid { children }),
            Node::Table(TableNode {
                schema: Some(Hash256::of(b"schema")),
                mids: children,
            }),
            Node::Table(TableNode {
                schema: None,
                mids: [None; FANOUT],
            }),
            Node::Root(Root {
                format: Some(Hash256::of(b"f")),
                config: None,
                extras: Some(Id::of(b"x")),
                tables: vec![
                    TableRef {
                        name: "a".into(),
                        id: Id::of(b"a"),
                        identity: Hash256::of(b"a"),
                    },
                    TableRef {
                        name: "b".into(),
                        id: Id::of(b"b"),
                        identity: Hash256::of(b"b"),
                    },
                ],
            }),
            Node::Root(Root {
                format: None,
                config: None,
                extras: None,
                tables: vec![],
            }),
            Node::Extras(Extras {
                entries: vec![("t/bad.json".into(), Id::of(b"bad"))],
            }),
        ]
    }

    #[test]
    fn test8240_every_node_kind_round_trips_through_its_encoding() {
        for node in samples() {
            let bytes = node.encode();
            assert!(bytes.starts_with(MAGIC));
            assert_eq!(Node::decode(&bytes).unwrap(), node);
        }
    }

    #[test]
    fn test8241_non_canonical_encodings_are_refused() {
        let leaf = Leaf {
            entries: vec![entry("t/a.json", 1, 1), entry("t/b.json", 2, 2)],
        };
        let reversed = Leaf {
            entries: vec![entry("t/b.json", 2, 2), entry("t/a.json", 1, 1)],
        };
        assert_eq!(
            Node::decode(&reversed.encode()).unwrap_err(),
            Corruption::UnsortedEntries("t/a.json".into())
        );
        let duplicated = Leaf {
            entries: vec![entry("t/a.json", 1, 1), entry("t/a.json", 1, 1)],
        };
        assert_eq!(
            Node::decode(&duplicated.encode()).unwrap_err(),
            Corruption::UnsortedEntries("t/a.json".into())
        );
        assert_eq!(
            Node::decode(&Leaf { entries: vec![] }.encode()).unwrap_err(),
            Corruption::EmptyNode("leaf")
        );
        assert_eq!(
            Node::decode(
                &Mid {
                    children: [None; FANOUT]
                }
                .encode()
            )
            .unwrap_err(),
            Corruption::EmptyNode("mid")
        );
        assert_eq!(
            Node::decode(&Extras { entries: vec![] }.encode()).unwrap_err(),
            Corruption::EmptyNode("extras")
        );
        // In a leaf, a stray byte reads as the start of another entry.
        let mut trailing = leaf.encode();
        trailing.push(0);
        assert_eq!(
            Node::decode(&trailing).unwrap_err(),
            Corruption::IllegalPath(String::new())
        );
        trailing.pop();
        trailing.push(1);
        assert_eq!(Node::decode(&trailing).unwrap_err(), Corruption::Truncated);
        let mut half = Mid {
            children: [Some(child(1)); FANOUT],
        }
        .encode();
        // Zero the identity of child 5 only.
        let at = 6 + 5 * 64 + 32;
        half[at..at + 32].fill(0);
        assert_eq!(
            Node::decode(&half).unwrap_err(),
            Corruption::HalfEmptyChild { index: 5 }
        );
        let mut mid_trailing = Mid {
            children: [Some(child(1)); FANOUT],
        }
        .encode();
        mid_trailing.push(7);
        assert_eq!(
            Node::decode(&mid_trailing).unwrap_err(),
            Corruption::TrailingBytes(1)
        );
        assert_eq!(
            Node::decode(b"\0RDN3Q").unwrap_err(),
            Corruption::UnknownNodeKind(b'Q')
        );
        assert_eq!(Node::decode(b"\0RDN2L").unwrap_err(), Corruption::BadMagic);
        assert_eq!(Node::decode(b"").unwrap_err(), Corruption::BadMagic);
    }

    #[test]
    fn test8242_paths_that_could_escape_or_confuse_are_refused_on_decode() {
        for bad in [
            "",
            "/abs",
            "t//a",
            "t/../../etc/passwd",
            "t/./a",
            "t/a/",
            "t\\a",
            "t/a\0b",
            "C:x/y",
            "..",
        ] {
            assert!(!valid_path(bad), "{bad:?}");
            let leaf = Leaf {
                entries: vec![entry(bad, 1, 1)],
            };
            assert!(
                matches!(Node::decode(&leaf.encode()).unwrap_err(), Corruption::IllegalPath(p) if p == bad),
                "{bad:?}"
            );
        }
        let mut invalid_utf8 = header(KIND_LEAF);
        invalid_utf8.extend_from_slice(&[2, 0xff, 0xfe]);
        invalid_utf8.extend_from_slice(&[0; 64]);
        assert_eq!(
            Node::decode(&invalid_utf8).unwrap_err(),
            Corruption::InvalidUtf8
        );
        for good in ["t/a.json", "t/sub/a b.json", "t/a:b.json", "é/ü.json"] {
            assert!(valid_path(good), "{good:?}");
        }
        for bad_table in ["a/b", "a:b", "", ".", ".."] {
            assert!(!valid_table_name(bad_table), "{bad_table:?}");
        }
        assert_eq!(table_of("users/u1.json"), Some("users"));
        assert_eq!(table_of("users/"), None);
        assert_eq!(table_of("users"), None);
    }

    #[test]
    fn test8243_identities_exclude_blobs_and_include_everything_logical() {
        let a = Leaf {
            entries: vec![entry("t/a.json", 1, 1)],
        };
        let reformatted = Leaf {
            entries: vec![entry("t/a.json", 1, 9)],
        };
        assert_eq!(a.identity(), reformatted.identity(), "a blob is formatting");
        assert_ne!(
            Id::of(&a.encode()),
            Id::of(&reformatted.encode()),
            "but bytes are not"
        );
        let changed = Leaf {
            entries: vec![entry("t/a.json", 2, 1)],
        };
        assert_ne!(a.identity(), changed.identity());
        let renamed = Leaf {
            entries: vec![entry("t/b.json", 1, 1)],
        };
        assert_ne!(a.identity(), renamed.identity());
        // The identity is the documented digest.
        let mut expected = b"reldir-leaf-v3\0t/a.json\0".to_vec();
        expected.extend_from_slice(&Hash256::of(&[1]).0);
        assert_eq!(a.identity(), Hash256::of(&expected));
        // An empty table's identity is the documented digest.
        let mut empty = b"reldir-table-v3\0".to_vec();
        empty.extend_from_slice(&[0; 32]);
        empty.extend_from_slice(&[0; 32 * FANOUT]);
        assert_eq!(empty_table_identity(None), Hash256::of(&empty));
        assert_ne!(
            empty_table_identity(None),
            empty_table_identity(Some(Hash256::of(b"s"))),
            "the schema is part of a table's identity"
        );
        // The root's identity ignores extras and table object ids.
        let root = |extras: Option<Id>, id: Id| Root {
            format: None,
            config: Some(Hash256::of(b"c")),
            extras,
            tables: vec![TableRef {
                name: "t".into(),
                id,
                identity: Hash256::of(b"t"),
            }],
        };
        assert_eq!(
            root(None, Id::of(b"1")).identity(),
            root(Some(Id::of(b"x")), Id::of(b"2")).identity()
        );
        let mut state = b"reldir-state-v3\0".to_vec();
        state.extend_from_slice(&[0; 32]);
        state.extend_from_slice(&Hash256::of(b"c").0);
        state.extend_from_slice(b"t\0");
        state.extend_from_slice(&Hash256::of(b"t").0);
        assert_eq!(root(None, Id::of(b"1")).identity(), Hash256::of(&state));
    }

    #[test]
    fn test8244_buckets_are_the_first_twelve_bits_of_the_path_hash() {
        let digest = Sha256::digest(b"users/u1.json");
        let bits = ((digest[0] as usize) << 4) | ((digest[1] as usize) >> 4);
        assert_eq!(bucket("users/u1.json"), (bits / 64, bits % 64));
        let mut seen = std::collections::HashSet::new();
        // 100,000 paths over 4,096 buckets leave one empty with probability
        // about 1e-7.
        for n in 0..100_000 {
            let (mid, leaf) = bucket(&format!("t/{n}.json"));
            assert!(mid < FANOUT && leaf < FANOUT);
            seen.insert((mid, leaf));
        }
        assert_eq!(seen.len(), FANOUT * FANOUT, "every bucket is reachable");
    }

    proptest! {
        #[test]
        fn test8245_arbitrary_bytes_after_the_magic_never_panic_the_decoder(
            kind in prop::sample::select(vec![b'L', b'M', b'T', b'R', b'X', b'?']),
            body in prop::collection::vec(any::<u8>(), 0..300),
        ) {
            let mut bytes = MAGIC.to_vec();
            bytes.push(kind);
            bytes.extend_from_slice(&body);
            if let Ok(node) = Node::decode(&bytes) {
                prop_assert_eq!(node.encode(), bytes, "whatever decodes re-encodes to itself");
            }
        }
    }
}
