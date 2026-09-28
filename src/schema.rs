//! A table's schema: one JSON Schema document, and the relational facts read
//! from it.
//!
//! The document is the only statement of the schema. [`Schema`] is a view of
//! it: every field is derived when the document is read, none can be assigned,
//! and the only way to change a schema is to edit its document
//! ([`document::Editor`]) and read the result again. There is therefore no
//! second representation that could drift from the file -- the defect that
//! let reldir enforce something other than what a pinned schema said.

pub mod document;
pub mod identity;
pub mod meta;
pub mod path;
pub mod row;

use crate::diagnostic::Diagnostic;
use indexmap::IndexMap;
use serde_json::Value;
use std::{collections::BTreeSet, sync::Arc};

pub use path::RefPath;

/// The relational type of a column, which decides how its values are stored,
/// compared and rendered.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

impl ColumnType {
    /// The name used on the command line and in diagnostics.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int => "int",
            Self::Float => "float",
            Self::Decimal => "decimal",
            Self::String => "string",
            Self::Bytes => "bytes",
            Self::Date => "date",
            Self::Timestamp => "timestamp",
            Self::Uuid => "uuid",
            Self::Ulid => "ulid",
            Self::Enum => "enum",
            Self::Array => "array",
            Self::Object => "object",
            Self::Json => "json",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "bool" => Self::Bool,
            "int" => Self::Int,
            "float" => Self::Float,
            "decimal" => Self::Decimal,
            "string" => Self::String,
            "bytes" => Self::Bytes,
            "date" => Self::Date,
            "timestamp" => Self::Timestamp,
            "uuid" => Self::Uuid,
            "ulid" => Self::Ulid,
            "enum" => Self::Enum,
            "array" => Self::Array,
            "object" => Self::Object,
            "json" => Self::Json,
            _ => return None,
        })
    }

    /// Whether values of this type are single scalars, which is what a key or a
    /// reference can be made of.
    pub fn is_scalar(&self) -> bool {
        !matches!(self, Self::Array | Self::Object | Self::Json)
    }
}

/// How a column's value is produced when a row omits it on insert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratedKind {
    Uuid,
    Ulid,
    Now,
    Sequence,
}

impl GeneratedKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Uuid => "uuid",
            Self::Ulid => "ulid",
            Self::Now => "now",
            Self::Sequence => "sequence",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "uuid" => Self::Uuid,
            "ulid" => Self::Ulid,
            "now" => Self::Now,
            "sequence" => Self::Sequence,
            _ => return None,
        })
    }

    /// The column type a generator can fill.
    pub fn column_type(self) -> ColumnType {
        match self {
            Self::Uuid => ColumnType::Uuid,
            Self::Ulid => ColumnType::Ulid,
            Self::Now => ColumnType::Timestamp,
            Self::Sequence => ColumnType::Int,
        }
    }
}

/// A column, as its subschema describes it. Read-only: see the module docs.
#[derive(Debug, Clone)]
pub struct Column {
    kind: ColumnType,
    nullable: bool,
    required: bool,
    default: Option<Value>,
    generated: Option<GeneratedKind>,
    values: Option<Vec<String>>,
    items: Option<Box<Column>>,
    properties: Option<IndexMap<String, Column>>,
    description: Option<String>,
}

impl Column {
    pub fn kind(&self) -> &ColumnType {
        &self.kind
    }
    /// Whether the column admits null as a value.
    pub fn nullable(&self) -> bool {
        self.nullable
    }
    /// Whether a row must carry the member, as the parent's `required` says.
    pub fn required(&self) -> bool {
        self.required
    }
    pub fn default(&self) -> Option<&Value> {
        self.default.as_ref()
    }
    pub fn generated(&self) -> Option<GeneratedKind> {
        self.generated
    }
    /// The string members of an `enum` column.
    pub fn values(&self) -> Option<&[String]> {
        self.values.as_deref()
    }
    /// The element column of an array, when the subschema declares one.
    pub fn items(&self) -> Option<&Column> {
        self.items.as_deref()
    }
    /// The declared members of an object column.
    pub fn properties(&self) -> Option<&IndexMap<String, Column>> {
        self.properties.as_ref()
    }
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
}

/// What a foreign key points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Rows of one table.
    Table(String),
    /// Rows of any of several tables.
    Tables(Vec<String>),
    /// Rows of any table in an identity domain.
    Domain(String),
}

impl Target {
    pub fn describe(&self) -> String {
        match self {
            Self::Table(table) => table.clone(),
            Self::Tables(tables) => format!("one of {}", tables.join(", ")),
            Self::Domain(domain) => format!("domain {domain}"),
        }
    }
}

