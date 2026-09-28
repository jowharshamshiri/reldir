//! Reading a schema document into its relational view, and editing one.
//!
//! This is the only place a schema document is interpreted and the only place
//! one is written. Reading ([`decode`]) never loses anything, because the view
//! keeps the document it was read from and derives every fact from it. Writing
//! ([`Editor`], [`TableBuilder`]) edits the document and reads the result back
//! through [`decode`], so an edit that would leave an unreadable or
//! self-contradicting schema is refused at the edit, with the same diagnostics a
//! hand-written file would get.

use super::{
    Acyclic, AdditionalFields, Assertion, Check, Column, ColumnType, ForeignKey, GeneratedKind,
    Schema, Severity, Target, identity, meta, path::RefPath, path::Step, row,
};
use crate::diagnostic::Diagnostic;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use std::{collections::BTreeSet, sync::Arc};

type Problems = Vec<Diagnostic>;

/// Read a document into a schema, reporting every fault.
pub(super) fn decode(document: Value, source: Option<&[u8]>) -> Result<Schema, Problems> {
    let spans = source.map(crate::locate::Spans::of);
    let locate = |mut diagnostic: Diagnostic| -> Diagnostic {
        if let (Some(spans), Some(pointer)) = (&spans, &diagnostic.pointer)
            && diagnostic.location.is_none()
        {
            diagnostic.location = spans.location(pointer);
        }
        diagnostic
    };

    let grammar =
        meta::check_document(&document, source).map_err(|error| vec![*error.diagnostic])?;
    if !grammar.is_empty() {
        return Err(grammar);
    }

    // The table schema guarantees the shapes read below; anything it cannot
    // express is checked after the view is built.
    let root = document
        .as_object()
        .expect("the table schema requires an object");
    let extension = root[meta::EXTENSION]
        .as_object()
        .expect("the table schema requires x-reldir to be an object");
    let properties = root["properties"]
        .as_object()
        .expect("the table schema requires properties to be an object");
    let required: BTreeSet<&str> = root
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut problems = vec![];
    let table = extension["table"].as_str().unwrap_or_default().to_string();
    let names = |key: &str| -> Vec<String> { strings(extension.get(key)) };
    let name_lists = |key: &str| -> Vec<Vec<String>> {
        extension
            .get(key)
            .and_then(Value::as_array)
            .map(|lists| lists.iter().map(|list| strings(Some(list))).collect())
            .unwrap_or_default()
    };

    let generated: IndexMap<String, GeneratedKind> = extension
        .get("generated")
        .and_then(Value::as_object)
        .map(|members| {
            members
                .iter()
                .filter_map(|(column, kind)| {
                    kind.as_str()
                        .and_then(GeneratedKind::from_name)
                        .map(|kind| (column.clone(), kind))
                })
                .collect()
        })
        .unwrap_or_default();

    // Column order is logical state -- rows are written in it -- and it is the
    // order `properties` declares its members in.
    let order: Vec<String> = properties.keys().cloned().collect();

    let mut columns = IndexMap::new();
    for name in &order {
        let Some(subschema) = properties.get(name) else {
            continue;
        };
        if name.is_empty() || name.contains('\0') {
            problems.push(
                Diagnostic::error(
                    "SCHEMA_COLUMN_UNKNOWN",
                    format!("invalid column name {name:?}"),
                )
                .pointer(format!("/properties/{}", super::path::escape_pointer(name))),
            );
            continue;
        }
        let mut column = project(subschema, &document, required.contains(name.as_str()), 0);
        column.generated = generated.get(name).copied();
        columns.insert(name.clone(), column);
    }

    let foreign_keys = decode_foreign_keys(extension, &mut problems);
    let checks = extension
        .get("checks")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| Check {
                    name: entry["name"].as_str().unwrap_or_default().to_string(),
                    expr: entry["expr"].as_str().unwrap_or_default().to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let acyclic = extension
        .get("acyclic")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .enumerate()
                .map(|(index, entry)| Acyclic {
                    name: entry["name"].as_str().unwrap_or_default().to_string(),
                    edges: parse_paths(
                        &entry["edges"],
                        &format!("/x-reldir/acyclic/{index}/edges"),
                        &mut problems,
                    ),
                })
                .collect()
        })
        .unwrap_or_default();
    let assertions = extension
        .get("assertions")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| Assertion {
                    name: entry["name"].as_str().unwrap_or_default().to_string(),
                    query: entry["query"].as_str().unwrap_or_default().to_string(),
                    severity: match entry.get("severity").and_then(Value::as_str) {
                        Some("warning") => Severity::Warning,
                        _ => Severity::Error,
                    },
                    message: entry
                        .get("message")
                        .and_then(Value::as_str)
                        .map(String::from),
                })
                .collect()
        })
        .unwrap_or_default();

    let validator = match row::RowValidator::compile(&document, &columns) {
        Ok(validator) => Some(validator),
        Err(problem) => {
            problems.push(*problem);
            None
        }
    };

    if !problems.is_empty() {
        return Err(problems.into_iter().map(locate).collect());
    }

    let schema = Schema {
        identity: identity::identity(&document),
        table,
        schema_version: extension
            .get("schemaVersion")
            .and_then(Value::as_u64)
            .unwrap_or(1),
        description: root
            .get("description")
            .and_then(Value::as_str)
            .map(String::from),
        primary_key: names("primaryKey"),
        columns,
        unique: name_lists("unique"),
        indexes: name_lists("indexes"),
        foreign_keys,
        checks,
        filename: extension.get("filename").map(|value| strings(Some(value))),
        additional_fields: match root.get("additionalProperties") {
            Some(Value::Bool(true)) => AdditionalFields::Allow,
            _ => AdditionalFields::Reject,
        },
        identity_domain: extension
            .get("identityDomain")
            .and_then(Value::as_str)
            .map(String::from),
        acyclic,
        assertions,
        validator: Arc::new(validator.expect("a compiled validator when there are no problems")),
        document: Arc::new(document),
    };
    let local = validate_local(&schema);
    if local.is_empty() {
        Ok(schema)
    } else {
        Err(local.into_iter().map(locate).collect())
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_paths(value: &Value, pointer: &str, problems: &mut Problems) -> Vec<RefPath> {
    let mut out = vec![];
    for (index, text) in value.as_array().into_iter().flatten().enumerate() {
        let text = text.as_str().unwrap_or_default();
        match RefPath::parse(text) {
            Ok(path) => out.push(path),
            Err(error) => problems.push(
                Diagnostic::error(
                    "SCHEMA_REFERENCE_PATH_INVALID",
                    format!("reference path {text:?} does not parse: {error}"),
                )
                .pointer(format!("{pointer}/{index}")),
            ),
        }
    }
    out
}

fn decode_foreign_keys(extension: &Map<String, Value>, problems: &mut Problems) -> Vec<ForeignKey> {
    let mut out = vec![];
    for (index, entry) in extension
        .get("foreignKeys")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let at = format!("/x-reldir/foreignKeys/{index}");
        let from = parse_paths(&entry["from"], &format!("{at}/from"), problems);
        let to = entry["to"]
            .as_object()
            .expect("the dialect requires `to` to be an object");
        let target = if let Some(table) = to.get("table").and_then(Value::as_str) {
            Target::Table(table.to_string())
        } else if let Some(tables) = to.get("tables") {
            Target::Tables(strings(Some(tables)))
        } else {
            Target::Domain(to["domain"].as_str().unwrap_or_default().to_string())
        };
        let action = |key: &str| {
            entry
                .get(key)
                .and_then(Value::as_str)
                .and_then(super::Action::from_name)
                .unwrap_or(super::Action::Restrict)
        };
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| default_foreign_key_name(&from));
        out.push(ForeignKey {
            name,
            from,
            to: target,
            columns: strings(entry.get("columns")),
            on_delete: action("onDelete"),
            on_update: action("onUpdate"),
        });
    }
    out
}

