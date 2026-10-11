//! Row-local faults, sizes, and the planner's statistics.

use super::*;

impl Mirror {
    /// Files whose row-local verdict is not clean.
    pub fn files_with_diagnostics(&self) -> Result<Vec<(String, Vec<Diagnostic>)>> {
        self.files_with_diagnostics_in(false)
    }

    /// [`Self::files_with_diagnostics`], of the scope's paths alone when
    /// `scoped`.
    pub fn files_with_diagnostics_in(
        &self,
        scoped: bool,
    ) -> Result<Vec<(String, Vec<Diagnostic>)>> {
        let within = if scoped {
            " AND path IN (SELECT path FROM temp._reldir_scope_paths)"
        } else {
            ""
        };
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT path, diagnostics FROM _reldir_files WHERE diagnostics <> '[]'{within} ORDER BY path"
            ))
            .map_err(corrupt)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(corrupt)?;
        rows.into_iter()
            .map(|(path, text)| {
                let diagnostics = decode_diagnostics(&text)?;
                Ok((path, diagnostics))
            })
            .collect()
    }

    /// The row hashes of every file in a table, in path order, for the state
    /// root.
    pub fn row_hashes(&self, table: &str) -> Result<Vec<(String, Option<String>)>> {
        let mut statement = self
            .conn
            .prepare("SELECT path, row_hash FROM _reldir_files WHERE tbl = ?1 ORDER BY path")
            .map_err(corrupt)?;
        let found = statement
            .query_map([table], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(corrupt)?;
        Ok(found)
    }

    /// Row count of a table.
    pub fn count(&self, table: &str) -> Result<u64> {
        self.conn
            .query_row(
                "SELECT count(*) FROM _reldir_files WHERE tbl = ?1",
                [table],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as u64)
            .map_err(corrupt)
    }

    /// The tables the mirror holds rows for: every table whose schema loads.
    pub(super) fn tables(&self) -> Result<Vec<String>> {
        self.conn
            .prepare("SELECT tbl FROM _reldir_tables ORDER BY tbl")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()
            })
            .map_err(corrupt)
    }

    /// Rebuild SQLite's planner statistics.
    pub fn analyze(&self) -> Result<()> {
        self.conn.execute_batch("ANALYZE").map_err(corrupt)?;
        for table in self.tables()? {
            let rows = self.count(&table)?;
            self.set_meta(&analyzed_key(&table), &rows.to_string())?;
        }
        Ok(())
    }

    /// Gather planner statistics again for each table that has none, or
    /// whose size has more than doubled or halved since they were gathered:
    /// statistics from ten rows mislead the planner about ten thousand, and
    /// gathering them after every change would scan each table each time.
    /// Returns the tables analyzed.
    pub fn analyze_drifted(&self) -> Result<Vec<String>> {
        // Below this many rows every plan is cheap; churn there is not drift.
        const FLOOR: u64 = 64;
        let mut analyzed = vec![];
        for table in &self.tables()? {
            let rows = self.count(table)?;
            let key = analyzed_key(table);
            let last = self
                .meta(&key)?
                .map(|text| {
                    text.parse::<u64>()
                        .map_err(|_| corrupt(format!("{key} is not a row count: {text:?}")))
                })
                .transpose()?;
            let drifted = match last {
                None => true,
                Some(last) => {
                    let (now, then) = (rows.max(FLOOR), last.max(FLOOR));
                    now > then.saturating_mul(2) || now.saturating_mul(2) < then
                }
            };
            if drifted {
                self.conn
                    .execute_batch(&format!("ANALYZE {}", quote(table)))
                    .map_err(corrupt)?;
                self.set_meta(&key, &rows.to_string())?;
                analyzed.push(table.clone());
            }
        }
        Ok(analyzed)
    }

    /// SQLite's own consistency check of the mirror file.
    pub fn integrity_check(&self) -> Result<Option<String>> {
        let result: String = self
            .conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(corrupt)?;
        Ok((result != "ok").then_some(result))
    }
}
