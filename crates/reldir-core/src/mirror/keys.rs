//! Keys and references: who holds a key, what references what, and the faults among them -- whole, or within a change's scope.

use super::*;

impl Mirror {
    /// The paths holding a key under a constraint in any of `tables`.
    pub fn holders(
        &self,
        key: &str,
        constraint: &str,
        tables: &[String],
    ) -> Result<Vec<(String, String)>> {
        let placeholders = (0..tables.len())
            .map(|i| format!("?{}", i + 3))
            .collect::<Vec<_>>()
            .join(", ");
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT tbl, path FROM _reldir_keys WHERE key = ?1 AND cons = ?2 AND tbl IN ({placeholders}) \
                 ORDER BY path"
            ))
            .map_err(corrupt)?;
        let mut arguments: Vec<SqlValue> = vec![
            SqlValue::Text(key.into()),
            SqlValue::Text(constraint.into()),
        ];
        arguments.extend(tables.iter().map(|t| SqlValue::Text(t.clone())));
        let found = statement
            .query_map(rusqlite::params_from_iter(arguments), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(corrupt)?;
        Ok(found)
    }

    /// The references, under the given rules, that name `target`.
    pub fn referrers(&self, target: &str, rules: &[String]) -> Result<Vec<Edge>> {
        let placeholders = (0..rules.len())
            .map(|i| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT path, rule, ptr, target FROM _reldir_edges WHERE target = ?1 AND rule IN ({placeholders}) \
                 ORDER BY path, ptr"
            ))
            .map_err(corrupt)?;
        let mut arguments: Vec<SqlValue> = vec![SqlValue::Text(target.into())];
        arguments.extend(rules.iter().map(|r| SqlValue::Text(r.clone())));
        let found = statement
            .query_map(rusqlite::params_from_iter(arguments), |row| {
                Ok(Edge {
                    path: row.get(0)?,
                    rule: row.get(1)?,
                    pointer: row.get(2)?,
                    target: row.get(3)?,
                })
            })
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(corrupt)?;
        Ok(found)
    }

    /// Every edge recorded under one rule.
    pub fn edges_of(&self, rule: &str) -> Result<Vec<Edge>> {
        let mut statement = self
            .conn
            .prepare("SELECT path, rule, ptr, target FROM _reldir_edges WHERE rule = ?1 ORDER BY path, ptr")
            .map_err(corrupt)?;
        let found = statement
            .query_map([rule], |row| {
                Ok(Edge {
                    path: row.get(0)?,
                    rule: row.get(1)?,
                    pointer: row.get(2)?,
                    target: row.get(3)?,
                })
            })
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(corrupt)?;
        Ok(found)
    }

    /// References under a rule whose target no key in the allowed tables holds.
    pub fn dangling(&self, rule: &str, constraint: &str, tables: &[String]) -> Result<Vec<Edge>> {
        self.dangling_in(rule, constraint, tables, false)
    }

    /// [`Self::dangling`], of the references the scope can change alone when
    /// `scoped`: those its paths hold, and those naming its keys.
    pub fn dangling_in(
        &self,
        rule: &str,
        constraint: &str,
        tables: &[String],
        scoped: bool,
    ) -> Result<Vec<Edge>> {
        let placeholders = (0..tables.len())
            .map(|i| format!("?{}", i + 3))
            .collect::<Vec<_>>()
            .join(", ");
        let missing = format!(
            "NOT EXISTS (SELECT 1 FROM _reldir_keys k WHERE k.key = e.target AND k.cons = ?2 AND k.tbl IN ({placeholders}))"
        );
        // Scoped, the references come from the scope -- those its paths hold,
        // through the path index, and those naming its keys, through the
        // target index -- rather than the rule's every reference.
        let text = if scoped {
            format!(
                "SELECT path, rule, ptr, target FROM ( \
                 SELECT e.path, e.rule, e.ptr, e.target FROM temp._reldir_scope_paths s \
                 CROSS JOIN _reldir_edges e ON e.path = s.path WHERE e.rule = ?1 AND {missing} \
                 UNION \
                 SELECT e.path, e.rule, e.ptr, e.target FROM temp._reldir_scope_keys s \
                 CROSS JOIN _reldir_edges e ON e.target = s.key WHERE e.rule = ?1 AND {missing}) \
                 ORDER BY path, ptr"
            )
        } else {
            format!(
                "SELECT e.path, e.rule, e.ptr, e.target FROM _reldir_edges e WHERE e.rule = ?1 AND {missing} \
                 ORDER BY e.path, e.ptr"
            )
        };
        let mut statement = self.conn.prepare(&text).map_err(corrupt)?;
        let mut arguments: Vec<SqlValue> = vec![
            SqlValue::Text(rule.into()),
            SqlValue::Text(constraint.into()),
        ];
        arguments.extend(tables.iter().map(|t| SqlValue::Text(t.clone())));
        let found = statement
            .query_map(rusqlite::params_from_iter(arguments), |row| {
                Ok(Edge {
                    path: row.get(0)?,
                    rule: row.get(1)?,
                    pointer: row.get(2)?,
                    target: row.get(3)?,
                })
            })
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(corrupt)?;
        Ok(found)
    }

    /// Keys held by more than one file under one constraint of one table, or --
    /// across tables -- under the primary keys of several.
    pub fn duplicates(
        &self,
        tables: &[String],
        constraint: &str,
        across: bool,
    ) -> Result<Vec<Duplicate>> {
        self.duplicates_in(tables, constraint, across, false)
    }

    /// [`Self::duplicates`], of the keys in the scope alone when `scoped`.
    pub fn duplicates_in(
        &self,
        tables: &[String],
        constraint: &str,
        across: bool,
        scoped: bool,
    ) -> Result<Vec<Duplicate>> {
        // Scoped, the keys are looked up from the scope's few, rather than
        // the constraint's every key scanned for them.
        let source = if scoped {
            "temp._reldir_scope_keys s CROSS JOIN _reldir_keys k ON k.key = s.key"
        } else {
            "_reldir_keys k"
        };
        let placeholders = (0..tables.len())
            .map(|i| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let having = if across {
            "count(DISTINCT tbl) > 1"
        } else {
            "count(*) > 1"
        };
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT key, group_concat(tbl || char(30) || path, char(31)) FROM \
                 (SELECT k.key AS key, k.tbl AS tbl, k.path AS path FROM {source} \
                  WHERE k.cons = ?1 AND k.tbl IN ({placeholders}) ORDER BY k.path) \
                 GROUP BY key HAVING {having} ORDER BY key"
            ))
            .map_err(corrupt)?;
        let mut arguments: Vec<SqlValue> = vec![SqlValue::Text(constraint.into())];
        arguments.extend(tables.iter().map(|t| SqlValue::Text(t.clone())));
        let found = statement
            .query_map(rusqlite::params_from_iter(arguments), |row| {
                let key: String = row.get(0)?;
                let holders: String = row.get(1)?;
                Ok(Duplicate {
                    key,
                    holders: holders
                        .split('\u{1f}')
                        .filter_map(|entry| {
                            entry
                                .split_once('\u{1e}')
                                .map(|(table, path)| (table.to_string(), path.to_string()))
                        })
                        .collect(),
                })
            })
            .map_err(corrupt)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(corrupt)?;
        Ok(found)
    }

    /// Name the paths a change touches, and the keys they hold before it or
    /// after it, for the `_in` queries to keep to. The scope lives in this
    /// connection's temporary storage and is replaced by each call.
    pub fn set_scope(&self, paths: &BTreeSet<String>, keys: &BTreeSet<String>) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS _reldir_scope_paths (path TEXT PRIMARY KEY) WITHOUT ROWID; \
                 CREATE TEMP TABLE IF NOT EXISTS _reldir_scope_keys (key TEXT PRIMARY KEY) WITHOUT ROWID; \
                 DELETE FROM temp._reldir_scope_paths; DELETE FROM temp._reldir_scope_keys;",
            )
            .map_err(corrupt)?;
        let mut path = self
            .conn
            .prepare("INSERT INTO temp._reldir_scope_paths (path) VALUES (?1)")
            .map_err(corrupt)?;
        for each in paths {
            path.execute([each]).map_err(corrupt)?;
        }
        let mut key = self
            .conn
            .prepare("INSERT INTO temp._reldir_scope_keys (key) VALUES (?1)")
            .map_err(corrupt)?;
        for each in keys {
            key.execute([each]).map_err(corrupt)?;
        }
        Ok(())
    }

    /// The keys files hold, under any constraint.
    pub fn keys_held(&self, paths: &BTreeSet<String>) -> Result<BTreeSet<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT key FROM _reldir_keys WHERE path = ?1")
            .map_err(corrupt)?;
        let mut out = BTreeSet::new();
        for path in paths {
            let keys = statement
                .query_map([path], |row| row.get::<_, String>(0))
                .map_err(corrupt)?;
            for key in keys {
                out.insert(key.map_err(corrupt)?);
            }
        }
        Ok(out)
    }

    /// The rowids of a table's rows at these paths.
    pub fn rowids_of(&self, table: &str, paths: &BTreeSet<String>) -> Result<Vec<i64>> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT trow FROM _reldir_files WHERE path = ?1 AND tbl = ?2 AND trow IS NOT NULL",
            )
            .map_err(corrupt)?;
        let mut out = vec![];
        for path in paths {
            if let Some(rowid) = statement
                .query_row(params![path, table], |row| row.get::<_, i64>(0))
                .optional()
                .map_err(corrupt)?
            {
                out.push(rowid);
            }
        }
        Ok(out)
    }
}

