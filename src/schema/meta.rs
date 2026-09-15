//! The JDB dialect of JSON Schema 2020-12.
//!
//! jdb's schemas are JSON Schema documents, but jdb does not accept arbitrary
//! JSON Schema: a document that says `oneOf` is asking for a semantics jdb has
//! no relational meaning for, and quietly ignoring it would make the file and
//! the database disagree. So the accepted surface is a dialect -- 2020-12 plus
//! one vocabulary of jdb's own -- and it is declared as such rather than merely
//! documented.
//!
//! Declaring `$vocabulary` is what makes the restriction honest. A conforming
//! implementation reading one of these documents learns that
//! `https://jdb.dev/vocab/jdb-1` is required, and therefore that it cannot
//! fully process the schema without understanding jdb's keywords. Without that
//! declaration a generic validator would silently ignore `x-jdb` and conclude a
//! schema was satisfied when jdb's own rules were never checked.
//!
//! The URI is an identifier, not a location. This binary never fetches it: the
//! meta-schema is compiled in below and registered offline. jdb should also
//! serve it at that address so editors can complete these documents, but
//! nothing here depends on the network.
//!
//! One caveat the documentation must not overstate: in 2020-12 `format` is an
//! annotation unless the format-assertion vocabulary is enabled, which this
//! dialect does not enable. A generic validator therefore understands the
//! structure of a jdb schema and checks its shape, while jdb remains the
//! authority on what a `uuid`, `ulid`, `decimal`, or `timestamp` actually
//! admits.

use crate::diagnostic::{DbError, Result};
use serde_json::Value;
use std::sync::OnceLock;

/// The dialect's identifier, and the value of `$schema` in every jdb schema.
pub const DIALECT_URI: &str = "https://jdb.dev/schema/jdb-1";

/// The vocabulary that carries jdb's relational keywords.
pub const VOCABULARY_URI: &str = "https://jdb.dev/vocab/jdb-1";

/// The namespace holding every relational fact that JSON Schema has no keyword
/// for. One object, so that a reader can see at a glance which parts of a
/// document are jdb's and which are standard.
pub const EXTENSION: &str = "x-jdb";

/// The per-subschema type tag.
///
/// Used only where a standard keyword cannot distinguish two jdb types: jdb's
/// `int` is lexical where JSON Schema's `integer` is mathematical, and
/// `decimal` and `ulid` are strings carrying application semantics. It sits on
/// the subschema it describes rather than in a root-level map, so it works at
/// any nesting depth -- `array<decimal>` has no name to key such a map by.
pub const TYPE_TAG: &str = "x-jdb-type";

