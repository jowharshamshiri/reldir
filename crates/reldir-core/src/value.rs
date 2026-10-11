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
/// the two differ only in JSON representation: `"42"` for an int, `3.0` for an
/// int written `3`, `3` for a float written `3.0`, `"true"` for a bool, an
/// upper-case uuid for its canonical spelling.
///
/// Only conversions that keep the value exactly are produced -- numbers are
/// converted from their written lexeme, never through a double -- and
/// anything that would change, invent or lose information returns `None`.
/// Whether the candidate satisfies the column's other rules is for the row
/// validator to judge.
pub fn lossless_convert(value: &Value, target: &ColumnType) -> Option<Value> {
    match target {
        ColumnType::Bool => match value.as_str()? {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        ColumnType::Int | ColumnType::Integer => {
            let lexeme = numeric_lexeme(value)?;
            let integer = integral_text(&lexeme)?;
            if *target == ColumnType::Int && integer.parse::<i64>().is_err() {
                return None;
            }
            number_from(&integer)
                .filter(|converted| Value::Number(converted.clone()) != *value)
                .map(Value::Number)
        }
        ColumnType::Float => {
            let lexeme = numeric_lexeme(value)?;
            let spelled = if lexeme.contains(['.', 'e', 'E']) {
                lexeme
            } else {
                format!("{lexeme}.0")
            };
            number_from(&spelled)
                .filter(|converted| Value::Number(converted.clone()) != *value)
                .map(Value::Number)
        }
        ColumnType::Number => {
            let text = value.as_str()?;
            number_from(text).map(Value::Number)
        }
        ColumnType::Decimal => {
            let Value::Number(number) = value else {
                return None;
            };
            normalize_numeric(&number.to_string()).map(Value::String)
        }
        ColumnType::String => match value {
            Value::Bool(boolean) => Some(Value::String(boolean.to_string())),
            Value::Number(number) => Some(Value::String(number.to_string())),
            _ => None,
        },
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
        ColumnType::Timestamp
        | ColumnType::Bytes
        | ColumnType::Date
        | ColumnType::Enum
        | ColumnType::Array
        | ColumnType::Object
        | ColumnType::Json => None,
    }
}

/// The written form of a number, or of a string that is a JSON number.
fn numeric_lexeme(value: &Value) -> Option<String> {
    match value {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) => number_from(text).map(|_| text.clone()),
        _ => None,
    }
}

/// A number with exactly this lexeme, when it is a JSON number denoting a
/// finite double.
fn number_from(lexeme: &str) -> Option<serde_json::Number> {
    if !lexeme.parse::<f64>().ok()?.is_finite() {
        return None;
    }
    lexeme.parse().ok()
}

/// The plain integer text of a number whose value is whole, written any way:
/// `3`, `3.0`, `3e0` and `30e-1` all give `3`.
fn integral_text(lexeme: &str) -> Option<String> {
    let normalized = normalize_numeric(lexeme)?;
    (!normalized.contains('.')).then_some(normalized)
}

/// A number's exact value: its sign, its significant digits with no leading or
/// trailing zeros (empty for zero), and the power of ten the last of them sits
/// at. `1.50`, `15e-1` and `0.15e1` are all `(false, "15", -1)`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Exact {
    negative: bool,
    digits: String,
    exponent: i64,
}

