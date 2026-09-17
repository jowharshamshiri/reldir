use crate::{
    canonical,
    catalog::Catalog,
    config::Config,
    diagnostic::{DbError, Diagnostic, Result},
    schema::{AdditionalFields, Column, ColumnType, ForeignKey, Reference, Schema},
};
use indexmap::IndexMap;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strictness {
    Strict,
    Balanced,
    Loose,
}
impl Strictness {
    pub fn name(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Balanced => "balanced",
            Self::Loose => "loose",
        }
    }
}
#[derive(Clone)]
struct Sample {
    path: PathBuf,
    stem: String,
    obj: serde_json::Map<String, Value>,
}

pub fn discover_tables(root: &Path) -> Result<Vec<String>> {
    let mut out = vec![];
    for e in fs::read_dir(root).map_err(|e| DbError::io(root, e))? {
        let p = e.map_err(|e| DbError::io(root, e))?.path();
        let n = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if matches!(n, "schema" | ".db" | ".git") || n.starts_with('.') {
            continue;
        }
        let metadata = fs::symlink_metadata(&p).map_err(|error| DbError::io(&p, error))?;
        if metadata.file_type().is_symlink()
            || (!metadata.file_type().is_dir() && !metadata.file_type().is_file())
        {
            return Err(DbError::from_diag(
                Diagnostic::error(
                    "NON_REGULAR_FILE",
                    "top-level candidate is a symlink or special file",
                )
                .at(PathBuf::from(n)),
                8,
            ));
        }
        if !metadata.file_type().is_dir() {
            continue;
        }
        let mut contains_json = false;
        for entry in fs::read_dir(&p).map_err(|e| DbError::io(&p, e))? {
            let path = entry.map_err(|e| DbError::io(&p, e))?.path();
            contains_json |= path.extension().and_then(|x| x.to_str()) == Some("json");
        }
        if contains_json {
            out.push(n.into())
        }
    }
    out.sort();
    Ok(out)
}

pub fn infer_all(
    root: &Path,
    tables: &[String],
    strictness: Strictness,
    config: &Config,
    pk_override: Option<&[String]>,
) -> Result<BTreeMap<String, Schema>> {
    infer_all_with_references(root, tables, strictness, config, pk_override, None)
}

pub fn infer_all_with_references(
    root: &Path,
    tables: &[String],
    strictness: Strictness,
    config: &Config,
    pk_override: Option<&[String]>,
    references: Option<&Catalog>,
) -> Result<BTreeMap<String, Schema>> {
    let mut schemas = BTreeMap::new();
    let mut samples = BTreeMap::new();
    for t in tables {
        let ss = load_samples(root, t, config)?;
        let s = infer_table(t, &ss, strictness, config, pk_override)?;
        samples.insert(t.clone(), ss);
        schemas.insert(t.clone(), s);
    }
    let mut snapshot = references
        .map(|catalog| catalog.schemas.clone())
        .unwrap_or_default();
    snapshot.extend(schemas.clone());
    for (table, s) in &mut schemas {
        let rows = &samples[table];
        for (name, col) in s.columns.clone() {
            if name == s.primary_key[0] {
                continue;
            }
            for (target, ts) in &snapshot {
                if target == table || ts.primary_key.len() != 1 {
                    continue;
                }
                let names = [
                    format!("{target}_id"),
                    format!("{}_id", singular(target)),
                    target.clone(),
                ];
                if !names.contains(&name) || ts.columns[&ts.primary_key[0]].kind != col.kind {
                    continue;
                }
                let target_values: HashSet<_> = if let Some(target_samples) = samples.get(target) {
                    target_samples
                        .iter()
                        .filter_map(|sample| {
                            sample
                                .obj
                                .get(&ts.primary_key[0])
                                .or(ts.columns[&ts.primary_key[0]].default.as_ref())
                                .map(canonical::compact)
                        })
                        .collect()
                } else {
                    references
                        .and_then(|catalog| catalog.rows.get(target))
                        .into_iter()
                        .flatten()
                        .filter_map(|row| {
                            row.value
                                .get(&ts.primary_key[0])
                                .or(ts.columns[&ts.primary_key[0]].default.as_ref())
                                .map(canonical::compact)
                        })
                        .collect()
                };
                let values = rows.iter().filter_map(|row| row.obj.get(&name));
                let has_value = values.clone().any(|value| !value.is_null());
                if has_value
                    && values.clone().all(|value| {
                        value.is_null() || target_values.contains(&canonical::compact(value))
                    })
                {
                    s.foreign_keys.push(ForeignKey {
                        columns: vec![name.clone()],
                        references: Reference {
                            table: target.clone(),
                            columns: ts.primary_key.clone(),
                        },
                        on_delete: Some(crate::schema::Action::Restrict),
                        on_update: Some(crate::schema::Action::Restrict),
                    });
                    s.indexes.push(vec![name.clone()]);
                    break;
                }
            }
        }
    }
    Ok(schemas)
}

