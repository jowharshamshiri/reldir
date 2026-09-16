use crate::{
    catalog::Catalog,
    diagnostic::Diagnostic,
    schema::{AdditionalFields, Schema},
    value,
};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};

pub fn validate(c: &Catalog) -> Vec<Diagnostic> {
    let mut out = c.diagnostics.clone();
    if c.diagnostics.iter().any(|d| d.code.starts_with("SCHEMA_")) {
        return out;
    }
    for (table, rows) in &c.rows {
        let Some(s) = c.schemas.get(table) else {
            continue;
        };
        for row in rows {
            let start = out.len();
            validate_row(s, &row.value, &row.relative, &mut out);
            for d in &mut out[start..] {
                if let Some(field) = &d.field {
                    d.location = locate(&row.raw, field);
                }
                if let Some(loc) = &d.location {
                    d.source_line = std::str::from_utf8(&row.raw)
                        .ok()
                        .and_then(|s| s.lines().nth(loc.line.saturating_sub(1)))
                        .map(String::from);
                }
            }
        }
        validate_unique(s, rows, &mut out);
    }
    for (table, s) in &c.schemas {
        for fk in &s.foreign_keys {
            let Some(target_schema) = c.schemas.get(&fk.references.table) else {
                continue;
            };
            let targets: std::collections::HashSet<String> = c.rows[&fk.references.table]
                .iter()
                .filter_map(|r| key(&r.value, &fk.references.columns, target_schema))
                .collect();
            for row in &c.rows[table] {
                if let Some(k) = key(&row.value, &fk.columns, s)
                    && !targets.contains(&k)
                {
                    let constraint = format!(
                        "{}.{} -> {}.{}",
                        table,
                        fk.columns.join(","),
                        fk.references.table,
                        fk.references.columns.join(",")
                    );
                    let mut d = Diagnostic::error(
                        "FOREIGN_KEY_VIOLATION",
                        format!(
                            "{}.{} references a row that does not exist",
                            table,
                            fk.columns.join(",")
                        ),
                    )
                    .at(row.relative.clone())
                    .table(table)
                    .field(fk.columns.join(","))
                    .expected(format!(
                        "existing {}.{}",
                        fk.references.table,
                        fk.references.columns.join(",")
                    ))
                    .observed(k)
                    .fix("FIX_ORPHAN_DELETE_ROW");
                    d.constraint = Some(constraint);
                    if fk
                        .columns
                        .iter()
                        .all(|x| s.columns.get(x).is_some_and(|c| c.nullable))
                    {
                        d.fixes.insert(0, "FIX_ORPHAN_SET_NULL".into())
                    }
                    out.push(d)
                }
            }
        }
    }
    out.extend(crate::sql::validate_checks(c));
    out
}

