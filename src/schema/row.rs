//! Validating one row against its table's schema.
//!
//! Two questions are asked of every row, and each has exactly one answerer:
//!
//! - *Does the row satisfy its JSON Schema document?* The document is compiled
//!   once by a JSON Schema 2020-12 implementation and every keyword it contains
//!   is enforced with its standard meaning -- composition, conditionals,
//!   patterns, formats, bounds, `$ref`, everything. reldir does not re-implement
//!   JSON Schema, so it cannot disagree with it.
//! - *Is every value written the way its relational type requires?* JSON Schema
//!   has no words for this: its `integer` accepts `1.0`, it has no decimal, and
//!   `format: uuid` accepts upper case. reldir's types are lexical because a
//!   value's written form is its identity -- it names files and is hashed -- so
//!   they are checked here, from the relational view of the same document.
//!
//! Each failure becomes one diagnostic naming the value by JSON Pointer, the
//! rule it broke by the rule's location in the schema, and -- when the caller
//! has the file's bytes -- the line and column to edit.

use super::{Column, ColumnType};
use crate::diagnostic::Diagnostic;
use indexmap::IndexMap;
use serde_json::{Map, Value};

/// A table schema compiled for validating rows.
pub struct RowValidator {
    compiled: jsonschema::Validator,
    columns: IndexMap<String, Column>,
}

impl std::fmt::Debug for RowValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowValidator").finish_non_exhaustive()
    }
}

impl RowValidator {
    /// Compile a table document. A document that cannot be compiled -- an
    /// uncompilable pattern, a `$ref` that points outside it -- is a schema
    /// fault, reported against the schema.
    pub fn compile(document: &Value, columns: &IndexMap<String, Column>) -> Result<Self, Diagnostic> {
        let compiled = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .should_validate_formats(true)
            .build(document)
            .map_err(|error| {
                let text = error.to_string();
                let external = text.contains("not present in a registry")
                    || text.contains("resolve external")
                    || text.contains("retrieving it failed");
                if external {
                    Diagnostic::error(
                        "SCHEMA_REF_EXTERNAL",
                        format!(
                            "a $ref leaves the document ({text}); a table's rules must be stated in \
                             its own schema so that validity never depends on a file the database \
                             does not govern"
                        ),
                    )
                } else {
                    Diagnostic::error("SCHEMA_GRAMMAR", format!("the schema does not compile: {text}"))
                        .pointer(error.instance_path().to_string())
                }
            })?;
        Ok(Self {
            compiled,
            columns: columns.clone(),
        })
    }

    /// Whether a row is valid, without describing why not.
    pub fn is_valid(&self, row: &Value) -> bool {
        self.compiled.is_valid(row) && {
            let mut out = vec![];
            lexical_row(row, &self.columns, &mut out);
            key_collisions(row, "", &mut out);
            out.is_empty()
        }
    }

    /// Every fault in a row, as diagnostics carrying pointers.
    pub fn check(&self, row: &Value) -> Vec<Diagnostic> {
        let mut out: Vec<Diagnostic> = vec![];
        for error in self.compiled.iter_errors(row) {
            out.extend(describe(&error, row));
        }
        lexical_row(row, &self.columns, &mut out);
        key_collisions(row, "", &mut out);
        out
    }

    /// A lossless conversion of the value at a top-level column that makes the
    /// row valid there, if one exists.
    ///
    /// The candidate is judged by validating the whole row with the value
    /// replaced, so a conversion is offered only when it satisfies every rule
    /// the schema states for that column -- not merely its type.
    pub fn coercion(&self, row: &Value, column: &str) -> Option<Value> {
        let current = row.get(column)?;
        let target = self.columns.get(column)?;
        let candidate = crate::value::lossless_convert(current, target.kind())?;
        let mut replaced = row.clone();
        replaced.as_object_mut()?.insert(column.to_string(), candidate.clone());
        let pointer = format!("/{}", super::path::escape_pointer(column));
        let still_wrong = self
            .check(&replaced)
            .iter()
            .any(|d| d.pointer.as_deref().is_some_and(|p| p == pointer || p.starts_with(&format!("{pointer}/"))));
        (!still_wrong).then_some(candidate)
    }
}