/// Read every row of a table as an inference sample.
///
/// Row files are untrusted input, so sampling runs under the configured nesting
/// bound (Sections 57 and 61), enforced by the parser as it deserializes.
fn load_samples(root: &Path, table: &str, config: &Config) -> Result<Vec<Sample>> {
    crate::json::with_depth_limit(config.max_nesting_depth, || {
        load_samples_bounded(root, table, config)
    })
}

fn load_samples_bounded(root: &Path, table: &str, config: &Config) -> Result<Vec<Sample>> {
    let dir = root.join(table);
    if !dir.is_dir() {
        return Err(DbError::from_diag(
            Diagnostic::error(
                "INFER_NO_ROWS",
                format!("table directory {table:?} does not exist"),
            )
            .at(table),
            8,
        ));
    }
    let directory_metadata =
        fs::symlink_metadata(&dir).map_err(|error| DbError::io(&dir, error))?;
    if !directory_metadata.file_type().is_dir() {
        return Err(DbError::from_diag(
            Diagnostic::error(
                "NON_REGULAR_FILE",
                "table path must be a real directory, not a symlink",
            )
            .at(table),
            8,
        ));
    }
    let mut paths = vec![];
    for e in fs::read_dir(&dir).map_err(|e| DbError::io(&dir, e))? {
        paths.push(e.map_err(|e| DbError::io(&dir, e))?.path())
    }
    paths.sort();
    // Section 49: inference reads every row of the table, so it reports the
    // same scan progress an observation does.
    let mut progress = crate::output::Progress::new("inferring", paths.len());
    let mut rows = vec![];
    let ignores = config
        .ignore_set()
        .map_err(|error| DbError::new("INTERNAL_METADATA_CORRUPT", error, 6))?;
    for p in paths {
        progress.advance();
        let rel = p.strip_prefix(root).unwrap().to_path_buf();
        if ignores.is_match(&rel)
            || p.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| ignores.is_match(name))
        {
            continue;
        }
        let md = fs::symlink_metadata(&p).map_err(|e| DbError::io(&p, e))?;
        if md.is_dir() {
            return Err(DbError::from_diag(
                Diagnostic::error("INFER_NESTED_DIRECTORY", "nested directory inside table")
                    .at(rel),
                8,
            ));
        }
        if !md.is_file() {
            return Err(DbError::from_diag(
                Diagnostic::error("NON_REGULAR_FILE", "non-regular file inside table").at(rel),
                8,
            ));
        }
        if has_multiple_links(&md) {
            return Err(DbError::from_diag(
                Diagnostic::error("NON_REGULAR_FILE", "hard-linked row files are rejected").at(rel),
                8,
            ));
        }
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            return Err(DbError::from_diag(
                Diagnostic::error("INFER_NON_JSON_FILE", "non-.json file inside table").at(rel),
                8,
            ));
        }
        if md.len() > config.max_json_file_size {
            return Err(DbError::from_diag(
                Diagnostic::error(
                    "RESOURCE_LIMIT",
                    format!("file exceeds {} byte limit", config.max_json_file_size),
                )
                .at(rel),
                8,
            ));
        }
        let b = fs::read(&p).map_err(|e| DbError::io(&p, e))?;
        let v: Value = crate::json::parse(&b).map_err(|e| {
            // A document that is well formed but too deep is a resource-limit
            // refusal, not malformed JSON.
            if crate::json::is_depth_limit(&e) {
                return DbError::from_diag(
                    Diagnostic::error(
                        "RESOURCE_LIMIT",
                        format!(
                            "JSON nesting exceeds depth limit {}",
                            config.max_nesting_depth
                        ),
                    )
                    .at(rel.clone()),
                    8,
                );
            }
            let mut diagnostic =
                Diagnostic::error("INFER_INVALID_JSON", e.to_string()).at(rel.clone());
            diagnostic.location = Some(crate::diagnostic::Location {
                line: e.line(),
                column: e.column(),
            });
            diagnostic.source_line = String::from_utf8_lossy(&b)
                .lines()
                .nth(e.line().saturating_sub(1))
                .map(String::from);
            DbError::from_diag(diagnostic, 8)
        })?;
        let Some(obj) = v.as_object() else {
            return Err(DbError::from_diag(
                Diagnostic::error("INFER_ROOT_NOT_OBJECT", "row root is not an object").at(rel),
                8,
            ));
        };
        rows.push(Sample {
            path: rel,
            stem: p.file_stem().unwrap().to_string_lossy().into(),
            obj: obj.clone(),
        })
    }
    if rows.is_empty() {
        return Err(DbError::from_diag(
            Diagnostic::error(
                "INFER_NO_ROWS",
                format!("table {table:?} contains no .json rows"),
            )
            .at(table)
            .help(format!("run `reldir schema new {table}`")),
            8,
        ));
    }
    Ok(rows)
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