/// What happens to a reference when the row it names is deleted, or when that
/// row's key changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Refuse the change.
    Restrict,
    /// Refuse the change; the same as `restrict`, because reldir checks every
    /// reference when the statement completes.
    NoAction,
    /// Delete the referencing row (on delete), or carry the new key into the
    /// reference (on update).
    Cascade,
    /// Remove the reference: the array element that holds it, or -- where no
    /// array was crossed -- the value, which must then be nullable.
    Remove,
    /// Replace the reference with null.
    SetNull,
    /// Replace the reference with the column's declared default.
    SetDefault,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Self::Restrict => "restrict",
            Self::NoAction => "no_action",
            Self::Cascade => "cascade",
            Self::Remove => "remove",
            Self::SetNull => "set_null",
            Self::SetDefault => "set_default",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "restrict" => Self::Restrict,
            "no_action" => Self::NoAction,
            "cascade" => Self::Cascade,
            "remove" => Self::Remove,
            "set_null" => Self::SetNull,
            "set_default" => Self::SetDefault,
            _ => return None,
        })
    }

    /// Whether this action refuses rather than acts.
    pub fn refuses(self) -> bool {
        matches!(self, Self::Restrict | Self::NoAction)
    }
}

/// A foreign key: the values reached by `from` must name existing rows.
#[derive(Debug, Clone)]
pub struct ForeignKey {
    name: String,
    from: Vec<RefPath>,
    to: Target,
    columns: Vec<String>,
    on_delete: Action,
    on_update: Action,
}

impl ForeignKey {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn from(&self) -> &[RefPath] {
        &self.from
    }
    pub fn to(&self) -> &Target {
        &self.to
    }
    /// The columns of the target that the references are compared with.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn on_delete(&self) -> Action {
        self.on_delete
    }
    pub fn on_update(&self) -> Action {
        self.on_update
    }
    /// Whether one row can hold many references through this key.
    pub fn iterates(&self) -> bool {
        self.from.iter().any(RefPath::iterates)
    }
    /// A human spelling of the key, as `constraint` in diagnostics.
    pub fn describe(&self, table: &str) -> String {
        let from: Vec<String> = self.from.iter().map(ToString::to_string).collect();
        format!(
            "{table}.{} -> {}({})",
            from.join(","),
            self.to.describe(),
            self.columns.join(",")
        )
    }
}

/// A row-local rule, as a boolean SQL expression over the row's columns.
#[derive(Debug, Clone)]
pub struct Check {
    name: String,
    expr: String,
}

impl Check {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn expr(&self) -> &str {
        &self.expr
    }
}

/// A graph over the table's own rows that must contain no cycle.
#[derive(Debug, Clone)]
pub struct Acyclic {
    name: String,
    edges: Vec<RefPath>,
}

impl Acyclic {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn edges(&self) -> &[RefPath] {
        &self.edges
    }
}

/// How much a violated rule matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The database is invalid while the rule is violated.
    Error,
    /// The violation is reported but the database remains valid.
    Warning,
}

/// A rule over sets of rows: a query naming the rows that violate it.
#[derive(Debug, Clone)]
pub struct Assertion {
    name: String,
    query: String,
    severity: Severity,
    message: Option<String>,
}

impl Assertion {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn query(&self) -> &str {
        &self.query
    }
    pub fn severity(&self) -> Severity {
        self.severity
    }
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }
}

/// Where a table's schema document lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaFileKind {
    /// `schema/<table>.json`: a declaration the user keeps. A pinned table's
    /// schema is its pin; there is no second copy of it.
    Pin,
    /// `.db/schema/<table>.json`: maintained by reldir -- inferred, or edited by
    /// migrations -- and reconstructible from the rows it describes.
    Working,
}

/// Whether a row may carry members the schema does not declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdditionalFields {
    Reject,
    Allow,
}

/// One table's schema. Read-only: see the module docs.
#[derive(Debug, Clone)]
pub struct Schema {
    document: Arc<Value>,
    identity: String,
    table: String,
    schema_version: u64,
    description: Option<String>,
    primary_key: Vec<String>,
    columns: IndexMap<String, Column>,
    unique: Vec<Vec<String>>,
    indexes: Vec<Vec<String>>,
    foreign_keys: Vec<ForeignKey>,
    checks: Vec<Check>,
    filename: Option<Vec<String>>,
    additional_fields: AdditionalFields,
    identity_domain: Option<String>,
    acyclic: Vec<Acyclic>,
    assertions: Vec<Assertion>,
    validator: Arc<row::RowValidator>,
}

impl Schema {
    /// Read a schema document. Every fault is reported, located against
    /// `source` when the bytes are available.
    pub fn from_document(
        document: Value,
        source: Option<&[u8]>,
    ) -> std::result::Result<Self, Vec<Diagnostic>> {
        document::decode(document, source)
    }

    /// Read a schema file's bytes.
    pub fn from_bytes(bytes: &[u8]) -> std::result::Result<Self, Vec<Diagnostic>> {
        let document = crate::json::parse(bytes).map_err(|error| {
            let mut diagnostic = Diagnostic::error("SCHEMA_INVALID_JSON", error.to_string());
            diagnostic.location = Some(crate::diagnostic::Location {
                line: error.line(),
                column: error.column(),
            });
            vec![diagnostic]
        })?;
        Self::from_document(document, Some(bytes))
    }

