//! How each column type is held in SQL, and read back.
//!
//! SQL compares and orders values; the files are what they mean. Each column
//! type is given a declared type whose SQLite *affinity* makes comparisons with
//! literals and parameters behave as the type requires, and -- where SQLite's
//! own ordering is wrong for the type -- a collation that compares the text as
//! the value it denotes:
//!
//! | Type | Declared as | Affinity | Collation | Holds |
//! |---|---|---|---|---|
//! | `bool` | `RELDIR_BOOL_BLOB` | blob | | 0 / 1 |
//! | `int` | `RELDIR_INT64` | integer | | the integer |
//! | `integer` | `RELDIR_BIGNUM_TEXT` | text | `RELDIR_NUMERIC` | the lexeme |
//! | `float` | `RELDIR_FLOAT` | real | | the double |
//! | `number` | `RELDIR_NUMBER` | numeric | | integer or double |
//! | `decimal` | `RELDIR_DECIMAL_TEXT` | text | `RELDIR_NUMERIC` | the text |
//! | `timestamp` | `RELDIR_TIMESTAMP_TEXT` | text | `RELDIR_INSTANT` | as written |
//! | string-like | `RELDIR_<KIND>_TEXT` | text | | the text |
//! | `array`, `object`, `json` | `RELDIR_<KIND>_TEXT` | text | | JSON, as written |
//!
//! Because a TEXT-affinity column applies its affinity to an untyped literal or
//! parameter it is compared with, `price > 5` compares `"5"` under the numeric
//! collation -- exactly, as decimals must be -- and `at = '2024-01-01T12:00:00+02:00'`
//! finds the row written `2024-01-01T10:00:00Z`. Values are stored as written:
//! a timestamp keeps its offset and a JSON column its member order and number
//! spellings, so what SQL returns is what the file says.

use crate::{
    code::Code,
    diagnostic::{DbError, Result},
    schema::{ColumnType, GeneratedKind},
};
use rusqlite::{
    Connection,
    functions::{Aggregate, Context, FunctionFlags},
    types::{Value as SqlValue, ValueRef},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

/// What `absent()` returns: a value no JSON member can hold, standing for "no
/// member". A blob survives every column affinity unchanged, so a statement
/// that writes it into any column is read back as a removal.
pub const ABSENT: &[u8] = b"\x00reldir:absent\x00";

/// The next value each sequence-generated column hands out during the
/// statement being planned, keyed by table and column. Seeded before a
/// mutation runs and cleared after, so a sequence is never handed out from a
/// stale count.
#[derive(Debug, Clone, Default)]
pub struct Sequences(Arc<Mutex<BTreeMap<(String, String), i64>>>);

impl Sequences {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, String), i64>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn seed(&self, table: &str, column: &str, next: i64) {
        self.lock()
            .insert((table.to_string(), column.to_string()), next);
    }

    pub fn clear(&self) {
        self.lock().clear();
    }

    fn take(&self, table: &str, column: &str) -> rusqlite::Result<i64> {
        let mut sequences = self.lock();
        let next = sequences
            .get_mut(&(table.to_string(), column.to_string()))
            .ok_or_else(|| {
                function_error(format!(
                    "the sequence for {table}.{column} is only handed out while a statement is \
                     planned"
                ))
            })?;
        let value = *next;
        *next = value.checked_add(1).ok_or_else(|| {
            function_error(format!(
                "the sequence for {table}.{column} has reached the largest int"
            ))
        })?;
        Ok(value)
    }
}

/// The column default that fills a generated column a new row leaves out:
/// SQLite evaluates it as the row is inserted, so `RETURNING` sees the value
/// the file will hold.
pub fn generated_default(kind: GeneratedKind, table: &str, column: &str) -> String {
    let literal = |text: &str| format!("'{}'", text.replace('\'', "''"));
    format!(
        "DEFAULT (reldir_generate({}, {}, {}))",
        literal(kind.name()),
        literal(table),
        literal(column)
    )
}

