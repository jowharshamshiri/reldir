//! Packs: many objects in one immutable file, found through a sorted index.
//!
//! A pack is `pack-<ulid>.pack` beside `pack-<ulid>.idx` in
//! `.db/objects/pack/`. All integers are little-endian.
//!
//! ```text
//! pack:  "RDPK" u32:version=1 u32:count
//!        count × ( u8:kind  varint:len  len bytes )
//!        32 bytes: SHA-256 of everything before
//!
//! idx:   "RDIX" u32:version=1
//!        256 × u32   fan-out: entry b counts the ids whose first byte <= b
//!        n × 32      the ids, strictly ascending (n = fan-out[255])
//!        n × u64     offset of each id's entry in the pack (at its kind byte)
//!        n × u32     each object's length
//!        32 bytes    the pack's trailer, binding this index to that pack
//!        32 bytes    SHA-256 of everything before
//! ```
//!
//! A pack becomes visible when its index appears: [`publish`] renames the pack
//! into place first and the index second, so a reader that sees an index
//! always finds its pack, and a pack without an index is an interrupted
//! publish that readers ignore. Packs are never modified; consolidation
//! writes a new one and only then removes the old.

use super::{
    Id, Object, ObjectKind,
    codec::{Cursor, put_varint, varint_len},
    error::{Corruption, Invalid, Result, StoreError},
};
use crate::engine::fs::{Durability, Fs};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
};

pub const PACK_MAGIC: &[u8; 4] = b"RDPK";
pub const IDX_MAGIC: &[u8; 4] = b"RDIX";
pub const VERSION: u32 = 1;
/// Magic, version and count.
pub const PACK_HEADER_LEN: u64 = 12;
pub const TRAILER_LEN: u64 = 32;
const FANOUT_LEN: usize = 256;
/// Magic, version, fan-out.
const IDX_HEADER_LEN: usize = 8 + FANOUT_LEN * 4;
/// Id, offset, length.
const IDX_RECORD_LEN: usize = 32 + 8 + 4;
/// Pack checksum and trailer.
const IDX_FOOTER_LEN: usize = 64;

/// A pack's name: `pack-` and a ULID in its canonical uppercase spelling, so
/// names sort by creation and two writers never pick the same one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackName(String);

impl PackName {
    /// A fresh name.
    pub fn generate() -> Self {
        Self(format!("pack-{}", ulid::Ulid::new()))
    }

