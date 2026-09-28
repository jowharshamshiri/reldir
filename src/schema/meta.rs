//! The reldir dialect of JSON Schema 2020-12.
//!
//! A reldir table schema is a JSON Schema 2020-12 document. Every standard
//! keyword has its standard meaning and is enforced on every row, and one
//! vocabulary is added for what JSON Schema has no keyword for: row identity,
//! uniqueness, references between rows, and rules over sets of rows. Those
//! live under `x-reldir`, and `x-reldir-type` marks the three column types a
//! standard keyword cannot distinguish (`int`, `decimal`, `ulid`).
//!
//! The dialect is *closed*: a member of a subschema that is neither a 2020-12
//! keyword, a reldir keyword, nor an `x-` annotation of someone else's is
//! refused, at every depth. JSON Schema itself ignores unknown keywords, which
//! means a typo such as `minLenght` silently stops constraining anything; a
//! database cannot accept a file that claims a rule it does not apply.
//!
//! The closure is written the way JSON Schema defines dialects: the meta-schema
//! carries `$dynamicAnchor: "meta"`, so the standard meta-schema's recursion
//! into `properties`, `items`, `oneOf` and every other applicator comes back to
//! this document and applies the closure again.
//!
//! The identifiers are URIs, not locations. Nothing here touches the network:
//! both documents are compiled in, and the standard 2020-12 meta-schemas ship
//! inside the validator.

use crate::diagnostic::{DbError, Diagnostic, Result};
use serde_json::{Value, json};
use std::sync::OnceLock;

/// The dialect's identifier, and the value of `$schema` in every table schema.
pub const DIALECT_URI: &str = "https://reldir.dev/schema/reldir-2";

/// The identifier of the table-document schema: the dialect plus what a table
/// schema's root must say.
pub const TABLE_URI: &str = "https://reldir.dev/schema/reldir-2/table";

/// The vocabulary that carries reldir's relational keywords.
pub const VOCABULARY_URI: &str = "https://reldir.dev/vocab/reldir-2";

/// The namespace holding every relational fact JSON Schema has no keyword for.
pub const EXTENSION: &str = "x-reldir";

/// The per-subschema tag for types a standard keyword cannot distinguish.
pub const TYPE_TAG: &str = "x-reldir-type";

/// Every keyword of JSON Schema 2020-12, by vocabulary.
pub const STANDARD_KEYWORDS: &[&str] = &[
    // core
    "$schema",
    "$id",
    "$ref",
    "$anchor",
    "$dynamicRef",
    "$dynamicAnchor",
    "$vocabulary",
    "$comment",
    "$defs",
    // applicator
    "prefixItems",
    "items",
    "contains",
    "additionalProperties",
    "properties",
    "patternProperties",
    "dependentSchemas",
    "propertyNames",
    "if",
    "then",
    "else",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    // unevaluated
    "unevaluatedItems",
    "unevaluatedProperties",
    // validation
    "type",
    "const",
    "enum",
    "multipleOf",
    "maximum",
    "exclusiveMaximum",
    "minimum",
    "exclusiveMinimum",
    "maxLength",
    "minLength",
    "pattern",
    "maxItems",
    "minItems",
    "uniqueItems",
    "maxContains",
    "minContains",
    "maxProperties",
    "minProperties",
    "required",
    "dependentRequired",
    // meta-data
    "title",
    "description",
    "default",
    "deprecated",
    "readOnly",
    "writeOnly",
    "examples",
    // format and content
    "format",
    "contentEncoding",
    "contentMediaType",
    "contentSchema",
];

/// The keywords reldir adds.
pub const RELDIR_KEYWORDS: &[&str] = &[EXTENSION, TYPE_TAG];

/// Every member name a subschema may carry, other than third-party `x-`
/// annotations.
pub fn known_keywords() -> impl Iterator<Item = &'static str> {
    STANDARD_KEYWORDS
        .iter()
        .chain(RELDIR_KEYWORDS.iter())
        .copied()
}

