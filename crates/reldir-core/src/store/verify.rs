//! Proving a history intact.
//!
//! [`verify_head`] is the check every open can afford: the head revision
//! parses, its root node is present and well-formed, and the root's identity
//! is the state the revision records -- two object reads, each hash-checked.
//!
//! [`verify_full`] proves everything: every pack byte for byte against its
//! index (so every stored object against its id, reachable or not), then
//! every revision reachable from the heads, and every tree recomputed from
//! its leaves up -- each identity a parent records checked against the one
//! its child actually has, each row checked to sit in its table and bucket,
//! each blob checked to be stored.
//!
//! Damage is reported as [`Finding`]s, all of them, rather than stopping at
//! the first; only a failure to read at all (an I/O error) is an `Err`.

use super::{
    Hash256, Id, Store,
    error::{Corruption, Result, StoreError},
    node::{self, Child, Children, FANOUT, TableNode, placed, state_identity},
    revision,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
};

/// One thing wrong with a history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// A pack or index failed byte-for-byte verification.
    Pack { path: PathBuf, what: Corruption },
    /// An object is damaged in its pack, or is not the object it is used as.
    Corrupt {
        id: Id,
        what: Corruption,
        /// The pack, when the damage is in the pack rather than the object.
        path: Option<PathBuf>,
    },
    /// An object that something names is in no pack.
    Missing { id: Id, referenced_by: Option<Id> },
    /// A node records an identity for a child that the child does not have.
    IdentityMismatch {
        parent: Id,
        child: Id,
        recorded: Hash256,
        computed: Hash256,
    },
    /// A revision's state is not the identity of its tree.
    StateMismatch {
        revision: Id,
        recorded: Hash256,
        computed: Hash256,
    },
    /// A row stored in a table or bucket its path does not belong to.
    Misplaced { leaf: Id, path: String },
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Finding::Pack { path, what } => write!(f, "pack {}: {what}", path.display()),
            Finding::Corrupt { id, what, path } => match path {
                Some(path) => write!(f, "object {id} in {}: {what}", path.display()),
                None => write!(f, "object {id}: {what}"),
            },
            Finding::Missing { id, referenced_by } => match referenced_by {
                Some(parent) => write!(f, "object {id}, named by {parent}, is in no pack"),
                None => write!(f, "object {id} is in no pack"),
            },
            Finding::IdentityMismatch {
                parent,
                child,
                recorded,
                computed,
            } => write!(
                f,
                "node {parent} records identity {recorded} for child {child}, whose identity is {computed}"
            ),
            Finding::StateMismatch {
                revision,
                recorded,
                computed,
            } => write!(
                f,
                "revision {revision} records state {recorded}, and its tree's identity is {computed}"
            ),
            Finding::Misplaced { leaf, path } => {
                write!(f, "leaf {leaf} holds {path:?}, which belongs elsewhere")
            }
        }
    }
}

/// What [`verify_full`] examined, and what it found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub packs: usize,
    /// Object records across all packs.
    pub objects: usize,
    pub revisions: usize,
    pub nodes: usize,
    pub blobs: usize,
}

impl Report {
    pub fn is_intact(&self) -> bool {
        self.findings.is_empty()
    }
}

/// A store error about an object, as a finding; any other error stays one.
fn finding(error: StoreError, id: Id, referenced_by: Option<Id>) -> Result<Finding> {
    match error {
        StoreError::Missing(missing) => Ok(Finding::Missing {
            id: missing,
            referenced_by,
        }),
        StoreError::CorruptObject { id, what } => Ok(Finding::Corrupt {
            id,
            what,
            path: None,
        }),
        StoreError::Corrupt { what, path } => Ok(Finding::Corrupt {
            id,
            what,
            path: Some(path),
        }),
        other => Err(other),
    }
}

/// The head revision parses, its tree's root is intact, and the root's
/// identity is the recorded state. Two object reads.
pub fn verify_head(store: &Store<'_>, head: &Id) -> Result<Vec<Finding>> {
    let revision = match revision::load(store, head) {
        Ok(revision) => revision,
        Err(error) => return Ok(vec![finding(error, *head, None)?]),
    };
    let root = match node::load_root(store, &revision.tree) {
        Ok(root) => root,
        Err(error) => return Ok(vec![finding(error, revision.tree, Some(*head))?]),
    };
    let computed = root.identity();
    if computed != revision.state {
        return Ok(vec![Finding::StateMismatch {
            revision: *head,
            recorded: revision.state,
            computed,
        }]);
    }
    Ok(vec![])
}

