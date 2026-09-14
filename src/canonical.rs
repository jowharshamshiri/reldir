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
