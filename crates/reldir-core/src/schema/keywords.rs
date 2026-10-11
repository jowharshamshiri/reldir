//! reldir's lexical type rules, compiled into the JSON Schema validator.
//!
//! JSON Schema cannot say how a value is *written*: its `integer` accepts
//! `1.0`, it has no decimal, `format: uuid` accepts upper case, and in
//! 2020-12 `contentEncoding` only annotates. reldir's types are lexical because
//! a value's written form is what other programs read, names files, and is
//! hashed. The rules are registered with the validator as keywords, so they
//! apply wherever a subschema applies -- under `allOf` and `oneOf` branches,
//! `prefixItems`, `additionalProperties`, `patternProperties`, at the end of a
//! chain of `$ref`s -- and never only where a separate walker happened to look.
//!
//! - `x-reldir-type: "int"` -- an integer written without fraction or exponent,
//!   within 64 signed bits;
//! - `x-reldir-type: "float"` -- a number written with a fraction or an
//!   exponent, as a program that reads floats expects;
//! - `x-reldir-type: "decimal"` -- a string holding a decimal in canonical form;
//! - `x-reldir-type: "ulid"` -- 26 uppercase Crockford base32 characters;
//! - `format: "uuid"` -- canonical: lowercase and hyphenated;
//! - `contentEncoding: "base64"` -- standard base64 that decodes.

use jsonschema::{Keyword, ValidationError, ValidationOptions, paths::Location};
use serde_json::{Map, Value};

/// Whether a validation failure is one of reldir's lexical type rules -- a
/// value of the right JSON type written the wrong way -- rather than any other
/// violation of the schema.
pub(crate) fn is_lexical(error: &jsonschema::ValidationError<'_>) -> bool {
    use jsonschema::error::ValidationErrorKind as Kind;
    match error.kind() {
        Kind::Custom { keyword, .. } => {
            keyword == super::meta::TYPE_TAG || keyword == "contentEncoding"
        }
        Kind::Format { format } => format == "uuid",
        _ => false,
    }
}

/// Register every reldir keyword and format on a set of validator options.
pub(crate) fn register(options: ValidationOptions<'static>) -> ValidationOptions<'static> {
    options
        .with_keyword(super::meta::TYPE_TAG, type_tag)
        .with_keyword("contentEncoding", content_encoding)
        .with_format("uuid", canonical_uuid)
}

fn canonical_uuid(text: &str) -> bool {
    text.len() == 36
        && uuid::Uuid::parse_str(text).is_ok()
        && !text.bytes().any(|byte| byte.is_ascii_uppercase())
}

#[derive(Clone, Copy)]
enum Tag {
    Int,
    Float,
    Decimal,
    Ulid,
}

impl Tag {
    fn name(self) -> &'static str {
        match self {
            Tag::Int => "int",
            Tag::Float => "float",
            Tag::Decimal => "decimal",
            Tag::Ulid => "ulid",
        }
    }

    /// The reason a value is not written as the tag requires, or `None` when
    /// it is, or when the value is of a kind the tag does not govern (the
    /// subschema's `type` judges that).
    fn refusal(self, value: &Value) -> Option<&'static str> {
        match (self, value) {
            (Tag::Int, Value::Number(number)) => {
                let lexeme = number.to_string();
                let plain = !lexeme.contains(['.', 'e', 'E']);
                (!(plain && lexeme.parse::<i64>().is_ok())).then_some(
                    "an int is written as a whole number, without a fraction or exponent, within \
                     the 64-bit signed range",
                )
            }
            (Tag::Float, Value::Number(number)) => (!number.to_string().contains(['.', 'e', 'E']))
                .then_some(
                    "a float is written with a fraction or an exponent, such as 3.0, so programs \
                     that read it keep reading a float",
                ),
            (Tag::Decimal, Value::String(text)) => (!crate::value::canonical_decimal(text))
                .then_some(
                    "a decimal is written in canonical form: an optional minus, no leading zeros, \
                     no trailing fractional zeros, no plus sign",
                ),
            (Tag::Ulid, Value::String(text)) => (!(text.len() == 26
                && ulid::Ulid::from_string(text).is_ok()
                && !text.bytes().any(|byte| byte.is_ascii_lowercase())))
            .then_some("a ulid is 26 uppercase Crockford base32 characters"),
            _ => None,
        }
    }
}

struct TypeTag(Tag);

