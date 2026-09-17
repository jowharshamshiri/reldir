//! What a schema *is*, independent of how it is written down.
//!
//! A revision is recorded against the hash of its schemas, so that hash is the
//! schema's identity. Identity must therefore be a property of the relational
//! content -- the columns, their types, the keys and constraints -- and not of
//! the file format that happens to carry it. Those are different things, and
//! conflating them means that reformatting a file rewrites history.
//!
//! They were conflated: the hash was taken over `serde_json::to_value(schema)`,
//! the same rendering that landed on disk. Replacing the on-disk grammar with a
//! JSON Schema dialect would therefore have moved every hash in every database,
//! for a change that alters no meaning whatsoever.
//!
//! So identity moves here, to an encoding that is versioned, private, and never
//! written to disk. It reproduces what the old derive produced in every respect
//! but one: `columns` carries its order explicitly, because the old form lost
//! it. That single change is the subject of the doc comment at `columns` below.

use crate::schema::{
    AdditionalFields, Action, Check, Column, ColumnType, CompositionKind, ForeignKey, Generated,
    GeneratedKind,
    Reference, Schema, Storage,
};
use serde_json::{Map, Value};

/// The logical content of a schema, as the bytes its identity is taken over.
///
/// Version 1. The name is part of the contract: a future encoding that orders
/// or spells anything differently is `encode_v2`, chosen explicitly, never a
/// silent edit to this function. Changing what this emits changes what every
/// database in existence believes its own history to be.
pub fn encode_v1(schema: &Schema) -> Value {
    let mut out = Map::new();
    out.insert("table".into(), Value::String(schema.table.clone()));
    out.insert("schema_version".into(), schema.schema_version.into());
    if let Some(format) = schema.schema_format {
        out.insert("schema_format".into(), format.into());
    }
    if let Some(description) = &schema.description {
        out.insert("description".into(), Value::String(description.clone()));
    }
    out.insert("primary_key".into(), strings(&schema.primary_key));

    // Column order is logical state, not presentation. `canonical_row` writes a
    // row's keys in schema column order, so two schemas that order their
    // columns differently write different bytes for the same logical row. The
    // old encoding lost this: it emitted `columns` as an object, and the
    // normalization applied before hashing sorts object keys, so the ordering
    // was erased and two genuinely different schemas shared one identity.
    //
    // Encoding the order explicitly, as a list of pairs, is what makes the hash
    // agree with what the database actually writes to disk. `test1126` is the
    // witness for the case that was previously indistinguishable.
    let columns: Vec<Value> = schema
        .columns
        .iter()
        .map(|(name, column)| Value::Array(vec![Value::String(name.clone()), encode_column(column)]))
        .collect();
    out.insert("columns".into(), Value::Array(columns));

    // The remaining collections are ordered by the user and carry that order as
    // written: a reordered `unique` list is the same set of constraints, but it
    // is also what the file says, and reldir does not reorder it on the way to
    // disk. Encoding them as given keeps identity agreeing with the artifact.
    out.insert("unique".into(), string_lists(&schema.unique));
    out.insert(
        "foreign_keys".into(),
        Value::Array(schema.foreign_keys.iter().map(encode_foreign_key).collect()),
    );
    out.insert(
        "check".into(),
        Value::Array(schema.check.iter().map(encode_check).collect()),
    );
    out.insert("indexes".into(), string_lists(&schema.indexes));
    if let Some(storage) = &schema.storage {
        out.insert("storage".into(), encode_storage(storage));
    }
    out.insert(
        "additional_fields".into(),
        Value::String(additional_fields(&schema.additional_fields).into()),
    );
    Value::Object(out)
}