/// The column definition suffix for a column type: its declared type and,
/// where the type needs one, its collation.
pub fn declared_type(kind: &ColumnType) -> &'static str {
    match kind {
        ColumnType::Bool => "RELDIR_BOOL_BLOB",
        ColumnType::Int => "RELDIR_INT64",
        ColumnType::Integer => "RELDIR_BIGNUM_TEXT COLLATE RELDIR_NUMERIC",
        ColumnType::Float => "RELDIR_FLOAT",
        ColumnType::Number => "RELDIR_NUMBER",
        ColumnType::Decimal => "RELDIR_DECIMAL_TEXT COLLATE RELDIR_NUMERIC",
        ColumnType::String => "RELDIR_STRING_TEXT",
        ColumnType::Bytes => "RELDIR_BYTES_TEXT",
        ColumnType::Date => "RELDIR_DATE_TEXT",
        ColumnType::Timestamp => "RELDIR_TIMESTAMP_TEXT COLLATE RELDIR_INSTANT",
        ColumnType::Uuid => "RELDIR_UUID_TEXT",
        ColumnType::Ulid => "RELDIR_ULID_TEXT",
        ColumnType::Enum => "RELDIR_ENUM_TEXT",
        ColumnType::Array => "RELDIR_ARRAY_TEXT",
        ColumnType::Object => "RELDIR_OBJECT_TEXT",
        ColumnType::Json => "RELDIR_JSON_TEXT",
    }
}

fn function_error(message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::UserFunctionError(message.into().into())
}

/// A SQL argument as the text of a number, for the exact decimal functions.
fn numeric_argument(context: &Context<'_>, index: usize) -> rusqlite::Result<Option<String>> {
    Ok(match context.get_raw(index) {
        ValueRef::Null => None,
        ValueRef::Integer(integer) => Some(integer.to_string()),
        ValueRef::Real(real) => Some(
            serde_json::Number::from_f64(real)
                .ok_or_else(|| function_error("a decimal function was given a non-finite number"))?
                .to_string(),
        ),
        ValueRef::Text(text) => {
            let text = std::str::from_utf8(text).map_err(|_| {
                function_error("a decimal function was given text that is not UTF-8")
            })?;
            if crate::value::normalize_numeric(text).is_none() {
                return Err(function_error(format!(
                    "a decimal function was given {text:?}, which is not a number"
                )));
            }
            Some(text.to_string())
        }
        ValueRef::Blob(_) => {
            return Err(function_error("a decimal function was given a blob"));
        }
    })
}

/// Register the collations and functions every connection over the mirror
/// needs: the orderings above, and exact arithmetic for decimals.
pub fn register(connection: &Connection, sequences: &Sequences) -> Result<()> {
    let corrupt = |error: rusqlite::Error| {
        DbError::new(
            Code::INTERNAL_METADATA_CORRUPT,
            format!("derived mirror: {error}"),
        )
    };
    connection
        .create_collation("RELDIR_NUMERIC", crate::value::collate_numeric)
        .map_err(corrupt)?;
    connection
        .create_collation("RELDIR_INSTANT", crate::value::collate_instant)
        .map_err(corrupt)?;
    let flags = FunctionFlags::SQLITE_UTF8
        | FunctionFlags::SQLITE_DETERMINISTIC
        | FunctionFlags::SQLITE_INNOCUOUS;
    for (name, operation) in [
        (
            "dec_add",
            crate::value::exact_add as fn(&str, &str) -> Option<String>,
        ),
        ("dec_sub", crate::value::exact_sub),
        ("dec_mul", crate::value::exact_mul),
    ] {
        connection
            .create_scalar_function(name, 2, flags, move |context| {
                match (numeric_argument(context, 0)?, numeric_argument(context, 1)?) {
                    (Some(left), Some(right)) => operation(&left, &right)
                        .map(SqlValue::Text)
                        .ok_or_else(|| function_error("the operands are not numbers")),
                    _ => Ok(SqlValue::Null),
                }
            })
            .map_err(corrupt)?;
    }
    connection
        .create_scalar_function("dec_cmp", 2, flags, |context| {
            match (numeric_argument(context, 0)?, numeric_argument(context, 1)?) {
                (Some(left), Some(right)) => crate::value::compare_numeric(&left, &right)
                    .map(|ordering| SqlValue::Integer(ordering as i64))
                    .ok_or_else(|| function_error("the operands are not numbers")),
                _ => Ok(SqlValue::Null),
            }
        })
        .map_err(corrupt)?;
    connection
        .create_aggregate_function("dec_sum", 1, flags, DecimalSum)
        .map_err(corrupt)?;
    connection
        .create_scalar_function("absent", 0, flags, |_| Ok(SqlValue::Blob(ABSENT.to_vec())))
        .map_err(corrupt)?;
    // Strings are compared exactly as written; these compare them as people
    // read them, whichever Unicode form a program wrote.
    connection
        .create_scalar_function("nfc", 1, flags, |context| {
            use unicode_normalization::UnicodeNormalization;
            Ok(match context.get_raw(0) {
                ValueRef::Text(text) => {
                    SqlValue::Text(String::from_utf8_lossy(text).nfc().collect())
                }
                ValueRef::Null => SqlValue::Null,
                _ => return Err(function_error("nfc() normalizes text")),
            })
        })
        .map_err(corrupt)?;
    connection
        .create_collation("RELDIR_NFC", |left: &str, right: &str| {
            use unicode_normalization::UnicodeNormalization;
            left.nfc().cmp(right.nfc())
        })
        .map_err(corrupt)?;
    let sequences = sequences.clone();
    connection
        .create_scalar_function(
            "reldir_generate",
            3,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_INNOCUOUS,
            move |context| {
                let kind: String = context.get(0)?;
                let table: String = context.get(1)?;
                let column: String = context.get(2)?;
                let kind = match kind.as_str() {
                    "uuid" => GeneratedKind::Uuid,
                    "ulid" => GeneratedKind::Ulid,
                    "now" => GeneratedKind::Now,
                    "sequence" => GeneratedKind::Sequence,
                    other => {
                        return Err(function_error(format!("{other:?} is not a generator")));
                    }
                };
                let sequence = match kind {
                    GeneratedKind::Sequence => sequences.take(&table, &column)?,
                    _ => 0,
                };
                Ok(to_sql_generic(&crate::value::generate(kind, sequence)))
            },
        )
        .map_err(corrupt)?;
    Ok(())
}

