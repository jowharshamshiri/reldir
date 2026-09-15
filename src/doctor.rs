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
            "LINT_SCHEMA_UNREVIEWED" => out.push(Fix {
                id: "FIX_ACCEPT_INFERRED".into(),
                class: "schema".into(),
                description: format!("accept inferred schema {}", d.table.as_deref().unwrap()),
                paths: vec![PathBuf::from(format!("schema/{}.json", d.table.unwrap()))],
            }),
            "LINT_NULLABLE_NEVER_NULL" => out.push(Fix {
                id: "FIX_TIGHTEN_NULLABLE".into(),
                class: "schema".into(),
                description: format!(
                    "make {}.{} NOT NULL",
                    d.table.as_deref().unwrap(),
                    d.field.as_deref().unwrap()
                ),
                paths: vec![PathBuf::from(format!("schema/{}.json", d.table.unwrap()))],
            }),
            "LINT_FK_NO_INDEX" => out.push(Fix {
                id: "FIX_ADD_INDEX".into(),
                class: "schema".into(),
                description: d.message,
                paths: vec![PathBuf::from(format!("schema/{}.json", d.table.unwrap()))],
            }),
            "LINT_WIDER_TYPE"
            | "LINT_ENUM_CANDIDATE"
            | "LINT_UNIQUE_CANDIDATE"
            | "LINT_FK_CANDIDATE"
            | "LINT_CHECK_CANDIDATE"
            | "LINT_PK_NOT_GENERATED" => {
                let id = match d.code.as_str() {
                    "LINT_WIDER_TYPE" => "FIX_NARROW_TYPE",
                    "LINT_ENUM_CANDIDATE" => "FIX_ADD_ENUM",
                    "LINT_UNIQUE_CANDIDATE" => "FIX_ADD_UNIQUE",
                    "LINT_FK_CANDIDATE" => "FIX_ADD_FK",
                    "LINT_CHECK_CANDIDATE" => "FIX_ADD_CHECK",
                    _ => "FIX_ADD_GENERATOR",
                };
                out.push(Fix {
                    id: id.into(),
                    class: "schema".into(),
                    description: d.message,
                    paths: vec![PathBuf::from(format!("schema/{}.json", d.table.unwrap()))],
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
            "LINT_SCHEMA_UNREVIEWED" => s.inferred = None,
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
        let old = serde_json::to_value(&db.catalog.schemas[&t]).unwrap();
        let new = serde_json::to_value(&s).unwrap();
        if old != new {
            out.push(Change::Write {
                path: PathBuf::from(format!("schema/{t}.json")),
                bytes: canonical::pretty_with_indent(&new, db.config.indentation_width),
            })
        }
    }
    Ok(out)
}

pub fn repair_changes(db: &Database, only: Option<&str>, allow_data: bool) -> Result<Vec<Change>> {
    let mut out = schema_changes(db, only)?;
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
    let mut candidates = db.catalog.schemas[table]
        .columns
        .keys()
        .filter(|name| !row.value.contains_key(*name))
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
