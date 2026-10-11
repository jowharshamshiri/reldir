//! Inferring a table's contract from its rows.
//!
//! A folder of JSON that programs already rely on has a shape those programs
//! depend on, written down nowhere. Inference writes it down -- and only it.
//! The *contract* states what is structurally true of every row: which members
//! a row always has, the type of every value at every depth, whether a number
//! is written as an integer or with a fraction, whether a value is ever null,
//! which strings are always timestamps or uuids, how files are named. Anything
//! never observed is refused, so nothing reldir writes can hand those programs
//! a shape they have not seen ([`crate::shape`]).
//!
//! What merely *happened* in the data -- a column holding few distinct values,
//! values that happen to be unique, numbers that happen to be positive -- is
//! not part of the contract: a new row is free to differ. Such statistics are
//! lint's proposals, and become constraints only when asked for (`--tighten`).
//!
//! References are found by [`crate::analysis::references`], the same analysis
//! lint and doctor use. Inference declares only the conventional ones -- those
//! a `reference_naming` pattern names -- always refusing a delete or a key
//! change that would orphan them, because a declaration is a rule every future
//! write must satisfy, and only a name is evidence of intent.

use crate::code::Code;
use crate::{
    analysis::references::{self, ProposedTarget, TableFacts},
    canonical,
    catalog::Catalog,
    config::Config,
    diagnostic::{DbError, Diagnostic, Result},
    naming::{Identity, NamingRule},
    schema::{ColumnType, Schema},
    shape::TableShape,
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
};

/// Statistical facts to state as constraints after all. Each is a decision
/// the person adopting the folder makes: none is evidence of a rule.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tighten {
    /// A string column with few distinct values becomes an `enum` of them.
    pub enums: bool,
    /// A column whose every value differs becomes `unique`.
    pub unique: bool,
    /// An int that is never negative, a string that is never empty, gain checks.
    pub checks: bool,
    /// An array that is never empty gains `minItems: 1`.
    pub min_items: bool,
}

impl Tighten {
    /// Read `--tighten`'s comma-separated list.
    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        let mut out = Self::default();
        for item in text
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            match item {
                "enums" => out.enums = true,
                "unique" => out.unique = true,
                "checks" => out.checks = true,
                "min-items" => out.min_items = true,
                "all" => {
                    out = Self {
                        enums: true,
                        unique: true,
                        checks: true,
                        min_items: true,
                    }
                }
                other => {
                    return Err(format!(
                        "{other:?} is not something --tighten states; choose from enums, unique, \
                         checks, min-items, all"
                    ));
                }
            }
        }
        Ok(out)
    }

    pub fn any(&self) -> bool {
        self.enums || self.unique || self.checks || self.min_items
    }
}

/// How inference runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options<'a> {
    pub tighten: Tighten,
    /// Key columns the person chose; inferred from the file names otherwise.
    pub key: Option<&'a [String]>,
}

