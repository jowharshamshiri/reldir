use crate::{
    config::Config,
    diagnostic::{DbError, Diagnostic, Result},
    schema::{self, Schema},
};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone)]
pub struct Row {
    pub table: String,
    pub path: PathBuf,
    pub relative: PathBuf,
    pub value: Map<String, Value>,
    pub raw: Vec<u8>,
}

/// What to tell someone holding a directory of rows the database does not
/// govern. One text, so the `UNGOVERNED_DIRECTORY` warning and the
/// `UNKNOWN_TABLE` error can never advise two different things.
fn ungoverned_help(name: &str) -> String {
    format!("run `db infer {name} --write` or add it to .db/config ignore")
}

#[derive(Debug, Clone)]
pub struct Catalog {
    pub root: PathBuf,
    /// Top-level directories that hold data but no schema governs.
    ///
    /// Recorded by the observation that decides the question, so that anything
    /// needing to know whether a name is an ungoverned directory reads the same
    /// answer the `UNGOVERNED_DIRECTORY` warning was derived from. Deciding it a
    /// second time elsewhere would let the two disagree.
    pub ungoverned: Vec<String>,
    /// Tables whose working schema is fixed by a pin in `schema/`.
    ///
    /// Recorded at observation because it is a property of the folder, not of
    /// any one schema: a table is pinned or it is not, and every subsystem that
    /// cares -- lint, doctor, the mutation rules -- must read the same answer.
    pub pinned: BTreeSet<String>,
    pub schemas: BTreeMap<String, Schema>,
    /// The bytes each schema was parsed from, keyed by table.
    ///
    /// Retained so that a finding about a schema can report where in the file
    /// the offending declaration sits. Without the original text there is no
    /// honest way to compute a line and column, and a diagnostic that points
    /// nowhere is no better than one that points at the wrong place.
    pub schema_sources: BTreeMap<String, Vec<u8>>,
    pub rows: BTreeMap<String, Vec<Row>>,
    pub diagnostics: Vec<Diagnostic>,
    pub warnings: Vec<Diagnostic>,
    pub indentation_width: usize,
}

impl Catalog {
    /// Observe the governed directory.
    ///
    /// Every document read here is untrusted input, so the whole observation
    /// runs under the configured nesting bound (Sections 57 and 61). The bound
    /// is applied by the parser during deserialization, which is the only place
    /// it can protect the recursion itself.
    pub fn observe(root: &Path, config: &Config) -> Result<Self> {
        crate::json::with_depth_limit(config.max_nesting_depth, || {
            Self::observe_bounded(root, config)
        })
    }

    /// Observe using schemas supplied by the caller rather than read from
    /// `schema/`.
    ///
    /// A folder of JSON with no `schema/` is still describable: inference can
    /// produce the schemas without writing them, and the rows are then read and
    /// validated exactly as they would be for a governed database. This is what
    /// lets a read-only invocation answer over an unadopted folder without
    /// creating anything.
    ///
    /// Row loading and validation are shared with `observe`, so there is one
    /// interpretation of validity regardless of where the schemas came from.
    /// The schemas carry no source bytes, which the only consumer -- lint's
    /// location reporting -- already handles by omitting a location rather than
    /// inventing one.
    pub fn observe_with_schemas(
        root: &Path,
        config: &Config,
        schemas: BTreeMap<String, Schema>,
    ) -> Result<Self> {
        crate::json::with_depth_limit(config.max_nesting_depth, || {
            let mut c = Self::empty(root, config);
            c.schemas = schemas;
            // Schemas supplied by the caller were never read from disk, so
            // nothing here is pinned: this observation describes a folder that
            // carries no database.
            c.pinned = BTreeSet::new();
            c.load_rows(root, config)?;
            Ok(c)
        })
    }

    /// The `UNKNOWN_TABLE` error for a name this catalog does not govern.
    ///
    /// A directory of rows sitting unclaimed beside the database is the case
    /// where the user is one command from what they wanted, so the error says
    /// which command. Where no such directory exists the name is simply wrong --
    /// a typo, a dropped table -- and advising inference of a directory that is
    /// not there would send them after nothing.
    pub fn unknown_table(&self, table: &str) -> DbError {
        let diagnostic = Diagnostic::error("UNKNOWN_TABLE", format!("unknown table {table:?}"));
        let diagnostic = if self.ungoverned.iter().any(|name| name == table) {
            diagnostic.at(table).help(ungoverned_help(table))
        } else {
            diagnostic
        };
        DbError::from_diag(diagnostic, 4)
    }

