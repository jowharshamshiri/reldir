//! Canonical forms: one rendering per logical value, so identity never depends
//! on how a value happened to be written.

use crate::{
    schema::{AdditionalFields, ColumnType, Schema},
    value,
};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// The longest filename reldir writes, in bytes. Every mainstream filesystem
/// allows 255 (ext4, APFS, XFS, Btrfs in bytes; NTFS in UTF-16 units), and a
/// row filename is ASCII after percent-encoding, so one limit serves them all.
pub const MAX_FILENAME_BYTES: usize = 255;

/// A value with object keys sorted and NFC-normalized, strings NFC-normalized,
/// and negative zero collapsed: the form comparisons and hashes are taken over.
pub fn normalize(value: &Value) -> Value {
    match value {
        Value::Object(m) => {
            let mut entries: Vec<_> = m
                .iter()
                .map(|(key, value)| (key.nfc().collect::<String>(), value))
                .collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut out = Map::new();
            for (key, value) in entries {
                out.insert(key, normalize(value));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(normalize).collect()),
        Value::String(s) => Value::String(s.nfc().collect()),
        Value::Number(n) if n.as_f64() == Some(-0.0) && n.is_f64() => Value::Number(0.into()),
        _ => value.clone(),
    }
}

/// A row as reldir writes it: schema columns in schema order, each present
/// (absent ones as their default, else null), timestamps in UTC, values
/// normalized; then undeclared members, when the schema admits them, in name
/// order.
pub fn canonical_row(row: &Map<String, Value>, schema: &Schema) -> Value {
    let mut out = Map::new();
    for (name, column) in schema.columns() {
        let mut value = row
            .get(name)
            .cloned()
            .or_else(|| column.default().cloned())
            .unwrap_or(Value::Null);
        if column.kind() == &ColumnType::Timestamp
            && let Some(text) = value::textual(&value, column.kind())
        {
            value = Value::String(text);
        }
        out.insert(name.nfc().collect(), normalize(&value));
    }
    if schema.additional_fields() == AdditionalFields::Allow {
        let mut extra: Vec<_> = row
            .keys()
            .filter(|key| !schema.columns().contains_key(*key))
            .collect();
        extra.sort();
        for key in extra {
            out.insert(key.clone(), normalize(&row[key]));
        }
    }
    Value::Object(out)
}

/// The comparison rendering of a value: compact JSON of its normalized form.
pub fn compact(v: &Value) -> String {
    serde_json::to_string(&normalize(v)).expect("JSON values serialize")
}

pub fn pretty(v: &Value) -> Vec<u8> {
    pretty_with_indent(v, 2)
}

/// Indented JSON with exactly one trailing newline, as reldir writes files.
pub fn pretty_with_indent(v: &Value, indentation_width: usize) -> Vec<u8> {
    let indent = vec![b' '; indentation_width];
    let formatter = serde_json::ser::PrettyFormatter::with_indent(&indent);
    let mut b = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut b, formatter);
    v.serialize(&mut serializer).expect("JSON values serialize");
    b.push(b'\n');
    b
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The one filename a row may have, derived from its identity: each filename
/// column's canonical text, percent-encoded outside `[A-Za-z0-9._-]` (a
/// leading `.` included), joined with `,`. `None` when a column is absent or
/// null, so the row has no identity.
pub fn filename(schema: &Schema, row: &Map<String, Value>) -> Option<String> {
    let mut parts = vec![];
    let columns = schema.filename_columns();
    for name in columns {
        let column = schema.column(name)?;
        let logical = row.get(name).or(column.default())?;
        let text = value::textual(logical, column.kind())?;
        let mut encoded = percent_encode(text.as_bytes());
        if columns.len() == 1 && windows_reserved_component(&text) {
            let first = text.as_bytes().first()?;
            encoded = format!("%{first:02X}{}", &encoded[1..]);
        }
        parts.push(encoded);
    }
    Some(format!("{}.json", parts.join(",")))
}

/// Whether a filename fits every filesystem reldir supports.
pub fn filename_fits(name: &str) -> bool {
    name.len() <= MAX_FILENAME_BYTES
}

fn windows_reserved_component(value: &str) -> bool {
    let base = value.split('.').next().unwrap_or(value).to_ascii_lowercase();
    matches!(base.as_str(), "con" | "prn" | "aux" | "nul")
        || base
            .strip_prefix("com")
            .or_else(|| base.strip_prefix("lpt"))
            .is_some_and(|number| number.len() == 1 && matches!(number.as_bytes()[0], b'1'..=b'9'))
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut s = String::new();
    for (i, b) in bytes.iter().enumerate() {
        let allowed = b.is_ascii_alphanumeric()
            || matches!(*b, b'_' | b'-')
            || (*b == b'.' && i + 1 < bytes.len());
        if allowed && !(i == 0 && *b == b'.') {
            s.push(*b as char)
        } else {
            s.push_str(&format!("%{b:02X}"))
        }
    }
    s
}

/// The text a percent-encoded filename component encodes, if it is valid.
pub fn percent_decode(component: &str) -> Option<String> {
    let bytes = component.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = component.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Schema;
    use serde_json::json;

    fn schema(columns: &[(&str, &str)], primary_key: &[&str], extra: Value) -> Schema {
        let mut properties = Map::new();
        for (name, kind) in columns {
            properties.insert(
                (*name).into(),
                crate::schema::document::subschema(&ColumnType::from_name(kind).unwrap(), false),
            );
        }
        let mut document = json!({
            "$schema": crate::schema::meta::DIALECT_URI,
            "type": "object",
            "properties": properties,
            "required": primary_key,
            "additionalProperties": false,
            "x-reldir": {
                "table": "t",
                "primaryKey": primary_key
            }
        });
        if let Value::Object(extra) = extra {
            for (key, value) in extra {
                document["x-reldir"][key] = value;
            }
        }
        Schema::from_document(document, None).unwrap()
    }

    fn row(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect()
    }

    #[test]
    fn test1000_filename_encoding_is_unambiguous_and_reversible() {
        let s = schema(&[("id", "string")], &["id"], json!({}));
        for (id, expected) in [
            ("a/b", "a%2Fb.json"),
            ("a,b", "a%2Cb.json"),
            (".hidden", "%2Ehidden.json"),
            ("..", "%2E%2E.json"),
            ("plain-name_1.v2", "plain-name_1.v2.json"),
            ("é", "%C3%A9.json"),
        ] {
            let name = filename(&s, &row(&[("id", json!(id))])).unwrap();
            assert_eq!(name, expected);
            assert_eq!(percent_decode(name.trim_end_matches(".json")).as_deref(), Some(id));
        }
    }

    #[test]
    fn test1001_windows_reserved_names_are_escaped() {
        let s = schema(&[("id", "string")], &["id"], json!({}));
        for reserved in ["CON", "con", "PRN", "aux", "NUL", "COM1", "lpt9"] {
            let produced = filename(&s, &row(&[("id", json!(reserved))])).unwrap();
            assert!(produced.starts_with('%'), "{reserved} must be escaped, got {produced}");
        }
        assert_eq!(filename(&s, &row(&[("id", json!("console"))])), Some("console.json".into()));
        assert_eq!(filename(&s, &row(&[("id", json!("COM10"))])), Some("COM10.json".into()));
    }

    #[test]
    fn test1002_composite_filenames_join_encoded_components() {
        let s = schema(&[("a", "string"), ("b", "string")], &["a", "b"], json!({}));
        assert_eq!(filename(&s, &row(&[("a", json!("x")), ("b", json!("y"))])), Some("x,y.json".into()));
        assert_eq!(
            filename(&s, &row(&[("a", json!("x,y")), ("b", json!("z"))])),
            Some("x%2Cy,z.json".into())
        );
    }

    #[test]
    fn test1003_canonical_rows_follow_schema_then_lexicographic_order() {
        let s = schema(&[("zeta", "string"), ("alpha", "string"), ("nested", "json")], &["alpha"], json!({}));
        let value = canonical_row(
            &row(&[("nested", json!({"b": 1, "a": 2})), ("alpha", json!("A")), ("zeta", json!("Z"))]),
            &s,
        );
        let text = String::from_utf8(pretty_with_indent(&value, 2)).unwrap();
        assert!(text.find("zeta").unwrap() < text.find("alpha").unwrap(), "schema order wins: {text}");
        assert!(text.find("\"a\"").unwrap() < text.find("\"b\"").unwrap(), "nested keys sort: {text}");
    }

    #[test]
    fn test1004_normalisation_collapses_incidental_representations() {
        assert_eq!(compact(&json!(-0.0)), "0");
        assert_eq!(compact(&Value::String("e\u{0301}".into())), compact(&Value::String("\u{e9}".into())));
        assert_eq!(compact(&json!({ "e\u{0301}": 1 })), compact(&json!({ "\u{e9}": 1 })));
    }

    #[test]
    fn test1006_defaults_participate_in_the_canonical_value() {
        let mut s = schema(&[("id", "string"), ("tag", "string")], &["id"], json!({})).edit();
        s.set_default("tag", Some(json!("fallback")));
        let s = s.finish().unwrap();
        let omitted = canonical_row(&row(&[("id", json!("a"))]), &s);
        let explicit = canonical_row(&row(&[("id", json!("a")), ("tag", json!("fallback"))]), &s);
        assert_eq!(compact(&omitted), compact(&explicit));
    }

    #[test]
    fn test1007_pretty_output_honours_indentation_and_trailing_newline() {
        let value = json!({"a": 1});
        let two = String::from_utf8(pretty_with_indent(&value, 2)).unwrap();
        let four = String::from_utf8(pretty_with_indent(&value, 4)).unwrap();
        assert!(two.contains("\n  \"a\""));
        assert!(four.contains("\n    \"a\""));
        assert!(two.ends_with("}\n"));
        assert_eq!(two.matches('\n').count(), 3);
    }

    #[test]
    fn test2080_long_identities_are_measured_after_encoding() {
        let s = schema(&[("id", "string")], &["id"], json!({}));
        // 100 characters of two-byte UTF-8 encode to 600 bytes of filename.
        let long = "é".repeat(100);
        let name = filename(&s, &row(&[("id", json!(long))])).unwrap();
        assert!(!filename_fits(&name));
        assert!(filename_fits(&filename(&s, &row(&[("id", json!("short"))])).unwrap()));
    }
}