/// A reference recorded in the mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    /// The file holding the reference.
    pub path: String,
    /// `fk:<table>/<name>` or `acyclic:<table>/<name>`.
    pub rule: String,
    /// Where in the file the referencing value sits.
    pub pointer: String,
    /// The key it names, rendered as keys are.
    pub target: String,
}

/// Files that share a key.
#[derive(Debug, Clone)]
pub struct Duplicate {
    pub key: String,
    /// (table, path) of each holder, in path order.
    pub holders: Vec<(String, String)>,
}

/// What the mirror's copy of a table depends on: the rules its rows are
/// judged by, and the order its columns are written in -- which identity
/// leaves out, because it changes how a row is written, not whether it is
/// valid.
pub(super) fn signature(schema: &Schema) -> String {
    let columns: Vec<&str> = schema.columns().keys().map(String::as_str).collect();
    format!(
        "{}:{}",
        schema.identity(),
        canonical::hash_bytes(columns.join("\u{1f}").as_bytes())
    )
}

/// The rule name a foreign key's edges are stored under.
pub fn fk_rule(table: &str, name: &str) -> String {
    format!("fk:{table}/{name}")
}

/// The rule name an acyclic graph's edges are stored under.
pub fn acyclic_rule(table: &str, name: &str) -> String {
    format!("acyclic:{table}/{name}")
}