/// `dec_sum(x)`: the exact sum of a column of numbers, null when every value
/// is null, as `sum` is.
struct DecimalSum;

impl Aggregate<Option<String>, SqlValue> for DecimalSum {
    fn init(&self, _context: &mut Context<'_>) -> rusqlite::Result<Option<String>> {
        Ok(None)
    }

    fn step(&self, context: &mut Context<'_>, total: &mut Option<String>) -> rusqlite::Result<()> {
        if let Some(value) = numeric_argument(context, 0)? {
            *total = Some(match total.take() {
                None => crate::value::normalize_numeric(&value).ok_or_else(|| {
                    function_error("dec_sum was given a value that is not a number")
                })?,
                Some(sum) => crate::value::exact_add(&sum, &value).ok_or_else(|| {
                    function_error("dec_sum was given a value that is not a number")
                })?,
            });
        }
        Ok(())
    }

    fn finalize(
        &self,
        _context: &mut Context<'_>,
        total: Option<Option<String>>,
    ) -> rusqlite::Result<SqlValue> {
        Ok(total.flatten().map_or(SqlValue::Null, SqlValue::Text))
    }
}

/// Whether a number lexeme is an integer written plainly within 64 bits.
fn plain_i64(lexeme: &str) -> Option<i64> {
    if lexeme.contains(['.', 'e', 'E']) {
        return None;
    }
    lexeme.parse().ok()
}

/// A JSON value as SQLite holds it in a column of a type. A value of the wrong
/// JSON type -- which only an invalid row holds -- is stored as it is.
pub fn to_sql(value: &Value, kind: &ColumnType) -> SqlValue {
    match (kind, value) {
        (_, Value::Null) => SqlValue::Null,
        (ColumnType::Bool, Value::Bool(flag)) => SqlValue::Integer(i64::from(*flag)),
        (ColumnType::Int | ColumnType::Number, Value::Number(number)) => {
            let lexeme = number.to_string();
            plain_i64(&lexeme)
                .map(SqlValue::Integer)
                .unwrap_or_else(|| real(&lexeme))
        }
        (ColumnType::Float, Value::Number(number)) => real(&number.to_string()),
        (ColumnType::Integer, Value::Number(number)) => SqlValue::Text(number.to_string()),
        (ColumnType::Array | ColumnType::Object | ColumnType::Json, _) => {
            SqlValue::Text(serde_json::to_string(value).expect("JSON values serialize"))
        }
        _ => to_sql_generic(value),
    }
}

fn real(lexeme: &str) -> SqlValue {
    lexeme
        .parse::<f64>()
        .ok()
        .filter(|real| real.is_finite())
        .map_or_else(|| SqlValue::Text(lexeme.to_string()), SqlValue::Real)
}

