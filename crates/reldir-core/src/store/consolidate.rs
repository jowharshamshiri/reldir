//! Keeping the number of packs small, and dropping what nothing reaches.
//!
//! Every commit publishes a pack, so packs accumulate; every lookup may try
//! each one. [`consolidate`] merges the smallest packs once there are more
//! than [`MAX_PACKS`], choosing them so the packs that remain form a
//! geometric progression -- each at least twice the size of the next smaller
//! -- which means a large pack is rewritten rarely and the cost of
//! consolidation stays proportional to what was recently added. [`repack`]
//! rewrites every pack into one keeping only a given set: garbage
//! collection, with the reachable set from [`super::dag::reachable`].
//!
//! Both write the new pack durably and publish it before removing anything,
//! and remove each old pack's index before its pack, so at every instant --
//! a crash included -- each object is in at least one visible pack, and no
//! index is without its pack. Every object copied is read through the store,
//! so it is proven against its id on the way: a damaged object stops the
//! rewrite before anything is removed. The caller holds the writer lock.

use super::{
    Id, Store,
    error::{Result, StoreError},
    objects::LoadedPack,
    pack::{PackName, PackWriter, publish},
};
use crate::engine::fs::Fs;
use std::{collections::HashSet, path::Path, sync::Arc};

/// Consolidation begins above this many packs.
pub const MAX_PACKS: usize = 16;
/// And leaves at most this many: half, so it is not needed again at once.
pub const PACKS_AFTER: usize = MAX_PACKS / 2;
/// Each remaining pack holds at least this many times the objects of all the
/// packs smaller than it.
pub const FACTOR: u64 = 2;

/// What a consolidation or repack did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consolidation {
    /// The pack written, unless nothing was kept.
    pub written: Option<PackName>,
    pub removed: Vec<PackName>,
    /// Distinct objects in the new pack.
    pub kept: usize,
    /// Distinct objects left out of it.
    pub dropped: usize,
}

/// How many of the smallest packs to merge, given object counts in ascending
/// order. None while there are at most [`MAX_PACKS`]. Otherwise every pack
/// below the largest run that is already a progression by [`FACTOR`], then
/// every next pack smaller than [`FACTOR`] times the merged total (so the
/// merged pack fits the progression), and at least enough that no more than
/// [`PACKS_AFTER`] remain.
pub fn rollup(counts: &[u64]) -> usize {
    let n = counts.len();
    if n <= MAX_PACKS {
        return 0;
    }
    let mut split = 0;
    for i in (1..n).rev() {
        if counts[i] < FACTOR.saturating_mul(counts[i - 1]) {
            // The larger of the pair cannot stay in the progression either.
            split = i + 1;
            break;
        }
    }
    let mut total: u64 = counts[..split].iter().sum();
    while split < n && counts[split] < FACTOR.saturating_mul(total) {
        total = total.saturating_add(counts[split]);
        split += 1;
    }
    split.max(n + 1 - PACKS_AFTER).min(n)
}

/// Merge the smallest packs when there are more than [`MAX_PACKS`]; `None`
/// when there are not. `staging` is a directory on the same volume where
/// the new pack is written before it is published.
pub fn consolidate(
    fs: &dyn Fs,
    store: &Store<'_>,
    staging: &Path,
) -> Result<Option<Consolidation>> {
    store.refresh()?;
    let mut packs = store.packs();
    if packs.len() <= MAX_PACKS {
        return Ok(None);
    }
    packs.sort_by(|a, b| {
        a.index
            .len()
            .cmp(&b.index.len())
            .then_with(|| a.name.cmp(&b.name))
    });
    let counts: Vec<u64> = packs.iter().map(|p| p.index.len() as u64).collect();
    let merge = rollup(&counts);
    packs.truncate(merge);
    rewrite(fs, store, staging, &packs, None).map(Some)
}

/// Rewrite every pack into one holding exactly the objects in `keep`. Every
/// object in `keep` must be in the store: a reachable object that is missing
/// is a damaged history, and collecting garbage over it is refused.
pub fn repack(
    fs: &dyn Fs,
    store: &Store<'_>,
    staging: &Path,
    keep: &HashSet<Id>,
) -> Result<Consolidation> {
    store.refresh()?;
    let packs = store.packs();
    rewrite(fs, store, staging, &packs, Some(keep))
}

