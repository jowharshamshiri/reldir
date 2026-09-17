use crate::schema::{Column, ColumnType, CompositionKind};
use base64::Engine;
use chrono::{DateTime, NaiveDate};
use serde_json::Value;
use std::cmp::Ordering;

pub fn matches_column(v: &Value, c: &Column) -> bool {
    if v.is_null() {
        return c.nullable;
    }
    // A pattern constrains string values wherever they appear. Checking it here
    // rather than per column type means it reaches array elements and nested
    // properties too, because this function is what recurses into them.
    //
    // An uncompilable pattern cannot reject a value here: `validate_column`
    // refuses such a schema outright, so reaching this point with one would
    // mean validating rows against a schema that was never accepted. Treating
    // it as unsatisfiable instead would fail every row of an already-rejected
    // schema, reporting the symptom in place of the cause.
    if let Some(pattern) = &c.pattern
        && !matches_pattern(v, pattern)
    {
        return false;
    }
    if !within_bounds(v, c) {
        return false;
    }
    // Composition constrains which values of the declared type are legal, so it
    // is asked alongside the type rather than instead of it. An alternative is
    // itself a column, so a nested pattern, bound or composition is judged by
    // the same recursion.
    if !satisfies_composition(v, c) {
        return false;
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
            // A closed object rejects members it does not declare. At the root
            // the same question is `Schema::additional_fields`, which names each
            // offending key as ROW_UNKNOWN_FIELD; here the object is a value, so
            // carrying an undeclared member is simply not matching the column.
            if !c.additional_properties
                && let Some(p) = c.properties.as_ref()
                && o.keys().any(|key| !p.contains_key(key))
            {
                return false;
            }
            c.properties.as_ref().is_none_or(|p| {
                p.iter()
                    .all(|(n, c)| o.get(n).map_or(c.nullable, |v| matches_column(v, c)))
            })
        }),
        ColumnType::Json => true,
    }
}

/// Whether a value satisfies the column's composition, if it declares one.
///
/// Shared with `integrity` so a diagnostic can say which of a column's three
/// constraints a value missed -- its pattern, its bounds, or its alternatives --
/// using the same judgement that rejected it. A column declaring no composition
/// is satisfied vacuously.
pub fn satisfies_composition(v: &Value, c: &Column) -> bool {
    let Some(composition) = &c.composition else {
        return true;
    };
    let satisfied = composition
        .alternatives
        .iter()
        .filter(|alternative| matches_column(v, alternative))
        .count();
    match composition.kind {
        CompositionKind::One => satisfied == 1,
        CompositionKind::Any => satisfied >= 1,
        CompositionKind::All => satisfied == composition.alternatives.len(),
        CompositionKind::Not => satisfied == 0,
    }
}

/// Whether a value satisfies the column's size and numeric bounds.
///
/// Size is one question asked of three shapes -- a string's characters, an
/// array's elements, an object's members -- so one pair of fields answers it
/// and the value's own shape decides which count to take. A value of a shape
/// the bound cannot describe is unconstrained by it rather than failed:
/// `validate_column` has already refused a schema that states a bound its
/// column type cannot carry, so reaching here with a mismatch would mean
/// judging rows against a schema that was never accepted.
///
/// String length counts characters, not bytes: JSON Schema counts code points,
/// and a byte count would make a bound mean different things for the same text
/// in different scripts.
pub fn within_bounds(v: &Value, c: &Column) -> bool {
    let size = match v {
        Value::String(text) => Some(text.chars().count() as u64),
        Value::Array(items) => Some(items.len() as u64),
        Value::Object(members) => Some(members.len() as u64),
        _ => None,
    };
    if let Some(size) = size {
        if c.min_size.is_some_and(|bound| size < bound) {
            return false;
        }
        if c.max_size.is_some_and(|bound| size > bound) {
            return false;
        }
    }

    if let Value::Array(items) = v
        && c.unique_items
    {
        // Compared by canonical rendering, which is reldir's own notion of when
        // two values are the same value: the rendering that decides a row's
        // hash. It normalizes strings to NFC and collapses `-0.0`, and it
        // deliberately keeps a number's spelling, so `1` and `1.0` are two
        // elements here where JSON Schema's `uniqueItems` counts them as one.
        //
        // The divergence is the right way round. Adopting JSON Schema's numeric
        // equality would mean either a second equality used only by this
        // keyword, or changing what `compact` says two values are -- and
        // `compact` decides row identity for every database in existence.
        let mut seen = std::collections::BTreeSet::new();
        if !items
            .iter()
            .all(|item| seen.insert(crate::canonical::compact(item)))
        {
            return false;
        }
    }

    if let Some(number) = v.as_f64() {
        if c.minimum.is_some_and(|bound| number < bound) {
            return false;
        }
        if c.maximum.is_some_and(|bound| number > bound) {
            return false;
        }
        if c.exclusive_minimum.is_some_and(|bound| number <= bound) {
            return false;
        }
        if c.exclusive_maximum.is_some_and(|bound| number >= bound) {
            return false;
        }
        if let Some(divisor) = c.multiple_of {
            // A non-positive divisor is refused by `validate_column`, so it
            // cannot reach here.
            let quotient = number / divisor;
            if (quotient - quotient.round()).abs() > f64::EPSILON * quotient.abs().max(1.0) {
                return false;
            }
        }
    }

    true
}