/// The dialect meta-schema: 2020-12, closed over its keywords.
pub fn meta_schema() -> &'static Value {
    static META: OnceLock<Value> = OnceLock::new();
    META.get_or_init(|| {
        let keywords: Vec<Value> = known_keywords().map(Value::from).collect();
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": DIALECT_URI,
            "$dynamicAnchor": "meta",
            "$vocabulary": {
                "https://json-schema.org/draft/2020-12/vocab/core": true,
                "https://json-schema.org/draft/2020-12/vocab/applicator": true,
                "https://json-schema.org/draft/2020-12/vocab/unevaluated": true,
                "https://json-schema.org/draft/2020-12/vocab/validation": true,
                "https://json-schema.org/draft/2020-12/vocab/meta-data": true,
                "https://json-schema.org/draft/2020-12/vocab/format-assertion": true,
                "https://json-schema.org/draft/2020-12/vocab/content": true,
                VOCABULARY_URI: true,
            },
            "title": "reldir schema dialect",
            "allOf": [
                { "$ref": "https://json-schema.org/draft/2020-12/schema" },
                { "$ref": "#/$defs/closed" }
            ],
            "$defs": {
                "closed": {
                    "type": ["object", "boolean"],
                    "propertyNames": {
                        "anyOf": [
                            { "enum": keywords },
                            { "pattern": "^x-(?!reldir)" }
                        ]
                    },
                    "properties": {
                        "x-reldir-type": { "enum": ["int", "decimal", "ulid"] }
                    }
                },
                "name": { "type": "string", "pattern": "^[a-z][a-z0-9_]*$" },
                "column": { "type": "string", "minLength": 1 },
                "columnList": {
                    "type": "array",
                    "minItems": 1,
                    "uniqueItems": true,
                    "items": { "$ref": "#/$defs/column" }
                },
                "columnLists": {
                    "type": "array",
                    "items": { "$ref": "#/$defs/columnList" }
                },
                "path": { "type": "string", "minLength": 1 },
                "action": {
                    "enum": ["restrict", "cascade", "remove", "set_null", "set_default", "no_action"]
                },
                "target": {
                    "oneOf": [
                        {
                            "type": "object",
                            "required": ["table"],
                            "additionalProperties": false,
                            "properties": { "table": { "$ref": "#/$defs/name" } }
                        },
                        {
                            "type": "object",
                            "required": ["tables"],
                            "additionalProperties": false,
                            "properties": {
                                "tables": {
                                    "type": "array",
                                    "minItems": 2,
                                    "uniqueItems": true,
                                    "items": { "$ref": "#/$defs/name" }
                                }
                            }
                        },
                        {
                            "type": "object",
                            "required": ["domain"],
                            "additionalProperties": false,
                            "properties": { "domain": { "$ref": "#/$defs/name" } }
                        }
                    ]
                },
                "foreignKey": {
                    "type": "object",
                    "required": ["from", "to"],
                    "additionalProperties": false,
                    "properties": {
                        "name": { "$ref": "#/$defs/name" },
                        "from": {
                            "type": "array",
                            "minItems": 1,
                            "items": { "$ref": "#/$defs/path" }
                        },
                        "to": { "$ref": "#/$defs/target" },
                        "columns": { "$ref": "#/$defs/columnList" },
                        "onDelete": { "$ref": "#/$defs/action" },
                        "onUpdate": { "$ref": "#/$defs/action" }
                    }
                },
                "extension": {
                    "type": "object",
                    "required": ["table", "primaryKey"],
                    "additionalProperties": false,
                    "properties": {
                        "table": { "$ref": "#/$defs/name" },
                        "schemaVersion": { "type": "integer", "minimum": 1 },
                        "primaryKey": { "$ref": "#/$defs/columnList" },
                        "unique": { "$ref": "#/$defs/columnLists" },
                        "indexes": { "$ref": "#/$defs/columnLists" },
                        "filename": { "$ref": "#/$defs/columnList" },
                        "generated": {
                            "type": "object",
                            "additionalProperties": { "enum": ["uuid", "ulid", "now", "sequence"] }
                        },
                        "identityDomain": { "$ref": "#/$defs/name" },
                        "foreignKeys": {
                            "type": "array",
                            "items": { "$ref": "#/$defs/foreignKey" }
                        },
                        "checks": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["name", "expr"],
                                "additionalProperties": false,
                                "properties": {
                                    "name": { "$ref": "#/$defs/name" },
                                    "expr": { "type": "string", "minLength": 1 }
                                }
                            }
                        },
                        "acyclic": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["name", "edges"],
                                "additionalProperties": false,
                                "properties": {
                                    "name": { "$ref": "#/$defs/name" },
                                    "edges": {
                                        "type": "array",
                                        "minItems": 1,
                                        "items": { "$ref": "#/$defs/path" }
                                    }
                                }
                            }
                        },
                        "assertions": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["name", "query"],
                                "additionalProperties": false,
                                "properties": {
                                    "name": { "$ref": "#/$defs/name" },
                                    "query": { "type": "string", "minLength": 1 },
                                    "severity": { "enum": ["error", "warning"] },
                                    "message": { "type": "string", "minLength": 1 }
                                }
                            }
                        }
                    }
                }
            }
        })
    })
}