fn validate_row(
    s: &Schema,
    row: &Map<String, Value>,
    path: &std::path::Path,
    out: &mut Vec<Diagnostic>,
) {
    {
        use unicode_normalization::UnicodeNormalization;
        let mut normalized = std::collections::BTreeSet::new();
        if row
            .keys()
            .any(|key| !normalized.insert(key.nfc().collect::<String>()))
        {
            out.push(
                Diagnostic::error(
                    "ROW_UNKNOWN_FIELD",
                    "row field names collide after NFC normalization",
                )
                .at(path)
                .table(&s.table),
            );
        }
    }
    if s.additional_fields == AdditionalFields::Reject {
        for name in row.keys() {
            if !s.columns.contains_key(name) {
                out.push(
                    Diagnostic::error("ROW_UNKNOWN_FIELD", format!("unknown field {name:?}"))
                        .at(path)
                        .table(&s.table)
                        .field(name)
                        .fix("FIX_DROP_UNKNOWN_FIELD"),
                );
            }
        }
    }
    for (name, col) in &s.columns {
        match row.get(name) {
            None if col.default.is_some() => {}
            None if col.nullable => {}
            None => out.push(
                Diagnostic::error(
                    "ROW_MISSING_FIELD",
                    format!("required field {name:?} is absent"),
                )
                .at(path)
                .table(&s.table)
                .field(name),
            ),
            Some(v) if v.is_null() && !col.nullable => out.push(
                Diagnostic::error("NOT_NULL_VIOLATION", format!("{name:?} cannot be null"))
                    .at(path)
                    .table(&s.table)
                    .field(name),
            ),
            Some(v) if normalized_key_collision(v) => out.push(
                Diagnostic::error(
                    "TYPE_MISMATCH",
                    format!(
                        "field {name:?} contains object keys that collide after NFC normalization"
                    ),
                )
                .at(path)
                .table(&s.table)
                .field(name),
            ),
            Some(v) if !value::matches_column(v, col) => {
                // A pattern is part of what the column admits, so a value that
                // satisfies the type but not the pattern would otherwise report
                // a type it plainly has.
                let missed_pattern = col
                    .pattern
                    .as_ref()
                    .filter(|pattern| v.is_string() && !value::matches_pattern(v, pattern));
                let diagnostic = Diagnostic::error(
                    "TYPE_MISMATCH",
                    match missed_pattern {
                        Some(pattern) => format!("field {name:?} does not match pattern {pattern:?}"),
                        None => format!("field {name:?} does not match type {:?}", col.kind),
                    },
                )
                .at(path)
                .table(&s.table)
                .field(name)
                .observed(crate::canonical::compact(v));
                // A coercion converts between representations of a value, so it
                // can answer a type miss. It can never answer a pattern miss:
                // the value already has the declared type, and no lossless
                // conversion turns one string into a different string. Offering
                // the fix anyway would name a remedy that selects nothing --
                // doctor already classes this as manual, and the diagnostic
                // must say the same thing.
                out.push(match missed_pattern {
                    Some(_) => diagnostic,
                    None => diagnostic.fix("FIX_COERCE_VALUE"),
                });
            }
            _ => {}
        }
    }
}

fn normalized_key_collision(value: &Value) -> bool {
    use unicode_normalization::UnicodeNormalization;
    match value {
        Value::Array(values) => values.iter().any(normalized_key_collision),
        Value::Object(values) => {
            let mut keys = std::collections::BTreeSet::new();
            values
                .keys()
                .any(|key| !keys.insert(key.nfc().collect::<String>()))
                || values.values().any(normalized_key_collision)
        }
        _ => false,
    }
}

fn validate_unique(s: &Schema, rows: &[crate::catalog::Row], out: &mut Vec<Diagnostic>) {
    let mut constraints = vec![(&s.primary_key, true)];
    constraints.extend(s.unique.iter().map(|x| (x, false)));
    for (cols, pk) in constraints {
        let mut seen: HashMap<String, &std::path::Path> = HashMap::new();
        for r in rows {
            let Some(k) = key(&r.value, cols, s) else {
                continue;
            };
            if let Some(first) = seen.insert(k.clone(), &r.relative) {
                let code = if pk {
                    "PRIMARY_KEY_VIOLATION"
                } else {
                    "UNIQUE_VIOLATION"
                };
                out.push(
                    Diagnostic::error(code, format!("{} must be unique", cols.join(",")))
                        .at(r.relative.clone())
                        .table(&s.table)
                        .observed(k)
                        .help(format!("also present in {}", first.display())),
                );
            }
        }
    }
}

pub fn key(row: &Map<String, Value>, cols: &[String], s: &Schema) -> Option<String> {
    let values: Vec<_> = cols
        .iter()
        .map(|name| {
            row.get(name)
                .cloned()
                .or_else(|| {
                    s.columns
                        .get(name)
                        .and_then(|column| column.default.clone())
                })
                .unwrap_or(Value::Null)
        })
        .collect();
    if values.iter().any(Value::is_null) {
        return None;
    }
    Some(crate::canonical::compact(&Value::Array(values)))
}

pub fn rows_by_key<'a>(c: &'a Catalog, table: &str) -> BTreeMap<String, &'a crate::catalog::Row> {
    let Some(s) = c.schemas.get(table) else {
        return BTreeMap::new();
    };
    c.rows
        .get(table)
        .into_iter()
        .flatten()
        .filter_map(|r| key(&r.value, &s.primary_key, s).map(|k| (k, r)))
        .collect()
}