fn rewrite(
    fs: &dyn Fs,
    store: &Store<'_>,
    staging: &Path,
    packs: &[Arc<LoadedPack>],
    keep: Option<&HashSet<Id>>,
) -> Result<Consolidation> {
    let mut writer = PackWriter::new();
    let mut dropped = HashSet::new();
    for pack in packs {
        for (at, id) in pack.index.ids().iter().enumerate() {
            if keep.is_some_and(|keep| !keep.contains(id)) {
                dropped.insert(*id);
                continue;
            }
            if !writer.contains(id) {
                writer.add_object(store.read_entry(pack, at)?)?;
            }
        }
    }
    if let Some(missing) = keep.and_then(|keep| keep.iter().find(|id| !writer.contains(id))) {
        return Err(StoreError::Missing(*missing));
    }
    let pack_dir = store.pack_dir();
    let written = if writer.is_empty() {
        None
    } else {
        let name = PackName::generate();
        writer.write_to(fs, staging, &name)?;
        publish(fs, staging, &pack_dir, &name)?;
        Some(name)
    };
    for pack in packs {
        fs.remove_file(&pack.idx_path)
            .map_err(|e| StoreError::io("remove index", &pack.idx_path, e))?;
        fs.remove_file(&pack.pack_path)
            .map_err(|e| StoreError::io("remove pack", &pack.pack_path, e))?;
    }
    fs.sync_dir(&pack_dir)
        .map_err(|e| StoreError::io("sync directory", &pack_dir, e))?;
    store.refresh()?;
    Ok(Consolidation {
        written,
        removed: packs.iter().map(|p| p.name.clone()).collect(),
        kept: writer.len(),
        dropped: dropped.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::fs::{Fs, Source};
    use crate::store::{
        Corruption, Layout,
        objects::tests::{fixture, put},
        test_fs::{Faulty, MemFs, Op},
    };
    use std::path::PathBuf;

    #[test]
    fn test8290_the_rollup_merges_the_small_and_keeps_the_large() {
        assert_eq!(rollup(&[1; 16]), 0, "at the limit, nothing to do");
        assert_eq!(
            rollup(&[1; 17]),
            17,
            "equal packs all merge: none is twice another"
        );
        let progression: Vec<u64> = (0..17).map(|k| 1 << k).collect();
        assert_eq!(
            rollup(&progression),
            17 + 1 - PACKS_AFTER,
            "a perfect progression merges only enough to leave room"
        );
        let mut one_large = vec![1; 16];
        one_large.push(1000);
        assert_eq!(rollup(&one_large), 16, "the large pack is not rewritten");
        // Merging the sixteen small ones makes a pack of 16, which the 20
        // above it is not twice of: it joins them, and the 1000 stays.
        let mut cascade = vec![1; 16];
        cascade.extend([20, 1000]);
        assert_eq!(rollup(&cascade), 17);
        // Saturating arithmetic: huge counts never overflow.
        assert_eq!(rollup(&[u64::MAX; 17]), 17 + 1 - PACKS_AFTER);
    }

    /// `n` packs in memory: pack `p` holds `p + 1` objects of its own and one
    /// object every pack shares.
    fn many_packs(n: usize) -> (MemFs, Layout, PathBuf, Vec<Vec<u8>>) {
        let layout = Layout::new("/db");
        let staging = PathBuf::from("/staging");
        let fs = MemFs::with_dirs(&[&layout.packs(), &staging]);
        let shared = b"in every pack".to_vec();
        let mut all = vec![shared.clone()];
        for p in 0..n {
            let objects: Vec<Vec<u8>> = (0..=p)
                .map(|k| format!("pack {p} object {k}").into_bytes())
                .collect();
            let mut refs: Vec<&[u8]> = objects.iter().map(Vec::as_slice).collect();
            refs.push(&shared);
            put(&fs, &layout, &staging, &refs);
            all.extend(objects);
        }
        (fs, layout, staging, all)
    }

    #[test]
    fn test8291_consolidation_publishes_before_it_removes_and_loses_nothing() {
        let (fs, layout, staging, all) = many_packs(20);
        let store = Store::open(&fs, &layout.objects()).unwrap();
        assert!(consolidate(&fs, &store, &staging).unwrap().is_some());
        // That run already brought the count down; another does nothing.
        assert!(consolidate(&fs, &store, &staging).unwrap().is_none());
        assert!(store.packs().len() <= PACKS_AFTER);
        for bytes in &all {
            assert_eq!(&store.get(&Id::of(bytes)).unwrap().bytes, bytes);
        }

        let (fs, layout, staging, _) = many_packs(20);
        let store = Store::open(&fs, &layout.objects()).unwrap();
        let faulty = Faulty::new(&fs, None);
        let done = consolidate(&faulty, &store, &staging).unwrap().unwrap();
        assert_eq!(done.dropped, 0);
        let log = faulty.log();
        let name = done.written.unwrap();
        let published = log
            .iter()
            .position(|op| {
                *op == Op::Rename(
                    staging.join(name.idx_file()),
                    layout.packs().join(name.idx_file()),
                )
            })
            .expect("the index is published by rename");
        assert_eq!(
            log[published + 1],
            Op::SyncDir(layout.packs()),
            "and made durable"
        );
        let first_removal = log
            .iter()
            .position(|op| matches!(op, Op::RemoveFile(_)))
            .unwrap();
        assert!(
            first_removal > published + 1,
            "nothing is removed before the new pack is durable"
        );
        for old in &done.removed {
            let idx = log
                .iter()
                .position(|op| *op == Op::RemoveFile(layout.packs().join(old.idx_file())))
                .unwrap();
            let pack = log
                .iter()
                .position(|op| *op == Op::RemoveFile(layout.packs().join(old.pack_file())))
                .unwrap();
            assert!(idx < pack, "an index goes before its pack");
        }
        assert_eq!(log.last(), Some(&Op::SyncDir(layout.packs())));
    }

    #[test]
    fn test8292_a_failure_at_any_step_of_consolidation_loses_no_object() {
        let total = {
            let (fs, layout, staging, _) = many_packs(18);
            let store = Store::open(&fs, &layout.objects()).unwrap();
            let faulty = Faulty::new(&fs, None);
            consolidate(&faulty, &store, &staging).unwrap();
            faulty.log().len()
        };
        for fail_at in 0..total {
            let (fs, layout, staging, all) = many_packs(18);
            let store = Store::open(&fs, &layout.objects()).unwrap();
            let faulty = Faulty::new(&fs, Some(fail_at));
            assert!(
                consolidate(&faulty, &store, &staging).is_err(),
                "fault at {fail_at}"
            );
            let reopened = Store::open(&fs, &layout.objects())
                .unwrap_or_else(|e| panic!("fault at {fail_at} left the store unreadable: {e}"));
            for bytes in &all {
                assert_eq!(
                    &reopened.get(&Id::of(bytes)).unwrap().bytes,
                    bytes,
                    "fault at {fail_at}"
                );
            }
        }
    }

    #[test]
    fn test8293_repack_keeps_exactly_what_it_is_told_to() {
        let (fs, layout, staging) = fixture();
        put(&fs, &layout, &staging, &[b"live", b"dead"]);
        put(
            &fs,
            &layout,
            &staging,
            &[b"live", b"also dead", b"live too"],
        );
        let store = Store::open(&fs, &layout.objects()).unwrap();
        let keep = HashSet::from([Id::of(b"live"), Id::of(b"live too")]);
        let done = repack(&fs, &store, &staging, &keep).unwrap();
        assert_eq!((done.kept, done.dropped, done.removed.len()), (2, 2, 2));
        assert_eq!(store.packs().len(), 1);
        assert_eq!(store.get(&Id::of(b"live")).unwrap().bytes, b"live");
        assert!(matches!(
            store.get(&Id::of(b"dead")),
            Err(StoreError::Missing(_))
        ));

        // A reachable object that is missing stops the collection untouched.
        let before = store.packs();
        let missing = HashSet::from([Id::of(b"live"), Id::of(b"never stored")]);
        assert!(matches!(
            repack(&fs, &store, &staging, &missing),
            Err(StoreError::Missing(id)) if id == Id::of(b"never stored")
        ));
        assert_eq!(store.packs().len(), before.len());
        assert_eq!(store.get(&Id::of(b"live too")).unwrap().bytes, b"live too");

        // Keeping nothing removes every pack and writes none.
        let done = repack(&fs, &store, &staging, &HashSet::new()).unwrap();
        assert_eq!(done.written, None);
        assert!(store.packs().is_empty());
    }

    #[test]
    fn test8294_a_damaged_object_stops_consolidation_before_anything_is_removed() {
        let (fs, layout, staging, _) = many_packs(17);
        let store = Store::open(&fs, &layout.objects()).unwrap();
        let victim = store
            .packs()
            .into_iter()
            .min_by_key(|p| p.index.len())
            .unwrap();
        let mut bytes = fs.read(&victim.pack_path).unwrap();
        let at = bytes.windows(6).position(|w| w == b"object").unwrap();
        bytes[at] = b'O';
        fs.write(&victim.pack_path, &bytes).unwrap();
        let error = consolidate(&fs, &store, &staging).unwrap_err();
        assert!(
            matches!(&error, StoreError::Corrupt { what: Corruption::HashMismatch { .. }, path } if *path == victim.pack_path),
            "{error:?}"
        );
        assert_eq!(
            fs.read_dir(&layout.packs()).unwrap().len(),
            34,
            "every pack is still there"
        );
    }
}