/// Everything, from the packs up. See the module documentation.
pub fn verify_full(store: &Store<'_>, heads: &[Id]) -> Result<Report> {
    store.refresh()?;
    let mut walk = Walk {
        store,
        report: Report::default(),
        roots: HashMap::new(),
        tables: HashMap::new(),
        mids: HashMap::new(),
        leaves: HashMap::new(),
        extras: HashSet::new(),
        nodes: HashSet::new(),
        blobs: HashSet::new(),
    };
    for pack in store.packs() {
        walk.report.packs += 1;
        walk.report.objects += pack.index.len();
        match store.verify_pack(&pack) {
            Ok(()) => {}
            Err(StoreError::Corrupt { what, path }) => {
                walk.report.findings.push(Finding::Pack { path, what })
            }
            Err(error) => return Err(error),
        }
    }
    let mut seen = HashSet::new();
    let mut queue: VecDeque<(Id, Option<Id>)> = heads.iter().map(|h| (*h, None)).collect();
    while let Some((id, child)) = queue.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        walk.report.revisions += 1;
        let revision = match revision::load(store, &id) {
            Ok(revision) => revision,
            Err(error) => {
                let found = finding(error, id, child)?;
                walk.report.findings.push(found);
                continue;
            }
        };
        queue.extend(revision.parents.iter().map(|p| (*p, Some(id))));
        if let Some(computed) = walk.root(&revision.tree, id)?
            && computed != revision.state
        {
            walk.report.findings.push(Finding::StateMismatch {
                revision: id,
                recorded: revision.state,
                computed,
            });
        }
    }
    walk.report.nodes = walk.nodes.len();
    walk.report.blobs = walk.blobs.len();
    Ok(walk.report)
}

/// A node's identity two ways: as its own bytes claim it (from the child
/// identities it records), and as it truly is (recomputed from the leaves
/// up). A parent's record is checked against the claim, so each broken link
/// is reported once, where it is; a revision's state is checked against the
/// truth, so a lie anywhere below is caught at the top. The truth is `None`
/// when something below could not be read.
#[derive(Debug, Clone, Copy)]
struct Identities {
    claimed: Hash256,
    truth: Option<Hash256>,
}

/// The tree walk, remembering each node's identities by the position it was
/// proven in -- a node is verified once per place it is used, since
/// placement is part of what is proven.
struct Walk<'s, 'a> {
    store: &'s Store<'a>,
    report: Report,
    roots: HashMap<Id, Option<Identities>>,
    tables: HashMap<(String, Id), Option<Identities>>,
    mids: HashMap<(String, usize, Id), Option<Identities>>,
    leaves: HashMap<(String, usize, usize, Id), Option<Identities>>,
    extras: HashSet<Id>,
    nodes: HashSet<Id>,
    blobs: HashSet<Id>,
}

impl Walk<'_, '_> {
    /// Record a failure to load `id`, named by `parent`.
    fn failed(&mut self, error: StoreError, id: Id, parent: Id) -> Result<()> {
        let found = finding(error, id, Some(parent))?;
        self.report.findings.push(found);
        Ok(())
    }

    fn blob(&mut self, blob: Id, parent: Id) -> Result<()> {
        if self.blobs.insert(blob) && !self.store.contains(&blob)? {
            self.report.findings.push(Finding::Missing {
                id: blob,
                referenced_by: Some(parent),
            });
        }
        Ok(())
    }

    /// Check what `parent` records for `child` against what the child
    /// claims, and return the child's true identity.
    fn link(&mut self, parent: Id, child: &Child, found: Option<Identities>) -> Option<Hash256> {
        let found = found?;
        if found.claimed != child.identity {
            self.report.findings.push(Finding::IdentityMismatch {
                parent,
                child: child.id,
                recorded: child.identity,
                computed: found.claimed,
            });
        }
        found.truth
    }

