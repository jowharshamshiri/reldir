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
