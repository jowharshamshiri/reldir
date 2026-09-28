//! What a schema *is*, independent of how it is written down.
//!
//! A schema document is the only statement of what a table admits, so its
//! identity is a hash of the document itself -- after removing what carries no
//! meaning for validity:
//!
//! - layout: whitespace, member order and number spelling are erased by
//!   rendering the document in the JSON Canonicalization Scheme (RFC 8785);
//! - annotations: `description`, `title`, `$comment`, `examples`, `deprecated`,
//!   `readOnly` and `writeOnly` describe a schema without constraining a row,
//!   and custom `x-` members that are not reldir's are the same.
//!
//! Annotations are removed only where they are *keywords*. `properties` maps
//! column names to subschemas, so a column named `description` is a column, not
//! commentary, and survives. `enum`, `const`, `default` and `x-reldir` hold
//! instance data or relational facts and are kept verbatim.
//!
//! Identity is taken over the document rather than over a model decoded from
//! it, so there is no second encoding that could drift from the file: two
//! documents share an identity exactly when they impose the same rules.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Keywords that annotate a subschema without constraining an instance.
const ANNOTATIONS: &[&str] = &[
    "title",
    "description",
    "$comment",
    "examples",
    "deprecated",
    "readOnly",
    "writeOnly",
];

/// Keywords whose value is a map from names to subschemas.
const SUBSCHEMA_MAPS: &[&str] = &["properties", "patternProperties", "$defs", "dependentSchemas"];

/// Keywords whose value is one subschema.
const SUBSCHEMA_SINGLE: &[&str] = &[
    "items",
    "not",
    "if",
    "then",
    "else",
    "contains",
    "additionalProperties",
    "unevaluatedProperties",
    "unevaluatedItems",
    "propertyNames",
    "contentSchema",
];

/// Keywords whose value is a list of subschemas.
const SUBSCHEMA_LISTS: &[&str] = &["allOf", "anyOf", "oneOf", "prefixItems"];

/// The identity of a schema document: a SHA-256 over its canonical, annotation
/// free form, prefixed with the encoding version so that a future change to
/// what is stripped is a new namespace rather than a silent reinterpretation.
pub fn identity(document: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"reldir-schema-v2\0");
    hasher.update(canonical_json(&strip_annotations(document)).as_bytes());
    hex::encode(hasher.finalize())
}

/// The document with every annotation keyword removed.
pub fn strip_annotations(document: &Value) -> Value {
    strip_subschema(document)
}

fn strip_subschema(value: &Value) -> Value {
    let Value::Object(object) = value else {
        // A boolean subschema, or a value the meta-schema already refused.
        return value.clone();
    };
    let mut out = Map::new();
    for (key, child) in object {
        if ANNOTATIONS.contains(&key.as_str()) {
            continue;
        }
        if key.starts_with("x-") && key != "x-reldir" && key != "x-reldir-type" {
            continue;
        }
        let kept = if SUBSCHEMA_MAPS.contains(&key.as_str()) {
            match child {
                Value::Object(members) => Value::Object(
                    members
                        .iter()
                        .map(|(name, subschema)| (name.clone(), strip_subschema(subschema)))
                        .collect(),
                ),
                other => other.clone(),
            }
        } else if SUBSCHEMA_SINGLE.contains(&key.as_str()) {
            strip_subschema(child)
        } else if SUBSCHEMA_LISTS.contains(&key.as_str()) {
            match child {
                Value::Array(items) => Value::Array(items.iter().map(strip_subschema).collect()),
                other => other.clone(),
            }
        } else {
            child.clone()
        };
        out.insert(key.clone(), kept);
    }
    Value::Object(out)
}