impl Exact {
    /// Parse JSON number grammar, or a decimal string without an exponent.
    fn parse(text: &str) -> Option<Self> {
        let (negative, unsigned) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
            Some(at) => {
                let power = &unsigned[at + 1..];
                let power = power.strip_prefix('+').unwrap_or(power);
                if power.is_empty()
                    || !power
                        .trim_start_matches('-')
                        .bytes()
                        .all(|byte| byte.is_ascii_digit())
                    || power.trim_start_matches('-').is_empty()
                {
                    return None;
                }
                // A power beyond this cannot be a finite double, which the
                // parser already refuses; bounding it keeps arithmetic exact.
                let power: i64 = power.parse().ok().filter(|p: &i64| p.abs() <= 100_000)?;
                (&unsigned[..at], power)
            }
            None => (unsigned, 0),
        };
        let (whole, fraction) = match mantissa.split_once('.') {
            Some((whole, fraction)) if !fraction.is_empty() => (whole, fraction),
            Some(_) => return None,
            None => (mantissa, ""),
        };
        if whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let all: String = format!("{whole}{fraction}");
        let trimmed = all.trim_start_matches('0');
        let trailing = trimmed.len() - trimmed.trim_end_matches('0').len();
        let digits = trimmed.trim_end_matches('0').to_string();
        let exponent = exponent - fraction.len() as i64 + trailing as i64;
        Some(if digits.is_empty() {
            Self {
                negative: false,
                digits,
                exponent: 0,
            }
        } else {
            Self {
                negative,
                digits,
                exponent,
            }
        })
    }

    /// The power of ten of the most significant digit.
    fn magnitude(&self) -> i64 {
        self.exponent + self.digits.len() as i64 - 1
    }

    fn cmp_magnitude(&self, other: &Self) -> Ordering {
        match (self.digits.is_empty(), other.digits.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        self.magnitude().cmp(&other.magnitude()).then_with(|| {
            // Same leading power: compare digit strings left-aligned.
            let length = self.digits.len().max(other.digits.len());
            let pad = |digits: &str| format!("{digits:0<length$}");
            pad(&self.digits).cmp(&pad(&other.digits))
        })
    }

    fn cmp(&self, other: &Self) -> Ordering {
        let sign = |exact: &Self| -> i8 {
            if exact.digits.is_empty() {
                0
            } else if exact.negative {
                -1
            } else {
                1
            }
        };
        match sign(self).cmp(&sign(other)) {
            Ordering::Equal if sign(self) < 0 => self.cmp_magnitude(other).reverse(),
            Ordering::Equal => self.cmp_magnitude(other),
            unequal => unequal,
        }
    }

    /// The value as plain decimal text with no exponent: the canonical
    /// decimal spelling.
    fn plain(&self) -> String {
        if self.digits.is_empty() {
            return "0".into();
        }
        let sign = if self.negative { "-" } else { "" };
        if self.exponent >= 0 {
            format!(
                "{sign}{}{}",
                self.digits,
                "0".repeat(self.exponent as usize)
            )
        } else {
            let point = self.digits.len() as i64 + self.exponent;
            if point > 0 {
                let (whole, fraction) = self.digits.split_at(point as usize);
                format!("{sign}{whole}.{fraction}")
            } else {
                format!("{sign}0.{}{}", "0".repeat((-point) as usize), self.digits)
            }
        }
    }
}

impl Exact {
    fn zero() -> Self {
        Self {
            negative: false,
            digits: String::new(),
            exponent: 0,
        }
    }

    fn from_parts(negative: bool, digits: Vec<u8>, exponent: i64) -> Self {
        let text: String = digits
            .iter()
            .map(|digit| char::from(b'0' + digit))
            .collect();
        let trimmed = text.trim_start_matches('0');
        let trailing = trimmed.len() - trimmed.trim_end_matches('0').len();
        let digits = trimmed.trim_end_matches('0').to_string();
        if digits.is_empty() {
            Self::zero()
        } else {
            Self {
                negative,
                digits,
                exponent: exponent + trailing as i64,
            }
        }
    }

    /// The digits, least significant first, scaled to `exponent` (which must
    /// not exceed this number's own).
    fn scaled(&self, exponent: i64) -> Vec<u8> {
        let mut out: Vec<u8> = vec![0; (self.exponent - exponent) as usize];
        out.extend(self.digits.bytes().rev().map(|byte| byte - b'0'));
        out
    }

    fn add(&self, other: &Self) -> Self {
        if self.digits.is_empty() {
            return other.clone();
        }
        if other.digits.is_empty() {
            return self.clone();
        }
        let exponent = self.exponent.min(other.exponent);
        let (left, right) = (self.scaled(exponent), other.scaled(exponent));
        if self.negative == other.negative {
            let mut sum = vec![];
            let mut carry = 0;
            for index in 0..left.len().max(right.len()) {
                let total = left.get(index).copied().unwrap_or(0)
                    + right.get(index).copied().unwrap_or(0)
                    + carry;
                sum.push(total % 10);
                carry = total / 10;
            }
            if carry > 0 {
                sum.push(carry);
            }
            sum.reverse();
            return Self::from_parts(self.negative, sum, exponent);
        }
        // Opposite signs: subtract the smaller magnitude from the larger.
        let (larger, smaller, negative) = match self.cmp_magnitude(other) {
            Ordering::Equal => return Self::zero(),
            Ordering::Greater => (left, right, self.negative),
            Ordering::Less => (right, left, other.negative),
        };
        let mut difference = vec![];
        let mut borrow = 0;
        for (index, &digit) in larger.iter().enumerate() {
            let mut digit = digit as i16 - borrow - *smaller.get(index).unwrap_or(&0) as i16;
            borrow = 0;
            if digit < 0 {
                digit += 10;
                borrow = 1;
            }
            difference.push(digit as u8);
        }
        difference.reverse();
        Self::from_parts(negative, difference, exponent)
    }