fn infer_table(
    table: &str,
    rows: &[Sample],
    strictness: Strictness,
    config: &Config,
    pk_override: Option<&[String]>,
) -> Result<Schema> {
    let mut names = BTreeSet::new();
    for r in rows {
        names.extend(r.obj.keys().cloned())
    }
    let mut columns = IndexMap::new();
    for name in names {
        let values: Vec<_> = rows.iter().filter_map(|r| r.obj.get(&name)).collect();
        let nullable = values.len() != rows.len() || values.iter().any(|v| v.is_null());
        let nonnull: Vec<_> = values.into_iter().filter(|v| !v.is_null()).collect();
        let col =
            infer_column(&name, &nonnull, nullable, strictness, config).map_err(|mut e| {
                if e.diagnostic.path.is_none() {
                    e.diagnostic.path = rows
                        .iter()
                        .find(|r| r.obj.contains_key(&name))
                        .map(|r| r.path.clone())
                }
                e
            })?;
        columns.insert(name, col);
    }
    let pk = if let Some(p) = pk_override {
        validate_pk_override(table, rows, &columns, p)?;
        p.to_vec()
    } else {
        infer_pk(table, rows, &columns)?
    };
    let mut unique = vec![];
    if rows.len() >= config.unique_min_rows {
        for name in columns.keys() {
            if pk.contains(name) || columns[name].nullable {
                continue;
            }
            let vals: HashSet<_> = rows
                .iter()
                .map(|r| canonical::compact(r.obj.get(name).unwrap_or(&Value::Null)))
                .collect();
            if vals.len() == rows.len() {
                unique.push(vec![name.clone()]);
            }
        }
    }
    let mut check = vec![];
    if strictness == Strictness::Strict && rows.len() >= config.unique_min_rows {
        for (name, column) in &columns {
            let values = rows.iter().filter_map(|row| row.obj.get(name));
            if column.kind == ColumnType::Int
                && values
                    .clone()
                    .all(|value| value.is_null() || value.as_i64().is_some_and(|value| value >= 0))
            {
                check.push(crate::schema::Check {
                    name: format!("{name}_nonnegative"),
                    expr: format!("\"{}\" >= 0", name.replace('"', "\"\"")),
                });
            } else if column.kind == ColumnType::String
                && values.clone().all(|value| {
                    value.is_null() || value.as_str().is_some_and(|value| !value.is_empty())
                })
            {
                check.push(crate::schema::Check {
                    name: format!("{name}_nonempty"),
                    expr: format!("\"{}\" <> ''", name.replace('"', "\"\"")),
                });
            }
        }
    }
    let schema = Schema {
        table: table.into(),
        schema_version: 1,
        schema_format: None,
        description: None,
        primary_key: pk,
        columns,
        unique,
        foreign_keys: vec![],
        check,
        indexes: vec![],
        storage: None,
        additional_fields: AdditionalFields::Reject,
        annotations: Default::default(),
    };
    for r in rows {
        let expected = canonical::filename(&schema, &r.obj);
        if expected.as_deref() != Some(&format!("{}.json", r.stem)) {
            return Err(DbError::from_diag(
                Diagnostic::error(
                    "INFER_FILENAME_INCONSISTENT",
                    format!(
                        "file name does not match inferred key; expected {}",
                        expected.unwrap_or_else(|| "a key-derived filename".into())
                    ),
                )
                .at(r.path.clone())
                .help("rename with `reldir doctor` or choose an appropriate x-reldir.filename"),
                8,
            ));
        }
    }
    Ok(schema)
}