/// Render a value in the JSON Canonicalization Scheme (RFC 8785).
///
/// Members are ordered by the UTF-16 code units of their names, strings use
/// the minimal JSON escapes, and numbers use the ECMAScript shortest
/// round-trip form, so every document with the same data renders to the same
/// bytes on every machine.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&canonical_number(number)),
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(members) => {
            let mut entries: Vec<(&String, &Value)> = members.iter().collect();
            entries.sort_by(|(left, _), (right, _)| {
                left.encode_utf16().cmp(right.encode_utf16())
            });
            out.push('{');
            for (index, (key, child)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_canonical(child, out);
            }
            out.push('}');
        }
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A number in ECMAScript's `Number.prototype.toString` form, as RFC 8785
/// requires.
fn canonical_number(number: &serde_json::Number) -> String {
    if let Some(integer) = number.as_i64() {
        return canonical_float(integer as f64).unwrap_or_else(|| integer.to_string());
    }
    if let Some(integer) = number.as_u64() {
        return canonical_float(integer as f64).unwrap_or_else(|| integer.to_string());
    }
    // `serde_json` never holds a non-finite number, so this always renders.
    canonical_float(number.as_f64().unwrap_or(0.0)).unwrap_or_else(|| "0".into())
}

fn canonical_float(value: f64) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    if value == 0.0 {
        return Some("0".into());
    }
    // Rust's `{:e}` renders the shortest digit string that round-trips, which
    // is the digit string ECMAScript's algorithm selects; only the placement of
    // the decimal point differs, and that is decided below by ECMAScript's rule.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e')?;
    let exponent: i32 = exponent.parse().ok()?;
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exponent + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let exponent = n - 1;
        let sign = if exponent < 0 { "-" } else { "+" };
        if k == 1 {
            format!("{digits}e{sign}{}", exponent.abs())
        } else {
            format!("{}.{}e{sign}{}", &digits[..1], &digits[1..], exponent.abs())
        }
    };
    Some(if value < 0.0 { format!("-{body}") } else { body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test2010_canonical_json_follows_rfc_8785() {
        // The RFC's own worked number examples.
        for (input, expected) in [
            (json!(0.0), "0"),
            (json!(-0.0), "0"),
            (json!(1.0), "1"),
            (json!(100), "100"),
            (json!(1e21), "1e+21"),
            (json!(1e20), "100000000000000000000"),
            (json!(0.000001), "0.000001"),
            (json!(0.0000001), "1e-7"),
            (json!(333333333.33333329), "333333333.3333333"),
            (json!(-1.5), "-1.5"),
            (json!(4.5), "4.5"),
            (json!(9007199254740992_u64), "9007199254740992"),
        ] {
            assert_eq!(canonical_json(&input), expected, "for {input}");
        }
        // Members sort by UTF-16 code units, not by UTF-8 bytes: U+E000 sorts
        // before U+1F600, whose surrogate pair begins with 0xD83D.
        let object = json!({"\u{1F600}": 1, "\u{E000}": 2, "a": 3});
        assert_eq!(canonical_json(&object), "{\"a\":3,\"\u{1F600}\":1,\"\u{E000}\":2}");
        assert_eq!(canonical_json(&json!("a\"b\n\u{1}")), "\"a\\\"b\\n\\u0001\"");
    }

    #[test]
    fn test2011_identity_ignores_layout_and_annotations_but_not_rules() {
        let base = json!({
            "type": "object",
            "properties": {"id": {"type": "string", "minLength": 1}},
            "x-reldir": {"table": "t"}
        });
        let annotated = json!({
            "description": "a table",
            "x-reldir": {"table": "t"},
            "properties": {"id": {"minLength": 1, "type": "string", "title": "Id", "$comment": "k"}},
            "type": "object",
            "x-editor": {"width": 3}
        });
        assert_eq!(identity(&base), identity(&annotated));

        // A different rule is a different schema.
        let stricter = json!({
            "type": "object",
            "properties": {"id": {"type": "string", "minLength": 2}},
            "x-reldir": {"table": "t"}
        });
        assert_ne!(identity(&base), identity(&stricter));

        // A column named like an annotation keyword is a column.
        let with_description_column = json!({
            "type": "object",
            "properties": {"id": {"type": "string", "minLength": 1}, "description": {"type": "string"}},
            "x-reldir": {"table": "t"}
        });
        assert_ne!(identity(&base), identity(&with_description_column));

        // Values that are instance data are never stripped.
        let with_enum = json!({"type": "string", "enum": ["description", "title"]});
        assert_eq!(strip_annotations(&with_enum), with_enum);

        // An unknown reldir keyword is a rule, not an annotation.
        let tagged = json!({"type": "integer", "x-reldir-type": "int"});
        assert_eq!(strip_annotations(&tagged), tagged);
    }
}
