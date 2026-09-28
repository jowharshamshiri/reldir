//! Observing the governed directory.
//!
//! A catalog is what the binary knows about the folder at one moment: which
//! tables exist and what their schemas say, which files hold their rows, and
//! everything wrong with any of it that can be judged file by file. Rows are
//! not held in memory: they live in the [`Mirror`], which this module keeps in
//! step with the files -- re-reading only files whose size, times or inode
//! moved, and re-validating only what it re-read.
//!
//! Observation reads through a [`Source`], so the same code observes the disk
//! and the disk-with-a-planned-change-laid-over-it that a mutation is judged
//! against before anything is written.
//!
//! Every document read here is untrusted input and is parsed under the
//! configured nesting and size bounds.

use crate::{
    canonical,
    config::Config,
    diagnostic::{DbError, Diagnostic, Result},
    fs::{Kind, Source},
    mirror::{self, Ingest, Mirror},
    schema::{Schema, SchemaFileKind},
};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    rc::Rc,
};
use unicode_normalization::UnicodeNormalization;

/// A row, as the mirror holds it.
#[derive(Debug, Clone)]
pub struct Row {
    pub table: String,
    /// Path relative to the database root, with `/` separators.
    pub relative: PathBuf,
    /// The row's members, in schema column order.
    pub value: Map<String, Value>,
}

impl Row {
    /// The file's bytes, read now, for diagnostics that point into them and for
    /// changes that must preserve them.
    pub fn raw(&self, root: &Path) -> Result<Vec<u8>> {
        let path = root.join(&self.relative);
        std::fs::read(&path).map_err(|error| DbError::io(&path, error))
    }
}

/// Where a table's schema came from.
#[derive(Debug, Clone)]
pub struct SchemaFile {
    pub kind: SchemaFileKind,
    /// Relative to the database root.
    pub relative: PathBuf,
    pub bytes: Vec<u8>,
}

/// What to tell someone holding a directory of rows the database does not
/// govern. One text, so the `UNGOVERNED_DIRECTORY` warning and the
/// `UNKNOWN_TABLE` error can never advise two different things.
fn ungoverned_help(name: &str) -> String {
    format!("run `reldir infer {name} --write`, or add it to the ignore list in .db/config")
}

pub struct Catalog {
    pub root: PathBuf,
    /// Top-level directories holding data that no schema governs.
    pub ungoverned: Vec<String>,
    /// Tables whose schema is a pin in `schema/`.
    pub pinned: BTreeSet<String>,
    /// Every schema that could be read.
    pub schemas: BTreeMap<String, Schema>,
    /// Where each table's schema lives, including those that could not be
    /// read -- a fault in a schema is still a fault at a path.
    pub schema_files: BTreeMap<String, SchemaFile>,
    /// Working schemas left behind by a table that now has a pin. The pin
    /// governs; these are derived copies to discard.
    pub superseded_working: Vec<String>,
    /// Faults found while observing: structure, schemas, and cross-schema
    /// rules. [`crate::integrity::validate`] adds row and set-level faults.
    pub diagnostics: Vec<Diagnostic>,
    pub warnings: Vec<Diagnostic>,
    pub indentation_width: usize,
    pub mirror: Rc<Mirror>,
    /// Paths read afresh in this observation, in path order.
    pub reread: Vec<String>,
    /// Tables whose rows were all re-read because their schema changed.
    pub rebuilt_tables: Vec<String>,
    /// Whether the persistent mirror had to be discarded and rebuilt.
    pub rebuilt_mirror: bool,
}

impl Catalog {
    /// Observe the governed directory against the schemas it declares.
    pub fn observe(
        root: &Path,
        config: &Config,
        source: &dyn Source,
        mirror: Rc<Mirror>,
        rebuilt_mirror: bool,
    ) -> Result<Self> {
        crate::json::with_depth_limit(config.max_nesting_depth, || {
            let mut catalog = Self::empty(root, config, mirror, rebuilt_mirror);
            catalog.load_schemas(config, source)?;
            crate::integrity::validate_schemas(&mut catalog);
            catalog.refresh(config, source)?;
            Ok(catalog)
        })
    }