    fn negate(&self) -> Self {
        let mut out = self.clone();
        if !out.digits.is_empty() {
            out.negative = !out.negative;
        }
        out
    }

    fn mul(&self, other: &Self) -> Self {
        if self.digits.is_empty() || other.digits.is_empty() {
            return Self::zero();
        }
        let left: Vec<u32> = self
            .digits
            .bytes()
            .rev()
            .map(|b| u32::from(b - b'0'))
            .collect();
        let right: Vec<u32> = other
            .digits
            .bytes()
            .rev()
            .map(|b| u32::from(b - b'0'))
            .collect();
        let mut product = vec![0u32; left.len() + right.len()];
        for (i, l) in left.iter().enumerate() {
            for (j, r) in right.iter().enumerate() {
                product[i + j] += l * r;
            }
        }
        let mut carry = 0;
        for slot in product.iter_mut() {
            let total = *slot + carry;
            *slot = total % 10;
            carry = total / 10;
        }
        let mut digits: Vec<u8> = product.into_iter().map(|digit| digit as u8).collect();
        digits.reverse();
        Self::from_parts(
            self.negative != other.negative,
            digits,
            self.exponent + other.exponent,
        )
    }
}

/// Exact sum, difference and product of two numbers in canonical decimal
/// text. `None` when either is not a number. Decimal SQL functions are built
/// on these so money adds up to the cent at any size.
pub fn exact_add(left: &str, right: &str) -> Option<String> {
    Some(Exact::parse(left)?.add(&Exact::parse(right)?).plain())
}

pub fn exact_sub(left: &str, right: &str) -> Option<String> {
    Some(
        Exact::parse(left)?
            .add(&Exact::parse(right)?.negate())
            .plain(),
    )
}

pub fn exact_mul(left: &str, right: &str) -> Option<String> {
    Some(Exact::parse(left)?.mul(&Exact::parse(right)?).plain())
}

/// Order two numbers -- JSON number lexemes or decimal strings -- by their
/// exact values, at any size and precision. `None` when either is not a
/// number.
pub fn compare_numeric(left: &str, right: &str) -> Option<Ordering> {
    Some(Exact::parse(left)?.cmp(&Exact::parse(right)?))
}

/// A number's canonical decimal text: `1e2` is `100`, `1.50` is `1.5`, `-0` is
/// `0`. Two numbers are equal exactly when their canonical texts are.
pub fn normalize_numeric(text: &str) -> Option<String> {
    Exact::parse(text).map(|exact| exact.plain())
}

/// The instant an RFC 3339 timestamp with an offset denotes, as seconds and
/// nanoseconds since the epoch.
pub fn instant(text: &str) -> Option<(i64, u32)> {
    let parsed = DateTime::parse_from_rfc3339(text).ok()?;
    Some((parsed.timestamp(), parsed.timestamp_subsec_nanos()))
}

/// Order two timestamps as the instants they denote.
pub fn compare_instant(left: &str, right: &str) -> Option<Ordering> {
    Some(instant(left)?.cmp(&instant(right)?))
}

/// An instant's canonical text: UTC, with only the fractional digits it needs.
/// Two timestamps denote one instant exactly when their canonical texts are
/// equal, whatever offset each was written in.
pub fn normalize_instant(text: &str) -> Option<String> {
    let parsed = DateTime::parse_from_rfc3339(text).ok()?;
    Some(
        parsed
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
    )
}

