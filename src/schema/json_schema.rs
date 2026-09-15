//! The only door between a schema file and the relational model.
//!
//! Schemas are stored as JSON Schema 2020-12 documents in jdb's dialect (see
//! [`super::meta`]). Everything inside jdb works on [`Schema`]; nothing else
//! reads or writes the file form. `encode` renders a schema, `decode` reads
//! one, and they are inverse:
//!
//! ```text
//! decode(encode(s)) == s                  for every schema jdb can hold
//! encode(decode(d)) == canonical(d)       for every document jdb accepts
//! ```
//!
//! The second law is deliberately weaker than byte equality. An ordinary JSON
//! tool may reindent a schema or reorder its members without changing what it
//! says, and jdb accepts that: `decode` is tolerant of layout, `encode` emits
//! one canonical form. What `decode` is *not* tolerant of is meaning it cannot
//! represent. A document using `oneOf` or `patternProperties` is valid JSON
//! Schema describing a constraint jdb has no relational equivalent for, so it
//! is refused with `SCHEMA_UNSUPPORTED_KEYWORD` rather than silently ignored --
//! ignoring it would let the file and the database disagree about what is
//! valid.
//!
//! The governing rule for anything else: if valid JSON Schema content does not
//! alter jdb's semantics, preserve it; if it would alter semantics jdb cannot
//! represent, reject it. Annotations such as `$comment` and `examples` fall on
//! the first side and survive a round trip untouched.

use super::meta::{DIALECT_URI, EXTENSION, TYPE_TAG};
use super::{
    Action, AdditionalFields, Check, Column, ColumnType, ForeignKey, Generated, GeneratedKind,
    Reference, Schema, Storage,
};
use crate::diagnostic::{DbError, Diagnostic, Result};
use indexmap::IndexMap;
use serde_json::{Map, Value};

/// Standard keywords that carry no relational meaning.
///
/// These are annotations in JSON Schema's own terms: they describe a schema
/// without constraining an instance. jdb keeps them verbatim so that a schema
/// written by a person, or by a tool, does not lose its commentary on the way
/// through -- but they take no part in validation and no part in identity.
const PRESERVED_ANNOTATIONS: &[&str] = &["title", "$comment", "examples", "readOnly", "deprecated"];

/// Keywords the codec consumes at the root of a document.
const ROOT_SEMANTIC: &[&str] = &[
    "$schema",
    "type",
    "properties",
    "required",
    "additionalProperties",
    "description",
    EXTENSION,
];

/// Keywords the codec consumes inside a column subschema.
const COLUMN_SEMANTIC: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "default",
    "description",
    "format",
    "pattern",
    "contentEncoding",
    TYPE_TAG,
];

/// Render a schema as a JSON Schema document.
///
/// Key order is chosen rather than sorted. Diagnostics locate a finding by
/// searching the schema's bytes for the first occurrence of a quoted name
/// (`integrity::locate`), and a column name appears in `properties`, in
/// `required`, and in `x-jdb.columnOrder`. Emitting `properties` first means a
/// diagnostic about a column points at the column's definition rather than at
/// a bare string in a list.
pub fn encode(schema: &Schema) -> Value {
    let mut out = Map::new();
    out.insert("$schema".into(), Value::String(DIALECT_URI.into()));
    out.insert("type".into(), Value::String("object".into()));
    if let Some(description) = &schema.description {
        out.insert("description".into(), Value::String(description.clone()));
    }

    let mut properties = Map::new();
    for (name, column) in &schema.columns {
        properties.insert(name.clone(), encode_column(column));
    }
    out.insert("properties".into(), Value::Object(properties));

    // `required` is about presence, not nullability. A column is required when
    // a row that omits it cannot be completed: no default to supply the value
    // and no nullability to excuse its absence. That is exactly the rule
    // `integrity` enforces when it reports ROW_MISSING_FIELD, and deriving
    // `required` from `nullable` instead would make every defaulted NOT NULL
    // column newly erroneous.
    let required: Vec<Value> = schema
        .columns
        .iter()
        .filter(|(_, column)| is_required(column))
        .map(|(name, _)| Value::String(name.clone()))
        .collect();
    if !required.is_empty() {
        out.insert("required".into(), Value::Array(required));
    }
    out.insert(
        "additionalProperties".into(),
        Value::Bool(schema.additional_fields == AdditionalFields::Allow),
    );

    out.insert(EXTENSION.into(), encode_extension(schema));
    for (key, value) in &schema.annotations {
        out.insert(key.clone(), value.clone());
    }
    Value::Object(out)
}

/// Whether a row that omits this column can still be completed.
pub fn is_required(column: &Column) -> bool {
    column.default.is_none() && !column.nullable && column.generated.is_none()
}

