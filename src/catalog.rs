use crate::{
    config::Config,
    diagnostic::{DbError, Diagnostic, Result},
    schema::{self, Schema},
};
use globset::{Glob, GlobSet, GlobSetBuilder};
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

#[derive(Debug, Clone)]
pub struct Catalog {
    pub root: PathBuf,
    pub schemas: BTreeMap<String, Schema>,
    pub rows: BTreeMap<String, Vec<Row>>,
    pub diagnostics: Vec<Diagnostic>,
    pub warnings: Vec<Diagnostic>,
    pub indentation_width: usize,
}

impl Catalog {
    pub fn observe(root: &Path, config: &Config) -> Result<Self> {
        let mut c = Self {
            root: root.to_path_buf(),
            schemas: BTreeMap::new(),
            rows: BTreeMap::new(),
            diagnostics: vec![],
            warnings: vec![],
            indentation_width: config.indentation_width,
        };
        let schema_dir = root.join("schema");
        if !schema_dir.is_dir() {
            c.diagnostics.push(
                Diagnostic::error("SCHEMA_MISSING", "mandatory schema/ directory is missing")
                    .at("schema"),
            );
            return Ok(c);
        }
        let schema_dir_metadata =
            fs::symlink_metadata(&schema_dir).map_err(|error| DbError::io(&schema_dir, error))?;
        if !schema_dir_metadata.file_type().is_dir() {
            c.diagnostics.push(
                Diagnostic::error("NON_REGULAR_FILE", "schema/ must be a real directory")
                    .at("schema"),
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
            if path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.ends_with(".inferred.json"))
            {
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
            if let Ok(schema_value) = crate::json::parse(&schema_bytes)
                && json_depth(&schema_value) > config.max_nesting_depth
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
        validate_cross(&c.schemas, &mut c.diagnostics);
        let ignores = compile_ignores(&config.ignore)?;
        for (table, s) in &c.schemas {
            let dir = root.join(table);
            c.rows.insert(table.clone(), vec![]);
            if !dir.exists() {
                continue;
            }
            let md = fs::symlink_metadata(&dir).map_err(|e| DbError::io(&dir, e))?;
            if !md.file_type().is_dir() {
                c.diagnostics.push(
                    Diagnostic::error("NON_REGULAR_FILE", "table path must be a directory")
                        .at(table),
                );
                continue;
            }
            let mut names = BTreeSet::new();
            for path in read_dir_sorted(&dir)? {
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                let name = path.file_name().and_then(|x| x.to_str()).unwrap_or("");
                if ignores.is_match(name) {
                    continue;
                }
                let norm: String = name.nfc().flat_map(char::to_lowercase).collect();
                if !names.insert(norm) {
                    c.diagnostics.push(
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
                    c.diagnostics.push(
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
                    c.diagnostics.push(
                        Diagnostic::error("NON_REGULAR_FILE", "hard-linked row files are rejected")
                            .at(rel),
                    );
                    continue;
                }
                if path.extension().and_then(|x| x.to_str()) != Some("json") {
                    c.diagnostics.push(
                        Diagnostic::error(
                            "UNEXPECTED_FILE",
                            "governed table entries must be .json files",
                        )
                        .at(rel),
                    );
                    continue;
                }
                if md.len() > config.max_json_file_size {
                    c.diagnostics.push(
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
                        c.diagnostics
                            .push(Diagnostic::error("INVALID_JSON", e.to_string()).at(rel));
                        continue;
                    }
                };
                let val: Value = match crate::json::parse(&raw) {
                    Ok(v) => v,
                    Err(e) => {
                        let mut d = Diagnostic::error("INVALID_JSON", e.to_string()).at(rel);
                        d.location = Some(crate::diagnostic::Location {
                            line: e.line(),
                            column: e.column(),
                        });
                        d.source_line = String::from_utf8_lossy(&raw)
                            .lines()
                            .nth(e.line().saturating_sub(1))
                            .map(String::from);
                        c.diagnostics.push(d);
                        continue;
                    }
                };
                if json_depth(&val) > config.max_nesting_depth {
                    c.diagnostics.push(
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
                let Some(obj) = val.as_object() else {
                    c.diagnostics.push(
                        Diagnostic::error("ROW_ROOT_NOT_OBJECT", "row JSON root must be an object")
                            .at(rel)
                            .table(table),
                    );
                    continue;
                };
                let expected = crate::canonical::filename(s, obj);
                if expected.as_deref() != Some(name) {
                    c.diagnostics.push(
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
                c.rows.get_mut(table).unwrap().push(Row {
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
                || c.schemas.contains_key(name)
                || ignores.is_match(name)
            {
                continue;
            }
            let md = fs::symlink_metadata(&entry).map_err(|e| DbError::io(&entry, e))?;
            if md.is_dir() {
                c.warnings.push(
                    Diagnostic::warning(
                        "UNGOVERNED_DIRECTORY",
                        format!("top-level directory {name:?} has no schema"),
                    )
                    .at(name)
                    .help(format!(
                        "run `db infer {name} --write` or add it to .db/config ignore"
                    )),
                );
            }
        }
        Ok(c)
    }
    pub fn row_count(&self) -> usize {
        self.rows.values().map(Vec::len).sum()
    }
}

fn json_depth(v: &Value) -> usize {
    match v {
        Value::Array(a) => 1 + a.iter().map(json_depth).max().unwrap_or(0),
        Value::Object(o) => 1 + o.values().map(json_depth).max().unwrap_or(0),
        _ => 0,
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
fn compile_ignores(items: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for x in items {
        b.add(Glob::new(x).map_err(|e| {
            DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                format!("invalid ignore glob {x:?}: {e}"),
                6,
            )
        })?);
    }
    b.build()
        .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))
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
                    && x.kind != y.kind
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
                .filter(|f| f.delete_action() == crate::schema::Action::Cascade)
                .any(|f| visit(&f.references.table, s, vis, stack))
        });
        stack.remove(n);
        cycle
    }
    let mut vis = BTreeSet::new();
    for n in schemas.keys() {
        if visit(n, schemas, &mut vis, &mut BTreeSet::new()) {
            out.push(Diagnostic::error(
                "SCHEMA_FK_CYCLE",
                "foreign keys form an all-cascade cycle",
            ));
            break;
        }
    }
}