/// A JSON value as SQLite holds it where no column type applies: a bound
/// parameter, or a value of the wrong type in an invalid row. A number a double
/// cannot hold exactly -- an integer beyond 64 bits, a decimal with more
/// digits than a double keeps -- is bound as its text, so a comparison with an
/// exact column stays exact.
pub fn to_sql_generic(value: &Value) -> SqlValue {
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(flag) => SqlValue::Integer(i64::from(*flag)),
        Value::Number(number) => {
            let lexeme = number.to_string();
            if let Some(integer) = plain_i64(&lexeme) {
                return SqlValue::Integer(integer);
            }
            let exact = lexeme.parse::<f64>().ok().filter(|real| {
                real.is_finite()
                    && serde_json::Number::from_f64(*real).is_some_and(|back| {
                        crate::value::compare_numeric(&back.to_string(), &lexeme)
                            == Some(std::cmp::Ordering::Equal)
                    })
            });
            match exact {
                Some(real) => SqlValue::Real(real),
                None => SqlValue::Text(lexeme),
            }
        }
        Value::String(text) => SqlValue::Text(text.clone()),
        other => SqlValue::Text(serde_json::to_string(other).expect("JSON values serialize")),
    }
}

/// A SQLite value read back as JSON, before its column's type is applied.
pub fn from_sql(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(integer) => Value::Number(integer.into()),
        ValueRef::Real(real) => serde_json::Number::from_f64(real)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        ValueRef::Text(text) => Value::String(String::from_utf8_lossy(text).into()),
        // `absent()` read where no member is being written is no value.
        ValueRef::Blob(ABSENT) => Value::Null,
        ValueRef::Blob(bytes) => Value::String(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            bytes,
        )),
    }
}

/// A value read from a column of a known type, as the JSON that type holds.
pub fn decode_typed(value: Value, kind: &ColumnType) -> Result<Value> {
    decode_declared(
        value,
        Some(declared_type(kind).split_whitespace().next().unwrap_or("")),
    )
}

