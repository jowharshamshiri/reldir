//! Walking history: ancestry, merge bases, what is reachable, and where a
//! path was last seen.
//!
//! Revisions form a DAG through their parents. Ids are content hashes, so
//! the DAG cannot hold a cycle; the walks keep a visited set regardless, so
//! each revision is read once however many paths lead to it.

use super::{
    Hash256, Id, Store,
    error::{Invalid, Result, StoreError},
    node::{self, bucket, table_of, valid_path},
    revision::{self, Revision},
};
use chrono::{DateTime, Utc};
use std::collections::{BinaryHeap, HashSet, VecDeque};

/// `from` and every revision it descends from, breadth first, each once.
pub fn ancestors(store: &Store<'_>, from: &Id) -> Result<Vec<Id>> {
    let mut seen = HashSet::from([*from]);
    let mut queue = VecDeque::from([*from]);
    let mut out = vec![];
    while let Some(id) = queue.pop_front() {
        out.push(id);
        for parent in revision::load(store, &id)?.parents {
            if seen.insert(parent) {
                queue.push_back(parent);
            }
        }
    }
    Ok(out)
}

/// Whether `ancestor` is `of` or one of its ancestors.
pub fn is_ancestor(store: &Store<'_>, ancestor: &Id, of: &Id) -> Result<bool> {
    let mut seen = HashSet::from([*of]);
    let mut queue = VecDeque::from([*of]);
    while let Some(id) = queue.pop_front() {
        if id == *ancestor {
            return Ok(true);
        }
        for parent in revision::load(store, &id)?.parents {
            if seen.insert(parent) {
                queue.push_back(parent);
            }
        }
    }
    Ok(false)
}

/// The best common ancestors of `a` and `b`: common ancestors that are not
/// ancestors of another common ancestor. Usually one; several after
/// criss-cross merges; none when the two share no history. Sorted.
pub fn merge_base(store: &Store<'_>, a: &Id, b: &Id) -> Result<Vec<Id>> {
    let of_a: HashSet<Id> = ancestors(store, a)?.into_iter().collect();
    let mut common = HashSet::new();
    let mut parents_of_common = HashSet::new();
    let mut seen = HashSet::from([*b]);
    let mut queue = VecDeque::from([*b]);
    while let Some(id) = queue.pop_front() {
        let parents = revision::load(store, &id)?.parents;
        if of_a.contains(&id) {
            common.insert(id);
            // Every ancestor of a common ancestor is common, so a common
            // ancestor is dominated exactly when it is a parent of another.
            parents_of_common.extend(parents.iter().copied());
        }
        for parent in parents {
            if seen.insert(parent) {
                queue.push_back(parent);
            }
        }
    }
    let mut best: Vec<Id> = common.difference(&parents_of_common).copied().collect();
    best.sort_unstable();
    Ok(best)
}

/// The heads that are not ancestors of another head, sorted and without
/// duplicates. Heads left behind by a crash between creating a new head and
/// removing its parents are exactly the ones this drops.
pub fn prune_ancestors(store: &Store<'_>, heads: &[Id]) -> Result<Vec<Id>> {
    let mut heads = heads.to_vec();
    heads.sort_unstable();
    heads.dedup();
    if heads.len() <= 1 {
        return Ok(heads);
    }
    // One walk from every head's parents together: a head it reaches is a
    // strict ancestor of some head.
    let mut seen = HashSet::new();
    let mut queue = VecDeque::new();
    for head in &heads {
        for parent in revision::load(store, head)?.parents {
            if seen.insert(parent) {
                queue.push_back(parent);
            }
        }
    }
    while let Some(id) = queue.pop_front() {
        for parent in revision::load(store, &id)?.parents {
            if seen.insert(parent) {
                queue.push_back(parent);
            }
        }
    }
    Ok(heads.into_iter().filter(|h| !seen.contains(h)).collect())
}

/// Every object reachable from these revisions (with all their ancestors)
/// and these trees: revisions, every node of their trees, and every blob
/// those nodes name. What garbage collection keeps.
///
/// Nodes are read -- and proven against their parents -- once each; blobs
/// are named, not read, so a missing blob is found by the repack that keeps
/// it, or by verification.
pub fn reachable(store: &Store<'_>, revisions: &[Id], trees: &[Id]) -> Result<HashSet<Id>> {
    let mut out = HashSet::new();
    walk(store, revisions, &mut out, Reach::Whole)?;
    for tree in trees {
        tree_objects(store, tree, &mut out, Reach::Whole)?;
    }
    Ok(out)
}

