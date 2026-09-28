//! Inferring schemas from rows.
//!
//! Inference writes documents, not models: each column's subschema is built as
//! JSON Schema from the values observed, the table is assembled with
//! [`TableBuilder`], and the result is read back through the one schema
//! decoder, so an inferred schema obeys every rule a hand-written one does.
//!
//! References are found by [`crate::analysis::references`], the same analysis
//! lint and doctor use. Inference declares only the conventional ones -- those
//! named as `reference_naming` names a reference -- because a declaration is a
//! rule every future write must satisfy, and only a name is evidence of intent.

use crate::{
    analysis::references::{self, ProposedTarget, TableFacts},
    canonical,
    catalog::Catalog,
    config::Config,
    diagnostic::{DbError, Diagnostic, Result},
    schema::{
        ColumnType, Schema,
        document::{TableBuilder, enum_subschema, subschema},
    },
};
use serde_json::{Map, Value, json};
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
    obj: Map<String, Value>,
}

fn fail(code: &str, message: impl Into<String>) -> DbError {
    DbError::new(code, message, 8)
}

/// The immediate child directories that hold JSON files: the tables a folder
/// would have.
pub fn discover_tables(root: &Path) -> Result<Vec<String>> {
    let mut out = vec![];
    for entry in fs::read_dir(root).map_err(|e| DbError::io(root, e))? {
        let path = entry.map_err(|e| DbError::io(root, e))?.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if matches!(name, "schema" | ".db" | ".git") || name.starts_with('.') {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|error| DbError::io(&path, error))?;
        if !metadata.file_type().is_dir() {
            continue;
        }
        let mut contains_json = false;
        for entry in fs::read_dir(&path).map_err(|e| DbError::io(&path, e))? {
            let child = entry.map_err(|e| DbError::io(&path, e))?.path();
            contains_json |= child.extension().and_then(|x| x.to_str()) == Some("json");
        }
        if contains_json {
            out.push(name.to_string());
        }
    }
    out.sort();
    Ok(out)
}

/// Infer schemas for `tables`. `governed`, when given, is the database the
/// tables join: its tables are reference targets, and its schemas are not
/// re-inferred.
pub fn infer_all(
    root: &Path,
    tables: &[String],
    strictness: Strictness,
    config: &Config,
    primary_key: Option<&[String]>,
    governed: Option<&Catalog>,
) -> Result<BTreeMap<String, Schema>> {
    let mut builders: BTreeMap<String, (TableBuilder, Vec<String>, ColumnType)> = BTreeMap::new();
    let mut samples: BTreeMap<String, Vec<Sample>> = BTreeMap::new();
    for table in tables {
        if !crate::schema::valid_name(table) {
            return Err(DbError::from_diag(
                Diagnostic::error(
                    "SCHEMA_INVALID_TABLE_NAME",
                    format!(
                        "{table:?} cannot be a table name: names are lowercase letters, digits and \
                         underscores, starting with a letter"
                    ),
                )
                .at(table.as_str()),
                8,
            ));
        }
        let rows = load_samples(root, table, config)?;
        let (builder, key, kind) = infer_table(table, &rows, strictness, config, primary_key)?;
        builders.insert(table.clone(), (builder, key, kind));
        samples.insert(table.clone(), rows);
    }

    // References, found over the new tables and the database they join.
    let mut facts = BTreeMap::new();
    for (table, (_, key, kind)) in &builders {
        let single = (key.len() == 1).then(|| (key[0].clone(), kind.clone()));
        facts.insert(
            table.clone(),
            TableFacts::gather(table, single, None, samples[table].iter().map(|s| &s.obj)),
        );
    }
    if let Some(catalog) = governed {
        for (table, schema) in &catalog.schemas {
            if builders.contains_key(table) {
                continue;
            }
            let rows = catalog.rows(table)?;
            let key = (schema.primary_key().len() == 1)
                .then(|| {
                    schema
                        .column(&schema.primary_key()[0])
                        .map(|c| (schema.primary_key()[0].clone(), c.kind().clone()))
                })
                .flatten();
            facts.insert(
                table.clone(),
                TableFacts::gather(table, key, Some(schema), rows.iter().map(|r| &r.value)),
            );
        }
    }
    for proposal in references::detect(&facts, config) {
        if !proposal.conventional || !builders.contains_key(&proposal.table) {
            continue;
        }
        if let ProposedTarget::Domain { name, join, .. } = &proposal.target {
            // A table the database already governs is joined to a domain by
            // editing its schema, which inference does not do on its own.
            if join.iter().any(|table| !builders.contains_key(table)) {
                continue;
            }
            for table in join {
                builders
                    .get_mut(table)
                    .expect("checked above")
                    .0
                    .identity_domain(Some(name.clone()));
            }
        }
        builders
            .get_mut(&proposal.table)
            .expect("checked above")
            .0
            .foreign_key(proposal.definition());
    }

    let mut out = BTreeMap::new();
    for (table, (builder, _, _)) in builders {
        let schema = builder.build().map_err(|problems| {
            let first = problems.into_iter().next().unwrap_or_else(|| {
                Diagnostic::error("SCHEMA_INVALID", "the inferred schema is invalid")
            });
            DbError::from_diag(first.table(table.as_str()), 8)
        })?;
        for row in &samples[&table] {
            let expected = canonical::filename(&schema, &row.obj);
            if expected.as_deref() != Some(&format!("{}.json", row.stem)) {
                return Err(DbError::from_diag(
                    Diagnostic::error(
                        "INFER_FILENAME_INCONSISTENT",
                        format!(
                            "the file is not named by the inferred key; its key names it {}",
                            expected.unwrap_or_else(|| "nothing (the key is null)".into())
                        ),
                    )
                    .at(row.path.clone())
                    .help(
                        "rename the file, choose the key with --pk, or declare x-reldir.filename",
                    ),
                    8,
                ));
            }
        }
        out.insert(table, schema);
    }
    Ok(out)
}