    /// Children with each identity replaced by the true one; `None` if any
    /// is unknown.
    fn true_children(children: &Children, truths: &[Option<Hash256>]) -> Option<Children> {
        let mut out = [None; FANOUT];
        for (index, child) in children.iter().enumerate() {
            if let Some(child) = child {
                out[index] = Some(Child {
                    id: child.id,
                    identity: truths[index]?,
                });
            }
        }
        Some(out)
    }

    /// The true identity of the state rooted at `id`.
    fn root(&mut self, id: &Id, revision: Id) -> Result<Option<Hash256>> {
        if let Some(known) = self.roots.get(id) {
            return Ok(known.and_then(|i| i.truth));
        }
        let found = self.root_uncached(id, revision)?;
        self.roots.insert(*id, found);
        Ok(found.and_then(|i| i.truth))
    }

    fn root_uncached(&mut self, id: &Id, revision: Id) -> Result<Option<Identities>> {
        let root = match node::load_root(self.store, id) {
            Ok(root) => root,
            Err(error) => {
                self.failed(error, *id, revision)?;
                return Ok(None);
            }
        };
        self.nodes.insert(*id);
        if let Some(extras) = root.extras
            && self.extras.insert(extras)
        {
            match node::load_extras(self.store, &extras) {
                Ok(node) => {
                    self.nodes.insert(extras);
                    for (_, blob) in node.entries {
                        self.blob(blob, extras)?;
                    }
                }
                Err(error) => self.failed(error, extras, *id)?,
            }
        }
        let mut truths = vec![];
        for table in &root.tables {
            let found = self.table(&table.name, &table.id, *id)?;
            let as_child = Child {
                id: table.id,
                identity: table.identity,
            };
            truths.push(self.link(*id, &as_child, found));
        }
        let truth = truths
            .into_iter()
            .collect::<Option<Vec<Hash256>>>()
            .map(|truths| {
                state_identity(
                    root.format,
                    root.config,
                    root.tables
                        .iter()
                        .zip(&truths)
                        .map(|(t, identity)| (t.name.as_str(), identity)),
                )
            });
        Ok(Some(Identities {
            claimed: root.identity(),
            truth,
        }))
    }

    fn table(&mut self, name: &str, id: &Id, root: Id) -> Result<Option<Identities>> {
        let key = (name.to_string(), *id);
        if let Some(known) = self.tables.get(&key) {
            return Ok(*known);
        }
        let found = match node::load_table_node(self.store, id) {
            Ok(table) => {
                self.nodes.insert(*id);
                let mut truths = vec![];
                for (index, mid) in table.mids.iter().enumerate() {
                    truths.push(match mid {
                        Some(mid) => {
                            let found = self.mid(name, index, mid, *id)?;
                            self.link(*id, mid, found)
                        }
                        None => None,
                    });
                }
                let truth = Self::true_children(&table.mids, &truths).map(|mids| {
                    TableNode {
                        schema: table.schema,
                        mids,
                    }
                    .identity()
                });
                Some(Identities {
                    claimed: table.identity(),
                    truth,
                })
            }
            Err(error) => {
                self.failed(error, *id, root)?;
                None
            }
        };
        self.tables.insert(key, found);
        Ok(found)
    }

    fn mid(
        &mut self,
        table: &str,
        index: usize,
        child: &Child,
        parent: Id,
    ) -> Result<Option<Identities>> {
        let key = (table.to_string(), index, child.id);
        if let Some(known) = self.mids.get(&key) {
            return Ok(*known);
        }
        let found = match node::load_mid_node(self.store, &child.id) {
            Ok(mid) => {
                self.nodes.insert(child.id);
                let mut truths = vec![];
                for (leaf_index, leaf) in mid.children.iter().enumerate() {
                    truths.push(match leaf {
                        Some(leaf) => {
                            let found = self.leaf(table, index, leaf_index, leaf, child.id)?;
                            self.link(child.id, leaf, found)
                        }
                        None => None,
                    });
                }
                let truth = Self::true_children(&mid.children, &truths)
                    .map(|children| node::Mid { children }.identity());
                Some(Identities {
                    claimed: mid.identity(),
                    truth,
                })
            }
            Err(error) => {
                self.failed(error, child.id, parent)?;
                None
            }
        };
        self.mids.insert(key, found);
        Ok(found)
    }

