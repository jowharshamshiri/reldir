use crate::{schema::Schema, value};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

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
        Value::Number(n) if n.as_f64() == Some(-0.0) => Value::Number(0.into()),
        _ => value.clone(),
    }
}

pub fn canonical_row(row: &Map<String, Value>, schema: &Schema) -> Value {
    let mut out = Map::new();
    for (name, col) in &schema.columns {
        let mut v = row
            .get(name)
            .cloned()
            .or_else(|| col.default.clone())
            .unwrap_or(Value::Null);
        if col.kind == crate::schema::ColumnType::Timestamp
            && let Some(s) = value::textual(&v, col)
        {
            v = Value::String(s);
        }
        out.insert(name.nfc().collect(), normalize(&v));
    }
    if schema.additional_fields == crate::schema::AdditionalFields::Allow {
        let mut extra: Vec<_> = row
            .keys()
            .filter(|k| !schema.columns.contains_key(*k))
            .collect();
        extra.sort();
        for k in extra {
            out.insert(k.clone(), normalize(&row[k]));
        }
    }
    Value::Object(out)
}

pub fn compact(v: &Value) -> String {
    serde_json::to_string(&normalize(v)).expect("JSON values serialize")
}
pub fn pretty(v: &Value) -> Vec<u8> {
    pretty_with_indent(v, 2)
}
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