/// Read every row of a table as an inference sample, under the configured
/// size and nesting bounds.
fn load_samples(root: &Path, table: &str, config: &Config) -> Result<Vec<Sample>> {
    crate::json::with_depth_limit(config.max_nesting_depth, || {
        load_samples_bounded(root, table, config)
    })
}

fn load_samples_bounded(root: &Path, table: &str, config: &Config) -> Result<Vec<Sample>> {
    let dir = root.join(table);
    let metadata = match fs::symlink_metadata(&dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(DbError::from_diag(
                Diagnostic::error(
                    "INFER_NO_ROWS",
                    format!("table directory {table:?} does not exist"),
                )
                .at(table),
                8,
            ));
        }
        Err(error) => return Err(DbError::io(&dir, error)),
    };
    if !metadata.file_type().is_dir() {
        return Err(DbError::from_diag(
            Diagnostic::error(
                "NON_REGULAR_FILE",
                "a table path must be a real directory, not a symlink",
            )
            .at(table),
            8,
        ));
    }
    let mut paths = vec![];
    for entry in fs::read_dir(&dir).map_err(|e| DbError::io(&dir, e))? {
        paths.push(entry.map_err(|e| DbError::io(&dir, e))?.path());
    }
    paths.sort();
    let mut progress = crate::output::Progress::new("inferring", paths.len());
    let ignores = config
        .ignore_set()
        .map_err(|error| DbError::new("CONFIG_INVALID", error, 1))?;
    let mut rows = vec![];
    for path in paths {
        progress.advance();
        let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if ignores.is_match(&relative)
            || ignores.is_match(name)
            || crate::metadata::is_in_progress_write(&path)
        {
            continue;
        }
        let md = fs::symlink_metadata(&path).map_err(|e| DbError::io(&path, e))?;
        let refuse = |code: &str, message: &str| {
            DbError::from_diag(Diagnostic::error(code, message).at(relative.clone()), 8)
        };
        if md.is_dir() {
            return Err(refuse(
                "INFER_NESTED_DIRECTORY",
                "a table directory holds rows, not directories",
            ));
        }
        if !md.is_file() || crate::catalog::has_multiple_links(&md) {
            return Err(refuse(
                "NON_REGULAR_FILE",
                "rows are private regular files: no symlinks or hard links",
            ));
        }
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            return Err(refuse(
                "INFER_NON_JSON_FILE",
                "a table directory holds only .json files",
            ));
        }
        if md.len() > config.max_json_file_size {
            return Err(refuse(
                "RESOURCE_LIMIT",
                &format!(
                    "the file exceeds the {} byte limit",
                    config.max_json_file_size
                ),
            ));
        }
        let bytes = fs::read(&path).map_err(|e| DbError::io(&path, e))?;
        let value = crate::json::parse(&bytes).map_err(|error| {
            if crate::json::is_depth_limit(&error) {
                return refuse(
                    "RESOURCE_LIMIT",
                    &format!(
                        "JSON nesting exceeds the depth limit {}",
                        config.max_nesting_depth
                    ),
                );
            }
            let mut diagnostic =
                Diagnostic::error("INFER_INVALID_JSON", error.to_string()).at(relative.clone());
            diagnostic.location = Some(crate::diagnostic::Location {
                line: error.line(),
                column: error.column(),
            });
            diagnostic.source_line = String::from_utf8_lossy(&bytes)
                .lines()
                .nth(error.line().saturating_sub(1))
                .map(String::from);
            DbError::from_diag(diagnostic, 8)
        })?;
        let Value::Object(obj) = value else {
            return Err(refuse(
                "INFER_ROOT_NOT_OBJECT",
                "a row file holds one JSON object",
            ));
        };
        rows.push(Sample {
            path: relative,
            stem: path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default(),
            obj,
        });
    }
    if rows.is_empty() {
        return Err(DbError::from_diag(
            Diagnostic::error(
                "INFER_NO_ROWS",
                format!("table {table:?} holds no .json rows"),
            )
            .at(table)
            .help(format!("declare it with `reldir schema new {table}`")),
            8,
        ));
    }
    Ok(rows)
}

