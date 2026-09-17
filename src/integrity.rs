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
                // An element key asks the same question once per element: each
                // id in the array must name a row that exists. A scalar key
                // asks it once for the row. Both fail the same way, because
                // both are the same relationship -- what differs is how many
                // lookups one row performs.
                for k in row_keys(&row.value, fk, s) {
                    if targets.contains(&k) {
                        continue;
                    }
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
                    if !fk.is_per_element()
                        && fk
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
                // A value can satisfy its type and still miss a bound, and
                // reporting "does not match type String" for a string that is
                // merely too short names the wrong thing. The bound is part of
                // what the column admits, so it is what the message says.
                let missed_bound = !value::within_bounds(v, col);
                // A value can satisfy its type and every bound and still match
                // no alternative. Saying "does not match type String" about a
                // string names the one thing that is not wrong with it.
                let missed_composition = !value::satisfies_composition(v, col);
                let diagnostic = Diagnostic::error(
                    "TYPE_MISMATCH",
                    match (missed_pattern, missed_bound, missed_composition) {
                        (Some(pattern), _, _) => {
                            format!("field {name:?} does not match pattern {pattern:?}")
                        }
                        (None, true, _) => {
                            format!("field {name:?} is outside the bounds declared for it")
                        }
                        (None, false, true) => format!(
                            "field {name:?} satisfies none of the alternatives declared for it"
                        ),
                        (None, false, false) => {
                            format!("field {name:?} does not match type {:?}", col.kind)
                        }
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
                // A coercion converts between representations of a value. It
                // cannot make one shorter, distinct, divisible, or a member of
                // an alternative set, so offering it for those names a remedy
                // that selects nothing.
                out.push(
                    if missed_pattern.is_some() || missed_bound || missed_composition {
                        diagnostic
                    } else {
                        diagnostic.fix("FIX_COERCE_VALUE")
                    },
                );
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

/// Every target key one row must find, for one foreign key.
///
/// A scalar key yields at most one: the row's own tuple. An element key yields
/// one per element of its array, because each element is a reference in its own
/// right. Returning a list for both means `integrity` asks the same question in
/// one loop rather than branching on a distinction that does not change what a
/// violation means.
///
/// A null element yields nothing, exactly as a null column does: a null never
/// matches a foreign key, and reporting one as an orphan would invent a
/// reference the row does not make.
pub fn row_keys(row: &Map<String, Value>, fk: &crate::schema::ForeignKey, s: &Schema) -> Vec<String> {
    if !fk.is_per_element() {
        return key(row, &fk.columns, s).into_iter().collect();
    }
    // An element key names exactly one column; `catalog` refuses any other
    // shape, so the first name is the whole key.
    let Some(spelled) = fk.columns.first() else {
        return vec![];
    };
    let column = crate::schema::key_column(spelled).name;
    row.get(column)
        .and_then(Value::as_array)
        .map(|elements| {
            elements
                .iter()
                .filter(|element| !element.is_null())
                .map(|element| crate::canonical::compact(&Value::Array(vec![element.clone()])))
                .collect()
        })
        .unwrap_or_default()
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
    use crate::schema::{
        AdditionalFields, Column, ColumnType, Composition, CompositionKind, Storage,
    };
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

    /// A diagnostic names the constraint a value actually missed.
    ///
    /// A value can satisfy its declared type and still be refused for its
    /// pattern, its bounds, or its alternatives. Reporting "does not match type
    /// String" about a string names the one thing that is not wrong with it,
    /// and sends a reader looking at the column's type when the type is fine.
    ///
    /// The fix offer follows the same rule: a coercion converts between
    /// representations, so it can answer a type miss and nothing else. Offering
    /// it for a bound or an alternative names a remedy that selects nothing --
    /// the defect `FIX_COERCE_VALUE` already had for patterns.
    #[test]
    fn test1163_a_diagnostic_names_the_constraint_that_was_missed() {
        let mut s = schema(
            &[
                ("id", ColumnType::String, false),
                ("name", ColumnType::String, false),
                ("code", ColumnType::String, false),
                ("n", ColumnType::Int, false),
            ],
            &["id"],
        );
        s.columns.get_mut("name").unwrap().min_size = Some(2);
        s.columns.get_mut("code").unwrap().composition = Some(Composition {
            kind: CompositionKind::One,
            alternatives: vec![{
                let mut upper = column(ColumnType::String, false);
                upper.pattern = Some("^[A-Z]+$".into());
                upper
            }],
        });

        let report = |body: Map<String, Value>| -> Diagnostic {
            let mut out = vec![];
            validate_row(&s, &body, &PathBuf::from("t/a.json"), &mut out);
            out.into_iter()
                .find(|d| d.code == "TYPE_MISMATCH")
                .expect("a TYPE_MISMATCH is raised")
        };

        // Too short: the value is a string, so the type is not the problem.
        let bound = report(body(&[
            ("id", json!("a")),
            ("name", json!("x")),
            ("code", json!("ABC")),
            ("n", json!(1)),
        ]));
        assert!(
            bound.message.contains("outside the bounds"),
            "a bound miss must say so: {}",
            bound.message
        );
        assert!(
            bound.fixes.is_empty(),
            "no coercion makes a value longer: {:?}",
            bound.fixes
        );

        // Satisfies no alternative, and is again a perfectly good string.
        let composed = report(body(&[
            ("id", json!("a")),
            ("name", json!("ok")),
            ("code", json!("lower")),
            ("n", json!(1)),
        ]));
        assert!(
            composed.message.contains("satisfies none of the alternatives"),
            "a composition miss must say so: {}",
            composed.message
        );
        assert!(composed.fixes.is_empty(), "no coercion satisfies an alternative");

        // A genuine type miss still reports the type, and still offers the
        // coercion that can answer it.
        let typed = report(body(&[
            ("id", json!("a")),
            ("name", json!("ok")),
            ("code", json!("ABC")),
            ("n", json!("7")),
        ]));
        assert!(
            typed.message.contains("does not match type"),
            "a type miss still names the type: {}",
            typed.message
        );
        assert!(
            typed.fixes.iter().any(|f| f == "FIX_COERCE_VALUE"),
            "a type miss keeps the fix that answers it: {:?}",
            typed.fixes
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
