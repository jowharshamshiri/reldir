//! The read side of the object store.
//!
//! [`Store`] knows the published packs by their indexes, loaded whole at
//! open, and reads an object with one positioned read of its pack. Opening
//! proves each index (its own trailer and structure), binds it to its pack
//! (the pack's header and trailer, read alone, must be the ones the index
//! names) and checks that every entry lies inside the pack; reading proves
//! each object (its bytes must hash to its id). A full read of every pack is
//! [`super::verify`]'s job, not open's, so opening costs the indexes, not the
//! history.
//!
//! Other processes publish and consolidate packs while a store is open. A
//! miss, or a pack that vanished under a read, re-lists the directory once
//! before the answer is final.

use super::{
    Id, Object,
    error::{Corruption, Result, StoreError},
    pack::{self, Index, PACK_HEADER_LEN, PackName, TRAILER_LEN},
};
use crate::engine::fs::{Kind, Source};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

/// One published pack, its index loaded.
#[derive(Debug)]
pub struct LoadedPack {
    pub name: PackName,
    pub pack_path: PathBuf,
    pub idx_path: PathBuf,
    /// The pack file's size in bytes.
    pub size: u64,
    pub index: Index,
}

#[derive(Debug, Default)]
struct Listing {
    packs: Vec<Arc<LoadedPack>>,
    orphans: Vec<PathBuf>,
}

/// The objects under `.db/objects/`.
pub struct Store<'a> {
    source: &'a dyn Source,
    objects: PathBuf,
    // Replaced whole by a refresh, never edited in place, so a reader holding
    // the previous listing keeps a consistent one -- and a lock poisoned by a
    // panicking refresh still guards a consistent value.
    listing: RwLock<Arc<Listing>>,
    /// Set while a [`Settled`] guard lives: a miss is an answer, not a reason
    /// to list the packs again.
    settled: std::sync::atomic::AtomicBool,
}

/// While it lives, the store answers from one listing: a lookup that misses
/// does not list the packs again. For a writer building a recording under
/// the writer lock, which no other process publishes under, and which itself
/// publishes nothing until the build is done: every object it asks about and
/// does not find is new, and finding that out costs nothing.
pub struct Settled<'s, 'a> {
    store: &'s Store<'a>,
}