/// A result value decoded by the declared type of the column it came from.
/// Expressions have no declared type and are returned as SQLite produced them.
pub fn decode_declared(value: Value, declared: Option<&str>) -> Result<Value> {
    if value.is_null() {
        return Ok(value);
    }
    match declared {
        Some("RELDIR_BOOL_BLOB") => match value.as_i64() {
            Some(0) => Ok(Value::Bool(false)),
            Some(1) => Ok(Value::Bool(true)),
            _ => Err(DbError::new(
                Code::QUERY_TYPE_ERROR,
                format!("a bool column produced {value}, which is neither true nor false"),
            )),
        },
        Some("RELDIR_BIGNUM_TEXT") => Ok(match &value {
            Value::String(text) => crate::json::parse_str(text)
                .ok()
                .filter(Value::is_number)
                .unwrap_or(value),
            _ => value,
        }),
        Some(kind @ ("RELDIR_ARRAY_TEXT" | "RELDIR_OBJECT_TEXT" | "RELDIR_JSON_TEXT")) => {
            let name = kind
                .trim_start_matches("RELDIR_")
                .trim_end_matches("_TEXT")
                .to_lowercase();
            let decoded = match &value {
                Value::String(text) => crate::json::parse_str(text).map_err(|error| {
                    DbError::new(
                        Code::QUERY_TYPE_ERROR,
                        format!("a {name} column produced invalid JSON: {error}"),
                    )
                })?,
                // A number or boolean SQL computed into a json column is
                // itself a JSON value.
                other => other.clone(),
            };
            let shaped = match kind {
                "RELDIR_ARRAY_TEXT" => decoded.is_array(),
                "RELDIR_OBJECT_TEXT" => decoded.is_object(),
                _ => true,
            };
            if shaped {
                Ok(decoded)
            } else {
                Err(DbError::new(
                    Code::QUERY_TYPE_ERROR,
                    format!("a {name} column produced {decoded}, which is not an {name}"),
                ))
            }
        }
        _ => Ok(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        register(&connection, &Sequences::default()).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TABLE t (price {}, at {}, n {}, big {});",
                declared_type(&ColumnType::Decimal),
                declared_type(&ColumnType::Timestamp),
                declared_type(&ColumnType::Number),
                declared_type(&ColumnType::Integer),
            ))
            .unwrap();
        for (price, at, n, big) in [
            ("10", "2024-01-01T10:00:00Z", "3", "99999999999999999999"),
            (
                "2",
                "2024-01-01T10:00:00.5Z",
                "2.5",
                "100000000000000000000",
            ),
            ("0.5", "2024-01-01T11:30:00+02:00", "1e1", "5"),
        ] {
            let row = [price, at, n, big]
                .map(|text| crate::json::parse_str(text).unwrap_or(Value::String(text.into())));
            connection
                .execute(
                    "INSERT INTO t VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params_from_iter([
                        to_sql(&Value::String(price.into()), &ColumnType::Decimal),
                        to_sql(&Value::String(at.into()), &ColumnType::Timestamp),
                        to_sql(&row[2], &ColumnType::Number),
                        to_sql(&row[3], &ColumnType::Integer),
                    ]),
                )
                .unwrap();
        }
        connection
    }

    fn column(connection: &Connection, sql: &str) -> Vec<String> {
        let mut statement = connection.prepare(sql).unwrap();
        statement
            .query_map([], |row| {
                Ok(match row.get_ref(0)? {
                    ValueRef::Text(text) => String::from_utf8_lossy(text).into_owned(),
                    ValueRef::Integer(integer) => integer.to_string(),
                    ValueRef::Real(real) => real.to_string(),
                    other => format!("{other:?}"),
                })
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn test8920_decimals_compare_with_numbers_exactly() {
        let connection = connection();
        assert_eq!(
            column(&connection, "SELECT price FROM t WHERE price > 5"),
            ["10"]
        );
        assert_eq!(
            column(&connection, "SELECT price FROM t ORDER BY price"),
            ["0.5", "2", "10"]
        );
        assert_eq!(column(&connection, "SELECT max(price) FROM t"), ["10"]);
        let bound: Vec<String> = connection
            .prepare("SELECT price FROM t WHERE price < ?1")
            .unwrap()
            .query_map(
                [to_sql_generic(&crate::json::parse_str("2.0").unwrap())],
                |row| row.get(0),
            )
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(bound, ["0.5"], "a bound number compares exactly too");
        assert_eq!(
            column(&connection, "SELECT dec_sum(price) FROM t"),
            ["12.5"]
        );
    }

    #[test]
    fn test8921_timestamps_order_as_instants_and_match_any_offset() {
        let connection = connection();
        assert_eq!(
            column(&connection, "SELECT at FROM t ORDER BY at"),
            [
                "2024-01-01T11:30:00+02:00",
                "2024-01-01T10:00:00Z",
                "2024-01-01T10:00:00.5Z"
            ],
            "stored as written, ordered as instants"
        );
        assert_eq!(
            column(
                &connection,
                "SELECT at FROM t WHERE at = '2024-01-01T12:00:00+02:00'"
            ),
            ["2024-01-01T10:00:00Z"]
        );
    }

    #[test]
    fn test8923_text_compares_as_written_unless_asked_to_normalize() {
        let connection = connection();
        let decomposed = "cafe\u{301}";
        let composed = "caf\u{e9}";
        let same = |sql: &str| -> i64 {
            connection
                .query_row(sql, [decomposed, composed], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(
            same("SELECT ?1 = ?2"),
            0,
            "strings are exactly what is written"
        );
        assert_eq!(same("SELECT nfc(?1) = nfc(?2)"), 1);
        assert_eq!(same("SELECT ?1 = ?2 COLLATE RELDIR_NFC"), 1);
        assert_eq!(same("SELECT nfc(?1) = ?2"), 1, "nfc() composes");
    }

    #[test]
    fn test8922_big_integers_stay_exact() {
        let connection = connection();
        assert_eq!(
            column(&connection, "SELECT big FROM t ORDER BY big"),
            ["5", "99999999999999999999", "100000000000000000000"]
        );
        // SQLite reads an unquoted integer literal beyond 64 bits as a double,
        // which cannot be exact; quoted, or bound, it compares exactly.
        assert_eq!(
            column(
                &connection,
                "SELECT big FROM t WHERE big > '99999999999999999998'"
            ),
            ["99999999999999999999", "100000000000000000000"]
        );
        let bound: Vec<String> = connection
            .prepare("SELECT big FROM t WHERE big = ?1")
            .unwrap()
            .query_map(
                [to_sql_generic(
                    &crate::json::parse_str("99999999999999999999").unwrap(),
                )],
                |row| row.get(0),
            )
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(bound, ["99999999999999999999"]);
        let decoded = decode_typed(
            Value::String("99999999999999999999".into()),
            &ColumnType::Integer,
        )
        .unwrap();
        assert_eq!(decoded.to_string(), "99999999999999999999");
    }
}