/// One column as inferred: its type and the subschema describing it.
struct Inferred {
    kind: ColumnType,
    subschema: Value,
}

fn infer_table(
    table: &str,
    rows: &[Sample],
    strictness: Strictness,
    config: &Config,
    requested_key: Option<&[String]>,
) -> Result<(TableBuilder, Vec<String>, ColumnType)> {
    // Columns in the order the rows first present them.
    let mut names = indexmap::IndexSet::new();
    for row in rows {
        names.extend(row.obj.keys().cloned());
    }
    let mut columns: indexmap::IndexMap<String, (Inferred, bool)> = indexmap::IndexMap::new();
    for name in &names {
        let present: Vec<&Value> = rows.iter().filter_map(|row| row.obj.get(name)).collect();
        let nullable = present.len() != rows.len() || present.iter().any(|value| value.is_null());
        let values: Vec<&Value> = present
            .into_iter()
            .filter(|value| !value.is_null())
            .collect();
        let inferred =
            infer_value(name, &values, nullable, strictness, config).map_err(|mut error| {
                if error.diagnostic.path.is_none() {
                    error.diagnostic.path = rows
                        .iter()
                        .find(|row| row.obj.contains_key(name))
                        .map(|row| row.path.clone());
                }
                error
            })?;
        let required = rows.iter().all(|row| row.obj.contains_key(name));
        columns.insert(name.clone(), (inferred, required && !nullable));
    }
    let key = match requested_key {
        Some(key) => {
            validate_requested_key(table, rows, &columns, key)?;
            key.to_vec()
        }
        None => infer_primary_key(table, rows, &columns)?,
    };
    let key_kind = columns[&key[0]].0.kind.clone();
    let mut builder = TableBuilder::new(table);
    for (name, (inferred, required)) in &columns {
        builder.column(
            name,
            inferred.subschema.clone(),
            *required || key.contains(name),
        );
    }
    builder.primary_key(key.clone());
    if rows.len() >= config.unique_min_rows {
        for (name, (_, required)) in &columns {
            if key.contains(name) || !required {
                continue;
            }
            let distinct: HashSet<String> = rows
                .iter()
                .map(|row| canonical::compact(row.obj.get(name).unwrap_or(&Value::Null)))
                .collect();
            if distinct.len() == rows.len() {
                builder.unique(vec![name.clone()]);
            }
        }
    }
    if strictness == Strictness::Strict && rows.len() >= config.unique_min_rows {
        for (name, (inferred, _)) in &columns {
            let values = || {
                rows.iter()
                    .filter_map(|row| row.obj.get(name))
                    .filter(|value| !value.is_null())
            };
            let quoted = crate::mirror::quote(name);
            if inferred.kind == ColumnType::Int
                && values().all(|value| value.as_i64().is_some_and(|v| v >= 0))
            {
                builder.check(&format!("{name}_nonnegative"), &format!("{quoted} >= 0"));
            } else if inferred.kind == ColumnType::String
                && values().all(|value| value.as_str().is_some_and(|v| !v.is_empty()))
            {
                builder.check(&format!("{name}_nonempty"), &format!("{quoted} <> ''"));
            }
        }
    }
    Ok((builder, key, key_kind))
}

