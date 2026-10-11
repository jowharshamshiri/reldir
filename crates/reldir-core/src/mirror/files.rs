//! Each file the mirror knows: its stamp, its row, its diagnostics; taking files in and forgetting them.

use super::*;

impl Mirror {
    /// The mirror's record of a file, if it has one.
    pub fn file(&self, path: &str) -> Result<Option<FileEntry>> {
        self.conn
            .query_row(
                "SELECT path, tbl, size, mtime_ns, ctime_ns, inode, device, raw_hash, seen_ns, \
                 row_hash, doc, diagnostics FROM _reldir_files WHERE path = ?1",
                [path],
                file_entry,
            )
            .optional()
            .map_err(corrupt)?
            .transpose()
    }

    /// The stamp of every file the mirror knows in a table, in one query: the
    /// stat pass compares these in memory and fetches nothing for a file its
    /// stamp still vouches for.
    pub fn stamps(&self, table: &str) -> Result<HashMap<String, Stamp>> {
        let mut statement = self
            .conn
            .prepare_cached(
                "SELECT path, size, mtime_ns, ctime_ns, inode, device, raw_hash, seen_ns \
                 FROM _reldir_files WHERE tbl = ?1",
            )
            .map_err(corrupt)?;
        let stamps = statement
            .query_map([table], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    Stamp {
                        stat: Stat {
                            size: row.get::<_, i64>(1)? as u64,
                            mtime_ns: row.get(2)?,
                            ctime_ns: row.get(3)?,
                            inode: row.get::<_, i64>(4)? as u64,
                            device: row.get::<_, i64>(5)? as u64,
                        },
                        raw_hash: row.get(6)?,
                        seen_ns: row.get(7)?,
                    },
                ))
            })
            .map_err(corrupt)?
            .collect::<std::result::Result<HashMap<_, _>, _>>()
            .map_err(corrupt)?;
        Ok(stamps)
    }

    /// Every path the mirror knows in a table.
    /// The files of a table whose names fold to `folded`, but `except`.
    pub fn same_folded(&self, table: &str, folded: &str, except: &str) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT path FROM _reldir_files WHERE tbl = ?1 AND folded = ?2 AND path <> ?3 ORDER BY path")
            .map_err(corrupt)?;
        let paths = statement
            .query_map(params![table, folded, except], |row| row.get(0))
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<String>, _>>()
            .map_err(corrupt)?;
        Ok(paths)
    }

    /// The files of a table written in another style than `style`.
    pub fn off_style(&self, table: &str, style: &str) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT path FROM _reldir_files WHERE tbl = ?1 AND style IS NOT ?2 ORDER BY path",
            )
            .map_err(corrupt)?;
        let paths = statement
            .query_map(params![table, style], |row| row.get(0))
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<String>, _>>()
            .map_err(corrupt)?;
        Ok(paths)
    }

    pub fn paths(&self, table: &str) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT path FROM _reldir_files WHERE tbl = ?1 ORDER BY path")
            .map_err(corrupt)?;
        let paths = statement
            .query_map([table], |row| row.get(0))
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<String>, _>>()
            .map_err(corrupt)?;
        Ok(paths)
    }

    /// Visit every file of a table in path order.
    pub fn each_file(
        &self,
        table: &str,
        mut visit: impl FnMut(FileEntry) -> Result<()>,
    ) -> Result<()> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT path, tbl, size, mtime_ns, ctime_ns, inode, device, raw_hash, seen_ns, \
                 row_hash, doc, diagnostics FROM _reldir_files WHERE tbl = ?1 ORDER BY path",
            )
            .map_err(corrupt)?;
        let mut rows = statement.query([table]).map_err(corrupt)?;
        while let Some(row) = rows.next().map_err(corrupt)? {
            visit(file_entry(row).map_err(corrupt)??)?;
        }
        Ok(())
    }

    /// Mark an unchanged file as seen now, so it stays out of the racy window.
    pub fn touch(&self, path: &str, stat: &Stat, seen_ns: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE _reldir_files SET size = ?2, mtime_ns = ?3, ctime_ns = ?4, inode = ?5, \
                 device = ?6, seen_ns = ?7 WHERE path = ?1",
                params![
                    path,
                    stat.size as i64,
                    stat.mtime_ns,
                    stat.ctime_ns,
                    stat.inode as i64,
                    stat.device as i64,
                    seen_ns
                ],
            )
            .map(|_| ())
            .map_err(corrupt)
    }

    /// Forget a file.
    pub fn remove(&self, path: &str) -> Result<()> {
        let trow: Option<(String, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT tbl, trow FROM _reldir_files WHERE path = ?1",
                [path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(corrupt)?;
        if let Some((table, Some(rowid))) = &trow {
            self.conn
                .execute(
                    &format!("DELETE FROM {} WHERE rowid = ?1", quote(table)),
                    [rowid],
                )
                .map_err(corrupt)?;
        }
        for statement in [
            "DELETE FROM _reldir_edges WHERE path = ?1",
            "DELETE FROM _reldir_keys WHERE path = ?1",
            "DELETE FROM _reldir_files WHERE path = ?1",
        ] {
            self.conn.execute(statement, [path]).map_err(corrupt)?;
        }
        Ok(())
    }

    /// Record a file: its stat, its row in the typed table, its keys, and the
    /// references it makes.
    pub fn put(&self, ingest: Ingest<'_>) -> Result<()> {
        self.remove(ingest.path)?;
        let schema = ingest.schema;
        let (doc, row_hash, trow) = match ingest.row {
            Some(row) => {
                let hash = canonical::row_hash(&Value::Object(row.clone()));
                let names: Vec<&String> = schema.columns().keys().collect();
                let values: Vec<SqlValue> = schema
                    .columns()
                    .iter()
                    .map(|(name, column)| {
                        crate::index::types::to_sql(
                            row.get(name).or(column.default()).unwrap_or(&Value::Null),
                            column.kind(),
                        )
                    })
                    .collect();
                let insert = format!(
                    "INSERT INTO {} ({}) VALUES ({})",
                    quote(ingest.table),
                    names
                        .iter()
                        .map(|n| quote(n))
                        .collect::<Vec<_>>()
                        .join(", "),
                    vec!["?"; names.len()].join(", ")
                );
                match self
                    .conn
                    .execute(&insert, rusqlite::params_from_iter(values.clone()))
                {
                    Ok(_) => {}
                    // A duplicate key in the files is a fault to report, not a
                    // row to lose: the key indexes stop being unique until the
                    // duplicate is gone.
                    Err(rusqlite::Error::SqliteFailure(failure, _))
                        if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
                    {
                        self.key_indexes(schema, false)?;
                        self.conn
                            .execute(&insert, rusqlite::params_from_iter(values))
                            .map_err(corrupt)?;
                    }
                    Err(error) => return Err(corrupt(error)),
                }
                let trow = self.conn.last_insert_rowid();
                for (constraint, columns) in constraints(schema) {
                    if let Some(key) = key(row, &columns, schema) {
                        self.conn
                            .execute(
                                "INSERT INTO _reldir_keys (tbl, cons, key, path) VALUES (?1, ?2, ?3, ?4)",
                                params![ingest.table, constraint, key, ingest.path],
                            )
                            .map_err(corrupt)?;
                    }
                }
                for edge in edges(row, schema) {
                    self.conn
                        .execute(
                            "INSERT OR IGNORE INTO _reldir_edges (path, rule, ptr, target) \
                             VALUES (?1, ?2, ?3, ?4)",
                            params![ingest.path, edge.rule, edge.pointer, edge.target],
                        )
                        .map_err(corrupt)?;
                }
                (
                    Some(serde_json::to_string(row).map_err(corrupt)?),
                    Some(hash),
                    Some(trow),
                )
            }
            None => (None, None, None),
        };
        let diagnostics = serde_json::to_string(&ingest.diagnostics).map_err(corrupt)?;
        self.conn
            .execute(
                "INSERT INTO _reldir_files (path, tbl, size, mtime_ns, ctime_ns, inode, device, \
                 raw_hash, seen_ns, row_hash, doc, diagnostics, trow, style, folded) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    ingest.path,
                    ingest.table,
                    ingest.stat.size as i64,
                    ingest.stat.mtime_ns,
                    ingest.stat.ctime_ns,
                    ingest.stat.inode as i64,
                    ingest.stat.device as i64,
                    ingest.raw_hash,
                    ingest.seen_ns,
                    row_hash,
                    doc,
                    diagnostics,
                    trow,
                    ingest.style,
                    crate::catalog::fold(ingest.path.rsplit('/').next().unwrap_or(ingest.path)),
                ],
            )
            .map_err(corrupt)?;
        Ok(())
    }

    /// The path of the file whose typed row has this rowid.
    pub fn path_of_rowid(&self, table: &str, rowid: i64) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT path FROM _reldir_files WHERE tbl = ?1 AND trow = ?2",
                params![table, rowid],
                |row| row.get(0),
            )
            .optional()
            .map_err(corrupt)
    }
}