/// The meta-schema, compiled in.
///
/// It describes the shape of `x-jdb` and constrains the standard keywords to
/// the subset the codec accepts. It is deliberately not a complete description
/// of every rule jdb enforces: cross-schema facts such as foreign-key targets
/// cannot be expressed in a single document, and `json_schema::decode` reports
/// those with their own diagnostics.
pub fn meta_schema() -> &'static Value {
    static META: OnceLock<Value> = OnceLock::new();
    META.get_or_init(|| {
        serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": DIALECT_URI,
            "$vocabulary": {
                "https://json-schema.org/draft/2020-12/vocab/core": true,
                "https://json-schema.org/draft/2020-12/vocab/applicator": true,
                "https://json-schema.org/draft/2020-12/vocab/validation": true,
                "https://json-schema.org/draft/2020-12/vocab/meta-data": true,
                "https://json-schema.org/draft/2020-12/vocab/format-annotation": true,
                "https://json-schema.org/draft/2020-12/vocab/content": true,
                // Required: a reader that does not understand jdb's relational
                // keywords cannot claim to have processed the document.
                "https://jdb.dev/vocab/jdb-1": true,
            },
            "title": "jdb table schema",
            "type": "object",
            "required": ["type", "properties", "x-jdb"],
            "properties": {
                "$schema": { "const": DIALECT_URI },
                "type": { "const": "object" },
                "title": { "type": "string" },
                "description": { "type": "string" },
                "properties": {
                    "type": "object",
                    "minProperties": 1,
                    "additionalProperties": { "$ref": "#/$defs/column" },
                },
                "required": { "type": "array", "items": { "type": "string" } },
                "additionalProperties": { "type": "boolean" },
                "x-jdb": { "$ref": "#/$defs/extension" },
            },
            "$defs": {
                // One column. Recursive through `items` and `properties`,
                // because nested shape is part of a column's type: an
                // array<decimal> and an array<string> are different columns.
                "column": {
                    "type": "object",
                    "properties": {
                        "type": {
                            "anyOf": [
                                { "enum": ["boolean", "integer", "number", "string", "array", "object", "null"] },
                                {
                                    "type": "array",
                                    "items": { "enum": ["boolean", "integer", "number", "string", "array", "object", "null"] },
                                },
                            ],
                        },
                        // Present only where a standard keyword cannot tell two
                        // jdb types apart.
                        "x-jdb-type": { "enum": ["int", "decimal", "ulid"] },
                        "format": { "type": "string" },
                        "pattern": { "type": "string" },
                        "contentEncoding": { "type": "string" },
                        "enum": { "type": "array", "minItems": 1 },
                        "default": true,
                        "description": { "type": "string" },
                        "items": { "$ref": "#/$defs/column" },
                        "properties": {
                            "type": "object",
                            "additionalProperties": { "$ref": "#/$defs/column" },
                        },
                        "required": { "type": "array", "items": { "type": "string" } },
                        "additionalProperties": { "type": "boolean" },
                        "x-jdb-column-order": { "type": "array", "items": { "type": "string" } },
                        // Standard annotations carry no relational meaning and
                        // are preserved verbatim rather than rejected.
                        "title": { "type": "string" },
                        "$comment": { "type": "string" },
                        "examples": { "type": "array" },
                        "readOnly": { "type": "boolean" },
                        "deprecated": { "type": "boolean" },
                    },
                },
                "extension": {
                    "type": "object",
                    "required": ["table", "primaryKey", "columnOrder"],
                    "additionalProperties": false,
                    "properties": {
                        "table": { "type": "string" },
                        "schemaVersion": { "type": "integer", "minimum": 0 },
                        "schemaFormat": { "type": "integer", "minimum": 0 },
                        "primaryKey": { "$ref": "#/$defs/columnList" },
                        // Column order is logical state: rows are written in it,
                        // so two schemas ordering their columns differently are
                        // different schemas. JSON object members are unordered,
                        // so the order is carried explicitly.
                        "columnOrder": { "$ref": "#/$defs/columnList" },
                        "unique": { "$ref": "#/$defs/columnLists" },
                        "indexes": { "$ref": "#/$defs/columnLists" },
                        "foreignKeys": {
                            "type": "array",
                            "items": { "$ref": "#/$defs/foreignKey" },
                        },
                        "checks": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["name", "expr"],
                                "additionalProperties": false,
                                "properties": {
                                    "name": { "type": "string", "minLength": 1 },
                                    "expr": { "type": "string", "minLength": 1 },
                                },
                            },
                        },
                        "generated": {
                            "type": "object",
                            "additionalProperties": {
                                "enum": ["uuid", "ulid", "now", "sequence"],
                            },
                        },
                        "filename": { "$ref": "#/$defs/columnList" },
                    },
                },
                // A foreign key is not a `$ref`. `$ref` means "apply this schema
                // here" -- composition -- and cannot express a composite key, a
                // referential action, or the existence of a row elsewhere.
                // Using it would make the format look standard while making its
                // meaning false.
                "foreignKey": {
                    "type": "object",
                    "required": ["columns", "references"],
                    "additionalProperties": false,
                    "properties": {
                        "columns": { "$ref": "#/$defs/columnList" },
                        "references": {
                            "type": "object",
                            "required": ["table", "columns"],
                            "additionalProperties": false,
                            "properties": {
                                "table": { "type": "string" },
                                "columns": { "$ref": "#/$defs/columnList" },
                            },
                        },
                        "onDelete": { "$ref": "#/$defs/action" },
                        "onUpdate": { "$ref": "#/$defs/action" },
                    },
                },
                "action": {
                    "enum": ["restrict", "cascade", "set_null", "set_default", "no_action"],
                },
                "columnList": {
                    "type": "array",
                    "minItems": 1,
                    "items": { "type": "string" },
                },
                "columnLists": {
                    "type": "array",
                    "items": { "$ref": "#/$defs/columnList" },
                },
            },
        })
    })
}

/// The compiled validator for the dialect.
///
/// Built once. The registry is populated from the compiled-in document, so
/// construction never touches the network; a failure here is a defect in this
/// file rather than a condition a user can provoke, and is reported as an
/// internal fault rather than a schema diagnostic.
pub fn validator() -> Result<&'static jsonschema::Validator> {
    static VALIDATOR: OnceLock<std::result::Result<jsonschema::Validator, String>> =
        OnceLock::new();
    match VALIDATOR.get_or_init(build) {
        Ok(validator) => Ok(validator),
        Err(error) => Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("the bundled jdb dialect is not a valid meta-schema: {error}"),
            6,
        )),
    }
}