/// The name a foreign key gets when its declaration names none: `fk_` and its
/// paths, reduced to the characters a constraint name may hold.
pub fn default_foreign_key_name(from: &[RefPath]) -> String {
    let mut name = String::from("fk");
    for path in from {
        name.push('_');
        let mut last_underscore = true;
        for ch in path.to_string().chars() {
            let ch = ch.to_ascii_lowercase();
            if ch.is_ascii_alphanumeric() {
                name.push(ch);
                last_underscore = false;
            } else if !last_underscore {
                name.push('_');
                last_underscore = true;
            }
        }
        while name.ends_with('_') {
            name.pop();
        }
    }
    name
}

/// Follow a local `$ref` to the subschema it names, if the value is one.
fn resolve<'a>(value: &'a Value, root: &'a Value, depth: usize) -> Option<&'a Value> {
    let reference = value.get("$ref")?.as_str()?;
    let pointer = reference.strip_prefix('#')?;
    if depth > 32 {
        return None;
    }
    root.pointer(pointer)
}

/// The relational view of one subschema.
///
/// Facts are read from the subschema's own keywords; where it states no type
/// and refers elsewhere with a local `$ref`, the referenced subschema supplies
/// them. A subschema whose type cannot be pinned to one relational type is
/// `json`: a value reldir stores and validates but does not type.
fn project(value: &Value, root: &Value, required: bool, depth: usize) -> Column {
    let own = value.as_object();
    let referenced = resolve(value, root, depth).and_then(Value::as_object);
    let get = |key: &str| -> Option<&Value> {
        own.and_then(|object| object.get(key))
            .or_else(|| referenced.and_then(|object| object.get(key)))
    };
    let (concrete, mut nullable) = match get("type") {
        Some(Value::String(name)) => (vec![name.as_str()], name == "null"),
        Some(Value::Array(names)) => {
            let names: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
            let nullable = names.contains(&"null");
            (
                names.into_iter().filter(|name| *name != "null").collect(),
                nullable,
            )
        }
        _ => (vec![], true),
    };
    let tag = get(meta::TYPE_TAG).and_then(Value::as_str);
    let enumerated = get("enum")
        .and_then(Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .or_else(|| {
            get("const")
                .and_then(Value::as_str)
                .map(|single| vec![single.to_string()])
        });
    let kind = match concrete.as_slice() {
        [single] => match (*single, tag) {
            ("integer", Some("int") | None) => ColumnType::Int,
            ("string", Some("decimal")) => ColumnType::Decimal,
            ("string", Some("ulid")) => ColumnType::Ulid,
            ("string", None) => {
                if enumerated.is_some() {
                    ColumnType::Enum
                } else if get("contentEncoding").and_then(Value::as_str) == Some("base64") {
                    ColumnType::Bytes
                } else {
                    match get("format").and_then(Value::as_str) {
                        Some("date") => ColumnType::Date,
                        Some("date-time") => ColumnType::Timestamp,
                        Some("uuid") => ColumnType::Uuid,
                        _ => ColumnType::String,
                    }
                }
            }
            ("boolean", None) => ColumnType::Bool,
            ("number", None) => ColumnType::Float,
            ("array", None) => ColumnType::Array,
            ("object", None) => ColumnType::Object,
            // A tag that disagrees with the type is refused by the row
            // validator's compilation; here it simply does not type.
            _ => ColumnType::Json,
        },
        _ => ColumnType::Json,
    };
    if kind == ColumnType::Json {
        nullable = true;
    }
    let items = match kind {
        ColumnType::Array => get("items")
            .filter(|items| items.is_object())
            .map(|items| Box::new(project(items, root, true, depth + 1))),
        _ => None,
    };
    let properties = match kind {
        ColumnType::Object => get("properties").and_then(Value::as_object).map(|members| {
            let nested_required: BTreeSet<&str> = get("required")
                .and_then(Value::as_array)
                .map(|names| names.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            members
                .iter()
                .map(|(name, subschema)| {
                    (
                        name.clone(),
                        project(
                            subschema,
                            root,
                            nested_required.contains(name.as_str()),
                            depth + 1,
                        ),
                    )
                })
                .collect()
        }),
        _ => None,
    };
    Column {
        kind: kind.clone(),
        nullable,
        required,
        default: get("default").cloned(),
        generated: None,
        values: if kind == ColumnType::Enum {
            enumerated
        } else {
            None
        },
        items,
        properties,
        description: get("description").and_then(Value::as_str).map(String::from),
    }
}

/// The leaf a reference path reaches in a table's columns.
pub struct PathLeaf<'a> {
    pub column: &'a Column,
    /// Whether the path crosses an array, so a removal removes an element.
    pub iterates: bool,
}

/// Resolve a path against a table's columns, or say why it cannot reach a
/// single typed value.
pub fn resolve_path<'a>(schema: &'a Schema, path: &RefPath) -> Result<PathLeaf<'a>, String> {
    let mut steps = path.steps().iter();
    let Some(Step::Member(first)) = steps.next() else {
        return Err("a path begins with a column".into());
    };
    let mut current = schema
        .columns
        .get(first)
        .ok_or_else(|| format!("column {first:?} does not exist"))?;
    let mut iterates = false;
    for step in steps {
        match step {
            Step::Member(name) => {
                current = match current.kind {
                    ColumnType::Object => current
                        .properties
                        .as_ref()
                        .and_then(|members| members.get(name))
                        .ok_or_else(|| format!("member {name:?} is not declared"))?,
                    ColumnType::Json => {
                        return Err(format!(
                            "member {name:?} lies inside an untyped value; declare its type so the reference has one"
                        ));
                    }
                    ref other => {
                        return Err(format!(
                            "member {name:?} is read from a {} value",
                            other.name()
                        ));
                    }
                };
            }
            Step::Each | Step::Where { .. } => {
                iterates = true;
                let items = match current.kind {
                    ColumnType::Array => current.items.as_deref().ok_or_else(|| {
                        "an array whose elements have no declared type cannot hold typed references"
                            .to_string()
                    })?,
                    ref other => return Err(format!("a {} value has no elements", other.name())),
                };
                if let Step::Where { member, equals } = step {
                    let tested = match items.kind {
                        ColumnType::Object => items
                            .properties
                            .as_ref()
                            .and_then(|members| members.get(member))
                            .ok_or_else(|| format!("filter member {member:?} is not declared"))?,
                        _ => return Err("a filter selects among object elements".into()),
                    };
                    if !literal_fits(equals, tested) {
                        return Err(format!(
                            "filter literal {equals} cannot equal a {} value",
                            tested.kind.name()
                        ));
                    }
                }
                current = items;
            }
        }
    }
    if !current.kind.is_scalar() {
        return Err(format!(
            "a reference must end at a scalar value, not a {} value",
            current.kind.name()
        ));
    }
    Ok(PathLeaf {
        column: current,
        iterates,
    })
}

