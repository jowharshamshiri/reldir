//! Canonical forms: one rendering per logical value, so identity never depends
//! on how a value happened to be formatted.
//!
//! A row's logical value is what SQL, keys and references see: its members
//! regardless of order, its numbers by exact value (`1.50` is `1.5`), its
//! strings and member names exactly as written. Formatting, member order and
//! number spelling are not part of it -- they are how the file is written, and
//! reldir keeps them as they are, never rewriting a value it did not change.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// The longest filename reldir writes, in bytes. Every mainstream filesystem
/// allows 255 (ext4, APFS, XFS, Btrfs in bytes; NTFS in UTF-16 units), and a
/// row filename is ASCII after percent-encoding, so one limit serves them all.
pub const MAX_FILENAME_BYTES: usize = 255;

/// A value with object members sorted by name and numbers in their canonical
/// decimal spelling: the form comparisons and hashes are taken over. Strings
/// and names are left exactly as written.
pub fn normalize(value: &Value) -> Value {
    match value {
        Value::Object(members) => {
            let mut entries: Vec<(&String, &Value)> = members.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), normalize(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize).collect()),
        Value::Number(number) => crate::value::normalize_numeric(&number.to_string())
            .and_then(|text| text.parse().ok())
            .map(Value::Number)
            .unwrap_or_else(|| value.clone()),
        other => other.clone(),
    }
}

/// A row's identity in the state hash and in recorded history: the SHA-256 of
/// its compact rendering with members sorted, so neither formatting nor member
/// order is a change.
pub fn row_hash(row: &Value) -> String {
    hash_bytes(compact(row).as_bytes())
}

/// The comparison rendering of a value: compact JSON of its normalized form.
pub fn compact(v: &Value) -> String {
    serde_json::to_string(&normalize(v)).expect("JSON values serialize")
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
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
    use serde_json::json;

    #[test]
    fn test1004_normalisation_collapses_number_spelling_and_keeps_strings_exact() {
        let parse = |text: &str| crate::json::parse_str(text).unwrap();
        assert_eq!(compact(&parse("-0.0")), "0");
        assert_eq!(compact(&parse("1.50")), compact(&parse("1.5")));
        assert_eq!(compact(&parse("1e2")), "100");
        assert_eq!(
            compact(&parse("12345678901234567890123")),
            "12345678901234567890123",
            "a big integer is its own exact value, never a rounded double"
        );
        assert_ne!(
            compact(&Value::String("e\u{0301}".into())),
            compact(&Value::String("\u{e9}".into())),
            "a string is exactly what it is written as"
        );
        assert_eq!(
            compact(&json!({"b": 1, "a": 2})),
            compact(&json!({"a": 2, "b": 1})),
            "member order is not part of a value"
        );
    }
}
