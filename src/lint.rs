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

/// Anchor a finding to the schema that declares the thing it is about.
///
/// Every schema-level finding is resolved by editing `.db/schema/<table>.json`, so
/// that is the file a reader needs. `anchor` -- a column name, or the table name
/// for table-level findings -- locates the declaration within it. The location is
/// reported only when the schema's original text was retained; a diagnostic never
/// points at a position that was guessed (Section 75).
fn in_schema(diagnostic: Diagnostic, c: &Catalog, table: &str, anchor: &str) -> Diagnostic {
    // A finding is resolved by editing the working schema, which is the file
    // jdb maintains; pinning afterwards is a separate, deliberate act.
    let mut diagnostic = diagnostic.at(crate::schema_store::working_relative(table));
    if let Some(source) = c.schema_sources.get(table) {
        diagnostic.location = crate::integrity::locate(source, anchor);
        if let Some(location) = &diagnostic.location {
            diagnostic.source_line = std::str::from_utf8(source)
                .ok()
                .and_then(|text| text.lines().nth(location.line.saturating_sub(1)))
                .map(str::to_string);
        }
    }
    diagnostic
}

/// Anchor a finding to a row file that exhibits it.
///
/// Findings about the rows themselves -- how they are formatted, which fields
/// they carry -- are properties of particular files, so they name one rather than
/// leaving the reader to search the table for it.
fn in_row(diagnostic: Diagnostic, row: &crate::catalog::Row) -> Diagnostic {
    diagnostic.at(row.relative.clone())
}