fn literal_fits(literal: &Value, column: &Column) -> bool {
    match literal {
        Value::Null => column.nullable,
        Value::Bool(_) => column.kind == ColumnType::Bool,
        Value::Number(number) => match column.kind {
            ColumnType::Int => number.is_i64() || number.is_u64(),
            ColumnType::Float => true,
            _ => false,
        },
        Value::String(text) => match column.kind {
            ColumnType::Enum => column
                .values
                .as_ref()
                .is_some_and(|values| values.contains(text)),
            ColumnType::Int | ColumnType::Float | ColumnType::Bool => false,
            _ => true,
        },
        _ => false,
    }
}

/// Rules a single document can break that its grammar cannot state.
fn validate_local(schema: &Schema) -> Problems {
    let mut out = vec![];
    let column_error = |code: &str, message: String, pointer: &str| {
        Diagnostic::error(code, message).pointer(pointer.to_string())
    };
    if !super::valid_name(&schema.table) {
        out.push(column_error(
            "SCHEMA_INVALID_TABLE_NAME",
            format!("{:?} is not a valid table name", schema.table),
            "/x-reldir/table",
        ));
    }
    for (index, key) in schema.primary_key.iter().enumerate() {
        let at = format!("/x-reldir/primaryKey/{index}");
        match schema.columns.get(key) {
            None => out.push(column_error(
                "SCHEMA_PK_COLUMN_UNKNOWN",
                format!("primary key column {key:?} does not exist"),
                &at,
            )),
            Some(column) if column.nullable => out.push(column_error(
                "SCHEMA_PK_NULLABLE",
                format!("primary key column {key:?} admits null, so a row could have no identity"),
                &at,
            )),
            Some(column) if !column.kind.is_scalar() => out.push(column_error(
                "SCHEMA_KEY_NOT_SCALAR",
                format!(
                    "primary key column {key:?} is {}, and a key is made of scalars",
                    column.kind.name()
                ),
                &at,
            )),
            Some(column) if !column.required && column.generated.is_none() => out.push(column_error(
                "SCHEMA_PK_NOT_REQUIRED",
                format!(
                    "primary key column {key:?} is not in `required`, so a row could omit its identity"
                ),
                &at,
            )),
            _ => {}
        }
    }
    let lists = schema
        .unique
        .iter()
        .enumerate()
        .map(|(index, list)| (format!("/x-reldir/unique/{index}"), list))
        .chain(
            schema
                .indexes
                .iter()
                .enumerate()
                .map(|(index, list)| (format!("/x-reldir/indexes/{index}"), list)),
        )
        .chain(
            schema
                .filename
                .iter()
                .map(|list| ("/x-reldir/filename".to_string(), list)),
        );
    for (at, list) in lists {
        for column in list {
            if !schema.columns.contains_key(column) {
                out.push(column_error(
                    "SCHEMA_COLUMN_UNKNOWN",
                    format!("constraint names unknown column {column:?}"),
                    &at,
                ));
            }
        }
    }
    // Only a declared filename can fail this: the default is the primary key,
    // whose own faults are reported against the key.
    if let Some(filename) = &schema.filename
        && (!(filename == &schema.primary_key || schema.unique.contains(filename))
            || filename
                .iter()
                .any(|column| schema.columns.get(column).is_some_and(|c| c.nullable)))
    {
        out.push(column_error(
            "SCHEMA_FILENAME_NOT_UNIQUE",
            "x-reldir.filename must be the primary key or a unique constraint over NOT NULL columns, \
             or two rows could claim one file"
                .into(),
            "/x-reldir/filename",
        ));
    }
    for (name, column) in &schema.columns {
        if let Some(generated) = column.generated {
            let at = format!("/x-reldir/generated/{}", super::path::escape_pointer(name));
            if generated.column_type() != column.kind {
                out.push(column_error(
                    "SCHEMA_DEFAULT_TYPE_MISMATCH",
                    format!(
                        "{name:?} is {} but its generator produces {}",
                        column.kind.name(),
                        generated.column_type().name()
                    ),
                    &at,
                ));
            }
            if column.default.is_some() {
                out.push(column_error(
                    "SCHEMA_DEFAULT_TYPE_MISMATCH",
                    format!(
                        "{name:?} declares both a default and a generator; a value comes from one"
                    ),
                    &at,
                ));
            }
        }
    }
    let mut names = BTreeSet::new();
    let named = schema
        .foreign_keys
        .iter()
        .map(|fk| fk.name.as_str())
        .chain(schema.checks.iter().map(|c| c.name.as_str()))
        .chain(schema.acyclic.iter().map(|a| a.name.as_str()))
        .chain(schema.assertions.iter().map(|a| a.name.as_str()));
    for name in named {
        if !names.insert(name) {
            out.push(column_error(
                "SCHEMA_CONSTRAINT_NAME_DUPLICATE",
                format!("two constraints are named {name:?}; names identify constraints"),
                "/x-reldir",
            ));
        }
    }
    let single_key = schema.primary_key.len() == 1;
    if schema.identity_domain.is_some() && !single_key {
        out.push(column_error(
            "SCHEMA_DOMAIN_KEY_INVALID",
            "an identity domain relates single-column keys".into(),
            "/x-reldir/identityDomain",
        ));
    }
    for (index, fk) in schema.foreign_keys.iter().enumerate() {
        let at = format!("/x-reldir/foreignKeys/{index}");
        let mut leaves = vec![];
        for (position, path) in fk.from.iter().enumerate() {
            match resolve_path(schema, path) {
                Ok(leaf) => leaves.push(leaf),
                Err(reason) => out.push(column_error(
                    "SCHEMA_REFERENCE_PATH_INVALID",
                    format!("reference path {path} cannot be followed: {reason}"),
                    &format!("{at}/from/{position}"),
                )),
            }
        }
        if fk.from.len() > 1 {
            if fk.iterates() {
                out.push(column_error(
                    "SCHEMA_REFERENCE_PATH_INVALID",
                    "a composite key pairs one value from each path; a path through an array \
                     has no single value to pair"
                        .into(),
                    &format!("{at}/from"),
                ));
            }
            if !matches!(fk.to, Target::Table(_)) {
                out.push(column_error(
                    "SCHEMA_FK_TARGET_INVALID",
                    "a composite key references one table's composite key".into(),
                    &format!("{at}/to"),
                ));
            }
        }
        if !fk.columns.is_empty() && !matches!(fk.to, Target::Table(_)) {
            out.push(column_error(
                "SCHEMA_FK_TARGET_INVALID",
                "references into several tables compare with each table's primary key; `columns` \
                 applies to a single-table target"
                    .into(),
                &format!("{at}/columns"),
            ));
        }
        if !fk.columns.is_empty() && fk.columns.len() != fk.from.len() {
            out.push(column_error(
                "SCHEMA_FK_ACTION_INVALID",
                format!(
                    "{} path(s) cannot be compared with {} target column(s)",
                    fk.from.len(),
                    fk.columns.len()
                ),
                &format!("{at}/columns"),
            ));
        }
        for (key, action) in [("onDelete", fk.on_delete), ("onUpdate", fk.on_update)] {
            let illegal = leaves.iter().find_map(|leaf| match action {
                super::Action::SetNull if !leaf.column.nullable => {
                    Some("set_null needs a reference that admits null")
                }
                super::Action::SetDefault if leaf.column.default.is_none() => {
                    Some("set_default needs a reference that declares a default")
                }
                super::Action::Remove if !leaf.iterates && !leaf.column.nullable => Some(
                    "remove takes out the array element holding the reference, or nulls a \
                     reference that crosses no array; this one is neither",
                ),
                _ => None,
            });
            if let Some(reason) = illegal {
                out.push(column_error(
                    "SCHEMA_FK_ACTION_INVALID",
                    reason.into(),
                    &format!("{at}/{key}"),
                ));
            }
        }
    }
    for (index, graph) in schema.acyclic.iter().enumerate() {
        let at = format!("/x-reldir/acyclic/{index}");
        if !single_key {
            out.push(column_error(
                "SCHEMA_ACYCLIC_INVALID",
                "an acyclic graph is drawn between single-column keys".into(),
                &at,
            ));
            continue;
        }
        let key_kind = schema
            .columns
            .get(&schema.primary_key[0])
            .map(|c| c.kind.clone());
        for (position, path) in graph.edges.iter().enumerate() {
            match resolve_path(schema, path) {
                Ok(leaf) if Some(leaf.column.kind.clone()) != key_kind => out.push(column_error(
                    "SCHEMA_ACYCLIC_INVALID",
                    format!(
                        "edge {path} reaches a {} value, not the table's key type",
                        leaf.column.kind.name()
                    ),
                    &format!("{at}/edges/{position}"),
                )),
                Ok(_) => {}
                Err(reason) => out.push(column_error(
                    "SCHEMA_REFERENCE_PATH_INVALID",
                    format!("edge path {path} cannot be followed: {reason}"),
                    &format!("{at}/edges/{position}"),
                )),
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Writing documents.

/// The subschema for a column of a relational type.
///
/// Standard keywords describe the value wherever they can, so a generic
/// validator understands it; `x-reldir-type` appears only where no standard
/// keyword distinguishes the type.
pub fn subschema(kind: &ColumnType, nullable: bool) -> Value {
    let typed = |name: &str| -> Value {
        if nullable {
            json!([name, "null"])
        } else {
            json!(name)
        }
    };
    match kind {
        ColumnType::Bool => json!({ "type": typed("boolean") }),
        ColumnType::Int => json!({ "type": typed("integer"), meta::TYPE_TAG: "int" }),
        ColumnType::Float => json!({ "type": typed("number") }),
        ColumnType::Decimal => json!({
            "type": typed("string"),
            meta::TYPE_TAG: "decimal",
            "pattern": crate::value::DECIMAL_PATTERN
        }),
        ColumnType::String | ColumnType::Enum => json!({ "type": typed("string") }),
        ColumnType::Bytes => json!({ "type": typed("string"), "contentEncoding": "base64" }),
        ColumnType::Date => json!({ "type": typed("string"), "format": "date" }),
        ColumnType::Timestamp => json!({ "type": typed("string"), "format": "date-time" }),
        ColumnType::Uuid => json!({ "type": typed("string"), "format": "uuid" }),
        ColumnType::Ulid => json!({
            "type": typed("string"),
            meta::TYPE_TAG: "ulid",
            "pattern": crate::value::ULID_PATTERN
        }),
        ColumnType::Array => json!({ "type": typed("array") }),
        ColumnType::Object => json!({ "type": typed("object") }),
        ColumnType::Json => json!({}),
    }
}

/// The subschema for an enumeration.
pub fn enum_subschema(values: &[String], nullable: bool) -> Value {
    let mut schema = subschema(&ColumnType::Enum, nullable);
    schema["enum"] = Value::Array(values.iter().cloned().map(Value::String).collect());
    schema
}

/// Builds a table document from nothing, for inference and scaffolding.
#[derive(Debug, Clone)]
pub struct TableBuilder {
    table: String,
    description: Option<String>,
    properties: Map<String, Value>,
    required: Vec<String>,
    primary_key: Vec<String>,
    unique: Vec<Vec<String>>,
    indexes: Vec<Vec<String>>,
    foreign_keys: Vec<Value>,
    checks: Vec<Value>,
    generated: Map<String, Value>,
    identity_domain: Option<String>,
    additional: bool,
}

impl TableBuilder {
    pub fn new(table: &str) -> Self {
        Self {
            table: table.to_string(),
            description: None,
            properties: Map::new(),
            required: vec![],
            primary_key: vec![],
            unique: vec![],
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
            generated: Map::new(),
            identity_domain: None,
            additional: false,
        }
    }
    pub fn column(&mut self, name: &str, subschema: Value, required: bool) -> &mut Self {
        self.properties.insert(name.to_string(), subschema);
        if required {
            self.required.push(name.to_string());
        }
        self
    }
    pub fn primary_key(&mut self, columns: Vec<String>) -> &mut Self {
        self.primary_key = columns;
        self
    }
    pub fn unique(&mut self, columns: Vec<String>) -> &mut Self {
        self.unique.push(columns);
        self
    }
    pub fn index(&mut self, columns: Vec<String>) -> &mut Self {
        self.indexes.push(columns);
        self
    }
    pub fn foreign_key(&mut self, definition: Value) -> &mut Self {
        self.foreign_keys.push(definition);
        self
    }
    pub fn check(&mut self, name: &str, expr: &str) -> &mut Self {
        self.checks.push(json!({ "name": name, "expr": expr }));
        self
    }
    pub fn generated(&mut self, column: &str, kind: GeneratedKind) -> &mut Self {
        self.generated
            .insert(column.to_string(), json!(kind.name()));
        self
    }
    pub fn identity_domain(&mut self, domain: Option<String>) -> &mut Self {
        self.identity_domain = domain;
        self
    }
    pub fn additional_fields(&mut self, allow: bool) -> &mut Self {
        self.additional = allow;
        self
    }
    pub fn description(&mut self, text: Option<String>) -> &mut Self {
        self.description = text;
        self
    }

    /// The document this builder describes.
    pub fn document(&self) -> Value {
        let mut root = Map::new();
        root.insert("$schema".into(), json!(meta::DIALECT_URI));
        root.insert("type".into(), json!("object"));
        if let Some(description) = &self.description {
            root.insert("description".into(), json!(description));
        }
        root.insert("properties".into(), Value::Object(self.properties.clone()));
        if !self.required.is_empty() {
            root.insert("required".into(), json!(self.required));
        }
        root.insert("additionalProperties".into(), json!(self.additional));
        let mut extension = Map::new();
        extension.insert("table".into(), json!(self.table));
        extension.insert("primaryKey".into(), json!(self.primary_key));
        if !self.unique.is_empty() {
            extension.insert("unique".into(), json!(self.unique));
        }
        if !self.indexes.is_empty() {
            extension.insert("indexes".into(), json!(self.indexes));
        }
        if !self.generated.is_empty() {
            extension.insert("generated".into(), Value::Object(self.generated.clone()));
        }
        if let Some(domain) = &self.identity_domain {
            extension.insert("identityDomain".into(), json!(domain));
        }
        if !self.foreign_keys.is_empty() {
            extension.insert(
                "foreignKeys".into(),
                Value::Array(self.foreign_keys.clone()),
            );
        }
        if !self.checks.is_empty() {
            extension.insert("checks".into(), Value::Array(self.checks.clone()));
        }
        root.insert(meta::EXTENSION.into(), Value::Object(extension));
        Value::Object(root)
    }

    pub fn build(&self) -> Result<Schema, Problems> {
        Schema::from_document(self.document(), None)
    }
}

/// An edit of one schema document. Each operation changes the document; the
/// result is read back through [`decode`] by [`Editor::finish`], so every rule
/// a hand-written file must satisfy, an edited one satisfies too.
#[derive(Debug, Clone)]
pub struct Editor {
    document: Value,
}

impl Editor {
    pub(super) fn new(document: Value) -> Self {
        Self { document }
    }

    /// The edited document, read back as a schema.
    pub fn finish(self) -> Result<Schema, Problems> {
        Schema::from_document(self.document, None)
    }

    /// The edited document itself.
    pub fn document(&self) -> &Value {
        &self.document
    }

    fn root(&mut self) -> &mut Map<String, Value> {
        self.document
            .as_object_mut()
            .expect("a schema document is an object")
    }

    fn extension(&mut self) -> &mut Map<String, Value> {
        self.root()
            .entry(meta::EXTENSION)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("x-reldir is an object")
    }

    fn list(&mut self, key: &str) -> &mut Vec<Value> {
        self.extension()
            .entry(key)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("an x-reldir list is an array")
    }

    fn prune(&mut self, key: &str) {
        let empty = self.extension().get(key).is_some_and(|value| {
            value.as_array().is_some_and(Vec::is_empty)
                || value.as_object().is_some_and(Map::is_empty)
        });
        if empty {
            self.extension().remove(key);
        }
    }

    pub fn rename_table(&mut self, table: &str) -> &mut Self {
        self.extension().insert("table".into(), json!(table));
        self
    }

    /// Add a column, appended to the column order.
    pub fn add_column(&mut self, name: &str, subschema: Value, required: bool) -> &mut Self {
        self.root()
            .entry("properties")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("properties is an object")
            .insert(name.to_string(), subschema);
        if required {
            self.set_required(name, true);
        }
        self
    }

    /// Remove a column and every mention of it that only ever described it:
    /// its presence, its order, its generator. Constraints that name it are
    /// left for [`Editor::finish`] to refuse, because dropping a constraint is
    /// a decision, not a consequence.
    pub fn drop_column(&mut self, name: &str) -> &mut Self {
        if let Some(properties) = self
            .root()
            .get_mut("properties")
            .and_then(Value::as_object_mut)
        {
            properties.shift_remove(name);
        }
        self.set_required(name, false);
        if let Some(generated) = self
            .extension()
            .get_mut("generated")
            .and_then(Value::as_object_mut)
        {
            generated.shift_remove(name);
        }
        self.prune("generated");
        self
    }

    /// Rename a column everywhere the document names it.
    pub fn rename_column(
        &mut self,
        old: &str,
        new: &str,
        table: &str,
    ) -> Result<&mut Self, String> {
        let rename = |value: &mut Value| {
            if value.as_str() == Some(old) {
                *value = json!(new);
            }
        };
        if let Some(properties) = self
            .root()
            .get_mut("properties")
            .and_then(Value::as_object_mut)
        {
            let rebuilt: Map<String, Value> = std::mem::take(properties)
                .into_iter()
                .map(|(key, value)| (if key == old { new.to_string() } else { key }, value))
                .collect();
            *properties = rebuilt;
        }
        if let Some(required) = self
            .root()
            .get_mut("required")
            .and_then(Value::as_array_mut)
        {
            required.iter_mut().for_each(rename);
        }
        for key in ["primaryKey", "filename"] {
            if let Some(list) = self.extension().get_mut(key).and_then(Value::as_array_mut) {
                list.iter_mut().for_each(rename);
            }
        }
        for key in ["unique", "indexes"] {
            if let Some(lists) = self.extension().get_mut(key).and_then(Value::as_array_mut) {
                for list in lists.iter_mut().filter_map(Value::as_array_mut) {
                    list.iter_mut().for_each(rename);
                }
            }
        }
        if let Some(generated) = self
            .extension()
            .get_mut("generated")
            .and_then(Value::as_object_mut)
        {
            let rebuilt: Map<String, Value> = std::mem::take(generated)
                .into_iter()
                .map(|(key, value)| (if key == old { new.to_string() } else { key }, value))
                .collect();
            *generated = rebuilt;
        }
        let rename_path = |value: &mut Value| -> Result<(), String> {
            let text = value.as_str().unwrap_or_default();
            let path = RefPath::parse(text).map_err(|error| error.to_string())?;
            if path.column() == old {
                *value = json!(path.with_column(new).to_string());
            }
            Ok(())
        };
        if let Some(keys) = self
            .extension()
            .get_mut("foreignKeys")
            .and_then(Value::as_array_mut)
        {
            for key in keys {
                if let Some(from) = key.get_mut("from").and_then(Value::as_array_mut) {
                    for path in from {
                        rename_path(path)?;
                    }
                }
                let self_reference =
                    key.pointer("/to/table").and_then(Value::as_str) == Some(table);
                if self_reference
                    && let Some(columns) = key.get_mut("columns").and_then(Value::as_array_mut)
                {
                    columns.iter_mut().for_each(rename);
                }
            }
        }
        if let Some(graphs) = self
            .extension()
            .get_mut("acyclic")
            .and_then(Value::as_array_mut)
        {
            for graph in graphs {
                if let Some(edges) = graph.get_mut("edges").and_then(Value::as_array_mut) {
                    for path in edges {
                        rename_path(path)?;
                    }
                }
            }
        }
        if let Some(checks) = self
            .extension()
            .get_mut("checks")
            .and_then(Value::as_array_mut)
        {
            for check in checks {
                let expr = check["expr"].as_str().unwrap_or_default().to_string();
                check["expr"] = json!(crate::sql::rename_identifier(&expr, old, new)?);
            }
        }
        Ok(self)
    }

    /// Replace a column's subschema, keeping its place and presence.
    pub fn set_column(&mut self, name: &str, subschema: Value) -> &mut Self {
        if let Some(properties) = self
            .root()
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            && let Some(slot) = properties.get_mut(name)
        {
            *slot = subschema;
        }
        self
    }

    /// The subschema of a column, if it exists.
    pub fn column(&self, name: &str) -> Option<&Value> {
        self.document.get("properties").and_then(|p| p.get(name))
    }

    pub fn set_required(&mut self, name: &str, required: bool) -> &mut Self {
        let root = self.root();
        let list = root
            .entry("required")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("required is an array");
        list.retain(|column| column != name);
        if required {
            list.push(json!(name));
        }
        if list.is_empty() {
            root.remove("required");
        }
        self
    }

    /// Admit or refuse null in a column, by editing its type union.
    pub fn set_nullable(&mut self, name: &str, nullable: bool) -> &mut Self {
        if let Some(column) = self
            .root()
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .and_then(|properties| properties.get_mut(name))
            .and_then(Value::as_object_mut)
            && let Some(kind) = column.get("type").cloned()
        {
            let mut names: Vec<String> = match kind {
                Value::String(single) => vec![single],
                Value::Array(many) => many
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect(),
                _ => vec![],
            };
            names.retain(|name| name != "null");
            if nullable {
                names.push("null".into());
            }
            column.insert(
                "type".into(),
                if names.len() == 1 {
                    json!(names[0])
                } else {
                    json!(names)
                },
            );
        }
        self
    }

    pub fn set_default(&mut self, name: &str, default: Option<Value>) -> &mut Self {
        if let Some(column) = self
            .root()
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .and_then(|properties| properties.get_mut(name))
            .and_then(Value::as_object_mut)
        {
            match default {
                Some(value) => {
                    column.insert("default".into(), value);
                }
                None => {
                    column.remove("default");
                }
            }
        }
        self
    }

    pub fn set_generated(&mut self, name: &str, kind: Option<GeneratedKind>) -> &mut Self {
        let generated = self
            .extension()
            .entry("generated")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("generated is an object");
        match kind {
            Some(kind) => {
                generated.insert(name.to_string(), json!(kind.name()));
            }
            None => {
                generated.shift_remove(name);
            }
        }
        self.prune("generated");
        self
    }

    pub fn set_primary_key(&mut self, columns: Vec<String>) -> &mut Self {
        self.extension().insert("primaryKey".into(), json!(columns));
        self
    }

    pub fn set_additional_fields(&mut self, allow: bool) -> &mut Self {
        self.root()
            .insert("additionalProperties".into(), json!(allow));
        self
    }

    pub fn set_identity_domain(&mut self, domain: Option<&str>) -> &mut Self {
        match domain {
            Some(domain) => {
                self.extension()
                    .insert("identityDomain".into(), json!(domain));
            }
            None => {
                self.extension().remove("identityDomain");
            }
        }
        self
    }

    /// Add a list to `unique` or `indexes`; returns false when it was present.
    pub fn add_list(&mut self, key: &str, columns: &[String]) -> bool {
        let entry = json!(columns);
        let list = self.list(key);
        if list.contains(&entry) {
            return false;
        }
        list.push(entry);
        true
    }

    /// Remove a list from `unique` or `indexes`; returns false when absent.
    pub fn remove_list(&mut self, key: &str, columns: &[String]) -> bool {
        let entry = json!(columns);
        let list = self.list(key);
        let before = list.len();
        list.retain(|existing| existing != &entry);
        let removed = list.len() != before;
        self.prune(key);
        removed
    }

    pub fn add_foreign_key(&mut self, definition: Value) -> &mut Self {
        self.list("foreignKeys").push(definition);
        self
    }

    pub fn add_check(&mut self, name: &str, expr: &str) -> &mut Self {
        self.list("checks")
            .push(json!({ "name": name, "expr": expr }));
        self
    }

    pub fn add_acyclic(&mut self, name: &str, edges: &[String]) -> &mut Self {
        self.list("acyclic")
            .push(json!({ "name": name, "edges": edges }));
        self
    }

    pub fn add_assertion(&mut self, definition: Value) -> &mut Self {
        self.list("assertions").push(definition);
        self
    }

    /// Remove the named constraint of whatever kind; returns false when no
    /// constraint has that name. A unique constraint is named
    /// `unique_<columns>`, joined with underscores.
    pub fn drop_constraint(&mut self, name: &str) -> bool {
        let mut removed = false;
        for key in ["checks", "acyclic", "assertions"] {
            let list = self.list(key);
            let before = list.len();
            list.retain(|entry| entry["name"].as_str() != Some(name));
            removed |= list.len() != before;
            self.prune(key);
        }
        {
            let list = self.list("foreignKeys");
            let before = list.len();
            list.retain(|entry| {
                let declared = entry.get("name").and_then(Value::as_str).map(String::from);
                let derived = entry["from"].as_array().map(|paths| {
                    default_foreign_key_name(
                        &paths
                            .iter()
                            .filter_map(Value::as_str)
                            .filter_map(|text| RefPath::parse(text).ok())
                            .collect::<Vec<_>>(),
                    )
                });
                declared.or(derived).as_deref() != Some(name)
            });
            removed |= list.len() != before;
            self.prune("foreignKeys");
        }
        {
            let list = self.list("unique");
            let before = list.len();
            list.retain(|entry| {
                let columns: Vec<&str> = entry
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                format!("unique_{}", columns.join("_")) != name
            });
            removed |= list.len() != before;
            self.prune("unique");
        }
        removed
    }

    /// Point every reference to `old` at `new`.
    pub fn rename_referenced_table(&mut self, old: &str, new: &str) -> &mut Self {
        if let Some(keys) = self
            .extension()
            .get_mut("foreignKeys")
            .and_then(Value::as_array_mut)
        {
            for key in keys {
                if let Some(to) = key.get_mut("to").and_then(Value::as_object_mut) {
                    if to.get("table").and_then(Value::as_str) == Some(old) {
                        to.insert("table".into(), json!(new));
                    }
                    if let Some(tables) = to.get_mut("tables").and_then(Value::as_array_mut) {
                        for table in tables {
                            if table.as_str() == Some(old) {
                                *table = json!(new);
                            }
                        }
                    }
                }
            }
        }
        self
    }

    /// Rename a column of `target` wherever a key here names it explicitly.
    pub fn rename_referenced_column(&mut self, target: &str, old: &str, new: &str) -> &mut Self {
        if let Some(keys) = self
            .extension()
            .get_mut("foreignKeys")
            .and_then(Value::as_array_mut)
        {
            for key in keys {
                if key.pointer("/to/table").and_then(Value::as_str) != Some(target) {
                    continue;
                }
                if let Some(columns) = key.get_mut("columns").and_then(Value::as_array_mut) {
                    for column in columns {
                        if column.as_str() == Some(old) {
                            *column = json!(new);
                        }
                    }
                }
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcas_items() -> Value {
        json!({
            "$schema": meta::DIALECT_URI,
            "type": "object",
            "properties": {
                "id": { "type": "string", "pattern": "^[a-z][a-z0-9._-]{2,127}$" },
                "feedback_rules": {
                    "type": ["array", "null"],
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["message"],
                        "properties": {
                            "message": { "type": "string", "minLength": 1 },
                            "when_choice_index": { "type": ["integer", "null"], "minimum": 0, "x-reldir-type": "int" },
                            "when_incorrect": { "type": ["boolean", "null"] },
                            "misconception_ref": { "type": ["string", "null"] }
                        },
                        "oneOf": [
                            { "required": ["when_choice_index"] },
                            { "required": ["when_incorrect"] }
                        ]
                    }
                },
                "relations": {
                    "type": ["array", "null"],
                    "items": {
                        "type": "object",
                        "required": ["target", "relation"],
                        "properties": {
                            "target": { "type": "string" },
                            "relation": { "type": "string", "enum": ["requires", "related_to"] }
                        }
                    }
                }
            },
            "required": ["id"],
            "additionalProperties": false,
            "x-reldir": {
                "table": "items",
                "primaryKey": ["id"],
                "identityDomain": "lcas",
                "foreignKeys": [
                    { "from": ["feedback_rules[].misconception_ref"], "to": { "domain": "lcas" }, "onDelete": "remove" },
                    { "name": "requires", "from": ["relations[?relation='requires'].target"], "to": { "domain": "lcas" } }
                ],
                "acyclic": [{ "name": "prerequisites", "edges": ["relations[?relation='requires'].target"] }]
            }
        })
    }

    fn codes(result: Result<Schema, Problems>) -> Vec<String> {
        match result {
            Ok(_) => vec![],
            Err(problems) => problems.into_iter().map(|d| d.code).collect(),
        }
    }

    #[test]
    fn test2050_a_real_standard_document_decodes_into_its_relational_view() {
        let schema = Schema::from_document(lcas_items(), None).expect("decodes");
        assert_eq!(schema.table(), "items");
        assert_eq!(schema.primary_key(), ["id".to_string()]);
        let rules = schema.column("feedback_rules").unwrap();
        assert_eq!(rules.kind(), &ColumnType::Array);
        assert!(rules.nullable());
        let rule = rules.items().unwrap();
        assert_eq!(
            rule.properties().unwrap()["when_choice_index"].kind(),
            &ColumnType::Int
        );
        assert_eq!(
            schema.foreign_keys()[0].name(),
            "fk_feedback_rules_misconception_ref"
        );
        assert_eq!(schema.foreign_keys()[1].name(), "requires");
        assert_eq!(schema.identity_domain(), Some("lcas"));
        assert_eq!(
            schema.acyclic()[0].edges()[0].to_string(),
            "relations[?relation='requires'].target"
        );
    }

    #[test]
    fn test2051_references_that_cannot_reach_a_typed_scalar_are_refused() {
        let mut through_json = lcas_items();
        through_json["properties"]["blob"] = json!({});
        through_json["x-reldir"]["foreignKeys"][0]["from"] = json!(["blob.inner"]);
        assert_eq!(
            codes(Schema::from_document(through_json, None)),
            vec!["SCHEMA_REFERENCE_PATH_INVALID"]
        );

        let mut to_object = lcas_items();
        to_object["x-reldir"]["foreignKeys"][0]["from"] = json!(["feedback_rules[]"]);
        assert_eq!(
            codes(Schema::from_document(to_object, None)),
            vec!["SCHEMA_REFERENCE_PATH_INVALID"]
        );

        let mut bad_filter = lcas_items();
        bad_filter["x-reldir"]["foreignKeys"][1]["from"] =
            json!(["relations[?relation='nonsense'].target"]);
        assert_eq!(
            codes(Schema::from_document(bad_filter, None)),
            vec!["SCHEMA_REFERENCE_PATH_INVALID"]
        );

        let mut unparsable = lcas_items();
        unparsable["x-reldir"]["foreignKeys"][1]["from"] = json!(["relations[x]"]);
        assert_eq!(
            codes(Schema::from_document(unparsable, None)),
            vec!["SCHEMA_REFERENCE_PATH_INVALID"]
        );
    }

    #[test]
    fn test2052_actions_must_be_executable_for_the_reference_shape() {
        // `remove` on a reference that crosses no array and admits no null has
        // nothing it could take out.
        let mut schema = lcas_items();
        schema["properties"]["owner"] = json!({ "type": "string" });
        schema["required"] = json!(["id", "owner"]);
        schema["x-reldir"]["foreignKeys"] = json!([
            { "from": ["owner"], "to": { "table": "people" }, "onDelete": "remove" }
        ]);
        assert_eq!(
            codes(Schema::from_document(schema.clone(), None)),
            vec!["SCHEMA_FK_ACTION_INVALID"]
        );
        schema["x-reldir"]["foreignKeys"][0]["onDelete"] = json!("set_null");
        assert_eq!(
            codes(Schema::from_document(schema.clone(), None)),
            vec!["SCHEMA_FK_ACTION_INVALID"]
        );
        schema["x-reldir"]["foreignKeys"][0]["onDelete"] = json!("cascade");
        assert!(codes(Schema::from_document(schema, None)).is_empty());
    }

    #[test]
    fn test2053_primary_keys_must_identify_every_row() {
        let mut nullable = lcas_items();
        nullable["properties"]["id"]["type"] = json!(["string", "null"]);
        assert_eq!(
            codes(Schema::from_document(nullable, None)),
            vec!["SCHEMA_PK_NULLABLE"]
        );

        let mut optional = lcas_items();
        optional["required"] = json!([]);
        assert_eq!(
            codes(Schema::from_document(optional, None)),
            vec!["SCHEMA_PK_NOT_REQUIRED"]
        );

        let mut missing = lcas_items();
        missing["x-reldir"]["primaryKey"] = json!(["ghost"]);
        assert!(
            codes(Schema::from_document(missing, None))
                .contains(&"SCHEMA_PK_COLUMN_UNKNOWN".to_string())
        );
    }

    #[test]
    fn test2054_column_order_is_the_order_properties_are_declared_in() {
        let schema = Schema::from_document(lcas_items(), None).unwrap();
        let order: Vec<&String> = schema.columns().keys().collect();
        assert_eq!(order, ["id", "feedback_rules", "relations"]);
        let mut stray = lcas_items();
        stray["x-reldir"]["columnOrder"] = json!(["id"]);
        assert!(
            Schema::from_document(stray, None).is_err(),
            "x-reldir has no columnOrder member"
        );
    }

    #[test]
    fn test2055_the_editor_changes_the_document_and_reads_it_back() {
        let schema = Schema::from_document(lcas_items(), None).unwrap();
        let mut editor = schema.edit();
        editor.add_column("rank", subschema(&ColumnType::Int, true), false);
        editor.rename_column("relations", "edges", "items").unwrap();
        let edited = editor.finish().expect("the edit is a valid schema");
        assert_eq!(edited.column("rank").unwrap().kind(), &ColumnType::Int);
        assert!(edited.column("relations").is_none());
        assert_eq!(
            edited.foreign_keys()[1].from()[0].to_string(),
            "edges[?relation='requires'].target",
            "a renamed column keeps every reference that followed it"
        );
        assert_eq!(edited.acyclic()[0].edges()[0].column(), "edges");
        assert_ne!(edited.identity(), schema.identity());

        // An edit that would orphan a constraint is refused at the edit.
        let mut editor = schema.edit();
        editor.drop_column("relations");
        assert!(codes(editor.finish()).contains(&"SCHEMA_REFERENCE_PATH_INVALID".to_string()));
    }

    #[test]
    fn test2056_the_view_never_disagrees_with_its_document() {
        // Reading a document, taking its document back, and reading that again
        // is the identity -- there is no encoder in between to lose anything.
        let schema = Schema::from_document(lcas_items(), None).unwrap();
        let again = Schema::from_document(schema.document().clone(), None).unwrap();
        assert_eq!(schema.identity(), again.identity());
        assert_eq!(schema.document(), &lcas_items());
    }

    #[test]
    fn test2057_the_builder_writes_a_document_the_reader_accepts() {
        let mut builder = TableBuilder::new("people");
        builder
            .column("id", subschema(&ColumnType::Uuid, false), true)
            .column(
                "tags",
                {
                    let mut tags = subschema(&ColumnType::Array, false);
                    tags["items"] = subschema(&ColumnType::String, false);
                    tags
                },
                true,
            )
            .primary_key(vec!["id".into()])
            .generated("id", GeneratedKind::Uuid);
        let schema = builder.build().expect("builds");
        assert_eq!(
            schema.column("id").unwrap().generated(),
            Some(GeneratedKind::Uuid)
        );
        assert_eq!(
            schema.column("tags").unwrap().items().unwrap().kind(),
            &ColumnType::String
        );
    }
}
