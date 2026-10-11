//! The typed tables: one per table, shaped by its schema, with its key and reference indexes.

use super::*;

impl Mirror {
    /// Bring the typed tables in line with a set of schemas. A table whose
    /// schema changed is emptied and recreated: its rows must be read again,
    /// because what they mean has changed. Returns the tables that were.
    pub fn sync_schemas(
        &self,
        schemas: &std::collections::BTreeMap<String, Schema>,
    ) -> Result<Vec<String>> {
        let known: Vec<(String, String)> = {
            let mut statement = self
                .conn
                .prepare("SELECT tbl, identity FROM _reldir_tables")
                .map_err(corrupt)?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(corrupt)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(corrupt)?
        };
        let mut rebuilt = vec![];
        for (table, identity) in &known {
            match schemas.get(table) {
                Some(schema) if &signature(schema) == identity => {}
                _ => self.drop_table(table)?,
            }
        }
        for (table, schema) in schemas {
            let current = known
                .iter()
                .any(|(name, identity)| name == table && identity == &signature(schema));
            if !current {
                self.create_table(schema)?;
                rebuilt.push(table.clone());
            }
        }
        Ok(rebuilt)
    }

    pub(super) fn drop_table(&self, table: &str) -> Result<()> {
        self.conn
            .execute_batch(&format!("DROP TABLE IF EXISTS {}", quote(table)))
            .map_err(corrupt)?;
        for statement in [
            "DELETE FROM _reldir_edges WHERE path IN (SELECT path FROM _reldir_files WHERE tbl = ?1)",
            "DELETE FROM _reldir_keys WHERE tbl = ?1",
            "DELETE FROM _reldir_files WHERE tbl = ?1",
            "DELETE FROM _reldir_tables WHERE tbl = ?1",
        ] {
            self.conn.execute(statement, [table]).map_err(corrupt)?;
        }
        self.conn
            .execute(
                "DELETE FROM _reldir_meta WHERE key = ?1",
                [analyzed_key(table)],
            )
            .map_err(corrupt)?;
        Ok(())
    }

    pub(super) fn create_table(&self, schema: &Schema) -> Result<()> {
        let table = schema.table();
        let columns: Vec<String> = schema
            .columns()
            .iter()
            .map(|(name, column)| {
                let mut definition = format!(
                    "{} {}",
                    quote(name),
                    crate::index::types::declared_type(column.kind())
                );
                if let Some(kind) = column.generated() {
                    definition.push(' ');
                    definition.push_str(&crate::index::types::generated_default(kind, table, name));
                }
                definition
            })
            .collect();
        self.conn
            .execute_batch(&format!(
                "CREATE TABLE {} ({})",
                quote(table),
                columns.join(", ")
            ))
            .map_err(corrupt)?;
        // A sequence's next value is read from the largest it holds, which an
        // index finds without a scan.
        let sequences = schema
            .columns()
            .iter()
            .filter(|(_, column)| {
                column.generated() == Some(crate::schema::GeneratedKind::Sequence)
            })
            .map(|(name, _)| vec![name.clone()]);
        // A reference held in a column of its own is how a join or a delete
        // of its target finds the rows that hold it.
        let references = schema.foreign_keys().iter().filter_map(|fk| {
            let columns: Option<Vec<String>> = fk
                .from()
                .iter()
                .map(|path| match path.steps() {
                    [crate::schema::path::Step::Member(column)] => Some(column.clone()),
                    _ => None,
                })
                .collect();
            columns
        });
        for columns in schema
            .indexes()
            .iter()
            .cloned()
            .chain(sequences)
            .chain(references)
        {
            let columns = &columns;
            self.conn
                .execute_batch(&format!(
                    "CREATE INDEX IF NOT EXISTS {} ON {} ({})",
                    quote(&index_name(table, columns)),
                    quote(table),
                    columns
                        .iter()
                        .map(|c| quote(c))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .map_err(corrupt)?;
        }
        self.conn
            .execute(
                "INSERT INTO _reldir_tables (tbl, identity, enforced) VALUES (?1, ?2, 1)",
                params![table, signature(schema)],
            )
            .map_err(corrupt)?;
        self.key_indexes(schema, true)?;
        Ok(())
    }

    /// Index every candidate key of a table: uniquely, so `ON CONFLICT` has a
    /// constraint to name and a duplicate is refused as it is written; or not,
    /// when the observed rows already hold a duplicate the mirror must be able
    /// to describe.
    pub(super) fn key_indexes(&self, schema: &Schema, unique: bool) -> Result<()> {
        let table = schema.table();
        for columns in schema.candidate_keys() {
            let name = format!("{}_key", index_name(table, columns));
            self.conn
                .execute_batch(&format!(
                    "DROP INDEX IF EXISTS {q}; CREATE {kind}INDEX {q} ON {t} ({c})",
                    q = quote(&name),
                    kind = if unique { "UNIQUE " } else { "" },
                    t = quote(table),
                    c = columns
                        .iter()
                        .map(|c| quote(c))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .map_err(corrupt)?;
        }
        self.conn
            .execute(
                "UPDATE _reldir_tables SET enforced = ?2 WHERE tbl = ?1",
                params![table, i64::from(unique)],
            )
            .map_err(corrupt)?;
        Ok(())
    }

    /// Make every table's key indexes unique again once its rows allow it.
    pub fn tighten(&self, schemas: &std::collections::BTreeMap<String, Schema>) -> Result<()> {
        let relaxed: Vec<String> = {
            let mut statement = self
                .conn
                .prepare("SELECT tbl FROM _reldir_tables WHERE enforced = 0")
                .map_err(corrupt)?;
            statement
                .query_map([], |row| row.get(0))
                .map_err(corrupt)?
                .collect::<std::result::Result<Vec<String>, _>>()
                .map_err(corrupt)?
        };
        for table in relaxed {
            let Some(schema) = schemas.get(&table) else {
                continue;
            };
            let mut clean = true;
            for columns in schema.candidate_keys() {
                if !self
                    .duplicates(
                        std::slice::from_ref(&table),
                        &constraint_name(schema, columns),
                        false,
                    )?
                    .is_empty()
                {
                    clean = false;
                }
            }
            if clean {
                self.key_indexes(schema, true)?;
            }
        }
        Ok(())
    }
}