/// The relational facts JSON Schema has no keyword for.
fn encode_extension(schema: &Schema) -> Value {
    let mut x = Map::new();
    x.insert("table".into(), Value::String(schema.table.clone()));
    x.insert("schemaVersion".into(), schema.schema_version.into());
    if let Some(format) = schema.schema_format {
        x.insert("schemaFormat".into(), format.into());
    }
    x.insert("primaryKey".into(), strings(&schema.primary_key));
    // JSON object members are unordered, but rows are written in schema column
    // order, so the order is logical state and has to be carried explicitly.
    x.insert(
        "columnOrder".into(),
        Value::Array(
            schema
                .columns
                .keys()
                .map(|name| Value::String(name.clone()))
                .collect(),
        ),
    );
    if !schema.unique.is_empty() {
        x.insert("unique".into(), string_lists(&schema.unique));
    }
    if !schema.indexes.is_empty() {
        x.insert("indexes".into(), string_lists(&schema.indexes));
    }
    if !schema.foreign_keys.is_empty() {
        x.insert(
            "foreignKeys".into(),
            Value::Array(schema.foreign_keys.iter().map(encode_foreign_key).collect()),
        );
    }
    if !schema.check.is_empty() {
        x.insert(
            "checks".into(),
            Value::Array(
                schema
                    .check
                    .iter()
                    .map(|check| {
                        Value::Object(Map::from_iter([
                            ("name".to_string(), Value::String(check.name.clone())),
                            ("expr".to_string(), Value::String(check.expr.clone())),
                        ]))
                    })
                    .collect(),
            ),
        );
    }
    let generated: Map<String, Value> = schema
        .columns
        .iter()
        .filter_map(|(name, column)| {
            column.generated.as_ref().map(|generated| {
                (
                    name.clone(),
                    Value::String(generated_kind(&generated.kind).into()),
                )
            })
        })
        .collect();
    if !generated.is_empty() {
        x.insert("generated".into(), Value::Object(generated));
    }
    if let Some(storage) = &schema.storage {
        x.insert("filename".into(), strings(&storage.filename));
    }
    Value::Object(x)
}

/// A foreign key is not a `$ref`.
///
/// `$ref` means "apply this schema to the instance here" -- composition and
/// reuse. It cannot say that a value names a row that must exist in another
/// table, and it has nowhere to put a composite column list or a referential
/// action. Spelling foreign keys with `$ref` would make the document look
/// standard while making its meaning false, so they live in the vocabulary
/// and `$ref` keeps its own job.
fn encode_foreign_key(key: &ForeignKey) -> Value {
    let mut out = Map::new();
    out.insert("columns".into(), strings(&key.columns));
    out.insert(
        "references".into(),
        Value::Object(Map::from_iter([
            ("table".to_string(), Value::String(key.references.table.clone())),
            ("columns".to_string(), strings(&key.references.columns)),
        ])),
    );
    if let Some(on_delete) = key.on_delete {
        out.insert("onDelete".into(), Value::String(action_name(on_delete).into()));
    }
    if let Some(on_update) = key.on_update {
        out.insert("onUpdate".into(), Value::String(action_name(on_update).into()));
    }
    Value::Object(out)
}

/// One column as a subschema.
///
/// Standard keywords describe the value wherever they can, so that a generic
/// validator understands the shape. `x-jdb-type` is added only where no
/// standard keyword distinguishes two jdb types: `int` is lexical where JSON
/// Schema's `integer` is mathematical, and `decimal` and `ulid` are strings
/// carrying application semantics. The tag sits on the subschema it describes,
/// so it works at any depth -- an `array<decimal>` has no column name to key a
/// root-level map by.
fn encode_column(column: &Column) -> Value {
    let mut out = Map::new();
    let mut tag: Option<&str> = None;

    match column.kind {
        ColumnType::Bool => {
            out.insert("type".into(), typename("boolean", column.nullable));
        }
        ColumnType::Int => {
            out.insert("type".into(), typename("integer", column.nullable));
            tag = Some("int");
        }
        ColumnType::Float => {
            out.insert("type".into(), typename("number", column.nullable));
        }
        ColumnType::Decimal => {
            out.insert("type".into(), typename("string", column.nullable));
            out.insert("pattern".into(), Value::String(DECIMAL_PATTERN.into()));
            tag = Some("decimal");
        }
        ColumnType::String => {
            out.insert("type".into(), typename("string", column.nullable));
        }
        ColumnType::Bytes => {
            out.insert("type".into(), typename("string", column.nullable));
            out.insert("contentEncoding".into(), Value::String("base64".into()));
        }
        ColumnType::Date => {
            out.insert("type".into(), typename("string", column.nullable));
            out.insert("format".into(), Value::String("date".into()));
        }
        ColumnType::Timestamp => {
            out.insert("type".into(), typename("string", column.nullable));
            out.insert("format".into(), Value::String("date-time".into()));
        }
        ColumnType::Uuid => {
            out.insert("type".into(), typename("string", column.nullable));
            out.insert("format".into(), Value::String("uuid".into()));
        }
        ColumnType::Ulid => {
            out.insert("type".into(), typename("string", column.nullable));
            out.insert("pattern".into(), Value::String(ULID_PATTERN.into()));
            tag = Some("ulid");
        }
        ColumnType::Enum => {
            out.insert("type".into(), typename("string", column.nullable));
            if let Some(values) = &column.values {
                out.insert("enum".into(), strings(values));
            }
        }
        ColumnType::Array => {
            out.insert("type".into(), typename("array", column.nullable));
            if let Some(items) = &column.items {
                out.insert("items".into(), encode_column(items));
            }
        }
        ColumnType::Object => {
            out.insert("type".into(), typename("object", column.nullable));
            if let Some(properties) = &column.properties {
                let mut nested = Map::new();
                for (name, inner) in properties {
                    nested.insert(name.clone(), encode_column(inner));
                }
                out.insert("properties".into(), Value::Object(nested));
                let required: Vec<Value> = properties
                    .iter()
                    .filter(|(_, inner)| is_required(inner))
                    .map(|(name, _)| Value::String(name.clone()))
                    .collect();
                if !required.is_empty() {
                    out.insert("required".into(), Value::Array(required));
                }
                // Nested property order is logical state for the same reason
                // the top level's is.
                out.insert(
                    "x-jdb-column-order".into(),
                    Value::Array(
                        properties
                            .keys()
                            .map(|name| Value::String(name.clone()))
                            .collect(),
                    ),
                );
            }
        }
        // `json` admits any value, which in JSON Schema is the empty schema.
        // A nullable json column is no different: it already admits null.
        ColumnType::Json => {}
    }

    if let Some(tag) = tag {
        out.insert(TYPE_TAG.into(), Value::String(tag.into()));
    }
    if let Some(default) = &column.default {
        out.insert("default".into(), default.clone());
    }
    if let Some(description) = &column.description {
        out.insert("description".into(), Value::String(description.clone()));
    }
    for (key, value) in &column.annotations {
        out.insert(key.clone(), value.clone());
    }
    Value::Object(out)
}