/// The constraint name a column list is stored under in `_reldir_keys`.
pub fn constraint_name(schema: &Schema, columns: &[String]) -> String {
    if columns == schema.primary_key() {
        PRIMARY.into()
    } else {
        format!("unique:{}", columns.join(","))
    }
}

pub(super) fn constraints(schema: &Schema) -> Vec<(String, Vec<String>)> {
    schema
        .candidate_keys()
        .map(|columns| (constraint_name(schema, columns), columns.to_vec()))
        .collect()
}

/// The keys a row holds, one per candidate key of its table that it takes
/// part in: what the mirror indexes it under.
pub fn row_keys(row: &Map<String, Value>, schema: &Schema) -> Vec<String> {
    constraints(schema)
        .into_iter()
        .filter_map(|(_, columns)| key(row, &columns, schema))
        .collect()
}

/// A key's canonical rendering: the canonical JSON array of its column values,
/// each written the way its type is compared -- a timestamp in UTC, so the same
/// instant is the same key whatever offset it was written in. Absent values
/// read as their default; a null anywhere means the row takes part in no key.
pub fn key(row: &Map<String, Value>, columns: &[String], schema: &Schema) -> Option<String> {
    let mut values = Vec::with_capacity(columns.len());
    for name in columns {
        let column = schema.column(name)?;
        let value = row
            .get(name)
            .or(column.default())
            .cloned()
            .unwrap_or(Value::Null);
        if value.is_null() {
            return None;
        }
        values.push(crate::value::key_component(&value, column.kind()));
    }
    Some(canonical::compact(&Value::Array(values)))
}