/// What a quarantined lineage reaches through the live store. Its packs that
/// failed verification were set aside with it, so an object the live store
/// does not hold is one the quarantine keeps: the walk does not pass it.
pub fn reachable_held(store: &Store<'_>, revisions: &[Id]) -> Result<HashSet<Id>> {
    let mut out = HashSet::new();
    walk(store, revisions, &mut out, Reach::Held)?;
    Ok(out)
}

/// Whether a walk proves every object it reaches, or passes those the live
/// store does not hold.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    Whole,
    Held,
}

impl Reach {
    fn passes(self, store: &Store<'_>, id: &Id) -> Result<bool> {
        Ok(self == Reach::Held && !store.contains(id)?)
    }
}

fn walk(store: &Store<'_>, revisions: &[Id], out: &mut HashSet<Id>, reach: Reach) -> Result<()> {
    let mut queue: VecDeque<Id> = revisions.iter().copied().collect();
    while let Some(id) = queue.pop_front() {
        if reach.passes(store, &id)? || !out.insert(id) {
            continue;
        }
        let revision = revision::load(store, &id)?;
        queue.extend(revision.parents);
        tree_objects(store, &revision.tree, out, reach)?;
    }
    Ok(())
}

fn tree_objects(store: &Store<'_>, root: &Id, out: &mut HashSet<Id>, reach: Reach) -> Result<()> {
    if reach.passes(store, root)? || !out.insert(*root) {
        return Ok(());
    }
    let root = node::load_root(store, root)?;
    if let Some(extras) = root.extras
        && !reach.passes(store, &extras)?
        && out.insert(extras)
    {
        out.extend(
            node::load_extras(store, &extras)?
                .entries
                .into_iter()
                .map(|(_, blob)| blob),
        );
    }
    for table in &root.tables {
        if reach.passes(store, &table.id)? || !out.insert(table.id) {
            continue;
        }
        let node = node::load_table(store, table)?;
        for (mid_index, mid) in node.mids.iter().enumerate() {
            let Some(mid) = mid else { continue };
            if reach.passes(store, &mid.id)? || !out.insert(mid.id) {
                continue;
            }
            for (leaf_index, leaf) in node::load_mid(store, mid)?.children.iter().enumerate() {
                let Some(leaf) = leaf else { continue };
                if reach.passes(store, &leaf.id)? || !out.insert(leaf.id) {
                    continue;
                }
                let leaf = node::load_leaf(store, leaf, &table.name, mid_index, leaf_index)?;
                out.extend(leaf.entries.into_iter().map(|e| e.blob));
            }
        }
    }
    Ok(())
}

/// Where a path was last seen in history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastKnown {
    /// The most recent revision, from the head back, whose tree holds the path.
    pub revision: Id,
    pub row_hash: Hash256,
    pub blob: Id,
}

/// Node ids already proven not to hold the path: a revision whose table, mid
/// or leaf for the path has one of these ids does not hold it either, and is
/// passed over without reading further.
#[derive(Default)]
struct Absent {
    tables: HashSet<Id>,
    mids: HashSet<Id>,
    leaves: HashSet<Id>,
}

/// The row at `path` in `tree`, recording what proved it absent.
fn probe(
    store: &Store<'_>,
    tree: &Id,
    table: &str,
    path: &str,
    absent: &mut Absent,
) -> Result<Option<(Hash256, Id)>> {
    let root = node::load_root(store, tree)?;
    let Some(table_ref) = root.table(table) else {
        return Ok(None);
    };
    if absent.tables.contains(&table_ref.id) {
        return Ok(None);
    }
    let (mid_index, leaf_index) = bucket(path);
    let (mut mid_id, mut leaf_id) = (None, None);
    let found = 'search: {
        let Some(mid) = node::load_table(store, table_ref)?.mids[mid_index] else {
            break 'search None;
        };
        if absent.mids.contains(&mid.id) {
            break 'search None;
        }
        mid_id = Some(mid.id);
        let Some(leaf) = node::load_mid(store, &mid)?.children[leaf_index] else {
            break 'search None;
        };
        if absent.leaves.contains(&leaf.id) {
            break 'search None;
        }
        leaf_id = Some(leaf.id);
        node::load_leaf(store, &leaf, table, mid_index, leaf_index)?
            .find(path)
            .map(|e| (e.row_hash, e.blob))
    };
    if found.is_none() {
        absent.tables.insert(table_ref.id);
        absent.mids.extend(mid_id);
        absent.leaves.extend(leaf_id);
    }
    Ok(found)
}