/// Translate one JSON Schema error into diagnostics a person can act on.
fn describe(error: &jsonschema::ValidationError<'_>, row: &Value) -> Vec<Diagnostic> {
    use jsonschema::error::ValidationErrorKind as Kind;
    let pointer = error.instance_path().to_string();
    let rule = error.schema_path().to_string();
    let top_level = pointer.is_empty();
    let column = super::path::pointer_tokens(&pointer).into_iter().next();
    let with_context = |diagnostic: Diagnostic| {
        let mut diagnostic = diagnostic.pointer(pointer.clone());
        diagnostic.constraint = Some(rule.clone());
        if let Some(column) = &column {
            diagnostic = diagnostic.field(column.clone());
        }
        diagnostic
    };
    match error.kind() {
        Kind::Required { property } if top_level => {
            let name = property.as_str().map(String::from).unwrap_or_else(|| property.to_string());
            let mut diagnostic = Diagnostic::error(
                "ROW_MISSING_FIELD",
                format!("required field {name:?} is absent"),
            )
            .pointer(String::new())
            .field(name);
            diagnostic.constraint = Some(rule);
            vec![diagnostic]
        }
        Kind::AdditionalProperties { unexpected } if top_level => unexpected
            .iter()
            .map(|name| {
                let mut diagnostic = Diagnostic::error(
                    "ROW_UNKNOWN_FIELD",
                    format!("unknown field {name:?}: the schema declares no such column"),
                )
                .pointer(format!("/{}", super::path::escape_pointer(name)))
                .field(name.clone())
                .fix("FIX_DROP_UNKNOWN_FIELD");
                diagnostic.constraint = Some(rule.clone());
                diagnostic
            })
            .collect(),
        Kind::Type { .. }
            if error.instance().is_null() && super::path::pointer_tokens(&pointer).len() == 1 =>
        {
            vec![with_context(Diagnostic::error(
                "NOT_NULL_VIOLATION",
                format!("{:?} cannot be null", column.clone().unwrap_or_default()),
            ))]
        }
        Kind::Type { kind } => vec![with_context(
            Diagnostic::error(
                "TYPE_MISMATCH",
                format!(
                    "{} is {}, but the schema requires {kind:?}",
                    at(&pointer),
                    json_kind(error.instance())
                ),
            )
            .observed(crate::canonical::compact(error.instance())),
        )
        .fix("FIX_COERCE_VALUE")],
        Kind::OneOfMultipleValid { context } => {
            let matched: Vec<String> = context
                .iter()
                .enumerate()
                .filter(|(_, errors)| errors.is_empty())
                .map(|(index, _)| index.to_string())
                .collect();
            vec![with_context(Diagnostic::error(
                "SCHEMA_VIOLATION",
                format!(
                    "{} matches {} of the {} `oneOf` alternatives ({}); exactly one must match",
                    at(&pointer),
                    matched.len(),
                    context.len(),
                    matched.join(", ")
                ),
            ))]
        }
        Kind::OneOfNotValid { context } | Kind::AnyOf { context } => {
            let keyword = if matches!(error.kind(), Kind::AnyOf { .. }) { "anyOf" } else { "oneOf" };
            let reasons: Vec<String> = context
                .iter()
                .enumerate()
                .map(|(index, errors)| {
                    let first = errors
                        .first()
                        .map(|error| error.to_string())
                        .unwrap_or_else(|| "rejected".into());
                    format!("alternative {index}: {first}")
                })
                .collect();
            vec![with_context(Diagnostic::error(
                "SCHEMA_VIOLATION",
                format!(
                    "{} matches none of the `{keyword}` alternatives -- {}",
                    at(&pointer),
                    reasons.join("; ")
                ),
            ))]
        }
        _ => vec![with_context(
            Diagnostic::error("SCHEMA_VIOLATION", format!("{}: {error}", at(&pointer)))
                .observed(crate::canonical::compact(
                    row.pointer(&pointer).unwrap_or(error.instance()),
                )),
        )],
    }
}

