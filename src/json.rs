use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Number, Value};
use std::fmt;

/// The marker carried by the error a document exceeding the depth limit
/// produces. Callers match on it to report `RESOURCE_LIMIT` rather than
/// `INVALID_JSON`: the document is well formed, it is merely too deep.
pub const DEPTH_LIMIT_MESSAGE: &str = "JSON nesting exceeds the configured depth limit";

thread_local! {
    /// Depth bound applied by the parser on this thread. Deserialization
    /// recurses through `StrictValue`, which serde reconstructs at every level
    /// with no state of its own, so the limit cannot travel as a parameter.
    static DEPTH_LIMIT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(crate::config::BOOTSTRAP_MAX_NESTING_DEPTH) };
    static DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Bound the nesting depth this thread's parser accepts, for the duration of
/// `body`.
///
/// Section 61 makes maximum nesting depth a configured limit and Section 57
/// requires parsers to enforce it. Enforcing it *during* deserialization is
/// what makes it real: a check applied to the parsed value cannot protect the
/// parse itself, which recurses before any caller sees a value.
pub fn with_depth_limit<T>(limit: usize, body: impl FnOnce() -> T) -> T {
    // Restoring through `Drop` rather than after the call keeps the bound
    // correct even if `body` panics. Threads are reused -- by the test harness
    // and by any future concurrent caller -- so a limit left behind by an
    // unwinding observation would silently govern an unrelated later parse.
    struct Restore(usize);
    impl Drop for Restore {
        fn drop(&mut self) {
            DEPTH_LIMIT.with(|cell| cell.set(self.0));
        }
    }
    let _restore = Restore(DEPTH_LIMIT.with(|cell| cell.replace(limit)));
    body()
}

fn enter<E: de::Error>() -> std::result::Result<usize, E> {
    let limit = DEPTH_LIMIT.with(|cell| cell.get());
    let depth = DEPTH.with(|cell| cell.get()) + 1;
    if depth > limit {
        return Err(E::custom(format!("{DEPTH_LIMIT_MESSAGE} of {limit}")));
    }
    DEPTH.with(|cell| cell.set(depth));
    Ok(depth)
}

fn leave() {
    DEPTH.with(|cell| cell.set(cell.get().saturating_sub(1)));
}

/// Parse one JSON document, rejecting duplicate object keys, non-finite
/// numbers, trailing content, and nesting beyond the configured depth limit.
///
/// The parser's own recursion limit is disabled because it is an undocumented
/// constant that no configuration could raise; the limit enforced here is the
/// documented, configurable one instead. Depth is therefore still bounded, and
/// bounded during the recursion rather than after it.
pub fn parse(bytes: &[u8]) -> serde_json::Result<Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    deserializer.disable_recursion_limit();
    DEPTH.with(|cell| cell.set(0));
    let parsed = StrictValue::deserialize(&mut deserializer).map(|value| value.0);
    DEPTH.with(|cell| cell.set(0));
    let value = parsed?;
    deserializer.end()?;
    Ok(value)
}

/// Whether an error reports that the depth limit was exceeded.
pub fn is_depth_limit(error: &serde_json::Error) -> bool {
    error.to_string().contains(DEPTH_LIMIT_MESSAGE)
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
        enter::<A::Error>()?;
        let mut array = Vec::new();
        loop {
            match values.next_element::<StrictValue>() {
                Ok(Some(value)) => array.push(value.0),
                Ok(None) => break,
                Err(error) => {
                    leave();
                    return Err(error);
                }
            }
        }
        leave();
        Ok(StrictValue(Value::Array(array)))
    }

    fn visit_map<A>(self, mut values: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        enter::<A::Error>()?;
        let mut object = Map::new();
        loop {
            match values.next_key::<String>() {
                Ok(Some(key)) => {
                    if object.contains_key(&key) {
                        leave();
                        return Err(de::Error::custom(format!("duplicate object key {key:?}")));
                    }
                    match values.next_value::<StrictValue>() {
                        Ok(value) => {
                            object.insert(key, value.0);
                        }
                        Err(error) => {
                            leave();
                            return Err(error);
                        }
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    leave();
                    return Err(error);
                }
            }
        }
        leave();
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

    /// Sections 57 and 61: nesting is bounded by the configured limit, enforced
    /// during deserialization. A document within the limit parses; one beyond it
    /// is refused with an error a caller can distinguish from malformed JSON,
    /// and the refusal happens while parsing rather than after, so the recursion
    /// itself is bounded.
    #[test]
    fn test9999_nesting_is_bounded_by_the_configured_depth_limit() {
        // Comfortably within the bootstrap bound.
        let shallow = format!("{}{}", "[".repeat(64), "]".repeat(64));
        assert!(parse_str(&shallow).is_ok());

        // Beyond serde's own historical recursion ceiling, but allowed when the
        // configured limit permits it -- the point of making the limit real.
        let deep = format!("{}{}", "[".repeat(300), "]".repeat(300));
        with_depth_limit(512, || {
            assert!(parse_str(&deep).is_ok(), "512 must admit a depth of 300");
        });

        // Past the limit the document is refused, and the refusal is
        // identifiable as a limit rather than as malformed syntax.
        with_depth_limit(64, || {
            let error = parse_str(&deep).expect_err("300 exceeds a limit of 64");
            assert!(is_depth_limit(&error), "unexpected error: {error}");
        });

        // Objects are bounded on the same footing as arrays.
        let nested_objects = format!("{}1{}", "{\"a\":".repeat(40), "}".repeat(40));
        with_depth_limit(8, || {
            let error = parse_str(&nested_objects).expect_err("40 exceeds a limit of 8");
            assert!(is_depth_limit(&error), "unexpected error: {error}");
        });
        with_depth_limit(64, || {
            assert!(parse_str(&nested_objects).is_ok());
        });

        // The limit is restored after each scope, so one parse cannot leak its
        // bound into the next.
        assert!(parse_str(&shallow).is_ok());
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
