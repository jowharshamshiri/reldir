use crate::schema::{Column, ColumnType};
use base64::Engine;
use chrono::{DateTime, NaiveDate};
use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr;

pub fn matches_column(v: &Value, c: &Column) -> bool {
    if v.is_null() {
        return c.nullable;
    }
    match c.kind {
        ColumnType::Bool => v.is_boolean(),
        ColumnType::Int => v.as_i64().is_some() && v.as_f64().is_none_or(|n| n.fract() == 0.0),
        ColumnType::Float => v.as_f64().is_some_and(|x| x.is_finite()),
        ColumnType::Decimal => v
            .as_str()
            .is_some_and(|s| Decimal::from_str(s).is_ok() && canonical_decimal(s)),
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

fn canonical_decimal(s: &str) -> bool {
    if s.starts_with('+')
        || s.contains('e')
        || s.contains('E')
        || (s.starts_with('0') && s.len() > 1 && !s.starts_with("0."))
        || s.ends_with('.')
        || (s.contains('.') && s.ends_with('0'))
    {
        return false;
    }
    Decimal::from_str(s).is_ok()
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