/// The line and column at which `field` is declared inside `raw`.
///
/// Shared with `lint` so that a finding about a schema column points at the
/// column's declaration in `schema/<table>.json`, rather than every layer
/// growing its own locator (Section 75: diagnostics identify line and column
/// wherever applicable).
pub(crate) fn locate(raw: &[u8], field: &str) -> Option<crate::diagnostic::Location> {
    let text = std::str::from_utf8(raw).ok()?;
    let needle = format!("\"{}\"", field.replace('"', "\\\""));
    let at = text.find(&needle)?;
    let before = &text[..at];
    Some(crate::diagnostic::Location {
        line: before.bytes().filter(|b| *b == b'\n').count() + 1,
        column: before
            .rsplit('\n')
            .next()
            .map_or(1, |x| x.chars().count() + 1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{AdditionalFields, Column, ColumnType, Storage};
    use indexmap::IndexMap;
    use serde_json::json;
    use std::path::PathBuf;

    fn column(kind: ColumnType, nullable: bool) -> Column {
        Column {
            kind,
            nullable,
            default: None,
            generated: None,
            values: None,
            items: None,
            properties: None,
            pattern: None,
            additional_properties: true,
            description: None,
            annotations: Default::default(),
        }
    }

    fn schema(columns: &[(&str, ColumnType, bool)], primary_key: &[&str]) -> Schema {
        let mut map = IndexMap::new();
        for (name, kind, nullable) in columns {
            map.insert((*name).to_string(), column(kind.clone(), *nullable));
        }
        Schema {
            table: "t".into(),
            schema_version: 1,
            schema_format: None,
            description: None,
            primary_key: primary_key.iter().map(|k| (*k).to_string()).collect(),
            columns: map,
            unique: vec![],
            foreign_keys: vec![],
            check: vec![],
            indexes: vec![],
            storage: None,
            additional_fields: AdditionalFields::Reject,
            annotations: Default::default(),
        }
    }

    fn body(pairs: &[(&str, Value)]) -> Map<String, Value> {
        let mut map = Map::new();
        for (key, value) in pairs {
            map.insert((*key).to_string(), value.clone());
        }
        map
    }

    fn codes(diagnostics: &[Diagnostic]) -> Vec<&str> {
        diagnostics.iter().map(|d| d.code.as_str()).collect()
    }

    /// Section 10: a key absent for a NOT NULL column with no default is
    /// ROW_MISSING_FIELD, while an explicit null in such a column is a
    /// NOT_NULL_VIOLATION. These are different faults and must not be conflated.
    #[test]
    fn test1047_absent_and_null_are_distinct_faults() {
        let s = schema(
            &[
                ("id", ColumnType::String, false),
                ("n", ColumnType::Int, false),
            ],
            &["id"],
        );
        let mut out = vec![];
        validate_row(
            &s,
            &body(&[("id", json!("a"))]),
            &PathBuf::from("t/a.json"),
            &mut out,
        );
        assert_eq!(codes(&out), vec!["ROW_MISSING_FIELD"]);

        let mut out = vec![];
        validate_row(
            &s,
            &body(&[("id", json!("a")), ("n", Value::Null)]),
            &PathBuf::from("t/a.json"),
            &mut out,
        );
        assert_eq!(codes(&out), vec!["NOT_NULL_VIOLATION"]);

        // A nullable column accepts both absence and an explicit null.
        let s = schema(
            &[
                ("id", ColumnType::String, false),
                ("n", ColumnType::Int, true),
            ],
            &["id"],
        );
        let mut out = vec![];
        validate_row(
            &s,
            &body(&[("id", json!("a"))]),
            &PathBuf::from("t/a.json"),
            &mut out,
        );
        validate_row(
            &s,
            &body(&[("id", json!("a")), ("n", Value::Null)]),
            &PathBuf::from("t/a.json"),
            &mut out,
        );
        assert!(
            out.is_empty(),
            "nullable column accepts null: {:?}",
            codes(&out)
        );
    }

    /// Section 10: a default satisfies a NOT NULL column that the row omits, so
    /// the row is valid without the key being physically present.
    #[test]
    fn test1048_a_default_satisfies_an_omitted_not_null_column() {
        let mut s = schema(
            &[
                ("id", ColumnType::String, false),
                ("tag", ColumnType::String, false),
            ],
            &["id"],
        );
        s.columns.get_mut("tag").unwrap().default = Some(json!("fallback"));
        let mut out = vec![];
        validate_row(
            &s,
            &body(&[("id", json!("a"))]),
            &PathBuf::from("t/a.json"),
            &mut out,
        );
        assert!(out.is_empty(), "{:?}", codes(&out));
    }

    /// Section 10: unknown fields are rejected by default so a typo cannot
    /// become invisible state, and accepted only under additional_fields allow.
    #[test]
    fn test1049_unknown_fields_follow_the_additional_fields_policy() {
        let mut s = schema(&[("id", ColumnType::String, false)], &["id"]);
        let row = body(&[("id", json!("a")), ("emial", json!("x"))]);

        let mut out = vec![];
        validate_row(&s, &row, &PathBuf::from("t/a.json"), &mut out);
        assert_eq!(codes(&out), vec!["ROW_UNKNOWN_FIELD"]);
        assert_eq!(out[0].field.as_deref(), Some("emial"));

        s.additional_fields = AdditionalFields::Allow;
        let mut out = vec![];
        validate_row(&s, &row, &PathBuf::from("t/a.json"), &mut out);
        assert!(out.is_empty(), "{:?}", codes(&out));
    }

    /// Section 55: two field names that differ only by Unicode normalisation
    /// form are a collision regardless of host behaviour, at the row root and
    /// nested inside values.
    #[test]
    fn test1050_normalisation_collisions_are_rejected_at_every_depth() {
        let s = schema(
            &[
                ("id", ColumnType::String, false),
                ("data", ColumnType::Json, false),
            ],
            &["id"],
        );

        // Two distinct byte sequences that normalise to the same key.
        let mut root = Map::new();
        root.insert("id".into(), json!("a"));
        root.insert("e\u{0301}".into(), json!(1));
        root.insert("\u{e9}".into(), json!(2));
        let mut out = vec![];
        validate_row(&s, &root, &PathBuf::from("t/a.json"), &mut out);
        assert!(codes(&out).contains(&"ROW_UNKNOWN_FIELD"));

        // The same collision nested inside a json value.
        let nested = body(&[
            ("id", json!("a")),
            ("data", json!({ "e\u{0301}": 1, "\u{e9}": 2 })),
        ]);
        let mut out = vec![];
        validate_row(&s, &nested, &PathBuf::from("t/a.json"), &mut out);
        assert!(codes(&out).contains(&"TYPE_MISMATCH"));

        // And inside an array element.
        let in_array = body(&[
            ("id", json!("a")),
            ("data", json!([{ "e\u{0301}": 1, "\u{e9}": 2 }])),
        ]);
        let mut out = vec![];
        validate_row(&s, &in_array, &PathBuf::from("t/a.json"), &mut out);
        assert!(codes(&out).contains(&"TYPE_MISMATCH"));
    }

    /// A diagnostic must not name a fix that cannot answer it.
    ///
    /// `FIX_COERCE_VALUE` converts between representations of a value, which
    /// can answer a type miss -- `"42"` into `42`. It can never answer a
    /// pattern miss: the value already has the declared type, and no lossless
    /// conversion turns one string into a different string. Doctor classes a
    /// pattern miss as manual and `--only FIX_COERCE_VALUE` selects nothing, so
    /// a diagnostic advertising the fix would send a reader after a remedy that
    /// does not exist.
    #[test]
    fn test1153_a_pattern_miss_advertises_no_coercion() {
        let mut s = schema(
            &[
                ("id", ColumnType::String, false),
                ("slug", ColumnType::String, false),
                ("n", ColumnType::Int, false),
            ],
            &["id"],
        );
        s.columns.get_mut("slug").unwrap().pattern = Some("^[a-z-]+$".into());

        // A value of the right type that misses the pattern.
        let mut out = vec![];
        validate_row(
            &s,
            &body(&[
                ("id", json!("a")),
                ("slug", json!("Not A Slug")),
                ("n", json!(1)),
            ]),
            &PathBuf::from("t/a.json"),
            &mut out,
        );
        let mismatch = out
            .iter()
            .find(|d| d.code == "TYPE_MISMATCH")
            .expect("a pattern miss is a TYPE_MISMATCH");
        assert!(
            mismatch.message.contains("pattern"),
            "the message must name the pattern: {}",
            mismatch.message
        );
        assert!(
            mismatch.fixes.is_empty(),
            "a pattern miss has no coercion, so it must advertise none: {:?}",
            mismatch.fixes
        );

        // A genuine type miss still offers the coercion that can answer it.
        let mut out = vec![];
        validate_row(
            &s,
            &body(&[
                ("id", json!("a")),
                ("slug", json!("fine-slug")),
                ("n", json!("7")),
            ]),
            &PathBuf::from("t/a.json"),
            &mut out,
        );
        let mismatch = out
            .iter()
            .find(|d| d.code == "TYPE_MISMATCH")
            .expect("a type miss is a TYPE_MISMATCH");
        assert!(
            mismatch.fixes.iter().any(|f| f == "FIX_COERCE_VALUE"),
            "a type miss must still name the fix that answers it: {:?}",
            mismatch.fixes
        );
    }

    /// Section 16: a key is the canonical rendering of its columns in order, so
    /// composite keys cannot be confused by concatenation, and a null component
    /// yields no key at all (a null never matches a foreign key).
    #[test]
    fn test1051_keys_are_unambiguous_and_null_free() {
        let s = schema(
            &[
                ("a", ColumnType::String, false),
                ("b", ColumnType::String, true),
            ],
            &["a", "b"],
        );
        let columns = vec!["a".to_string(), "b".to_string()];

        // ("ab","c") and ("a","bc") must not collide.
        let left = key(
            &body(&[("a", json!("ab")), ("b", json!("c"))]),
            &columns,
            &s,
        );
        let right = key(
            &body(&[("a", json!("a")), ("b", json!("bc"))]),
            &columns,
            &s,
        );
        assert!(left.is_some() && right.is_some());
        assert_ne!(left, right, "composite keys must not be ambiguous");

        // A null component means the row participates in no key.
        assert_eq!(
            key(
                &body(&[("a", json!("a")), ("b", Value::Null)]),
                &columns,
                &s
            ),
            None
        );
        // An absent component with no default behaves the same way.
        assert_eq!(key(&body(&[("a", json!("a"))]), &columns, &s), None);
    }

    /// Section 15: defaults are logical values, so two rows that omit a
    /// defaulted column share that column's value for uniqueness purposes.
    #[test]
    fn test1052_defaults_participate_in_key_identity() {
        let mut s = schema(
            &[
                ("id", ColumnType::String, false),
                ("tag", ColumnType::String, false),
            ],
            &["id"],
        );
        s.columns.get_mut("tag").unwrap().default = Some(json!("same"));
        let columns = vec!["tag".to_string()];
        let omitted = key(&body(&[("id", json!("a"))]), &columns, &s);
        let explicit = key(
            &body(&[("id", json!("b")), ("tag", json!("same"))]),
            &columns,
            &s,
        );
        assert_eq!(omitted, explicit, "a default is a logical value");
    }

    /// Section 10: storage.filename defaults to the primary key and is
    /// overridden by an explicit declaration.
    #[test]
    fn test1053_filename_columns_default_to_the_primary_key() {
        let mut s = schema(
            &[
                ("id", ColumnType::String, false),
                ("slug", ColumnType::String, false),
            ],
            &["id"],
        );
        assert_eq!(s.filename_columns(), ["id".to_string()]);
        s.storage = Some(Storage {
            filename: vec!["slug".into()],
        });
        assert_eq!(s.filename_columns(), ["slug".to_string()]);
    }
}