/// A nullable column admits its type or null, which is how JSON Schema says it.
fn typename(name: &str, nullable: bool) -> Value {
    if nullable {
        Value::Array(vec![Value::String(name.into()), Value::String("null".into())])
    } else {
        Value::String(name.into())
    }
}

const DECIMAL_PATTERN: &str = r"^-?(0|[1-9][0-9]*)(\.[0-9]+)?$";
const ULID_PATTERN: &str = "^[0-7][0-9A-HJKMNP-TV-Z]{25}$";

fn strings(values: &[String]) -> Value {
    Value::Array(values.iter().map(|v| Value::String(v.clone())).collect())
}

fn string_lists(values: &[Vec<String>]) -> Value {
    Value::Array(values.iter().map(|v| strings(v)).collect())
}

fn generated_kind(kind: &GeneratedKind) -> &'static str {
    match kind {
        GeneratedKind::Uuid => "uuid",
        GeneratedKind::Ulid => "ulid",
        GeneratedKind::Now => "now",
        GeneratedKind::Sequence => "sequence",
    }
}

fn action_name(action: Action) -> &'static str {
    match action {
        Action::Restrict => "restrict",
        Action::Cascade => "cascade",
        Action::SetNull => "set_null",
        Action::SetDefault => "set_default",
        Action::NoAction => "no_action",
    }
}

fn parse_action(value: &Value, where_: &str) -> Result<Action> {
    let name = value
        .as_str()
        .ok_or_else(|| bad("SCHEMA_FK_ACTION_INVALID", format!("{where_} must be a string")))?;
    match name {
        "restrict" => Ok(Action::Restrict),
        "cascade" => Ok(Action::Cascade),
        "set_null" => Ok(Action::SetNull),
        "set_default" => Ok(Action::SetDefault),
        "no_action" => Ok(Action::NoAction),
        other => Err(bad(
            "SCHEMA_FK_ACTION_INVALID",
            format!("{where_}: unknown referential action {other:?}"),
        )),
    }
}

/// A list of column names.
///
/// A malformed list is refused rather than salvaged: a `primaryKey` that is not
/// an array of names does not describe row identity, and silently dropping the
/// parts that do not parse would produce a table keyed differently from what
/// its file says.
fn name_list(value: Option<&Value>, where_: &str) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    let entries = value.as_array().ok_or_else(|| {
        bad(
            "SCHEMA_MISSING_REQUIRED",
            format!("{where_} must be an array of column names"),
        )
    })?;
    entries
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(String::from)
                .ok_or_else(|| bad(
                    "SCHEMA_COLUMN_UNKNOWN",
                    format!("{where_} must contain only column names"),
                ))
        })
        .collect()
}

/// A list of column-name lists, as `unique` and `indexes` are written.
fn name_lists(value: Option<&Value>, where_: &str) -> Result<Vec<Vec<String>>> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    let entries = value.as_array().ok_or_else(|| {
        bad(
            "SCHEMA_MISSING_REQUIRED",
            format!("{where_} must be an array of column-name arrays"),
        )
    })?;
    entries
        .iter()
        .map(|entry| name_list(Some(entry), where_))
        .collect()
}

fn bad(code: &str, message: impl Into<String>) -> DbError {
    DbError::from_diag(Diagnostic::error(code, message), 2)
}

/// The same fault, anchored to the key it concerns.
///
/// The decoder works on a parsed document and so has no byte offsets, but it
/// does know the name of the member at fault. [`load`] resolves that name to a
/// line in the file, the way lint findings are resolved, so a schema error
/// points at the declaration rather than at the file as a whole.
fn bad_at(code: &str, anchor: &str, message: impl Into<String>) -> DbError {
    DbError::from_diag(Diagnostic::error(code, message).field(anchor), 2)
}