    /// The name, if `text` is exactly one.
    pub fn parse(text: &str) -> Option<Self> {
        let ulid = text.strip_prefix("pack-")?;
        let parsed = ulid::Ulid::from_string(ulid).ok()?;
        (parsed.to_string() == ulid).then(|| Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn pack_file(&self) -> String {
        format!("{}.pack", self.0)
    }

    pub fn idx_file(&self) -> String {
        format!("{}.idx", self.0)
    }
}

impl std::fmt::Display for PackName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Objects gathered for one pack, each kept once.
#[derive(Debug, Default)]
pub struct PackWriter {
    objects: Vec<(Id, Object)>,
    index: HashMap<Id, usize>,
}

impl PackWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an object and return its id. Adding bytes already present is a
    /// no-op: the first role recorded for them stays (see [`ObjectKind`]).
    pub fn add(&mut self, kind: ObjectKind, bytes: Vec<u8>) -> Result<Id> {
        self.add_object(Object::new(kind, bytes))
    }

    pub fn add_object(&mut self, object: Object) -> Result<Id> {
        if u32::try_from(object.bytes.len()).is_err() {
            return Err(StoreError::Invalid(Invalid::ObjectTooLarge(
                object.bytes.len() as u64,
            )));
        }
        let id = object.id();
        if self.index.contains_key(&id) {
            return Ok(id);
        }
        if self.objects.len() >= u32::MAX as usize {
            return Err(StoreError::Invalid(Invalid::TooManyObjects));
        }
        self.index.insert(id, self.objects.len());
        self.objects.push((id, object));
        Ok(id)
    }

    pub fn contains(&self, id: &Id) -> bool {
        self.index.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// The pack's bytes and its index's bytes.
    pub fn encode(&self) -> Result<(Vec<u8>, Vec<u8>)> {
        if self.objects.is_empty() {
            return Err(StoreError::Invalid(Invalid::EmptyPack));
        }
        let count = u32::try_from(self.objects.len())
            .map_err(|_| StoreError::Invalid(Invalid::TooManyObjects))?;
        let mut pack = Vec::new();
        pack.extend_from_slice(PACK_MAGIC);
        pack.extend_from_slice(&VERSION.to_le_bytes());
        pack.extend_from_slice(&count.to_le_bytes());
        let mut records = Vec::with_capacity(self.objects.len());
        for (id, object) in &self.objects {
            let offset = pack.len() as u64;
            pack.push(object.kind.code());
            put_varint(&mut pack, object.bytes.len() as u64);
            pack.extend_from_slice(&object.bytes);
            // `add_object` refused anything longer.
            records.push((*id, offset, object.bytes.len() as u32));
        }
        let checksum: [u8; 32] = Sha256::digest(&pack).into();
        pack.extend_from_slice(&checksum);

        records.sort_unstable_by_key(|record| record.0);
        let mut fanout = [0u32; FANOUT_LEN];
        for (id, _, _) in &records {
            fanout[id.0[0] as usize] += 1;
        }
        let mut running = 0;
        for slot in &mut fanout {
            running += *slot;
            *slot = running;
        }
        let mut idx =
            Vec::with_capacity(IDX_HEADER_LEN + records.len() * IDX_RECORD_LEN + IDX_FOOTER_LEN);
        idx.extend_from_slice(IDX_MAGIC);
        idx.extend_from_slice(&VERSION.to_le_bytes());
        for slot in fanout {
            idx.extend_from_slice(&slot.to_le_bytes());
        }
        for (id, _, _) in &records {
            idx.extend_from_slice(&id.0);
        }
        for (_, offset, _) in &records {
            idx.extend_from_slice(&offset.to_le_bytes());
        }
        for (_, _, len) in &records {
            idx.extend_from_slice(&len.to_le_bytes());
        }
        idx.extend_from_slice(&checksum);
        let trailer: [u8; 32] = Sha256::digest(&idx).into();
        idx.extend_from_slice(&trailer);
        Ok((pack, idx))
    }

    /// Write the pack and its index into `dir` as new files, each synced
    /// fully on its own write handle. Returns their paths. Nothing here is
    /// visible to readers: that is [`publish`]'s job.
    pub fn write_to(&self, fs: &dyn Fs, dir: &Path, name: &PackName) -> Result<(PathBuf, PathBuf)> {
        let (pack, idx) = self.encode()?;
        let pack_path = dir.join(name.pack_file());
        let idx_path = dir.join(name.idx_file());
        fs.create_synced(&pack_path, &pack, Durability::Full)
            .map_err(|e| StoreError::io("write pack", &pack_path, e))?;
        fs.create_synced(&idx_path, &idx, Durability::Full)
            .map_err(|e| StoreError::io("write index", &idx_path, e))?;
        Ok((pack_path, idx_path))
    }
}

/// Move a written pack from `from_dir` into `pack_dir`: the pack first, then
/// its index, then the directory made durable. Returns the published paths.
///
/// Re-running after a crash completes the publish: a file already moved is
/// recognised by its absence from `from_dir` and its presence in `pack_dir`.
/// A published file is never replaced.
pub fn publish(
    fs: &dyn Fs,
    from_dir: &Path,
    pack_dir: &Path,
    name: &PackName,
) -> Result<(PathBuf, PathBuf)> {
    let pack = move_into_place(fs, from_dir, pack_dir, &name.pack_file())?;
    let idx = move_into_place(fs, from_dir, pack_dir, &name.idx_file())?;
    fs.sync_dir(pack_dir)
        .map_err(|e| StoreError::io("sync directory", pack_dir, e))?;
    Ok((pack, idx))
}

/// Rename `file` from `from_dir` into `pack_dir`, or recognise that an
/// earlier run already did.
fn move_into_place(fs: &dyn Fs, from_dir: &Path, pack_dir: &Path, file: &str) -> Result<PathBuf> {
    let from = from_dir.join(file);
    let to = pack_dir.join(file);
    match (exists(fs, &from)?, exists(fs, &to)?) {
        (true, false) => fs
            .rename(&from, &to)
            .map_err(|e| StoreError::io("publish", &to, e))?,
        (true, true) => return Err(StoreError::Invalid(Invalid::PackExists(to))),
        (false, true) => {}
        (false, false) => {
            return Err(StoreError::io(
                "publish",
                &from,
                io::Error::from(io::ErrorKind::NotFound),
            ));
        }
    }
    Ok(to)
}

fn exists(fs: &dyn Fs, path: &Path) -> Result<bool> {
    match fs.metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(StoreError::io("inspect", path, e)),
    }
}

/// A parsed, validated index.
#[derive(Debug, Clone)]
pub struct Index {
    fanout: [u32; FANOUT_LEN],
    ids: Vec<Id>,
    offsets: Vec<u64>,
    lengths: Vec<u32>,
    pack_checksum: [u8; 32],
}

impl Index {
    /// Parse an index, proving everything it can about itself: magic,
    /// version, trailer, a non-decreasing fan-out that agrees with each id's
    /// first byte, strictly ascending ids, and a size exactly what the fan-out
    /// implies. The trailer is checked first, so these structural checks
    /// guard against a writer's bug rather than against damage.
    pub fn parse(bytes: &[u8]) -> Result<Self, Corruption> {
        if bytes.len() < IDX_HEADER_LEN + IDX_FOOTER_LEN {
            return Err(Corruption::Truncated);
        }
        let mut cursor = Cursor::new(bytes);
        if &cursor.array::<4>()? != IDX_MAGIC {
            return Err(Corruption::BadMagic);
        }
        let version = cursor.u32_le()?;
        if version != VERSION {
            return Err(Corruption::UnsupportedVersion(version));
        }
        let (body, trailer) = bytes.split_at(bytes.len() - 32);
        if Sha256::digest(body).as_slice() != trailer {
            return Err(Corruption::TrailerMismatch);
        }
        let mut fanout = [0u32; FANOUT_LEN];
        let mut previous = 0;
        for (byte, slot) in fanout.iter_mut().enumerate() {
            *slot = cursor.u32_le()?;
            if *slot < previous {
                return Err(Corruption::FanoutNotMonotonic { byte: byte as u8 });
            }
            previous = *slot;
        }
        let count = fanout[FANOUT_LEN - 1] as u64;
        let expected =
            IDX_HEADER_LEN as u64 + count * IDX_RECORD_LEN as u64 + IDX_FOOTER_LEN as u64;
        if expected != bytes.len() as u64 {
            return Err(Corruption::WrongSize {
                expected,
                actual: bytes.len() as u64,
            });
        }
        let count = count as usize;
        let mut ids = Vec::with_capacity(count);
        for index in 0..count {
            let id = Id(cursor.array()?);
            if let Some(last) = ids.last() {
                if id == *last {
                    return Err(Corruption::DuplicateId { id });
                }
                if id < *last {
                    return Err(Corruption::UnsortedIds {
                        index: index as u64,
                    });
                }
            }
            let first = id.0[0] as usize;
            let start = if first == 0 {
                0
            } else {
                fanout[first - 1] as usize
            };
            if index < start || index >= fanout[first] as usize {
                return Err(Corruption::FanoutMisplaced {
                    index: index as u64,
                });
            }
            ids.push(id);
        }
        let mut offsets = Vec::with_capacity(count);
        for _ in 0..count {
            offsets.push(cursor.u64_le()?);
        }
        let mut lengths = Vec::with_capacity(count);
        for _ in 0..count {
            lengths.push(cursor.u32_le()?);
        }
        let pack_checksum = cursor.array()?;
        Ok(Self {
            fanout,
            ids,
            offsets,
            lengths,
            pack_checksum,
        })
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn ids(&self) -> &[Id] {
        &self.ids
    }

    pub fn pack_checksum(&self) -> &[u8; 32] {
        &self.pack_checksum
    }

    /// The position of `id`, through the fan-out and a binary search.
    pub fn find(&self, id: &Id) -> Option<usize> {
        let first = id.0[0] as usize;
        let start = if first == 0 {
            0
        } else {
            self.fanout[first - 1] as usize
        };
        let end = self.fanout[first] as usize;
        self.ids[start..end]
            .binary_search(id)
            .ok()
            .map(|at| start + at)
    }

    pub fn offset(&self, at: usize) -> u64 {
        self.offsets[at]
    }

    pub fn length(&self, at: usize) -> u32 {
        self.lengths[at]
    }

    /// The bytes entry `at` spans in the pack: kind, length, content.
    pub fn entry_len(&self, at: usize) -> u64 {
        let len = u64::from(self.lengths[at]);
        1 + varint_len(len) as u64 + len
    }

    /// Every entry lies wholly between the pack's header and its trailer.
    pub fn check_bounds(&self, pack_len: u64) -> Result<(), Corruption> {
        let end = pack_len
            .checked_sub(TRAILER_LEN)
            .filter(|end| *end >= PACK_HEADER_LEN)
            .ok_or(Corruption::Truncated)?;
        for at in 0..self.ids.len() {
            let offset = self.offsets[at];
            let fits = offset >= PACK_HEADER_LEN
                && offset
                    .checked_add(self.entry_len(at))
                    .is_some_and(|stop| stop <= end);
            if !fits {
                return Err(Corruption::EntryOutOfBounds { id: self.ids[at] });
            }
        }
        Ok(())
    }
}

/// Decode one entry read from a pack at the offset its index records, and
/// prove its content is the object its id names.
pub fn decode_entry(bytes: &[u8], id: Id, length: u32) -> Result<Object, Corruption> {
    let mut cursor = Cursor::new(bytes);
    let code = cursor.u8()?;
    let kind = ObjectKind::from_code(code).ok_or(Corruption::UnknownObjectKind(code))?;
    let len = cursor
        .varint()
        .map_err(|_| Corruption::EntryHeaderMismatch { id })?;
    if len != u64::from(length) || cursor.remaining() as u64 != len {
        return Err(Corruption::EntryHeaderMismatch { id });
    }
    let content = cursor.take(len as usize)?;
    let actual = Id::of(content);
    if actual != id {
        return Err(Corruption::HashMismatch {
            expected: id,
            actual,
        });
    }
    Ok(Object::new(kind, content.to_vec()))
}

/// Check the parts of a pack's header that an index can be held against.
pub fn check_header(header: &[u8], index: &Index) -> Result<(), Corruption> {
    let mut cursor = Cursor::new(header);
    if &cursor.array::<4>()? != PACK_MAGIC {
        return Err(Corruption::BadMagic);
    }
    let version = cursor.u32_le()?;
    if version != VERSION {
        return Err(Corruption::UnsupportedVersion(version));
    }
    let count = u64::from(cursor.u32_le()?);
    if count != index.len() as u64 {
        return Err(Corruption::CountMismatch {
            idx: index.len() as u64,
            pack: count,
        });
    }
    Ok(())
}

/// Prove a whole pack: header, trailer, and that its entries, read in order,
/// are exactly the entries its index records -- each where the index says,
/// each hashing to its id, none extra, none missing.
pub fn verify_pack(bytes: &[u8], index: &Index) -> Result<(), Corruption> {
    if (bytes.len() as u64) < PACK_HEADER_LEN + TRAILER_LEN {
        return Err(Corruption::Truncated);
    }
    check_header(&bytes[..PACK_HEADER_LEN as usize], index)?;
    let (body, trailer) = bytes.split_at(bytes.len() - TRAILER_LEN as usize);
    if Sha256::digest(body).as_slice() != trailer {
        return Err(Corruption::TrailerMismatch);
    }
    if trailer != index.pack_checksum() {
        return Err(Corruption::PackChecksumMismatch);
    }
    let mut cursor = Cursor::new(body);
    cursor.take(PACK_HEADER_LEN as usize)?;
    for entry in 0..index.len() as u64 {
        let offset = cursor.position() as u64;
        let code = cursor.u8()?;
        ObjectKind::from_code(code).ok_or(Corruption::UnknownObjectKind(code))?;
        let content = cursor.length_prefixed()?;
        let id = Id::of(content);
        let matches = index.find(&id).is_some_and(|at| {
            index.offset(at) == offset && u64::from(index.length(at)) == content.len() as u64
        });
        if !matches {
            return Err(Corruption::PackEntryMismatch { index: entry });
        }
    }
    cursor.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer(objects: &[&[u8]]) -> PackWriter {
        let mut writer = PackWriter::new();
        for bytes in objects {
            writer.add(ObjectKind::Blob, bytes.to_vec()).unwrap();
        }
        writer
    }

    /// Rewrite an index's trailer after tampering, so the structural checks --
    /// not the checksum -- are what the tampering meets.
    pub(crate) fn reseal(idx: &mut [u8]) {
        let end = idx.len() - 32;
        let digest = Sha256::digest(&idx[..end]);
        idx[end..].copy_from_slice(&digest);
    }

    #[test]
    fn test8210_a_pack_round_trips_every_object_through_its_index() {
        let objects: Vec<Vec<u8>> = (0..300u32)
            .map(|n| format!("object {}", n % 50).into_bytes())
            .chain([vec![]])
            .collect();
        let mut writer = PackWriter::new();
        for bytes in &objects {
            writer.add(ObjectKind::Node, bytes.clone()).unwrap();
        }
        assert_eq!(writer.len(), 51, "each distinct object is kept once");
        let (pack, idx) = writer.encode().unwrap();
        let index = Index::parse(&idx).unwrap();
        verify_pack(&pack, &index).unwrap();
        for bytes in &objects {
            let id = Id::of(bytes);
            let at = index.find(&id).expect("every object is indexed");
            let start = index.offset(at) as usize;
            let entry = &pack[start..start + index.entry_len(at) as usize];
            let object = decode_entry(entry, id, index.length(at)).unwrap();
            assert_eq!(&object.bytes, bytes);
            assert_eq!(object.kind, ObjectKind::Node);
        }
        assert_eq!(index.find(&Id::of(b"absent")), None);
    }

    #[test]
    fn test8211_the_first_role_recorded_for_identical_bytes_stays() {
        let mut writer = PackWriter::new();
        let a = writer.add(ObjectKind::Blob, b"same".to_vec()).unwrap();
        let b = writer.add(ObjectKind::Node, b"same".to_vec()).unwrap();
        assert_eq!(a, b);
        assert_eq!(writer.len(), 1);
        let (pack, idx) = writer.encode().unwrap();
        let index = Index::parse(&idx).unwrap();
        assert_eq!(pack[index.offset(0) as usize], ObjectKind::Blob.code());
    }

    #[test]
    fn test8212_an_empty_pack_is_refused() {
        assert!(matches!(
            PackWriter::new().encode(),
            Err(StoreError::Invalid(Invalid::EmptyPack))
        ));
    }

    #[test]
    fn test8213_pack_names_have_one_spelling() {
        let name = PackName::generate();
        assert_eq!(PackName::parse(name.as_str()), Some(name.clone()));
        assert!(name.pack_file().ends_with(".pack") && name.idx_file().ends_with(".idx"));
        for bad in [
            "pack-",
            "pack-123",
            "pack-01arz3ndektsv4rrffq69g5fav",
            "PACK-01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "pack-01ARZ3NDEKTSV4RRFFQ69G5FAV.pack",
        ] {
            assert_eq!(PackName::parse(bad), None, "{bad}");
        }
        assert!(PackName::parse("pack-01ARZ3NDEKTSV4RRFFQ69G5FAV").is_some());
    }

    #[test]
    fn test8214_index_damage_is_named_precisely() {
        let (_, idx) = writer(&[b"a", b"b", b"c", b"d", b"e"]).encode().unwrap();
        // Any flipped byte is caught by the trailer.
        for at in [10, 300, idx.len() - 40, idx.len() - 1] {
            let mut damaged = idx.clone();
            damaged[at] ^= 1;
            assert_eq!(
                Index::parse(&damaged).unwrap_err(),
                Corruption::TrailerMismatch,
                "byte {at}"
            );
        }
        let mut magic = idx.clone();
        magic[0] = b'X';
        assert_eq!(Index::parse(&magic).unwrap_err(), Corruption::BadMagic);
        let mut version = idx.clone();
        version[4] = 2;
        assert_eq!(
            Index::parse(&version).unwrap_err(),
            Corruption::UnsupportedVersion(2)
        );
        assert_eq!(
            Index::parse(&idx[..100]).unwrap_err(),
            Corruption::Truncated
        );
        // A truncation that keeps a valid-looking trailer is caught by size.
        let mut short = idx[..idx.len() - 44 - 32].to_vec();
        short.extend_from_slice(&[0; 32]);
        reseal(&mut short);
        assert!(matches!(
            Index::parse(&short).unwrap_err(),
            Corruption::WrongSize { .. }
        ));
    }

    fn ids_region(idx: &[u8]) -> std::ops::Range<usize> {
        let count = u32::from_le_bytes(idx[8 + 255 * 4..8 + 256 * 4].try_into().unwrap()) as usize;
        IDX_HEADER_LEN..IDX_HEADER_LEN + count * 32
    }

    /// Two objects whose ids share a first byte, the smaller id first.
    fn same_first_byte() -> (Vec<u8>, Vec<u8>) {
        let mut seen: HashMap<u8, Vec<u8>> = HashMap::new();
        let mut n = 0u32;
        loop {
            let bytes = n.to_le_bytes().to_vec();
            if let Some(other) = seen.insert(Id::of(&bytes).0[0], bytes.clone()) {
                return if Id::of(&other) < Id::of(&bytes) {
                    (other, bytes)
                } else {
                    (bytes, other)
                };
            }
            n += 1;
        }
    }

    #[test]
    fn test8215_structurally_inconsistent_indexes_are_refused_even_when_sealed() {
        let (low, high) = same_first_byte();
        let (_, idx) = writer(&[&low, &high]).encode().unwrap();
        let ids = ids_region(&idx);
        let (first, second) = (ids.start, ids.start + 32);

        let mut unsorted = idx.clone();
        unsorted[first..first + 32].copy_from_slice(&Id::of(&high).0);
        unsorted[second..second + 32].copy_from_slice(&Id::of(&low).0);
        reseal(&mut unsorted);
        assert_eq!(
            Index::parse(&unsorted).unwrap_err(),
            Corruption::UnsortedIds { index: 1 }
        );

        let mut duplicate = idx.clone();
        duplicate[second..second + 32].copy_from_slice(&Id::of(&low).0);
        reseal(&mut duplicate);
        assert_eq!(
            Index::parse(&duplicate).unwrap_err(),
            Corruption::DuplicateId { id: Id::of(&low) }
        );

        // One fan-out slot above the next: no longer non-decreasing.
        let mut fanout = idx.clone();
        let slot = 8 + 10 * 4;
        fanout[slot..slot + 4].copy_from_slice(&1000u32.to_le_bytes());
        reseal(&mut fanout);
        assert_eq!(
            Index::parse(&fanout).unwrap_err(),
            Corruption::FanoutNotMonotonic { byte: 11 }
        );
    }

    #[test]
    fn test8216_an_index_whose_count_disagrees_with_its_size_is_refused() {
        let (_, mut idx) = writer(&[b"a", b"b"]).encode().unwrap();
        // Claim a third object in the last fan-out slot only.
        let last = 8 + 255 * 4;
        idx[last..last + 4].copy_from_slice(&3u32.to_le_bytes());
        reseal(&mut idx);
        let actual = idx.len() as u64;
        assert_eq!(
            Index::parse(&idx).unwrap_err(),
            Corruption::WrongSize {
                expected: actual + 44,
                actual
            }
        );
    }

    #[test]
    fn test8217_a_fanout_that_disagrees_with_the_ids_is_refused() {
        let (low, high) = (Id::of(b"x"), Id::of(b"y"));
        assert_ne!(low.0[0], high.0[0], "the two ids need distinct first bytes");
        let (_, mut idx) = writer(&[b"x", b"y"]).encode().unwrap();
        // Count both ids under the smaller first byte: the fan-out stays
        // non-decreasing and totals two, but the second id's first byte no
        // longer owns its position.
        let smaller = low.0[0].min(high.0[0]) as usize;
        for byte in 0..256usize {
            let count: u32 = if byte < smaller { 0 } else { 2 };
            idx[8 + byte * 4..12 + byte * 4].copy_from_slice(&count.to_le_bytes());
        }
        reseal(&mut idx);
        let error = Index::parse(&idx).unwrap_err();
        assert_eq!(error, Corruption::FanoutMisplaced { index: 1 });
    }

    #[test]
    fn test8218_pack_damage_is_named_precisely() {
        let w = writer(&[b"alpha", b"beta", b"gamma"]);
        let (pack, idx) = w.encode().unwrap();
        let index = Index::parse(&idx).unwrap();
        let mut flipped = pack.clone();
        flipped[20] ^= 1;
        assert_eq!(
            verify_pack(&flipped, &index).unwrap_err(),
            Corruption::TrailerMismatch
        );
        assert_eq!(
            verify_pack(&pack[..20], &index).unwrap_err(),
            Corruption::Truncated
        );
        let mut magic = pack.clone();
        magic[1] = b'X';
        assert_eq!(
            verify_pack(&magic, &index).unwrap_err(),
            Corruption::BadMagic
        );
        let mut count = pack.clone();
        count[8] = 9;
        assert_eq!(
            verify_pack(&count, &index).unwrap_err(),
            Corruption::CountMismatch { idx: 3, pack: 9 }
        );
        // A pack whose trailer is intact but which is not the pack the index
        // was written for.
        let (other, _) = writer(&[b"alpha", b"beta", b"delta"]).encode().unwrap();
        assert!(matches!(
            verify_pack(&other, &index).unwrap_err(),
            Corruption::PackChecksumMismatch
        ));
        // An entry's content swapped for other content of the same length,
        // resealed: the entry no longer matches the index.
        let mut swapped = pack.clone();
        let at = index.find(&Id::of(b"gamma")).unwrap();
        let start = index.offset(at) as usize + 2;
        swapped[start..start + 5].copy_from_slice(b"GAMMA");
        let end = swapped.len() - 32;
        let digest = Sha256::digest(&swapped[..end]);
        swapped[end..].copy_from_slice(&digest);
        let mut rebound = idx.clone();
        let checksum_at = rebound.len() - 64;
        rebound[checksum_at..checksum_at + 32].copy_from_slice(&digest);
        reseal(&mut rebound);
        let rebound = Index::parse(&rebound).unwrap();
        assert!(matches!(
            verify_pack(&swapped, &rebound).unwrap_err(),
            Corruption::PackEntryMismatch { .. }
        ));
    }

    #[test]
    fn test8219_an_entry_is_held_to_its_id_kind_and_length() {
        let (pack, idx) = writer(&[b"payload"]).encode().unwrap();
        let index = Index::parse(&idx).unwrap();
        let id = Id::of(b"payload");
        let start = index.offset(0) as usize;
        let entry = pack[start..start + index.entry_len(0) as usize].to_vec();
        assert_eq!(decode_entry(&entry, id, 7).unwrap().bytes, b"payload");
        let mut kind = entry.clone();
        kind[0] = 9;
        assert_eq!(
            decode_entry(&kind, id, 7).unwrap_err(),
            Corruption::UnknownObjectKind(9)
        );
        let mut content = entry.clone();
        content[3] ^= 0x20;
        assert!(matches!(
            decode_entry(&content, id, 7).unwrap_err(),
            Corruption::HashMismatch { expected, .. } if expected == id
        ));
        assert_eq!(
            decode_entry(&entry, id, 6).unwrap_err(),
            Corruption::EntryHeaderMismatch { id }
        );
        assert!(index.check_bounds(pack.len() as u64).is_ok());
        assert_eq!(
            index.check_bounds(pack.len() as u64 - 1).unwrap_err(),
            Corruption::EntryOutOfBounds { id }
        );
        assert_eq!(index.check_bounds(10).unwrap_err(), Corruption::Truncated);
    }
}