impl<'i> Keyword<'i> for TypeTag {
    fn validate(&self, instance: &'i Value) -> Result<(), ValidationError<'i>> {
        match self.0.refusal(instance) {
            None => Ok(()),
            Some(reason) => Err(ValidationError::custom(format!(
                "is not written as {} requires: {reason}",
                self.0.name()
            ))),
        }
    }

    fn is_valid(&self, instance: &'i Value) -> bool {
        self.0.refusal(instance).is_none()
    }
}

fn type_tag<'a>(
    parent: &'a Map<String, Value>,
    value: &'a Value,
    _location: Location,
) -> Result<Box<dyn for<'i> Keyword<'i>>, ValidationError<'a>> {
    let tag = match value.as_str() {
        Some("int") => Tag::Int,
        Some("float") => Tag::Float,
        Some("decimal") => Tag::Decimal,
        Some("ulid") => Tag::Ulid,
        _ => {
            return Err(ValidationError::schema(format!(
                "{} must be one of \"int\", \"float\", \"decimal\", \"ulid\", not {value}",
                super::meta::TYPE_TAG
            )));
        }
    };
    // A tag refines exactly one JSON type; one that contradicts its
    // subschema's `type` would be either vacuous or unsatisfiable, and both
    // mean the author meant something else.
    let governs = match tag {
        Tag::Int => "integer",
        Tag::Float => "number",
        Tag::Decimal | Tag::Ulid => "string",
    };
    let declared: Vec<&str> = match parent.get("type") {
        Some(Value::String(name)) => vec![name.as_str()],
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    let concrete: Vec<&str> = declared
        .into_iter()
        .filter(|name| *name != "null")
        .collect();
    if concrete != [governs] {
        return Err(ValidationError::schema(format!(
            "{} {:?} refines {governs:?}, but the subschema's type is {}",
            super::meta::TYPE_TAG,
            tag.name(),
            match parent.get("type") {
                Some(declared) => declared.to_string(),
                None => "not stated".into(),
            }
        )));
    }
    Ok(Box::new(TypeTag(tag)))
}

struct Base64;

impl<'i> Keyword<'i> for Base64 {
    fn validate(&self, instance: &'i Value) -> Result<(), ValidationError<'i>> {
        if Keyword::is_valid(self, instance) {
            Ok(())
        } else {
            Err(ValidationError::custom(
                "is not standard base64 (RFC 4648 alphabet, with padding)",
            ))
        }
    }

    fn is_valid(&self, instance: &'i Value) -> bool {
        use base64::Engine;
        instance.as_str().is_none_or(|text| {
            base64::engine::general_purpose::STANDARD
                .decode(text)
                .is_ok()
        })
    }
}

fn content_encoding<'a>(
    _parent: &'a Map<String, Value>,
    value: &'a Value,
    _location: Location,
) -> Result<Box<dyn for<'i> Keyword<'i>>, ValidationError<'a>> {
    match value.as_str() {
        Some("base64") => Ok(Box::new(Base64)),
        _ => Err(ValidationError::schema(format!(
            "contentEncoding {value} is not one reldir decodes; only \"base64\" is"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn validator(schema: Value) -> jsonschema::Validator {
        register(jsonschema::options().with_draft(jsonschema::Draft::Draft202012))
            .should_validate_formats(true)
            .build(&schema)
            .unwrap()
    }

    fn parse(text: &str) -> Value {
        crate::json::parse_str(text).unwrap()
    }

    #[test]
    fn test8910_type_tags_hold_at_every_depth_and_through_references() {
        let schema = json!({
            "$defs": {
                "count": { "$ref": "#/$defs/plain" },
                "plain": { "type": "integer", "x-reldir-type": "int" }
            },
            "type": "object",
            "properties": {
                "branches": { "allOf": [ { "type": "number", "x-reldir-type": "float" } ] },
                "chained": { "$ref": "#/$defs/count" },
                "tuple": { "prefixItems": [ { "type": "string", "x-reldir-type": "ulid" } ] },
                "map": { "additionalProperties": { "type": "string", "x-reldir-type": "decimal" } }
            }
        });
        let validator = validator(schema);
        assert!(validator.is_valid(&parse(
            r#"{"branches": 3.0, "chained": 7, "tuple": ["01ARZ3NDEKTSV4RRFFQ69G5FAV"], "map": {"a": "1.5"}}"#
        )));
        for wrong in [
            r#"{"branches": 3}"#,
            r#"{"chained": 7.0}"#,
            r#"{"chained": 1e2}"#,
            r#"{"chained": 99999999999999999999}"#,
            r#"{"tuple": ["01arz3ndektsv4rrffq69g5fav"]}"#,
            r#"{"map": {"a": "1.50"}}"#,
        ] {
            assert!(
                !validator.is_valid(&parse(wrong)),
                "{wrong} must be refused"
            );
        }
    }

    #[test]
    fn test8911_a_tag_that_contradicts_its_type_does_not_compile() {
        for schema in [
            json!({ "type": "number", "x-reldir-type": "int" }),
            json!({ "type": ["string", "null"], "x-reldir-type": "float" }),
            json!({ "x-reldir-type": "decimal" }),
            json!({ "type": "string", "x-reldir-type": "money" }),
        ] {
            let built = register(jsonschema::options().with_draft(jsonschema::Draft::Draft202012))
                .build(&schema);
            assert!(built.is_err(), "{schema} must not compile");
        }
        // A nullable tagged type is one concrete type plus null: allowed.
        assert!(
            register(jsonschema::options().with_draft(jsonschema::Draft::Draft202012))
                .build(&json!({ "type": ["string", "null"], "x-reldir-type": "decimal" }))
                .is_ok()
        );
    }

    #[test]
    fn test8912_uuids_are_canonical_and_base64_decodes() {
        let validator = validator(json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "format": "uuid" },
                "blob": { "type": "string", "contentEncoding": "base64" }
            }
        }));
        assert!(
            validator
                .is_valid(&json!({"id": "0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e", "blob": "aGk="}))
        );
        assert!(!validator.is_valid(&json!({"id": "0193B1F4-7C3A-7B1E-9C2D-3F4A5B6C7D8E"})));
        assert!(!validator.is_valid(&json!({"id": "0193b1f47c3a7b1e9c2d3f4a5b6c7d8e"})));
        assert!(!validator.is_valid(&json!({"blob": "not base64!"})));
    }
}