/// Whether a value satisfies a column's pattern.
///
/// A pattern constrains strings, so a non-string value is unconstrained by it
/// and is judged by its type alone. An uncompilable pattern cannot reject
/// anything: [`crate::schema::validate_column`] refuses such a schema outright
/// with `SCHEMA_CHECK_INVALID`, so a row is never judged against one. Failing
/// rows here instead would report every row of a broken schema rather than the
/// one thing that is actually wrong.
///
/// Shared with `integrity` so a diagnostic can say whether it was the pattern
/// or the type that a value missed, using the same judgment that rejected it.
pub fn matches_pattern(v: &Value, pattern: &str) -> bool {
    let Some(text) = v.as_str() else {
        return true;
    };
    match compiled(pattern) {
        Some(regex) => regex.is_match(text),
        None => true,
    }
}

/// One compiled automaton per distinct pattern, for the life of the process.
///
/// Validation walks every value of every row, so compiling on each call made
/// the cost of a pattern proportional to the corpus rather than to the schema:
/// a 1,109-row database with 153 declared patterns took twice as long to check
/// as the same database with the patterns stripped out. A schema has a fixed,
/// small set of patterns and they never change while a command runs, so the
/// compile belongs to the pattern, not to the value being judged.
///
/// Keyed by source text rather than by column so that the same pattern written
/// on twenty columns compiles once, which is the shape a real corpus has: one
/// id spelling repeated across every table that references it.
fn compiled(pattern: &str) -> Option<std::sync::Arc<regex::Regex>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<regex::Regex>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    // A poisoned lock means another thread panicked mid-insert. The map holds
    // only derived values, so the contents remain sound and are recovered
    // rather than turning an unrelated panic into this one.
    let mut map = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(entry) = map.get(pattern) {
        return entry.clone();
    }
    // A failed compile is cached too: `validate_column` has already reported it
    // as SCHEMA_CHECK_INVALID, and retrying the same doomed compile for every
    // value would pay the cost repeatedly to reach the same answer.
    let entry = regex::Regex::new(pattern).ok().map(Arc::new);
    map.insert(pattern.to_string(), entry.clone());
    entry
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

