//! Reading a schema document into its relational view, and editing one.
//!
//! This is the only place a schema document is interpreted and the only place
//! one is written. Reading (`decode`) never loses anything, because the view
//! keeps the document it was read from and derives every fact from it. Writing
//! ([`Editor`], [`TableBuilder`]) edits the document and reads the result back
//! through `decode`, so an edit that would leave an unreadable or
//! self-contradicting schema is refused at the edit, with the same diagnostics a
//! hand-written file would get.

use super::{
    Acyclic, AdditionalFields, Assertion, Check, Column, ColumnType, ForeignKey, GeneratedKind,
    Schema, Severity, Target, identity, meta, path::RefPath, path::Step, row,
};
use crate::code::Code;
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
    let _names = |key: &str| -> Vec<String> { strings(extension.get(key)) };
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

    let mut columns: IndexMap<String, Column> = IndexMap::new();
    for name in &order {
        let Some(subschema) = properties.get(name) else {
            continue;
        };
        if name.is_empty() || name.contains('\0') {
            problems.push(
                Diagnostic::error(
                    Code::SCHEMA_COLUMN_UNKNOWN,
                    format!("invalid column name {name:?}"),
                )
                .pointer(format!("/properties/{}", super::path::escape_pointer(name))),
            );
            continue;
        }
        let mut column = project(
            subschema,
            &document,
            required.contains(name.as_str()),
            &mut Vec::new(),
        );
        column.generated = generated.get(name).copied();
        columns.insert(name.clone(), column);
    }

    let row_identity: Option<crate::naming::Identity> =
        match serde_json::from_value(extension["identity"].clone()) {
            Ok(identity) => Some(identity),
            Err(error) => {
                problems.push(
                    Diagnostic::error(
                        Code::SCHEMA_IDENTITY_INVALID,
                        format!("x-reldir.identity is not an identity: {error}"),
                    )
                    .pointer("/x-reldir/identity"),
                );
                None
            }
        };
    if let Some(crate::naming::Identity::FileStem { column, kind }) = &row_identity {
        if columns.contains_key(column) {
            problems.push(
                Diagnostic::error(
                    Code::SCHEMA_IDENTITY_INVALID,
                    format!(
                        "the file name is exposed as {column:?}, which is also a member of the body; \
                         one name cannot be both"
                    ),
                )
                .pointer("/x-reldir/identity/column"),
            );
        } else {
            let stem = Column {
                kind: match kind {
                    crate::naming::StemKind::String => ColumnType::String,
                    crate::naming::StemKind::Int => ColumnType::Int,
                },
                nullable: false,
                required: true,
                default: None,
                generated: None,
                values: None,
                items: None,
                properties: None,
                description: Some("the row's file name, without .json".into()),
                stem: true,
                positional: false,
            };
            let mut ordered = IndexMap::with_capacity(columns.len() + 1);
            ordered.insert(column.clone(), stem);
            ordered.extend(columns);
            columns = ordered;
        }
    }
    let style = match extension.get("style") {
        None => None,
        Some(value) => match serde_json::from_value::<crate::json_edit::Style>(value.clone()) {
            Ok(style) => Some(style),
            Err(error) => {
                problems.push(
                    Diagnostic::error(
                        Code::SCHEMA_GRAMMAR,
                        format!("x-reldir.style is not a style: {error}"),
                    )
                    .pointer("/x-reldir/style"),
                );
                None
            }
        },
    };

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
    let row_identity = row_identity.expect("an identity when there are no problems");

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
        primary_key: row_identity.columns(),
        row_identity,
        columns,
        unique: name_lists("unique"),
        indexes: name_lists("indexes"),
        foreign_keys,
        checks,
        style,
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
                    Code::SCHEMA_REFERENCE_PATH_INVALID,
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

