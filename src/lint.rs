use crate::{catalog::Catalog, config::Config, diagnostic::Diagnostic, schema::ColumnType};
use std::collections::HashSet;

/// Lint reasons about a schema's declared shape, so it can only analyse a table
/// whose primary key actually resolves to declared columns. A schema that fails
/// that requirement is already reported by schema validation as
/// `SCHEMA_PK_COLUMN_UNKNOWN` (Section 11); lint must contribute no findings for
/// it rather than index a column that does not exist. Section 6 requires `lint`,
/// `check`, and `doctor` to keep operating while the database is INVALID, so
/// this is a precondition of the analysis, not an error path of its own.
fn analyzable(schema: &crate::schema::Schema) -> bool {
    !schema.primary_key.is_empty()
        && schema
            .primary_key
            .iter()
            .all(|column| schema.columns.contains_key(column))
}

pub fn lint(c: &Catalog, config: &Config, descriptions: bool) -> Vec<Diagnostic> {
    let mut out = vec![];
    for (table, s) in &c.schemas {
        if !analyzable(s) {
            continue;
        }
        let rows = &c.rows[table];
        if s.inferred.is_some() {
            out.push(
                Diagnostic::warning(
                    "LINT_SCHEMA_UNREVIEWED",
                    format!("schema {table:?} was inferred and has not been accepted"),
                )
                .table(table)
                .fix("FIX_ACCEPT_INFERRED"),
            );
        }
        if s.additional_fields == crate::schema::AdditionalFields::Allow {
            out.push(
                Diagnostic::warning(
                    "LINT_ADDITIONAL_FIELDS_ALLOWED",
                    format!("{table} permits unknown row fields"),
                )
                .table(table),
            );
        }
        for (name, col) in &s.columns {
            let physical_values: Vec<_> = rows.iter().map(|r| r.value.get(name)).collect();
            let values: Vec<_> = rows
                .iter()
                .map(|row| row.value.get(name).or(col.default.as_ref()))
                .collect();
            if !rows.is_empty()
                && col.nullable
                && values.iter().all(|v| v.is_some_and(|v| !v.is_null()))
            {
                out.push(
                    Diagnostic::warning(
                        "LINT_NULLABLE_NEVER_NULL",
                        format!("{table}.{name} is nullable but no row is null"),
                    )
                    .table(table)
                    .field(name)
                    .fix("FIX_TIGHTEN_NULLABLE"),
                );
            }
            if !rows.is_empty() && values.iter().all(|v| v.is_none_or(|v| v.is_null())) {
                out.push(
                    Diagnostic::warning(
                        "LINT_COLUMN_NEVER_POPULATED",
                        format!("{table}.{name} is never populated"),
                    )
                    .table(table)
                    .field(name),
                );
            }
            let present = physical_values.iter().filter(|v| v.is_some()).count();
            if present > 0 && present < rows.len() {
                out.push(
                    Diagnostic::warning(
                        "LINT_INCONSISTENT_PRESENCE",
                        format!("{table}.{name} is present in {present}/{} rows", rows.len()),
                    )
                    .table(table)
                    .field(name),
                );
            }
            if col.kind == ColumnType::String {
                let strings: Vec<_> = values
                    .iter()
                    .filter_map(|v| v.and_then(|v| v.as_str()))
                    .collect();
                let narrower = if !strings.is_empty()
                    && strings.iter().all(|s| {
                        uuid::Uuid::parse_str(s).is_ok()
                            && s.len() == 36
                            && **s == s.to_ascii_lowercase()
                    }) {
                    Some("uuid")
                } else if !strings.is_empty()
                    && strings.iter().all(|s| {
                        ulid::Ulid::from_string(s).is_ok()
                            && s.len() == 26
                            && **s == s.to_ascii_uppercase()
                    })
                {
                    Some("ulid")
                } else if !strings.is_empty()
                    && strings
                        .iter()
                        .all(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok())
                {
                    Some("timestamp")
                } else if !strings.is_empty()
                    && strings.iter().all(|s| {
                        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() && s.len() == 10
                    })
                {
                    Some("date")
                } else {
                    None
                };
                if let Some(kind) = narrower {
                    out.push(
                        Diagnostic::warning(
                            "LINT_WIDER_TYPE",
                            format!("{table}.{name} can be narrowed from string to {kind}"),
                        )
                        .table(table)
                        .field(name)
                        .fix("FIX_NARROW_TYPE"),
                    );
                }
                let distinct: HashSet<_> = strings.iter().copied().collect();
                if strings.len() >= 3 * distinct.len() && distinct.len() <= config.enum_max_values {
                    out.push(
                        Diagnostic::suggestion(
                            "LINT_ENUM_CANDIDATE",
                            format!("{table}.{name} has {} distinct values", distinct.len()),
                        )
                        .table(table)
                        .field(name)
                        .fix("FIX_ADD_ENUM"),
                    );
                }
            }
            if col.kind == ColumnType::Float
                && values
                    .iter()
                    .any(|value| value.is_some_and(|value| !value.is_null()))
                && values.iter().all(|value| {
                    value.is_none_or(|value| value.is_null() || value.as_i64().is_some())
                })
            {
                out.push(
                    Diagnostic::warning(
                        "LINT_WIDER_TYPE",
                        format!("{table}.{name} can be narrowed from float to int"),
                    )
                    .table(table)
                    .field(name)
                    .fix("FIX_NARROW_TYPE"),
                );
            }
            let nonnull: Vec<_> = values
                .iter()
                .filter_map(|v| v.filter(|v| !v.is_null()))
                .collect();
            if !rows.is_empty()
                && !s.primary_key.contains(name)
                && !s.unique.iter().any(|u| u == &vec![name.clone()])
                && nonnull.len() == rows.len()
            {
                let distinct: HashSet<_> = nonnull
                    .iter()
                    .map(|v| crate::canonical::compact(v))
                    .collect();
                if distinct.len() == rows.len() {
                    out.push(
                        Diagnostic::suggestion(
                            "LINT_UNIQUE_CANDIDATE",
                            format!("{table}.{name} has {} distinct values", distinct.len()),
                        )
                        .table(table)
                        .field(name)
                        .fix("FIX_ADD_UNIQUE"),
                    );
                }
            }
            if rows.len() >= config.unique_min_rows && !nonnull.is_empty() {
                if col.kind == ColumnType::Int
                    && nonnull.iter().all(|v| v.as_i64().is_some_and(|x| x >= 0))
                    && !s.check.iter().any(|check| {
                        check.expr == format!("\"{}\" >= 0", name.replace('"', "\"\""))
                    })
                {
                    out.push(
                        Diagnostic::suggestion(
                            "LINT_CHECK_CANDIDATE",
                            format!("{table}.{name} is never negative"),
                        )
                        .table(table)
                        .field(name)
                        .fix("FIX_ADD_CHECK"),
                    );
                }
                if col.kind == ColumnType::String
                    && nonnull
                        .iter()
                        .all(|v| v.as_str().is_some_and(|x| !x.is_empty()))
                    && !s.check.iter().any(|check| {
                        check.expr == format!("\"{}\" <> ''", name.replace('"', "\"\""))
                    })
                {
                    out.push(
                        Diagnostic::suggestion(
                            "LINT_CHECK_CANDIDATE",
                            format!("{table}.{name} is never empty"),
                        )
                        .table(table)
                        .field(name)
                        .fix("FIX_ADD_CHECK"),
                    );
                }
            }
            if descriptions && col.description.is_none() {
                out.push(
                    Diagnostic::suggestion(
                        "LINT_NO_DESCRIPTION",
                        format!("{table}.{name} has no description"),
                    )
                    .table(table)
                    .field(name),
                );
            }
        }
        for fk in &s.foreign_keys {
            if !s.indexes.iter().any(|i| i == &fk.columns) {
                out.push(
                    Diagnostic::warning(
                        "LINT_FK_NO_INDEX",
                        format!(
                            "foreign key {}.{} has no index",
                            table,
                            fk.columns.join(",")
                        ),
                    )
                    .table(table)
                    .fix("FIX_ADD_INDEX"),
                );
            }
            if fk.on_delete.is_none() || fk.on_update.is_none() {
                out.push(
                    Diagnostic::warning(
                        "LINT_FK_ACTION_DEFAULTED",
                        format!(
                            "foreign key {}.{} relies on default restrict actions",
                            table,
                            fk.columns.join(",")
                        ),
                    )
                    .table(table),
                );
            }
        }
        if s.primary_key
            .iter()
            .all(|n| matches!(s.columns[n].kind, ColumnType::Uuid | ColumnType::Ulid))
            && s.primary_key
                .iter()
                .any(|n| s.columns[n].generated.is_none())
        {
            out.push(
                Diagnostic::suggestion(
                    "LINT_PK_NOT_GENERATED",
                    format!("{table} primary key has no generator"),
                )
                .table(table),
            );
        }
        if rows.iter().any(|r| {
            r.raw
                != crate::canonical::pretty_with_indent(
                    &crate::canonical::canonical_row(&r.value, s),
                    config.indentation_width,
                )
        }) {
            out.push(
                Diagnostic::info(
                    "LINT_NON_CANONICAL_FORMATTING",
                    format!("{table} contains rows not in canonical formatting"),
                )
                .table(table)
                .fix("FIX_CANONICALIZE"),
            );
        }
        for (name, col) in &s.columns {
            if s.foreign_keys
                .iter()
                .any(|fk| fk.columns == vec![name.clone()])
            {
                continue;
            }
            for (target, ts) in &c.schemas {
                if target == table || ts.primary_key.len() != 1 || !analyzable(ts) {
                    continue;
                }
                if ts.columns[&ts.primary_key[0]].kind != col.kind {
                    continue;
                }
                let expected = [
                    format!("{target}_id"),
                    format!("{}_id", target.strip_suffix('s').unwrap_or(target)),
                    target.clone(),
                ];
                if !expected.contains(name) {
                    continue;
                }
                let targets: HashSet<_> = c.rows[target]
                    .iter()
                    .filter_map(|r| {
                        r.value
                            .get(&ts.primary_key[0])
                            .map(crate::canonical::compact)
                    })
                    .collect();
                let values: Vec<_> = rows
                    .iter()
                    .filter_map(|r| r.value.get(name))
                    .filter(|v| !v.is_null())
                    .map(crate::canonical::compact)
                    .collect();
                if !values.is_empty() && values.iter().all(|v| targets.contains(v)) {
                    out.push(
                        Diagnostic::suggestion(
                            "LINT_FK_CANDIDATE",
                            format!("{table}.{name} values match {target}.{}", ts.primary_key[0]),
                        )
                        .table(table)
                        .field(name)
                        .fix("FIX_ADD_FK"),
                    );
                    break;
                }
            }
        }
        if descriptions && s.description.is_none() {
            out.push(
                Diagnostic::suggestion(
                    "LINT_NO_DESCRIPTION",
                    format!("table {table} has no description"),
                )
                .table(table),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, Row};
    use crate::schema::{AdditionalFields, Column, Schema};
    use indexmap::IndexMap;
    use serde_json::{Map, Value, json};
    use std::collections::BTreeMap;
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
            description: None,
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
            inferred: None,
        }
    }

    fn catalog_with(schema: Schema, rows: &[Value]) -> Catalog {
        let table = schema.table.clone();
        let rows: Vec<Row> = rows
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let object: Map<String, Value> = value.as_object().unwrap().clone();
                let raw = crate::canonical::pretty_with_indent(
                    &crate::canonical::canonical_row(&object, &schema),
                    2,
                );
                Row {
                    table: table.clone(),
                    path: PathBuf::from(format!("/tmp/{table}/{index}.json")),
                    relative: PathBuf::from(format!("{table}/{index}.json")),
                    value: object,
                    raw,
                }
            })
            .collect();
        Catalog {
            root: PathBuf::from("/tmp"),
            schemas: BTreeMap::from([(table.clone(), schema)]),
            rows: BTreeMap::from([(table, rows)]),
            diagnostics: vec![],
            warnings: vec![],
            indentation_width: 2,
        }
    }

    fn codes(catalog: &Catalog, descriptions: bool) -> Vec<String> {
        lint(catalog, &Config::default(), descriptions)
            .into_iter()
            .map(|d| d.code)
            .collect()
    }

    /// Section 6 and 13: lint reasons about a declared shape. A schema whose
    /// primary key does not resolve is already reported by schema validation, so
    /// lint must decline to analyse that table rather than index a column that
    /// does not exist.
    #[test]
    fn test9999_a_table_whose_primary_key_does_not_resolve_is_not_analysed() {
        let mut broken = schema(&[("id", ColumnType::String, false)], &["ghost"]);
        broken.inferred = Some(crate::schema::Inferred {
            at: "2026-09-14T00:00:00Z".into(),
            rows: 1,
            strictness: "balanced".into(),
            evidence: BTreeMap::new(),
        });
        let catalog = catalog_with(broken, &[json!({"id": "a"})]);
        // Would otherwise report LINT_SCHEMA_UNREVIEWED; contributes nothing
        // instead, and above all does not panic.
        assert!(codes(&catalog, false).is_empty());

        // An empty primary key is equally unanalysable.
        let empty = schema(&[("id", ColumnType::String, false)], &[]);
        let catalog = catalog_with(empty, &[json!({"id": "a"})]);
        assert!(codes(&catalog, false).is_empty());
    }

    /// Section 13: a nullable column that is never null can be tightened.
    #[test]
    fn test9999_a_nullable_column_that_is_never_null_is_reported() {
        let s = schema(
            &[
                ("id", ColumnType::String, false),
                ("maybe", ColumnType::String, true),
            ],
            &["id"],
        );
        let populated = catalog_with(
            s.clone(),
            &[json!({"id": "a", "maybe": "x"}), json!({"id": "b", "maybe": "y"})],
        );
        assert!(codes(&populated, false).contains(&"LINT_NULLABLE_NEVER_NULL".to_string()));

        // With a null actually present, the column is correctly nullable.
        let with_null = catalog_with(
            s,
            &[json!({"id": "a", "maybe": Value::Null}), json!({"id": "b", "maybe": "y"})],
        );
        assert!(!codes(&with_null, false).contains(&"LINT_NULLABLE_NEVER_NULL".to_string()));
    }

    /// Section 13: a string column whose every value is a narrower type should
    /// be narrowed, and a float column holding only integers likewise.
    #[test]
    fn test9999_wider_types_than_the_data_requires_are_reported() {
        let s = schema(
            &[
                ("id", ColumnType::String, false),
                ("at", ColumnType::String, false),
            ],
            &["id"],
        );
        let catalog = catalog_with(
            s,
            &[
                json!({"id": "a", "at": "2026-09-14T10:00:00Z"}),
                json!({"id": "b", "at": "2026-09-15T10:00:00Z"}),
            ],
        );
        assert!(codes(&catalog, false).contains(&"LINT_WIDER_TYPE".to_string()));

        let s = schema(
            &[
                ("id", ColumnType::String, false),
                ("n", ColumnType::Float, false),
            ],
            &["id"],
        );
        let integral = catalog_with(s.clone(), &[json!({"id": "a", "n": 1}), json!({"id": "b", "n": 2})]);
        assert!(codes(&integral, false).contains(&"LINT_WIDER_TYPE".to_string()));

        let fractional = catalog_with(s, &[json!({"id": "a", "n": 1.5})]);
        assert!(!codes(&fractional, false).contains(&"LINT_WIDER_TYPE".to_string()));
    }

    /// Section 13: a column declared but never populated, and a column present
    /// in only some rows, are both reported so the schema can be corrected.
    #[test]
    fn test9999_unpopulated_and_inconsistently_present_columns_are_reported() {
        let s = schema(
            &[
                ("id", ColumnType::String, false),
                ("ghost", ColumnType::String, true),
            ],
            &["id"],
        );
        let never = catalog_with(s.clone(), &[json!({"id": "a"}), json!({"id": "b"})]);
        assert!(codes(&never, false).contains(&"LINT_COLUMN_NEVER_POPULATED".to_string()));

        let sometimes = catalog_with(s, &[json!({"id": "a", "ghost": "x"}), json!({"id": "b"})]);
        assert!(codes(&sometimes, false).contains(&"LINT_INCONSISTENT_PRESENCE".to_string()));
    }

    /// Section 13: an inferred schema is unreviewed until accepted, and a schema
    /// permitting unknown fields is worth flagging.
    #[test]
    fn test9999_unreviewed_and_permissive_schemas_are_reported() {
        let mut s = schema(&[("id", ColumnType::String, false)], &["id"]);
        s.inferred = Some(crate::schema::Inferred {
            at: "2026-09-14T00:00:00Z".into(),
            rows: 1,
            strictness: "balanced".into(),
            evidence: BTreeMap::new(),
        });
        s.additional_fields = AdditionalFields::Allow;
        let catalog = catalog_with(s, &[json!({"id": "a"})]);
        let found = codes(&catalog, false);
        assert!(found.contains(&"LINT_SCHEMA_UNREVIEWED".to_string()));
        assert!(found.contains(&"LINT_ADDITIONAL_FIELDS_ALLOWED".to_string()));
    }

    /// Section 13: description findings are opt-in, so ordinary runs are not
    /// noisy with them.
    #[test]
    fn test9999_description_findings_are_opt_in() {
        let s = schema(&[("id", ColumnType::String, false)], &["id"]);
        let catalog = catalog_with(s, &[json!({"id": "a"})]);
        assert!(!codes(&catalog, false).contains(&"LINT_NO_DESCRIPTION".to_string()));
        assert!(codes(&catalog, true).contains(&"LINT_NO_DESCRIPTION".to_string()));
    }

    /// Section 13: a uuid primary key without a generator is a suggestion, so
    /// that new rows get identifiers from the binary rather than by hand.
    #[test]
    fn test9999_an_ungenerated_identifier_primary_key_is_reported() {
        let s = schema(&[("id", ColumnType::Uuid, false)], &["id"]);
        let catalog = catalog_with(
            s,
            &[json!({"id": "0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e"})],
        );
        assert!(codes(&catalog, false).contains(&"LINT_PK_NOT_GENERATED".to_string()));

        // A plain string key carries no such expectation.
        let s = schema(&[("id", ColumnType::String, false)], &["id"]);
        let catalog = catalog_with(s, &[json!({"id": "a"})]);
        assert!(!codes(&catalog, false).contains(&"LINT_PK_NOT_GENERATED".to_string()));
    }
}