fn validate_pk_override(
    table: &str,
    rows: &[Sample],
    columns: &IndexMap<String, Column>,
    requested: &[String],
) -> Result<()> {
    let mut names = BTreeSet::new();
    for name in requested {
        if !names.insert(name) {
            return Err(DbError::new(
                "INFER_NO_PRIMARY_KEY",
                format!("--pk repeats column {name:?} for table {table}"),
                8,
            ));
        }
        let column = columns.get(name).ok_or_else(|| {
            DbError::new(
                "INFER_NO_PRIMARY_KEY",
                format!("--pk names unknown column {name:?} for table {table}"),
                8,
            )
        })?;
        if column.nullable {
            let path = rows
                .iter()
                .find(|row| row.obj.get(name).is_none_or(Value::is_null))
                .map(|row| row.path.display().to_string())
                .unwrap_or_else(|| "an observed row".into());
            return Err(DbError::new(
                "INFER_NO_PRIMARY_KEY",
                format!("--pk column {name:?} is null or absent in {path}"),
                8,
            ));
        }
    }
    let mut seen = BTreeMap::<String, &Sample>::new();
    for row in rows {
        let key = Value::Array(
            requested
                .iter()
                .map(|name| row.obj.get(name).cloned().unwrap_or(Value::Null))
                .collect(),
        );
        let key = canonical::compact(&key);
        if let Some(previous) = seen.insert(key.clone(), row) {
            return Err(DbError::new(
                "INFER_NO_PRIMARY_KEY",
                format!(
                    "--pk is not unique: duplicate value {key} in {} and {}",
                    previous.path.display(),
                    row.path.display()
                ),
                8,
            ));
        }
    }
    Ok(())
}