pub(super) fn decode_diagnostics(text: &str) -> Result<Vec<Diagnostic>> {
    let values: Vec<Value> = serde_json::from_str(text).map_err(corrupt)?;
    values
        .into_iter()
        .map(|value| Diagnostic::from_json(&value).map_err(corrupt))
        .collect()
}

pub(super) fn file_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<FileEntry>> {
    let doc: Option<String> = row.get(10)?;
    let diagnostics: String = row.get(11)?;
    Ok((|| -> Result<FileEntry> {
        Ok(FileEntry {
            path: row.get(0).map_err(corrupt)?,
            table: row.get(1).map_err(corrupt)?,
            stat: Stat {
                size: row.get::<_, i64>(2).map_err(corrupt)? as u64,
                mtime_ns: row.get(3).map_err(corrupt)?,
                ctime_ns: row.get(4).map_err(corrupt)?,
                inode: row.get::<_, i64>(5).map_err(corrupt)? as u64,
                device: row.get::<_, i64>(6).map_err(corrupt)? as u64,
            },
            raw_hash: row.get(7).map_err(corrupt)?,
            seen_ns: row.get(8).map_err(corrupt)?,
            row_hash: row.get(9).map_err(corrupt)?,
            doc: doc
                .map(|text| {
                    serde_json::from_str::<Value>(&text)
                        .map_err(corrupt)
                        .and_then(|value| match value {
                            Value::Object(map) => Ok(map),
                            _ => Err(corrupt("a mirrored row is not an object")),
                        })
                })
                .transpose()?,
            diagnostics: decode_diagnostics(&diagnostics)?,
        })
    })())
}