fn validate_requested_key(
    table: &str,
    rows: &[Sample],
    columns: &indexmap::IndexMap<String, (Inferred, bool)>,
    requested: &[String],
) -> Result<()> {
    let mut seen_names = BTreeSet::new();
    for name in requested {
        if !seen_names.insert(name) {
            return Err(fail(
                "INFER_NO_PRIMARY_KEY",
                format!("--pk repeats column {name:?} for table {table}"),
            ));
        }
        if !columns.contains_key(name) {
            return Err(fail(
                "INFER_NO_PRIMARY_KEY",
                format!("--pk names {name:?}, which no row of {table} holds"),
            ));
        }
        if let Some(row) = rows
            .iter()
            .find(|row| row.obj.get(name).is_none_or(Value::is_null))
        {
            return Err(fail(
                "INFER_NO_PRIMARY_KEY",
                format!(
                    "--pk column {name:?} is null or absent in {}",
                    row.path.display()
                ),
            ));
        }
    }
    let mut seen = BTreeMap::<String, &Sample>::new();
    for row in rows {
        let key = canonical::compact(&Value::Array(
            requested.iter().map(|name| row.obj[name].clone()).collect(),
        ));
        if let Some(previous) = seen.insert(key.clone(), row) {
            return Err(fail(
                "INFER_NO_PRIMARY_KEY",
                format!(
                    "--pk is not unique: {key} is in {} and {}",
                    previous.path.display(),
                    row.path.display()
                ),
            ));
        }
    }
    Ok(())
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        Value::Null => "null",
    }
}