/// Read a JSON Schema document as a schema.
///
/// Tolerant of layout, strict about meaning: any keyword that would change what
/// counts as a valid row, and that jdb cannot represent, is refused by name.
pub fn decode(document: &Value) -> Result<Schema> {
    let root = document.as_object().ok_or_else(|| {
        bad(
            "SCHEMA_MISSING_REQUIRED",
            "a schema document must be a JSON object",
        )
    })?;

    if let Some(declared) = root.get("$schema").and_then(Value::as_str)
        && declared != DIALECT_URI
    {
        return Err(bad(
            "SCHEMA_UNSUPPORTED_KEYWORD",
            format!("$schema must be {DIALECT_URI:?}, not {declared:?}"),
        ));
    }
    reject_unsupported(root, ROOT_SEMANTIC, "schema")?;

    let extension = root.get(EXTENSION).and_then(Value::as_object).ok_or_else(|| {
        bad(
            "SCHEMA_MISSING_REQUIRED",
            format!("{EXTENSION} is required: it names the table and its key"),
        )
    })?;

    reject_unknown_extension_keys(extension)?;

    let table = extension
        .get("table")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            bad(
                "SCHEMA_MISSING_REQUIRED",
                "table is required to identify the relation",
            )
        })?
        .to_string();

    let primary_key = name_list(extension.get("primaryKey"), "primaryKey")?;
    if primary_key.is_empty() {
        return Err(bad(
            "SCHEMA_MISSING_REQUIRED",
            "primaryKey is required for row identity and keyed operations",
        ));
    }

    let properties = root
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            bad(
                "SCHEMA_MISSING_REQUIRED",
                "properties is required to define relational attributes",
            )
        })?;

    let generated = extension
        .get("generated")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let required: Vec<&str> = root
        .get("required")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let order = column_order(extension.get("columnOrder"), properties, "columnOrder")?;
    let mut columns = IndexMap::new();
    for name in order {
        let subschema = properties.get(&name).ok_or_else(|| {
            bad(
                "SCHEMA_COLUMN_UNKNOWN",
                format!("columnOrder names unknown column {name:?}"),
            )
        })?;
        let generated = match generated.get(&name) {
            Some(kind) => Some(parse_generated(kind, &name)?),
            None => None,
        };
        columns.insert(
            name.clone(),
            decode_column(subschema, &name, required.contains(&name.as_str()), generated)?,
        );
    }

    Ok(Schema {
        table,
        schema_version: extension
            .get("schemaVersion")
            .and_then(Value::as_u64)
            .unwrap_or(1) as u32,
        schema_format: extension
            .get("schemaFormat")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        description: root
            .get("description")
            .and_then(Value::as_str)
            .map(String::from),
        primary_key,
        columns,
        unique: name_lists(extension.get("unique"), "unique")?,
        foreign_keys: decode_foreign_keys(extension.get("foreignKeys"))?,
        check: decode_checks(extension.get("checks"))?,
        indexes: name_lists(extension.get("indexes"), "indexes")?,
        storage: match extension.get("filename") {
            Some(value) => Some(Storage {
                filename: name_list(Some(value), "filename")?,
            }),
            None => None,
        },
        additional_fields: match root.get("additionalProperties") {
            Some(Value::Bool(true)) => AdditionalFields::Allow,
            _ => AdditionalFields::Reject,
        },
        annotations: annotations_of(root),
    })
}

/// The order columns are written in.
///
/// Required, because JSON object members are unordered while rows are written
/// in column order. A document whose `columnOrder` disagrees with `properties`
/// describes two different tables, so the disagreement is reported rather than
/// resolved by preferring one side.
fn column_order(
    value: Option<&Value>,
    properties: &Map<String, Value>,
    where_: &str,
) -> Result<Vec<String>> {
    let order = name_list(value, where_)?;
    if order.len() != properties.len() {
        return Err(bad(
            "SCHEMA_COLUMN_UNKNOWN",
            format!(
                "{where_} lists {} columns but properties defines {}",
                order.len(),
                properties.len()
            ),
        ));
    }
    Ok(order)
}

fn parse_generated(value: &Value, column: &str) -> Result<Generated> {
    let kind = match value.as_str() {
        Some("uuid") => GeneratedKind::Uuid,
        Some("ulid") => GeneratedKind::Ulid,
        Some("now") => GeneratedKind::Now,
        Some("sequence") => GeneratedKind::Sequence,
        other => {
            return Err(bad(
                "SCHEMA_TYPE_UNKNOWN",
                format!("generated kind for {column:?} is {other:?}, which is not a generator"),
            ));
        }
    };
    Ok(Generated { kind })
}

fn decode_foreign_keys(value: Option<&Value>) -> Result<Vec<ForeignKey>> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    let entries = value.as_array().ok_or_else(|| {
        bad("SCHEMA_MISSING_REQUIRED", "foreignKeys must be an array")
    })?;
    let mut out = vec![];
    for entry in entries {
        let object = entry.as_object().ok_or_else(|| {
            bad(
                "SCHEMA_MISSING_REQUIRED",
                "each foreign key must be an object",
            )
        })?;
        let references = object
            .get("references")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                bad(
                    "SCHEMA_MISSING_REQUIRED",
                    "a foreign key must name what it references",
                )
            })?;
        out.push(ForeignKey {
            columns: name_list(object.get("columns"), "foreignKeys.columns")?,
            references: Reference {
                table: references
                    .get("table")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        bad(
                            "SCHEMA_MISSING_REQUIRED",
                            "a foreign key must name the table it references",
                        )
                    })?
                    .to_string(),
                columns: name_list(references.get("columns"), "references.columns")?,
            },
            on_delete: match object.get("onDelete") {
                Some(value) => Some(parse_action(value, "onDelete")?),
                None => None,
            },
            on_update: match object.get("onUpdate") {
                Some(value) => Some(parse_action(value, "onUpdate")?),
                None => None,
            },
        });
    }
    Ok(out)
}

fn decode_checks(value: Option<&Value>) -> Result<Vec<Check>> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    let entries = value
        .as_array()
        .ok_or_else(|| bad("SCHEMA_MISSING_REQUIRED", "checks must be an array"))?;
    let mut out = vec![];
    for entry in entries {
        let object = entry
            .as_object()
            .ok_or_else(|| bad("SCHEMA_CHECK_INVALID", "each check must be an object"))?;
        out.push(Check {
            name: object
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("SCHEMA_CHECK_INVALID", "a check must be named"))?
                .to_string(),
            expr: object
                .get("expr")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("SCHEMA_CHECK_INVALID", "a check must have an expression"))?
                .to_string(),
        });
    }
    Ok(out)
}