impl Drop for Settled<'_, '_> {
    fn drop(&mut self) {
        self.store
            .settled
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

impl<'a> Store<'a> {
    /// Open the store whose packs are in `objects_dir/pack`, which must
    /// exist ([`super::Layout::init`] creates it).
    pub fn open(source: &'a dyn Source, objects_dir: &Path) -> Result<Self> {
        let store = Self {
            source,
            objects: objects_dir.to_path_buf(),
            listing: RwLock::new(Arc::new(Listing::default())),
            settled: std::sync::atomic::AtomicBool::new(false),
        };
        store.refresh()?;
        Ok(store)
    }

    pub fn source(&self) -> &'a dyn Source {
        self.source
    }

    pub fn objects_dir(&self) -> &Path {
        &self.objects
    }

    pub fn pack_dir(&self) -> PathBuf {
        self.objects.join("pack")
    }

    fn current(&self) -> Arc<Listing> {
        match self.listing.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// The published packs, in name (creation) order.
    pub fn packs(&self) -> Vec<Arc<LoadedPack>> {
        self.current().packs.clone()
    }

    /// Packs whose index is absent: publishes that were interrupted between
    /// the two renames. Readers ignore them; whoever holds the writer lock
    /// completes or removes them.
    pub fn orphan_packs(&self) -> Vec<PathBuf> {
        self.current().orphans.clone()
    }

    /// How many objects the packs index, counting an object stored in two
    /// packs twice.
    pub fn object_records(&self) -> usize {
        self.current().packs.iter().map(|p| p.index.len()).sum()
    }

    /// Re-list the pack directory: load new indexes, forget removed packs.
    /// Packs already loaded are kept as they are: a published pack never
    /// changes.
    pub fn refresh(&self) -> Result<()> {
        let dir = self.pack_dir();
        let entries = self
            .source
            .read_dir(&dir)
            .map_err(|e| StoreError::io("list packs", &dir, e))?;
        let mut packs: BTreeMap<PackName, PathBuf> = BTreeMap::new();
        let mut indexes: BTreeMap<PackName, PathBuf> = BTreeMap::new();
        for entry in entries {
            let file_name = entry
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| StoreError::corrupt(Corruption::UnexpectedEntry, &entry.path))?;
            let (stem, is_pack) = if let Some(stem) = file_name.strip_suffix(".pack") {
                (stem, true)
            } else if let Some(stem) = file_name.strip_suffix(".idx") {
                (stem, false)
            } else {
                return Err(StoreError::corrupt(
                    Corruption::UnexpectedEntry,
                    &entry.path,
                ));
            };
            let name = PackName::parse(stem)
                .ok_or_else(|| StoreError::corrupt(Corruption::UnexpectedEntry, &entry.path))?;
            if entry.meta.kind != Kind::File {
                return Err(StoreError::corrupt(Corruption::NotAFile, &entry.path));
            }
            if is_pack {
                packs.insert(name, entry.path);
            } else {
                indexes.insert(name, entry.path);
            }
        }
        let previous = self.current();
        let mut listing = Listing::default();
        for (name, idx_path) in indexes {
            let Some(pack_path) = packs.remove(&name) else {
                return Err(StoreError::corrupt(Corruption::IdxWithoutPack, &idx_path));
            };
            if let Some(known) = previous.packs.iter().find(|p| p.name == name) {
                listing.packs.push(known.clone());
                continue;
            }
            match self.load(name, pack_path, idx_path.clone()) {
                Ok(loaded) => listing.packs.push(Arc::new(loaded)),
                // Removed by a consolidation between the listing and the
                // load: the index goes first, so its absence now proves the
                // pack was retired, not damaged.
                Err(error) if error.is_not_found() && !self.exists(&idx_path)? => {}
                Err(error) => return Err(error),
            }
        }
        listing.orphans = packs.into_values().collect();
        let listing = Arc::new(listing);
        match self.listing.write() {
            Ok(mut guard) => *guard = listing,
            Err(poisoned) => *poisoned.into_inner() = listing,
        }
        Ok(())
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        match self.source.metadata(path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(StoreError::io("inspect", path, e)),
        }
    }

    fn load(&self, name: PackName, pack_path: PathBuf, idx_path: PathBuf) -> Result<LoadedPack> {
        let idx = self
            .source
            .read(&idx_path)
            .map_err(|e| StoreError::io("read index", &idx_path, e))?;
        let index = Index::parse(&idx).map_err(|what| StoreError::corrupt(what, &idx_path))?;
        let meta = self
            .source
            .metadata(&pack_path)
            .map_err(|e| StoreError::io("inspect pack", &pack_path, e))?;
        if meta.kind != Kind::File {
            return Err(StoreError::corrupt(Corruption::NotAFile, &pack_path));
        }
        let size = meta.stat.size;
        if size < PACK_HEADER_LEN + TRAILER_LEN {
            return Err(StoreError::corrupt(Corruption::Truncated, &pack_path));
        }
        let header = self.read_pack(&pack_path, 0, PACK_HEADER_LEN as usize)?;
        pack::check_header(&header, &index)
            .map_err(|what| StoreError::corrupt(what, &pack_path))?;
        let trailer = self.read_pack(&pack_path, size - TRAILER_LEN, TRAILER_LEN as usize)?;
        if trailer.as_slice() != index.pack_checksum() {
            return Err(StoreError::corrupt(
                Corruption::PackChecksumMismatch,
                &idx_path,
            ));
        }
        index
            .check_bounds(size)
            .map_err(|what| StoreError::corrupt(what, &idx_path))?;
        Ok(LoadedPack {
            name,
            pack_path,
            idx_path,
            size,
            index,
        })
    }

    /// A positioned read of a pack. A pack shorter than a read its validated
    /// index implies was cut short after it was opened: that is truncation,
    /// not an I/O failure.
    fn read_pack(&self, path: &Path, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.source
            .read_at(path, offset, len)
            .map_err(|e| match e.kind() {
                io::ErrorKind::UnexpectedEof => StoreError::corrupt(Corruption::Truncated, path),
                _ => StoreError::io("read pack", path, e),
            })
    }

    /// Read entry `at` of a loaded pack, proving it is the object its id names.
    pub fn read_entry(&self, pack: &LoadedPack, at: usize) -> Result<Object> {
        let id = pack.index.ids()[at];
        let len = usize::try_from(pack.index.entry_len(at)).map_err(|_| {
            StoreError::corrupt(Corruption::EntryOutOfBounds { id }, &pack.pack_path)
        })?;
        let bytes = self.read_pack(&pack.pack_path, pack.index.offset(at), len)?;
        pack::decode_entry(&bytes, id, pack.index.length(at))
            .map_err(|what| StoreError::corrupt(what, &pack.pack_path))
    }

    /// The object `id` names. Every object returned hashes to its id.
    pub fn get(&self, id: &Id) -> Result<Object> {
        if let Some(object) = self.try_get(id, true)? {
            return Ok(object);
        }
        self.refresh()?;
        self.try_get(id, false)?.ok_or(StoreError::Missing(*id))
    }

    /// `Ok(None)` on a miss. With `tolerate_vanished`, a pack that disappeared
    /// since it was listed counts as a miss, so the caller re-lists; without
    /// it, it is the error it is.
    ///
    /// An object can be held by several packs -- a lineage begun over damaged
    /// history writes fresh copies of what it keeps. A copy whose bytes do not
    /// prove themselves against the id is passed over for one that does; only
    /// when no copy is intact is the damage the answer. Verification reports
    /// the damaged pack regardless.
    fn try_get(&self, id: &Id, tolerate_vanished: bool) -> Result<Option<Object>> {
        let mut damaged = None;
        for pack in &self.current().packs {
            if let Some(at) = pack.index.find(id) {
                match self.read_entry(pack, at) {
                    Ok(object) => return Ok(Some(object)),
                    Err(error) if tolerate_vanished && error.is_not_found() => continue,
                    Err(
                        error @ (StoreError::Corrupt { .. } | StoreError::CorruptObject { .. }),
                    ) => {
                        damaged.get_or_insert(error);
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        match damaged {
            Some(error) => Err(error),
            None => Ok(None),
        }
    }

    /// Whether some pack holds `id`, re-listing once on a miss -- unless the
    /// store is [`Settled`]. Answers from the indexes alone: the object's
    /// bytes are not read.
    pub fn contains(&self, id: &Id) -> Result<bool> {
        if self.indexed(id) {
            return Ok(true);
        }
        if self.settled.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(false);
        }
        self.refresh()?;
        Ok(self.indexed(id))
    }

    /// List the packs once, and answer from that listing until the guard is
    /// dropped. See [`Settled`].
    pub fn settle(&self) -> Result<Settled<'_, 'a>> {
        self.refresh()?;
        self.settled
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(Settled { store: self })
    }

    fn indexed(&self, id: &Id) -> bool {
        self.current()
            .packs
            .iter()
            .any(|pack| pack.index.find(id).is_some())
    }

    /// Read and prove a whole pack against its index.
    pub fn verify_pack(&self, pack: &LoadedPack) -> Result<()> {
        let bytes = self
            .source
            .read(&pack.pack_path)
            .map_err(|e| StoreError::io("read pack", &pack.pack_path, e))?;
        pack::verify_pack(&bytes, &pack.index)
            .map_err(|what| StoreError::corrupt(what, &pack.pack_path))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::engine::fs::Fs;
    use crate::store::{
        Layout, ObjectKind,
        pack::{PackWriter, publish},
        test_fs::StdFs,
    };
    use sha2::{Digest, Sha256};

    /// A database root with its history directories, and a staging directory
    /// beside them for packs on their way to being published.
    pub(crate) fn fixture() -> (StdFs, Layout, PathBuf) {
        let fs = StdFs::new();
        let root = fs.root().join("db");
        std::fs::create_dir_all(root.join(".db")).unwrap();
        let layout = Layout::new(&root);
        layout.init(&fs).unwrap();
        let staging = fs.root().join("staging");
        std::fs::create_dir(&staging).unwrap();
        (fs, layout, staging)
    }

    pub(crate) fn put(fs: &dyn Fs, layout: &Layout, staging: &Path, objects: &[&[u8]]) -> PackName {
        let mut writer = PackWriter::new();
        for bytes in objects {
            writer.add(ObjectKind::Blob, bytes.to_vec()).unwrap();
        }
        let name = PackName::generate();
        writer.write_to(fs, staging, &name).unwrap();
        publish(fs, staging, &layout.packs(), &name).unwrap();
        name
    }

    fn reseal(bytes: &mut [u8]) {
        let end = bytes.len() - 32;
        let digest = Sha256::digest(&bytes[..end]);
        bytes[end..].copy_from_slice(&digest);
    }

    fn open_error(fs: &StdFs, layout: &Layout) -> StoreError {
        match Store::open(fs, &layout.objects()) {
            Ok(_) => panic!("the store opened"),
            Err(error) => error,
        }
    }

    #[test]
    fn test8220_objects_are_found_across_packs_and_misses_are_missing() {
        let (fs, layout, staging) = fixture();
        put(&fs, &layout, &staging, &[b"one", b"two"]);
        put(&fs, &layout, &staging, &[b"two", b"three"]);
        let store = Store::open(&fs, &layout.objects()).unwrap();
        assert_eq!(store.packs().len(), 2);
        for bytes in [&b"one"[..], b"two", b"three"] {
            let object = store.get(&Id::of(bytes)).unwrap();
            assert_eq!(object.bytes, bytes);
            assert!(store.contains(&Id::of(bytes)).unwrap());
        }
        let absent = Id::of(b"four");
        assert!(matches!(store.get(&absent), Err(StoreError::Missing(id)) if id == absent));
        assert!(!store.contains(&absent).unwrap());
    }

    /// A damaged copy is passed over for an intact one in another pack; with
    /// no intact copy anywhere, the damage is the answer, never a guess.
    #[test]
    fn test8239_an_intact_copy_is_preferred_to_a_damaged_one() {
        let (fs, layout, staging) = fixture();
        let damaged = put(&fs, &layout, &staging, &[b"shared payload"]);
        // Flip a bit of the object's bytes as a failing disk would: the
        // trailer the index is bound to is untouched, so the pack still opens
        // and only reading the object finds the damage.
        let path = layout.packs().join(damaged.pack_file());
        let mut bytes = std::fs::read(&path).unwrap();
        let at = bytes
            .windows(b"shared payload".len())
            .position(|window| window == b"shared payload")
            .unwrap();
        bytes[at] ^= 0x20;
        std::fs::write(&path, &bytes).unwrap();
        let id = Id::of(b"shared payload");
        let store = Store::open(&fs, &layout.objects()).unwrap();
        assert!(
            matches!(store.get(&id), Err(StoreError::Corrupt { .. })),
            "the only copy is damaged"
        );
        put(&fs, &layout, &staging, &[b"shared payload"]);
        store.refresh().unwrap();
        assert_eq!(store.get(&id).unwrap().bytes, b"shared payload");
    }

    #[test]
    fn test8221_a_pack_published_after_open_is_found_on_a_miss() {
        let (fs, layout, staging) = fixture();
        put(&fs, &layout, &staging, &[b"early"]);
        let store = Store::open(&fs, &layout.objects()).unwrap();
        put(&fs, &layout, &staging, &[b"late"]);
        assert_eq!(store.get(&Id::of(b"late")).unwrap().bytes, b"late");
        assert_eq!(store.packs().len(), 2);
    }

    #[test]
    fn test8222_a_pack_retired_after_open_is_dropped_not_an_error() {
        let (fs, layout, staging) = fixture();
        let old = put(&fs, &layout, &staging, &[b"moved"]);
        let store = Store::open(&fs, &layout.objects()).unwrap();
        // Another process consolidates: a new pack with the object, then the
        // old one removed, index first.
        put(&fs, &layout, &staging, &[b"moved"]);
        std::fs::remove_file(layout.packs().join(old.idx_file())).unwrap();
        std::fs::remove_file(layout.packs().join(old.pack_file())).unwrap();
        assert_eq!(store.get(&Id::of(b"moved")).unwrap().bytes, b"moved");
        assert_eq!(store.packs().len(), 1);
    }

    #[test]
    fn test8223_a_pack_without_its_index_is_an_orphan_and_ignored() {
        let (fs, layout, staging) = fixture();
        let mut writer = PackWriter::new();
        writer.add(ObjectKind::Blob, b"half".to_vec()).unwrap();
        let name = PackName::generate();
        writer.write_to(&fs, &staging, &name).unwrap();
        std::fs::rename(
            staging.join(name.pack_file()),
            layout.packs().join(name.pack_file()),
        )
        .unwrap();
        let store = Store::open(&fs, &layout.objects()).unwrap();
        assert_eq!(
            store.orphan_packs(),
            vec![layout.packs().join(name.pack_file())]
        );
        assert!(matches!(
            store.get(&Id::of(b"half")),
            Err(StoreError::Missing(_))
        ));
        // Completing the publish makes it visible.
        publish(&fs, &staging, &layout.packs(), &name).unwrap();
        assert_eq!(store.get(&Id::of(b"half")).unwrap().bytes, b"half");
        assert!(store.orphan_packs().is_empty());
    }

    #[test]
    fn test8224_an_index_without_its_pack_is_corrupt() {
        let (fs, layout, staging) = fixture();
        let name = put(&fs, &layout, &staging, &[b"gone"]);
        std::fs::remove_file(layout.packs().join(name.pack_file())).unwrap();
        let error = open_error(&fs, &layout);
        assert!(
            matches!(&error, StoreError::Corrupt { what: Corruption::IdxWithoutPack, path } if path.ends_with(name.idx_file())),
            "{error:?}"
        );
    }

    #[test]
    fn test8225_foreign_entries_in_the_pack_directory_are_corrupt() {
        for (file, dir) in [
            ("notes.txt", false),
            ("pack-xyz.pack", false),
            ("pack-01ARZ3NDEKTSV4RRFFQ69G5FAV.idx", true),
        ] {
            let (fs, layout, _) = fixture();
            let path = layout.packs().join(file);
            if dir {
                std::fs::create_dir(&path).unwrap();
            } else {
                std::fs::write(&path, b"").unwrap();
            }
            let error = open_error(&fs, &layout);
            let expected = if dir {
                Corruption::NotAFile
            } else {
                Corruption::UnexpectedEntry
            };
            assert!(
                matches!(&error, StoreError::Corrupt { what, path: at } if *what == expected && *at == path),
                "{file}: {error:?}"
            );
        }
    }

    fn damaged(edit: impl Fn(&mut Vec<u8>, &mut Vec<u8>)) -> (StoreError, PathBuf, PathBuf) {
        let (fs, layout, staging) = fixture();
        let name = put(&fs, &layout, &staging, &[b"alpha", b"beta", b"gamma"]);
        let pack_path = layout.packs().join(name.pack_file());
        let idx_path = layout.packs().join(name.idx_file());
        let mut pack = std::fs::read(&pack_path).unwrap();
        let mut idx = std::fs::read(&idx_path).unwrap();
        edit(&mut pack, &mut idx);
        std::fs::write(&pack_path, &pack).unwrap();
        std::fs::write(&idx_path, &idx).unwrap();
        let error = match Store::open(&fs, &layout.objects()) {
            Err(error) => error,
            Ok(store) => {
                let mut first = None;
                for bytes in [&b"alpha"[..], b"beta", b"gamma"] {
                    if let Err(error) = store.get(&Id::of(bytes)) {
                        first.get_or_insert(error);
                    }
                }
                first.expect("the damage is noticed")
            }
        };
        (error, pack_path, idx_path)
    }

    fn assert_corrupt(error: &StoreError, expected: impl Fn(&Corruption) -> bool, path: &Path) {
        assert!(
            matches!(error, StoreError::Corrupt { what, path: at } if expected(what) && at == path),
            "{error:?}"
        );
    }

    #[test]
    fn test8226_a_flipped_index_byte_is_a_trailer_mismatch_on_the_index() {
        let (error, _, idx) = damaged(|_, idx| idx[600] ^= 0x10);
        assert_corrupt(&error, |w| *w == Corruption::TrailerMismatch, &idx);
    }

    #[test]
    fn test8227_a_truncated_index_is_corrupt() {
        let (error, _, idx) = damaged(|_, idx| idx.truncate(500));
        assert_corrupt(&error, |w| *w == Corruption::Truncated, &idx);
    }

    #[test]
    fn test8228_a_pack_whose_trailer_is_not_the_one_indexed_is_refused() {
        let (error, _, idx) = damaged(|pack, _| {
            let last = pack.len() - 1;
            pack[last] ^= 1;
        });
        assert_corrupt(&error, |w| *w == Corruption::PackChecksumMismatch, &idx);
    }

    #[test]
    fn test8229_a_truncated_pack_is_corrupt() {
        let (error, pack, _) = damaged(|pack, _| pack.truncate(30));
        assert_corrupt(&error, |w| *w == Corruption::Truncated, &pack);
        // Truncated in the middle with its trailer kept: entries fall outside.
        let (error, _, idx) = damaged(|pack, _| {
            let trailer = pack[pack.len() - 32..].to_vec();
            pack.truncate(pack.len() - 40);
            pack.extend_from_slice(&trailer);
        });
        assert_corrupt(
            &error,
            |w| matches!(w, Corruption::EntryOutOfBounds { .. }),
            &idx,
        );
    }

    #[test]
    fn test8230_a_flipped_content_byte_is_caught_by_the_object_hash() {
        // The pack's trailer is not read on get, so only the object's own
        // hash can catch this, and it must.
        let (error, pack, _) = damaged(|pack, _| {
            let at = pack.windows(5).position(|w| w == b"gamma").unwrap();
            pack[at] = b'G';
        });
        assert_corrupt(
            &error,
            |w| matches!(w, Corruption::HashMismatch { expected, .. } if *expected == Id::of(b"gamma")),
            &pack,
        );
    }

    #[test]
    fn test8231_a_wrong_kind_byte_or_header_length_is_corrupt() {
        let (error, pack, _) = damaged(|pack, _| {
            let at = pack.windows(5).position(|w| w == b"alpha").unwrap();
            pack[at - 2] = 0x7f;
        });
        assert_corrupt(&error, |w| *w == Corruption::UnknownObjectKind(0x7f), &pack);
        let (error, pack, _) = damaged(|pack, _| {
            let at = pack.windows(4).position(|w| w == b"beta").unwrap();
            pack[at - 1] = 3;
        });
        assert_corrupt(
            &error,
            |w| matches!(w, Corruption::EntryHeaderMismatch { .. }),
            &pack,
        );
    }

    #[test]
    fn test8232_an_index_pointing_at_the_wrong_entry_is_a_hash_mismatch() {
        // Swap two same-length objects' offsets and reseal: the index is
        // self-consistent, the pack untouched, and each id now leads to the
        // other's bytes.
        let (error, pack, _) = damaged(|_, idx| {
            let index = Index::parse(idx).unwrap();
            let count = index.len();
            let offsets = 8 + 1024 + count * 32;
            // alpha and gamma are both five bytes.
            let (a, b) = (
                index.find(&Id::of(b"alpha")).unwrap(),
                index.find(&Id::of(b"gamma")).unwrap(),
            );
            let (oa, ob) = (offsets + a * 8, offsets + b * 8);
            let first = idx[oa..oa + 8].to_vec();
            let second = idx[ob..ob + 8].to_vec();
            idx[oa..oa + 8].copy_from_slice(&second);
            idx[ob..ob + 8].copy_from_slice(&first);
            reseal(idx);
        });
        assert_corrupt(
            &error,
            |w| matches!(w, Corruption::HashMismatch { .. }),
            &pack,
        );
    }

    #[test]
    fn test8233_bad_magic_and_version_in_a_pack_header_are_named() {
        let (error, pack, _) = damaged(|pack, _| pack[0] = b'X');
        assert_corrupt(&error, |w| *w == Corruption::BadMagic, &pack);
        let (error, pack, _) = damaged(|pack, _| pack[4] = 7);
        assert_corrupt(&error, |w| *w == Corruption::UnsupportedVersion(7), &pack);
        let (error, pack, _) = damaged(|pack, _| pack[8] = 2);
        assert_corrupt(
            &error,
            |w| *w == Corruption::CountMismatch { idx: 3, pack: 2 },
            &pack,
        );
    }

    #[test]
    fn test8234_verify_pack_reads_what_get_does_not() {
        // A damaged byte in an entry never read is invisible to `get` of the
        // others, and is exactly what a full verification exists to find.
        let (fs, layout, staging) = fixture();
        let name = put(&fs, &layout, &staging, &[b"kept", b"rotten"]);
        let pack_path = layout.packs().join(name.pack_file());
        let mut pack = std::fs::read(&pack_path).unwrap();
        let at = pack.windows(6).position(|w| w == b"rotten").unwrap();
        pack[at] = b'R';
        std::fs::write(&pack_path, &pack).unwrap();
        let store = Store::open(&fs, &layout.objects()).unwrap();
        assert_eq!(store.get(&Id::of(b"kept")).unwrap().bytes, b"kept");
        let loaded = &store.packs()[0];
        let error = store.verify_pack(loaded).unwrap_err();
        assert_corrupt(&error, |w| *w == Corruption::TrailerMismatch, &pack_path);
    }

    #[test]
    fn test8235_the_store_requires_its_pack_directory() {
        let fs = StdFs::new();
        let error = match Store::open(&fs, &fs.root().join("objects")) {
            Err(error) => error,
            Ok(_) => panic!("opened"),
        };
        assert!(error.is_not_found(), "{error:?}");
    }

    #[test]
    fn test8236_publish_completes_an_interrupted_publish_and_never_replaces() {
        let (fs, layout, staging) = fixture();
        let mut writer = PackWriter::new();
        writer.add(ObjectKind::Blob, b"x".to_vec()).unwrap();
        let name = PackName::generate();
        writer.write_to(&fs, &staging, &name).unwrap();
        std::fs::rename(
            staging.join(name.pack_file()),
            layout.packs().join(name.pack_file()),
        )
        .unwrap();
        publish(&fs, &staging, &layout.packs(), &name).unwrap();
        publish(&fs, &staging, &layout.packs(), &name).unwrap();
        let store = Store::open(&fs, &layout.objects()).unwrap();
        assert!(store.contains(&Id::of(b"x")).unwrap());
        // A second pack of the same name staged again is refused.
        writer.write_to(&fs, &staging, &name).unwrap();
        assert!(matches!(
            publish(&fs, &staging, &layout.packs(), &name),
            Err(StoreError::Invalid(crate::store::Invalid::PackExists(_)))
        ));
        // Nothing staged and nothing published is an error, not a no-op.
        let never = PackName::generate();
        assert!(
            publish(&fs, &staging, &layout.packs(), &never)
                .unwrap_err()
                .is_not_found()
        );
    }
}