pub fn filename(schema: &Schema, row: &Map<String, Value>) -> Option<String> {
    let mut parts = vec![];
    for n in schema.filename_columns() {
        let c = schema.columns.get(n)?;
        let logical = row.get(n).or(c.default.as_ref())?;
        let t = value::textual(logical, c)?;
        let mut encoded = percent_encode(t.as_bytes());
        if schema.filename_columns().len() == 1 && windows_reserved_component(&t) {
            let first = t.as_bytes().first()?;
            encoded = format!("%{first:02X}{}", &encoded[1..]);
        }
        parts.push(encoded);
    }
    Some(format!("{}.json", parts.join(",")))
}
fn windows_reserved_component(value: &str) -> bool {
    let base = value
        .split('.')
        .next()
        .unwrap_or(value)
        .to_ascii_lowercase();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{AdditionalFields, Column, ColumnType, Schema, Storage};
    use indexmap::IndexMap;
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
            description: None,
        }
    }

    fn schema(columns: &[(&str, ColumnType)], primary_key: &[&str]) -> Schema {
        let mut map = IndexMap::new();
        for (name, kind) in columns {
            map.insert((*name).to_string(), column(kind.clone()));
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
        }
    }

    fn row(pairs: &[(&str, Value)]) -> Map<String, Value> {
        let mut map = Map::new();
        for (key, value) in pairs {
            map.insert((*key).to_string(), value.clone());
        }
        map
    }

    /// Section 10: the filename rule percent-encodes every byte outside
    /// [A-Za-z0-9._-], so a value can never escape its table directory or
    /// collide with the separator.
    #[test]
    fn test1000_filename_encoding_is_unambiguous_and_reversible() {
        let s = schema(&[("id", ColumnType::String)], &["id"]);

        // A path separator must never survive into the filename.
        assert_eq!(
            filename(&s, &row(&[("id", json!("a/b"))])),
            Some("a%2Fb.json".into())
        );
        // The composite separator itself is always encoded.
        assert_eq!(
            filename(&s, &row(&[("id", json!("a,b"))])),
            Some("a%2Cb.json".into())
        );
        // A leading dot is encoded so a row can never become a hidden file.
        assert_eq!(
            filename(&s, &row(&[("id", json!(".hidden"))])),
            Some("%2Ehidden.json".into())
        );
        // Parent-directory traversal is encoded byte for byte.
        assert_eq!(
            filename(&s, &row(&[("id", json!(".."))])),
            Some("%2E%2E.json".into())
        );
        // Ordinary characters stay literal so files remain human-readable.
        assert_eq!(
            filename(&s, &row(&[("id", json!("plain-name_1.v2"))])),
            Some("plain-name_1.v2.json".into())
        );
        // Multi-byte UTF-8 is encoded per byte, uppercase hex.
        assert_eq!(
            filename(&s, &row(&[("id", json!("é"))])),
            Some("%C3%A9.json".into())
        );
    }

    /// Section 56: a single-component key that would form a Windows reserved
    /// device name is encoded so the path is portable.
    #[test]
    fn test1001_windows_reserved_names_are_escaped() {
        let s = schema(&[("id", ColumnType::String)], &["id"]);
        for reserved in ["CON", "con", "PRN", "aux", "NUL", "COM1", "lpt9"] {
            let produced = filename(&s, &row(&[("id", json!(reserved))])).unwrap();
            assert!(
                produced.starts_with('%'),
                "{reserved} must be escaped, got {produced}"
            );
        }
        // A name merely starting with a reserved prefix is not reserved.
        assert_eq!(
            filename(&s, &row(&[("id", json!("console"))])),
            Some("console.json".into())
        );
        // COM10 is not a reserved device name.
        assert_eq!(
            filename(&s, &row(&[("id", json!("COM10"))])),
            Some("COM10.json".into())
        );
    }

    /// Section 10: a composite filename key joins its components with a comma,
    /// and each component is encoded independently.
    #[test]
    fn test1002_composite_filenames_join_encoded_components() {
        let mut s = schema(
            &[("a", ColumnType::String), ("b", ColumnType::String)],
            &["a", "b"],
        );
        s.storage = Some(Storage {
            filename: vec!["a".into(), "b".into()],
        });
        assert_eq!(
            filename(&s, &row(&[("a", json!("x")), ("b", json!("y"))])),
            Some("x,y.json".into())
        );
        // A comma inside a component cannot be confused with the separator.
        assert_eq!(
            filename(&s, &row(&[("a", json!("x,y")), ("b", json!("z"))])),
            Some("x%2Cy,z.json".into())
        );
    }

    /// Section 20: canonicalisation orders row keys by schema column order and
    /// nested object keys lexicographically, so the same logical state always
    /// produces the same bytes.
    #[test]
    fn test1003_canonical_rows_follow_schema_then_lexicographic_order() {
        let s = schema(
            &[
                ("zeta", ColumnType::String),
                ("alpha", ColumnType::String),
                ("nested", ColumnType::Json),
            ],
            &["alpha"],
        );
        // Row written in an arbitrary order with unsorted nested keys.
        let value = canonical_row(
            &row(&[
                ("nested", json!({"b": 1, "a": 2})),
                ("alpha", json!("A")),
                ("zeta", json!("Z")),
            ]),
            &s,
        );
        // The written bytes are the canonical form the spec governs: rows are
        // serialized in schema column order, and only the keys *inside* an
        // object or json value are lexicographic.
        let text = String::from_utf8(pretty_with_indent(&value, 2)).unwrap();
        let zeta = text.find("zeta").unwrap();
        let alpha = text.find("alpha").unwrap();
        assert!(zeta < alpha, "schema column order must win: {text}");
        assert!(
            text.find("\"a\"").unwrap() < text.find("\"b\"").unwrap(),
            "nested keys must be lexicographic: {text}"
        );

        // `compact` is a comparison rendering, not the row's written form: it
        // imposes a total lexicographic order so that two logically equal
        // values always produce the same key. Callers use it for keys and
        // value comparison, never to serialize a row to disk.
        let comparison = compact(&value);
        assert!(
            comparison.find("alpha").unwrap() < comparison.find("zeta").unwrap(),
            "comparison rendering is totally ordered: {comparison}"
        );
    }

    /// Section 20: numeric and Unicode normalisation give one identity per
    /// logical value.
    #[test]
    fn test1004_normalisation_collapses_incidental_representations() {
        // Negative zero is a distinct IEEE value but not a distinct logical one.
        assert_eq!(compact(&json!(-0.0)), "0");
        // Strings are NFC, so a decomposed sequence hashes as its composed form.
        let decomposed = Value::String("e\u{0301}".into());
        let composed = Value::String("\u{e9}".into());
        assert_eq!(compact(&decomposed), compact(&composed));
        // Object keys are normalised too.
        let a = json!({ "e\u{0301}": 1 });
        let b = json!({ "\u{e9}": 1 });
        assert_eq!(compact(&a), compact(&b));
    }

    /// Section 20: hashing covers the canonical logical value, so formatting
    /// differences in the source bytes cannot change identity.
    #[test]
    fn test1005_hashing_is_independent_of_incidental_formatting() {
        let s = schema(
            &[("id", ColumnType::String), ("n", ColumnType::Int)],
            &["id"],
        );
        // Hash what the state root actually hashes: the serialized canonical
        // row (see `metadata::state`), not a comparison rendering.
        let declared_order = canonical_row(&row(&[("id", json!("a")), ("n", json!(1))]), &s);
        let source_order = canonical_row(&row(&[("n", json!(1)), ("id", json!("a"))]), &s);
        assert_eq!(
            hash_bytes(&serde_json::to_vec(&declared_order).unwrap()),
            hash_bytes(&serde_json::to_vec(&source_order).unwrap()),
            "key order in the source must not affect the logical hash"
        );
    }

    /// Section 10: a column absent from the row body reads as its declared
    /// default, so identity and hashing see the same logical value either way.
    #[test]
    fn test1006_defaults_participate_in_the_canonical_value() {
        let mut s = schema(
            &[("id", ColumnType::String), ("tag", ColumnType::String)],
            &["id"],
        );
        s.columns.get_mut("tag").unwrap().default = Some(json!("fallback"));
        let omitted = canonical_row(&row(&[("id", json!("a"))]), &s);
        let explicit = canonical_row(&row(&[("id", json!("a")), ("tag", json!("fallback"))]), &s);
        assert_eq!(compact(&omitted), compact(&explicit));
    }

    /// Section 20: binary writes use the configured indentation and always end
    /// with exactly one trailing newline.
    #[test]
    fn test1007_pretty_output_honours_indentation_and_trailing_newline() {
        let value = json!({"a": 1});
        let two = String::from_utf8(pretty_with_indent(&value, 2)).unwrap();
        let four = String::from_utf8(pretty_with_indent(&value, 4)).unwrap();
        assert!(two.contains("\n  \"a\""), "two-space indent: {two:?}");
        assert!(four.contains("\n    \"a\""), "four-space indent: {four:?}");
        assert!(two.ends_with("}\n"));
        assert_eq!(two.matches('\n').count(), 3);
    }
}