pub fn lint(c: &Catalog, config: &Config, descriptions: bool) -> Vec<Diagnostic> {
    let mut out = vec![];
    for (table, s) in &c.schemas {
        if !analyzable(s) {
            continue;
        }
        let rows = &c.rows[table];
        if !c.pinned.contains(table) {
            out.push(in_schema(
                Diagnostic::warning(
                    "LINT_SCHEMA_UNPINNED",
                    format!(
                        "schema {table:?} is maintained by jdb and is not pinned; \
                         deleting .db discards any refinement inference cannot re-derive"
                    ),
                )
                .table(table)
                .fix("FIX_PIN_SCHEMA"),
                c,
                table,
                "table",
            ));
        }
        if s.additional_fields == crate::schema::AdditionalFields::Allow {
            out.push(in_schema(
                Diagnostic::warning(
                    "LINT_ADDITIONAL_FIELDS_ALLOWED",
                    format!("{table} permits unknown row fields"),
                )
                .table(table),
                c,
                table,
                "additionalProperties",
            ));
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
                out.push(in_schema(
                    Diagnostic::warning(
                        "LINT_NULLABLE_NEVER_NULL",
                        format!("{table}.{name} is nullable but no row is null"),
                    )
                    .table(table)
                    .field(name)
                    .fix("FIX_TIGHTEN_NULLABLE"),
                    c,
                    table,
                    name,
                ));
            }
            if !rows.is_empty() && values.iter().all(|v| v.is_none_or(|v| v.is_null())) {
                out.push(in_schema(
                    Diagnostic::warning(
                        "LINT_COLUMN_NEVER_POPULATED",
                        format!("{table}.{name} is never populated"),
                    )
                    .table(table)
                    .field(name),
                    c,
                    table,
                    name,
                ));
            }
            let present = physical_values.iter().filter(|v| v.is_some()).count();
            if present > 0
                && present < rows.len()
                && let Some(absent) = rows.iter().find(|row| !row.value.contains_key(name))
            {
                // A row that actually lacks the field, so the reader can see the
                // inconsistency rather than search the table for an example.
                out.push(in_row(
                    Diagnostic::warning(
                        "LINT_INCONSISTENT_PRESENCE",
                        format!("{table}.{name} is present in {present}/{} rows", rows.len()),
                    )
                    .table(table)
                    .field(name),
                    absent,
                ));
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
                    out.push(in_schema(
                        Diagnostic::warning(
                            "LINT_WIDER_TYPE",
                            format!("{table}.{name} can be narrowed from string to {kind}"),
                        )
                        .table(table)
                        .field(name)
                        .fix("FIX_NARROW_TYPE"),
                        c,
                        table,
                        name,
                    ));
                }
                let distinct: HashSet<_> = strings.iter().copied().collect();
                if strings.len() >= 3 * distinct.len() && distinct.len() <= config.enum_max_values {
                    out.push(in_schema(
                        Diagnostic::suggestion(
                            "LINT_ENUM_CANDIDATE",
                            format!("{table}.{name} has {} distinct values", distinct.len()),
                        )
                        .table(table)
                        .field(name)
                        .fix("FIX_ADD_ENUM"),
                        c,
                        table,
                        name,
                    ));
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
                out.push(in_schema(
                    Diagnostic::warning(
                        "LINT_WIDER_TYPE",
                        format!("{table}.{name} can be narrowed from float to int"),
                    )
                    .table(table)
                    .field(name)
                    .fix("FIX_NARROW_TYPE"),
                    c,
                    table,
                    name,
                ));
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
                    out.push(in_schema(
                        Diagnostic::suggestion(
                            "LINT_UNIQUE_CANDIDATE",
                            format!("{table}.{name} has {} distinct values", distinct.len()),
                        )
                        .table(table)
                        .field(name)
                        .fix("FIX_ADD_UNIQUE"),
                        c,
                        table,
                        name,
                    ));
                }
            }
            if rows.len() >= config.unique_min_rows && !nonnull.is_empty() {
                if col.kind == ColumnType::Int
                    && nonnull.iter().all(|v| v.as_i64().is_some_and(|x| x >= 0))
                    && !s.check.iter().any(|check| {
                        check.expr == format!("\"{}\" >= 0", name.replace('"', "\"\""))
                    })
                {
                    out.push(in_schema(
                        Diagnostic::suggestion(
                            "LINT_CHECK_CANDIDATE",
                            format!("{table}.{name} is never negative"),
                        )
                        .table(table)
                        .field(name)
                        .fix("FIX_ADD_CHECK"),
                        c,
                        table,
                        name,
                    ));
                }
                if col.kind == ColumnType::String
                    && nonnull
                        .iter()
                        .all(|v| v.as_str().is_some_and(|x| !x.is_empty()))
                    && !s.check.iter().any(|check| {
                        check.expr == format!("\"{}\" <> ''", name.replace('"', "\"\""))
                    })
                {
                    out.push(in_schema(
                        Diagnostic::suggestion(
                            "LINT_CHECK_CANDIDATE",
                            format!("{table}.{name} is never empty"),
                        )
                        .table(table)
                        .field(name)
                        .fix("FIX_ADD_CHECK"),
                        c,
                        table,
                        name,
                    ));
                }
            }
            if descriptions && col.description.is_none() {
                out.push(in_schema(
                    Diagnostic::suggestion(
                        "LINT_NO_DESCRIPTION",
                        format!("{table}.{name} has no description"),
                    )
                    .table(table)
                    .field(name),
                    c,
                    table,
                    name,
                ));
            }
        }
        for fk in &s.foreign_keys {
            if !s.indexes.iter().any(|i| i == &fk.columns) {
                out.push(in_schema(
                    Diagnostic::warning(
                        "LINT_FK_NO_INDEX",
                        format!(
                            "foreign key {}.{} has no index",
                            table,
                            fk.columns.join(",")
                        ),
                    )
                    .table(table)
                    .field(fk.columns.join(","))
                    .fix("FIX_ADD_INDEX"),
                    c,
                    table,
                    &fk.columns[0],
                ));
            }
            if fk.on_delete.is_none() || fk.on_update.is_none() {
                out.push(in_schema(
                    Diagnostic::warning(
                        "LINT_FK_ACTION_DEFAULTED",
                        format!(
                            "foreign key {}.{} relies on default restrict actions",
                            table,
                            fk.columns.join(",")
                        ),
                    )
                    .table(table)
                    .field(fk.columns.join(",")),
                    c,
                    table,
                    &fk.columns[0],
                ));
            }
        }
        if s.primary_key
            .iter()
            .all(|n| matches!(s.columns[n].kind, ColumnType::Uuid | ColumnType::Ulid))
            && s.primary_key
                .iter()
                .any(|n| s.columns[n].generated.is_none())
        {
            out.push(in_schema(
                Diagnostic::suggestion(
                    "LINT_PK_NOT_GENERATED",
                    format!("{table} primary key has no generator"),
                )
                .table(table)
                .field(s.primary_key.join(",")),
                c,
                table,
                &s.primary_key[0],
            ));
        }
        let non_canonical: Vec<_> = rows
            .iter()
            .filter(|r| {
                r.raw
                    != crate::canonical::pretty_with_indent(
                        &crate::canonical::canonical_row(&r.value, s),
                        config.indentation_width,
                    )
            })
            .collect();
        if let Some(first) = non_canonical.first() {
            // Formatting is a property of particular files, so name one and say
            // how many share the finding.
            out.push(in_row(
                Diagnostic::info(
                    "LINT_NON_CANONICAL_FORMATTING",
                    format!(
                        "{table} contains {} row(s) not in canonical formatting",
                        non_canonical.len()
                    ),
                )
                .table(table)
                .fix("FIX_CANONICALIZE"),
                first,
            ));
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
                    out.push(in_schema(
                        Diagnostic::suggestion(
                            "LINT_FK_CANDIDATE",
                            format!("{table}.{name} values match {target}.{}", ts.primary_key[0]),
                        )
                        .table(table)
                        .field(name)
                        .fix("FIX_ADD_FK"),
                        c,
                        table,
                        name,
                    ));
                    break;
                }
            }
        }
        if descriptions && s.description.is_none() {
            out.push(in_schema(
                Diagnostic::suggestion(
                    "LINT_NO_DESCRIPTION",
                    format!("table {table} has no description"),
                )
                .table(table),
                c,
                table,
                "table",
            ));
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
    use std::collections::{BTreeMap, BTreeSet};
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
            ungoverned: vec![],
            pinned: BTreeSet::new(),
            schema_sources: BTreeMap::from([(
                table.clone(),
                crate::canonical::pretty_with_indent(&crate::schema::json_schema::encode(&schema), 2),
            )]),
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
    fn test1060_a_table_whose_primary_key_does_not_resolve_is_not_analysed() {
        let broken = schema(&[("id", ColumnType::String, false)], &["ghost"]);
        let catalog = catalog_with(broken, &[json!({"id": "a"})]);
        // Would otherwise report LINT_SCHEMA_UNPINNED; contributes nothing
        // instead, and above all does not panic.
        assert!(codes(&catalog, false).is_empty());

        // An empty primary key is equally unanalysable.
        let empty = schema(&[("id", ColumnType::String, false)], &[]);
        let catalog = catalog_with(empty, &[json!({"id": "a"})]);
        assert!(codes(&catalog, false).is_empty());
    }

    /// Section 13: a nullable column that is never null can be tightened.
    #[test]
    fn test1061_a_nullable_column_that_is_never_null_is_reported() {
        let s = schema(
            &[
                ("id", ColumnType::String, false),
                ("maybe", ColumnType::String, true),
            ],
            &["id"],
        );
        let populated = catalog_with(
            s.clone(),
            &[
                json!({"id": "a", "maybe": "x"}),
                json!({"id": "b", "maybe": "y"}),
            ],
        );
        assert!(codes(&populated, false).contains(&"LINT_NULLABLE_NEVER_NULL".to_string()));

        // With a null actually present, the column is correctly nullable.
        let with_null = catalog_with(
            s,
            &[
                json!({"id": "a", "maybe": Value::Null}),
                json!({"id": "b", "maybe": "y"}),
            ],
        );
        assert!(!codes(&with_null, false).contains(&"LINT_NULLABLE_NEVER_NULL".to_string()));
    }

    /// Section 13: a string column whose every value is a narrower type should
    /// be narrowed, and a float column holding only integers likewise.
    #[test]
    fn test1062_wider_types_than_the_data_requires_are_reported() {
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
        let integral = catalog_with(
            s.clone(),
            &[json!({"id": "a", "n": 1}), json!({"id": "b", "n": 2})],
        );
        assert!(codes(&integral, false).contains(&"LINT_WIDER_TYPE".to_string()));

        let fractional = catalog_with(s, &[json!({"id": "a", "n": 1.5})]);
        assert!(!codes(&fractional, false).contains(&"LINT_WIDER_TYPE".to_string()));
    }

    /// Section 13: a column declared but never populated, and a column present
    /// in only some rows, are both reported so the schema can be corrected.
    #[test]
    fn test1063_unpopulated_and_inconsistently_present_columns_are_reported() {
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

    /// Section 13: a schema jdb maintains is worth pinning, and a schema
    /// permitting unknown fields is worth flagging.
    #[test]
    fn test1064_unpinned_and_permissive_schemas_are_reported() {
        let mut s = schema(&[("id", ColumnType::String, false)], &["id"]);
        s.additional_fields = AdditionalFields::Allow;
        let catalog = catalog_with(s, &[json!({"id": "a"})]);
        let found = codes(&catalog, false);
        assert!(found.contains(&"LINT_SCHEMA_UNPINNED".to_string()));
        assert!(found.contains(&"LINT_ADDITIONAL_FIELDS_ALLOWED".to_string()));
    }

    /// Section 13: description findings are opt-in, so ordinary runs are not
    /// noisy with them.
    #[test]
    fn test1065_description_findings_are_opt_in() {
        let s = schema(&[("id", ColumnType::String, false)], &["id"]);
        let catalog = catalog_with(s, &[json!({"id": "a"})]);
        assert!(!codes(&catalog, false).contains(&"LINT_NO_DESCRIPTION".to_string()));
        assert!(codes(&catalog, true).contains(&"LINT_NO_DESCRIPTION".to_string()));
    }

    /// Section 75: a diagnostic identifies where the problem is. A lint finding
    /// that names only a table leaves the reader to hunt for the file, so every
    /// finding carries a path: schema-level findings point at the schema that
    /// declares the offending thing, and row-level findings name a row that
    /// actually exhibits it.
    #[test]
    fn test1066_every_finding_identifies_a_file() {
        let s = schema(
            &[
                ("id", ColumnType::String, false),
                ("maybe", ColumnType::String, true),
            ],
            &["id"],
        );
        // Rows differ in whether `maybe` is present, and are not canonical, so
        // both schema-level and row-level findings are produced at once.
        let catalog = catalog_with(s, &[json!({"id": "a", "maybe": "x"}), json!({"id": "b"})]);
        let findings = lint(&catalog, &Config::default(), true);
        assert!(!findings.is_empty(), "expected findings to inspect");

        for finding in &findings {
            let path = finding
                .path
                .as_ref()
                .unwrap_or_else(|| panic!("{} carries no path", finding.code));
            let path = path.to_string_lossy();
            match finding.code.as_str() {
                // Row-level findings name a row file.
                "LINT_INCONSISTENT_PRESENCE" | "LINT_NON_CANONICAL_FORMATTING" => assert!(
                    path.starts_with("t/"),
                    "{} should name a row file, got {path}",
                    finding.code
                ),
                // Everything else is fixed by editing the schema.
                _ => assert_eq!(
                    path, ".db/schema/t.json",
                    "{} should name the schema",
                    finding.code
                ),
            }
        }

        // A schema-level finding locates the declaration inside the schema, so
        // the reader is pointed at a line rather than a whole file.
        let located = findings
            .iter()
            .find(|f| f.code == "LINT_SCHEMA_UNPINNED")
            .expect("an unpinned-schema finding");
        let location = located
            .location
            .as_ref()
            .expect("a schema finding carries a location");
        assert!(location.line >= 1);
        assert!(
            located
                .source_line
                .as_deref()
                .is_some_and(|line| line.contains("table")),
            "the excerpt should show the declaration: {:?}",
            located.source_line
        );

        // The row-level finding names the row that actually lacks the field,
        // not merely the first row of the table.
        let presence = findings
            .iter()
            .find(|f| f.code == "LINT_INCONSISTENT_PRESENCE")
            .expect("an inconsistent-presence finding");
        assert_eq!(
            presence.path.as_ref().unwrap().to_string_lossy(),
            "t/1.json",
            "the named row must be the one missing the field"
        );
    }

    /// Section 13: a uuid primary key without a generator is a suggestion, so
    /// that new rows get identifiers from the binary rather than by hand.
    #[test]
    fn test1067_an_ungenerated_identifier_primary_key_is_reported() {
        let s = schema(&[("id", ColumnType::Uuid, false)], &["id"]);
        let catalog = catalog_with(s, &[json!({"id": "0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e"})]);
        assert!(codes(&catalog, false).contains(&"LINT_PK_NOT_GENERATED".to_string()));

        // A plain string key carries no such expectation.
        let s = schema(&[("id", ColumnType::String, false)], &["id"]);
        let catalog = catalog_with(s, &[json!({"id": "a"})]);
        assert!(!codes(&catalog, false).contains(&"LINT_PK_NOT_GENERATED".to_string()));
    }
}