/// The layers of a subschema: its own keywords, then those of the subschema
/// its local `$ref` names, then that one's, to the end of the chain. A keyword
/// is read from the first layer stating it.
///
/// `expanding` holds the references being expanded by the subschemas that
/// enclose this one. A reference that would re-enter one of them -- a tree
/// whose nodes contain nodes -- ends the chain and is reported, so the caller
/// projects it once, as an opaque value, instead of unfolding it without end.
fn layers<'a>(
    value: &'a Value,
    root: &'a Value,
    expanding: &[String],
) -> (Vec<&'a Map<String, Value>>, Vec<String>, bool) {
    let mut out = vec![];
    let mut followed: Vec<String> = vec![];
    let mut current = value;
    while let Some(object) = current.as_object() {
        out.push(object);
        let Some(reference) = object.get("$ref").and_then(Value::as_str) else {
            break;
        };
        if expanding
            .iter()
            .chain(&followed)
            .any(|seen| seen == reference)
        {
            return (out, followed, true);
        }
        let Some(target) = reference
            .strip_prefix('#')
            .and_then(|pointer| root.pointer(pointer))
        else {
            break;
        };
        followed.push(reference.to_string());
        current = target;
    }
    (out, followed, false)
}

/// The relational view of one subschema.
///
/// Facts are read from the subschema's own keywords and, where it states none,
/// from the subschemas its local `$ref`s name. A subschema whose type cannot be
/// pinned to one relational type -- or whose shape refers back into itself --
/// is `json`: a value reldir stores and the validator judges in full, but that
/// SQL sees as one JSON value rather than typed parts.
fn project(value: &Value, root: &Value, required: bool, expanding: &mut Vec<String>) -> Column {
    let (layers, followed, recursive) = layers(value, root, expanding);
    let opaque = || Column {
        kind: ColumnType::Json,
        nullable: true,
        required,
        default: None,
        generated: None,
        values: None,
        items: None,
        properties: None,
        description: None,
        stem: false,
        positional: false,
    };
    if recursive {
        return opaque();
    }
    let get = |key: &str| -> Option<&Value> { layers.iter().find_map(|layer| layer.get(key)) };
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
            ("integer", Some("int")) => ColumnType::Int,
            ("integer", None) => ColumnType::Integer,
            ("number", Some("float")) => ColumnType::Float,
            ("number", None) => ColumnType::Number,
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
            ("array", None) => ColumnType::Array,
            ("object", None) => ColumnType::Object,
            // A tag that contradicts its type does not compile: the
            // validator refuses it with SCHEMA_TYPE_TAG_CONFLICT, so no
            // schema that reaches use is typed by this arm.
            _ => ColumnType::Json,
        },
        _ => ColumnType::Json,
    };
    if kind == ColumnType::Json {
        nullable = true;
    }
    let depth = expanding.len();
    expanding.extend(followed);
    let items = match kind {
        ColumnType::Array => get("items")
            .filter(|items| items.is_object())
            .map(|items| Box::new(project(items, root, true, expanding))),
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
                            expanding,
                        ),
                    )
                })
                .collect()
        }),
        _ => None,
    };
    expanding.truncate(depth);
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
        stem: false,
        positional: kind == ColumnType::Array && get("prefixItems").is_some(),
    }
}

/// The leaf a reference path reaches in a table's columns.
pub struct PathLeaf<'a> {
    pub column: &'a Column,
    /// Whether the path crosses an array, so a removal removes an element.
    pub iterates: bool,
    /// Whether an array the path crosses types its elements by position.
    pub positional: bool,
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
    let mut positional = false;
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
                positional |= current.positional;
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
        positional,
    })
}

fn literal_fits(literal: &Value, column: &Column) -> bool {
    match literal {
        Value::Null => column.nullable,
        Value::Bool(_) => column.kind == ColumnType::Bool,
        Value::Number(number) => match column.kind {
            ColumnType::Int => number.is_i64(),
            ColumnType::Integer => !number.to_string().contains(['.', 'e', 'E']),
            ColumnType::Float => number.to_string().contains(['.', 'e', 'E']),
            ColumnType::Number => true,
            _ => false,
        },
        Value::String(text) => match column.kind {
            ColumnType::Enum => column
                .values
                .as_ref()
                .is_some_and(|values| values.contains(text)),
            ColumnType::Int
            | ColumnType::Integer
            | ColumnType::Float
            | ColumnType::Number
            | ColumnType::Bool => false,
            _ => true,
        },
        _ => false,
    }
}

