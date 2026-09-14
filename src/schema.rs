use crate::diagnostic::{DbError, Diagnostic, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    #[serde(rename = "type")]
    pub kind: ColumnType,
    #[serde(default)]
    pub nullable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated: Option<Generated>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<Column>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<IndexMap<String, Column>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
    pub columns: Vec<String>,
    pub references: Reference,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_delete: Option<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_update: Option<Action>,
}
impl ForeignKey {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inferred {
    pub at: String,
    pub rows: usize,
    pub strictness: String,
    #[serde(default)]
    pub evidence: BTreeMap<String, String>,
}

fn one() -> u32 {
    1
}
fn reject() -> AdditionalFields {
    AdditionalFields::Reject
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdditionalFields {
    Reject,
    Allow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schema {
    pub table: String,
    #[serde(default = "one")]
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_format: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub primary_key: Vec<String>,
    pub columns: IndexMap<String, Column>,
    #[serde(default)]
    pub unique: Vec<Vec<String>>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKey>,
    #[serde(default)]
    pub check: Vec<Check>,
    #[serde(default)]
    pub indexes: Vec<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<Storage>,
    #[serde(default = "reject")]
    pub additional_fields: AdditionalFields,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inferred: Option<Inferred>,
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
                "primary_key must be a non-empty array",
            ));
        }
        if self.primary_key.iter().collect::<BTreeSet<_>>().len() != self.primary_key.len() {
            out.push(Diagnostic::error(
                "SCHEMA_PK_COLUMN_UNKNOWN",
                "primary_key must not repeat a column",
            ));
        }
        if self.columns.is_empty() {
            out.push(Diagnostic::error(
                "SCHEMA_MISSING_REQUIRED",
                "columns must be a non-empty object",
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
                "storage.filename must be a NOT NULL primary key or unique constraint",
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
            format!("{table}.{name}: values is only valid for enum columns"),
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

pub fn load(path: &Path) -> Result<Schema> {
    let data = fs::read(path).map_err(|e| DbError::io(path, e))?;
    crate::json::parse(&data).map_err(|e| {
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
    let mut de = serde_json::Deserializer::from_slice(&data);
    let schema: Schema = serde::Deserialize::deserialize(&mut de).map_err(|e| {
        let text = e.to_string();
        let code = if text.contains("unknown field") {
            "SCHEMA_UNKNOWN_KEY"
        } else if text.contains("missing field `type`") {
            "SCHEMA_COLUMN_TYPE_MISSING"
        } else if text.contains("unknown variant") {
            "SCHEMA_TYPE_UNKNOWN"
        } else if text.contains("missing field") {
            "SCHEMA_MISSING_REQUIRED"
        } else {
            "SCHEMA_INVALID_JSON"
        };
        let message = if code == "SCHEMA_UNKNOWN_KEY" {
            nearest_key_message(&text)
        } else if code == "SCHEMA_MISSING_REQUIRED" {
            missing_key_message(&text)
        } else if code == "SCHEMA_COLUMN_TYPE_MISSING" {
            format!("{text}; type is required for validation, comparison, and canonical hashing")
        } else {
            text
        };
        let mut d = Diagnostic::error(code, message).at(path);
        d.location = Some(crate::diagnostic::Location {
            line: e.line(),
            column: e.column(),
        });
        d.source_line = std::str::from_utf8(&data)
            .ok()
            .and_then(|s| s.lines().nth(e.line().saturating_sub(1)))
            .map(String::from);
        DbError::from_diag(d, 2)
    })?;
    Ok(schema)
}

fn nearest_key_message(message: &str) -> String {
    const KEYS: &[&str] = &[
        "table",
        "schema_version",
        "schema_format",
        "description",
        "primary_key",
        "columns",
        "unique",
        "foreign_keys",
        "check",
        "indexes",
        "storage",
        "additional_fields",
        "inferred",
        "type",
        "nullable",
        "default",
        "generated",
        "values",
        "items",
        "properties",
        "on_delete",
        "on_update",
        "references",
        "filename",
    ];
    let Some(unknown) = message
        .split_once("unknown field `")
        .and_then(|(_, rest)| rest.split_once('`').map(|(field, _)| field))
    else {
        return message.into();
    };
    let nearest = KEYS
        .iter()
        .min_by_key(|candidate| strsim::levenshtein(unknown, candidate));
    match nearest {
        Some(nearest) => format!("{message}; nearest valid key is {nearest:?}"),
        None => message.into(),
    }
}

fn missing_key_message(message: &str) -> String {
    if message.contains("`table`") {
        format!("{message}; table is required to identify the relation")
    } else if message.contains("`primary_key`") {
        format!("{message}; primary_key is required for row identity and keyed operations")
    } else if message.contains("`columns`") {
        format!("{message}; columns is required to define relational attributes")
    } else {
        message.into()
    }
}
