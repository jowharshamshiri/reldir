//! A tree seen through a set of planned changes.
//!
//! What a change would leave behind is judged before anything is written: the
//! catalog observes the tree through an [`Overlay`] that answers with the
//! planned bytes where the change writes and with nothing where it deletes.
//! A reader that may write nothing sees committed-but-unfinished changes the
//! same way.

use super::{Entry, Kind, Meta, Source, Stat};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
};

/// A tree with planned writes and deletes laid over it. Planned writes appear
/// as files with fresh metadata -- no stat cache can mistake one for the file
/// it replaces -- planned deletes vanish, and directories that only planned
/// writes create appear too.
pub struct Overlay<'a> {
    base: &'a dyn Source,
    writes: BTreeMap<PathBuf, Vec<u8>>,
    deletes: BTreeSet<PathBuf>,
}

impl<'a> Overlay<'a> {
    /// `writes` and `deletes` are absolute paths.
    pub fn new(
        base: &'a dyn Source,
        writes: BTreeMap<PathBuf, Vec<u8>>,
        deletes: BTreeSet<PathBuf>,
    ) -> Self {
        Self {
            base,
            writes,
            deletes,
        }
    }

    fn created_dir(&self, path: &Path) -> bool {
        self.writes
            .keys()
            .any(|written| written.starts_with(path) && written != path)
    }

    fn written(bytes: &[u8]) -> Meta {
        Meta {
            kind: Kind::File,
            links: 1,
            stat: Stat {
                size: bytes.len() as u64,
                ..Stat::default()
            },
        }
    }
}

fn name_of(path: &Path) -> &[u8] {
    path.file_name().map_or(&[], OsStr::as_encoded_bytes)
}

impl Source for Overlay<'_> {
    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        if let Some(bytes) = self.writes.get(path) {
            return Ok(Self::written(bytes));
        }
        if self.deletes.contains(path) {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        match self.base.metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound && self.created_dir(path) => {
                Ok(Meta {
                    kind: Kind::Dir,
                    links: 1,
                    stat: Stat::default(),
                })
            }
            other => other,
        }
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<Entry>> {
        let mut entries = match self.base.read_dir(path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound && self.created_dir(path) => {
                vec![]
            }
            Err(error) => return Err(error),
        };
        entries.retain(|entry| !self.deletes.contains(&entry.path));
        // The planned changes are few beside the listing: each is placed by
        // a search in the sorted entries rather than by sorting again.
        for written in self.writes.keys() {
            let Ok(rest) = written.strip_prefix(path) else {
                continue;
            };
            let Some(first) = rest.components().next() else {
                continue;
            };
            let child = path.join(first);
            let meta = match self.writes.get(&child) {
                Some(bytes) => Self::written(bytes),
                None => self.metadata(&child)?,
            };
            match entries.binary_search_by(|entry| name_of(&entry.path).cmp(name_of(&child))) {
                Ok(at) => entries[at].meta = meta,
                Err(at) => entries.insert(at, Entry { path: child, meta }),
            }
        }
        Ok(entries)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        if let Some(bytes) = self.writes.get(path) {
            return Ok(bytes.clone());
        }
        if self.deletes.contains(path) {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        self.base.read(path)
    }

    fn read_at(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        if let Some(bytes) = self.writes.get(path) {
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            return bytes
                .get(start..start.saturating_add(len))
                .map(<[u8]>::to_vec)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "{} ends before byte {}",
                            path.display(),
                            offset + len as u64
                        ),
                    )
                });
        }
        if self.deletes.contains(path) {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        self.base.read_at(path, offset, len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::fs::{Disk, Fs};

    #[test]
    fn test8060_an_overlay_shows_planned_writes_and_hides_planned_deletes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        Disk.create_dir(&root.join("t")).unwrap();
        for name in ["a.json", "c.json"] {
            Disk.write(&root.join("t").join(name), b"{}").unwrap();
        }
        let overlay = Overlay::new(
            &Disk,
            BTreeMap::from([
                (root.join("t/b.json"), b"{\"b\":1}".to_vec()),
                (root.join("t/c.json"), b"{\"c\":22}".to_vec()),
                (root.join("u/x.json"), b"{}".to_vec()),
            ]),
            BTreeSet::from([root.join("t/a.json")]),
        );
        let names: Vec<String> = overlay
            .read_dir(&root.join("t"))
            .unwrap()
            .iter()
            .map(|entry| {
                entry
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["b.json", "c.json"], "sorted, with the delete gone");
        let c = overlay.metadata(&root.join("t/c.json")).unwrap();
        assert_eq!(c.stat.size, 8);
        assert_eq!(
            c.stat.mtime_ns, 0,
            "a planned write never matches a cached stat"
        );
        assert_eq!(
            overlay.read_at(&root.join("t/c.json"), 5, 2).unwrap(),
            b"22"
        );
        assert!(overlay.read(&root.join("t/a.json")).is_err());
        assert_eq!(
            overlay.metadata(&root.join("u")).unwrap().kind,
            Kind::Dir,
            "a directory only a planned write creates is there"
        );
    }
}