/// One column, recursively.
///
/// Nested shape is part of the type: `array<decimal>` and `array<string>` are
/// different columns, so `items` and `properties` are encoded through the same
/// function rather than summarized.
fn encode_column(column: &Column) -> Value {
    let mut out = Map::new();
    out.insert("type".into(), Value::String(column_type(&column.kind).into()));
    out.insert("nullable".into(), Value::Bool(column.nullable));
    if let Some(default) = &column.default {
        out.insert("default".into(), default.clone());
    }
    if let Some(generated) = &column.generated {
        out.insert("generated".into(), encode_generated(generated));
    }
    if let Some(values) = &column.values {
        out.insert("values".into(), strings(values));
    }
    if let Some(items) = &column.items {
        out.insert("items".into(), encode_column(items));
    }
    if let Some(properties) = &column.properties {
        // Nested property order matters for the same reason the top level does.
        let entries: Vec<Value> = properties
            .iter()
            .map(|(name, nested)| {
                Value::Array(vec![Value::String(name.clone()), encode_column(nested)])
            })
            .collect();
        out.insert("properties".into(), Value::Array(entries));
    }
    // A pattern decides which rows a column admits, so two schemas differing
    // only by one are not the same schema and must not share an identity.
    if let Some(pattern) = &column.pattern {
        out.insert("pattern".into(), Value::String(pattern.clone()));
    }
    // Likewise: closing an object rejects rows an open one accepts. Only the
    // closed case is encoded, so a schema that never mentions the keyword keeps
    // the identity it had before the keyword existed.
    if !column.additional_properties {
        out.insert("additionalProperties".into(), Value::Bool(false));
    }
    // Every bound decides which rows a column admits, so each is part of what
    // the schema IS. A column with no bound encodes nothing, which is what lets
    // schemas written before bounds existed keep the identity they had.
    if let Some(bound) = column.min_size {
        out.insert("min_size".into(), bound.into());
    }
    if let Some(bound) = column.max_size {
        out.insert("max_size".into(), bound.into());
    }
    for (key, bound) in [
        ("minimum", column.minimum),
        ("maximum", column.maximum),
        ("exclusive_minimum", column.exclusive_minimum),
        ("exclusive_maximum", column.exclusive_maximum),
        ("multiple_of", column.multiple_of),
    ] {
        if let Some(bound) = bound
            && let Some(number) = serde_json::Number::from_f64(bound)
        {
            out.insert(key.into(), Value::Number(number));
        }
    }
    if column.unique_items {
        out.insert("unique_items".into(), Value::Bool(true));
    }
    // An object that must carry a member admits fewer values than one that need
    // not, so the requirement is part of what the schema is. Sorted, because a
    // set has no order and identity must not depend on one.
    if !column.required.is_empty() {
        out.insert(
            "required".into(),
            Value::Array(column.required.iter().map(|n| Value::String(n.clone())).collect()),
        );
    }
    // Composition changes which values are legal, and the ORDER of alternatives
    // is not meaningful to `oneOf` -- but it is meaningful to the document, and
    // reldir does not reorder what a person wrote. Encoding them as given keeps
    // identity agreeing with the artifact, as `unique` and `indexes` do.
    if let Some(composition) = &column.composition {
        let kind = match composition.kind {
            CompositionKind::One => "one_of",
            CompositionKind::Any => "any_of",
            CompositionKind::All => "all_of",
            CompositionKind::Not => "not",
        };
        let mut encoded = Map::new();
        encoded.insert("kind".into(), Value::String(kind.into()));
        encoded.insert(
            "alternatives".into(),
            Value::Array(composition.alternatives.iter().map(encode_column).collect()),
        );
        out.insert("composition".into(), Value::Object(encoded));
    }
    if let Some(description) = &column.description {
        out.insert("description".into(), Value::String(description.clone()));
    }
    Value::Object(out)
}

fn encode_generated(generated: &Generated) -> Value {
    let kind = match generated.kind {
        GeneratedKind::Uuid => "uuid",
        GeneratedKind::Ulid => "ulid",
        GeneratedKind::Now => "now",
        GeneratedKind::Sequence => "sequence",
    };
    Value::Object(Map::from_iter([(
        "kind".to_string(),
        Value::String(kind.into()),
    )]))
}

/// A foreign key, including the actions it leaves unstated.
///
/// An omitted `on_delete` means restrict, so a key that omits it and one that
/// writes it mean the same thing. Encoding the resolved action rather than the
/// written one makes those two schemas share an identity, which is correct:
/// they impose identical referential behaviour.
fn encode_foreign_key(key: &ForeignKey) -> Value {
    let mut out = Map::new();
    out.insert("columns".into(), strings(&key.columns));
    out.insert("references".into(), encode_reference(&key.references));
    out.insert(
        "on_delete".into(),
        Value::String(action(key.delete_action()).into()),
    );
    out.insert(
        "on_update".into(),
        Value::String(action(key.update_action()).into()),
    );
    Value::Object(out)
}

fn encode_reference(reference: &Reference) -> Value {
    let mut out = Map::new();
    out.insert("table".into(), Value::String(reference.table.clone()));
    out.insert("columns".into(), strings(&reference.columns));
    Value::Object(out)
}

fn encode_check(check: &Check) -> Value {
    let mut out = Map::new();
    out.insert("name".into(), Value::String(check.name.clone()));
    out.insert("expr".into(), Value::String(check.expr.clone()));
    Value::Object(out)
}

fn encode_storage(storage: &Storage) -> Value {
    Value::Object(Map::from_iter([(
        "filename".to_string(),
        strings(&storage.filename),
    )]))
}

fn strings(values: &[String]) -> Value {
    Value::Array(values.iter().map(|v| Value::String(v.clone())).collect())
}

fn string_lists(values: &[Vec<String>]) -> Value {
    Value::Array(values.iter().map(|v| strings(v)).collect())
}

/// The wire spelling of each column type.
///
/// Written out rather than derived from the enum's name so that renaming a
/// variant in Rust cannot silently change every hash in every database.
fn column_type(kind: &ColumnType) -> &'static str {
    match kind {
        ColumnType::Bool => "bool",
        ColumnType::Int => "int",
        ColumnType::Float => "float",
        ColumnType::Decimal => "decimal",
        ColumnType::String => "string",
        ColumnType::Bytes => "bytes",
        ColumnType::Date => "date",
        ColumnType::Timestamp => "timestamp",
        ColumnType::Uuid => "uuid",
        ColumnType::Ulid => "ulid",
        ColumnType::Enum => "enum",
        ColumnType::Array => "array",
        ColumnType::Object => "object",
        ColumnType::Json => "json",
    }
}

fn action(action: Action) -> &'static str {
    match action {
        Action::Restrict => "restrict",
        Action::Cascade => "cascade",
        Action::SetNull => "set_null",
        Action::SetDefault => "set_default",
        Action::NoAction => "no_action",
    }
}

fn additional_fields(fields: &AdditionalFields) -> &'static str {
    match fields {
        AdditionalFields::Reject => "reject",
        AdditionalFields::Allow => "allow",
    }
}