/// The `RELDIR_NUMERIC` collation: numbers by exact value, every text that is
/// not a number after every number, those in byte order. A total order, as a
/// collation must be -- an index sorted by it stays sorted.
pub fn collate_numeric(left: &str, right: &str) -> Ordering {
    match (Exact::parse(left), Exact::parse(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.cmp(right),
    }
}

/// The `RELDIR_INSTANT` collation: timestamps by the instant they denote,
/// every other text after them in byte order.
pub fn collate_instant(left: &str, right: &str) -> Ordering {
    match (instant(left), instant(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.cmp(right),
    }
}

/// A value as it takes part in a key: two values are the same key exactly
/// when their components are equal. Numbers by exact value, timestamps by
/// instant; everything else -- strings included -- exactly as written, so a
/// key compares the way SQL compares it.
pub fn key_component(value: &Value, kind: &ColumnType) -> Value {
    match (kind, value) {
        (ColumnType::Timestamp, Value::String(text)) => normalize_instant(text)
            .map(Value::String)
            .unwrap_or_else(|| value.clone()),
        (ColumnType::Decimal, Value::String(text)) => normalize_numeric(text)
            .map(Value::String)
            .unwrap_or_else(|| value.clone()),
        (_, Value::Number(number)) => normalize_numeric(&number.to_string())
            .and_then(|text| text.parse().ok())
            .map(Value::Number)
            .unwrap_or_else(|| value.clone()),
        _ => value.clone(),
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
        let pattern = regex::Regex::new(DECIMAL_PATTERN).unwrap();
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
            assert!(pattern.is_match(accepted));
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

    #[test]
    fn test1110_numbers_compare_by_exact_value_however_written() {
        let huge = "9".repeat(40);
        let bigger = format!("1{}", "0".repeat(40));
        assert_eq!(compare_numeric(&huge, &bigger), Some(Ordering::Less));
        assert_eq!(
            compare_numeric("9007199254740993", "9007199254740992"),
            Some(Ordering::Greater),
            "beyond 2^53, where a double cannot tell them apart"
        );
        assert_eq!(compare_numeric("0.5", "0.4999"), Some(Ordering::Greater));
        assert_eq!(compare_numeric("-100", "-2"), Some(Ordering::Less));
        assert_eq!(compare_numeric("-1", "1"), Some(Ordering::Less));
        assert_eq!(compare_numeric("-0", "0"), Some(Ordering::Equal));
        for (left, right) in [
            ("1", "1.0"),
            ("100", "1e2"),
            ("1.5", "15e-1"),
            ("0.015", "1.5E-2"),
            ("1.50", "1.5"),
        ] {
            assert_eq!(
                compare_numeric(left, right),
                Some(Ordering::Equal),
                "{left} and {right} are one value"
            );
            assert_eq!(normalize_numeric(left), normalize_numeric(right));
        }
        assert_eq!(normalize_numeric("1e2").as_deref(), Some("100"));
        assert_eq!(normalize_numeric("1.50").as_deref(), Some("1.5"));
        assert_eq!(normalize_numeric("-0.000").as_deref(), Some("0"));
        assert_eq!(normalize_numeric("12e-4").as_deref(), Some("0.0012"));
        for not_a_number in ["", "-", "1.", ".5", "1e", "1e+", "abc", "0x10", "1 "] {
            assert_eq!(compare_numeric(not_a_number, "1"), None, "{not_a_number:?}");
        }
    }

    #[test]
    fn test1112_collations_are_total_orders_that_place_garbage_last() {
        let mut texts = vec!["10", "2", "x", "1e1", "-3", "abc", "2.0", "0.5"];
        texts.sort_by(|a, b| collate_numeric(a, b));
        assert_eq!(texts, ["-3", "0.5", "2", "2.0", "10", "1e1", "abc", "x"]);
        // Transitivity across valid and invalid text, which a byte-order
        // fallback inside the numeric comparison would break.
        for (a, b, c) in [("10", "1a", "2"), ("2", "10", "1a")] {
            let ab = collate_numeric(a, b);
            let bc = collate_numeric(b, c);
            if ab == bc && ab != Ordering::Equal {
                assert_eq!(collate_numeric(a, c), ab, "{a} {b} {c}");
            }
        }
        let mut stamps = vec![
            "2024-01-01T10:00:00.5Z",
            "not a time",
            "2024-01-01T10:00:00Z",
            "2024-01-01T11:00:00+02:00",
        ];
        stamps.sort_by(|a, b| collate_instant(a, b));
        assert_eq!(
            stamps,
            [
                "2024-01-01T11:00:00+02:00",
                "2024-01-01T10:00:00Z",
                "2024-01-01T10:00:00.5Z",
                "not a time"
            ],
            "instants in time order; a sub-second value after the whole second"
        );
        assert_eq!(
            collate_instant("2024-01-01T12:00:00+02:00", "2024-01-01T10:00:00Z"),
            Ordering::Equal
        );
    }

    #[test]
    fn test1111_lossless_convert_refuses_every_lossy_conversion() {
        let parse = |text: &str| crate::json::parse_str(text).unwrap();
        assert_eq!(
            lossless_convert(&json!("42"), &ColumnType::Int),
            Some(json!(42))
        );
        assert_eq!(
            lossless_convert(&parse("42.0"), &ColumnType::Int),
            Some(parse("42"))
        );
        assert_eq!(
            lossless_convert(&parse("4.2e1"), &ColumnType::Int),
            Some(parse("42"))
        );
        assert_eq!(
            lossless_convert(&parse("3"), &ColumnType::Float),
            Some(parse("3.0")),
            "a float written as an integer gains a fraction, not a different value"
        );
        assert_eq!(lossless_convert(&parse("3.0"), &ColumnType::Float), None);
        assert_eq!(
            lossless_convert(
                &parse("123456789012345678901234567890"),
                &ColumnType::Decimal
            ),
            Some(json!("123456789012345678901234567890")),
            "converted from the lexeme, never through a double"
        );
        assert_eq!(
            lossless_convert(&parse("99999999999999999999"), &ColumnType::Int),
            None,
            "beyond 64 bits is not an int"
        );
        assert_eq!(
            lossless_convert(&parse("99999999999999999999.0"), &ColumnType::Integer),
            Some(parse("99999999999999999999"))
        );
        assert_eq!(lossless_convert(&json!("01"), &ColumnType::Int), None);
        assert_eq!(lossless_convert(&parse("1.5"), &ColumnType::Int), None);
        assert_eq!(lossless_convert(&json!("nope"), &ColumnType::Int), None);
        assert_eq!(lossless_convert(&Value::Null, &ColumnType::Int), None);
        assert_eq!(
            lossless_convert(&json!("true"), &ColumnType::Bool),
            Some(json!(true))
        );
        assert_eq!(lossless_convert(&json!("TRUE"), &ColumnType::Bool), None);
        assert_eq!(lossless_convert(&json!(1), &ColumnType::Bool), None);
        assert_eq!(
            lossless_convert(
                &json!("0193B1F4-7C3A-7B1E-9C2D-3F4A5B6C7D8E"),
                &ColumnType::Uuid
            ),
            Some(json!("0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e")),
            "case is a spelling of a uuid, not part of its value"
        );
        for kind in [
            ColumnType::Timestamp,
            ColumnType::Date,
            ColumnType::Array,
            ColumnType::Object,
            ColumnType::Bytes,
            ColumnType::Enum,
        ] {
            assert_eq!(
                lossless_convert(&json!("whatever"), &kind),
                None,
                "{kind:?} is never guessed at"
            );
        }
    }

    #[test]
    fn test1113_keys_compare_numbers_by_value_and_timestamps_by_instant() {
        let parse = |text: &str| crate::json::parse_str(text).unwrap();
        assert_eq!(
            key_component(&json!("2026-09-14T12:00:00+02:00"), &ColumnType::Timestamp),
            key_component(&json!("2026-09-14T10:00:00Z"), &ColumnType::Timestamp),
        );
        assert_eq!(
            key_component(&parse("1"), &ColumnType::Number),
            key_component(&parse("1.0"), &ColumnType::Number),
        );
        assert_ne!(
            key_component(&json!("caf\u{e9}"), &ColumnType::String),
            key_component(&json!("cafe\u{301}"), &ColumnType::String),
            "strings are keys exactly as written, as SQL compares them"
        );
    }

    #[test]
    fn test1114_decimal_arithmetic_is_exact_at_any_size() {
        assert_eq!(exact_add("0.1", "0.2").as_deref(), Some("0.3"));
        assert_eq!(exact_add("1.5", "-1.5").as_deref(), Some("0"));
        assert_eq!(exact_sub("10", "0.01").as_deref(), Some("9.99"));
        assert_eq!(exact_sub("-2", "3").as_deref(), Some("-5"));
        assert_eq!(exact_mul("1.5", "-2").as_deref(), Some("-3"));
        assert_eq!(exact_mul("0.1", "0.1").as_deref(), Some("0.01"));
        let big = "123456789012345678901234567890";
        assert_eq!(
            exact_add(big, "1").as_deref(),
            Some("123456789012345678901234567891")
        );
        assert_eq!(
            exact_mul(big, "10").as_deref(),
            Some("1234567890123456789012345678900")
        );
        assert_eq!(exact_add("1e2", "1").as_deref(), Some("101"));
        assert_eq!(exact_add("x", "1"), None);
    }
}
