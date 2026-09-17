// Identity, kept apart from the file format that carries it. A schema's hash
// must follow its relational content, not the grammar it happens to be written
// in, or changing the grammar would rewrite every database's history.
pub mod json_schema;
pub mod meta;
pub mod semantic;

use crate::diagnostic::{DbError, Diagnostic, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, fs, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ColumnType {
    Bool,
    Int,
    Float,
    Decimal,
    String,
    Bytes,
    Date,
    Timestamp,
    Uuid,
    Ulid,
    Enum,
    Array,
    Object,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generated {
    pub kind: GeneratedKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeneratedKind {
    Uuid,
    Ulid,
    Now,
    Sequence,
}

/// A column's logical type and constraints.
///
/// Deliberately not `Serialize`/`Deserialize`: the file form is JSON Schema,
/// produced and consumed only by [`json_schema`], and a derive here would be a
/// second way to write a schema that no one maintains. The compiler enforces
/// the boundary.
#[derive(Debug, Clone)]
pub struct Column {
    pub kind: ColumnType,
    pub nullable: bool,
    pub default: Option<Value>,
    pub generated: Option<Generated>,
    pub values: Option<Vec<String>>,
    pub items: Option<Box<Column>>,
    pub properties: Option<IndexMap<String, Column>>,
    /// A regular expression every string value must match.
    ///
    /// Held as its source text rather than a compiled `Regex` because a column
    /// is cloned, compared, and hashed, and a compiled automaton is none of
    /// those things. Compilation is checked once when the schema is validated,
    /// so an uncompilable pattern is a schema error rather than a surprise at
    /// row-validation time.
    pub pattern: Option<String>,
    /// Whether an object value may carry keys its `properties` do not declare.
    ///
    /// JSON Schema's own default is `true`. The root of a document has the same
    /// question answered by [`Schema::additional_fields`], which reports
    /// `ROW_UNKNOWN_FIELD` per key; here the answer belongs to the value, so it
    /// is part of whether the value matches its column at all.
    pub additional_properties: bool,
    /// Member names an object value must carry.
    ///
    /// Distinct from a declared column's own nullability, which answers whether
    /// THIS column may be absent from its parent. This answers which members
    /// the value itself must present, and it can name a member `properties`
    /// never declares -- which is the whole content of a composition
    /// alternative like `{"type": "object", "required": ["when_incorrect"]}`.
    ///
    /// Without it such an alternative decoded to a column with no properties,
    /// which matched every object: `oneOf` then counted every alternative
    /// satisfied and refused every row.
    pub required: BTreeSet<String>,
    /// Bounds on the size of a value: string length, array length, object size.
    ///
    /// JSON Schema states these as separate keywords per type -- `minLength`,
    /// `minItems`, `minProperties` -- but they are one question asked of three
    /// shapes, and which one applies is already decided by the column's type.
    /// Holding them as one pair keeps `matches_column` from growing three
    /// near-identical branches, and keeps a schema from declaring a string
    /// bound on an array.
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// Bounds on a numeric value. Exclusive bounds are separate fields rather
    /// than a flag, because JSON Schema 2020-12 states them as separate
    /// keywords and a column may carry one of each.
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub exclusive_minimum: Option<f64>,
    pub exclusive_maximum: Option<f64>,
    /// A number every value must be a multiple of. Must be strictly positive.
    pub multiple_of: Option<f64>,
    /// Whether an array's elements must be distinct, compared by their
    /// canonical rendering so that two equal values written differently still
    /// collide.
    pub unique_items: bool,
    /// Alternative subschemas a value must satisfy, beyond its declared type.
    ///
    /// Composition is a *constraint*, never the type itself: a column has one
    /// declared type, which SQL binding, canonical column order and doctor's
    /// coercions all depend on. `oneOf` narrows which values of that type are
    /// legal; it cannot make a column two types at once.
    pub composition: Option<Composition>,
    pub description: Option<String>,
    /// Standard JSON Schema annotations carried through untouched.
    ///
    /// These describe a schema without constraining an instance, so preserving
    /// them costs no semantics: a `$comment` a person wrote survives a load and
    /// a save. They take no part in validation and none in identity.
    pub annotations: IndexMap<String, Value>,
}

/// How a value must relate to a set of alternative subschemas.
///
/// JSON Schema spells four of these, and they differ only in how many
/// alternatives a value must satisfy. Holding the arity as a variant rather
/// than as four fields means `matches_column` asks one question, and a schema
/// cannot declare two composition modes on one column.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompositionKind {
    /// `oneOf`: exactly one alternative.
    One,
    /// `anyOf`: at least one.
    Any,
    /// `allOf`: every one.
    All,
    /// `not`: none. Carries exactly one alternative.
    Not,
}

/// Alternative subschemas, and how many of them a value must satisfy.
///
/// The alternatives are `Column`s because that is what a subschema decodes to,
/// and it makes composition recursive for free: an alternative may itself carry
/// a pattern, a bound, or a nested composition.
#[derive(Debug, Clone)]
pub struct Composition {
    pub kind: CompositionKind,
    pub alternatives: Vec<Column>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub table: String,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
    NoAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKey {
    /// The referencing columns.
    ///
    /// A name may carry a `[]` suffix -- `objective_refs[]` -- which addresses
    /// *each element* of an array column rather than the column's value. That
    /// is a different relationship from a composite key: a scalar key names one
    /// target row per row, while an element key names one target row per
    /// element. Both are spelled here because both are foreign keys; what
    /// changes is how many lookups a row performs, not what a lookup means.
    ///
    /// Element addressing and composite keys do not combine: an element key
    /// names exactly one column, because a tuple drawn from two arrays has no
    /// defined pairing.
    pub columns: Vec<String>,
    pub references: Reference,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_delete: Option<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_update: Option<Action>,
}
/// The column a foreign-key name addresses, and whether it addresses elements.
pub struct KeyColumn<'a> {
    pub name: &'a str,
    pub per_element: bool,
}

/// Split a foreign-key column name into the column it names and whether it
/// addresses that column's elements.
pub fn key_column(spelled: &str) -> KeyColumn<'_> {
    match spelled.strip_suffix("[]") {
        Some(name) => KeyColumn { name, per_element: true },
        None => KeyColumn { name: spelled, per_element: false },
    }
}

impl ForeignKey {
    /// Whether this key relates array elements rather than column values.
    pub fn is_per_element(&self) -> bool {
        self.columns.iter().any(|c| key_column(c).per_element)
    }

    pub fn delete_action(&self) -> Action {
        self.on_delete.unwrap_or(Action::Restrict)
    }
    pub fn update_action(&self) -> Action {
        self.on_update.unwrap_or(Action::Restrict)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    pub expr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    pub filename: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdditionalFields {
    Reject,
    Allow,
}

/// One table's relational model.
///
/// Not serializable for the same reason as [`Column`]: [`json_schema::encode`]
/// renders it and [`json_schema::decode`] reads it, and identity comes from
/// [`semantic::encode_v1`]. Three destinations, each explicit.
#[derive(Debug, Clone)]
pub struct Schema {
    pub table: String,
    pub schema_version: u32,
    pub schema_format: Option<u32>,
    pub description: Option<String>,
    pub primary_key: Vec<String>,
    pub columns: IndexMap<String, Column>,
    pub unique: Vec<Vec<String>>,
    pub foreign_keys: Vec<ForeignKey>,
    pub check: Vec<Check>,
    pub indexes: Vec<Vec<String>>,
    pub storage: Option<Storage>,
    pub additional_fields: AdditionalFields,
    /// Document-level annotations, preserved exactly as [`Column::annotations`].
    pub annotations: IndexMap<String, Value>,
}

impl Schema {
    pub fn filename_columns(&self) -> &[String] {
        self.storage
            .as_ref()
            .map(|s| s.filename.as_slice())
            .unwrap_or(&self.primary_key)
    }
    pub fn validate_local(&self, stem: &str) -> Vec<Diagnostic> {
        let mut out = vec![];
        if self
            .schema_format
            .is_some_and(|version| version != crate::FORMAT_VERSION)
        {
            out.push(Diagnostic::error(
                "FORMAT_UNSUPPORTED",
                format!(
                    "schema pins grammar format {}, but this binary supports {}",
                    self.schema_format.unwrap_or_default(),
                    crate::FORMAT_VERSION
                ),
            ));
        }
        if self.table != stem {
            out.push(Diagnostic::error(
                "SCHEMA_TABLE_NAME_MISMATCH",
                format!(
                    "schema table {:?} does not equal file stem {:?}",
                    self.table, stem
                ),
            ));
        }
        if !valid_name(&self.table) || self.table == "schema" {
            out.push(Diagnostic::error(
                "SCHEMA_INVALID_TABLE_NAME",
                format!("invalid table name {:?}", self.table),
            ));
        }
        if self.primary_key.is_empty() {
            out.push(Diagnostic::error(
                "SCHEMA_MISSING_REQUIRED",
                "x-reldir.primaryKey must be a non-empty array",
            ));
        }
        if self.primary_key.iter().collect::<BTreeSet<_>>().len() != self.primary_key.len() {
            out.push(Diagnostic::error(
                "SCHEMA_PK_COLUMN_UNKNOWN",
                "x-reldir.primaryKey must not repeat a column",
            ));
        }
        if self.columns.is_empty() {
            out.push(Diagnostic::error(
                "SCHEMA_MISSING_REQUIRED",
                "properties must declare at least one column",
            ));
        }
        let mut normalized_columns = BTreeSet::new();
        for name in self.columns.keys() {
            use unicode_normalization::UnicodeNormalization;
            if !normalized_columns.insert(name.nfc().collect::<String>()) {
                out.push(Diagnostic::error(
                    "SCHEMA_COLUMN_UNKNOWN",
                    "column names collide after NFC normalization",
                ));
            }
        }
        for key in &self.primary_key {
            match self.columns.get(key) {
                None => out.push(Diagnostic::error(
                    "SCHEMA_PK_COLUMN_UNKNOWN",
                    format!("primary key column {key:?} does not exist"),
                )),
                Some(c) if c.nullable => out.push(Diagnostic::error(
                    "SCHEMA_PK_NULLABLE",
                    format!("primary key column {key:?} cannot be nullable"),
                )),
                _ => {}
            }
        }
        for (name, col) in &self.columns {
            if !valid_column_name(name) {
                out.push(Diagnostic::error(
                    "SCHEMA_COLUMN_UNKNOWN",
                    format!("invalid column name {name:?}"),
                ));
            }
            validate_column(&self.table, name, col, &mut out);
        }
        for cols in self
            .unique
            .iter()
            .chain(self.indexes.iter())
            .chain(std::iter::once(&self.filename_columns().to_vec()))
        {
            if cols.is_empty() {
                out.push(Diagnostic::error(
                    "SCHEMA_COLUMN_UNKNOWN",
                    "constraint column list cannot be empty",
                ));
            }
            if cols.iter().collect::<BTreeSet<_>>().len() != cols.len() {
                out.push(Diagnostic::error(
                    "SCHEMA_COLUMN_UNKNOWN",
                    "constraint column list must not repeat a column",
                ));
            }
            for c in cols {
                if !self.columns.contains_key(c) {
                    out.push(Diagnostic::error(
                        "SCHEMA_COLUMN_UNKNOWN",
                        format!("constraint names unknown column {c:?}"),
                    ));
                }
            }
        }
        let filename = self.filename_columns();
        let unique = filename == self.primary_key || self.unique.iter().any(|u| u == filename);
        if !unique
            || filename
                .iter()
                .any(|c| self.columns.get(c).is_some_and(|x| x.nullable))
        {
            out.push(Diagnostic::error(
                "SCHEMA_FILENAME_NOT_UNIQUE",
                "x-reldir.filename must be a NOT NULL primary key or unique constraint",
            ));
        }
        let mut check_names = BTreeSet::new();
        for c in &self.check {
            if c.name.is_empty() || !check_names.insert(&c.name) || c.expr.trim().is_empty() {
                out.push(Diagnostic::error(
                    "SCHEMA_CHECK_INVALID",
                    format!("invalid check constraint {:?}", c.name),
                ));
            } else if !crate::sql::check_expression_is_boolean(self, &c.expr) {
                out.push(Diagnostic::error(
                    "SCHEMA_CHECK_INVALID",
                    format!("check {:?} is not a boolean SQL expression", c.name),
                ));
            }
        }
        out
    }
}

fn validate_column(table: &str, name: &str, c: &Column, out: &mut Vec<Diagnostic>) {
    if let Some(g) = &c.generated {
        let valid = matches!(
            (&g.kind, &c.kind),
            (GeneratedKind::Uuid, ColumnType::Uuid)
                | (GeneratedKind::Ulid, ColumnType::Ulid)
                | (GeneratedKind::Now, ColumnType::Timestamp)
                | (GeneratedKind::Sequence, ColumnType::Int)
        );
        if !valid {
            out.push(Diagnostic::error(
                "SCHEMA_DEFAULT_TYPE_MISMATCH",
                format!("generated kind for {table}.{name} is incompatible with its column type"),
            ));
        }
        if c.default.is_some() {
            out.push(Diagnostic::error(
                "SCHEMA_DEFAULT_TYPE_MISMATCH",
                format!("{table}.{name} cannot declare both default and generated"),
            ));
        }
    }
    if c.kind == ColumnType::Enum && c.values.as_ref().is_none_or(|v| v.is_empty()) {
        out.push(Diagnostic::error(
            "SCHEMA_MISSING_REQUIRED",
            format!("{table}.{name}: enum requires non-empty values"),
        ));
    }
    if c.kind != ColumnType::Enum && c.values.is_some() {
        out.push(Diagnostic::error(
            "SCHEMA_UNKNOWN_KEY",
            format!("{table}.{name}: enum is only valid for string columns"),
        ));
    }
    if let Some(values) = &c.values {
        let set: BTreeSet<_> = values.iter().collect();
        if set.len() != values.len() {
            out.push(Diagnostic::error(
                "SCHEMA_DEFAULT_TYPE_MISMATCH",
                format!("{table}.{name}: enum values must be distinct"),
            ));
        }
    }
    if c.kind == ColumnType::Array && c.items.is_none() {
        out.push(Diagnostic::error(
            "SCHEMA_MISSING_REQUIRED",
            format!("{table}.{name}: array requires items"),
        ));
    }
    if c.kind != ColumnType::Array && c.items.is_some() {
        out.push(Diagnostic::error(
            "SCHEMA_UNKNOWN_KEY",
            format!("{table}.{name}: items is only valid for array columns"),
        ));
    }
    if c.kind != ColumnType::Object && c.properties.is_some() {
        out.push(Diagnostic::error(
            "SCHEMA_UNKNOWN_KEY",
            format!("{table}.{name}: properties is only valid for object columns"),
        ));
    }
    // A pattern that does not compile is a malformed schema, caught once here
    // rather than per row. reldir matches with the `regex` crate, whose syntax is
    // ECMA-262 without backreferences or lookaround; a pattern using those is
    // refused by name instead of silently never matching.
    if let Some(pattern) = &c.pattern {
        if c.kind != ColumnType::String && c.kind != ColumnType::Enum {
            out.push(Diagnostic::error(
                "SCHEMA_UNKNOWN_KEY",
                format!("{table}.{name}: pattern is only valid for string columns"),
            ));
        }
        if let Err(error) = regex::Regex::new(pattern) {
            out.push(Diagnostic::error(
                "SCHEMA_CHECK_INVALID",
                format!("{table}.{name}: pattern {pattern:?} is not a valid regular expression: {error}"),
            ));
        }
    }
    if !c.additional_properties && c.kind != ColumnType::Object {
        out.push(Diagnostic::error(
            "SCHEMA_UNKNOWN_KEY",
            format!("{table}.{name}: additionalProperties is only valid for object columns"),
        ));
    }
    // A bound stated for a type it cannot describe is a schema that means
    // nothing: `minLength` on a boolean constrains no value reldir will ever
    // see. Refusing it here is the same rule `items` and `properties` follow.
    let sized = matches!(
        c.kind,
        ColumnType::String
            | ColumnType::Enum
            | ColumnType::Decimal
            | ColumnType::Bytes
            | ColumnType::Array
            | ColumnType::Object
    );
    if (c.min_size.is_some() || c.max_size.is_some()) && !sized {
        out.push(Diagnostic::error(
            "SCHEMA_UNKNOWN_KEY",
            format!(
                "{table}.{name}: a size bound is only valid for string, array, or object columns"
            ),
        ));
    }
    if let (Some(low), Some(high)) = (c.min_size, c.max_size)
        && low > high
    {
        out.push(Diagnostic::error(
            "SCHEMA_BOUND_INVALID",
            format!("{table}.{name}: minimum size {low} exceeds maximum size {high}"),
        ));
    }
    let numeric = matches!(c.kind, ColumnType::Int | ColumnType::Float);
    let has_numeric_bound = c.minimum.is_some()
        || c.maximum.is_some()
        || c.exclusive_minimum.is_some()
        || c.exclusive_maximum.is_some()
        || c.multiple_of.is_some();
    if has_numeric_bound && !numeric {
        out.push(Diagnostic::error(
            "SCHEMA_UNKNOWN_KEY",
            format!("{table}.{name}: a numeric bound is only valid for int or float columns"),
        ));
    }
    if let (Some(low), Some(high)) = (c.minimum, c.maximum)
        && low > high
    {
        out.push(Diagnostic::error(
            "SCHEMA_BOUND_INVALID",
            format!("{table}.{name}: minimum {low} exceeds maximum {high}"),
        ));
    }
    if c.multiple_of.is_some_and(|divisor| divisor <= 0.0) {
        out.push(Diagnostic::error(
            "SCHEMA_BOUND_INVALID",
            format!("{table}.{name}: multipleOf must be greater than zero"),
        ));
    }
    if !c.required.is_empty() && c.kind != ColumnType::Object {
        out.push(Diagnostic::error(
            "SCHEMA_UNKNOWN_KEY",
            format!("{table}.{name}: required is only valid for object columns"),
        ));
    }
    if c.unique_items && c.kind != ColumnType::Array {
        out.push(Diagnostic::error(
            "SCHEMA_UNKNOWN_KEY",
            format!("{table}.{name}: uniqueItems is only valid for array columns"),
        ));
    }
    // An alternative is a column, so it is held to every rule a column is --
    // including this one, which is what makes a nested composition legal.
    if let Some(composition) = &c.composition {
        for (index, alternative) in composition.alternatives.iter().enumerate() {
            validate_column(table, &format!("{name}/{index}"), alternative, out);
        }
    }
    if let Some(default) = &c.default
        && !crate::value::matches_column(default, c)
    {
        out.push(Diagnostic::error(
            "SCHEMA_DEFAULT_TYPE_MISMATCH",
            format!("default for {table}.{name} does not match its type"),
        ));
    }
    if let Some(items) = &c.items {
        validate_column(table, &format!("{name}[]"), items, out);
    }
    if let Some(props) = &c.properties {
        let mut normalized = BTreeSet::new();
        for (n, p) in props {
            use unicode_normalization::UnicodeNormalization;
            if !valid_column_name(n) {
                out.push(Diagnostic::error(
                    "SCHEMA_COLUMN_UNKNOWN",
                    format!("invalid nested column name {n:?} in {table}.{name}"),
                ));
            }
            if !normalized.insert(n.nfc().collect::<String>()) {
                out.push(Diagnostic::error(
                    "SCHEMA_COLUMN_UNKNOWN",
                    format!("{table}.{name} property names collide after NFC normalization"),
                ));
            }
            validate_column(table, &format!("{name}.{n}"), p, out);
        }
    }
}

pub fn valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !s.starts_with('.')
        && !matches!(
            s,
            "con"
                | "prn"
                | "aux"
                | "nul"
                | "com1"
                | "com2"
                | "com3"
                | "com4"
                | "com5"
                | "com6"
                | "com7"
                | "com8"
                | "com9"
                | "lpt1"
                | "lpt2"
                | "lpt3"
                | "lpt4"
                | "lpt5"
                | "lpt6"
                | "lpt7"
                | "lpt8"
                | "lpt9"
        )
}
fn valid_column_name(s: &str) -> bool {
    !s.is_empty() && !s.contains('\0')
}

/// Read a schema from bytes already in hand.
///
/// The same two steps as [`load`], for callers that have the document rather
/// than a path -- so there is still exactly one way a schema is parsed.
pub fn load_bytes(data: &[u8]) -> Result<Schema> {
    let document = crate::json::parse(data).map_err(|e| {
        DbError::from_diag(Diagnostic::error("SCHEMA_INVALID_JSON", e.to_string()), 2)
    })?;
    json_schema::decode(&document)
}

/// Read a schema file.
///
/// The only way a schema enters the process. Files are JSON Schema documents in
/// reldir's dialect, so parsing is two steps -- JSON, then the dialect -- and each
/// reports its own faults at the place they occur.
pub fn load(path: &Path) -> Result<Schema> {
    let data = fs::read(path).map_err(|e| DbError::io(path, e))?;
    let document = crate::json::parse(&data).map_err(|e| {
        let mut diagnostic = Diagnostic::error("SCHEMA_INVALID_JSON", e.to_string()).at(path);
        diagnostic.location = Some(crate::diagnostic::Location {
            line: e.line(),
            column: e.column(),
        });
        diagnostic.source_line = std::str::from_utf8(&data)
            .ok()
            .and_then(|text| text.lines().nth(e.line().saturating_sub(1)))
            .map(String::from);
        DbError::from_diag(diagnostic, 2)
    })?;
    json_schema::decode(&document).map_err(|error| {
        // The decoder knows what is wrong but not which file it was reading, so
        // the path is attached here where it is known.
        let mut diagnostic = *error.diagnostic;
        if diagnostic.path.is_none() {
            diagnostic = diagnostic.at(path);
        }
        // The decoder names the key at fault; the file is what has line
        // numbers. Resolving one against the other is what lets a schema error
        // point at the declaration a reader has to edit.
        if diagnostic.location.is_none()
            && let Some(anchor) = diagnostic.field.clone()
        {
            diagnostic.location = crate::integrity::locate(&data, &anchor);
        }
        diagnostic.source_line = diagnostic
            .location
            .as_ref()
            .and_then(|location| std::str::from_utf8(&data).ok().map(|t| (t, location)))
            .and_then(|(text, location)| text.lines().nth(location.line.saturating_sub(1)))
            .map(String::from);
        DbError::from_diag(diagnostic, 2)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn column(kind: ColumnType) -> Column {
        Column {
            kind,
            nullable: false,
            default: None,
            generated: None,
            values: None,
            items: None,
            properties: None,
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

    fn base(columns: &[(&str, Column)], primary_key: &[&str]) -> Schema {
        let mut map = IndexMap::new();
        for (name, c) in columns {
            map.insert((*name).to_string(), c.clone());
        }
        Schema {
            table: "t".into(),
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

    fn codes(schema: &Schema) -> Vec<String> {
        schema
            .validate_local("t")
            .into_iter()
            .map(|d| d.code)
            .collect()
    }

    /// A pattern reldir cannot compile is a malformed schema, not a constraint
    /// that silently never matches.
    ///
    /// The failure has to land when the schema is validated. Deferring it to
    /// row validation would report every row of the table as mismatched, naming
    /// the data rather than the one thing that is actually wrong; and treating
    /// an uncompilable pattern as vacuously satisfied would accept a document
    /// that claims a constraint the database does not apply, which is the
    /// defect this whole keyword was added to end.
    #[test]
    fn test1150_an_uncompilable_pattern_is_refused_when_the_schema_is_checked() {
        let mut broken = column(ColumnType::String);
        // A backreference: valid ECMA-262, outside what `regex` compiles.
        broken.pattern = Some(r"(a)\1".into());
        let schema = base(
            &[("id", column(ColumnType::String)), ("v", broken)],
            &["id"],
        );
        let found = codes(&schema);
        assert!(
            found.contains(&"SCHEMA_CHECK_INVALID".to_string()),
            "an uncompilable pattern must be refused by name, got {found:?}"
        );
        let message = schema
            .validate_local("t")
            .into_iter()
            .find(|d| d.code == "SCHEMA_CHECK_INVALID")
            .expect("the diagnostic exists")
            .message;
        assert!(
            message.contains("v"),
            "the message must name the column: {message}"
        );

        // A compilable one is accepted, so the check discriminates rather than
        // refusing patterns as a class.
        let mut fine = column(ColumnType::String);
        fine.pattern = Some("^[a-z]+$".into());
        assert!(
            !codes(&base(
                &[("id", column(ColumnType::String)), ("v", fine)],
                &["id"]
            ))
            .contains(&"SCHEMA_CHECK_INVALID".to_string()),
            "a valid pattern must not be refused"
        );
    }

    /// A member meaningless for its column's type is refused, as `items` and
    /// `properties` already are. A pattern cannot constrain a boolean, and a
    /// document that states one is asking for something reldir will not do.
    #[test]
    fn test1151_type_specific_members_include_the_new_ones() {
        let mut patterned_bool = column(ColumnType::Bool);
        patterned_bool.pattern = Some("^x$".into());
        assert!(
            codes(&base(
                &[("id", column(ColumnType::String)), ("v", patterned_bool)],
                &["id"]
            ))
            .contains(&"SCHEMA_UNKNOWN_KEY".to_string()),
            "pattern on a non-string column must be refused"
        );

        let mut closed_string = column(ColumnType::String);
        closed_string.additional_properties = false;
        assert!(
            codes(&base(
                &[("id", column(ColumnType::String)), ("v", closed_string)],
                &["id"]
            ))
            .contains(&"SCHEMA_UNKNOWN_KEY".to_string()),
            "additionalProperties on a non-object column must be refused"
        );

        // An enum is a string with a fixed set, so a pattern over it is
        // meaningful and must not be refused.
        let mut patterned_enum = column(ColumnType::Enum);
        patterned_enum.values = Some(vec!["aa".into(), "ab".into()]);
        patterned_enum.pattern = Some("^a".into());
        assert!(
            !codes(&base(
                &[("id", column(ColumnType::String)), ("v", patterned_enum)],
                &["id"]
            ))
            .contains(&"SCHEMA_UNKNOWN_KEY".to_string()),
            "a pattern over an enum's strings is meaningful"
        );
    }

    /// Section 9: table names are a restricted lowercase identifier, must not be
    /// `schema`, and must avoid Windows reserved device names so that every
    /// governed directory is portable.
    #[test]
    fn test1077_table_names_are_restricted_and_portable() {
        for accepted in ["users", "u", "a_b", "t1", "order_items"] {
            assert!(valid_name(accepted), "{accepted} should be a valid name");
        }
        for rejected in [
            "",
            "Users",
            "1users",
            "_users",
            "user-items",
            "user.items",
            "üsers",
            "con",
            "prn",
            "aux",
            "nul",
            "com1",
            "lpt9",
            ".hidden",
        ] {
            assert!(!valid_name(rejected), "{rejected:?} must be rejected");
        }
        // `schema` is a reserved directory even though it is a valid identifier.
        let mut s = base(&[("id", column(ColumnType::String))], &["id"]);
        s.table = "schema".into();
        assert!(
            s.validate_local("schema")
                .iter()
                .any(|d| d.code == "SCHEMA_INVALID_TABLE_NAME")
        );
    }

    /// Section 11: the schema's `table` must equal its file stem, so a renamed
    /// schema file cannot silently govern a different relation.
    #[test]
    fn test1078_table_must_equal_the_file_stem() {
        let s = base(&[("id", column(ColumnType::String))], &["id"]);
        assert!(
            s.validate_local("t")
                .iter()
                .all(|d| d.code != "SCHEMA_TABLE_NAME_MISMATCH")
        );
        assert!(
            s.validate_local("other")
                .iter()
                .any(|d| d.code == "SCHEMA_TABLE_NAME_MISMATCH")
        );
    }

    /// Section 11: a schema may pin its grammar version; an unsupported pin is a
    /// format error rather than a best-effort interpretation.
    #[test]
    fn test1079_a_pinned_unsupported_grammar_is_refused() {
        let mut s = base(&[("id", column(ColumnType::String))], &["id"]);
        s.schema_format = Some(crate::FORMAT_VERSION);
        assert!(!codes(&s).contains(&"FORMAT_UNSUPPORTED".to_string()));
        s.schema_format = Some(crate::FORMAT_VERSION + 1);
        assert!(codes(&s).contains(&"FORMAT_UNSUPPORTED".to_string()));
    }

    /// Section 11: required elements are required, and the primary key must name
    /// real, non-nullable, non-repeating columns.
    #[test]
    fn test1080_primary_keys_must_name_real_non_nullable_columns() {
        let mut missing = base(&[("id", column(ColumnType::String))], &["ghost"]);
        assert!(codes(&missing).contains(&"SCHEMA_PK_COLUMN_UNKNOWN".to_string()));

        missing.primary_key = vec![];
        assert!(codes(&missing).contains(&"SCHEMA_MISSING_REQUIRED".to_string()));

        let repeated = base(&[("id", column(ColumnType::String))], &["id", "id"]);
        assert!(codes(&repeated).contains(&"SCHEMA_PK_COLUMN_UNKNOWN".to_string()));

        let mut nullable_column = column(ColumnType::String);
        nullable_column.nullable = true;
        let nullable = base(&[("id", nullable_column)], &["id"]);
        assert!(codes(&nullable).contains(&"SCHEMA_PK_NULLABLE".to_string()));

        let empty = base(&[], &["id"]);
        assert!(codes(&empty).contains(&"SCHEMA_MISSING_REQUIRED".to_string()));
    }

    /// Section 10: storage.filename must identify rows uniquely and never be
    /// nullable, otherwise two rows could claim one path.
    #[test]
    fn test1081_filename_columns_must_be_unique_and_not_null() {
        let mut s = base(
            &[
                ("id", column(ColumnType::String)),
                ("slug", column(ColumnType::String)),
            ],
            &["id"],
        );
        // A non-unique column cannot name files.
        s.storage = Some(Storage {
            filename: vec!["slug".into()],
        });
        assert!(codes(&s).contains(&"SCHEMA_FILENAME_NOT_UNIQUE".to_string()));

        // Declaring it unique makes it a legitimate filename key.
        s.unique = vec![vec!["slug".into()]];
        assert!(!codes(&s).contains(&"SCHEMA_FILENAME_NOT_UNIQUE".to_string()));

        // A nullable filename column is refused even when unique.
        s.columns.get_mut("slug").unwrap().nullable = true;
        assert!(codes(&s).contains(&"SCHEMA_FILENAME_NOT_UNIQUE".to_string()));
    }

    /// Section 11: every constraint column list must be non-empty, free of
    /// repeats, and name declared columns.
    #[test]
    fn test1082_constraint_column_lists_are_validated() {
        let mut s = base(&[("id", column(ColumnType::String))], &["id"]);
        s.unique = vec![vec![]];
        assert!(codes(&s).contains(&"SCHEMA_COLUMN_UNKNOWN".to_string()));

        s.unique = vec![vec!["id".into(), "id".into()]];
        assert!(codes(&s).contains(&"SCHEMA_COLUMN_UNKNOWN".to_string()));

        s.unique = vec![];
        s.indexes = vec![vec!["ghost".into()]];
        assert!(codes(&s).contains(&"SCHEMA_COLUMN_UNKNOWN".to_string()));
    }

    /// Section 11: type-specific members belong only to their own type, and a
    /// declared default must match the column it defaults.
    #[test]
    fn test1083_type_specific_members_are_strict() {
        // enum requires values; values are meaningless elsewhere.
        let mut enumeration = column(ColumnType::Enum);
        let s = base(
            &[
                ("id", column(ColumnType::String)),
                ("e", enumeration.clone()),
            ],
            &["id"],
        );
        assert!(codes(&s).contains(&"SCHEMA_MISSING_REQUIRED".to_string()));

        enumeration.values = Some(vec!["a".into(), "a".into()]);
        let s = base(
            &[
                ("id", column(ColumnType::String)),
                ("e", enumeration.clone()),
            ],
            &["id"],
        );
        assert!(codes(&s).contains(&"SCHEMA_DEFAULT_TYPE_MISMATCH".to_string()));

        let mut misplaced = column(ColumnType::String);
        misplaced.values = Some(vec!["a".into()]);
        let s = base(
            &[("id", column(ColumnType::String)), ("v", misplaced)],
            &["id"],
        );
        assert!(codes(&s).contains(&"SCHEMA_UNKNOWN_KEY".to_string()));

        // array requires items; items are meaningless elsewhere.
        let s = base(
            &[
                ("id", column(ColumnType::String)),
                ("a", column(ColumnType::Array)),
            ],
            &["id"],
        );
        assert!(codes(&s).contains(&"SCHEMA_MISSING_REQUIRED".to_string()));

        let mut with_items = column(ColumnType::String);
        with_items.items = Some(Box::new(column(ColumnType::Int)));
        let s = base(
            &[("id", column(ColumnType::String)), ("v", with_items)],
            &["id"],
        );
        assert!(codes(&s).contains(&"SCHEMA_UNKNOWN_KEY".to_string()));

        // a default must satisfy its own column type.
        let mut wrong_default = column(ColumnType::Int);
        wrong_default.default = Some(json!("text"));
        let s = base(
            &[("id", column(ColumnType::String)), ("n", wrong_default)],
            &["id"],
        );
        assert!(codes(&s).contains(&"SCHEMA_DEFAULT_TYPE_MISMATCH".to_string()));
    }

    /// Section 11: a generated column must be generated in a way its type can
    /// represent, and cannot also carry a default.
    #[test]
    fn test1084_generated_columns_match_their_type() {
        for (kind, generated, valid) in [
            (ColumnType::Uuid, GeneratedKind::Uuid, true),
            (ColumnType::Ulid, GeneratedKind::Ulid, true),
            (ColumnType::Timestamp, GeneratedKind::Now, true),
            (ColumnType::Int, GeneratedKind::Sequence, true),
            (ColumnType::String, GeneratedKind::Uuid, false),
            (ColumnType::Int, GeneratedKind::Now, false),
        ] {
            let mut c = column(kind);
            c.generated = Some(Generated { kind: generated });
            let s = base(&[("id", column(ColumnType::String)), ("g", c)], &["id"]);
            let mismatched = codes(&s).contains(&"SCHEMA_DEFAULT_TYPE_MISMATCH".to_string());
            assert_eq!(!mismatched, valid, "unexpected result for {s:?}");
        }

        let mut both = column(ColumnType::Uuid);
        both.generated = Some(Generated {
            kind: GeneratedKind::Uuid,
        });
        both.default = Some(json!("0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e"));
        let s = base(&[("id", column(ColumnType::String)), ("g", both)], &["id"]);
        assert!(codes(&s).contains(&"SCHEMA_DEFAULT_TYPE_MISMATCH".to_string()));
    }

    /// Section 11: nested column definitions are validated recursively, so a
    /// fault inside an array's items or an object's properties is still caught.
    #[test]
    fn test1085_nested_column_definitions_are_validated_recursively() {
        // An array whose items are an enum without values.
        let mut items = column(ColumnType::Enum);
        items.values = None;
        let mut array = column(ColumnType::Array);
        array.items = Some(Box::new(items));
        let s = base(&[("id", column(ColumnType::String)), ("a", array)], &["id"]);
        assert!(codes(&s).contains(&"SCHEMA_MISSING_REQUIRED".to_string()));

        // An object whose property carries a mistyped default.
        let mut property = column(ColumnType::Int);
        property.default = Some(json!("text"));
        let mut properties = IndexMap::new();
        properties.insert("n".to_string(), property);
        let mut object = column(ColumnType::Object);
        object.properties = Some(properties);
        let s = base(
            &[("id", column(ColumnType::String)), ("o", object)],
            &["id"],
        );
        assert!(codes(&s).contains(&"SCHEMA_DEFAULT_TYPE_MISMATCH".to_string()));
    }

    /// Section 11: check constraints need a name and a non-empty expression, and
    /// names must be distinct within a table.
    #[test]
    fn test1086_check_constraints_require_distinct_names_and_expressions() {
        let mut s = base(&[("id", column(ColumnType::String))], &["id"]);
        s.check = vec![Check {
            name: String::new(),
            expr: "1=1".into(),
        }];
        assert!(codes(&s).contains(&"SCHEMA_CHECK_INVALID".to_string()));

        s.check = vec![Check {
            name: "c".into(),
            expr: "   ".into(),
        }];
        assert!(codes(&s).contains(&"SCHEMA_CHECK_INVALID".to_string()));

        s.check = vec![
            Check {
                name: "dup".into(),
                expr: "id <> ''".into(),
            },
            Check {
                name: "dup".into(),
                expr: "id <> 'x'".into(),
            },
        ];
        assert!(codes(&s).contains(&"SCHEMA_CHECK_INVALID".to_string()));
    }

    /// A schema exercising many features at once must validate cleanly, so the
    /// rules above reject faults rather than well-formed schemas.
    #[test]
    fn test1087_a_fully_featured_valid_schema_reports_nothing() {
        let mut id = column(ColumnType::Uuid);
        id.generated = Some(Generated {
            kind: GeneratedKind::Uuid,
        });
        let mut role = column(ColumnType::Enum);
        role.values = Some(vec!["admin".into(), "member".into()]);
        role.default = Some(json!("member"));
        let mut tags = column(ColumnType::Array);
        tags.items = Some(Box::new(column(ColumnType::String)));
        let mut email = column(ColumnType::String);
        email.description = Some("contact address".into());

        let mut s = base(
            &[("id", id), ("email", email), ("role", role), ("tags", tags)],
            &["id"],
        );
        s.unique = vec![vec!["email".into()]];
        s.indexes = vec![vec!["role".into()]];
        s.check = vec![Check {
            name: "email_has_at".into(),
            expr: "email LIKE '%@%'".into(),
        }];
        assert!(
            s.validate_local("t").is_empty(),
            "unexpected findings: {:?}",
            codes(&s)
        );
    }
}