fn at(pointer: &str) -> String {
    if pointer.is_empty() {
        "the row".into()
    } else {
        format!("the value at {pointer}")
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(number) if number.is_f64() => "a number",
        Value::Number(_) => "an integer",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn lexical_row(row: &Value, columns: &IndexMap<String, Column>, out: &mut Vec<Diagnostic>) {
    let Some(object) = row.as_object() else { return };
    for (name, column) in columns {
        if let Some(value) = object.get(name) {
            let pointer = format!("/{}", super::path::escape_pointer(name));
            lexical(value, column, &pointer, name, out);
        }
    }
}

/// reldir's lexical type rules, which JSON Schema cannot state.
fn lexical(value: &Value, column: &Column, pointer: &str, field: &str, out: &mut Vec<Diagnostic>) {
    if value.is_null() {
        return;
    }
    let refuse = |reason: String| {
        Diagnostic::error("TYPE_MISMATCH", reason)
            .pointer(pointer.to_string())
            .field(field.to_string())
            .observed(crate::canonical::compact(value))
            .fix("FIX_COERCE_VALUE")
    };
    match column.kind() {
        ColumnType::Int if value.is_number() => {
            let lexical_int = value.as_i64().is_some() && !value.is_f64();
            if !lexical_int {
                out.push(refuse(format!(
                    "{} is not an int: an int is written as a whole number, without a fraction or \
                     exponent, within the 64-bit signed range",
                    at(pointer)
                )));
            }
        }
        ColumnType::Decimal => {
            if let Some(text) = value.as_str()
                && !crate::value::canonical_decimal(text)
            {
                out.push(refuse(format!(
                    "{} is not a decimal in canonical form (an optional minus, no leading zeros, no \
                     trailing fractional zeros)",
                    at(pointer)
                )));
            }
        }
        ColumnType::Uuid => {
            if let Some(text) = value.as_str()
                && uuid::Uuid::parse_str(text).is_ok()
                && (text.len() != 36 || text != text.to_ascii_lowercase())
            {
                out.push(refuse(format!(
                    "{} is a uuid but not in canonical form: lowercase, hyphenated, 36 characters",
                    at(pointer)
                )));
            }
        }
        ColumnType::Ulid => {
            if let Some(text) = value.as_str()
                && !(ulid::Ulid::from_string(text).is_ok()
                    && text.len() == 26
                    && text == text.to_ascii_uppercase())
            {
                out.push(refuse(format!(
                    "{} is not a ulid in canonical form: 26 uppercase Crockford base32 characters",
                    at(pointer)
                )));
            }
        }
        ColumnType::Bytes => {
            use base64::Engine;
            if let Some(text) = value.as_str()
                && base64::engine::general_purpose::STANDARD.decode(text).is_err()
            {
                out.push(refuse(format!("{} is not standard base64", at(pointer))));
            }
        }
        ColumnType::Array => {
            if let (Some(items), Some(elements)) = (column.items(), value.as_array()) {
                for (index, element) in elements.iter().enumerate() {
                    lexical(element, items, &format!("{pointer}/{index}"), field, out);
                }
            }
        }
        ColumnType::Object => {
            if let (Some(properties), Some(members)) = (column.properties(), value.as_object()) {
                for (name, nested) in properties {
                    if let Some(member) = members.get(name) {
                        lexical(
                            member,
                            nested,
                            &format!("{pointer}/{}", super::path::escape_pointer(name)),
                            field,
                            out,
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

/// Two member names that differ only by Unicode normalization name the same
/// member to every tool that normalizes, and different members to every tool
/// that does not. reldir refuses the ambiguity at any depth.
fn key_collisions(value: &Value, pointer: &str, out: &mut Vec<Diagnostic>) {
    use unicode_normalization::UnicodeNormalization;
    match value {
        Value::Object(members) => {
            let mut seen = std::collections::BTreeMap::<String, &str>::new();
            for key in members.keys() {
                let normalized: String = key.nfc().collect();
                if let Some(first) = seen.insert(normalized, key) {
                    out.push(
                        Diagnostic::error(
                            "KEY_COLLISION",
                            format!(
                                "members {first:?} and {key:?} are the same name after Unicode \
                                 normalization"
                            ),
                        )
                        .pointer(format!("{pointer}/{}", super::path::escape_pointer(key))),
                    );
                }
            }
            for (key, child) in members {
                key_collisions(child, &format!("{pointer}/{}", super::path::escape_pointer(key)), out);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                key_collisions(item, &format!("{pointer}/{index}"), out);
            }
        }
        _ => {}
    }
}

/// A row as the value the validator reads.
pub fn row_value(row: &Map<String, Value>) -> Value {
    Value::Object(row.clone())
}

#[cfg(test)]
mod tests {
    use crate::schema::Schema;
    use serde_json::json;

    fn items() -> Schema {
        Schema::from_document(
            json!({
                "$schema": crate::schema::meta::DIALECT_URI,
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "n": { "type": ["integer", "null"], "x-reldir-type": "int" },
                    "rules": {
                        "type": ["array", "null"],
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["message"],
                            "properties": {
                                "message": { "type": "string", "minLength": 1 },
                                "when_choice_index": { "type": ["integer", "null"], "x-reldir-type": "int" },
                                "when_incorrect": { "type": ["boolean", "null"] }
                            },
                            "oneOf": [
                                { "required": ["when_choice_index"] },
                                { "required": ["when_incorrect"] }
                            ]
                        }
                    }
                },
                "required": ["id"],
                "additionalProperties": false,
                "if": { "properties": { "id": { "const": "x" } }, "required": ["id"] },
                "then": { "required": ["n"] },
                "x-reldir": { "table": "items", "primaryKey": ["id"] }
            }),
            None,
        )
        .expect("the schema decodes")
    }

    fn codes(row: serde_json::Value) -> Vec<(String, Option<String>)> {
        items()
            .validator()
            .check(&row)
            .into_iter()
            .map(|d| (d.code, d.pointer))
            .collect()
    }

    /// The defect the LCAS corpus hit: every feedback rule with exactly one
    /// trigger was refused, because the alternatives lost their `required`.
    #[test]
    fn test2060_one_trigger_per_rule_is_valid_and_two_or_none_are_not() {
        assert!(codes(json!({"id": "a", "rules": [{"message": "m", "when_choice_index": 1}]})).is_empty());
        assert!(codes(json!({"id": "a", "rules": [{"message": "m", "when_incorrect": true}]})).is_empty());

        let both = items()
            .validator()
            .check(&json!({"id": "a", "rules": [{"message": "m", "when_choice_index": 1, "when_incorrect": true}]}));
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].code, "SCHEMA_VIOLATION");
        assert_eq!(both[0].pointer.as_deref(), Some("/rules/0"));
        assert!(both[0].message.contains("(0, 1)"), "names the matching alternatives: {}", both[0].message);
        assert!(both[0].fixes.is_empty(), "no coercion answers a composition miss");

        let none = codes(json!({"id": "a", "rules": [{"message": "m"}]}));
        assert_eq!(none, vec![("SCHEMA_VIOLATION".into(), Some("/rules/0".into()))]);
    }

    #[test]
    fn test2061_root_faults_get_their_own_codes() {
        assert_eq!(codes(json!({})), vec![("ROW_MISSING_FIELD".into(), Some(String::new()))]);
        assert_eq!(
            codes(json!({"id": "a", "emial": 1})),
            vec![("ROW_UNKNOWN_FIELD".into(), Some("/emial".into()))]
        );
        assert_eq!(codes(json!({"id": null})), vec![("NOT_NULL_VIOLATION".into(), Some("/id".into()))]);
        assert_eq!(codes(json!({"id": 5})), vec![("TYPE_MISMATCH".into(), Some("/id".into()))]);
        // Conditionals are enforced: an `x` must carry `n`.
        assert_eq!(codes(json!({"id": "x"})), vec![("ROW_MISSING_FIELD".into(), Some(String::new()))]);
    }

    #[test]
    fn test2062_lexical_types_are_enforced_where_json_schema_is_silent() {
        // JSON Schema accepts 1.0 as an integer; reldir's int is lexical.
        assert_eq!(codes(json!({"id": "a", "n": 1.0})), vec![("TYPE_MISMATCH".into(), Some("/n".into()))]);
        assert_eq!(
            codes(json!({"id": "a", "rules": [{"message": "m", "when_choice_index": 2.0}]})),
            vec![("TYPE_MISMATCH".into(), Some("/rules/0/when_choice_index".into()))]
        );
        assert!(codes(json!({"id": "a", "n": 7})).is_empty());
        // Beyond the signed 64-bit range is not an int.
        assert_eq!(
            codes(json!({"id": "a", "n": 18446744073709551615_u64})),
            vec![("TYPE_MISMATCH".into(), Some("/n".into()))]
        );
    }

    #[test]
    fn test2063_normalization_collisions_are_found_at_any_depth() {
        let decomposed = "e\u{0301}";
        let composed = "\u{e9}";
        let mut nested = serde_json::Map::new();
        nested.insert(decomposed.into(), json!(1));
        nested.insert(composed.into(), json!(2));
        let row = json!({"id": "a", "rules": [serde_json::Value::Object(nested)]});
        let found = items().validator().check(&row);
        assert!(found.iter().any(|d| d.code == "KEY_COLLISION"), "{found:?}");
    }

    #[test]
    fn test2064_a_coercion_is_offered_only_when_it_satisfies_the_column() {
        let schema = items();
        assert_eq!(schema.validator().coercion(&json!({"id": "a", "n": "7"}), "n"), Some(json!(7)));
        // "07" would change the written value, so no conversion is lossless.
        assert_eq!(schema.validator().coercion(&json!({"id": "a", "n": "07"}), "n"), None);
    }
}