/// The last revision, walking back from `head` newest first by timestamp,
/// whose tree holds `path`, with the row it held there. What restores a row
/// removed by mistake.
///
/// Each revision visited costs its own read and its root's. Below the root,
/// a revision whose table, mid or leaf for the path is a node already proven
/// not to hold it is passed over at once, so the deeper reads are paid only
/// where the path's leaf changed.
pub fn last_known(store: &Store<'_>, head: &Id, path: &str) -> Result<Option<LastKnown>> {
    let table = table_of(path)
        .filter(|_| valid_path(path))
        .ok_or_else(|| StoreError::Invalid(Invalid::Path(path.into())))?;
    let mut heap = BinaryHeap::from([Newest::load(store, *head)?]);
    let mut seen = HashSet::from([*head]);
    let mut absent = Absent::default();
    while let Some(Newest { id, revision, .. }) = heap.pop() {
        if let Some((row_hash, blob)) = probe(store, &revision.tree, table, path, &mut absent)? {
            return Ok(Some(LastKnown {
                revision: id,
                row_hash,
                blob,
            }));
        }
        for parent in revision.parents {
            if seen.insert(parent) {
                heap.push(Newest::load(store, parent)?);
            }
        }
    }
    Ok(None)
}

/// A revision ordered newest first -- by timestamp, ties by id -- for the
/// max-heap [`last_known`] walks.
struct Newest {
    timestamp: DateTime<Utc>,
    id: Id,
    revision: Revision,
}

impl Newest {
    fn load(store: &Store<'_>, id: Id) -> Result<Self> {
        let revision = revision::load(store, &id)?;
        Ok(Self {
            timestamp: revision.timestamp,
            id,
            revision,
        })
    }
}

impl PartialEq for Newest {
    fn eq(&self, other: &Self) -> bool {
        (self.timestamp, self.id) == (other.timestamp, other.id)
    }
}

impl Eq for Newest {}

impl PartialOrd for Newest {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Newest {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.timestamp, self.id).cmp(&(other.timestamp, other.id))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::store::{
        revision::{Origin, Stats},
        test_fs::MemFs,
        tree::{
            TreeUpdate,
            tests::{OBJECTS, State, build, mem, row, save},
        },
    };
    use std::path::Path;

    /// A history built in memory: each commit records a revision over a tree
    /// and publishes its objects.
    pub(crate) struct History {
        pub fs: MemFs,
        pub clock: i64,
    }

    impl History {
        pub fn new() -> Self {
            Self {
                fs: mem(),
                clock: 1_790_000_000,
            }
        }

        pub fn commit(&mut self, parents: &[Id], tree: Id, identity: Hash256) -> Id {
            self.clock += 1;
            let revision = Revision {
                parents: parents.to_vec(),
                state: identity,
                tree,
                timestamp: DateTime::from_timestamp(self.clock, 0).unwrap(),
                origin: Origin::Internal,
                transaction_id: None,
                idempotency_key: None,
                binary_version: "test".into(),
                stats: Stats::default(),
                lineage: None,
                imported: None,
            };
            let object = revision.object().unwrap();
            let id = object.id();
            save(&self.fs, &[object]);
            id
        }

        /// A revision over a tree of `rows` in table `t`.
        pub fn state(&mut self, parents: &[Id], rows: &[(&str, u8)]) -> Id {
            let mut state = State::default();
            state.tables.insert("t".into(), None);
            for (path, n) in rows {
                state
                    .rows
                    .insert(path.to_string(), (Hash256::of(&[*n]), Id::of(&[*n])));
            }
            let built = build(&state);
            save(&self.fs, &built.objects);
            self.commit(parents, built.root, built.identity)
        }

