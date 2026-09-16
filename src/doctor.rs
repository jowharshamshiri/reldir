use crate::{
    canonical, db::Database, diagnostic::Result, lint, schema::ColumnType, transaction::Change,
};
use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Fix {
    pub id: String,
    pub class: String,
    pub description: String,
    pub paths: Vec<PathBuf>,
}
pub fn plan(db: &Database) -> Vec<Fix> {
    let mut out = vec![];
    for d in &db.diagnostics {
        match d.code.as_str() {
            "IDENTITY_MISMATCH" => {
                if let (Some(p), Some(expected)) = (&d.path, &d.expected) {
                    out.push(Fix {
                        id: "FIX_RENAME_TO_IDENTITY".into(),
                        class: "layout".into(),
                        description: format!("rename {} to {expected}", p.display()),
                        paths: vec![p.clone()],
                    });
                }
            }
            "ROW_UNKNOWN_FIELD" => {
                if let Some(p) = &d.path {
                    let rename = near_field(db, d);
                    out.push(Fix {
                        id: if rename.is_some() {
                            "FIX_RENAME_FIELD"
                        } else {
                            "FIX_DROP_UNKNOWN_FIELD"
                        }
                        .into(),
                        class: "data".into(),
                        description: if let Some(to) = rename {
                            format!(
                                "rename unknown field {} to {to} in {}",
                                d.field.as_deref().unwrap_or("?"),
                                p.display()
                            )
                        } else {
                            format!(
                                "remove unknown field {} from {}",
                                d.field.as_deref().unwrap_or("?"),
                                p.display()
                            )
                        },
                        paths: vec![p.clone()],
                    });
                }
            }
            "TYPE_MISMATCH" => {
                if let (Some(path), Some(field)) = (&d.path, &d.field)
                    && let Some(row) = db
                        .catalog
                        .rows
                        .values()
                        .flatten()
                        .find(|r| &r.relative == path)
                    && lossless_coerce(
                        row.value.get(field).unwrap_or(&serde_json::Value::Null),
                        &db.catalog.schemas[&row.table].columns[field],
                    )
                    .is_some()
                {
                    out.push(Fix {
                        id: "FIX_COERCE_VALUE".into(),
                        class: "data".into(),
                        description: format!("losslessly coerce {} in {}", field, path.display()),
                        paths: vec![path.clone()],
                    });
                    continue;
                }
                if let Some(path) = &d.path {
                    out.push(Fix {
                        id: "TYPE_MISMATCH".into(),
                        class: "manual".into(),
                        description: d.message.clone(),
                        paths: vec![path.clone()],
                    });
                }
            }
            "FOREIGN_KEY_VIOLATION" => {
                if let Some(p) = &d.path {
                    if d.fixes.iter().any(|x| x == "FIX_ORPHAN_SET_NULL") {
                        out.push(Fix {
                            id: "FIX_ORPHAN_SET_NULL".into(),
                            class: "data".into(),
                            description: format!(
                                "set orphan foreign key to null in {}",
                                p.display()
                            ),
                            paths: vec![p.clone()],
                        });
                    }
                    out.push(Fix {
                        id: "FIX_ORPHAN_DELETE_ROW".into(),
                        class: "data".into(),
                        description: format!("delete orphan row {}", p.display()),
                        paths: vec![p.clone()],
                    });
                }
            }
            _ => {
                if let Some(p) = &d.path {
                    out.push(Fix {
                        id: d.code.clone(),
                        class: "manual".into(),
                        description: d.message.clone(),
                        paths: vec![p.clone()],
                    });
                }
            }
        }
    }
    for d in lint::lint(&db.catalog, &db.config, false) {
        match d.code.as_str() {
            "LINT_SCHEMA_UNPINNED" => out.push(Fix {
                id: "FIX_PIN_SCHEMA".into(),
                class: "schema".into(),
                description: format!("pin schema {}", d.table.as_deref().unwrap()),
                paths: vec![PathBuf::from(crate::schema_store::pin_relative(
                    d.table.as_deref().unwrap(),
                ))],
            }),
            "LINT_NULLABLE_NEVER_NULL" => out.push(Fix {
                id: "FIX_TIGHTEN_NULLABLE".into(),
                class: "schema".into(),
                description: format!(
                    "make {}.{} NOT NULL",
                    d.table.as_deref().unwrap(),
                    d.field.as_deref().unwrap()
                ),
                paths: vec![PathBuf::from(crate::schema_store::working_relative(
                    &d.table.unwrap(),
                ))],
            }),
            "LINT_FK_NO_INDEX" => out.push(Fix {
                id: "FIX_ADD_INDEX".into(),
                class: "schema".into(),
                description: d.message,
                paths: vec![PathBuf::from(crate::schema_store::working_relative(
                    &d.table.unwrap(),
                ))],
            }),
            "LINT_WIDER_TYPE"
            | "LINT_ENUM_CANDIDATE"
            | "LINT_UNIQUE_CANDIDATE"
            | "LINT_FK_CANDIDATE"
            | "LINT_CHECK_CANDIDATE"
            | "LINT_PK_NOT_GENERATED" => {
                // Named exhaustively rather than through a wildcard: a
                // seventh code added to the arm above would otherwise be
                // labelled FIX_ADD_GENERATOR and offer the wrong remedy.
                let id = match d.code.as_str() {
                    "LINT_WIDER_TYPE" => "FIX_NARROW_TYPE",
                    "LINT_ENUM_CANDIDATE" => "FIX_ADD_ENUM",
                    "LINT_UNIQUE_CANDIDATE" => "FIX_ADD_UNIQUE",
                    "LINT_FK_CANDIDATE" => "FIX_ADD_FK",
                    "LINT_CHECK_CANDIDATE" => "FIX_ADD_CHECK",
                    "LINT_PK_NOT_GENERATED" => "FIX_ADD_GENERATOR",
                    other => unreachable!(
                        "{other} reaches the schema-fix arm without a fix id; add one"
                    ),
                };
                out.push(Fix {
                    id: id.into(),
                    class: "schema".into(),
                    description: d.message,
                    paths: vec![PathBuf::from(crate::schema_store::working_relative(
                        &d.table.unwrap(),
                    ))],
                });
            }
            "LINT_NON_CANONICAL_FORMATTING" => out.push(Fix {
                id: "FIX_CANONICALIZE".into(),
                class: "data".into(),
                description: d.message,
                paths: db.catalog.rows[d.table.as_ref().unwrap()]
                    .iter()
                    .map(|r| r.relative.clone())
                    .collect(),
            }),
            _ => {}
        }
    }
    if db.manifest.is_none() || db.manifest_needs_rebuild {
        out.push(Fix {
            id: "FIX_MANIFEST".into(),
            class: "derived".into(),
            description: "rebuild manifest".into(),
            paths: vec![PathBuf::from(".db/manifest.json")],
        });
    }
    if db
        .catalog
        .warnings
        .iter()
        .any(|warning| warning.code == "INDEX_STALE")
    {
        out.push(Fix {
            id: "FIX_REINDEX".into(),
            class: "derived".into(),
            description: "rebuild stale or corrupt indexes".into(),
            paths: vec![PathBuf::from(".db/indexes")],
        });
    }
    out
}
pub fn schema_changes(db: &Database, only: Option<&str>) -> Result<Vec<Change>> {
    let findings = lint::lint(&db.catalog, &db.config, false);
    let mut schemas = db.catalog.schemas.clone();
    for d in findings {
        if only.is_some_and(|x| x != d.code && !d.fixes.iter().any(|f| f == x)) {
            continue;
        }
        let Some(t) = d.table.as_ref() else { continue };
        let s = schemas.get_mut(t).unwrap();
        match d.code.as_str() {
            "LINT_NULLABLE_NEVER_NULL" => {
                if let Some(f) = d.field.as_ref() {
                    s.columns[f].nullable = false
                }
            }
            "LINT_FK_NO_INDEX" => {
                if let Some(fk) = s
                    .foreign_keys
                    .iter()
                    .find(|fk| !s.indexes.contains(&fk.columns))
                {
                    s.indexes.push(fk.columns.clone())
                }
            }
            "LINT_WIDER_TYPE" => {
                if let Some(f) = d.field.as_ref() {
                    if s.columns[f].kind == ColumnType::Float {
                        s.columns[f].kind = ColumnType::Int;
                        continue;
                    }
                    let vals: Vec<_> = db.catalog.rows[t]
                        .iter()
                        .filter_map(|r| r.value.get(f).and_then(|v| v.as_str()))
                        .collect();
                    s.columns[f].kind = if !vals.is_empty()
                        && vals.iter().all(|x| uuid::Uuid::parse_str(x).is_ok())
                    {
                        ColumnType::Uuid
                    } else if !vals.is_empty()
                        && vals.iter().all(|x| ulid::Ulid::from_string(x).is_ok())
                    {
                        ColumnType::Ulid
                    } else if !vals.is_empty()
                        && vals
                            .iter()
                            .all(|x| chrono::NaiveDate::parse_from_str(x, "%Y-%m-%d").is_ok())
                    {
                        ColumnType::Date
                    } else {
                        ColumnType::Timestamp
                    }
                }
            }
            "LINT_ENUM_CANDIDATE" => {
                if let Some(f) = &d.field {
                    let mut values: Vec<_> = db.catalog.rows[t]
                        .iter()
                        .filter_map(|r| r.value.get(f).and_then(|v| v.as_str()).map(String::from))
                        .collect();
                    values.sort();
                    values.dedup();
                    s.columns[f].kind = ColumnType::Enum;
                    s.columns[f].values = Some(values);
                }
            }
            "LINT_UNIQUE_CANDIDATE" => {
                if let Some(f) = &d.field {
                    let cols = vec![f.clone()];
                    if !s.unique.contains(&cols) {
                        s.unique.push(cols)
                    }
                }
            }
            "LINT_CHECK_CANDIDATE" => {
                if let Some(f) = &d.field {
                    let expr = if s.columns[f].kind == ColumnType::Int {
                        format!("\"{}\" >= 0", f.replace('"', "\"\""))
                    } else {
                        format!("\"{}\" <> ''", f.replace('"', "\"\""))
                    };
                    let name = format!("{}_inferred_check", f);
                    if !s.check.iter().any(|c| c.name == name) {
                        s.check.push(crate::schema::Check { name, expr })
                    }
                }
            }
            "LINT_PK_NOT_GENERATED" => {
                for f in &s.primary_key {
                    let col = &mut s.columns[f];
                    if col.generated.is_none() {
                        col.generated = Some(crate::schema::Generated {
                            kind: if col.kind == ColumnType::Uuid {
                                crate::schema::GeneratedKind::Uuid
                            } else {
                                crate::schema::GeneratedKind::Ulid
                            },
                        })
                    }
                }
            }
            "LINT_FK_CANDIDATE" => {
                if let Some(f) = &d.field {
                    for (target, ts) in &db.catalog.schemas {
                        // A target whose primary key does not resolve to a
                        // declared column is already reported as
                        // SCHEMA_PK_COLUMN_UNKNOWN; it cannot be a foreign-key
                        // target and must not be indexed blindly here.
                        if target == t
                            || ts.primary_key.len() != 1
                            || !ts.columns.contains_key(&ts.primary_key[0])
                            || ts.columns[&ts.primary_key[0]].kind != s.columns[f].kind
                        {
                            continue;
                        }
                        let targets: std::collections::HashSet<_> = db.catalog.rows[target]
                            .iter()
                            .filter_map(|r| r.value.get(&ts.primary_key[0]).map(canonical::compact))
                            .collect();
                        if db.catalog.rows[t]
                            .iter()
                            .filter_map(|r| r.value.get(f))
                            .filter(|v| !v.is_null())
                            .all(|v| targets.contains(&canonical::compact(v)))
                        {
                            s.foreign_keys.push(crate::schema::ForeignKey {
                                columns: vec![f.clone()],
                                references: crate::schema::Reference {
                                    table: target.clone(),
                                    columns: ts.primary_key.clone(),
                                },
                                on_delete: Some(crate::schema::Action::Restrict),
                                on_update: Some(crate::schema::Action::Restrict),
                            });
                            s.indexes.push(vec![f.clone()]);
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = vec![];
    for (t, s) in schemas {
        let old = crate::schema::json_schema::encode(&db.catalog.schemas[&t]);
        let new = crate::schema::json_schema::encode(&s);
        if old != new {
            out.push(Change::Write {
                path: PathBuf::from(crate::schema_store::working_relative(&t)),
                bytes: canonical::pretty_with_indent(&new, db.config.indentation_width),
            })
        }
    }
    Ok(out)
}

/// Promote unpinned working schemas into declarations.
///
/// `LINT_SCHEMA_UNPINNED` says a table's schema exists only under `.db/`, where
/// deleting the directory discards any refinement inference cannot re-derive.
/// The remedy is to copy it to `schema/`, which is what `db schema pin` does --
/// so the fix performs that copy rather than editing the schema, which is why
/// it lives here and not in the lint-driven rewriting above.
///
/// A table that already has a pin never raises the finding, so this can only
/// ever create a declaration, never replace one. Replacing a pin discards
/// something a person wrote and stays an explicit `db schema pin --overwrite`.
fn pin_changes(db: &Database, only: Option<&str>) -> Result<Vec<Change>> {
    let mut out = vec![];
    for d in lint::lint(&db.catalog, &db.config, false) {
        if d.code != "LINT_SCHEMA_UNPINNED"
            || !only.is_none_or(|x| x == d.code || d.fixes.iter().any(|f| f == x))
        {
            continue;
        }
        let Some(table) = d.table.as_ref() else {
            continue;
        };
        let Some(schema) = db.catalog.schemas.get(table) else {
            continue;
        };
        out.push(Change::Write {
            path: PathBuf::from(crate::schema_store::pin_relative(table)),
            bytes: crate::schema_store::canonical_bytes(schema, db.config.indentation_width)?,
        });
    }
    Ok(out)
}

pub fn repair_changes(db: &Database, only: Option<&str>, allow_data: bool) -> Result<Vec<Change>> {
    let mut out = schema_changes(db, only)?;
    out.extend(pin_changes(db, only)?);
    if allow_data && only == Some("FIX_CANONICALIZE") {
        for (table, rows) in &db.catalog.rows {
            let s = &db.catalog.schemas[table];
            for row in rows {
                let bytes = canonical::pretty_with_indent(
                    &canonical::canonical_row(&row.value, s),
                    db.config.indentation_width,
                );
                if bytes != row.raw {
                    out.push(Change::Write {
                        path: row.relative.clone(),
                        bytes,
                    });
                }
            }
        }
    }
    for d in &db.diagnostics {
        if d.code == "IDENTITY_MISMATCH"
            && only.is_none_or(|x| x == "IDENTITY_MISMATCH" || x == "FIX_RENAME_TO_IDENTITY")
            && let (Some(old), Some(expected)) = (&d.path, &d.expected)
            && !expected.contains('/')
        {
            let new = old
                .parent()
                .unwrap_or(std::path::Path::new(""))
                .join(expected);
            let bytes = std::fs::read(db.root.join(old))
                .map_err(|e| crate::diagnostic::DbError::io(&db.root.join(old), e))?;
            out.push(Change::Delete { path: old.clone() });
            out.push(Change::Write { path: new, bytes });
        }
        if !allow_data {
            continue;
        }
        if d.code == "ROW_UNKNOWN_FIELD"
            && only.is_none_or(|x| {
                x == "ROW_UNKNOWN_FIELD" || x == "FIX_DROP_UNKNOWN_FIELD" || x == "FIX_RENAME_FIELD"
            })
        {
            if only == Some("FIX_RENAME_FIELD") && near_field(db, d).is_none() {
                continue;
            }
            if let (Some(path), Some(field)) = (&d.path, &d.field)
                && let Some(row) = db
                    .catalog
                    .rows
                    .values()
                    .flatten()
                    .find(|r| &r.relative == path)
            {
                let s = &db.catalog.schemas[&row.table];
                let mut value = row.value.clone();
                let old = value.remove(field).unwrap_or(serde_json::Value::Null);
                if only != Some("FIX_DROP_UNKNOWN_FIELD")
                    && let Some(to) = near_field(db, d)
                {
                    value.insert(to, old);
                }
                out.push(Change::Write {
                    path: path.clone(),
                    bytes: canonical::pretty_with_indent(
                        &canonical::canonical_row(&value, s),
                        db.config.indentation_width,
                    ),
                });
            }
        }
        if d.code == "TYPE_MISMATCH"
            && only.is_none_or(|x| x == "TYPE_MISMATCH" || x == "FIX_COERCE_VALUE")
            && let (Some(path), Some(field)) = (&d.path, &d.field)
            && let Some(row) = db
                .catalog
                .rows
                .values()
                .flatten()
                .find(|r| &r.relative == path)
        {
            let s = &db.catalog.schemas[&row.table];
            if let Some(value) = lossless_coerce(
                row.value.get(field).unwrap_or(&serde_json::Value::Null),
                &s.columns[field],
            ) {
                let mut body = row.value.clone();
                body.insert(field.clone(), value);
                out.push(Change::Write {
                    path: path.clone(),
                    bytes: canonical::pretty_with_indent(
                        &canonical::canonical_row(&body, s),
                        db.config.indentation_width,
                    ),
                });
            }
        }
        if d.code == "FOREIGN_KEY_VIOLATION"
            && only.is_none_or(|x| x == "FIX_ORPHAN_SET_NULL")
            && let (Some(path), Some(fields)) = (&d.path, &d.field)
            && let Some(row) = db
                .catalog
                .rows
                .values()
                .flatten()
                .find(|r| &r.relative == path)
        {
            let s = &db.catalog.schemas[&row.table];
            let names: Vec<_> = fields.split(',').collect();
            if names
                .iter()
                .all(|f| s.columns.get(*f).is_some_and(|c| c.nullable))
            {
                let mut value = row.value.clone();
                for f in names {
                    value.insert(f.into(), serde_json::Value::Null);
                }
                out.push(Change::Write {
                    path: path.clone(),
                    bytes: canonical::pretty_with_indent(
                        &canonical::canonical_row(&value, s),
                        db.config.indentation_width,
                    ),
                });
            }
        }
        if d.code == "FOREIGN_KEY_VIOLATION"
            && (only.is_some_and(|x| x == "FOREIGN_KEY_VIOLATION" || x == "FIX_ORPHAN_DELETE_ROW")
                || (only.is_none() && !d.fixes.iter().any(|x| x == "FIX_ORPHAN_SET_NULL")))
            && let Some(path) = &d.path
            && let Some(row) = db
                .catalog
                .rows
                .values()
                .flatten()
                .find(|r| &r.relative == path)
        {
            let s = &db.catalog.schemas[&row.table];
            let where_sql = s
                .primary_key
                .iter()
                .map(|c| format!("\"{}\" = ?", c.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ");
            let query = format!(
                "DELETE FROM \"{}\" WHERE {where_sql}",
                row.table.replace('"', "\"\"")
            );
            let params: Vec<_> = s
                .primary_key
                .iter()
                .map(|c| row.value.get(c).cloned().unwrap_or(serde_json::Value::Null))
                .collect();
            out.extend(crate::sql::execute(&db.catalog, &query, &params)?.changes);
        }
    }
    let mut moves = std::collections::BTreeMap::<PathBuf, PathBuf>::new();
    for diagnostic in &db.diagnostics {
        if diagnostic.code != "IDENTITY_MISMATCH"
            || !only.is_none_or(|value| {
                value == "IDENTITY_MISMATCH" || value == "FIX_RENAME_TO_IDENTITY"
            })
        {
            continue;
        }
        if let (Some(old), Some(expected)) = (&diagnostic.path, &diagnostic.expected)
            && !expected.contains('/')
        {
            moves.insert(
                old.clone(),
                old.parent()
                    .unwrap_or(std::path::Path::new(""))
                    .join(expected),
            );
        }
    }
    // Merge independent, lossless edits to the same row relative to the
    // observed body. This prevents one safe fix from silently undoing another.
    let mut dedup = std::collections::BTreeMap::<PathBuf, Change>::new();
    for change in out {
        match change {
            Change::Delete { path } => {
                dedup.insert(path.clone(), Change::Delete { path });
            }
            Change::Write { path, bytes } => {
                let effective = moves.get(&path).cloned().unwrap_or(path.clone());
                let source = moves
                    .iter()
                    .find_map(|(old, new)| (new == &effective).then_some(old))
                    .unwrap_or(&path);
                let original = db
                    .catalog
                    .rows
                    .values()
                    .flatten()
                    .find(|row| &row.relative == source);
                let merged = match (dedup.get(&effective), original) {
                    (Some(Change::Write { bytes: prior, .. }), Some(original)) => merge_row_edits(
                        prior,
                        &bytes,
                        &original.value,
                        &db.catalog.schemas[&original.table],
                        db.config.indentation_width,
                    )?,
                    _ => bytes,
                };
                dedup.insert(
                    effective.clone(),
                    Change::Write {
                        path: effective,
                        bytes: merged,
                    },
                );
            }
        }
    }
    Ok(dedup.into_values().collect())
}
fn merge_row_edits(
    prior: &[u8],
    next: &[u8],
    original: &serde_json::Map<String, serde_json::Value>,
    schema: &crate::schema::Schema,
    indentation_width: usize,
) -> Result<Vec<u8>> {
    let mut merged = crate::json::parse(prior)
        .map_err(|error| {
            crate::diagnostic::DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                format!("doctor generated invalid JSON: {error}"),
                6,
            )
        })?
        .as_object()
        .cloned()
        .ok_or_else(|| {
            crate::diagnostic::DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                "doctor generated a non-object row",
                6,
            )
        })?;
    let next = crate::json::parse(next)
        .map_err(|error| {
            crate::diagnostic::DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                format!("doctor generated invalid JSON: {error}"),
                6,
            )
        })?
        .as_object()
        .cloned()
        .ok_or_else(|| {
            crate::diagnostic::DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                "doctor generated a non-object row",
                6,
            )
        })?;
    let keys = original
        .keys()
        .chain(next.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    for key in keys {
        if next.get(&key) != original.get(&key) {
            if let Some(value) = next.get(&key) {
                merged.insert(key, value.clone());
            } else {
                merged.remove(&key);
            }
        }
    }
    Ok(canonical::pretty_with_indent(
        &canonical::canonical_row(&merged, schema),
        indentation_width,
    ))
}
fn lossless_coerce(
    value: &serde_json::Value,
    column: &crate::schema::Column,
) -> Option<serde_json::Value> {
    crate::value::lossless_convert(value, column)
}
fn near_field(db: &Database, d: &crate::diagnostic::Diagnostic) -> Option<String> {
    let table = d.table.as_ref()?;
    let unknown = d.field.as_ref()?;
    let row = db.catalog.rows[table]
        .iter()
        .find(|r| d.path.as_ref() == Some(&r.relative))?;
    nearest_missing_column(&db.catalog.schemas[table], &row.value, unknown)
}

/// The schema column that an unknown field most plausibly misspells, if any.
///
/// A rename is only proposed when the intent is unambiguous: the column must be
/// within a small edit distance, must not already be present in the row (which
/// would make the rename destructive), and must be strictly closer than every
/// other candidate. A tie is not a typo this can resolve on the user's behalf
/// (Section 14).
fn nearest_missing_column(
    schema: &crate::schema::Schema,
    row: &serde_json::Map<String, serde_json::Value>,
    unknown: &str,
) -> Option<String> {
    let mut candidates = schema
        .columns
        .keys()
        .filter(|name| !row.contains_key(*name))
        .filter_map(|name| {
            let distance = strsim::levenshtein(unknown, name);
            (distance <= 2).then_some((distance, name.clone()))
        })
        .collect::<Vec<_>>();
    candidates.sort();
    if candidates.len() == 1
        || candidates
            .first()
            .zip(candidates.get(1))
            .is_some_and(|(a, b)| a.0 < b.0)
    {
        candidates.first().map(|x| x.1.clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{AdditionalFields, Column, ColumnType, Schema};
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
            description: None,
            annotations: Default::default(),
        }
    }

    fn schema(columns: &[(&str, ColumnType)]) -> Schema {
        let mut map = IndexMap::new();
        for (name, kind) in columns {
            map.insert((*name).to_string(), column(kind.clone()));
        }
        Schema {
            table: "t".into(),
            schema_version: 1,
            schema_format: None,
            description: None,
            primary_key: vec!["id".into()],
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

    /// Section 14: FIX_COERCE_VALUE applies only lossless coercions. Doctor must
    /// never guess at a conversion that would change the logical value, because
    /// the fix rewrites authoritative data.
    #[test]
    fn test1030_value_coercion_offered_by_doctor_is_always_lossless() {
        // Representational corrections that preserve the value.
        assert_eq!(
            lossless_coerce(&json!("42"), &column(ColumnType::Int)),
            Some(json!(42))
        );
        assert_eq!(
            lossless_coerce(&json!("true"), &column(ColumnType::Bool)),
            Some(json!(true))
        );

        // Conversions that would lose or invent information are refused, so no
        // fix is offered and doctor classifies the violation as manual instead.
        assert_eq!(lossless_coerce(&json!(1.5), &column(ColumnType::Int)), None);
        assert_eq!(
            lossless_coerce(&json!("01"), &column(ColumnType::Int)),
            None
        );
        assert_eq!(
            lossless_coerce(&json!("yes"), &column(ColumnType::Bool)),
            None
        );
        assert_eq!(
            lossless_coerce(&json!("not-a-uuid"), &column(ColumnType::Uuid)),
            None
        );
    }

    /// Section 14: FIX_RENAME_FIELD is offered when an unknown field is a near
    /// miss for a schema column that the row is missing. The match must be
    /// unambiguous: a tie between two equally close columns is not a typo that
    /// doctor may resolve on the user's behalf.
    #[test]
    fn test1031_field_rename_suggestions_require_an_unambiguous_near_match() {
        let s = schema(&[
            ("id", ColumnType::String),
            ("email", ColumnType::String),
            ("name", ColumnType::String),
        ]);

        // "emial" is distance 2 from "email" and far from everything else.
        let row = json!({"id": "a", "emial": "x"});
        let row = row.as_object().unwrap().clone();
        assert_eq!(
            nearest_missing_column(&s, &row, "emial"),
            Some("email".to_string())
        );

        // A column already present in the row is not a rename target: renaming
        // onto it would destroy the value that is already there.
        let occupied = json!({"id": "a", "email": "real", "emial": "x"});
        assert_eq!(
            nearest_missing_column(&s, occupied.as_object().unwrap(), "emial"),
            None
        );

        // Too distant to be a typo.
        let distant = json!({"id": "a", "telephone": "x"});
        assert_eq!(
            nearest_missing_column(&s, distant.as_object().unwrap(), "telephone"),
            None
        );

        // An exact tie between two candidates is ambiguous and must be refused.
        let tied = schema(&[
            ("id", ColumnType::String),
            ("ax", ColumnType::String),
            ("bx", ColumnType::String),
        ]);
        let row = json!({"id": "a", "cx": "v"});
        assert_eq!(
            nearest_missing_column(&tied, row.as_object().unwrap(), "cx"),
            None,
            "a tie must not be resolved by chance ordering"
        );
    }
}