    fn empty(root: &Path, config: &Config) -> Self {
        Self {
            root: root.to_path_buf(),
            ungoverned: vec![],
            pinned: BTreeSet::new(),
            schemas: BTreeMap::new(),
            schema_sources: BTreeMap::new(),
            rows: BTreeMap::new(),
            diagnostics: vec![],
            warnings: vec![],
            indentation_width: config.indentation_width,
        }
    }

    fn observe_bounded(root: &Path, config: &Config) -> Result<Self> {
        let mut c = Self::empty(root, config);
        // Working schemas are what everything validates against. They live
        // under `.db/`, so an absent directory means this folder has not been
        // established yet -- a bootstrap condition the caller resolves, not a
        // fault in a database that exists.
        let schema_dir = crate::schema_store::working_dir(root);
        let schema_dir_metadata = match fs::symlink_metadata(&schema_dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(c),
            Err(error) => return Err(DbError::io(&schema_dir, error)),
        };
        if !schema_dir_metadata.file_type().is_dir() {
            c.diagnostics.push(
                Diagnostic::error("NON_REGULAR_FILE", ".db/schema must be a real directory")
                    .at(".db/schema"),
            );
            return Ok(c);
        }
        let mut schema_names = BTreeSet::new();
        for path in read_dir_sorted(&schema_dir)? {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            let meta = fs::symlink_metadata(&path).map_err(|e| DbError::io(&path, e))?;
            if !meta.file_type().is_file() {
                c.diagnostics.push(
                    Diagnostic::error("NON_REGULAR_FILE", "schema entries must be regular files")
                        .at(rel),
                );
                continue;
            }
            if has_multiple_links(&meta) {
                c.diagnostics.push(
                    Diagnostic::error("NON_REGULAR_FILE", "hard-linked schema files are rejected")
                        .at(rel),
                );
                continue;
            }
            if meta.len() > config.max_json_file_size {
                c.diagnostics.push(
                    Diagnostic::error(
                        "RESOURCE_LIMIT",
                        format!("schema exceeds {} byte limit", config.max_json_file_size),
                    )
                    .at(rel),
                );
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                c.diagnostics.push(
                    Diagnostic::error(
                        "UNEXPECTED_FILE",
                        "schema directory may contain only .json files",
                    )
                    .at(rel),
                );
                continue;
            }
            let schema_bytes = fs::read(&path).map_err(|error| DbError::io(&path, error))?;
            if let Err(error) = crate::json::parse(&schema_bytes)
                && crate::json::is_depth_limit(&error)
            {
                c.diagnostics.push(
                    Diagnostic::error(
                        "RESOURCE_LIMIT",
                        format!(
                            "schema JSON nesting exceeds depth limit {}",
                            config.max_nesting_depth
                        ),
                    )
                    .at(rel),
                );
                continue;
            }
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let normalized: String = stem.nfc().flat_map(char::to_lowercase).collect();
            if !schema_names.insert(normalized) {
                c.diagnostics.push(
                    Diagnostic::error(
                        "PATH_COLLISION",
                        "schema paths collide under case/Unicode normalization",
                    )
                    .at(rel),
                );
                continue;
            }
            match schema::load(&path) {
                Ok(s) => {
                    let local = s.validate_local(stem);
                    c.diagnostics
                        .extend(local.into_iter().map(|d| d.at(rel.clone()).table(stem)));
                    c.schema_sources.insert(stem.into(), schema_bytes);
                    if c.schemas.insert(stem.into(), s).is_some() {
                        c.diagnostics.push(
                            Diagnostic::error("PATH_COLLISION", "duplicate normalized schema path")
                                .at(rel),
                        );
                    }
                }
                Err(e) => c.diagnostics.push(*e.diagnostic),
            }
        }
        // A pin is the declaration; the working copy is jdb's. When they
        // disagree the pin governs, and the working copy -- derived state, like
        // an index -- is rebuilt from it. That is what makes `schema/` worth
        // keeping in version control and `.db/` safe to delete.
        //
        // There is deliberately no arbitration over which side moved. jdb only
        // ever writes a working copy equal to the pin or to what it inferred,
        // so a disagreement always resolves the same way, and the manifest --
        // the only witness that could tell them apart -- goes stale the moment
        // a pin is adopted without a revision being recorded.
        c.pinned = crate::schema_store::pinned_tables(root)?;
        for table in &c.pinned {
            let Some(pin) = crate::schema_store::load_pin(root, table)? else {
                continue;
            };
            if c.schemas.get(table).is_some_and(|working| {
                crate::schema_store::equivalent(&pin, working).unwrap_or(false)
            }) {
                continue;
            }
            // A pin is checked exactly as a working schema is: it comes from a
            // file a human wrote, so adopting it unchecked would let a
            // malformed declaration govern silently.
            c.diagnostics
                .extend(pin.validate_local(table).into_iter().map(|diagnostic| {
                    diagnostic
                        .at(crate::schema_store::pin_relative(table))
                        .table(table)
                }));
            c.schema_sources.insert(
                table.clone(),
                crate::schema_store::canonical_bytes(&pin, config.indentation_width)?,
            );
            c.schemas.insert(table.clone(), pin);
        }
        c.load_rows(root, config)?;
        Ok(c)
    }
    pub fn row_count(&self) -> usize {
        self.rows.values().map(Vec::len).sum()
    }