    /// Observe a folder that carries no database: its pins are read as usual,
    /// and tables nobody pinned are governed by schemas inferred in memory and
    /// never written. Rows are read and judged exactly as for a governed
    /// database, so there is one interpretation of validity regardless of where
    /// the schemas came from.
    pub fn observe_with_schemas(
        root: &Path,
        config: &Config,
        source: &dyn Source,
        inferred: BTreeMap<String, Schema>,
    ) -> Result<Self> {
        crate::json::with_depth_limit(config.max_nesting_depth, || {
            let mut catalog = Self::empty(root, config, Rc::new(Mirror::open_memory()?), false);
            catalog.load_schemas(config, source)?;
            for (table, schema) in inferred {
                if !catalog.schema_files.contains_key(&table) {
                    catalog.schemas.insert(table, schema);
                }
            }
            crate::integrity::validate_schemas(&mut catalog);
            catalog.refresh(config, source)?;
            Ok(catalog)
        })
    }

    /// Observe the directory as a planned change would leave it, judge it, and
    /// discard the observation. Nothing is written: the planned bytes are read
    /// from memory, and the mirror is changed only inside a savepoint that is
    /// rolled back.
    pub fn prospect(&self, config: &Config, source: &dyn Source) -> Result<crate::integrity::Verdict> {
        self.mirror.savepoint("reldir_prospect")?;
        let outcome = crate::json::with_depth_limit(config.max_nesting_depth, || {
            let mut future = Self::empty(&self.root, config, Rc::clone(&self.mirror), false);
            future.load_schemas(config, source)?;
            crate::integrity::validate_schemas(&mut future);
            future.refresh(config, source)?;
            crate::integrity::validate_through(&future, source)
        });
        self.mirror.rollback_to("reldir_prospect")?;
        outcome
    }

    fn empty(root: &Path, config: &Config, mirror: Rc<Mirror>, rebuilt_mirror: bool) -> Self {
        Self {
            root: root.to_path_buf(),
            ungoverned: vec![],
            pinned: BTreeSet::new(),
            schemas: BTreeMap::new(),
            schema_files: BTreeMap::new(),
            superseded_working: vec![],
            diagnostics: vec![],
            warnings: vec![],
            indentation_width: config.indentation_width,
            mirror,
            reread: vec![],
            rebuilt_tables: vec![],
            rebuilt_mirror,
        }
    }

    /// The `UNKNOWN_TABLE` error for a name this catalog does not govern.
    pub fn unknown_table(&self, table: &str) -> DbError {
        let diagnostic = Diagnostic::error("UNKNOWN_TABLE", format!("unknown table {table:?}"));
        let diagnostic = if self.ungoverned.iter().any(|name| name == table) {
            diagnostic.at(table).help(ungoverned_help(table))
        } else {
            let nearest = self
                .schemas
                .keys()
                .min_by_key(|name| strsim::levenshtein(name, table))
                .filter(|name| strsim::levenshtein(name, table) <= 2);
            match nearest {
                Some(name) => diagnostic.help(format!("did you mean {name:?}?")),
                None => diagnostic,
            }
        };
        DbError::from_diag(diagnostic, 4)
    }