/// The narrowest description every observed value satisfies.
fn infer_value(
    name: &str,
    values: &[&Value],
    nullable: bool,
    strictness: Strictness,
    config: &Config,
) -> Result<Inferred> {
    if values.is_empty() {
        if strictness == Strictness::Strict {
            return Err(fail(
                "INFER_UNTYPED_COLUMN",
                format!("{name:?} is null or absent in every row"),
            ));
        }
        return Ok(Inferred {
            kind: ColumnType::Json,
            subschema: json!({}),
        });
    }
    let kinds: BTreeSet<&str> = values.iter().map(|value| kind_of(value)).collect();
    if kinds.len() > 1 {
        if strictness != Strictness::Loose {
            return Err(fail(
                "INFER_TYPE_CONFLICT",
                format!(
                    "{name:?} holds incompatible JSON kinds ({}); use --strictness loose to accept any JSON there",
                    kinds.into_iter().collect::<Vec<_>>().join(", ")
                ),
            ));
        }
        return Ok(Inferred {
            kind: ColumnType::Json,
            subschema: json!({}),
        });
    }
    let scalar = |kind: ColumnType| Inferred {
        subschema: subschema(&kind, nullable),
        kind,
    };
    Ok(match values[0] {
        Value::Bool(_) => scalar(ColumnType::Bool),
        Value::Number(_) => {
            if values.iter().all(|value| {
                value.as_i64().is_some() && !value.as_number().is_some_and(|n| n.is_f64())
            }) {
                scalar(ColumnType::Int)
            } else {
                scalar(ColumnType::Float)
            }
        }
        Value::String(_) => {
            let texts: Vec<&str> = values.iter().filter_map(|value| value.as_str()).collect();
            if texts.iter().all(|s| {
                s.len() == 36 && uuid::Uuid::parse_str(s).is_ok() && *s == s.to_ascii_lowercase()
            }) {
                scalar(ColumnType::Uuid)
            } else if texts.iter().all(|s| {
                s.len() == 26 && ulid::Ulid::from_string(s).is_ok() && *s == s.to_ascii_uppercase()
            }) {
                scalar(ColumnType::Ulid)
            } else if texts
                .iter()
                .all(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok())
            {
                scalar(ColumnType::Timestamp)
            } else if texts
                .iter()
                .all(|s| s.len() == 10 && chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok())
            {
                scalar(ColumnType::Date)
            } else {
                let distinct: BTreeSet<&str> = texts.iter().copied().collect();
                if distinct.len() <= config.enum_max_values && values.len() >= 3 * distinct.len() {
                    let members: Vec<String> = distinct.into_iter().map(String::from).collect();
                    Inferred {
                        kind: ColumnType::Enum,
                        subschema: enum_subschema(&members, nullable),
                    }
                } else {
                    scalar(ColumnType::String)
                }
            }
        }
        Value::Array(_) => {
            let elements: Vec<&Value> = values
                .iter()
                .flat_map(|value| value.as_array().into_iter().flatten())
                .collect();
            let mut schema = subschema(&ColumnType::Array, nullable);
            if !elements.is_empty() {
                let nonnull: Vec<&Value> = elements
                    .iter()
                    .copied()
                    .filter(|value| !value.is_null())
                    .collect();
                let items = infer_value(
                    &format!("{name}[]"),
                    &nonnull,
                    nonnull.len() != elements.len(),
                    strictness,
                    config,
                )?;
                schema["items"] = items.subschema;
            }
            Inferred {
                kind: ColumnType::Array,
                subschema: schema,
            }
        }
        Value::Object(_) => {
            let mut keys = indexmap::IndexSet::new();
            for value in values {
                keys.extend(
                    value
                        .as_object()
                        .into_iter()
                        .flat_map(|object| object.keys().cloned()),
                );
            }
            let mut properties = Map::new();
            let mut required = vec![];
            for key in keys {
                let present: Vec<&Value> = values
                    .iter()
                    .filter_map(|value| value.as_object().and_then(|object| object.get(&key)))
                    .collect();
                let everywhere = present.len() == values.len();
                let nonnull: Vec<&Value> = present
                    .iter()
                    .copied()
                    .filter(|value| !value.is_null())
                    .collect();
                let member = infer_value(
                    &format!("{name}.{key}"),
                    &nonnull,
                    nonnull.len() != present.len(),
                    strictness,
                    config,
                )?;
                if everywhere && nonnull.len() == present.len() {
                    required.push(key.clone());
                }
                properties.insert(key, member.subschema);
            }
            let mut schema = subschema(&ColumnType::Object, nullable);
            schema["properties"] = Value::Object(properties);
            if !required.is_empty() {
                schema["required"] = json!(required);
            }
            Inferred {
                kind: ColumnType::Object,
                subschema: schema,
            }
        }
        Value::Null => Inferred {
            kind: ColumnType::Json,
            subschema: json!({}),
        },
    })
}

