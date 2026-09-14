use crate::{catalog::Catalog, config::Config, diagnostic::Diagnostic, schema::ColumnType};
use std::collections::HashSet;

pub fn lint(c: &Catalog, config: &Config, descriptions: bool) -> Vec<Diagnostic> {
    let mut out = vec![];
    for (table, s) in &c.schemas {
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
                if target == table
                    || ts.primary_key.len() != 1
                    || ts.columns[&ts.primary_key[0]].kind != col.kind
                {
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