/// The schema every table document must satisfy: the dialect, plus what a
/// table's root must say.
pub fn table_schema() -> &'static Value {
    static TABLE: OnceLock<Value> = OnceLock::new();
    TABLE.get_or_init(|| {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": TABLE_URI,
            "title": "reldir table schema",
            "allOf": [
                { "$ref": DIALECT_URI },
                {
                    "type": "object",
                    "required": ["$schema", "type", "properties", "additionalProperties", EXTENSION],
                    "properties": {
                        "$schema": { "const": DIALECT_URI },
                        "type": { "const": "object" },
                        "properties": { "type": "object", "minProperties": 1 },
                        "additionalProperties": { "type": "boolean" },
                        EXTENSION: { "$ref": format!("{DIALECT_URI}#/$defs/extension") }
                    }
                }
            ]
        })
    })
}

fn registry() -> std::result::Result<&'static jsonschema::Registry<'static>, String> {
    static REGISTRY: OnceLock<std::result::Result<jsonschema::Registry<'static>, String>> =
        OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            jsonschema::Registry::new()
                .add(
                    DIALECT_URI,
                    jsonschema::Resource::from_contents(meta_schema().clone()),
                )
                .map_err(|error| error.to_string())?
                .prepare()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// The compiled validator for table documents.
///
/// A failure to build it is a defect in this file, not a condition a user can
/// provoke, and is reported as an internal fault.
pub fn table_validator() -> Result<&'static jsonschema::Validator> {
    static VALIDATOR: OnceLock<std::result::Result<jsonschema::Validator, String>> =
        OnceLock::new();
    let built = VALIDATOR.get_or_init(|| {
        let registry = registry()?;
        jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .with_registry(registry)
            .build(table_schema())
            .map_err(|error| error.to_string())
    });
    built.as_ref().map_err(|error| {
        DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("the bundled reldir dialect does not compile: {error}"),
            6,
        )
    })
}

/// Everything wrong with a document as a table schema, as located diagnostics.
///
/// Locations are resolved against `source` when the caller has the bytes the
/// document came from, so a fault points at the line a person has to edit.
pub fn check_document(document: &Value, source: Option<&[u8]>) -> Result<Vec<Diagnostic>> {
    let validator = table_validator()?;
    let spans = source.map(crate::locate::Spans::of);
    let mut out = vec![];
    for error in validator.iter_errors(document) {
        let pointer = error.instance_path().to_string();
        let mut diagnostic = describe(&error, &pointer, document);
        diagnostic.pointer = Some(pointer.clone());
        if let Some(spans) = &spans {
            diagnostic.location = spans.location(&pointer);
        }
        out.push(diagnostic);
    }
    // The dialect cannot say "only at the root" about `x-reldir`, so a nested
    // one is refused here: it would be a relational declaration in a place
    // that governs no table.
    nested_extension(document, "", true, &mut out);
    if let Some(spans) = &spans {
        for diagnostic in &mut out {
            if diagnostic.location.is_none()
                && let Some(pointer) = &diagnostic.pointer
            {
                diagnostic.location = spans.location(pointer);
            }
        }
    }
    Ok(out)
}