    /// Read every schema file. A table's schema is its pin when it has one,
    /// else its working schema; a table with both is governed by the pin, and
    /// the working copy is recorded as superseded derived state.
    fn load_schemas(&mut self, config: &Config, source: &dyn Source) -> Result<()> {
        let mut candidates: BTreeMap<String, (SchemaFileKind, PathBuf)> = BTreeMap::new();
        for (kind, directory) in [
            (SchemaFileKind::Working, crate::schema_store::working_dir(&self.root)),
            (SchemaFileKind::Pin, crate::schema_store::pin_dir(&self.root)),
        ] {
            let relative_directory = relative(&self.root, &directory);
            match source.metadata(&directory) {
                Ok(meta) if meta.kind == Kind::Dir => {}
                Ok(_) => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "NON_REGULAR_FILE",
                            format!("{} must be a real directory", relative_directory.display()),
                        )
                        .at(relative_directory),
                    );
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(DbError::io(&directory, error)),
            }
            let mut seen = BTreeSet::new();
            for path in source.read_dir(&directory).map_err(|e| DbError::io(&directory, e))? {
                let relative_path = relative(&self.root, &path);
                if crate::metadata::is_in_progress_write(&path) {
                    continue;
                }
                let meta = match source.metadata(&path) {
                    Ok(meta) => meta,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(DbError::io(&path, error)),
                };
                if meta.kind != Kind::File || meta.links > 1 {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "NON_REGULAR_FILE",
                            "schema entries must be private regular files",
                        )
                        .at(relative_path),
                    );
                    continue;
                }
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    self.diagnostics.push(
                        Diagnostic::error("UNEXPECTED_FILE", "a schema directory holds only .json files")
                            .at(relative_path),
                    );
                    continue;
                }
                if meta.len > config.max_json_file_size {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "RESOURCE_LIMIT",
                            format!("schema exceeds the {} byte file limit", config.max_json_file_size),
                        )
                        .at(relative_path),
                    );
                    continue;
                }
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
                let normalized: String = stem.nfc().flat_map(char::to_lowercase).collect();
                if !seen.insert(normalized) {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "PATH_COLLISION",
                            "schema paths collide under case or Unicode normalization",
                        )
                        .at(relative_path),
                    );
                    continue;
                }
                if kind == SchemaFileKind::Pin {
                    self.pinned.insert(stem.clone());
                    if candidates.contains_key(&stem) {
                        self.superseded_working.push(stem.clone());
                    }
                }
                candidates.insert(stem, (kind, path));
            }
        }
        for (table, (kind, path)) in candidates {
            let relative_path = relative(&self.root, &path);
            let bytes = source.read(&path).map_err(|error| DbError::io(&path, error))?;
            let spans = crate::locate::Spans::of(&bytes);
            match Schema::from_bytes(&bytes) {
                Ok(schema) if schema.table() != table => self.diagnostics.push(
                    Diagnostic::error(
                        "SCHEMA_TABLE_NAME_MISMATCH",
                        format!(
                            "x-reldir.table is {:?}, but the file is named for {table:?}; a schema governs \
                             the table its file is named for",
                            schema.table()
                        ),
                    )
                    .at(relative_path.clone())
                    .table(&table)
                    .pointer("/x-reldir/table")
                    .locate_in(&bytes, &spans),
                ),
                Ok(schema) => {
                    self.schemas.insert(table.clone(), schema);
                }
                Err(problems) => {
                    self.diagnostics.extend(problems.into_iter().map(|diagnostic| {
                        let mut located = diagnostic.at(relative_path.clone()).table(&table);
                        if located.location.is_none() {
                            located = located.locate_in(&bytes, &spans);
                        } else if located.source_line.is_none()
                            && let Some(location) = &located.location
                        {
                            located.source_line = std::str::from_utf8(&bytes)
                                .ok()
                                .and_then(|text| text.lines().nth(location.line.saturating_sub(1)))
                                .map(String::from);
                        }
                        located
                    }));
                }
            }
            self.schema_files.insert(
                table,
                SchemaFile {
                    kind,
                    relative: relative_path,
                    bytes,
                },
            );
        }
        Ok(())
    }

    /// Bring the mirror in line with the files.
    fn refresh(&mut self, config: &Config, source: &dyn Source) -> Result<()> {
        let seen_ns = now_ns();
        let ignores = config
            .ignore_set()
            .map_err(|error| DbError::new("CONFIG_INVALID", error, 1))?;
        // Tables whose schema cannot be read are not interpreted: their rows
        // have no rules to be judged by, and judging them by stale ones would
        // report faults that are not there.
        let governed: BTreeMap<String, Schema> = self
            .schemas
            .iter()
            .filter(|(table, _)| !self.blocked(table))
            .map(|(table, schema)| (table.clone(), schema.clone()))
            .collect();
        let mirror = Rc::clone(&self.mirror);
        mirror.savepoint("reldir_refresh")?;
        let outcome = (|| -> Result<()> {
            self.rebuilt_tables = mirror.sync_schemas(&governed)?;
            let mut present = BTreeSet::new();
            for (table, schema) in &governed {
                self.scan_table(table, schema, config, &ignores, seen_ns, source, &mut present)?;
            }
            for table in governed.keys() {
                for path in mirror.paths(table)? {
                    if !present.contains(&path) {
                        mirror.remove(&path)?;
                    }
                }
            }
            // A duplicate that relaxed a table's key indexes may be gone now.
            mirror.tighten(&governed)?;
            Ok(())
        })();
        match outcome {
            Ok(()) => mirror.release("reldir_refresh")?,
            Err(error) => {
                mirror.rollback_to("reldir_refresh")?;
                return Err(error);
            }
        }
        let entries = match source.read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(error) => return Err(DbError::io(&self.root, error)),
        };
        for entry in entries {
            let name = entry.file_name().and_then(|x| x.to_str()).unwrap_or("");
            if matches!(name, "schema" | ".db" | ".git")
                || name.starts_with('.')
                || self.schema_files.contains_key(name)
                || ignores.is_match(name)
            {
                continue;
            }
            let is_dir = source.metadata(&entry).is_ok_and(|meta| meta.kind == Kind::Dir);
            if is_dir {
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

    /// Whether a table's schema has faults that stop its rows being judged.
    pub fn blocked(&self, table: &str) -> bool {
        !self.schemas.contains_key(table)
            || self
                .diagnostics
                .iter()
                .any(|d| d.table.as_deref() == Some(table) && d.code.starts_with("SCHEMA_"))
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_table(
        &mut self,
        table: &str,
        schema: &Schema,
        config: &Config,
        ignores: &globset::GlobSet,
        seen_ns: i64,
        source: &dyn Source,
        present: &mut BTreeSet<String>,
    ) -> Result<()> {
        let directory = self.root.join(table);
        match source.metadata(&directory) {
            Ok(meta) if meta.kind == Kind::Dir => {}
            Ok(_) => {
                self.diagnostics.push(
                    Diagnostic::error("NON_REGULAR_FILE", "a table's path must be a real directory")
                        .at(table)
                        .table(table),
                );
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(DbError::io(&directory, error)),
        }
        let entries = source.read_dir(&directory).map_err(|e| DbError::io(&directory, e))?;
        let rebuilt = self.rebuilt_tables.iter().any(|t| t == table);
        let mut names = BTreeSet::new();
        let mut progress = crate::output::Progress::new("scanning", entries.len());
        for path in entries {
            progress.advance();
            let relative_path = relative(&self.root, &path);
            let key = slash(&relative_path);
            let name = path.file_name().and_then(|x| x.to_str()).unwrap_or("");
            if ignores.is_match(&relative_path) || ignores.is_match(name) {
                continue;
            }
            // A writer replaces a row by renaming a temp sibling over it, so a
            // table directory legitimately holds one for the length of a
            // commit. It is not a row and not this reader's to judge.
            if crate::metadata::is_in_progress_write(&path) {
                continue;
            }
            let normalized: String = name.nfc().flat_map(char::to_lowercase).collect();
            if !names.insert(normalized) {
                self.diagnostics.push(
                    Diagnostic::error(
                        "PATH_COLLISION",
                        "row paths collide under case or Unicode normalization, so they name one \
                         file on some filesystems and two on others",
                    )
                    .at(relative_path)
                    .table(table),
                );
                continue;
            }
            let meta = match source.metadata(&path) {
                Ok(meta) => meta,
                // Listed, then renamed away before it could be examined:
                // absent, which is an ordinary state.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(DbError::io(&path, error)),
            };
            if meta.kind != Kind::File {
                self.diagnostics.push(
                    Diagnostic::error(
                        if meta.kind == Kind::Dir { "UNEXPECTED_FILE" } else { "NON_REGULAR_FILE" },
                        "a table directory holds only regular .json files",
                    )
                    .at(relative_path)
                    .table(table),
                );
                continue;
            }
            if meta.links > 1 {
                self.diagnostics.push(
                    Diagnostic::error("NON_REGULAR_FILE", "hard-linked row files are refused")
                        .at(relative_path)
                        .table(table),
                );
                continue;
            }
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                self.diagnostics.push(
                    Diagnostic::error("UNEXPECTED_FILE", "a table directory holds only .json files")
                        .at(relative_path)
                        .table(table)
                        .help("move it out, or add it to the ignore list in .db/config"),
                );
                continue;
            }
            if meta.len > config.max_json_file_size {
                self.diagnostics.push(
                    Diagnostic::error(
                        "RESOURCE_LIMIT",
                        format!("file exceeds the {} byte limit", config.max_json_file_size),
                    )
                    .at(relative_path)
                    .table(table),
                );
                continue;
            }
            present.insert(key.clone());
            let cached = if rebuilt { None } else { self.mirror.file(&key)? };
            if let Some(entry) = &cached
                && entry.trusted_for(&meta.stat)
            {
                continue;
            }
            let raw = match source.read(&path) {
                Ok(raw) => raw,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    present.remove(&key);
                    continue;
                }
                Err(error) => return Err(DbError::io(&path, error)),
            };
            let raw_hash = canonical::hash_bytes(&raw);
            if let Some(entry) = &cached
                && entry.raw_hash == raw_hash
            {
                self.mirror.touch(&key, &meta.stat, seen_ns)?;
                continue;
            }
            self.reread.push(key.clone());
            let (row, diagnostics) = judge_row(table, schema, name, &raw, config);
            self.mirror.put(Ingest {
                path: &key,
                table,
                schema,
                stat: meta.stat,
                raw_hash,
                seen_ns,
                row: row.as_ref(),
                diagnostics,
            })?;
        }
        Ok(())
    }

    /// Every row of a table, in path order.
    pub fn rows(&self, table: &str) -> Result<Vec<Row>> {
        let mut out = vec![];
        self.each_row(table, |row| {
            out.push(row);
            Ok(())
        })?;
        Ok(out)
    }

    /// Visit every row of a table in path order without holding the table.
    pub fn each_row(&self, table: &str, mut visit: impl FnMut(Row) -> Result<()>) -> Result<()> {
        self.mirror.each_file(table, |entry| {
            if let Some(value) = entry.doc {
                visit(Row {
                    table: entry.table,
                    relative: PathBuf::from(entry.path),
                    value,
                })?;
            }
            Ok(())
        })
    }

    /// The row a file holds, if the mirror has read one there.
    pub fn row_at(&self, relative_path: &Path) -> Result<Option<Row>> {
        Ok(self.mirror.file(&slash(relative_path))?.and_then(|entry| {
            entry.doc.map(|value| Row {
                table: entry.table,
                relative: relative_path.to_path_buf(),
                value,
            })
        }))
    }

    /// The row a table holds under a primary key, rendered as keys are.
    pub fn row_by_key(&self, table: &str, key: &str) -> Result<Option<Row>> {
        let holders = self.mirror.holders(key, mirror::PRIMARY, &[table.to_string()])?;
        match holders.first() {
            Some((_, path)) => self.row_at(Path::new(path)),
            None => Ok(None),
        }
    }

    pub fn row_count(&self) -> Result<u64> {
        let mut total = 0;
        for table in self.schemas.keys() {
            total += self.mirror.count(table)?;
        }
        Ok(total)
    }
}

/// Everything wrong with one file on its own: whether it parses, whether it is
/// an object, whether it satisfies its schema, and whether its name is the one
/// its identity derives.
pub fn judge_row(
    table: &str,
    schema: &Schema,
    name: &str,
    raw: &[u8],
    config: &Config,
) -> (Option<Map<String, Value>>, Vec<Diagnostic>) {
    let mut out = vec![];
    let value = match crate::json::parse(raw) {
        Ok(value) => value,
        Err(error) => {
            let diagnostic = if crate::json::is_depth_limit(&error) {
                Diagnostic::error(
                    "RESOURCE_LIMIT",
                    format!("JSON nesting exceeds the depth limit {}", config.max_nesting_depth),
                )
            } else {
                let mut diagnostic = Diagnostic::error("INVALID_JSON", error.to_string());
                diagnostic.location = Some(crate::diagnostic::Location {
                    line: error.line(),
                    column: error.column(),
                });
                diagnostic.source_line = String::from_utf8_lossy(raw)
                    .lines()
                    .nth(error.line().saturating_sub(1))
                    .map(String::from);
                if raw.windows(7).any(|window| window == b"<<<<<<<") {
                    diagnostic = diagnostic.help("the file holds merge-conflict markers; resolve the merge");
                }
                diagnostic
            };
            out.push(diagnostic.table(table));
            return (None, out);
        }
    };
    let Some(object) = value.as_object() else {
        out.push(
            Diagnostic::error("ROW_ROOT_NOT_OBJECT", "a row file holds one JSON object")
                .table(table)
                .pointer(""),
        );
        return (None, out);
    };
    let spans = crate::locate::Spans::of(raw);
    let found = schema.validator().check(&value);
    let missing: Vec<&str> = found
        .iter()
        .filter(|d| d.code == "ROW_MISSING_FIELD")
        .filter_map(|d| d.field.as_deref())
        .collect();
    for mut diagnostic in found.iter().cloned() {
        attach_fixes(&mut diagnostic, schema, &value, &missing);
        out.push(diagnostic.table(table).locate_in(raw, &spans));
    }
    match canonical::filename(schema, object) {
        Some(expected) if !canonical::filename_fits(&expected) => out.push(
            Diagnostic::error(
                "FILENAME_TOO_LONG",
                format!(
                    "the row's identity renders to a {}-byte filename, beyond the {}-byte limit \
                     filesystems share",
                    expected.len(),
                    canonical::MAX_FILENAME_BYTES
                ),
            )
            .table(table),
        ),
        Some(expected) if expected != name => out.push(
            Diagnostic::error(
                "IDENTITY_MISMATCH",
                format!("the file is named {name:?}, but the row's identity names it {expected:?}"),
            )
            .table(table)
            .expected(expected.clone())
            .observed(name)
            .fix("FIX_RENAME_TO_IDENTITY")
            .help(format!("rename the file to {expected}; the row itself is unaffected")),
        ),
        Some(_) => {}
        None => out.push(
            Diagnostic::error(
                "IDENTITY_MISMATCH",
                "the row has no identity: a column its filename is built from is absent or null",
            )
            .table(table)
            .observed(name),
        ),
    }
    (Some(object.clone()), out)
}

/// Offer exactly the repairs that would work for this row: a coercion only when
/// one exists, a default only when the column declares one, a rename only when
/// an unknown member is unambiguously a misspelt missing column.
fn attach_fixes(diagnostic: &mut Diagnostic, schema: &Schema, row: &Value, missing: &[&str]) {
    let top_column = diagnostic
        .pointer
        .as_deref()
        .map(crate::schema::path::pointer_tokens)
        .filter(|tokens| tokens.len() == 1)
        .map(|mut tokens| tokens.remove(0));
    match diagnostic.code.as_str() {
        "TYPE_MISMATCH" => {
            let coercible = top_column
                .as_deref()
                .is_some_and(|column| schema.validator().coercion(row, column).is_some());
            if !coercible {
                diagnostic.fixes.retain(|fix| fix != "FIX_COERCE_VALUE");
            }
        }
        "ROW_MISSING_FIELD" | "NOT_NULL_VIOLATION" => {
            let column = diagnostic.field.as_deref().and_then(|name| schema.column(name));
            if column.is_some_and(|column| column.default().is_some_and(|value| !value.is_null())) {
                diagnostic.fixes.push("FIX_FILL_DEFAULT".into());
            }
        }
        "ROW_UNKNOWN_FIELD" => {
            if let Some(unknown) = diagnostic.field.clone() {
                let mut ranked: Vec<(usize, &str)> = missing
                    .iter()
                    .map(|name| (strsim::levenshtein(&unknown, name), *name))
                    .filter(|(distance, _)| *distance <= 2)
                    .collect();
                ranked.sort();
                let unambiguous = match ranked.as_slice() {
                    [only] => Some(only.1),
                    [first, second, ..] if first.0 < second.0 => Some(first.1),
                    _ => None,
                };
                if let Some(name) = unambiguous {
                    diagnostic.expected = Some(name.to_string());
                    diagnostic.fixes.insert(0, "FIX_RENAME_FIELD".into());
                    diagnostic.help = Some(format!("did you mean {name:?}?"));
                }
            }
        }
        _ => {}
    }
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as i64)
}

fn relative(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

/// Whether a file has other names. A hard-linked metadata or row file could be
/// changed through a path reldir does not govern, so it is refused.
#[cfg(unix)]
pub fn has_multiple_links(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() > 1
}

#[cfg(not(unix))]
pub fn has_multiple_links(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// A relative path with `/` separators, as the mirror and provenance name it.
pub fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