    /// Validate the schema set, read every governed row, and report top-level
    /// directories that no schema claims.
    fn load_rows(&mut self, root: &Path, config: &Config) -> Result<()> {
        validate_cross(&self.schemas, &mut self.diagnostics);
        let ignores = config
            .ignore_set()
            .map_err(|error| DbError::new("INTERNAL_METADATA_CORRUPT", error, 6))?;
        for (table, s) in &self.schemas {
            let dir = root.join(table);
            self.rows.insert(table.clone(), vec![]);
            if !dir.exists() {
                continue;
            }
            let md = fs::symlink_metadata(&dir).map_err(|e| DbError::io(&dir, e))?;
            if !md.file_type().is_dir() {
                self.diagnostics.push(
                    Diagnostic::error("NON_REGULAR_FILE", "table path must be a directory")
                        .at(table),
                );
                continue;
            }
            let mut names = BTreeSet::new();
            let entries = read_dir_sorted(&dir)?;
            // Section 49: a scan that outlasts a second reports its progress on
            // a terminal. The reporter is inert off-TTY and under --quiet.
            let mut progress = crate::output::Progress::new("scanning", entries.len());
            for path in entries {
                progress.advance();
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                let name = path.file_name().and_then(|x| x.to_str()).unwrap_or("");
                if ignores.is_match(&rel) || ignores.is_match(name) {
                    continue;
                }
                let norm: String = name.nfc().flat_map(char::to_lowercase).collect();
                if !names.insert(norm) {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "PATH_COLLISION",
                            "row paths collide under case-insensitive normalization",
                        )
                        .at(rel.clone()),
                    );
                    continue;
                }
                let md = fs::symlink_metadata(&path).map_err(|e| DbError::io(&path, e))?;
                if !md.file_type().is_file() {
                    self.diagnostics.push(
                        Diagnostic::error(
                            if md.file_type().is_dir() {
                                "UNEXPECTED_FILE"
                            } else {
                                "NON_REGULAR_FILE"
                            },
                            "governed table entries must be regular .json files",
                        )
                        .at(rel),
                    );
                    continue;
                }
                if has_multiple_links(&md) {
                    self.diagnostics.push(
                        Diagnostic::error("NON_REGULAR_FILE", "hard-linked row files are rejected")
                            .at(rel),
                    );
                    continue;
                }
                if path.extension().and_then(|x| x.to_str()) != Some("json") {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "UNEXPECTED_FILE",
                            "governed table entries must be .json files",
                        )
                        .at(rel),
                    );
                    continue;
                }
                if md.len() > config.max_json_file_size {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "RESOURCE_LIMIT",
                            format!("file exceeds {} byte limit", config.max_json_file_size),
                        )
                        .at(rel),
                    );
                    continue;
                }
                let raw = match fs::read(&path) {
                    Ok(v) => v,
                    Err(e) => {
                        self.diagnostics
                            .push(Diagnostic::error("INVALID_JSON", e.to_string()).at(rel));
                        continue;
                    }
                };
                let val: Value = match crate::json::parse(&raw) {
                    Ok(v) => v,
                    Err(e) => {
                        // A document that is well formed but too deep is a
                        // resource-limit refusal, not malformed JSON.
                        if crate::json::is_depth_limit(&e) {
                            self.diagnostics.push(
                                Diagnostic::error(
                                    "RESOURCE_LIMIT",
                                    format!(
                                        "JSON nesting exceeds depth limit {}",
                                        config.max_nesting_depth
                                    ),
                                )
                                .at(rel),
                            );
                            continue;
                        }
                        let mut d = Diagnostic::error("INVALID_JSON", e.to_string()).at(rel);
                        d.location = Some(crate::diagnostic::Location {
                            line: e.line(),
                            column: e.column(),
                        });
                        d.source_line = String::from_utf8_lossy(&raw)
                            .lines()
                            .nth(e.line().saturating_sub(1))
                            .map(String::from);
                        self.diagnostics.push(d);
                        continue;
                    }
                };
                let Some(obj) = val.as_object() else {
                    self.diagnostics.push(
                        Diagnostic::error("ROW_ROOT_NOT_OBJECT", "row JSON root must be an object")
                            .at(rel)
                            .table(table),
                    );
                    continue;
                };
                let expected = crate::canonical::filename(s, obj);
                if expected.as_deref() != Some(name) {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "IDENTITY_MISMATCH",
                            format!("filename {name:?} does not match row identity"),
                        )
                        .at(rel.clone())
                        .table(table)
                        .expected(expected.unwrap_or_else(|| {
                            "a filename derived from non-null storage.filename columns".into()
                        }))
                        .observed(name)
                        .fix("FIX_RENAME_TO_IDENTITY"),
                    );
                }
                self.rows.get_mut(table).unwrap().push(Row {
                    table: table.clone(),
                    path,
                    relative: rel,
                    value: obj.clone(),
                    raw,
                });
            }
        }
        for entry in read_dir_sorted(root)? {
            let name = entry.file_name().and_then(|x| x.to_str()).unwrap_or("");
            if matches!(name, "schema" | ".db" | ".git")
                || self.schemas.contains_key(name)
                || ignores.is_match(name)
            {
                continue;
            }
            let md = fs::symlink_metadata(&entry).map_err(|e| DbError::io(&entry, e))?;
            if md.is_dir() {
                self.ungoverned.push(name.to_string());
                self.warnings.push(
                    Diagnostic::warning(
                        "UNGOVERNED_DIRECTORY",
                        format!("top-level directory {name:?} has no schema"),
                    )
                    .at(name)
                    .help(ungoverned_help(name)),
                );
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn has_multiple_links(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() > 1
}

#[cfg(not(unix))]
fn has_multiple_links(_metadata: &fs::Metadata) -> bool {
    false
}

fn read_dir_sorted(path: &Path) -> Result<Vec<PathBuf>> {
    let mut v = vec![];
    for e in fs::read_dir(path).map_err(|e| DbError::io(path, e))? {
        v.push(e.map_err(|e| DbError::io(path, e))?.path())
    }
    v.sort();
    Ok(v)
}
fn validate_cross(schemas: &BTreeMap<String, Schema>, out: &mut Vec<Diagnostic>) {
    for (table, s) in schemas {
        for fk in &s.foreign_keys {
            if fk.columns.is_empty() || fk.columns.len() != fk.references.columns.len() {
                out.push(
                    Diagnostic::error(
                        "SCHEMA_FK_ACTION_INVALID",
                        "foreign key column arity must match target",
                    )
                    .table(table),
                );
                continue;
            }
            if fk.columns.iter().collect::<BTreeSet<_>>().len() != fk.columns.len()
                || fk.references.columns.iter().collect::<BTreeSet<_>>().len()
                    != fk.references.columns.len()
            {
                out.push(
                    Diagnostic::error(
                        "SCHEMA_FK_ACTION_INVALID",
                        "foreign key column lists must not repeat columns",
                    )
                    .table(table),
                );
                continue;
            }
            for c in &fk.columns {
                if !s.columns.contains_key(c) {
                    out.push(
                        Diagnostic::error(
                            "SCHEMA_COLUMN_UNKNOWN",
                            format!("foreign key names unknown column {c:?}"),
                        )
                        .table(table),
                    );
                }
            }
            let Some(target) = schemas.get(&fk.references.table) else {
                out.push(
                    Diagnostic::error(
                        "SCHEMA_FK_TARGET_MISSING",
                        format!(
                            "foreign key target table {:?} has no schema",
                            fk.references.table
                        ),
                    )
                    .table(table),
                );
                continue;
            };
            for column in &fk.references.columns {
                if !target.columns.contains_key(column) {
                    out.push(
                        Diagnostic::error(
                            "SCHEMA_COLUMN_UNKNOWN",
                            format!(
                                "foreign key target names unknown column {:?}.{:?}",
                                fk.references.table, column
                            ),
                        )
                        .table(table),
                    );
                }
            }
            let unique = target.primary_key == fk.references.columns
                || target.unique.iter().any(|u| u == &fk.references.columns);
            if !unique {
                out.push(
                    Diagnostic::error(
                        "SCHEMA_FK_TARGET_NOT_UNIQUE",
                        "foreign key target columns are not a primary key or unique constraint",
                    )
                    .table(table),
                );
            }
            for (a, b) in fk.columns.iter().zip(&fk.references.columns) {
                if let (Some(x), Some(y)) = (s.columns.get(a), target.columns.get(b))
                    && !same_column_type(x, y, false)
                {
                    out.push(
                        Diagnostic::error(
                            "SCHEMA_FK_TYPE_MISMATCH",
                            format!(
                                "{table}.{a} and {}.{b} have different types",
                                fk.references.table
                            ),
                        )
                        .table(table),
                    );
                }
            }
            if [fk.delete_action(), fk.update_action()].contains(&crate::schema::Action::SetNull)
                && fk
                    .columns
                    .iter()
                    .any(|c| s.columns.get(c).is_some_and(|v| !v.nullable))
            {
                out.push(
                    Diagnostic::error(
                        "SCHEMA_FK_ACTION_INVALID",
                        "set_null requires nullable referencing columns",
                    )
                    .table(table),
                );
            }
            if [fk.delete_action(), fk.update_action()].contains(&crate::schema::Action::SetDefault)
                && fk
                    .columns
                    .iter()
                    .any(|c| s.columns.get(c).is_some_and(|v| v.default.is_none()))
            {
                out.push(
                    Diagnostic::error(
                        "SCHEMA_FK_ACTION_INVALID",
                        "set_default requires defaults on referencing columns",
                    )
                    .table(table),
                );
            }
        }
    }
    fn visit<'a>(
        n: &'a str,
        s: &'a BTreeMap<String, Schema>,
        vis: &mut BTreeSet<&'a str>,
        stack: &mut BTreeSet<&'a str>,
        update: bool,
    ) -> bool {
        if stack.contains(n) {
            return true;
        }
        if !vis.insert(n) {
            return false;
        }
        stack.insert(n);
        let cycle = s.get(n).is_some_and(|x| {
            x.foreign_keys
                .iter()
                .filter(|f| {
                    (if update {
                        f.update_action()
                    } else {
                        f.delete_action()
                    }) == crate::schema::Action::Cascade
                })
                .any(|f| visit(&f.references.table, s, vis, stack, update))
        });
        stack.remove(n);
        cycle
    }
    for (update, action) in [(false, "delete"), (true, "update")] {
        let mut vis = BTreeSet::new();
        for n in schemas.keys() {
            if visit(n, schemas, &mut vis, &mut BTreeSet::new(), update) {
                out.push(Diagnostic::error(
                    "SCHEMA_FK_CYCLE",
                    format!("foreign keys form an all-cascade {action} cycle"),
                ));
                break;
            }
        }
    }
}

fn same_column_type(
    left: &crate::schema::Column,
    right: &crate::schema::Column,
    compare_nullable: bool,
) -> bool {
    if left.kind != right.kind || (compare_nullable && left.nullable != right.nullable) {
        return false;
    }
    match left.kind {
        crate::schema::ColumnType::Enum => left.values == right.values,
        crate::schema::ColumnType::Array => match (&left.items, &right.items) {
            (Some(left), Some(right)) => same_column_type(left, right, true),
            (None, None) => true,
            _ => false,
        },
        crate::schema::ColumnType::Object => match (&left.properties, &right.properties) {
            (Some(left), Some(right)) => {
                left.len() == right.len()
                    && left.iter().all(|(name, left)| {
                        right
                            .get(name)
                            .is_some_and(|right| same_column_type(left, right, true))
                    })
            }
            (None, None) => true,
            _ => false,
        },
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        Action, AdditionalFields, Column, ColumnType, ForeignKey, Reference, Storage,
    };
    use indexmap::IndexMap;
    use serde_json::json;

    fn column(kind: ColumnType, nullable: bool) -> Column {
        Column {
            kind,
            nullable,
            default: None,
            generated: None,
            values: None,
            items: None,
            properties: None,
            description: None,
            annotations: Default::default(),
        }
    }

    fn schema(table: &str, columns: &[(&str, ColumnType, bool)], primary_key: &[&str]) -> Schema {
        let mut map = IndexMap::new();
        for (name, kind, nullable) in columns {
            map.insert((*name).to_string(), column(kind.clone(), *nullable));
        }
        Schema {
            table: table.into(),
            schema_version: 1,
            schema_format: None,
            description: None,
            primary_key: primary_key.iter().map(|k| (*k).to_string()).collect(),
            columns: map,
            unique: vec![],
            foreign_keys: vec![],
            check: vec![],
            indexes: vec![],
            storage: None,
            additional_fields: AdditionalFields::Reject,
            annotations: Default::default(),
        }
    }

    fn foreign_key(columns: &[&str], table: &str, target: &[&str]) -> ForeignKey {
        ForeignKey {
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            references: Reference {
                table: table.into(),
                columns: target.iter().map(|c| (*c).to_string()).collect(),
            },
            on_delete: Some(Action::Restrict),
            on_update: Some(Action::Restrict),
        }
    }

    fn cross(schemas: Vec<Schema>) -> Vec<String> {
        let map: BTreeMap<String, Schema> =
            schemas.into_iter().map(|s| (s.table.clone(), s)).collect();
        let mut out = vec![];
        validate_cross(&map, &mut out);
        out.into_iter().map(|d| d.code).collect()
    }

    fn parent() -> Schema {
        schema("a", &[("id", ColumnType::String, false)], &["id"])
    }

    /// Section 11: a foreign key must reference a table that exists, name real
    /// columns on both sides, and target a key that actually identifies a row.
    #[test]
    fn test1008_foreign_keys_must_reference_a_real_unique_target() {
        let mut child = schema(
            "b",
            &[
                ("id", ColumnType::String, false),
                ("a_id", ColumnType::String, false),
            ],
            &["id"],
        );
        child.foreign_keys = vec![foreign_key(&["a_id"], "ghost", &["id"])];
        assert!(cross(vec![parent(), child.clone()]).contains(&"SCHEMA_FK_TARGET_MISSING".into()));

        // A target column that does not exist on the target table.
        child.foreign_keys = vec![foreign_key(&["a_id"], "a", &["nope"])];
        let codes = cross(vec![parent(), child.clone()]);
        assert!(codes.contains(&"SCHEMA_COLUMN_UNKNOWN".into()));
        assert!(codes.contains(&"SCHEMA_FK_TARGET_NOT_UNIQUE".into()));

        // A local column that does not exist on the referencing table.
        child.foreign_keys = vec![foreign_key(&["ghost"], "a", &["id"])];
        assert!(cross(vec![parent(), child.clone()]).contains(&"SCHEMA_COLUMN_UNKNOWN".into()));

        // A target that exists but is neither a primary key nor unique.
        let mut wide = schema(
            "a",
            &[
                ("id", ColumnType::String, false),
                ("label", ColumnType::String, false),
            ],
            &["id"],
        );
        child.foreign_keys = vec![foreign_key(&["a_id"], "a", &["label"])];
        assert!(
            cross(vec![wide.clone(), child.clone()])
                .contains(&"SCHEMA_FK_TARGET_NOT_UNIQUE".into())
        );

        // Declaring that target unique makes the same foreign key legitimate.
        wide.unique = vec![vec!["label".into()]];
        assert!(!cross(vec![wide, child]).contains(&"SCHEMA_FK_TARGET_NOT_UNIQUE".into()));
    }

    /// Section 11: referencing and referenced column types must be identical, so
    /// a key can never be compared across incompatible representations.
    #[test]
    fn test1009_foreign_key_column_types_must_match_exactly() {
        let mut child = schema(
            "b",
            &[
                ("id", ColumnType::String, false),
                ("a_id", ColumnType::Int, false),
            ],
            &["id"],
        );
        child.foreign_keys = vec![foreign_key(&["a_id"], "a", &["id"])];
        assert!(cross(vec![parent(), child]).contains(&"SCHEMA_FK_TYPE_MISMATCH".into()));

        // The same types agree.
        let mut ok = schema(
            "b",
            &[
                ("id", ColumnType::String, false),
                ("a_id", ColumnType::String, false),
            ],
            &["id"],
        );
        ok.foreign_keys = vec![foreign_key(&["a_id"], "a", &["id"])];
        assert!(!cross(vec![parent(), ok]).contains(&"SCHEMA_FK_TYPE_MISMATCH".into()));
    }

    /// Section 11: arity must match and neither side may repeat a column,
    /// because a malformed pairing has no defined meaning.
    #[test]
    fn test1010_foreign_key_column_lists_must_be_well_formed() {
        let mut child = schema(
            "b",
            &[
                ("id", ColumnType::String, false),
                ("x", ColumnType::String, false),
                ("y", ColumnType::String, false),
            ],
            &["id"],
        );
        // Two local columns against one target column.
        child.foreign_keys = vec![foreign_key(&["x", "y"], "a", &["id"])];
        assert!(cross(vec![parent(), child.clone()]).contains(&"SCHEMA_FK_ACTION_INVALID".into()));

        // An empty column list.
        child.foreign_keys = vec![foreign_key(&[], "a", &[])];
        assert!(cross(vec![parent(), child.clone()]).contains(&"SCHEMA_FK_ACTION_INVALID".into()));

        // A repeated column on the referencing side.
        let mut composite = schema(
            "a",
            &[
                ("p", ColumnType::String, false),
                ("q", ColumnType::String, false),
            ],
            &["p", "q"],
        );
        composite.table = "a".into();
        child.foreign_keys = vec![foreign_key(&["x", "x"], "a", &["p", "q"])];
        assert!(cross(vec![composite, child]).contains(&"SCHEMA_FK_ACTION_INVALID".into()));
    }

    /// Section 11: set_null needs somewhere to put the null and set_default
    /// needs a default to restore, otherwise the action could not be executed.
    #[test]
    fn test1011_referential_actions_require_columns_that_can_hold_them() {
        let mut child = schema(
            "b",
            &[
                ("id", ColumnType::String, false),
                ("a_id", ColumnType::String, false),
            ],
            &["id"],
        );
        let mut fk = foreign_key(&["a_id"], "a", &["id"]);
        fk.on_delete = Some(Action::SetNull);
        child.foreign_keys = vec![fk.clone()];
        assert!(cross(vec![parent(), child.clone()]).contains(&"SCHEMA_FK_ACTION_INVALID".into()));

        // Making the column nullable satisfies set_null.
        child.columns.get_mut("a_id").unwrap().nullable = true;
        assert!(!cross(vec![parent(), child.clone()]).contains(&"SCHEMA_FK_ACTION_INVALID".into()));

        // set_default requires a declared default.
        let mut fk = foreign_key(&["a_id"], "a", &["id"]);
        fk.on_delete = Some(Action::SetDefault);
        child.foreign_keys = vec![fk];
        assert!(cross(vec![parent(), child.clone()]).contains(&"SCHEMA_FK_ACTION_INVALID".into()));

        child.columns.get_mut("a_id").unwrap().default = Some(json!("fallback"));
        assert!(!cross(vec![parent(), child]).contains(&"SCHEMA_FK_ACTION_INVALID".into()));
    }

    /// Section 11: a cycle in which every edge cascades has no defined
    /// termination, and is rejected for delete and update edges independently.
    #[test]
    fn test1012_all_cascade_cycles_are_rejected_per_action() {
        let cyclic = |action: Action| {
            let mut a = schema(
                "a",
                &[
                    ("id", ColumnType::String, false),
                    ("b_id", ColumnType::String, true),
                ],
                &["id"],
            );
            let mut b = schema(
                "b",
                &[
                    ("id", ColumnType::String, false),
                    ("a_id", ColumnType::String, true),
                ],
                &["id"],
            );
            let mut to_b = foreign_key(&["b_id"], "b", &["id"]);
            let mut to_a = foreign_key(&["a_id"], "a", &["id"]);
            to_b.on_delete = Some(action);
            to_b.on_update = Some(action);
            to_a.on_delete = Some(action);
            to_a.on_update = Some(action);
            a.foreign_keys = vec![to_b];
            b.foreign_keys = vec![to_a];
            cross(vec![a, b])
        };
        assert!(cyclic(Action::Cascade).contains(&"SCHEMA_FK_CYCLE".into()));
        // A cycle whose edges restrict instead terminates and is allowed.
        assert!(!cyclic(Action::Restrict).contains(&"SCHEMA_FK_CYCLE".into()));

        // A self-referencing cascade is a cycle of length one.
        let mut self_ref = schema(
            "a",
            &[
                ("id", ColumnType::String, false),
                ("parent", ColumnType::String, true),
            ],
            &["id"],
        );
        let mut fk = foreign_key(&["parent"], "a", &["id"]);
        fk.on_delete = Some(Action::Cascade);
        fk.on_update = Some(Action::Cascade);
        self_ref.foreign_keys = vec![fk];
        assert!(cross(vec![self_ref]).contains(&"SCHEMA_FK_CYCLE".into()));
    }

    /// Type identity is structural: two columns agree only when their nested
    /// shapes agree, so a foreign key cannot bridge differently shaped values.
    #[test]
    fn test1013_column_type_identity_is_structural() {
        let plain = column(ColumnType::String, false);
        assert!(same_column_type(&plain, &plain, true));
        assert!(!same_column_type(
            &plain,
            &column(ColumnType::Int, false),
            true
        ));

        // Enum membership is part of the type.
        let mut left = column(ColumnType::Enum, false);
        left.values = Some(vec!["a".into(), "b".into()]);
        let mut right = column(ColumnType::Enum, false);
        right.values = Some(vec!["a".into()]);
        assert!(!same_column_type(&left, &right, false));
        right.values = Some(vec!["a".into(), "b".into()]);
        assert!(same_column_type(&left, &right, false));

        // Array element types must agree.
        let mut left = column(ColumnType::Array, false);
        left.items = Some(Box::new(column(ColumnType::Int, false)));
        let mut right = column(ColumnType::Array, false);
        right.items = Some(Box::new(column(ColumnType::String, false)));
        assert!(!same_column_type(&left, &right, false));
        right.items = Some(Box::new(column(ColumnType::Int, false)));
        assert!(same_column_type(&left, &right, false));

        // Object property sets must agree in both name and shape.
        let mut properties = IndexMap::new();
        properties.insert("n".to_string(), column(ColumnType::Int, false));
        let mut left = column(ColumnType::Object, false);
        left.properties = Some(properties.clone());
        let mut right = column(ColumnType::Object, false);
        properties.insert("extra".to_string(), column(ColumnType::Int, false));
        right.properties = Some(properties);
        assert!(!same_column_type(&left, &right, false));

        // Nullability participates only when the caller asks for it, because a
        // foreign key may point from a nullable column at a NOT NULL key.
        let nullable = column(ColumnType::String, true);
        assert!(same_column_type(&plain, &nullable, false));
        assert!(!same_column_type(&plain, &nullable, true));
    }

    /// A schema with no storage override names files by its primary key, which
    /// the observer relies on to map a row to its path.
    #[test]
    fn test1014_filename_columns_track_the_storage_declaration() {
        let mut s = schema(
            "t",
            &[
                ("id", ColumnType::String, false),
                ("slug", ColumnType::String, false),
            ],
            &["id"],
        );
        assert_eq!(s.filename_columns(), ["id".to_string()]);
        s.storage = Some(Storage {
            filename: vec!["slug".into()],
        });
        assert_eq!(s.filename_columns(), ["slug".to_string()]);
    }
}