fn nested_extension(value: &Value, pointer: &str, root: bool, out: &mut Vec<Diagnostic>) {
    match value {
        Value::Object(members) => {
            for (key, child) in members {
                let at = format!("{pointer}/{}", crate::schema::path::escape_pointer(key));
                if key == EXTENSION && !root {
                    let mut diagnostic = Diagnostic::error(
                        "SCHEMA_UNKNOWN_KEY",
                        format!("{EXTENSION} is only meaningful at the root of a table schema"),
                    );
                    diagnostic.pointer = Some(at.clone());
                    out.push(diagnostic);
                }
                // Instance data and the extension itself are not subschemas.
                if matches!(key.as_str(), "enum" | "const" | "default" | "examples" | EXTENSION) {
                    continue;
                }
                nested_extension(child, &at, false, out);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                nested_extension(item, &format!("{pointer}/{index}"), false, out);
            }
        }
        _ => {}
    }
}

/// The member name a `propertyNames` failure is about, however the evaluation
/// reached it: reported either as the wrapping `propertyNames` error or as the
/// inner failure whose instance is the name itself.
fn unknown_member(error: &jsonschema::ValidationError<'_>, pointer: &str, document: &Value) -> Option<String> {
    use jsonschema::error::ValidationErrorKind as Kind;
    let named = |name: &str| {
        document.pointer(pointer).and_then(Value::as_object).is_some_and(|object| object.contains_key(name))
    };
    match error.kind() {
        Kind::PropertyNames { error: inner } => inner.instance().as_str().filter(|name| named(name)).map(String::from),
        Kind::AnyOf { .. } => error.instance().as_str().filter(|name| named(name)).map(String::from),
        _ => None,
    }
}

