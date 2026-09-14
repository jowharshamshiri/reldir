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
                let vals: Vec<_> = fk
                    .columns
                    .iter()
                    .map(|x| row.value.get(x).unwrap_or(&Value::Null))
                    .collect();
                if vals.iter().any(|v| v.is_null()) {
                    continue;
                }
                if let Some(k) = key(&row.value, &fk.columns, s) {
                    if !targets.contains(&k) {
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
    }
    out.extend(crate::sql::validate_checks(c));
    out
}

pub fn validate_row(
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
            Some(v) if !value::matches_column(v, col) => out.push(
                Diagnostic::error(
                    "TYPE_MISMATCH",
                    format!("field {name:?} does not match type {:?}", col.kind),
                )
                .at(path)
                .table(&s.table)
                .field(name)
                .observed(crate::canonical::compact(v))
                .fix("FIX_COERCE_VALUE"),
            ),
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

pub fn key(row: &Map<String, Value>, cols: &[String], _s: &Schema) -> Option<String> {
    let values: Vec<_> = cols
        .iter()
        .map(|c| row.get(c).cloned().unwrap_or(Value::Null))
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

fn locate(raw: &[u8], field: &str) -> Option<crate::diagnostic::Location> {
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