fn build() -> std::result::Result<jsonschema::Validator, String> {
    // Every `$ref` in the document points inside it, so compilation resolves
    // them without consulting a registry and without touching the network.
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(meta_schema())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A document in the shape the codec emits, used to check that the dialect
    /// accepts what jdb actually writes.
    fn users() -> Value {
        json!({
            "$schema": DIALECT_URI,
            "type": "object",
            "properties": {
                "id": { "type": "string", "format": "uuid" },
                "email": { "type": "string" },
                "role": { "type": "string", "enum": ["admin", "member"], "default": "member" },
                "team_id": { "type": ["string", "null"], "format": "uuid" },
                "amounts": {
                    "type": "array",
                    "items": { "type": "string", "x-jdb-type": "decimal", "pattern": "^-?[0-9]+$" },
                },
            },
            "required": ["id", "email", "amounts"],
            "additionalProperties": false,
            "x-jdb": {
                "table": "users",
                "schemaVersion": 1,
                "primaryKey": ["id"],
                "columnOrder": ["id", "email", "role", "team_id", "amounts"],
                "unique": [["email"]],
                "indexes": [["team_id"]],
                "foreignKeys": [{
                    "columns": ["team_id"],
                    "references": { "table": "teams", "columns": ["id"] },
                    "onDelete": "set_null",
                    "onUpdate": "restrict",
                }],
                "checks": [{ "name": "email_has_at", "expr": "email LIKE '%@%'" }],
                "generated": { "id": "uuid" },
            },
        })
    }

    /// The dialect describes the documents jdb writes.
    ///
    /// A meta-schema that compiles is not thereby correct: it has to accept a
    /// real schema and reject a malformed one. Without both halves a later
    /// codec test would be calibrated against a broken dialect and would
    /// confirm whatever the dialect happened to say.
    #[test]
    fn test1128_the_dialect_accepts_a_schema_jdb_would_write() {
        let validator = validator().expect("the bundled dialect compiles");
        let document = users();
        let errors: Vec<String> = validator
            .iter_errors(&document)
            .map(|error| format!("{} at {}", error, error.instance_path()))
            .collect();
        assert!(
            errors.is_empty(),
            "the dialect must accept what the codec emits: {errors:?}"
        );
    }

    /// Each rejection is a distinct way a document could fail to describe a
    /// table, and each must be caught rather than passed through.
    #[test]
    fn test1129_the_dialect_rejects_documents_that_cannot_describe_a_table() {
        let validator = validator().expect("the bundled dialect compiles");

        // Without `x-jdb` there is no table, no primary key, no column order:
        // valid JSON Schema, but not a relation.
        let mut missing_extension = users();
        missing_extension.as_object_mut().unwrap().remove("x-jdb");
        assert!(
            !validator.is_valid(&missing_extension),
            "a document with no x-jdb names no table"
        );

        // Column order is logical state, so a document that omits it does not
        // determine the bytes its rows would be written in.
        let mut no_order = users();
        no_order["x-jdb"]
            .as_object_mut()
            .unwrap()
            .remove("columnOrder");
        assert!(
            !validator.is_valid(&no_order),
            "columnOrder is required: rows are written in it"
        );

        // An unknown key inside `x-jdb` is a relational claim jdb has no
        // meaning for, and silently ignoring it would let the file and the
        // database disagree.
        let mut unknown = users();
        unknown["x-jdb"]["cascadeEverything"] = json!(true);
        assert!(
            !validator.is_valid(&unknown),
            "unknown x-jdb keys must be refused, not ignored"
        );

        // Referential actions are a closed set; anything else has no defined
        // behaviour.
        let mut bad_action = users();
        bad_action["x-jdb"]["foreignKeys"][0]["onDelete"] = json!("explode");
        assert!(
            !validator.is_valid(&bad_action),
            "an undefined referential action must be refused"
        );

        // The root of a table schema describes an object with named columns.
        let mut not_object = users();
        not_object["type"] = json!("array");
        assert!(
            !validator.is_valid(&not_object),
            "a table is an object of columns"
        );
    }

    /// The dialect declares its own vocabulary as required, which is what tells
    /// a generic validator that it cannot fully process these documents on its
    /// own. Without the declaration `x-jdb` would look like an ignorable
    /// extension and a schema could be reported satisfied while none of jdb's
    /// rules had been checked.
    #[test]
    fn test1130_the_dialect_declares_its_vocabulary_as_required() {
        let vocabularies = meta_schema()["$vocabulary"]
            .as_object()
            .expect("the dialect declares $vocabulary");
        assert_eq!(
            vocabularies.get(VOCABULARY_URI),
            Some(&json!(true)),
            "jdb's own vocabulary must be declared required"
        );
        assert_eq!(meta_schema()["$id"], json!(DIALECT_URI));
    }
}
