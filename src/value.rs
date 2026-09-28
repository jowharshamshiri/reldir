//! Relational values: how a JSON value of a column type is written, compared,
//! converted, and generated.
//!
//! Whether a value is *valid* is not decided here -- that is the row
//! validator's question, answered from the schema document. This module owns
//! what JSON Schema has no notion of: the canonical text of a typed value,
//! ordering for decimals, and the conversions that change a value's JSON
//! representation without changing what it means.

use crate::schema::{ColumnType, GeneratedKind};
use chrono::DateTime;
use serde_json::Value;
use std::cmp::Ordering;

/// The pattern a decimal's canonical text matches. Written into generated
/// schemas so a generic JSON Schema tool understands the column too.
pub const DECIMAL_PATTERN: &str = r"^-?(0|[1-9][0-9]*)(\.[0-9]*[1-9])?$";

/// The pattern a ULID's canonical text matches.
pub const ULID_PATTERN: &str = "^[0-7][0-9A-HJKMNP-TV-Z]{25}$";

/// A candidate value of `target` type with the same meaning as `value`, when
/// the two differ only in JSON representation: `"42"` for an int, `"true"`
/// for a bool, an offset timestamp for its UTC spelling.
///
/// Only conversions that round-trip are produced; anything that would change,
/// invent or lose information returns `None`. Whether the candidate satisfies
/// the column's other rules is for the row validator to judge.
pub fn lossless_convert(value: &Value, target: &ColumnType) -> Option<Value> {
    match target {
        ColumnType::Bool => match value.as_str()? {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        ColumnType::Int => {
            if let Some(text) = value.as_str() {
                let number = text.parse::<i64>().ok()?;
                (number.to_string() == text).then(|| Value::Number(number.into()))
            } else {
                let number = value.as_f64()?;
                if !value.is_f64()
                    || !number.is_finite()
                    || number.fract() != 0.0
                    || number < i64::MIN as f64
                    || number > i64::MAX as f64
                {
                    return None;
                }
                let integer = number as i64;
                (integer as f64 == number).then(|| Value::Number(integer.into()))
            }
        }
        ColumnType::Float => {
            let text = value.as_str()?;
            let number = text.parse::<f64>().ok()?;
            if !number.is_finite() {
                return None;
            }
            let converted = serde_json::Number::from_f64(number)?;
            // Only a spelling that reads back to the same text is lossless.
            (converted.to_string() == text || number.to_string() == text)
                .then_some(Value::Number(converted))
        }
        ColumnType::Decimal => {
            let text = if let Some(integer) = value.as_i64() {
                integer.to_string()
            } else {
                return None;
            };
            canonical_decimal(&text).then_some(Value::String(text))
        }
        ColumnType::String => match value {
            Value::Bool(boolean) => Some(Value::String(boolean.to_string())),
            Value::Number(number) => Some(Value::String(number.to_string())),
            _ => None,
        },
        ColumnType::Timestamp => {
            let timestamp = DateTime::parse_from_rfc3339(value.as_str()?).ok()?;
            let utc = timestamp
                .with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
            (utc != value.as_str()?).then_some(Value::String(utc))
        }
        ColumnType::Uuid => {
            let text = value.as_str()?;
            let parsed = uuid::Uuid::parse_str(text).ok()?;
            let canonical = parsed.hyphenated().to_string();
            (canonical != text).then_some(Value::String(canonical))
        }
        ColumnType::Ulid => {
            let text = value.as_str()?;
            let parsed = ulid::Ulid::from_string(text).ok()?;
            let canonical = parsed.to_string();
            (canonical != text).then_some(Value::String(canonical))
        }
        ColumnType::Bytes
        | ColumnType::Date
        | ColumnType::Enum
        | ColumnType::Array
        | ColumnType::Object
        | ColumnType::Json => None,
    }
}

/// Whether text is a decimal in its one canonical spelling: an optional minus,
/// no leading zeros, and a fraction -- if any -- without trailing zeros. Zero
/// is `0`, never `-0`.
pub fn canonical_decimal(text: &str) -> bool {
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    if unsigned.is_empty() || text.starts_with('+') || text == "-0" {
        return false;
    }
    let mut parts = unsigned.split('.');
    let whole = parts.next().unwrap_or("");
    let fraction = parts.next();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || (whole.len() > 1 && whole.starts_with('0'))
    {
        return false;
    }
    match fraction {
        None => true,
        Some(fraction) => {
            !fraction.is_empty()
                && fraction.bytes().all(|byte| byte.is_ascii_digit())
                && !fraction.ends_with('0')
        }
    }
}

/// Order two canonical decimals at arbitrary precision.
pub fn compare_decimal(left: &str, right: &str) -> Option<Ordering> {
    let left_parts = decimal_parts(left)?;
    let right_parts = decimal_parts(right)?;
    if left_parts.0 != right_parts.0 {
        return Some(left_parts.0.cmp(&right_parts.0));
    }
    if left_parts.0 == 0 {
        return Some(Ordering::Equal);
    }
    let magnitude = left_parts
        .1
        .len()
        .cmp(&right_parts.1.len())
        .then_with(|| left_parts.1.cmp(right_parts.1))
        .then_with(|| compare_fraction(left_parts.2, right_parts.2));
    Some(if left_parts.0 < 0 {
        magnitude.reverse()
    } else {
        magnitude
    })
}

fn decimal_parts(text: &str) -> Option<(i8, &str, &str)> {
    if !canonical_decimal(text) {
        return None;
    }
    let negative = text.starts_with('-');
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let sign = if whole == "0" && fraction.bytes().all(|byte| byte == b'0') {
        0
    } else if negative {
        -1
    } else {
        1
    };
    Some((sign, whole, fraction))
}

fn compare_fraction(left: &str, right: &str) -> Ordering {
    let length = left.len().max(right.len());
    (0..length)
        .map(|index| {
            left.as_bytes()
                .get(index)
                .copied()
                .unwrap_or(b'0')
                .cmp(&right.as_bytes().get(index).copied().unwrap_or(b'0'))
        })
        .find(|ordering| *ordering != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}

/// The canonical text of a value of a column type: what a key renders as, and
/// what a filename is built from. `None` for null.
pub fn textual(v: &Value, kind: &ColumnType) -> Option<String> {
    if v.is_null() {
        return None;
    }
    Some(match kind {
        ColumnType::Bool => if v.as_bool()? { "true".into() } else { "false".into() },
        ColumnType::Int => v.as_i64()?.to_string(),
        ColumnType::Float => {
            let n = v.as_f64()?;
            if n == 0.0 { "0".into() } else { n.to_string() }
        }
        ColumnType::Timestamp => DateTime::parse_from_rfc3339(v.as_str()?)
            .ok()?
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        _ => v
            .as_str()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| crate::canonical::compact(v)),
    })
}

/// A freshly generated value.
pub fn generate(kind: GeneratedKind, sequence: i64) -> Value {
    match kind {
        GeneratedKind::Uuid => Value::String(uuid::Uuid::new_v4().to_string()),
        GeneratedKind::Ulid => Value::String(ulid::Ulid::new().to_string()),
        GeneratedKind::Now => {
            Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        }
        GeneratedKind::Sequence => Value::Number(sequence.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test1109_canonical_decimal_accepts_exactly_one_spelling_per_value() {
        for accepted in ["0", "-1", "1", "10", "1.5", "-0.5", "123456789012345678901234567890"] {
            assert!(canonical_decimal(accepted), "{accepted} must be canonical");
            assert!(regex::Regex::new(DECIMAL_PATTERN).unwrap().is_match(accepted));
        }
        for rejected in [
            "", "+1", "-0", "01", "1.", ".5", "1.10", "1.0", "1..2", "1e5", "abc", "-", "1 ", " 1",
            "0x10",
        ] {
            assert!(!canonical_decimal(rejected), "{rejected:?} must not be canonical");
        }
    }

    #[test]
    fn test1110_decimal_comparison_is_arbitrary_precision() {
        let huge = "9".repeat(40);
        let bigger = format!("1{}", "0".repeat(40));
        assert_eq!(compare_decimal(&huge, &bigger), Some(Ordering::Less));
        assert_eq!(compare_decimal(&bigger, &huge), Some(Ordering::Greater));
        assert_eq!(compare_decimal("9007199254740993", "9007199254740992"), Some(Ordering::Greater));
        assert_eq!(compare_decimal("0.5", "0.4999"), Some(Ordering::Greater));
        assert_eq!(compare_decimal("1.5", "1.5"), Some(Ordering::Equal));
        assert_eq!(compare_decimal("-100", "-2"), Some(Ordering::Less));
        assert_eq!(compare_decimal("-1", "1"), Some(Ordering::Less));
        assert_eq!(compare_decimal("0", "0"), Some(Ordering::Equal));
        assert_eq!(compare_decimal("1.0", "1"), None);
    }

    #[test]
    fn test1111_lossless_convert_refuses_every_lossy_conversion() {
        assert_eq!(lossless_convert(&json!("42"), &ColumnType::Int), Some(json!(42)));
        assert_eq!(lossless_convert(&json!(42.0), &ColumnType::Int), Some(json!(42)));
        assert_eq!(lossless_convert(&json!("01"), &ColumnType::Int), None);
        assert_eq!(lossless_convert(&json!(1.5), &ColumnType::Int), None);
        assert_eq!(lossless_convert(&json!("nope"), &ColumnType::Int), None);
        assert_eq!(lossless_convert(&Value::Null, &ColumnType::Int), None);
        assert_eq!(lossless_convert(&json!("true"), &ColumnType::Bool), Some(json!(true)));
        assert_eq!(lossless_convert(&json!("TRUE"), &ColumnType::Bool), None);
        assert_eq!(lossless_convert(&json!(1), &ColumnType::Bool), None);
        assert_eq!(
            lossless_convert(&json!("0193B1F4-7C3A-7B1E-9C2D-3F4A5B6C7D8E"), &ColumnType::Uuid),
            Some(json!("0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e")),
            "case is a spelling of a uuid, not part of its value"
        );
        assert_eq!(
            lossless_convert(&json!("2026-09-14T12:00:00+02:00"), &ColumnType::Timestamp),
            Some(json!("2026-09-14T10:00:00Z"))
        );
        for kind in [ColumnType::Date, ColumnType::Array, ColumnType::Object, ColumnType::Bytes, ColumnType::Enum] {
            assert_eq!(lossless_convert(&json!("whatever"), &kind), None, "{kind:?} is never guessed at");
        }
    }

    #[test]
    fn test1113_timestamp_text_is_normalised_to_utc() {
        let offset = textual(&json!("2026-09-14T12:00:00+02:00"), &ColumnType::Timestamp).unwrap();
        let utc = textual(&json!("2026-09-14T10:00:00Z"), &ColumnType::Timestamp).unwrap();
        assert_eq!(offset, utc, "the same instant must render identically");
    }
}