    /// The document this schema was read from.
    pub fn document(&self) -> &Value {
        &self.document
    }
    /// Identity: equal exactly when two schemas impose the same rules.
    pub fn identity(&self) -> &str {
        &self.identity
    }
    pub fn table(&self) -> &str {
        &self.table
    }
    pub fn schema_version(&self) -> u64 {
        self.schema_version
    }
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
    pub fn primary_key(&self) -> &[String] {
        &self.primary_key
    }
    pub fn columns(&self) -> &IndexMap<String, Column> {
        &self.columns
    }
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.get(name)
    }
    pub fn unique(&self) -> &[Vec<String>] {
        &self.unique
    }
    pub fn indexes(&self) -> &[Vec<String>] {
        &self.indexes
    }
    pub fn foreign_keys(&self) -> &[ForeignKey] {
        &self.foreign_keys
    }
    pub fn checks(&self) -> &[Check] {
        &self.checks
    }
    pub fn acyclic(&self) -> &[Acyclic] {
        &self.acyclic
    }
    pub fn assertions(&self) -> &[Assertion] {
        &self.assertions
    }
    pub fn identity_domain(&self) -> Option<&str> {
        self.identity_domain.as_deref()
    }
    pub fn additional_fields(&self) -> AdditionalFields {
        self.additional_fields
    }
    pub fn validator(&self) -> &row::RowValidator {
        &self.validator
    }
    /// The columns a row's filename is built from: `x-reldir.filename`, else
    /// the primary key.
    pub fn filename_columns(&self) -> &[String] {
        self.filename.as_deref().unwrap_or(&self.primary_key)
    }
    /// Whether the table declares a filename other than its primary key.
    pub fn has_filename_override(&self) -> bool {
        self.filename.is_some()
    }
    /// Every candidate key: the primary key, then each unique constraint.
    pub fn candidate_keys(&self) -> impl Iterator<Item = &[String]> {
        std::iter::once(self.primary_key.as_slice()).chain(self.unique.iter().map(Vec::as_slice))
    }
    /// The names of every named constraint, which share one namespace.
    pub fn constraint_names(&self) -> BTreeSet<&str> {
        self.foreign_keys
            .iter()
            .map(ForeignKey::name)
            .chain(self.checks.iter().map(Check::name))
            .chain(self.acyclic.iter().map(Acyclic::name))
            .chain(self.assertions.iter().map(Assertion::name))
            .collect()
    }
    /// The document, rendered for writing to disk.
    pub fn bytes(&self, indentation_width: usize) -> Vec<u8> {
        crate::canonical::pretty_with_indent(&self.document, indentation_width)
    }
    /// Begin an edit of this schema's document.
    pub fn edit(&self) -> document::Editor {
        document::Editor::new((*self.document).clone())
    }
}

/// Table names are lowercase identifiers, never `schema`, and never a name
/// Windows reserves for a device.
pub fn valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && s != "schema"
        && !matches!(
            s,
            "con" | "prn" | "aux" | "nul"
                | "com1" | "com2" | "com3" | "com4" | "com5" | "com6" | "com7" | "com8" | "com9"
                | "lpt1" | "lpt2" | "lpt3" | "lpt4" | "lpt5" | "lpt6" | "lpt7" | "lpt8" | "lpt9"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test1077_table_names_are_restricted_and_portable() {
        for accepted in ["users", "u", "a_b", "t1", "order_items"] {
            assert!(valid_name(accepted), "{accepted} should be a valid name");
        }
        for rejected in [
            "", "Users", "1users", "_users", "user-items", "user.items", "üsers", "con", "prn",
            "aux", "nul", "com1", "lpt9", ".hidden", "schema",
        ] {
            assert!(!valid_name(rejected), "{rejected:?} must be rejected");
        }
    }

    #[test]
    fn test2040_type_and_action_names_round_trip() {
        for kind in [
            ColumnType::Bool, ColumnType::Int, ColumnType::Float, ColumnType::Decimal,
            ColumnType::String, ColumnType::Bytes, ColumnType::Date, ColumnType::Timestamp,
            ColumnType::Uuid, ColumnType::Ulid, ColumnType::Enum, ColumnType::Array,
            ColumnType::Object, ColumnType::Json,
        ] {
            assert_eq!(ColumnType::from_name(kind.name()), Some(kind));
        }
        for action in [
            Action::Restrict, Action::NoAction, Action::Cascade, Action::Remove,
            Action::SetNull, Action::SetDefault,
        ] {
            assert_eq!(Action::from_name(action.name()), Some(action));
        }
        for generated in [GeneratedKind::Uuid, GeneratedKind::Ulid, GeneratedKind::Now, GeneratedKind::Sequence] {
            assert_eq!(GeneratedKind::from_name(generated.name()), Some(generated));
        }
    }
}
