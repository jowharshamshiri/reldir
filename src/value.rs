use crate::schema::{Column, ColumnType};
use base64::Engine;
use chrono::{DateTime, NaiveDate};
use serde_json::Value;
use std::cmp::Ordering;

pub fn matches_column(v: &Value, c: &Column) -> bool {
    if v.is_null() {
        return c.nullable;
    }
    match c.kind {
        ColumnType::Bool => v.is_boolean(),
        ColumnType::Int => v.as_i64().is_some() && v.as_f64().is_none_or(|n| n.fract() == 0.0),
        ColumnType::Float => v.as_f64().is_some_and(|x| x.is_finite()),
        ColumnType::Decimal => v.as_str().is_some_and(canonical_decimal),
        ColumnType::String => v.is_string(),
        ColumnType::Bytes => v
            .as_str()
            .is_some_and(|s| base64::engine::general_purpose::STANDARD.decode(s).is_ok()),
        ColumnType::Date => v
            .as_str()
            .is_some_and(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() && s.len() == 10),
        ColumnType::Timestamp => v
            .as_str()
            .is_some_and(|s| DateTime::parse_from_rfc3339(s).is_ok()),
        ColumnType::Uuid => v.as_str().is_some_and(|s| {
            uuid::Uuid::parse_str(s).is_ok() && s.len() == 36 && s == s.to_ascii_lowercase()
        }),
        ColumnType::Ulid => v.as_str().is_some_and(|s| {
            ulid::Ulid::from_string(s).is_ok() && s.len() == 26 && s == s.to_ascii_uppercase()
        }),
        ColumnType::Enum => v
            .as_str()
            .is_some_and(|s| c.values.as_ref().is_some_and(|x| x.iter().any(|v| v == s))),
        ColumnType::Array => v.as_array().is_some_and(|a| {
            c.items
                .as_ref()
                .is_some_and(|i| a.iter().all(|v| matches_column(v, i)))
        }),
        ColumnType::Object => v.as_object().is_some_and(|o| {
            c.properties.as_ref().is_none_or(|p| {
                p.iter()
                    .all(|(n, c)| o.get(n).map_or(c.nullable, |v| matches_column(v, c)))
            })
        }),
        ColumnType::Json => true,
    }
}

/// Convert a value only when the target representation preserves its logical
/// value. This is shared by doctor and migrations so the database has one
/// definition of a safe implicit conversion.
pub fn lossless_convert(value: &Value, target: &Column) -> Option<Value> {
    if matches_column(value, target) {
        return Some(value.clone());
    }
    if value.is_null() {
        return None;
    }

    let converted = match target.kind {
        ColumnType::Bool => match value.as_str()? {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return None,
        },
        ColumnType::Int => {
            if let Some(text) = value.as_str() {
                let number = text.parse::<i64>().ok()?;
                if number.to_string() != text {
                    return None;
                }
                Value::Number(number.into())
            } else {
                let number = value.as_f64()?;
                if !number.is_finite()
                    || number.fract() != 0.0
                    || number < i64::MIN as f64
                    || number > i64::MAX as f64
                {
                    return None;
                }
                let integer = number as i64;
                if integer as f64 != number {
                    return None;
                }
                Value::Number(integer.into())
            }
        }
        ColumnType::Float => {
            let number = value.as_str()?.parse::<f64>().ok()?;
            if !number.is_finite() {
                return None;
            }
            Value::Number(serde_json::Number::from_f64(number)?)
        }
        ColumnType::Decimal => {
            let integer = value.as_i64()?;
            Value::String(integer.to_string())
        }
        ColumnType::String => match value {
            Value::Bool(boolean) => Value::String(boolean.to_string()),
            Value::Number(number) => Value::String(number.to_string()),
            _ => return None,
        },
        ColumnType::Timestamp => {
            let timestamp = DateTime::parse_from_rfc3339(value.as_str()?).ok()?;
            Value::String(
                timestamp
                    .with_timezone(&chrono::Utc)
                    .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
            )
        }
        ColumnType::Json => value.clone(),
        ColumnType::Bytes
        | ColumnType::Date
        | ColumnType::Uuid
        | ColumnType::Ulid
        | ColumnType::Enum
        | ColumnType::Array
        | ColumnType::Object => return None,
    };
    matches_column(&converted, target).then_some(converted)
}

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