    fn leaf(
        &mut self,
        table: &str,
        mid: usize,
        index: usize,
        child: &Child,
        parent: Id,
    ) -> Result<Option<Identities>> {
        let key = (table.to_string(), mid, index, child.id);
        if let Some(known) = self.leaves.get(&key) {
            return Ok(*known);
        }
        let found = match node::load_leaf_node(self.store, &child.id) {
            Ok(leaf) => {
                self.nodes.insert(child.id);
                for entry in &leaf.entries {
                    if !placed(&entry.path, table, mid, index) {
                        self.report.findings.push(Finding::Misplaced {
                            leaf: child.id,
                            path: entry.path.clone(),
                        });
                    }
                    self.blob(entry.blob, child.id)?;
                }
                let identity = leaf.identity();
                Some(Identities {
                    claimed: identity,
                    truth: Some(identity),
                })
            }
            Err(error) => {
                self.failed(error, child.id, parent)?;
                None
            }
        };
        self.leaves.insert(key, found);
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::fs::{Fs, Source};
    use crate::store::{
        Object, ObjectKind,
        dag::tests::History,
        node::{Leaf, Mid, Root, TableRef, bucket},
        tree::tests::{Counting, OBJECTS, State, build, row, save},
    };
    use std::{path::Path, sync::atomic::AtomicUsize};

    /// A history of three revisions whose blobs are all stored.
    fn intact() -> (History, Id) {
        let mut h = History::new();
        let mut parent = None;
        for n in 0u8..3 {
            let mut state = State::default();
            state.tables.insert("t".into(), None);
            for k in 0..=n {
                let e = row(&format!("t/{k}.json"), k, n);
                state.rows.insert(e.path, (e.row_hash, e.blob));
            }
            state.extras.insert("t/bad.txt".into(), Id::of(&[b'x', n]));
            let built = build(&state);
            let blobs: Vec<Object> = (0..=n)
                .map(|k| Object::new(ObjectKind::Blob, vec![k, n]))
                .chain([Object::new(ObjectKind::Blob, vec![b'x', n])])
                .collect();
            save(&h.fs, &blobs);
            save(&h.fs, &built.objects);
            let id = h.commit(parent.as_slice(), built.root, built.identity);
            parent = Some(id);
        }
        (h, parent.unwrap())
    }

    #[test]
    fn test8300_an_intact_history_verifies_and_is_counted() {
        let (h, head) = intact();
        let store = h.store();
        assert_eq!(verify_head(&store, &head).unwrap(), vec![]);
        let report = verify_full(&store, &[head]).unwrap();
        assert!(report.is_intact(), "{:?}", report.findings);
        assert_eq!(report.revisions, 3);
        assert_eq!(
            report.blobs,
            3 + 3 + 3,
            "rows 0..=n per revision, all distinct, plus extras"
        );
        assert!(
            report.nodes >= 3 * 4,
            "a root, extras, table, mid and leaves per revision"
        );
        assert_eq!(report.packs, 3 * 3);
    }

    #[test]
    fn test8301_verify_head_reads_two_objects_and_catches_a_wrong_state() {
        let (mut h, head) = intact();
        let tree = {
            let counting = Counting {
                inner: &h.fs,
                reads: AtomicUsize::new(0),
            };
            let store = Store::open(&counting, Path::new(OBJECTS)).unwrap();
            counting.take();
            assert_eq!(verify_head(&store, &head).unwrap(), vec![]);
            assert_eq!(counting.take(), 2, "the revision and its root");
            revision::load(&store, &head).unwrap().tree
        };
        let lying = h.commit(&[head], tree, Hash256::of(b"some other state"));
        let store = h.store();
        assert!(matches!(
            verify_head(&store, &lying).unwrap()[..],
            [Finding::StateMismatch { revision, recorded, .. }] if revision == lying && recorded == Hash256::of(b"some other state")
        ));
        let report = verify_full(&store, &[lying]).unwrap();
        assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    }

    #[test]
    fn test8302_verify_head_reports_what_it_cannot_read() {
        let (h, head) = intact();
        let store = h.store();
        let nowhere = Id::of(b"nowhere");
        assert_eq!(
            verify_head(&store, &nowhere).unwrap(),
            vec![Finding::Missing {
                id: nowhere,
                referenced_by: None
            }]
        );
        // A head naming bytes that are not a revision.
        save(&h.fs, &[Object::new(ObjectKind::Blob, b"{}".to_vec())]);
        assert!(matches!(
            verify_head(&store, &Id::of(b"{}")).unwrap()[..],
            [Finding::Corrupt {
                what: Corruption::Revision(_),
                path: None,
                ..
            }]
        ));
        // A revision whose tree is missing.
        let mut h = h;
        let orphan = h.commit(&[head], Id::of(b"no tree"), Hash256::of(b"s"));
        let store = h.store();
        assert_eq!(
            verify_head(&store, &orphan).unwrap(),
            vec![Finding::Missing {
                id: Id::of(b"no tree"),
                referenced_by: Some(orphan)
            }]
        );
    }

    #[test]
    fn test8303_full_verification_finds_every_kind_of_damage_at_once() {
        let mut h = History::new();
        let path = "t/a.json";
        let (mid_index, leaf_index) = bucket(path);
        let leaf = Leaf {
            entries: vec![row(path, 1, 1)],
        };
        let leaf_id = Id::of(&leaf.encode());
        // The leaf filed in the wrong bucket, and its mid lying about its
        // identity: misplacement and a mismatch in one tree.
        let mut children = [None; FANOUT];
        children[(leaf_index + 1) % FANOUT] = Some(Child {
            id: leaf_id,
            identity: Hash256::of(b"wrong"),
        });
        let mid = Mid { children };
        let mid_child = Child {
            id: Id::of(&mid.encode()),
            identity: mid.identity(),
        };
        let mut table = TableNode {
            schema: None,
            mids: [None; FANOUT],
        };
        table.mids[mid_index] = Some(mid_child);
        let root = Root {
            format: None,
            config: None,
            extras: None,
            tables: vec![TableRef {
                name: "t".into(),
                id: Id::of(&table.encode()),
                identity: table.identity(),
            }],
        };
        let nodes = [leaf.encode(), mid.encode(), table.encode(), root.encode()]
            .into_iter()
            .map(|b| Object::new(ObjectKind::Node, b))
            .collect::<Vec<_>>();
        save(&h.fs, &nodes);
        let root_id = Id::of(&root.encode());
        // Its revision names a missing parent; its blob is never stored.
        let missing_parent = Id::of(b"lost parent");
        let head = h.commit(&[missing_parent], root_id, root.identity());
        // And an unreachable object damaged in its pack.
        save(
            &h.fs,
            &[Object::new(ObjectKind::Blob, b"unreachable".to_vec())],
        );
        let store = h.store();
        let victim = store
            .packs()
            .into_iter()
            .find(|p| p.index.find(&Id::of(b"unreachable")).is_some())
            .unwrap();
        let mut bytes = h.fs.read(&victim.pack_path).unwrap();
        let at = bytes.windows(11).position(|w| w == b"unreachable").unwrap();
        bytes[at] = b'U';
        h.fs.write(&victim.pack_path, &bytes).unwrap();

        let report = verify_full(&store, &[head]).unwrap();
        let findings = &report.findings;
        assert!(findings.contains(&Finding::Pack {
            path: victim.pack_path.clone(),
            what: Corruption::TrailerMismatch
        }));
        assert!(findings.contains(&Finding::Missing {
            id: missing_parent,
            referenced_by: Some(head)
        }));
        assert!(findings.contains(&Finding::Missing {
            id: Id::of(&[1, 1]),
            referenced_by: Some(leaf_id)
        }));
        assert!(findings.contains(&Finding::Misplaced {
            leaf: leaf_id,
            path: path.into()
        }));
        assert!(findings.contains(&Finding::IdentityMismatch {
            parent: mid_child.id,
            child: leaf_id,
            recorded: Hash256::of(b"wrong"),
            computed: leaf.identity(),
        }));
        // The mid recorded the wrong leaf identity, so the state recomputed
        // from the leaves up is not the one the revision records -- while
        // every link above the mid agrees with the bytes it links to, and is
        // not reported.
        assert!(
            findings
                .iter()
                .any(|f| matches!(f, Finding::StateMismatch { revision, .. } if *revision == head))
        );
        assert_eq!(findings.len(), 6, "{findings:?}");
        // The cheap check sees none of it: the root agrees with itself.
        assert_eq!(verify_head(&store, &head).unwrap(), vec![]);
    }
}