fn infer_column(
    name: &str,
    values: &[&Value],
    nullable: bool,
    strictness: Strictness,
    config: &Config,
) -> Result<Column> {
    if values.is_empty() {
        if strictness == Strictness::Strict {
            return Err(DbError::new(
                "INFER_UNTYPED_COLUMN",
                format!("column {name:?} is null or absent in every row"),
                8,
            ));
        }
        return Ok(column(ColumnType::Json, true));
    }
    let kinds: HashSet<_> = values
        .iter()
        .map(|v| {
            if v.is_boolean() {
                "bool"
            } else if v.is_number() {
                "number"
            } else if v.is_string() {
                "string"
            } else if v.is_array() {
                "array"
            } else if v.is_object() {
                "object"
            } else {
                "null"
            }
        })
        .collect();
    if kinds.len() > 1 {
        if strictness != Strictness::Loose {
            return Err(DbError::new(
                "INFER_TYPE_CONFLICT",
                format!("column {name:?} has incompatible JSON kinds: {kinds:?}"),
                8,
            ));
        }
        return Ok(column(ColumnType::Json, nullable));
    }
    let first = values[0];
    let mut c = if first.is_boolean() {
        column(ColumnType::Bool, nullable)
    } else if first.is_number() {
        if values.iter().all(|v| v.as_i64().is_some()) {
            column(ColumnType::Int, nullable)
        } else {
            column(ColumnType::Float, nullable)
        }
    } else if first.is_string() {
        let ss: Vec<_> = values.iter().map(|v| v.as_str().unwrap()).collect();
        if ss.iter().all(|s| {
            uuid::Uuid::parse_str(s).is_ok() && s.len() == 36 && *s == s.to_ascii_lowercase()
        }) {
            column(ColumnType::Uuid, nullable)
        } else if ss.iter().all(|s| {
            ulid::Ulid::from_string(s).is_ok() && s.len() == 26 && *s == s.to_ascii_uppercase()
        }) {
            column(ColumnType::Ulid, nullable)
        } else if ss
            .iter()
            .all(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok())
        {
            column(ColumnType::Timestamp, nullable)
        } else if ss
            .iter()
            .all(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() && s.len() == 10)
        {
            column(ColumnType::Date, nullable)
        } else {
            let distinct: BTreeSet<_> = ss.iter().copied().collect();
            if distinct.len() <= config.enum_max_values && values.len() >= 3 * distinct.len() {
                let mut x = column(ColumnType::Enum, nullable);
                x.values = Some(distinct.into_iter().map(Into::into).collect());
                x
            } else {
                column(ColumnType::String, nullable)
            }
        }
    } else if first.is_array() {
        let elements: Vec<_> = values.iter().flat_map(|v| v.as_array().unwrap()).collect();
        let mut x = column(ColumnType::Array, nullable);
        x.items = Some(Box::new(infer_column(
            &format!("{name}[]"),
            &elements,
            false,
            strictness,
            config,
        )?));
        x
    } else if first.is_object() {
        let mut keys = BTreeSet::new();
        for v in values {
            keys.extend(v.as_object().unwrap().keys().cloned())
        }
        let mut props = IndexMap::new();
        for k in keys {
            let nested: Vec<_> = values
                .iter()
                .filter_map(|v| v.as_object().unwrap().get(&k))
                .collect();
            props.insert(
                k.clone(),
                infer_column(
                    &format!("{name}.{k}"),
                    &nested,
                    nested.len() != values.len(),
                    strictness,
                    config,
                )?,
            );
        }
        let mut x = column(ColumnType::Object, nullable);
        x.properties = Some(props);
        x
    } else {
        column(ColumnType::Json, nullable)
    };
    c.nullable = nullable;
    Ok(c)
}
fn column(kind: ColumnType, nullable: bool) -> Column {
    Column {
        kind,
        nullable,
        default: None,
        generated: None,
        values: None,
        items: None,
        properties: None,
        // Inference reports the shape it observed. A pattern is a claim about
        // values never sampled, so guessing one from the rows present would
        // invent a constraint the corpus never stated.
        pattern: None,
        additional_properties: true,
        required: Default::default(),
        min_size: None,
        max_size: None,
        minimum: None,
        maximum: None,
        exclusive_minimum: None,
        exclusive_maximum: None,
        multiple_of: None,
        unique_items: false,
        composition: None,
        description: None,
        annotations: Default::default(),
    }
}
fn infer_pk(
    table: &str,
    rows: &[Sample],
    columns: &IndexMap<String, Column>,
) -> Result<Vec<String>> {
    let mut candidates = vec![];
    let mut rejected = vec![];
    for (n, c) in columns {
        if c.nullable {
            let example = rows
                .iter()
                .find(|row| row.obj.get(n).is_none_or(Value::is_null))
                .map(|row| row.path.display().to_string())
                .unwrap_or_else(|| "an observed row".into());
            rejected.push(format!("{n}: null or absent in {example}"));
            continue;
        }
        let mut first = BTreeMap::<String, &Sample>::new();
        let mut duplicate = None;
        for row in rows {
            if let Some(value) = row.obj.get(n) {
                let value = canonical::compact(value);
                if let Some(previous) = first.insert(value.clone(), row) {
                    duplicate = Some(format!(
                        "duplicate value {value} in {} and {}",
                        previous.path.display(),
                        row.path.display()
                    ));
                    break;
                }
            }
        }
        if let Some(reason) = duplicate {
            rejected.push(format!("{n}: {reason}"));
        } else if first.len() == rows.len() {
            candidates.push(n.clone())
        } else {
            rejected.push(format!("{n}: missing in an observed row"));
        }
    }
    let filename_matches: Vec<_> = candidates
        .iter()
        .filter(|n| {
            rows.iter().all(|r| {
                let value = r.obj.get(*n).unwrap();
                let fake = Schema {
                    table: table.into(),
                    schema_version: 1,
                    schema_format: None,
                    description: None,
                    primary_key: vec![(*n).clone()],
                    columns: columns.clone(),
                    unique: vec![],
                    foreign_keys: vec![],
                    check: vec![],
                    indexes: vec![],
                    storage: None,
                    additional_fields: AdditionalFields::Reject,
                    annotations: Default::default(),
                };
                canonical::filename(&fake, &r.obj).as_deref() == Some(&format!("{}.json", r.stem))
                    && !value.is_null()
            })
        })
        .cloned()
        .collect();
    if filename_matches.len() == 1 {
        return Ok(vec![filename_matches[0].clone()]);
    }
    if candidates.contains(&"id".into()) {
        return Ok(vec!["id".into()]);
    }
    let conventional = format!("{}_id", singular(table));
    if candidates.contains(&conventional) {
        return Ok(vec![conventional]);
    }
    if candidates.len() == 1 {
        return Ok(vec![candidates[0].clone()]);
    }
    if candidates.is_empty() {
        Err(DbError::new(
            "INFER_NO_PRIMARY_KEY",
            format!(
                "no non-null distinct column can serve as a primary key; {}",
                rejected.join("; ")
            ),
            8,
        ))
    } else {
        Err(DbError::new(
            "INFER_AMBIGUOUS_PRIMARY_KEY",
            format!(
                "several primary-key candidates remain: {}; use --pk",
                candidates.join(", ")
            ),
            8,
        ))
    }
}
fn singular(s: &str) -> String {
    s.strip_suffix('s').unwrap_or(s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn values(items: &[Value]) -> Vec<&Value> {
        items.iter().collect()
    }

    fn infer(items: &[Value], nullable: bool, strictness: Strictness) -> Result<Column> {
        infer_column(
            "c",
            &values(items),
            nullable,
            strictness,
            &Config::default(),
        )
    }

    /// Section 12: the type ladder picks the narrowest type every observed value
    /// satisfies, so that inference produces the strictest valid schema.
    #[test]
    fn test1037_the_type_ladder_prefers_the_narrowest_valid_type() {
        let cases: Vec<(Vec<Value>, ColumnType)> = vec![
            (vec![json!(true), json!(false)], ColumnType::Bool),
            (vec![json!(1), json!(-2)], ColumnType::Int),
            (vec![json!(1.5), json!(2)], ColumnType::Float),
            (
                vec![
                    json!("0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e"),
                    json!("0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8f"),
                ],
                ColumnType::Uuid,
            ),
            (
                vec![
                    json!("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
                    json!("01ARZ3NDEKTSV4RRFFQ69G5FAW"),
                ],
                ColumnType::Ulid,
            ),
            (
                vec![json!("2026-09-14T10:00:00Z"), json!("2026-09-15T10:00:00Z")],
                ColumnType::Timestamp,
            ),
            (
                vec![json!("2026-09-14"), json!("2026-09-15")],
                ColumnType::Date,
            ),
            (vec![json!("free form text")], ColumnType::String),
        ];
        for (items, expected) in cases {
            let column = infer(&items, false, Strictness::Balanced).unwrap();
            assert_eq!(column.kind, expected, "for {items:?}");
        }
    }

    /// Section 12: an integer-valued float column stays float only when a
    /// fractional value is actually observed; a whole number is an int.
    #[test]
    fn test1038_integers_are_not_widened_to_float() {
        let column = infer(&[json!(1), json!(2), json!(3)], false, Strictness::Balanced).unwrap();
        assert_eq!(column.kind, ColumnType::Int);
        let column = infer(&[json!(1), json!(2.5)], false, Strictness::Balanced).unwrap();
        assert_eq!(column.kind, ColumnType::Float);
    }

    /// Section 12: a low-cardinality string column becomes an enum only when the
    /// sample is large enough to justify it, so a small sample is not over-fit.
    #[test]
    fn test1039_enum_inference_requires_supporting_evidence() {
        // Three rows per distinct value satisfies the ratio rule.
        let plenty: Vec<Value> = (0..9)
            .map(|index| json!(if index % 3 == 0 { "a" } else { "b" }))
            .collect();
        let column = infer(&plenty, false, Strictness::Balanced).unwrap();
        assert_eq!(column.kind, ColumnType::Enum);
        let members = column.values.unwrap();
        assert_eq!(members, vec!["a".to_string(), "b".to_string()]);

        // Two distinct values across two rows is not enough evidence.
        let sparse = vec![json!("a"), json!("b")];
        let column = infer(&sparse, false, Strictness::Balanced).unwrap();
        assert_eq!(column.kind, ColumnType::String);
    }

    /// Section 12: mixed JSON kinds are a conflict that strict and balanced
    /// inference refuse; only loose widens to the opaque json type.
    #[test]
    fn test1040_mixed_kinds_are_refused_except_under_loose_strictness() {
        let mixed = [json!(1), json!("text")];
        for strictness in [Strictness::Strict, Strictness::Balanced] {
            let error = infer(&mixed, false, strictness).unwrap_err();
            assert_eq!(error.diagnostic.code, "INFER_TYPE_CONFLICT");
            assert_eq!(error.exit, 8);
        }
        let column = infer(&mixed, false, Strictness::Loose).unwrap();
        assert_eq!(column.kind, ColumnType::Json);
    }

    /// Section 12: a column with no observed value cannot be typed. Strict
    /// refuses; the looser modes fall back to a nullable json column.
    #[test]
    fn test1041_an_unobserved_column_cannot_be_typed_under_strict() {
        let error = infer(&[], true, Strictness::Strict).unwrap_err();
        assert_eq!(error.diagnostic.code, "INFER_UNTYPED_COLUMN");

        for strictness in [Strictness::Balanced, Strictness::Loose] {
            let column = infer(&[], true, strictness).unwrap();
            assert_eq!(column.kind, ColumnType::Json);
            assert!(column.nullable, "an unobserved column must be nullable");
        }
    }

    /// Section 12: arrays infer their element type over every element of every
    /// row, so a single row cannot fix the element type for the rest.
    #[test]
    fn test1042_array_items_are_inferred_across_all_rows() {
        let column = infer(&[json!([1, 2]), json!([3])], false, Strictness::Balanced).unwrap();
        assert_eq!(column.kind, ColumnType::Array);
        assert_eq!(column.items.unwrap().kind, ColumnType::Int);

        // A conflicting element across rows is still a conflict.
        let error = infer(&[json!([1]), json!(["text"])], false, Strictness::Balanced).unwrap_err();
        assert_eq!(error.diagnostic.code, "INFER_TYPE_CONFLICT");
    }

    /// Section 12: object properties are inferred recursively, and a property
    /// missing from some rows is nullable.
    #[test]
    fn test1043_object_properties_are_inferred_recursively_with_nullability() {
        let column = infer(
            &[json!({"a": 1, "b": "x"}), json!({"a": 2})],
            false,
            Strictness::Balanced,
        )
        .unwrap();
        assert_eq!(column.kind, ColumnType::Object);
        let properties = column.properties.unwrap();
        assert_eq!(properties["a"].kind, ColumnType::Int);
        assert!(!properties["a"].nullable, "present in every row");
        assert_eq!(properties["b"].kind, ColumnType::String);
        assert!(properties["b"].nullable, "absent from one row");
    }

    /// Section 12: a uuid must be canonical lowercase and a ulid canonical
    /// uppercase, otherwise the value is merely a string.
    #[test]
    fn test1044_identifier_types_require_canonical_spelling() {
        let upper_uuid = infer(
            &[json!("0193B1F4-7C3A-7B1E-9C2D-3F4A5B6C7D8E")],
            false,
            Strictness::Balanced,
        )
        .unwrap();
        assert_eq!(upper_uuid.kind, ColumnType::String);

        let lower_ulid = infer(
            &[json!("01arz3ndektsv4rrffq69g5fav")],
            false,
            Strictness::Balanced,
        )
        .unwrap();
        assert_eq!(lower_ulid.kind, ColumnType::String);
    }

    /// Section 12: nullability is recorded as observed, independently of type.
    #[test]
    fn test1045_nullability_is_propagated_onto_the_inferred_column() {
        let column = infer(&[json!("x")], true, Strictness::Balanced).unwrap();
        assert_eq!(column.kind, ColumnType::String);
        assert!(column.nullable);

        let column = infer(&[json!("x")], false, Strictness::Balanced).unwrap();
        assert!(!column.nullable);
    }

    /// Section 12: a table name's singular form drives conventional primary-key
    /// and foreign-key naming.
    #[test]
    fn test1046_singular_forms_drive_conventional_names() {
        assert_eq!(singular("users"), "user");
        assert_eq!(singular("posts"), "post");
        // A name that is already singular is unchanged.
        assert_eq!(singular("team"), "team");
    }
}