/// A reference found in a row.
pub struct RowEdge {
    pub rule: String,
    pub pointer: String,
    pub target: String,
}

/// Every reference and acyclic edge a row makes.
pub fn edges(row: &Map<String, Value>, schema: &Schema) -> Vec<RowEdge> {
    let mut out = vec![];
    for fk in schema.foreign_keys() {
        let rule = fk_rule(schema.table(), fk.name());
        if fk.from().len() == 1 {
            let path = &fk.from()[0];
            let kind = crate::schema::document::resolve_path(schema, path)
                .map(|leaf| leaf.column.kind().clone())
                .unwrap_or(ColumnType::Json);
            for occurrence in path.occurrences(row) {
                out.push(RowEdge {
                    rule: rule.clone(),
                    pointer: occurrence.pointer,
                    target: canonical::compact(&Value::Array(vec![crate::value::key_component(
                        occurrence.value,
                        &kind,
                    )])),
                });
            }
        } else {
            // A composite key: one value per path, none of them iterating.
            let mut values = vec![];
            let mut pointers = vec![];
            for path in fk.from() {
                let kind = crate::schema::document::resolve_path(schema, path)
                    .map(|leaf| leaf.column.kind().clone())
                    .unwrap_or(ColumnType::Json);
                match path.occurrences(row).into_iter().next() {
                    Some(occurrence) => {
                        values.push(crate::value::key_component(occurrence.value, &kind));
                        pointers.push(occurrence.pointer);
                    }
                    None => {
                        values.clear();
                        break;
                    }
                }
            }
            if values.len() == fk.from().len() {
                out.push(RowEdge {
                    rule,
                    pointer: pointers[0].clone(),
                    target: canonical::compact(&Value::Array(values)),
                });
            }
        }
    }
    for graph in schema.acyclic() {
        let rule = acyclic_rule(schema.table(), graph.name());
        let kind = schema
            .column(&schema.primary_key()[0])
            .map(|c| c.kind().clone())
            .unwrap_or(ColumnType::Json);
        for path in graph.edges() {
            for occurrence in path.occurrences(row) {
                out.push(RowEdge {
                    rule: rule.clone(),
                    pointer: occurrence.pointer,
                    target: canonical::compact(&Value::Array(vec![crate::value::key_component(
                        occurrence.value,
                        &kind,
                    )])),
                });
            }
        }
    }
    out
}

/// A quoted SQL identifier.
pub fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// The name of the index over a column list, stable across processes.
pub fn index_name(table: &str, columns: &[String]) -> String {
    let mut identity = String::new();
    for column in columns {
        identity.push_str(&column.len().to_string());
        identity.push(':');
        identity.push_str(column);
        identity.push(';');
    }
    format!(
        "reldir_{table}_{}",
        &canonical::hash_bytes(identity.as_bytes())[..12]
    )
}