fn canonical_decimal(text: &str) -> bool {
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
    use crate::schema::{Column, ColumnType, Composition, CompositionKind};
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
            pattern: None,
            additional_properties: true,
            min_size: None,
            max_size: None,
            minimum: None,
            maximum: None,
            exclusive_minimum: None,
            exclusive_maximum: None,
            multiple_of: None,
            unique_items: false,
            composition: None,
            description: None,
            annotations: Default::default(),
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

    /// A `pattern` is a constraint on which rows are valid, so a value that
    /// satisfies the type but not the pattern does not match the column.
    ///
    /// It is checked wherever a string appears, not only at the top level: a
    /// corpus whose ids are patterned usually carries those same ids inside
    /// arrays of references and inside nested objects, and a pattern enforced
    /// at depth 0 but ignored at depth 1 would be worse than one uniformly
    /// unsupported, because the schema would read as though it applied.
    #[test]
    fn test1148_patterns_constrain_strings_at_every_depth() {
        let mut slug = column(ColumnType::String);
        slug.pattern = Some("^[a-z][a-z0-9-]{2,63}$".into());
        assert!(matches_column(&json!("valid-slug"), &slug));
        assert!(
            !matches_column(&json!("Not A Slug"), &slug),
            "a string that misses the pattern must not match the column"
        );

        // An array's elements are judged by `items`, which carries its own
        // pattern.
        let mut element = column(ColumnType::String);
        element.pattern = Some("^obj-[0-9]+$".into());
        let mut refs = column(ColumnType::Array);
        refs.items = Some(Box::new(element));
        assert!(matches_column(&json!(["obj-1", "obj-22"]), &refs));
        assert!(
            !matches_column(&json!(["obj-1", "objective-3"]), &refs),
            "one element missing the pattern must fail the whole array"
        );

        // A nested property is judged by its own subschema, likewise.
        let mut inner = column(ColumnType::String);
        inner.pattern = Some("^v[0-9]+$".into());
        let mut properties = IndexMap::new();
        properties.insert("version".to_string(), inner);
        let mut meta = column(ColumnType::Object);
        meta.properties = Some(properties);
        assert!(matches_column(&json!({"version": "v2"}), &meta));
        assert!(
            !matches_column(&json!({"version": "2"}), &meta),
            "a nested property missing its pattern must fail the object"
        );

        // A pattern constrains strings. A non-string value is judged by its
        // type, which is the only thing a pattern could not have decided.
        let mut number = column(ColumnType::Int);
        number.pattern = Some("^[0-9]+$".into());
        assert!(
            matches_column(&json!(7), &number),
            "a pattern must not reject a value it cannot describe"
        );

        // Unanchored patterns match anywhere, as they do in JSON Schema.
        let mut loose = column(ColumnType::String);
        loose.pattern = Some("abc".into());
        assert!(matches_column(&json!("xxabcxx"), &loose));
        assert!(!matches_column(&json!("xxabxx"), &loose));
    }

    /// A closed object rejects members it does not declare.
    ///
    /// At the root the same question is `Schema::additional_fields`, which
    /// names each offending key as ROW_UNKNOWN_FIELD. One level down there was
    /// no equivalent, so a schema could say `additionalProperties: false` on a
    /// nested object and have it mean nothing.
    #[test]
    fn test1149_a_closed_nested_object_rejects_undeclared_members() {
        let mut properties = IndexMap::new();
        properties.insert("source".to_string(), column(ColumnType::String));
        let mut meta = column(ColumnType::Object);
        meta.properties = Some(properties);

        // Open is JSON Schema's default, and stays the default here.
        assert!(
            matches_column(&json!({"source": "a", "extra": 1}), &meta),
            "an open object admits undeclared members"
        );

        meta.additional_properties = false;
        assert!(matches_column(&json!({"source": "a"}), &meta));
        assert!(
            !matches_column(&json!({"source": "a", "extra": 1}), &meta),
            "a closed object must reject a member it does not declare"
        );
    }

    /// A bound constrains the size of a value, and the shape of the value
    /// decides which count it is asked for. A value the bound cannot describe
    /// is unconstrained by it rather than failed: `validate_column` has already
    /// refused a schema that states a bound its type cannot carry.
    #[test]
    fn test1154_size_bounds_constrain_strings_arrays_and_objects() {
        let mut text = column(ColumnType::String);
        text.min_size = Some(2);
        text.max_size = Some(4);
        assert!(matches_column(&json!("abc"), &text));
        assert!(!matches_column(&json!("a"), &text), "shorter than the minimum");
        assert!(!matches_column(&json!("abcde"), &text), "longer than the maximum");

        // Characters, not bytes: a bound must mean the same thing in every
        // script, and "é" is one character however it is encoded.
        assert!(matches_column(&json!("éé"), &text), "two characters is two");

        let mut list = column(ColumnType::Array);
        list.items = Some(Box::new(column(ColumnType::String)));
        list.min_size = Some(1);
        assert!(matches_column(&json!(["a"]), &list));
        assert!(!matches_column(&json!([]), &list), "empty is below the minimum");

        let mut object = column(ColumnType::Object);
        object.min_size = Some(1);
        assert!(matches_column(&json!({"a": 1}), &object));
        assert!(!matches_column(&json!({}), &object), "no members is below the minimum");
    }

    /// Numeric bounds, including the exclusive pair and divisibility.
    #[test]
    fn test1155_numeric_bounds_are_enforced() {
        let mut n = column(ColumnType::Int);
        n.minimum = Some(0.0);
        n.maximum = Some(100.0);
        assert!(matches_column(&json!(0), &n), "the minimum itself is admitted");
        assert!(matches_column(&json!(100), &n), "the maximum itself is admitted");
        assert!(!matches_column(&json!(-1), &n));
        assert!(!matches_column(&json!(101), &n));

        let mut exclusive = column(ColumnType::Float);
        exclusive.exclusive_minimum = Some(0.0);
        exclusive.exclusive_maximum = Some(1.0);
        assert!(matches_column(&json!(0.5), &exclusive));
        assert!(
            !matches_column(&json!(0.0), &exclusive),
            "an exclusive bound excludes its own value"
        );
        assert!(!matches_column(&json!(1.0), &exclusive));

        let mut step = column(ColumnType::Int);
        step.multiple_of = Some(5.0);
        assert!(matches_column(&json!(10), &step));
        assert!(!matches_column(&json!(7), &step));
    }

    /// `uniqueItems` compares elements by their canonical rendering, so two
    /// equal values written differently are one element -- which is what JSON
    /// Schema's own equality says, and what a byte comparison would miss.
    #[test]
    fn test1156_unique_items_compares_canonically() {
        let mut list = column(ColumnType::Array);
        list.items = Some(Box::new(column(ColumnType::Json)));
        list.unique_items = true;

        assert!(matches_column(&json!([1, 2, 3]), &list));
        assert!(!matches_column(&json!([1, 1]), &list), "a literal repeat");

        // Distinctness is reldir's own: the canonical rendering that decides a
        // row's hash. It keeps a number's spelling, so these are two elements
        // where JSON Schema's `uniqueItems` would call them one. Documented in
        // `docs/schemas.md`, because a reader coming from JSON Schema will
        // otherwise expect the other answer.
        assert!(
            matches_column(&json!([1, 1.0]), &list),
            "canonical rendering keeps a number's spelling, so these differ"
        );

        // What it does collapse is what canonical form collapses everywhere:
        // NFC for strings, and the sign on zero.
        assert!(
            !matches_column(&json!([0, -0.0]), &list),
            "-0.0 normalizes to 0, so this is a repeat"
        );

        list.unique_items = false;
        assert!(matches_column(&json!([1, 1]), &list), "repeats are fine unless asked");
    }

    /// Composition narrows which values of the declared type are legal, and the
    /// arity is what distinguishes the four keywords.
    #[test]
    fn test1157_composition_arity_decides_what_is_admitted() {
        let mut upper = column(ColumnType::String);
        upper.pattern = Some("^[A-Z]+$".into());
        let mut short = column(ColumnType::String);
        short.max_size = Some(3);

        let mut one = column(ColumnType::String);
        one.composition = Some(Composition {
            kind: CompositionKind::One,
            alternatives: vec![upper.clone(), short.clone()],
        });
        assert!(matches_column(&json!("ABCDE"), &one), "upper only");
        assert!(matches_column(&json!("ab"), &one), "short only");
        assert!(
            !matches_column(&json!("AB"), &one),
            "satisfies both, and oneOf admits exactly one"
        );
        assert!(!matches_column(&json!("abcde"), &one), "neither");

        let mut any = column(ColumnType::String);
        any.composition = Some(Composition {
            kind: CompositionKind::Any,
            alternatives: vec![upper.clone(), short.clone()],
        });
        assert!(matches_column(&json!("AB"), &any), "both is at least one");
        assert!(!matches_column(&json!("abcde"), &any));

        let mut all = column(ColumnType::String);
        all.composition = Some(Composition {
            kind: CompositionKind::All,
            alternatives: vec![upper.clone(), short.clone()],
        });
        assert!(matches_column(&json!("AB"), &all));
        assert!(!matches_column(&json!("ABCDE"), &all), "upper but not short");

        let mut not = column(ColumnType::String);
        not.composition = Some(Composition {
            kind: CompositionKind::Not,
            alternatives: vec![upper],
        });
        assert!(matches_column(&json!("abc"), &not));
        assert!(!matches_column(&json!("ABC"), &not));
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
