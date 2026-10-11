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
//!   `format: uuid` accepts upper case. reldir's lexical rules are compiled into
//!   the same validator as keywords ([`super::keywords`]), so they hold wherever
//!   a subschema does.
//!
//! Each failure becomes one diagnostic naming the value by JSON Pointer, the
//! rule it broke by the rule's location in the schema, and -- when the caller
//! has the file's bytes -- the line and column to edit.

use super::Column;
use crate::code::Code;
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
    pub fn compile(
        document: &Value,
        columns: &IndexMap<String, Column>,
    ) -> Result<Self, Box<Diagnostic>> {
        let compiled = super::keywords::register(
            jsonschema::options().with_draft(jsonschema::Draft::Draft202012),
        )
        .should_validate_formats(true)
        .should_ignore_unknown_formats(false)
        .build(&members_sorted(document))
        .map_err(|error| {
            let location = error.schema_path().to_string();
            let text = error.to_string();
            // The validator is built without any way to fetch a document, so
            // a reference it cannot resolve is one that leaves the document --
            // the standard 2020-12 meta-schemas, which it carries, resolve.
            if matches!(
                error.kind(),
                jsonschema::error::ValidationErrorKind::Referencing(_)
            ) {
                return Box::new(Diagnostic::error(
                    Code::SCHEMA_REF_EXTERNAL,
                    format!(
                        "a $ref leaves the document ({text}); a table's rules must be stated in \
                         its own schema so that validity never depends on a file the database \
                         does not govern"
                    ),
                ));
            }
            let keyword = location.rsplit('/').next().unwrap_or_default();
            let code = match keyword {
                "format" => Code::SCHEMA_UNKNOWN_FORMAT,
                super::meta::TYPE_TAG => Code::SCHEMA_TYPE_TAG_CONFLICT,
                _ => Code::SCHEMA_GRAMMAR,
            };
            Box::new(
                Diagnostic::error(code, format!("the schema does not compile: {text}"))
                    .pointer(location),
            )
        })?;
        Ok(Self {
            compiled,
            columns: columns.clone(),
        })
    }

    /// Whether a row is valid, without describing why not.
    pub fn is_valid(&self, row: &Value) -> bool {
        self.compiled.is_valid(&members_sorted(row)) && {
            let mut out = vec![];
            key_collisions(row, "", &mut out);
            out.is_empty()
        }
    }

    /// Every fault in a row, as diagnostics carrying pointers.
    pub fn check(&self, row: &Value) -> Vec<Diagnostic> {
        let mut out: Vec<Diagnostic> = vec![];
        let sorted = members_sorted(row);
        for error in self.compiled.iter_errors(&sorted) {
            out.extend(describe(&error, &sorted));
        }
        key_collisions(row, "", &mut out);
        out
    }

    /// Every rule a row breaks, as the rule's location in the schema and what
    /// it refused -- what widening a contract edits.
    pub fn failures(&self, row: &Value) -> Vec<Failure> {
        use jsonschema::error::ValidationErrorKind as Kind;
        let sorted = members_sorted(row);
        self.compiled
            .iter_errors(&sorted)
            .map(|error| {
                let refused = if super::keywords::is_lexical(&error) {
                    Refused::Lexical
                } else {
                    match error.kind() {
                        Kind::Required { property } => Refused::Required(
                            property
                                .as_str()
                                .map(String::from)
                                .unwrap_or_else(|| property.to_string()),
                        ),
                        Kind::AdditionalProperties { unexpected } => {
                            Refused::Additional(unexpected.clone())
                        }
                        Kind::FalseSchema => Refused::Nothing,
                        _ => Refused::Keyword,
                    }
                };
                Failure {
                    rule: error.schema_path().to_string(),
                    at: error.instance_path().to_string(),
                    instance: error.instance().clone().into_owned(),
                    refused,
                }
            })
            .collect()
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
        replaced
            .as_object_mut()?
            .insert(column.to_string(), candidate.clone());
        let pointer = format!("/{}", super::path::escape_pointer(column));
        let still_wrong = self.check(&replaced).iter().any(|d| {
            d.pointer
                .as_deref()
                .is_some_and(|p| p == pointer || p.starts_with(&format!("{pointer}/")))
        });
        (!still_wrong).then_some(candidate)
    }
}

/// One rule a value breaks.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    /// The rule's location in the schema, as a JSON Pointer that passes
    /// through each `$ref` it followed.
    pub rule: String,
    /// The value's location in the row.
    pub at: String,
    pub instance: Value,
    pub refused: Refused,
}

/// What a broken rule refused.
#[derive(Debug, Clone, PartialEq)]
pub enum Refused {
    /// The keyword at the end of the rule's location refused the value.
    Keyword,
    /// `required` refused an object that lacks this member.
    Required(String),
    /// `additionalProperties` refused these members.
    Additional(Vec<String>),
    /// The subschema is `false`: it admits nothing.
    Nothing,
    /// A value of the right type is not written as reldir's type requires.
    Lexical,
}