/// Rules a single document can break that its grammar cannot state.
fn validate_local(schema: &Schema) -> Problems {
    let mut out = vec![];
    let column_error = |code: Code, message: String, pointer: &str| {
        Diagnostic::error(code, message).pointer(pointer.to_string())
    };
    if !super::valid_name(&schema.table) {
        out.push(column_error(
            Code::SCHEMA_INVALID_TABLE_NAME,
            format!("{:?} is not a valid table name", schema.table),
            "/x-reldir/table",
        ));
    }
    if let crate::naming::Identity::Body { columns, .. } = &schema.row_identity {
        for (index, key) in columns.iter().enumerate() {
            let at = format!("/x-reldir/identity/columns/{index}");
            let problem = match schema.columns.get(key) {
                None => Some(format!("identity column {key:?} does not exist")),
                Some(column) if column.nullable => Some(format!(
                    "identity column {key:?} admits null, so a row could have no identity"
                )),
                Some(column) if !column.kind.is_scalar() => Some(format!(
                    "identity column {key:?} is {}, and a key is made of scalars",
                    column.kind.name()
                )),
                Some(column) if !column.required && column.generated.is_none() => Some(format!(
                    "identity column {key:?} is not in `required`, so a row could omit its identity"
                )),
                Some(column)
                    if matches!(
                        column.kind,
                        ColumnType::Float
                            | ColumnType::Number
                            | ColumnType::Bool
                            | ColumnType::Bytes
                    ) =>
                {
                    Some(format!(
                        "identity column {key:?} is {}, whose values do not spell a file name \
                         unambiguously",
                        column.kind.name()
                    ))
                }
                Some(_) => None,
            };
            if let Some(problem) = problem {
                out.push(column_error(Code::SCHEMA_IDENTITY_INVALID, problem, &at));
            }
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
        );
    for (at, list) in lists {
        let unique = at.starts_with("/x-reldir/unique/");
        for column in list {
            match schema.columns.get(column) {
                None => out.push(column_error(
                    Code::SCHEMA_COLUMN_UNKNOWN,
                    format!("constraint names unknown column {column:?}"),
                    &at,
                )),
                // Two arrays or objects are one key only by an equality JSON
                // does not define -- member order, number spelling -- so a
                // key is made of scalars.
                Some(found) if unique && !found.kind.is_scalar() => out.push(column_error(
                    Code::SCHEMA_KEY_NOT_SCALAR,
                    format!(
                        "unique key column {column:?} is {}, and a key is made of scalars",
                        found.kind.name()
                    ),
                    &at,
                )),
                Some(_) => {}
            }
        }
    }
    for (name, column) in &schema.columns {
        if let Some(generated) = column.generated {
            let at = format!("/x-reldir/generated/{}", super::path::escape_pointer(name));
            if generated.column_type() != column.kind {
                out.push(column_error(
                    Code::SCHEMA_DEFAULT_TYPE_MISMATCH,
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
                    Code::SCHEMA_DEFAULT_TYPE_MISMATCH,
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
                Code::SCHEMA_CONSTRAINT_NAME_DUPLICATE,
                format!("two constraints are named {name:?}; names identify constraints"),
                "/x-reldir",
            ));
        }
    }
    let single_key = schema.primary_key.len() == 1;
    if schema.identity_domain.is_some() && !single_key {
        out.push(column_error(
            Code::SCHEMA_DOMAIN_KEY_INVALID,
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
                    Code::SCHEMA_REFERENCE_PATH_INVALID,
                    format!("reference path {path} cannot be followed: {reason}"),
                    &format!("{at}/from/{position}"),
                )),
            }
        }
        if fk.from.len() > 1 {
            if fk.iterates() {
                out.push(column_error(
                    Code::SCHEMA_REFERENCE_PATH_INVALID,
                    "a composite key pairs one value from each path; a path through an array \
                     has no single value to pair"
                        .into(),
                    &format!("{at}/from"),
                ));
            }
            if !matches!(fk.to, Target::Table(_)) {
                out.push(column_error(
                    Code::SCHEMA_FK_TARGET_INVALID,
                    "a composite key references one table's composite key".into(),
                    &format!("{at}/to"),
                ));
            }
        }
        if !fk.columns.is_empty() && !matches!(fk.to, Target::Table(_)) {
            out.push(column_error(
                Code::SCHEMA_FK_TARGET_INVALID,
                "references into several tables compare with each table's primary key; `columns` \
                 applies to a single-table target"
                    .into(),
                &format!("{at}/columns"),
            ));
        }
        if !fk.columns.is_empty() && fk.columns.len() != fk.from.len() {
            out.push(column_error(
                Code::SCHEMA_FK_ACTION_INVALID,
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
                super::Action::Remove if leaf.positional => Some(
                    "remove takes out the array element holding the reference, and the array \
                     types its elements by position (`prefixItems`): every later element would \
                     move into a place that means something else",
                ),
                super::Action::Remove if !leaf.iterates && leaf.column.required => Some(
                    "remove takes out the array element holding the reference, or -- where the \
                     path crosses no array -- the member itself, which must then be optional; \
                     this one is required",
                ),
                _ => None,
            });
            if let Some(reason) = illegal {
                out.push(column_error(
                    Code::SCHEMA_FK_ACTION_INVALID,
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
                Code::SCHEMA_ACYCLIC_INVALID,
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
                    Code::SCHEMA_ACYCLIC_INVALID,
                    format!(
                        "edge {path} reaches a {} value, not the table's key type",
                        leaf.column.kind.name()
                    ),
                    &format!("{at}/edges/{position}"),
                )),
                Ok(_) => {}
                Err(reason) => out.push(column_error(
                    Code::SCHEMA_REFERENCE_PATH_INVALID,
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
        ColumnType::Integer => json!({ "type": typed("integer") }),
        ColumnType::Float => json!({ "type": typed("number"), meta::TYPE_TAG: "float" }),
        ColumnType::Number => json!({ "type": typed("number") }),
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

/// The subschema for an enumeration. `enum` judges every value, null too, so
/// a nullable enumeration lists null among its members.
pub fn enum_subschema(values: &[String], nullable: bool) -> Value {
    let mut schema = subschema(&ColumnType::Enum, nullable);
    let mut members: Vec<Value> = values.iter().cloned().map(Value::String).collect();
    if nullable {
        members.push(Value::Null);
    }
    schema["enum"] = Value::Array(members);
    schema
}

/// Rename member `old` to `new` everywhere a subschema describing one object
/// names it: `properties` (keeping its place), `required`,
/// `dependentRequired` and `dependentSchemas`, `propertyNames`, and the
/// branches of `if`, `then`, `else`, `not`, `allOf`, `anyOf` and `oneOf`,
/// which describe the same object. A `$ref`, or a `patternProperties`
/// pattern either name matches, could rule on the member from somewhere this
/// cannot see; renaming then is refused rather than half done.
pub fn rename_member(schema: &mut Map<String, Value>, old: &str, new: &str) -> Result<(), String> {
    for reference in ["$ref", "$dynamicRef"] {
        if schema.contains_key(reference) {
            return Err(format!(
                "the object's schema reaches a definition through `{reference}`, which may name \
                 {old:?}; rename it there by editing the schema"
            ));
        }
    }
    if let Some(Value::Object(patterns)) = schema.get("patternProperties") {
        for pattern in patterns.keys() {
            let regex = regex::Regex::new(pattern).map_err(|error| {
                format!("patternProperties {pattern:?} is not a pattern: {error}")
            })?;
            if regex.is_match(old) || regex.is_match(new) {
                return Err(format!(
                    "patternProperties {pattern:?} matches {:?}, so renaming would change which \
                     rules govern the member; edit the schema",
                    if regex.is_match(old) { old } else { new }
                ));
            }
        }
    }
    let rename_key = |members: &mut Map<String, Value>| {
        if members.contains_key(old) {
            let rebuilt: Map<String, Value> = std::mem::take(members)
                .into_iter()
                .map(|(key, value)| (if key == old { new.to_string() } else { key }, value))
                .collect();
            *members = rebuilt;
        }
    };
    let rename_names = |names: &mut Value| {
        if let Value::Array(names) = names {
            for name in names {
                if name.as_str() == Some(old) {
                    *name = json!(new);
                }
            }
        }
    };
    if let Some(Value::Object(properties)) = schema.get_mut("properties") {
        rename_key(properties);
    }
    if let Some(required) = schema.get_mut("required") {
        rename_names(required);
    }
    if let Some(Value::Object(dependent)) = schema.get_mut("dependentRequired") {
        rename_key(dependent);
        for names in dependent.values_mut() {
            rename_names(names);
        }
    }
    if let Some(Value::Object(dependent)) = schema.get_mut("dependentSchemas") {
        rename_key(dependent);
        for branch in dependent.values_mut() {
            if let Value::Object(branch) = branch {
                rename_member(branch, old, new)?;
            }
        }
    }
    if let Some(Value::Object(names)) = schema.get_mut("propertyNames") {
        if names.get("const").and_then(Value::as_str) == Some(old) {
            names.insert("const".into(), json!(new));
        }
        if let Some(listed) = names.get_mut("enum") {
            rename_names(listed);
        }
    }
    for key in ["if", "then", "else", "not"] {
        if let Some(Value::Object(branch)) = schema.get_mut(key) {
            rename_member(branch, old, new)?;
        }
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(Value::Array(branches)) = schema.get_mut(key) {
            for branch in branches {
                if let Value::Object(branch) = branch {
                    rename_member(branch, old, new)?;
                }
            }
        }
    }
    Ok(())
}

/// Builds a table document from nothing, for inference and scaffolding.
#[derive(Debug, Clone)]
pub struct TableBuilder {
    table: String,
    description: Option<String>,
    properties: Map<String, Value>,
    required: Vec<String>,
    identity: Option<crate::naming::Identity>,
    style: Option<crate::json_edit::Style>,
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
            identity: None,
            style: None,
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
    /// Where rows' identity comes from, and how their files are named.
    pub fn identity(&mut self, identity: crate::naming::Identity) -> &mut Self {
        self.identity = Some(identity);
        self
    }
    /// Key the table by these body columns, its files named by reldir's own
    /// encoding: the identity of a table declared from nothing.
    pub fn key(&mut self, columns: Vec<String>) -> &mut Self {
        self.identity(crate::naming::Identity::Body {
            columns,
            rule: crate::naming::NamingRule::Reldir,
        })
    }
    /// The style new rows' files are written in.
    pub fn style(&mut self, style: Option<crate::json_edit::Style>) -> &mut Self {
        self.style = style;
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
        if let Some(identity) = &self.identity {
            extension.insert(
                "identity".into(),
                serde_json::to_value(identity).expect("an identity serializes"),
            );
        }
        if let Some(style) = &self.style {
            extension.insert(
                "style".into(),
                serde_json::to_value(style).expect("a style serializes"),
            );
        }
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
/// result is read back through `decode` by [`Editor::finish`], so every rule
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
        rename_member(self.root(), old, new)?;
        if let Some(identity) = self.extension().get_mut("identity") {
            if let Some(columns) = identity.get_mut("columns").and_then(Value::as_array_mut) {
                columns.iter_mut().for_each(rename);
            }
            if let Some(column) = identity.get_mut("column") {
                rename(column);
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
                check["expr"] = json!(crate::sql::rename_in_check(&expr, table, old, new)?);
            }
        }
        Ok(self)
    }

    /// Rename column `old` of `table` to `new` in every assertion this schema
    /// declares; `has_column` says which columns other tables have, to tell a
    /// bare name that could mean either.
    pub fn rename_in_assertions(
        &mut self,
        table: &str,
        old: &str,
        new: &str,
        has_column: &dyn Fn(&str, &str) -> bool,
    ) -> Result<&mut Self, String> {
        if let Some(assertions) = self
            .extension()
            .get_mut("assertions")
            .and_then(Value::as_array_mut)
        {
            for assertion in assertions {
                let name = assertion["name"].as_str().unwrap_or_default().to_string();
                let query = assertion["query"].as_str().unwrap_or_default().to_string();
                let renamed = crate::sql::rename_in_assertion(&query, table, old, new, has_column)
                    .map_err(|reason| format!("assertion {name:?}: {reason}"))?;
                if renamed != query {
                    assertion["query"] = json!(renamed);
                }
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

    pub fn set_identity(&mut self, identity: &crate::naming::Identity) -> &mut Self {
        self.extension().insert(
            "identity".into(),
            serde_json::to_value(identity).expect("an identity serializes"),
        );
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
                "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}},
                "identityDomain": "lcas",
                "foreignKeys": [
                    { "from": ["feedback_rules[].misconception_ref"], "to": { "domain": "lcas" }, "onDelete": "remove" },
                    { "name": "requires", "from": ["relations[?relation='requires'].target"], "to": { "domain": "lcas" } }
                ],
                "acyclic": [{ "name": "prerequisites", "edges": ["relations[?relation='requires'].target"] }]
            }
        })
    }

    fn codes(result: Result<Schema, Problems>) -> Vec<crate::code::Code> {
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
            vec![Code::SCHEMA_REFERENCE_PATH_INVALID]
        );

        let mut to_object = lcas_items();
        to_object["x-reldir"]["foreignKeys"][0]["from"] = json!(["feedback_rules[]"]);
        assert_eq!(
            codes(Schema::from_document(to_object, None)),
            vec![Code::SCHEMA_REFERENCE_PATH_INVALID]
        );

        let mut bad_filter = lcas_items();
        bad_filter["x-reldir"]["foreignKeys"][1]["from"] =
            json!(["relations[?relation='nonsense'].target"]);
        assert_eq!(
            codes(Schema::from_document(bad_filter, None)),
            vec![Code::SCHEMA_REFERENCE_PATH_INVALID]
        );

        let mut unparsable = lcas_items();
        unparsable["x-reldir"]["foreignKeys"][1]["from"] = json!(["relations[x]"]);
        assert_eq!(
            codes(Schema::from_document(unparsable, None)),
            vec![Code::SCHEMA_REFERENCE_PATH_INVALID]
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
            vec![Code::SCHEMA_FK_ACTION_INVALID]
        );
        schema["x-reldir"]["foreignKeys"][0]["onDelete"] = json!("set_null");
        assert_eq!(
            codes(Schema::from_document(schema.clone(), None)),
            vec![Code::SCHEMA_FK_ACTION_INVALID]
        );
        schema["x-reldir"]["foreignKeys"][0]["onDelete"] = json!("cascade");
        assert!(codes(Schema::from_document(schema.clone(), None)).is_empty());

        // Outside an array, `remove` takes the member out: it must be one a
        // row may leave out.
        schema["required"] = json!(["id"]);
        schema["x-reldir"]["foreignKeys"][0]["onDelete"] = json!("remove");
        assert!(codes(Schema::from_document(schema.clone(), None)).is_empty());

        // Elements typed by position cannot be taken out of their places.
        schema["properties"]["pair"] = json!({
            "type": "array",
            "prefixItems": [{"type": "string"}, {"type": "integer"}],
            "items": {"type": "string"}
        });
        schema["x-reldir"]["foreignKeys"] = json!([
            { "from": ["pair[]"], "to": { "table": "people" }, "onDelete": "remove" }
        ]);
        assert_eq!(
            codes(Schema::from_document(schema, None)),
            vec![Code::SCHEMA_FK_ACTION_INVALID]
        );
    }

    #[test]
    fn test2058_unique_keys_are_made_of_scalars() {
        let mut keyed = lcas_items();
        keyed["x-reldir"]["unique"] = json!([["relations"]]);
        assert_eq!(
            codes(Schema::from_document(keyed.clone(), None)),
            vec![Code::SCHEMA_KEY_NOT_SCALAR]
        );
        keyed["x-reldir"].as_object_mut().unwrap().remove("unique");
        keyed["x-reldir"]["indexes"] = json!([["relations"]]);
        assert!(
            codes(Schema::from_document(keyed, None)).is_empty(),
            "an index orders; it does not decide equality"
        );
    }

    #[test]
    fn test2053_identity_columns_must_identify_every_row() {
        let mut nullable = lcas_items();
        nullable["properties"]["id"]["type"] = json!(["string", "null"]);
        assert_eq!(
            codes(Schema::from_document(nullable, None)),
            vec![Code::SCHEMA_IDENTITY_INVALID]
        );

        let mut optional = lcas_items();
        optional["required"] = json!([]);
        assert_eq!(
            codes(Schema::from_document(optional, None)),
            vec![Code::SCHEMA_IDENTITY_INVALID]
        );

        let mut missing = lcas_items();
        missing["x-reldir"]["identity"]["columns"] = json!(["ghost"]);
        assert!(
            codes(Schema::from_document(missing, None)).contains(&Code::SCHEMA_IDENTITY_INVALID)
        );

        let mut fractional = lcas_items();
        fractional["properties"]["id"] = json!({"type": "number"});
        assert!(
            codes(Schema::from_document(fractional, None)).contains(&Code::SCHEMA_IDENTITY_INVALID),
            "a number has many spellings, so it cannot name a file"
        );

        let mut from_name = lcas_items();
        from_name["x-reldir"]["identity"] =
            json!({"source": "filename", "column": "id", "type": "string"});
        assert!(
            codes(Schema::from_document(from_name, None)).contains(&Code::SCHEMA_IDENTITY_INVALID),
            "a column read from the file name cannot also be a member of the body"
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
        assert!(codes(editor.finish()).contains(&crate::code::Code::SCHEMA_REFERENCE_PATH_INVALID));
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
            .key(vec!["id".into()])
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