/// One validation error against the dialect, stated for a schema author.
fn describe(error: &jsonschema::ValidationError<'_>, pointer: &str, document: &Value) -> Diagnostic {
    use jsonschema::error::ValidationErrorKind as Kind;
    let at = if pointer.is_empty() { "/" } else { pointer };
    match error.kind() {
        // The closure: a member name that is no keyword. Name the nearest one,
        // because nearly every such error is a typo.
        // `propertyNames` reports the member name itself as the instance, at
        // the pointer of the object holding it; however the evaluation got
        // there, that shape is an unknown keyword.
        _ if unknown_member(error, pointer, document).is_some() => {
            let name = unknown_member(error, pointer, document).unwrap_or_default();
            let nearest = known_keywords()
                .min_by_key(|candidate| strsim::levenshtein(&name, candidate))
                .filter(|candidate| strsim::levenshtein(&name, candidate) <= 3);
            let message = match nearest {
                Some(nearest) => format!(
                    "{name:?} is not a JSON Schema or reldir keyword (did you mean {nearest:?}?); \
                     an unknown keyword would constrain nothing"
                ),
                None => format!(
                    "{name:?} is not a JSON Schema or reldir keyword; an unknown keyword would \
                     constrain nothing (prefix third-party annotations with \"x-\")"
                ),
            };
            Diagnostic::error("SCHEMA_UNKNOWN_KEY", message).field(name)
        }
        Kind::Required { property } => Diagnostic::error(
            "SCHEMA_MISSING_REQUIRED",
            format!(
                "{at}: {} is required",
                property.as_str().map_or_else(|| property.to_string(), |p| format!("{p:?}"))
            ),
        ),
        Kind::AdditionalProperties { unexpected } => Diagnostic::error(
            "SCHEMA_UNKNOWN_KEY",
            format!("{at}: unknown member(s) {}", unexpected.join(", ")),
        ),
        _ => Diagnostic::error("SCHEMA_GRAMMAR", format!("{at}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users() -> Value {
        json!({
            "$schema": DIALECT_URI,
            "type": "object",
            "properties": {
                "id": { "type": "string", "format": "uuid" },
                "email": { "type": "string", "format": "email" },
                "role": { "type": "string", "enum": ["admin", "member"], "default": "member" },
                "team_id": { "type": ["string", "null"], "format": "uuid" },
                "amounts": {
                    "type": "array",
                    "items": { "type": "string", "x-reldir-type": "decimal" }
                },
                "rules": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "a": { "type": "integer", "x-reldir-type": "int" } },
                        "oneOf": [{ "required": ["a"] }, { "not": { "required": ["a"] } }]
                    }
                }
            },
            "required": ["id", "email", "amounts"],
            "additionalProperties": false,
            "if": { "properties": { "role": { "const": "admin" } }, "required": ["role"] },
            "then": { "required": ["team_id"] },
            "x-reldir": {
                "table": "users",
                "primaryKey": ["id"],
                "unique": [["email"]],
                "identityDomain": "people",
                "foreignKeys": [{
                    "from": ["team_id"],
                    "to": { "table": "teams" },
                    "onDelete": "set_null"
                }],
                "checks": [{ "name": "email_has_at", "expr": "email LIKE '%@%'" }],
                "assertions": [{ "name": "one_admin", "query": "SELECT id FROM users", "severity": "warning" }],
                "generated": { "id": "uuid" }
            }
        })
    }

    fn codes(document: &Value) -> Vec<String> {
        check_document(document, None)
            .unwrap()
            .into_iter()
            .map(|d| d.code)
            .collect()
    }

    #[test]
    fn test2020_the_dialect_accepts_every_standard_keyword_reldir_holds() {
        let found = check_document(&users(), None).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn test2021_unknown_keywords_are_refused_at_every_depth_with_a_suggestion() {
        let mut typo = users();
        typo["properties"]["rules"]["items"]["oneOf"][0]["requird"] = json!(["a"]);
        let found = check_document(&typo, None).unwrap();
        let hit = found
            .iter()
            .find(|d| d.code == "SCHEMA_UNKNOWN_KEY")
            .unwrap_or_else(|| panic!("a typo deep in a composition is caught: {found:?}"));
        assert!(hit.message.contains("\"required\""), "{}", hit.message);
        assert_eq!(hit.pointer.as_deref(), Some("/properties/rules/items/oneOf/0"));

        // Third-party annotations remain welcome; reldir's own namespace does not
        // tolerate a typo.
        let mut annotated = users();
        annotated["properties"]["id"]["x-ui-width"] = json!(3);
        assert!(codes(&annotated).is_empty());
        annotated["properties"]["id"]["x-reldir-tpye"] = json!("int");
        assert!(codes(&annotated).contains(&"SCHEMA_UNKNOWN_KEY".to_string()));
    }

    #[test]
    fn test2022_table_roots_must_state_what_a_table_needs() {
        for (label, mutate) in [
            ("no x-reldir", (|d: &mut Value| { d.as_object_mut().unwrap().remove("x-reldir"); }) as fn(&mut Value)),
            ("no additionalProperties", |d: &mut Value| { d.as_object_mut().unwrap().remove("additionalProperties"); }),
            ("wrong dialect", |d: &mut Value| d["$schema"] = json!("https://json-schema.org/draft/2020-12/schema")),
            ("array root", |d: &mut Value| d["type"] = json!("array")),
            ("unknown x-reldir key", |d: &mut Value| d["x-reldir"]["cascadeEverything"] = json!(true)),
            ("bad action", |d: &mut Value| d["x-reldir"]["foreignKeys"][0]["onDelete"] = json!("explode")),
            ("two targets", |d: &mut Value| d["x-reldir"]["foreignKeys"][0]["to"] = json!({"table": "a", "domain": "b"})),
            ("nested x-reldir", |d: &mut Value| d["properties"]["id"]["x-reldir"] = json!({})),
            ("bad tag", |d: &mut Value| d["properties"]["id"]["x-reldir-type"] = json!("bogus")),
            ("negative bound", |d: &mut Value| d["properties"]["email"]["minLength"] = json!(-1)),
        ] {
            let mut document = users();
            mutate(&mut document);
            assert!(!codes(&document).is_empty(), "{label} must be refused");
        }
    }

    #[test]
    fn test2023_the_dialect_declares_its_vocabularies_and_asserts_format() {
        let vocabularies = meta_schema()["$vocabulary"].as_object().unwrap();
        assert_eq!(vocabularies.get(VOCABULARY_URI), Some(&json!(true)));
        assert_eq!(
            vocabularies.get("https://json-schema.org/draft/2020-12/vocab/format-assertion"),
            Some(&json!(true)),
            "format asserts in this dialect"
        );
        assert_eq!(meta_schema()["$id"], json!(DIALECT_URI));
    }
}