        pub fn store(&self) -> Store<'_> {
            Store::open(&self.fs, Path::new(OBJECTS)).unwrap()
        }
    }

    #[test]
    fn test8280_ancestry_and_merge_bases_follow_the_dag() {
        //   r0 - a1 - a2 ---- m
        //     \           /
        //      b1 ------ b2
        let mut h = History::new();
        let r0 = h.state(&[], &[("t/x", 0)]);
        let a1 = h.state(&[r0], &[("t/x", 1)]);
        let a2 = h.state(&[a1], &[("t/x", 2)]);
        let b1 = h.state(&[r0], &[("t/y", 1)]);
        let b2 = h.state(&[b1], &[("t/y", 2)]);
        let m = h.state(&[a2, b2], &[("t/x", 2), ("t/y", 2)]);
        let other = h.state(&[], &[("t/z", 9)]);
        let store = h.store();
        let all: HashSet<Id> = ancestors(&store, &m).unwrap().into_iter().collect();
        assert_eq!(all, HashSet::from([m, a2, a1, b2, b1, r0]));
        assert_eq!(ancestors(&store, &m).unwrap()[0], m, "itself first");
        assert!(is_ancestor(&store, &r0, &m).unwrap());
        assert!(is_ancestor(&store, &m, &m).unwrap());
        assert!(!is_ancestor(&store, &m, &r0).unwrap());
        assert!(!is_ancestor(&store, &b1, &a2).unwrap());
        assert_eq!(merge_base(&store, &a2, &b2).unwrap(), vec![r0]);
        assert_eq!(merge_base(&store, &m, &b1).unwrap(), vec![b1]);
        assert_eq!(merge_base(&store, &a2, &other).unwrap(), vec![]);
        assert_eq!(merge_base(&store, &a1, &a1).unwrap(), vec![a1]);
    }

    #[test]
    fn test8281_criss_cross_histories_have_two_merge_bases() {
        //   r - a - x1 - p
        //    \    X
        //     b - x2 - q
        let mut h = History::new();
        let r = h.state(&[], &[]);
        let a = h.state(&[r], &[("t/a", 1)]);
        let b = h.state(&[r], &[("t/b", 1)]);
        let x1 = h.state(&[a, b], &[("t/a", 1), ("t/b", 1)]);
        let x2 = h.state(&[a, b], &[("t/a", 1), ("t/b", 2)]);
        let p = h.state(&[x1], &[("t/p", 1)]);
        let q = h.state(&[x2], &[("t/q", 1)]);
        let store = h.store();
        // x1 and x2 each merged a and b, so both a and b are best common
        // ancestors -- and neither dominates the other.
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(merge_base(&store, &p, &q).unwrap(), expected);
        assert_eq!(merge_base(&store, &x1, &x2).unwrap(), expected);
        let mut tips = vec![x1, x2];
        tips.sort();
        assert_eq!(prune_ancestors(&store, &[x1, x2, a]).unwrap(), tips);
    }

    #[test]
    fn test8282_heads_that_are_ancestors_of_heads_are_pruned() {
        let mut h = History::new();
        let r = h.state(&[], &[]);
        let a = h.state(&[r], &[("t/a", 1)]);
        let b = h.state(&[r], &[("t/b", 1)]);
        let c = h.state(&[a], &[("t/c", 1)]);
        let store = h.store();
        let mut tips = vec![b, c];
        tips.sort();
        assert_eq!(
            prune_ancestors(&store, &[r, a, b, c, c]).unwrap(),
            tips,
            "divergence stays"
        );
        assert_eq!(prune_ancestors(&store, &[a, c]).unwrap(), vec![c]);
        assert_eq!(prune_ancestors(&store, &[r]).unwrap(), vec![r]);
        assert_eq!(prune_ancestors(&store, &[]).unwrap(), vec![]);
        assert!(matches!(
            prune_ancestors(&store, &[c, Id::of(b"nope")]),
            Err(StoreError::Missing(_))
        ));
    }

    #[test]
    fn test8283_reachability_covers_revisions_nodes_and_blobs_and_nothing_else() {
        let mut h = History::new();
        let r = h.state(&[], &[("t/a", 1)]);
        let c = h.state(&[r], &[("t/a", 1), ("t/b", 2)]);
        // An unreachable revision and tree.
        let lost = h.state(&[], &[("t/z", 9)]);
        let store = h.store();
        let reached = reachable(&store, &[c], &[]).unwrap();
        assert!(reached.contains(&c) && reached.contains(&r));
        assert!(!reached.contains(&lost));
        assert!(
            reached.contains(&Id::of(&[1])) && reached.contains(&Id::of(&[2])),
            "blobs"
        );
        assert!(!reached.contains(&Id::of(&[9])));
        // Every reachable non-blob is stored; every stored object outside the
        // set belongs to the lost revision.
        let lost_set = reachable(&store, &[lost], &[]).unwrap();
        for pack in store.packs() {
            for id in pack.index.ids() {
                assert!(reached.contains(id) || lost_set.contains(id), "{id}");
            }
        }
        // A tree reached only through a snapshot ref.
        let lost_tree = revision::load(&store, &lost).unwrap().tree;
        let with_tree = reachable(&store, &[c], &[lost_tree]).unwrap();
        assert!(with_tree.contains(&Id::of(&[9])) && !with_tree.contains(&lost));
    }

    #[test]
    fn test8284_last_known_finds_the_newest_revision_holding_a_path() {
        let mut h = History::new();
        let r1 = h.state(&[], &[("t/a.json", 1)]);
        let r2 = h.state(&[r1], &[("t/a.json", 2)]);
        let r3 = h.state(&[r2], &[]);
        let r4 = h.state(&[r3], &[("t/b.json", 7)]);
        let store = h.store();
        assert_eq!(
            last_known(&store, &r4, "t/a.json").unwrap(),
            Some(LastKnown {
                revision: r2,
                row_hash: Hash256::of(&[2]),
                blob: Id::of(&[2])
            })
        );
        assert_eq!(
            last_known(&store, &r4, "t/b.json")
                .unwrap()
                .unwrap()
                .revision,
            r4
        );
        assert_eq!(last_known(&store, &r4, "t/never.json").unwrap(), None);
        assert_eq!(last_known(&store, &r4, "u/a.json").unwrap(), None);
        assert!(matches!(
            last_known(&store, &r4, "t/../a"),
            Err(StoreError::Invalid(_))
        ));
    }

    #[test]
    fn test8285_last_known_skips_revisions_whose_leaf_did_not_change() {
        use crate::store::tree::tests::Counting;
        use std::sync::atomic::AtomicUsize;
        // A history of 31 revisions in which every step changes the table
        // but never the mid holding the path -- which holds another row, so
        // proving the path absent there takes reading its mid and leaf once.
        let target = "t/gone.json";
        let (target_mid, target_leaf) = bucket(target);
        let neighbour = (0u32..)
            .map(|n| format!("t/k{n}.json"))
            .find(|p| bucket(p) == (target_mid, target_leaf))
            .unwrap();
        let mut h = History::new();
        let mut state = State::default();
        state.tables.insert("t".into(), None);
        state
            .rows
            .insert(neighbour, (Hash256::of(b"k"), Id::of(b"k")));
        let built = build(&state);
        save(&h.fs, &built.objects);
        let mut head = h.commit(&[], built.root, built.identity);
        let mut tree = built.root;
        let others = (0u32..)
            .map(|n| format!("t/other{n}.json"))
            .filter(|p| bucket(p).0 != target_mid)
            .take(30);
        for path in others {
            let store = h.store();
            let mut update = TreeUpdate::new(&store, tree).unwrap();
            update.add(row(&path, 1, 1)).unwrap();
            let next = update.finish().unwrap();
            drop(store);
            save(&h.fs, &next.objects);
            tree = next.root;
            head = h.commit(&[head], next.root, next.identity);
        }
        let counting = Counting {
            inner: &h.fs,
            reads: AtomicUsize::new(0),
        };
        let store = Store::open(&counting, Path::new(OBJECTS)).unwrap();
        counting.take();
        assert_eq!(last_known(&store, &head, target).unwrap(), None);
        // The head: revision, root, table, mid, leaf. Every other revision:
        // revision, root, table -- its mid is the one already proven.
        assert_eq!(counting.take(), 5 + 30 * 3);
    }
}