#[derive(Clone)]
struct Sample {
    path: PathBuf,
    raw: Vec<u8>,
    stem: String,
    obj: Map<String, Value>,
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

/// Infer the contracts of `tables`. `governed`, when given, is the database the
/// tables join: its tables are reference targets, and its schemas are not
/// re-inferred.
pub fn infer_all(
    root: &Path,
    tables: &[String],
    options: Options<'_>,
    config: &Config,
    governed: Option<&Catalog>,
) -> Result<BTreeMap<String, Schema>> {
    let mut documents: BTreeMap<String, Value> = BTreeMap::new();
    let mut samples: BTreeMap<String, Vec<Sample>> = BTreeMap::new();
    for table in tables {
        if !crate::schema::valid_name(table) {
            return Err(DbError::from_diag(
                Diagnostic::error(
                    Code::SCHEMA_INVALID_TABLE_NAME,
                    format!(
                        "{table:?} cannot be a table name: names are lowercase letters, digits and \
                         underscores, starting with a letter"
                    ),
                )
                .at(table.as_str()),
            ));
        }
        let rows = load_samples(root, table, config)?;
        documents.insert(table.clone(), contract(table, &rows, options, config)?);
        samples.insert(table.clone(), rows);
    }

    // References, found over the new tables and the database they join.
    let mut facts = BTreeMap::new();
    for (table, document) in &documents {
        let schema = decode(table, document)?;
        let key = single_key(&schema);
        let views: Vec<Map<String, Value>> = samples[table]
            .iter()
            .map(|sample| view(&schema, sample))
            .collect();
        facts.insert(
            table.clone(),
            TableFacts::gather(table, key, None, views.iter()),
        );
    }
    if let Some(catalog) = governed {
        for (table, schema) in &catalog.schemas {
            if documents.contains_key(table) {
                continue;
            }
            let rows = catalog.rows(table)?;
            facts.insert(
                table.clone(),
                TableFacts::gather(
                    table,
                    single_key(schema),
                    Some(schema),
                    rows.iter().map(|r| &r.value),
                ),
            );
        }
    }
    for proposal in references::detect(&facts, config) {
        if !proposal.conventional || !documents.contains_key(&proposal.table) {
            continue;
        }
        if let ProposedTarget::Domain { name, join, .. } = &proposal.target {
            // A table the database already governs is joined to a domain by
            // editing its schema, which inference does not do on its own.
            if join.iter().any(|table| !documents.contains_key(table)) {
                continue;
            }
            for table in join {
                documents.get_mut(table).expect("checked above")["x-reldir"]["identityDomain"] =
                    json!(name);
            }
        }
        let extension = &mut documents.get_mut(&proposal.table).expect("checked above")["x-reldir"];
        match extension
            .get_mut("foreignKeys")
            .and_then(Value::as_array_mut)
        {
            Some(list) => list.push(proposal.definition()),
            None => extension["foreignKeys"] = json!([proposal.definition()]),
        }
    }

    let mut out = BTreeMap::new();
    for (table, document) in documents {
        let schema = decode(&table, &document)?;
        // A contract must admit every row it was read from: one that does not
        // is a defect in inference, never something to write down.
        for sample in &samples[&table] {
            let found = schema.validator().check(&Value::Object(sample.obj.clone()));
            if let Some(fault) = found.first() {
                return Err(DbError::new(
                    Code::INTERNAL_ERROR,
                    format!(
                        "the contract inferred for {table} refuses {}, which it was inferred from: {}",
                        sample.path.display(),
                        fault.message
                    ),
                ));
            }
        }
        out.insert(table, schema);
    }
    Ok(out)
}

/// Read a document inference produced. A fault is a defect in inference.
fn decode(table: &str, document: &Value) -> Result<Schema> {
    Schema::from_document(document.clone(), None).map_err(|problems| {
        let first = problems.into_iter().next().map_or_else(
            || "it does not load".to_string(),
            |diagnostic| diagnostic.message,
        );
        DbError::new(
            Code::INTERNAL_ERROR,
            format!("the contract inferred for {table} is not a valid schema: {first}"),
        )
    })
}

/// The table's single key column and its type, which references are judged
/// against; `None` for a composite key.
fn single_key(schema: &Schema) -> Option<(String, ColumnType)> {
    match schema.primary_key() {
        [key] => schema
            .column(key)
            .map(|column| (key.clone(), column.kind().clone())),
        _ => None,
    }
}

/// A sample as the database will see it: its body, and -- for a table keyed
/// by file name -- the name as its key column.
fn view(schema: &Schema, sample: &Sample) -> Map<String, Value> {
    let mut row = sample.obj.clone();
    if let (Some(column), Some(value)) = (
        schema.stem_column(),
        schema.row_identity().stem_value(&sample.stem),
    ) {
        row.insert(column.to_string(), value);
    }
    row
}

/// The contract of one table, as a schema document.
fn contract(table: &str, rows: &[Sample], options: Options<'_>, config: &Config) -> Result<Value> {
    let paths: Vec<String> = rows
        .iter()
        .map(|sample| canonical_path(&sample.path))
        .collect();
    let shape = TableShape::from_rows(
        rows.iter()
            .zip(&paths)
            .map(|(sample, path)| (path.as_str(), &sample.obj)),
    )
    .map_err(|error| DbError::new(Code::INTERNAL_ERROR, error.to_string()))?;
    let row = shape.to_subschema();
    let columns: Vec<String> = shape.columns().into_iter().map(String::from).collect();
    let stems: Vec<(String, &Map<String, Value>)> = rows
        .iter()
        .map(|sample| (sample.stem.clone(), &sample.obj))
        .collect();
    let identity = match options.key {
        Some(key) => requested_identity(table, rows, key)?,
        None => crate::naming::infer_identity(table, &stems, &columns).identity,
    };
    let mut extension = Map::new();
    extension.insert("table".into(), json!(table));
    extension.insert(
        "identity".into(),
        serde_json::to_value(&identity).expect("an identity serializes"),
    );
    if let Some(style) = majority_style(rows) {
        extension.insert(
            "style".into(),
            serde_json::to_value(&style).expect("a style serializes"),
        );
    }
    let mut document = json!({
        "$schema": crate::schema::meta::DIALECT_URI,
        "type": "object",
        "properties": row.get("properties").cloned().unwrap_or_else(|| json!({})),
        "required": row.get("required").cloned().unwrap_or_else(|| json!([])),
        "additionalProperties": false,
        "x-reldir": Value::Object(extension),
    });
    if options.tighten.any() {
        tighten(&mut document, rows, options.tighten, config, &identity);
    }
    Ok(document)
}

fn canonical_path(path: &Path) -> String {
    crate::catalog::slash(path)
}

/// The key the person chose, with the naming rule the files already follow.
fn requested_identity(table: &str, rows: &[Sample], key: &[String]) -> Result<Identity> {
    let mut seen = BTreeSet::new();
    for name in key {
        if !seen.insert(name) {
            return Err(DbError::usage(format!(
                "--pk repeats column {name:?} for table {table}"
            )));
        }
        if let Some(row) = rows
            .iter()
            .find(|row| row.obj.get(name).is_none_or(Value::is_null))
        {
            return Err(DbError::new(
                Code::INFER_KEY_INVALID,
                format!(
                    "--pk column {name:?} is null or absent in {}",
                    row.path.display()
                ),
            ));
        }
    }
    let stems: Vec<(String, &Map<String, Value>)> = rows
        .iter()
        .map(|sample| (sample.stem.clone(), &sample.obj))
        .collect();
    if let [column] = key {
        let inferred = crate::naming::infer_identity(table, &stems, std::slice::from_ref(column));
        if let Identity::Body { columns, .. } = &inferred.identity
            && columns == key
        {
            return Ok(inferred.identity);
        }
    }
    // A composite key, or a single one no verbatim or template rule spells:
    // reldir's own encoding, when it is what the files already use.
    let candidate = Identity::Body {
        columns: key.to_vec(),
        rule: NamingRule::Reldir,
    };
    for row in rows {
        match candidate.stem(&row.obj) {
            Ok(stem) if stem == row.stem => {}
            Ok(stem) => {
                return Err(DbError::from_diag(
                    Diagnostic::error(
                        Code::INFER_FILENAME_INCONSISTENT,
                        format!(
                            "{} is not named by --pk {}: its key would name it {stem}.json",
                            row.path.display(),
                            key.join(",")
                        ),
                    )
                    .at(row.path.clone())
                    .help(
                        "adopting a folder never renames its files; omit --pk to key the table by \
                         its file names",
                    ),
                ));
            }
            Err(error) => {
                return Err(DbError::new(
                    Code::INFER_FILENAME_INCONSISTENT,
                    format!("{}: {error}", row.path.display()),
                ));
            }
        }
    }
    Ok(candidate)
}

/// The style most of a table's files are written in, recorded so that rows
/// reldir creates look like their neighbours.
fn majority_style(rows: &[Sample]) -> Option<crate::json_edit::Style> {
    let mut counts: Vec<(crate::json_edit::Style, usize)> = vec![];
    for row in rows {
        let style = crate::json_edit::Style::detect(&row.raw);
        match counts.iter_mut().find(|(seen, _)| *seen == style) {
            Some((_, count)) => *count += 1,
            None => counts.push((style, 1)),
        }
    }
    // The first style seen wins a tie: rows are in path order, so the answer
    // does not depend on the order files were listed in.
    let mut best: Option<(crate::json_edit::Style, usize)> = None;
    for (style, count) in counts {
        if best.as_ref().is_none_or(|(_, most)| count > *most) {
            best = Some((style, count));
        }
    }
    best.map(|(style, _)| style)
}

/// State the statistical facts the person asked for.
fn tighten(
    document: &mut Value,
    rows: &[Sample],
    tighten: Tighten,
    config: &Config,
    identity: &Identity,
) {
    let key = identity.columns();
    let names: Vec<String> = document["properties"]
        .as_object()
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default();
    let required: BTreeSet<String> = document["required"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let mut unique = vec![];
    let mut checks = vec![];
    for name in &names {
        let present: Vec<&Value> = rows.iter().filter_map(|row| row.obj.get(name)).collect();
        let values: Vec<&Value> = present.iter().copied().filter(|v| !v.is_null()).collect();
        let subschema = &mut document["properties"][name];
        let types: Vec<String> = match &subschema["type"] {
            Value::String(name) => vec![name.clone()],
            Value::Array(list) => list
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect(),
            _ => vec![],
        };
        let is = |wanted: &str| types.iter().any(|name| name == wanted);
        let plain_string = is("string")
            && subschema.get("format").is_none()
            && subschema.get(crate::schema::meta::TYPE_TAG).is_none();
        if tighten.enums && plain_string && !values.is_empty() {
            let distinct: BTreeSet<&str> = values.iter().filter_map(|v| v.as_str()).collect();
            if distinct.len() <= config.enum_max_values && values.len() >= distinct.len() * 3 {
                let mut members: Vec<Value> = distinct.iter().map(|v| json!(v)).collect();
                if is("null") {
                    members.push(Value::Null);
                }
                subschema["enum"] = Value::Array(members);
            }
        }
        if tighten.min_items
            && is("array")
            && !values.is_empty()
            && values
                .iter()
                .all(|v| v.as_array().is_some_and(|items| !items.is_empty()))
        {
            subschema["minItems"] = json!(1);
        }
        let scalar_key_type = is("string") || is("integer");
        if tighten.unique
            && scalar_key_type
            && !key.contains(name)
            && required.contains(name)
            && !is("null")
            && rows.len() >= config.unique_min_rows
        {
            let distinct: HashSet<String> = values.iter().map(|v| canonical::compact(v)).collect();
            if distinct.len() == rows.len() {
                unique.push(json!([name]));
            }
        }
        if tighten.checks && rows.len() >= config.unique_min_rows && !values.is_empty() {
            let quoted = crate::mirror::quote(name);
            if subschema.get(crate::schema::meta::TYPE_TAG) == Some(&json!("int"))
                && values.iter().all(|v| v.as_i64().is_some_and(|n| n >= 0))
            {
                checks.push(json!({"name": format!("{name}_nonnegative"), "expr": format!("{quoted} >= 0")}));
            } else if plain_string
                && values
                    .iter()
                    .all(|v| v.as_str().is_some_and(|s| !s.is_empty()))
            {
                checks.push(
                    json!({"name": format!("{name}_nonempty"), "expr": format!("{quoted} <> ''")}),
                );
            }
        }
    }
    if !unique.is_empty() {
        document["x-reldir"]["unique"] = Value::Array(unique);
    }
    if !checks.is_empty() {
        document["x-reldir"]["checks"] = Value::Array(checks);
    }
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
                    Code::INFER_NO_ROWS,
                    format!("table directory {table:?} does not exist"),
                )
                .at(table),
            ));
        }
        Err(error) => return Err(DbError::io(&dir, error)),
    };
    if !metadata.file_type().is_dir() {
        return Err(DbError::from_diag(
            Diagnostic::error(
                Code::INFER_NON_REGULAR_FILE,
                "a table path must be a real directory, not a symlink",
            )
            .at(table),
        ));
    }
    let mut paths = vec![];
    for entry in fs::read_dir(&dir).map_err(|e| DbError::io(&dir, e))? {
        paths.push(entry.map_err(|e| DbError::io(&dir, e))?.path());
    }
    paths.sort();
    let mut progress = crate::progress::Progress::new("inferring", paths.len());
    let ignores = config
        .ignore_set()
        .map_err(|error| DbError::new(Code::CONFIG_INVALID, error))?;
    let mut rows = vec![];
    for path in paths {
        progress.advance();
        let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if ignores.is_match(&relative) || ignores.is_match(name) {
            continue;
        }
        let md = fs::symlink_metadata(&path).map_err(|e| DbError::io(&path, e))?;
        let refuse = |code: Code, message: &str| {
            DbError::from_diag(Diagnostic::error(code, message).at(relative.clone()))
        };
        // What a table directory may hold is rows alone; anything else is
        // moved out, or left ungoverned by an ignore pattern from the start.
        let not_a_row = |code: Code, message: &str| {
            refuse(code, message).with_help(format!(
                "move it out of the table directory, or leave it ungoverned with an ignore pattern: \
                 `reldir init --adopt --ignore {}` for a folder not yet adopted, `ignore` in \
                 .db/config for a database",
                relative.display()
            ))
        };
        if md.is_dir() {
            return Err(not_a_row(
                Code::INFER_NESTED_DIRECTORY,
                "this directory is inside a table directory, which holds only row files",
            ));
        }
        if !md.is_file() || crate::catalog::has_multiple_links(&md) {
            return Err(not_a_row(
                Code::INFER_NON_REGULAR_FILE,
                "this is a symlink, a special file or a hard-linked file, and a row is a private \
                 regular file",
            ));
        }
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            return Err(not_a_row(
                Code::INFER_NON_JSON_FILE,
                "this file is not .json, and a table directory holds only rows",
            ));
        }
        if md.len() > config.max_json_file_size {
            return Err(refuse(
                Code::FILE_TOO_LARGE,
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
                    Code::NESTING_TOO_DEEP,
                    &format!(
                        "JSON nesting exceeds the depth limit {}",
                        config.max_nesting_depth
                    ),
                );
            }
            let mut diagnostic =
                Diagnostic::error(Code::INFER_INVALID_JSON, error.to_string()).at(relative.clone());
            diagnostic.location = Some(crate::diagnostic::Location {
                line: error.line(),
                column: error.column(),
            });
            diagnostic.source_line = String::from_utf8_lossy(&bytes)
                .lines()
                .nth(error.line().saturating_sub(1))
                .map(String::from);
            DbError::from_diag(diagnostic)
        })?;
        let Value::Object(obj) = value else {
            return Err(refuse(
                Code::INFER_ROOT_NOT_OBJECT,
                "a row file holds one JSON object",
            ));
        };
        rows.push(Sample {
            path: relative,
            raw: bytes.clone(),
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
                Code::INFER_NO_ROWS,
                format!("table {table:?} holds no .json rows"),
            )
            .at(table)
            .help(format!("declare it with `reldir schema new {table}`")),
        ));
    }
    Ok(rows)
}
