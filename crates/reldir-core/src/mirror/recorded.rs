//! What history recorded: the entries of the recorded head, and the paths that may differ from it.

use super::*;

impl Mirror {
    /// Rows whose hash differs from the recorded head, and recorded rows no
    /// longer present: found through the paths the triggers marked unsettled,
    /// so the cost follows what changed.
    pub fn row_changes(&self) -> Result<Vec<RowChangeSince>> {
        let mut out = vec![];
        let mut changed = self
            .conn
            .prepare(
                // CROSS JOIN fixes the order: the unsettled paths -- usually
                // none -- drive the lookups, never a scan of every file.
                "SELECT f.path, f.row_hash, f.raw_hash, r.hash IS NOT NULL FROM _reldir_unsettled u \
                 CROSS JOIN _reldir_files f ON f.path = u.path \
                 LEFT JOIN _reldir_recorded r ON r.path = f.path AND r.kind = 'row' \
                 WHERE f.row_hash IS NOT NULL AND (r.hash IS NULL OR r.hash <> f.row_hash) \
                 ORDER BY f.path",
            )
            .map_err(corrupt)?;
        for row in changed
            .query_map([], |row| {
                Ok(RowChangeSince {
                    path: row.get(0)?,
                    after: Some((row.get(1)?, row.get(2)?)),
                    recorded: row.get(3)?,
                })
            })
            .map_err(corrupt)?
        {
            out.push(row.map_err(corrupt)?);
        }
        let mut removed = self
            .conn
            .prepare(
                "SELECT r.path FROM _reldir_unsettled u CROSS JOIN _reldir_recorded r ON r.path = u.path \
                 WHERE r.kind = 'row' AND NOT EXISTS \
                 (SELECT 1 FROM _reldir_files f WHERE f.path = r.path AND f.row_hash IS NOT NULL) \
                 ORDER BY r.path",
            )
            .map_err(corrupt)?;
        for row in removed
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(corrupt)?
        {
            out.push(RowChangeSince {
                path: row.map_err(corrupt)?,
                after: None,
                recorded: true,
            });
        }
        Ok(out)
    }

    /// Every row of a table as history records it: path, row hash, and the
    /// hash of the file's bytes.
    pub fn row_entries(&self, table: &str) -> Result<Vec<(String, String, String)>> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT path, row_hash, raw_hash FROM _reldir_files \
                 WHERE tbl = ?1 AND row_hash IS NOT NULL ORDER BY path",
            )
            .map_err(corrupt)?;
        let rows = statement
            .query_map([table], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(corrupt)?;
        Ok(rows)
    }

    /// The recorded entries that are not rows.
    pub fn recorded_non_rows(
        &self,
    ) -> Result<std::collections::BTreeMap<String, crate::history::RecordedEntry>> {
        let mut statement = self
            .conn
            .prepare("SELECT path, kind, hash FROM _reldir_recorded WHERE kind <> 'row'")
            .map_err(corrupt)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    crate::history::RecordedEntry {
                        kind: row.get(1)?,
                        hash: row.get(2)?,
                    },
                ))
            })
            .map_err(corrupt)?
            .collect::<std::result::Result<std::collections::BTreeMap<_, _>, _>>()
            .map_err(corrupt)?;
        Ok(rows)
    }

    /// Every path the recorded head holds.
    pub fn recorded_paths(&self) -> Result<BTreeSet<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT path FROM _reldir_recorded")
            .map_err(corrupt)?;
        let paths = statement
            .query_map([], |row| row.get(0))
            .map_err(corrupt)?
            .collect::<std::result::Result<_, _>>()
            .map_err(corrupt)?;
        Ok(paths)
    }

    /// Replace the recorded head wholesale.
    pub fn replace_recorded(
        &self,
        entries: &std::collections::BTreeMap<String, crate::history::RecordedEntry>,
        head: Option<(&crate::store::Id, &crate::store::Id)>,
    ) -> Result<()> {
        self.atomically("reldir_recorded", || {
            self.replace_recorded_inner(entries, head)
        })
    }

    /// Run `work` as one unit: all of it lands, with one flush, or none does.
    pub(super) fn atomically<T>(&self, name: &str, work: impl FnOnce() -> Result<T>) -> Result<T> {
        self.savepoint(name)?;
        match work() {
            Ok(value) => {
                self.release(name)?;
                Ok(value)
            }
            Err(error) => {
                self.rollback_to(name)?;
                Err(error)
            }
        }
    }

    pub(super) fn replace_recorded_inner(
        &self,
        entries: &std::collections::BTreeMap<String, crate::history::RecordedEntry>,
        head: Option<(&crate::store::Id, &crate::store::Id)>,
    ) -> Result<()> {
        self.conn
            .execute("DELETE FROM _reldir_recorded", [])
            .map_err(corrupt)?;
        for (path, entry) in entries {
            self.conn
                .execute(
                    "INSERT INTO _reldir_recorded (path, kind, hash) VALUES (?1, ?2, ?3)",
                    params![path, entry.kind, entry.hash],
                )
                .map_err(corrupt)?;
        }
        self.settle()?;
        match head {
            Some((id, tree)) => {
                self.set_meta("recorded_head", &id.to_hex())?;
                self.set_meta("recorded_tree", &tree.to_hex())?;
            }
            None => {
                self.conn
                    .execute(
                        "DELETE FROM _reldir_meta WHERE key IN ('recorded_head', 'recorded_tree')",
                        [],
                    )
                    .map_err(corrupt)?;
            }
        }
        Ok(())
    }

    /// Apply one recorded revision's changes to the recorded head.
    pub fn apply_recorded(
        &self,
        changes: &std::collections::BTreeMap<String, Option<crate::history::RecordedEntry>>,
        head: &crate::store::Id,
        tree: &crate::store::Id,
    ) -> Result<()> {
        self.atomically("reldir_recorded", || {
            self.apply_recorded_inner(changes, head, tree)
        })
    }

    pub(super) fn apply_recorded_inner(
        &self,
        changes: &std::collections::BTreeMap<String, Option<crate::history::RecordedEntry>>,
        head: &crate::store::Id,
        tree: &crate::store::Id,
    ) -> Result<()> {
        for (path, entry) in changes {
            match entry {
                Some(entry) => {
                    self.conn
                        .execute(
                            "INSERT INTO _reldir_recorded (path, kind, hash) VALUES (?1, ?2, ?3) \
                             ON CONFLICT (path) DO UPDATE SET kind = excluded.kind, hash = excluded.hash",
                            params![path, entry.kind, entry.hash],
                        )
                        .map_err(corrupt)?;
                }
                None => {
                    self.conn
                        .execute("DELETE FROM _reldir_recorded WHERE path = ?1", [path])
                        .map_err(corrupt)?;
                }
            }
        }
        self.settle()?;
        self.set_meta("recorded_head", &head.to_hex())?;
        self.set_meta("recorded_tree", &tree.to_hex())
    }

    /// Forget the unsettled paths whose file now holds what was recorded
    /// there -- the same row, or no row on either side.
    pub(super) fn settle(&self) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM _reldir_unsettled WHERE \
                 (SELECT f.row_hash FROM _reldir_files f WHERE f.path = _reldir_unsettled.path) IS \
                 (SELECT r.hash FROM _reldir_recorded r \
                  WHERE r.path = _reldir_unsettled.path AND r.kind = 'row')",
                [],
            )
            .map(|_| ())
            .map_err(corrupt)
    }
}