pub fn textual(v: &Value, c: &Column) -> Option<String> {
    if v.is_null() {
        return None;
    }
    Some(match c.kind {
        ColumnType::Bool => {
            if v.as_bool()? {
                "true".into()
            } else {
                "false".into()
            }
        }
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

pub fn generate(c: &Column, sequence: i64) -> Option<Value> {
    let g = c.generated.as_ref()?;
    Some(match g.kind {
        crate::schema::GeneratedKind::Uuid => Value::String(uuid::Uuid::new_v4().to_string()),
        crate::schema::GeneratedKind::Ulid => Value::String(ulid::Ulid::new().to_string()),
        crate::schema::GeneratedKind::Now => {
            Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        }
        crate::schema::GeneratedKind::Sequence => Value::Number(sequence.into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Column, ColumnType};
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

    /// Section 15: `decimal` is arbitrary-precision text in one canonical form,
    /// so every non-canonical spelling of a number must be rejected rather than
    /// silently accepted and later compared wrongly.
    #[test]
    fn test1109_canonical_decimal_accepts_exactly_one_spelling_per_value() {
        for accepted in [
            "0",
            "-1",
            "1",
            "10",
            "1.5",
            "-0.5",
            "123456789012345678901234567890",
        ] {
            assert!(canonical_decimal(accepted), "{accepted} must be canonical");
        }
        for rejected in [
            "", "+1", "-0", "01", "1.", ".5", "1.10", "1.0", "1..2", "1e5", "abc", "-", "1 ", " 1",
            "0x10",
        ] {
            assert!(
                !canonical_decimal(rejected),
                "{rejected:?} must not be canonical"
            );
        }
    }

    /// Section 15: decimal ordering is arbitrary-precision; it must not degrade
    /// to i64 or f64 comparison at the extremes.
    #[test]
    fn test1110_decimal_comparison_is_arbitrary_precision() {
        let huge = "9".repeat(40);
        let bigger = format!("1{}", "0".repeat(40));
        assert_eq!(compare_decimal(&huge, &bigger), Some(Ordering::Less));
        assert_eq!(compare_decimal(&bigger, &huge), Some(Ordering::Greater));

        // Values beyond f64's 53-bit integer precision must stay distinct.
        assert_eq!(
            compare_decimal("9007199254740993", "9007199254740992"),
            Some(Ordering::Greater)
        );

        // Fractions of differing length compare digit-by-digit, not by length.
        assert_eq!(compare_decimal("0.5", "0.4999"), Some(Ordering::Greater));
        assert_eq!(compare_decimal("1.5", "1.5"), Some(Ordering::Equal));

        // Sign handling: magnitude ordering reverses for negatives.
        assert_eq!(compare_decimal("-100", "-2"), Some(Ordering::Less));
        assert_eq!(compare_decimal("-1", "1"), Some(Ordering::Less));
        assert_eq!(compare_decimal("0", "0"), Some(Ordering::Equal));

        // Non-canonical input has no defined order and must report so.
        assert_eq!(compare_decimal("1.0", "1"), None);
    }

    /// Section 15: coercion is never silently lossy. Doctor may apply only
    /// conversions that round-trip to the same logical value.
    #[test]
    fn test1111_lossless_convert_refuses_every_lossy_conversion() {
        let int = column(ColumnType::Int);
        assert_eq!(lossless_convert(&json!("42"), &int), Some(json!(42)));
        assert_eq!(lossless_convert(&json!(42.0), &int), Some(json!(42)));
        // "01" parses as 1 but does not round-trip to "01", so it is lossy.
        assert_eq!(lossless_convert(&json!("01"), &int), None);
        assert_eq!(lossless_convert(&json!(1.5), &int), None);
        assert_eq!(lossless_convert(&json!("nope"), &int), None);
        assert_eq!(lossless_convert(&Value::Null, &int), None);

        let boolean = column(ColumnType::Bool);
        assert_eq!(
            lossless_convert(&json!("true"), &boolean),
            Some(json!(true))
        );
        assert_eq!(lossless_convert(&json!("TRUE"), &boolean), None);
        assert_eq!(lossless_convert(&json!(1), &boolean), None);

        // Opaque and structured targets are never guessed at.
        for kind in [
            ColumnType::Date,
            ColumnType::Uuid,
            ColumnType::Ulid,
            ColumnType::Array,
            ColumnType::Object,
        ] {
            assert_eq!(
                lossless_convert(&json!("whatever"), &column(kind.clone())),
                None,
                "{kind:?} must not be guessed at"
            );
        }

        // `bytes` is standard base64 text (Section 15), so any string that
        // decodes is already a valid value rather than a conversion: it is
        // returned unchanged. A string that is not base64 has no lossless
        // reading and must be refused.
        let bytes = column(ColumnType::Bytes);
        assert_eq!(
            lossless_convert(&json!("whatever"), &bytes),
            Some(json!("whatever")),
            "valid base64 is already a bytes value, not a coercion"
        );
        assert_eq!(lossless_convert(&json!("not-base64!!"), &bytes), None);
        assert_eq!(lossless_convert(&json!(42), &bytes), None);
    }

    /// Section 15: an enum accepts only its declared members, and an int column
    /// rejects a fractional number rather than truncating it.
    #[test]
    fn test1112_column_matching_is_strict_about_logical_values() {
        let mut enumeration = column(ColumnType::Enum);
        enumeration.values = Some(vec!["admin".into(), "member".into()]);
        assert!(matches_column(&json!("admin"), &enumeration));
        assert!(!matches_column(&json!("owner"), &enumeration));

        assert!(!matches_column(&json!(1.5), &column(ColumnType::Int)));
        assert!(matches_column(&json!(1), &column(ColumnType::Float)));

        // A uuid must be lowercase canonical form, a ulid uppercase.
        assert!(matches_column(
            &json!("0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e"),
            &column(ColumnType::Uuid)
        ));
        assert!(!matches_column(
            &json!("0193B1F4-7C3A-7B1E-9C2D-3F4A5B6C7D8E"),
            &column(ColumnType::Uuid)
        ));

        // NULL is permitted only where the column is nullable.
        let mut nullable = column(ColumnType::String);
        assert!(!matches_column(&Value::Null, &nullable));
        nullable.nullable = true;
        assert!(matches_column(&Value::Null, &nullable));
    }

    /// Section 15: timestamps are normalised to UTC for their canonical textual
    /// rendering, so the same instant written in any offset has one identity.
    #[test]
    fn test1113_timestamp_text_is_normalised_to_utc() {
        let timestamp = column(ColumnType::Timestamp);
        let offset = textual(&json!("2026-09-14T12:00:00+02:00"), &timestamp).unwrap();
        let utc = textual(&json!("2026-09-14T10:00:00Z"), &timestamp).unwrap();
        assert_eq!(offset, utc, "the same instant must render identically");
    }
}