/// A value with every object's members in name order.
///
/// The validator compares objects member by member in the order it holds
/// them, so `{"a":1,"b":2}` would fail a `const` of `{"b":2,"a":1}` and two
/// reordered copies would pass `uniqueItems`. Member order is never part of
/// what a value is, so both the schema and every row are judged in one order.
fn members_sorted(value: &Value) -> Value {
    match value {
        Value::Object(members) => {
            let mut entries: Vec<(&String, &Value)> = members.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), members_sorted(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(members_sorted).collect()),
        other => other.clone(),
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
    if super::keywords::is_lexical(error) {
        return vec![
            with_context(
                Diagnostic::error(Code::TYPE_MISMATCH, format!("{} {error}", at(&pointer)))
                    .observed(crate::canonical::compact(error.instance())),
            )
            .fix("FIX_COERCE_VALUE"),
        ];
    }
    match error.kind() {
        Kind::Required { property } if top_level => {
            let name = property
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| property.to_string());
            let mut diagnostic = Diagnostic::error(
                Code::ROW_MISSING_FIELD,
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
                    Code::ROW_UNKNOWN_FIELD,
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
                Code::NOT_NULL_VIOLATION,
                format!("{:?} cannot be null", column.clone().unwrap_or_default()),
            ))]
        }
        Kind::Type { kind } => vec![
            with_context(
                Diagnostic::error(
                    Code::TYPE_MISMATCH,
                    format!(
                        "{} is {}, but the schema requires {}",
                        at(&pointer),
                        json_kind(error.instance()),
                        match kind {
                            jsonschema::error::TypeKind::Single(one) => format!("{one}"),
                            jsonschema::error::TypeKind::Multiple(set) => set
                                .iter()
                                .map(|one| one.to_string())
                                .collect::<Vec<_>>()
                                .join(" or "),
                        }
                    ),
                )
                .observed(crate::canonical::compact(error.instance())),
            )
            .fix("FIX_COERCE_VALUE"),
        ],
        Kind::OneOfMultipleValid { context } => {
            let matched: Vec<String> = context
                .iter()
                .enumerate()
                .filter(|(_, errors)| errors.is_empty())
                .map(|(index, _)| index.to_string())
                .collect();
            vec![with_context(Diagnostic::error(
                Code::SCHEMA_VIOLATION,
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
            let keyword = if matches!(error.kind(), Kind::AnyOf { .. }) {
                "anyOf"
            } else {
                "oneOf"
            };
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
                Code::SCHEMA_VIOLATION,
                format!(
                    "{} matches none of the `{keyword}` alternatives -- {}",
                    at(&pointer),
                    reasons.join("; ")
                ),
            ))]
        }
        _ => vec![with_context(
            Diagnostic::error(Code::SCHEMA_VIOLATION, format!("{}: {error}", at(&pointer)))
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
                            Code::KEY_COLLISION,
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
                key_collisions(
                    child,
                    &format!("{pointer}/{}", super::path::escape_pointer(key)),
                    out,
                );
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
    use crate::code::Code;
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
                "x-reldir": { "table": "items", "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}} }
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
            .map(|d| (d.code.as_str().to_string(), d.pointer))
            .collect()
    }

    /// The defect the LCAS corpus hit: every feedback rule with exactly one
    /// trigger was refused, because the alternatives lost their `required`.
    #[test]
    fn test2060_one_trigger_per_rule_is_valid_and_two_or_none_are_not() {
        assert!(
            codes(json!({"id": "a", "rules": [{"message": "m", "when_choice_index": 1}]}))
                .is_empty()
        );
        assert!(
            codes(json!({"id": "a", "rules": [{"message": "m", "when_incorrect": true}]}))
                .is_empty()
        );

        let both = items()
            .validator()
            .check(&json!({"id": "a", "rules": [{"message": "m", "when_choice_index": 1, "when_incorrect": true}]}));
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].code, Code::SCHEMA_VIOLATION);
        assert_eq!(both[0].pointer.as_deref(), Some("/rules/0"));
        assert!(
            both[0].message.contains("(0, 1)"),
            "names the matching alternatives: {}",
            both[0].message
        );
        assert!(
            both[0].fixes.is_empty(),
            "no coercion answers a composition miss"
        );

        let none = codes(json!({"id": "a", "rules": [{"message": "m"}]}));
        assert_eq!(
            none,
            vec![("SCHEMA_VIOLATION".into(), Some("/rules/0".into()))]
        );
    }

    #[test]
    fn test2061_root_faults_get_their_own_codes() {
        assert_eq!(
            codes(json!({})),
            vec![("ROW_MISSING_FIELD".into(), Some(String::new()))]
        );
        assert_eq!(
            codes(json!({"id": "a", "emial": 1})),
            vec![("ROW_UNKNOWN_FIELD".into(), Some("/emial".into()))]
        );
        assert_eq!(
            codes(json!({"id": null})),
            vec![("NOT_NULL_VIOLATION".into(), Some("/id".into()))]
        );
        assert_eq!(
            codes(json!({"id": 5})),
            vec![("TYPE_MISMATCH".into(), Some("/id".into()))]
        );
        // Conditionals are enforced: an `x` must carry `n`.
        assert_eq!(
            codes(json!({"id": "x"})),
            vec![("ROW_MISSING_FIELD".into(), Some(String::new()))]
        );
    }

    #[test]
    fn test2062_lexical_types_are_enforced_where_json_schema_is_silent() {
        // JSON Schema accepts 1.0 as an integer; reldir's int is lexical.
        assert_eq!(
            codes(json!({"id": "a", "n": 1.0})),
            vec![("TYPE_MISMATCH".into(), Some("/n".into()))]
        );
        assert_eq!(
            codes(json!({"id": "a", "rules": [{"message": "m", "when_choice_index": 2.0}]})),
            vec![(
                "TYPE_MISMATCH".into(),
                Some("/rules/0/when_choice_index".into())
            )]
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
        assert!(
            found.iter().any(|d| d.code == Code::KEY_COLLISION),
            "{found:?}"
        );
    }

    #[test]
    fn test2064_a_coercion_is_offered_only_when_it_satisfies_the_column() {
        let schema = items();
        assert_eq!(
            schema
                .validator()
                .coercion(&json!({"id": "a", "n": "7"}), "n"),
            Some(json!(7))
        );
        // "07" would change the written value, so no conversion is lossless.
        assert_eq!(
            schema
                .validator()
                .coercion(&json!({"id": "a", "n": "07"}), "n"),
            None
        );
    }
}