fn infer_primary_key(
    table: &str,
    rows: &[Sample],
    columns: &indexmap::IndexMap<String, (Inferred, bool)>,
) -> Result<Vec<String>> {
    let mut candidates = vec![];
    let mut rejected = vec![];
    for (name, (inferred, _)) in columns {
        if !inferred.kind.is_scalar() {
            rejected.push(format!(
                "{name}: {} values cannot be a key",
                inferred.kind.name()
            ));
            continue;
        }
        if let Some(row) = rows
            .iter()
            .find(|row| row.obj.get(name).is_none_or(Value::is_null))
        {
            rejected.push(format!("{name}: null or absent in {}", row.path.display()));
            continue;
        }
        let mut first = BTreeMap::<String, &Sample>::new();
        let mut duplicate = None;
        for row in rows {
            let value = canonical::compact(&row.obj[name]);
            if let Some(previous) = first.insert(value.clone(), row) {
                duplicate = Some(format!(
                    "{value} is in {} and {}",
                    previous.path.display(),
                    row.path.display()
                ));
                break;
            }
        }
        match duplicate {
            Some(reason) => rejected.push(format!("{name}: {reason}")),
            None => candidates.push(name.clone()),
        }
    }
    // A key that names every file is the strongest evidence there is.
    let naming: Vec<&String> = candidates
        .iter()
        .filter(|name| {
            let mut builder = TableBuilder::new(table);
            for (column, (inferred, _)) in columns {
                builder.column(column, inferred.subschema.clone(), false);
            }
            builder.primary_key(vec![(*name).clone()]);
            builder.column(name, columns[*name].0.subschema.clone(), true);
            match builder.build() {
                Ok(trial) => rows.iter().all(|row| {
                    canonical::filename(&trial, &row.obj).as_deref()
                        == Some(&format!("{}.json", row.stem))
                }),
                Err(_) => false,
            }
        })
        .collect();
    if naming.len() == 1 {
        return Ok(vec![naming[0].clone()]);
    }
    let conventional = format!("{}_id", table.strip_suffix('s').unwrap_or(table));
    for preferred in ["id", conventional.as_str()] {
        if candidates.iter().any(|name| name == preferred) {
            return Ok(vec![preferred.to_string()]);
        }
    }
    match candidates.len() {
        1 => Ok(candidates),
        0 => Err(fail(
            "INFER_NO_PRIMARY_KEY",
            format!(
                "no column of {table} is present, non-null and distinct in every row: {}",
                rejected.join("; ")
            ),
        )),
        _ => Err(fail(
            "INFER_AMBIGUOUS_PRIMARY_KEY",
            format!(
                "several columns of {table} could be its key: {}; choose one with --pk",
                candidates.join(", ")
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn infer(items: &[Value], nullable: bool, strictness: Strictness) -> Result<Inferred> {
        let values: Vec<&Value> = items.iter().collect();
        infer_value("c", &values, nullable, strictness, &Config::default())
    }

    fn write(root: &Path, relative: &str, value: Value) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    #[test]
    fn test1037_the_type_ladder_prefers_the_narrowest_valid_type() {
        let cases: Vec<(Vec<Value>, ColumnType)> = vec![
            (vec![json!(true), json!(false)], ColumnType::Bool),
            (vec![json!(1), json!(-2)], ColumnType::Int),
            (vec![json!(1.5), json!(2)], ColumnType::Float),
            (
                vec![json!("0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e")],
                ColumnType::Uuid,
            ),
            (vec![json!("01ARZ3NDEKTSV4RRFFQ69G5FAV")], ColumnType::Ulid),
            (vec![json!("2026-09-14T10:00:00Z")], ColumnType::Timestamp),
            (vec![json!("2026-09-14")], ColumnType::Date),
            (vec![json!("free form text")], ColumnType::String),
        ];
        for (items, expected) in cases {
            assert_eq!(
                infer(&items, false, Strictness::Balanced).unwrap().kind,
                expected,
                "{items:?}"
            );
        }
    }

    #[test]
    fn test1039_enum_inference_requires_supporting_evidence() {
        let plenty: Vec<Value> = (0..9)
            .map(|i| json!(if i % 3 == 0 { "a" } else { "b" }))
            .collect();
        let column = infer(&plenty, false, Strictness::Balanced).unwrap();
        assert_eq!(column.kind, ColumnType::Enum);
        assert_eq!(column.subschema["enum"], json!(["a", "b"]));
        assert_eq!(
            infer(&[json!("a"), json!("b")], false, Strictness::Balanced)
                .unwrap()
                .kind,
            ColumnType::String
        );
    }

    #[test]
    fn test1040_mixed_kinds_are_refused_except_under_loose_strictness() {
        let mixed = [json!(1), json!("text")];
        for strictness in [Strictness::Strict, Strictness::Balanced] {
            let error = infer(&mixed, false, strictness).err().unwrap();
            assert_eq!(error.diagnostic.code, "INFER_TYPE_CONFLICT");
            assert_eq!(error.exit, 8);
        }
        assert_eq!(
            infer(&mixed, false, Strictness::Loose).unwrap().kind,
            ColumnType::Json
        );
    }

    #[test]
    fn test1043_nested_shapes_are_described_with_their_required_members() {
        let column = infer(
            &[json!({"a": 1, "b": "x"}), json!({"a": 2})],
            false,
            Strictness::Balanced,
        )
        .unwrap();
        assert_eq!(column.subschema["properties"]["a"]["type"], "integer");
        assert_eq!(
            column.subschema["required"],
            json!(["a"]),
            "b is absent from one row"
        );
        let array = infer(
            &[json!([{"k": "v"}]), json!([])],
            true,
            Strictness::Balanced,
        )
        .unwrap();
        assert_eq!(array.subschema["type"], json!(["array", "null"]));
        assert_eq!(array.subschema["items"]["required"], json!(["k"]));
    }

    /// A row missing a column makes that column optional, not required: the
    /// defect that made every real row of a hand-edited corpus fail.
    #[test]
    fn test2180_inferred_schemas_accept_every_row_they_were_inferred_from() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(
            root,
            "users/u1.json",
            json!({"id": "u1", "name": "A", "tags": ["x"], "meta": {"k": 1}}),
        );
        write(
            root,
            "users/u2.json",
            json!({"id": "u2", "tags": [], "meta": null}),
        );
        write(
            root,
            "posts/p1.json",
            json!({"id": "p1", "user_id": "u1", "reviewers": ["u1", "u2"]}),
        );
        write(
            root,
            "posts/p2.json",
            json!({"id": "p2", "user_id": "u2", "reviewers": []}),
        );
        let schemas = infer_all(
            root,
            &["posts".into(), "users".into()],
            Strictness::Balanced,
            &Config::default(),
            None,
            None,
        )
        .unwrap();
        for (table, schema) in &schemas {
            for entry in fs::read_dir(root.join(table)).unwrap() {
                let value = crate::json::parse(&fs::read(entry.unwrap().path()).unwrap()).unwrap();
                assert!(
                    schema.validator().check(&value).is_empty(),
                    "{table}: {value}"
                );
            }
        }
        let posts = &schemas["posts"];
        let declared: Vec<String> = posts
            .foreign_keys()
            .iter()
            .map(|fk| fk.from()[0].to_string())
            .collect();
        assert_eq!(
            declared,
            vec!["user_id".to_string()],
            "only the conventionally named reference is declared"
        );
    }

    #[test]
    fn test2181_the_key_that_names_every_file_is_chosen() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(root, "things/alpha.json", json!({"slug": "alpha", "n": 1}));
        write(root, "things/beta.json", json!({"slug": "beta", "n": 2}));
        let schemas = infer_all(
            root,
            &["things".into()],
            Strictness::Balanced,
            &Config::default(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(schemas["things"].primary_key(), ["slug".to_string()]);
    }

    #[test]
    fn test2182_no_candidate_key_is_explained_column_by_column() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(root, "things/a.json", json!({"x": 1, "tags": []}));
        write(root, "things/b.json", json!({"x": 1, "tags": []}));
        let error = infer_all(
            root,
            &["things".into()],
            Strictness::Balanced,
            &Config::default(),
            None,
            None,
        )
        .err()
        .unwrap();
        assert_eq!(error.diagnostic.code, "INFER_NO_PRIMARY_KEY");
        assert!(
            error.diagnostic.message.contains("x: 1 is in"),
            "{}",
            error.diagnostic.message
        );
        assert!(
            error
                .diagnostic
                .message
                .contains("tags: array values cannot be a key"),
            "{}",
            error.diagnostic.message
        );
    }
}