/// One column subschema.
///
/// `required` and `generated` arrive from the root, because JSON Schema states
/// presence at the object level and jdb's generators are a table-level fact.
fn decode_column(
    value: &Value,
    name: &str,
    required: bool,
    generated: Option<Generated>,
) -> Result<Column> {
    let object = value.as_object().ok_or_else(|| {
        bad_at(
            "SCHEMA_COLUMN_TYPE_MISSING",
            name,
            format!("column {name:?} must be a subschema object"),
        )
    })?;
    reject_unsupported(object, COLUMN_SEMANTIC, name)?;

    let (type_name, nullable) = read_type(object, name)?;
    let tag = object.get(TYPE_TAG).and_then(Value::as_str);
    let kind = column_kind(type_name.as_deref(), tag, object, name)?;

    let items = match object.get("items") {
        Some(items) => Some(Box::new(decode_column(items, &format!("{name}[]"), true, None)?)),
        None => None,
    };

    let properties = match object.get("properties").and_then(Value::as_object) {
        Some(nested) => {
            let required_names: Vec<&str> = object
                .get("required")
                .and_then(Value::as_array)
                .map(|values| values.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let order = match object.get("x-jdb-column-order") {
                Some(value) => column_order(Some(value), nested, "x-jdb-column-order")?,
                None => nested.keys().cloned().collect(),
            };
            let mut map = IndexMap::new();
            for key in order {
                let subschema = nested.get(&key).ok_or_else(|| {
                    bad(
                        "SCHEMA_COLUMN_UNKNOWN",
                        format!("{name}: column order names unknown property {key:?}"),
                    )
                })?;
                let inner = decode_column(
                    subschema,
                    &format!("{name}.{key}"),
                    required_names.contains(&key.as_str()),
                    None,
                )?;
                map.insert(key, inner);
            }
            Some(map)
        }
        None => None,
    };

    let values = object.get("enum").and_then(Value::as_array).map(|values| {
        values
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect()
    });

    let default = object.get("default").cloned();

    // Nullability comes from the type union, because that is where a schema
    // states it: `required` says whether a row may omit the key, which is a
    // different question. Reading nullability out of `required` would make
    // every defaulted NOT NULL column nullable on the next load.
    //
    // A column with no declared type is the exception, and the only one. It is
    // the empty schema -- jdb's `json` -- which admits any value including
    // null, so there is no union to carry the flag. For those, absence from
    // `required` is the only statement the document makes about whether a row
    // may leave the column out, and it is what `integrity` reads back when it
    // decides between a missing field and a legitimate omission.
    let nullable = match type_name {
        Some(_) => nullable,
        None => nullable || (!required && default.is_none() && generated.is_none()),
    };

    Ok(Column {
        kind,
        nullable,
        default,
        generated,
        values,
        items,
        properties,
        description: object
            .get("description")
            .and_then(Value::as_str)
            .map(String::from),
        annotations: annotations_of(object),
    })
}

/// The declared type, and whether null is among the alternatives.
fn read_type(object: &Map<String, Value>, name: &str) -> Result<(Option<String>, bool)> {
    match object.get("type") {
        None => Ok((None, false)),
        Some(Value::String(single)) => Ok((Some(single.clone()), false)),
        Some(Value::Array(alternatives)) => {
            let mut concrete = None;
            let mut nullable = false;
            for alternative in alternatives {
                match alternative.as_str() {
                    Some("null") => nullable = true,
                    Some(other) if concrete.is_none() => concrete = Some(other.to_string()),
                    _ => {
                        return Err(bad(
                            "SCHEMA_TYPE_UNKNOWN",
                            format!(
                                "column {name:?} declares more than one non-null type, which has \
                                 no single relational type"
                            ),
                        ));
                    }
                }
            }
            Ok((concrete, nullable))
        }
        Some(_) => Err(bad(
            "SCHEMA_TYPE_UNKNOWN",
            format!("column {name:?} has a type that is neither a name nor a list of names"),
        )),
    }
}

/// Which jdb type a subschema describes.
///
/// The tag decides where it is present, because it exists precisely for the
/// cases a standard keyword cannot express. Otherwise the standard keywords
/// decide, and an unrecognised combination is refused rather than guessed at:
/// a column whose type jdb cannot name is one it cannot validate, compare, or
/// hash.
fn column_kind(
    type_name: Option<&str>,
    tag: Option<&str>,
    object: &Map<String, Value>,
    name: &str,
) -> Result<ColumnType> {
    if let Some(tag) = tag {
        return match (tag, type_name) {
            ("int", Some("integer")) => Ok(ColumnType::Int),
            ("decimal", Some("string")) => Ok(ColumnType::Decimal),
            ("ulid", Some("string")) => Ok(ColumnType::Ulid),
            _ => Err(bad_at(
                "SCHEMA_TYPE_UNKNOWN",
                name,
                format!("column {name:?}: {TYPE_TAG} {tag:?} does not agree with its type"),
            )),
        };
    }
    let Some(type_name) = type_name else {
        // No type at all is the empty schema, which admits any value.
        return Ok(ColumnType::Json);
    };
    match type_name {
        "boolean" => Ok(ColumnType::Bool),
        "integer" => Ok(ColumnType::Int),
        "number" => Ok(ColumnType::Float),
        "array" => Ok(ColumnType::Array),
        "object" => Ok(ColumnType::Object),
        "string" => {
            if object.contains_key("enum") {
                return Ok(ColumnType::Enum);
            }
            if object.get("contentEncoding").and_then(Value::as_str) == Some("base64") {
                return Ok(ColumnType::Bytes);
            }
            match object.get("format").and_then(Value::as_str) {
                Some("date") => Ok(ColumnType::Date),
                Some("date-time") => Ok(ColumnType::Timestamp),
                Some("uuid") => Ok(ColumnType::Uuid),
                Some(other) => Err(bad(
                    "SCHEMA_TYPE_UNKNOWN",
                    format!("column {name:?}: format {other:?} names no jdb type"),
                )),
                None => Ok(ColumnType::String),
            }
        }
        other => Err(bad_at(
            "SCHEMA_TYPE_UNKNOWN",
            name,
            format!("column {name:?}: type {other:?} names no jdb type"),
        )),
    }
}

/// Keep what is inert, refuse what is not.
///
/// A keyword this codec does not consume is either an annotation, which is
/// preserved untouched, or an assertion that would change which rows are valid.
/// jdb cannot enforce the latter, and a schema whose file claims a constraint
/// the database does not apply is worse than one that refuses to load.
fn reject_unsupported(object: &Map<String, Value>, semantic: &[&str], where_: &str) -> Result<()> {
    for key in object.keys() {
        if semantic.contains(&key.as_str()) || PRESERVED_ANNOTATIONS.contains(&key.as_str()) {
            continue;
        }
        if key == "x-jdb-column-order" {
            continue;
        }
        return Err(bad(
            "SCHEMA_UNSUPPORTED_KEYWORD",
            format!(
                "{where_}: {key:?} is not part of the jdb dialect; it would change which rows are \
                 valid in a way jdb cannot enforce"
            ),
        ));
    }
    Ok(())
}

/// The keys `x-jdb` accepts.
const EXTENSION_KEYS: &[&str] = &[
    "table",
    "schemaVersion",
    "schemaFormat",
    "primaryKey",
    "columnOrder",
    "unique",
    "indexes",
    "foreignKeys",
    "checks",
    "generated",
    "filename",
];

/// Refuse an unrecognised relational key, and say what was probably meant.
///
/// `x-jdb` has a closed key set, so a key that is not in it is a mistake rather
/// than an extension -- and a mistake with an obvious intent, since `"uniqe"` is
/// one edit away from `"unique"`. Naming the nearest key turns a rejection into
/// a correction; silently ignoring the key would leave the constraint the author
/// wrote unenforced.
fn reject_unknown_extension_keys(extension: &Map<String, Value>) -> Result<()> {
    for key in extension.keys() {
        if EXTENSION_KEYS.contains(&key.as_str()) {
            continue;
        }
        let nearest = EXTENSION_KEYS
            .iter()
            .min_by_key(|candidate| strsim::levenshtein(key, candidate));
        let message = match nearest {
            Some(nearest) => format!(
                "{EXTENSION}: unknown key {key:?}; nearest valid key is {nearest:?}"
            ),
            None => format!("{EXTENSION}: unknown key {key:?}"),
        };
        return Err(bad("SCHEMA_UNKNOWN_KEY", message));
    }
    Ok(())
}

/// The annotations on a node, in the order a canonical rendering gives them.
fn annotations_of(object: &Map<String, Value>) -> IndexMap<String, Value> {
    let mut out = IndexMap::new();
    for key in PRESERVED_ANNOTATIONS {
        if let Some(value) = object.get(*key) {
            out.insert((*key).to_string(), value.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{AdditionalFields, ColumnType};
    use serde_json::json;

    fn col(kind: ColumnType) -> Column {
        Column {
            kind,
            nullable: false,
            default: None,
            generated: None,
            values: None,
            items: None,
            properties: None,
            description: None,
            annotations: IndexMap::new(),
        }
    }

    fn table(columns: Vec<(&str, Column)>) -> Schema {
        let mut map = IndexMap::new();
        for (name, column) in columns {
            map.insert(name.to_string(), column);
        }
        Schema {
            table: "t".into(),
            schema_version: 1,
            schema_format: None,
            description: None,
            primary_key: vec!["id".into()],
            columns: map,
            unique: vec![],
            foreign_keys: vec![],
            check: vec![],
            indexes: vec![],
            storage: None,
            additional_fields: AdditionalFields::Reject,
            annotations: IndexMap::new(),
        }
    }

    /// Everything the encoder must survive, one column shape per entry.
    fn every_shape() -> Vec<(&'static str, Column)> {
        let mut out = vec![];
        for (label, kind) in [
            ("bool", ColumnType::Bool),
            ("int", ColumnType::Int),
            ("float", ColumnType::Float),
            ("decimal", ColumnType::Decimal),
            ("string", ColumnType::String),
            ("bytes", ColumnType::Bytes),
            ("date", ColumnType::Date),
            ("timestamp", ColumnType::Timestamp),
            ("uuid", ColumnType::Uuid),
            ("ulid", ColumnType::Ulid),
            ("json", ColumnType::Json),
        ] {
            out.push((label, col(kind)));
        }
        let mut enumerated = col(ColumnType::Enum);
        enumerated.values = Some(vec!["a".into(), "b".into()]);
        out.push(("enum", enumerated));

        // Nesting is where a root-level type map would have failed: an
        // array<decimal> has no column name to key such a map by.
        let mut decimals = col(ColumnType::Array);
        decimals.items = Some(Box::new(col(ColumnType::Decimal)));
        out.push(("array_of_decimal", decimals));

        let mut deep = col(ColumnType::Array);
        let mut inner = col(ColumnType::Array);
        inner.items = Some(Box::new(col(ColumnType::Ulid)));
        deep.items = Some(Box::new(inner));
        out.push(("array_of_array_of_ulid", deep));

        let mut properties = IndexMap::new();
        properties.insert("when".to_string(), col(ColumnType::Timestamp));
        let mut nullable_inner = col(ColumnType::Int);
        nullable_inner.nullable = true;
        properties.insert("count".to_string(), nullable_inner);
        let mut object = col(ColumnType::Object);
        object.properties = Some(properties);
        out.push(("object_with_properties", object));

        let mut nullable = col(ColumnType::String);
        nullable.nullable = true;
        out.push(("nullable", nullable));

        // A `json` column is the empty schema, so it has no type union to carry
        // nullability. It is exactly the shape the law would otherwise not
        // cover, and the one where losing the flag turns a legitimate omission
        // into ROW_MISSING_FIELD.
        let mut nullable_json = col(ColumnType::Json);
        nullable_json.nullable = true;
        out.push(("nullable_json", nullable_json));

        let mut nullable_array = col(ColumnType::Array);
        nullable_array.nullable = true;
        nullable_array.items = Some(Box::new(col(ColumnType::String)));
        out.push(("nullable_array", nullable_array));

        let mut defaulted = col(ColumnType::String);
        defaulted.default = Some(json!("fixed"));
        out.push(("default_not_null", defaulted));

        let mut both = col(ColumnType::String);
        both.nullable = true;
        both.default = Some(json!("fixed"));
        out.push(("nullable_and_default", both));

        let mut described = col(ColumnType::String);
        described.description = Some("what it holds".into());
        out.push(("described", described));

        out
    }

    /// A nullable `json` column must survive the file form.
    ///
    /// `json` admits any value, which the dialect spells as the empty schema --
    /// and an empty schema already admits null. The risk is that nullability
    /// then has nowhere to live, so the column comes back NOT NULL and a row
    /// that legitimately omits it becomes invalid.
    #[test]
    fn test1140_a_nullable_json_column_stays_nullable() {
        let mut nullable = col(ColumnType::Json);
        nullable.nullable = true;
        let before = table(vec![("id", col(ColumnType::String)), ("v", nullable)]);
        let document = encode(&before);
        let after = decode(&document).expect("decodes");
        assert!(
            after.columns["v"].nullable,
            "a nullable json column must not come back NOT NULL: {}",
            serde_json::to_string(&document).unwrap()
        );
        assert_eq!(
            crate::schema::semantic::encode_v1(&before),
            crate::schema::semantic::encode_v1(&after)
        );
    }

    /// `decode(encode(s)) == s`, for every column shape jdb can hold.
    ///
    /// This is the law that makes the format change safe: if a schema does not
    /// survive a trip through the file form, then writing it and reading it
    /// back has silently changed the database's rules.
    #[test]
    fn test1131_every_schema_survives_a_round_trip_through_the_file_form() {
        for (label, column) in every_shape() {
            let before = table(vec![("id", col(ColumnType::String)), ("v", column)]);
            let document = encode(&before);
            let after = decode(&document)
                .unwrap_or_else(|error| panic!("{label}: decode failed: {}", error.diagnostic.code));

            assert_eq!(
                crate::schema::semantic::encode_v1(&before),
                crate::schema::semantic::encode_v1(&after),
                "{label}: the schema changed meaning on a round trip\n{}",
                serde_json::to_string_pretty(&document).unwrap()
            );
        }
    }

    /// Relational facts survive too, including the ones JSON Schema has no
    /// keyword for.
    #[test]
    fn test1132_relational_facts_survive_a_round_trip() {
        let mut schema = table(vec![
            ("id", col(ColumnType::String)),
            ("email", col(ColumnType::String)),
            ("team_id", {
                let mut c = col(ColumnType::String);
                c.nullable = true;
                c
            }),
        ]);
        schema.unique = vec![vec!["email".into()]];
        schema.indexes = vec![vec!["team_id".into()]];
        schema.check = vec![Check {
            name: "has_at".into(),
            expr: "email LIKE '%@%'".into(),
        }];
        schema.foreign_keys = vec![ForeignKey {
            columns: vec!["team_id".into()],
            references: Reference {
                table: "teams".into(),
                columns: vec!["id".into()],
            },
            on_delete: Some(Action::SetNull),
            on_update: Some(Action::Restrict),
        }];
        schema.storage = Some(Storage {
            filename: vec!["email".into()],
        });
        schema.additional_fields = AdditionalFields::Allow;
        schema.schema_version = 4;
        schema.schema_format = Some(crate::FORMAT_VERSION);
        schema.description = Some("people".into());
        schema.columns.get_mut("id").unwrap().generated = Some(Generated {
            kind: GeneratedKind::Ulid,
        });
        schema.columns.get_mut("id").unwrap().kind = ColumnType::Ulid;

        let after = decode(&encode(&schema)).expect("decodes");
        assert_eq!(
            crate::schema::semantic::encode_v1(&schema),
            crate::schema::semantic::encode_v1(&after)
        );
        // The vocabulary carries what JSON Schema cannot say.
        let document = encode(&schema);
        assert_eq!(document["x-jdb"]["foreignKeys"][0]["onDelete"], json!("set_null"));
        assert_eq!(document["x-jdb"]["filename"], json!(["email"]));
        assert_eq!(document["x-jdb"]["generated"]["id"], json!("ulid"));
        assert_eq!(document["additionalProperties"], json!(true));
    }

    /// Column order is logical state, so the file has to carry it and a decode
    /// has to restore it. Object members are unordered in JSON, so without
    /// `columnOrder` a reordered file would silently rewrite every row.
    #[test]
    fn test1133_column_order_survives_the_file_form() {
        let schema = table(vec![
            ("id", col(ColumnType::String)),
            ("zeta", col(ColumnType::String)),
            ("alpha", col(ColumnType::String)),
        ]);
        let after = decode(&encode(&schema)).expect("decodes");
        assert_eq!(
            after.columns.keys().collect::<Vec<_>>(),
            vec!["id", "zeta", "alpha"],
            "the order columns were declared in must come back"
        );
    }

    /// `required` states presence, not nullability.
    ///
    /// A column with a default may be omitted from a row, so it is not
    /// required; that says nothing about whether null is an admissible value.
    /// Deriving one from the other would make every defaulted NOT NULL column
    /// either newly erroneous or newly nullable.
    #[test]
    fn test1134_required_follows_the_rule_that_decides_a_missing_field() {
        let mut plain = col(ColumnType::String);
        let mut nullable = col(ColumnType::String);
        nullable.nullable = true;
        let mut defaulted = col(ColumnType::String);
        defaulted.default = Some(json!("x"));
        let mut generated = col(ColumnType::Ulid);
        generated.generated = Some(Generated {
            kind: GeneratedKind::Ulid,
        });

        assert!(is_required(&plain), "no default and not nullable: required");
        assert!(!is_required(&nullable), "nullable excuses absence");
        assert!(!is_required(&defaulted), "a default supplies the value");
        assert!(!is_required(&generated), "a generator supplies the value");

        plain.description = Some("still required".into());
        let schema = table(vec![
            ("id", col(ColumnType::String)),
            ("plain", plain),
            ("nullable", nullable),
            ("defaulted", defaulted.clone()),
        ]);
        let document = encode(&schema);
        assert_eq!(
            document["required"],
            json!(["id", "plain"]),
            "only columns a row cannot omit are required"
        );

        // And the defaulted NOT NULL column must not come back nullable.
        let after = decode(&document).expect("decodes");
        assert!(
            !after.columns["defaulted"].nullable,
            "a default must not make a column nullable"
        );
        assert_eq!(after.columns["defaulted"].default, Some(json!("x")));
    }

    /// Valid JSON Schema that jdb has no relational meaning for is refused by
    /// name, never ignored. Ignoring it would leave the file claiming a
    /// constraint the database does not enforce.
    #[test]
    fn test1135_unsupported_keywords_are_refused_rather_than_ignored() {
        for keyword in [
            "oneOf",
            "anyOf",
            "allOf",
            "not",
            "if",
            "patternProperties",
            "dependentSchemas",
            "unevaluatedProperties",
            "$ref",
            "$defs",
            "minLength",
        ] {
            let mut document = encode(&table(vec![("id", col(ColumnType::String))]));
            document[keyword] = json!({});
            let error = decode(&document)
                .expect_err(&format!("{keyword} must be refused at the document root"));
            assert_eq!(
                error.diagnostic.code, "SCHEMA_UNSUPPORTED_KEYWORD",
                "{keyword} must be refused by name"
            );

            let mut nested = encode(&table(vec![("id", col(ColumnType::String))]));
            nested["properties"]["id"][keyword] = json!({});
            let error = decode(&nested)
                .expect_err(&format!("{keyword} must be refused inside a column"));
            assert_eq!(error.diagnostic.code, "SCHEMA_UNSUPPORTED_KEYWORD");
        }
    }

    /// Annotations carry no relational meaning, so they are preserved rather
    /// than refused -- and preserved without becoming part of what a schema
    /// *is*, so a comment cannot change a database's identity.
    #[test]
    fn test1136_annotations_survive_without_changing_identity() {
        let plain = table(vec![("id", col(ColumnType::String))]);

        let mut annotated = plain.clone();
        annotated
            .annotations
            .insert("$comment".into(), json!("written by hand"));
        annotated
            .annotations
            .insert("title".into(), json!("People"));
        annotated.columns.get_mut("id").unwrap().annotations.insert(
            "$comment".into(),
            json!("the key"),
        );

        let document = encode(&annotated);
        assert_eq!(document["$comment"], json!("written by hand"));
        assert_eq!(document["properties"]["id"]["$comment"], json!("the key"));

        let after = decode(&document).expect("annotations are accepted");
        assert_eq!(after.annotations, annotated.annotations, "kept verbatim");
        assert_eq!(after.columns["id"].annotations.len(), 1);

        assert_eq!(
            crate::schema::semantic::encode_v1(&plain),
            crate::schema::semantic::encode_v1(&annotated),
            "an annotation must not change what a schema is"
        );
    }

    /// `encode(decode(d)) == encode(d)` -- the encoder emits one canonical
    /// form, so a document written by an ordinary tool at a different
    /// indentation or member order still lands on the same bytes.
    #[test]
    fn test1137_decoding_is_tolerant_of_layout_and_encoding_is_canonical() {
        let schema = table(vec![
            ("id", col(ColumnType::String)),
            ("b", col(ColumnType::Int)),
            ("a", col(ColumnType::Bool)),
        ]);
        let canonical = encode(&schema);

        // Rebuild the same document with its members in a different order.
        let mut shuffled = Map::new();
        let object = canonical.as_object().unwrap();
        for key in object.keys().rev() {
            shuffled.insert(key.clone(), object[key].clone());
        }
        let shuffled = Value::Object(shuffled);
        assert_ne!(
            serde_json::to_string(&shuffled).unwrap(),
            serde_json::to_string(&canonical).unwrap(),
            "the fixture must actually differ in layout"
        );

        let reencoded = encode(&decode(&shuffled).expect("layout does not matter"));
        assert_eq!(
            serde_json::to_string(&reencoded).unwrap(),
            serde_json::to_string(&canonical).unwrap(),
            "one canonical rendering, whatever the input layout"
        );
    }

    /// Every document the encoder produces is a valid instance of the bundled
    /// dialect. Without this the meta-schema would be decoration.
    #[test]
    fn test1138_encoded_schemas_conform_to_the_bundled_dialect() {
        let validator = crate::schema::meta::validator().expect("the dialect compiles");
        for (label, column) in every_shape() {
            let schema = table(vec![("id", col(ColumnType::String)), ("v", column)]);
            let document = encode(&schema);
            let errors: Vec<String> = validator
                .iter_errors(&document)
                .map(|error| format!("{} at {}", error, error.instance_path()))
                .collect();
            assert!(errors.is_empty(), "{label}: {errors:?}");
        }
    }
}
