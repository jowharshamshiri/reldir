use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Number, Value};
use std::fmt;

pub fn parse(bytes: &[u8]) -> serde_json::Result<Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = StrictValue::deserialize(&mut deserializer)?.0;
    deserializer.end()?;
    Ok(value)
}

pub fn parse_str(text: &str) -> serde_json::Result<Value> {
    parse(text.as_bytes())
}

pub fn parse_as<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> serde_json::Result<T> {
    serde_json::from_value(parse(bytes)?)
}

struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictVisitor)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Number::from_f64(value)
            .map(|number| StrictValue(Value::Number(number)))
            .ok_or_else(|| E::custom("JSON number is not finite"))
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value.into())))
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_seq<A>(self, mut values: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut array = Vec::new();
        while let Some(value) = values.next_element::<StrictValue>()? {
            array.push(value.0);
        }
        Ok(StrictValue(Value::Array(array)))
    }

    fn visit_map<A>(self, mut values: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut object = Map::new();
        while let Some(key) = values.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate object key {key:?}")));
            }
            let value = values.next_value::<StrictValue>()?;
            object.insert(key, value.0);
        }
        Ok(StrictValue(Value::Object(object)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Section 57: governed file contents are untrusted. A duplicate object key
    /// makes a row's logical value ambiguous, so it is rejected at parse time
    /// rather than silently resolved to the last occurrence.
    #[test]
    fn test9999_duplicate_object_keys_are_rejected() {
        let error = parse_str(r#"{"id":"a","id":"b"}"#).unwrap_err();
        assert!(
            error.to_string().contains("duplicate object key"),
            "unexpected error: {error}"
        );
        // Nested duplicates are caught at any depth.
        assert!(parse_str(r#"{"outer":{"k":1,"k":2}}"#).is_err());
        // Duplicate keys inside an array element are caught too.
        assert!(parse_str(r#"[{"k":1,"k":2}]"#).is_err());
        // The same key in sibling objects is perfectly legal.
        assert!(parse_str(r#"{"a":{"k":1},"b":{"k":2}}"#).is_ok());
    }

    /// Section 15: a JSON number must denote a finite value; NaN and infinity
    /// have no canonical representation and cannot be stored.
    #[test]
    fn test9999_non_finite_numbers_are_rejected() {
        // These are not legal JSON literals and must not be accepted.
        for text in ["NaN", "Infinity", "-Infinity", "1e999"] {
            assert!(parse_str(text).is_err(), "{text} must be rejected");
        }
    }

    /// Trailing content after a complete value means the file is not a single
    /// JSON document; accepting it would silently ignore part of the file.
    #[test]
    fn test9999_trailing_content_is_rejected() {
        assert!(parse_str(r#"{"a":1} trailing"#).is_err());
        assert!(parse_str(r#"{"a":1}{"b":2}"#).is_err());
        assert!(parse_str(r#"{"a":1}"#).is_ok());
    }

    /// Ordinary documents round-trip unchanged, including all scalar kinds.
    #[test]
    fn test9999_well_formed_documents_parse_to_their_values() {
        let value = parse_str(r#"{"s":"x","n":1,"f":1.5,"b":true,"z":null,"a":[1,2]}"#).unwrap();
        assert_eq!(value["s"], "x");
        assert_eq!(value["n"], 1);
        assert_eq!(value["f"], 1.5);
        assert_eq!(value["b"], true);
        assert!(value["z"].is_null());
        assert_eq!(value["a"], serde_json::json!([1, 2]));
    }

    /// A deeply nested document parses; depth limits are a policy applied by the
    /// caller against configuration, not a parser-level refusal.
    #[test]
    fn test9999_deep_nesting_parses_so_limits_stay_configurable() {
        let depth = 64;
        let text = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        assert!(parse_str(&text).is_ok());
    }

    /// Reported error positions carry line and column so diagnostics can point
    /// at the offending token (Section 49).
    #[test]
    fn test9999_errors_report_line_and_column() {
        let error = parse_str("{\n  \"a\": ,\n}").unwrap_err();
        assert_eq!(error.line(), 2);
        assert!(error.column() > 0);
    }
}
