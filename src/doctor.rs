//! Repairs: what can be done about each fault and finding, least destructive
//! first.
//!
//! Every error `check` reports and every lint finding with a remedy becomes one
//! or more *alternatives*, each a concrete change computed here -- the exact
//! schema documents, row values or renames it would write. Where several could
//! resolve one fault they are ordered from least to most destructive, and the
//! first is the default: a dangling reference is first answered by restoring
//! the row it named from recorded history, then by removing the reference,
//! and only when asked by deleting the row that holds it.
//!
//! Doctor never decides what a person must: a fault with no safe repair is
//! reported as `manual`, with what to look at.

use crate::{
    catalog::{Catalog, Row},
    db::Database,
    diagnostic::{DbError, Diagnostic, Result},
    lint::{self, Remedy},
    plan::RowChange,
    schema::path::pointer_tokens,
    transaction::Change,
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// What a fix touches, which decides when it may be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Schema documents, including creating a pin.
    Schema,
    /// File names; contents unchanged.
    Layout,
    /// Row contents.
    Data,
    /// Nothing: a person must decide.
    Manual,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Self::Schema => "schema",
            Self::Layout => "layout",
            Self::Data => "data",
            Self::Manual => "manual",
        }
    }
}

/// One way to resolve one problem.
#[derive(Debug, Clone)]
pub struct Fix {
    pub id: &'static str,
    pub class: Class,
    pub description: String,
    /// The code of the fault or finding it resolves.
    pub resolves: String,
    /// Where that fault or finding is.
    pub at: Option<PathBuf>,
    /// 0 for the default alternative; higher is more destructive.
    pub rank: usize,
    /// Whether it loses data a person wrote: a deleted row, a removed value.
    pub destructive: bool,
    pub action: Action,
}

#[derive(Debug, Clone)]
pub enum Action {
    /// New documents for these tables' schemas.
    Schemas(BTreeMap<String, Value>),
    /// Make the table's working schema its pin.
    Pin(String),
    Rename { from: PathBuf, to: PathBuf },
    /// Row edits, completed by referential actions when applied.
    Rows(Vec<RowChange>),
    /// Rewrite files in canonical form.
    Canonicalize(Vec<PathBuf>),
    Manual,
}

impl Fix {
    /// The paths the fix writes, relative to the root.
    pub fn paths(&self, catalog: &Catalog) -> Vec<PathBuf> {
        match &self.action {
            Action::Schemas(documents) => documents.keys().map(|table| crate::schema_store::home(catalog, table)).collect(),
            Action::Pin(table) => vec![PathBuf::from(crate::schema_store::pin_relative(table))],
            Action::Rename { from, to } => vec![from.clone(), to.clone()],
            Action::Rows(rows) => rows
                .iter()
                .filter_map(|change| {
                    change.before.as_ref().map(|row| row.relative.clone()).or_else(|| {
                        let schema = catalog.schemas.get(&change.table)?;
                        crate::plan::row_path(schema, change.after.as_ref()?).ok()
                    })
                })
                .collect(),
            Action::Canonicalize(paths) => paths.clone(),
            Action::Manual => self.at.iter().cloned().collect(),
        }
    }

    pub fn to_json(&self, catalog: &Catalog) -> Value {
        json!({
            "kind": "fix",
            "id": self.id,
            "class": self.class.name(),
            "description": self.description,
            "resolves": self.resolves,
            "at": self.at,
            "default": self.rank == 0,
            "destructive": self.destructive,
            "paths": self.paths(catalog),
        })
    }
}

/// Every fix doctor could apply now, each problem's alternatives in order.
pub fn plan(database: &Database) -> Result<Vec<Fix>> {
    let catalog = &database.catalog;
    let mut out = vec![];
    for diagnostic in &database.verdict.errors {
        out.extend(fixes_for(database, diagnostic)?);
    }
    // Schema refinements are offered only for a valid database: tightening a
    // schema over rows that already break it would bury the faults under
    // more of them.
    if database.is_valid() {
        for finding in lint::lint(catalog, &database.config, false)? {
            let Some(remedy) = finding.remedy else { continue };
            let Some(id) = finding.diagnostic.fixes.first().map(|fix| fix_id(fix)) else { continue };
            let action = match remedy {
                Remedy::Edit(documents) => Action::Schemas(documents),
                Remedy::Pin(table) => Action::Pin(table),
                Remedy::Canonicalize(paths) => Action::Canonicalize(paths),
            };
            out.push(Fix {
                id,
                class: if matches!(action, Action::Canonicalize(_)) { Class::Data } else { Class::Schema },
                description: finding.diagnostic.message.clone(),
                resolves: finding.diagnostic.code.clone(),
                at: finding.diagnostic.path.clone(),
                rank: 0,
                destructive: false,
                action,
            });
        }
    }
    Ok(out)
}

/// A fix id as a `'static` name, from the fixed catalogue.
fn fix_id(text: &str) -> &'static str {
    FIXES
        .iter()
        .find(|(id, _)| *id == text)
        .map(|(id, _)| *id)
        .unwrap_or("FIX_MANUAL")
}

/// Every fix, and what it does. The catalogue `doctor --explain` reads.
pub const FIXES: &[(&str, &str)] = &[
    ("FIX_RESTORE_TARGET", "restores the row a dangling reference names, exactly as recorded history last had it"),
    ("FIX_REMOVE_REFERENCE", "removes the dangling reference: the array element that holds it, or -- where no array holds it -- the value, which becomes null"),
    ("FIX_ORPHAN_DELETE_ROW", "deletes the row holding a dangling reference, with whatever its own referrers' actions require"),
    ("FIX_RENAME_TO_IDENTITY", "renames a row file to the name its key gives it; its contents are unchanged"),
    ("FIX_RENAME_FIELD", "renames an unknown member to the missing column it is an unambiguous misspelling of"),
    ("FIX_DROP_UNKNOWN_FIELD", "removes a member the schema does not declare"),
    ("FIX_COERCE_VALUE", "replaces a value with the same value written as its column's type, where the conversion loses nothing"),
    ("FIX_FILL_DEFAULT", "sets an absent or null column to the default its schema declares"),
    ("FIX_PIN_SCHEMA", "moves an inferred working schema to schema/, making it the table's declaration"),
    ("FIX_TIGHTEN_NULLABLE", "removes null from a column's type"),
    ("FIX_NARROW_TYPE", "narrows a column to the type every value already has"),
    ("FIX_ADD_ENUM", "restricts a column to the values it holds"),
    ("FIX_ADD_UNIQUE", "declares a column unique"),
    ("FIX_ADD_FK", "declares a reference the data already satisfies, joining tables to an identity domain where needed"),
    ("FIX_ADD_CHECK", "declares a check every row already passes"),
    ("FIX_ADD_GENERATOR", "generates a uuid or ulid key when an insert supplies none"),
    ("FIX_CANONICALIZE", "rewrites row files in canonical formatting; the rows are unchanged"),
    ("FIX_MANUAL", "nothing: the fault needs a person's decision"),
];

fn fixes_for(database: &Database, diagnostic: &Diagnostic) -> Result<Vec<Fix>> {
    let catalog = &database.catalog;
    let base = |id: &'static str, class: Class, description: String, rank: usize, destructive: bool, action: Action| Fix {
        id,
        class,
        description,
        resolves: diagnostic.code.clone(),
        at: diagnostic.path.clone(),
        rank,
        destructive,
        action,
    };
    let manual = || {
        vec![base(
            "FIX_MANUAL",
            Class::Manual,
            diagnostic.help.clone().unwrap_or_else(|| diagnostic.message.clone()),
            0,
            false,
            Action::Manual,
        )]
    };
    let Some(path) = diagnostic.path.clone() else { return Ok(manual()) };
    let row = catalog.row_at(&path)?;
    let offered = |fix: &str| diagnostic.fixes.iter().any(|offered| offered == fix);
    let mut out = vec![];
    match diagnostic.code.as_str() {
        "IDENTITY_MISMATCH" if offered("FIX_RENAME_TO_IDENTITY") => {
            if let Some(expected) = &diagnostic.expected {
                let to = path.parent().unwrap_or(Path::new("")).join(expected);
                if catalog.mirror.file(&crate::catalog::slash(&to))?.is_none() {
                    out.push(base(
                        "FIX_RENAME_TO_IDENTITY",
                        Class::Layout,
                        format!("rename {} to {}", path.display(), to.display()),
                        0,
                        false,
                        Action::Rename { from: path.clone(), to },
                    ));
                }
            }
        }
        "FOREIGN_KEY_VIOLATION" => {
            let Some(row) = row else { return Ok(manual()) };
            if let Some(restore) = restoration(database, diagnostic)? {
                out.push(base(
                    "FIX_RESTORE_TARGET",
                    Class::Data,
                    format!(
                        "restore {} from revision {}, which {} still names",
                        restore.1.display(),
                        restore.2,
                        path.display()
                    ),
                    out.len(),
                    false,
                    Action::Rows(vec![RowChange::insert(&restore.0, restore.3)]),
                ));
            }
            if offered("FIX_REMOVE_REFERENCE")
                && let Some(pointer) = &diagnostic.pointer
                && let Some(edited) = remove_reference(&row.value, pointer)
            {
                out.push(base(
                    "FIX_REMOVE_REFERENCE",
                    Class::Data,
                    format!("remove the reference at {pointer} in {}", path.display()),
                    out.len(),
                    true,
                    Action::Rows(vec![RowChange::update(row.clone(), edited)]),
                ));
            }
            out.push(base(
                "FIX_ORPHAN_DELETE_ROW",
                Class::Data,
                format!("delete {}, which holds the dangling reference", path.display()),
                out.len(),
                true,
                Action::Rows(vec![RowChange::delete(row)]),
            ));
        }
        "ROW_UNKNOWN_FIELD" => {
            let (Some(row), Some(field)) = (row, diagnostic.field.clone()) else { return Ok(manual()) };
            if offered("FIX_RENAME_FIELD")
                && let Some(to) = &diagnostic.expected
            {
                let mut edited = row.value.clone();
                if let Some(value) = edited.shift_remove(&field) {
                    edited.insert(to.clone(), value);
                }
                out.push(base(
                    "FIX_RENAME_FIELD",
                    Class::Data,
                    format!("rename member {field:?} to {to:?} in {}", path.display()),
                    out.len(),
                    false,
                    Action::Rows(vec![RowChange::update(row.clone(), edited)]),
                ));
            }
            let mut edited = row.value.clone();
            edited.shift_remove(&field);
            out.push(base(
                "FIX_DROP_UNKNOWN_FIELD",
                Class::Data,
                format!("remove member {field:?} from {}", path.display()),
                out.len(),
                true,
                Action::Rows(vec![RowChange::update(row, edited)]),
            ));
        }
        "TYPE_MISMATCH" if offered("FIX_COERCE_VALUE") => {
            let (Some(row), Some(field)) = (row, diagnostic.field.clone()) else { return Ok(manual()) };
            let schema = &catalog.schemas[&row.table];
            if let Some(value) = schema.validator().coercion(&Value::Object(row.value.clone()), &field) {
                let mut edited = row.value.clone();
                edited.insert(field.clone(), value.clone());
                out.push(base(
                    "FIX_COERCE_VALUE",
                    Class::Data,
                    format!("write {field} in {} as {value}", path.display()),
                    0,
                    false,
                    Action::Rows(vec![RowChange::update(row, edited)]),
                ));
            }
        }
        "ROW_MISSING_FIELD" | "NOT_NULL_VIOLATION" if offered("FIX_FILL_DEFAULT") => {
            let (Some(row), Some(field)) = (row, diagnostic.field.clone()) else { return Ok(manual()) };
            let schema = &catalog.schemas[&row.table];
            if let Some(default) = schema.column(&field).and_then(|column| column.default()).cloned() {
                let mut edited = row.value.clone();
                edited.insert(field.clone(), default.clone());
                out.push(base(
                    "FIX_FILL_DEFAULT",
                    Class::Data,
                    format!("set {field} in {} to its default {default}", path.display()),
                    0,
                    false,
                    Action::Rows(vec![RowChange::update(row, edited)]),
                ));
            }
        }
        _ => {}
    }
    if out.is_empty() {
        return Ok(manual());
    }
    Ok(out)
}

/// The row a dangling reference names, as recorded history last had it:
/// (table, path, revision, row).
fn restoration(database: &Database, diagnostic: &Diagnostic) -> Result<Option<(String, PathBuf, u64, Map<String, Value>)>> {
    let catalog = &database.catalog;
    let (Some(observed), Some(table)) = (&diagnostic.observed, &diagnostic.table) else {
        return Ok(None);
    };
    let Some(schema) = catalog.schemas.get(table) else { return Ok(None) };
    let Some(fk) = schema.foreign_keys().iter().find(|fk| {
        diagnostic
            .constraint
            .as_deref()
            .is_some_and(|constraint| constraint.starts_with(&fk.describe(table)))
    }) else {
        return Ok(None);
    };
    let Ok(Value::Array(key)) = serde_json::from_str::<Value>(observed) else { return Ok(None) };
    for target in crate::integrity::target_tables(&catalog.schemas, fk.to()) {
        let Some(target_schema) = catalog.schemas.get(&target) else { continue };
        let columns = crate::integrity::target_columns(fk, target_schema);
        if columns != target_schema.primary_key() || columns.len() != key.len() {
            continue;
        }
        let probe: Map<String, Value> = columns.iter().cloned().zip(key.iter().cloned()).collect();
        let Ok(relative) = crate::plan::row_path(target_schema, &probe) else { continue };
        if let Some((revision, Value::Object(row))) = crate::metadata::last_known(&database.root, &crate::catalog::slash(&relative))? {
            return Ok(Some((target, relative, revision, row)));
        }
    }
    Ok(None)
}

/// A row with the reference at `pointer` removed: the array element holding
/// it, when the reference sits inside an array, else the value set to null.
fn remove_reference(row: &Map<String, Value>, pointer: &str) -> Option<Map<String, Value>> {
    let tokens = pointer_tokens(pointer);
    let mut root = Value::Object(row.clone());
    // The deepest array element on the way to the value.
    let mut element: Option<usize> = None;
    {
        let mut at = &root;
        for (depth, token) in tokens.iter().enumerate() {
            at = match at {
                Value::Array(items) => {
                    element = Some(depth);
                    items.get(token.parse::<usize>().ok()?)?
                }
                Value::Object(members) => members.get(token)?,
                _ => return None,
            };
        }
    }
    match element {
        Some(depth) => {
            let parent: String = tokens[..depth].iter().map(|t| format!("/{}", crate::schema::path::escape_pointer(t))).collect();
            let index: usize = tokens[depth].parse().ok()?;
            root.pointer_mut(&parent)?.as_array_mut()?.remove(index);
        }
        None => *root.pointer_mut(pointer)? = Value::Null,
    }
    match root {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// The fixes a run applies: the chosen alternative of each problem, never two
/// that touch the same file. Returns what was chosen and what was deferred to
/// a later run because it touches a file another chosen fix changes.
pub fn select<'f>(
    catalog: &Catalog,
    fixes: &'f [Fix],
    only: Option<&str>,
    allow_data: bool,
) -> (Vec<&'f Fix>, Vec<&'f Fix>) {
    let mut chosen = vec![];
    let mut deferred = vec![];
    let mut touched: BTreeSet<PathBuf> = BTreeSet::new();
    let mut resolved: BTreeSet<(String, Option<PathBuf>, String)> = BTreeSet::new();
    for fix in fixes {
        if fix.class == Class::Manual {
            continue;
        }
        let selected = match only {
            // Naming an alternative chooses it over the default.
            Some(only) => only == fix.id || (only == fix.resolves && fix.rank == 0),
            None => fix.rank == 0,
        };
        if !selected || (fix.class == Class::Data && !allow_data) {
            continue;
        }
        let problem = (fix.resolves.clone(), fix.at.clone(), fix.description.clone());
        if !resolved.insert(problem) {
            continue;
        }
        let paths = fix.paths(catalog);
        if paths.iter().any(|path| touched.contains(path)) {
            deferred.push(fix);
            continue;
        }
        touched.extend(paths);
        chosen.push(fix);
    }
    (chosen, deferred)
}

/// The changes a set of fixes amounts to, ready for one transaction.
pub fn changes(database: &Database, fixes: &[&Fix]) -> Result<(Vec<RowChange>, Vec<Change>)> {
    let catalog = &database.catalog;
    let width = database.config.indentation_width;
    let mut rows = vec![];
    let mut files = vec![];
    for fix in fixes {
        match &fix.action {
            Action::Schemas(documents) => {
                for (table, document) in documents {
                    files.push(Change::Write {
                        path: crate::schema_store::home(catalog, table),
                        bytes: crate::canonical::pretty_with_indent(document, width),
                    });
                }
            }
            Action::Pin(table) => {
                let schema = catalog.schemas.get(table).ok_or_else(|| catalog.unknown_table(table))?;
                files.push(Change::Write {
                    path: PathBuf::from(crate::schema_store::pin_relative(table)),
                    bytes: schema.bytes(width),
                });
                files.push(Change::Delete { path: crate::schema_store::working_relative(table) });
            }
            Action::Rename { from, to } => {
                let bytes = std::fs::read(database.root.join(from)).map_err(|e| DbError::io(&database.root.join(from), e))?;
                files.push(Change::Delete { path: from.clone() });
                files.push(Change::Write { path: to.clone(), bytes });
            }
            Action::Rows(changes) => rows.extend(changes.iter().cloned()),
            Action::Canonicalize(paths) => {
                for path in paths {
                    let row: Row = catalog
                        .row_at(path)?
                        .ok_or_else(|| DbError::new("CONCURRENT_MODIFICATION", format!("{} is gone", path.display()), 3))?;
                    let schema = &catalog.schemas[&row.table];
                    files.push(Change::Write {
                        path: path.clone(),
                        bytes: crate::canonical::pretty_with_indent(&crate::canonical::canonical_row(&row.value, schema), width),
                    });
                }
            }
            Action::Manual => {}
        }
    }
    Ok((rows, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test2200_removing_a_reference_removes_the_deepest_array_element_holding_it() {
        let row: Map<String, Value> = serde_json::from_value(json!({
            "id": "c1",
            "modules": [{"lessons": [{"lesson_ref": "l1"}, {"lesson_ref": "gone"}]}],
            "owner": "ghost"
        }))
        .unwrap();
        let edited = remove_reference(&row, "/modules/0/lessons/1/lesson_ref").unwrap();
        assert_eq!(edited["modules"], json!([{"lessons": [{"lesson_ref": "l1"}]}]));
        let nulled = remove_reference(&row, "/owner").unwrap();
        assert_eq!(nulled["owner"], Value::Null, "with no array, the value itself is removed");
        assert!(remove_reference(&row, "/missing").is_none());
    }

    #[test]
    fn test2201_every_fix_has_an_explanation_and_is_documented() {
        let documentation = std::fs::read_to_string("docs/validation.md").unwrap();
        for (id, explanation) in FIXES {
            assert!(!explanation.is_empty());
            assert!(documentation.contains(&format!("`{id}`")), "{id} is not documented");
        }
        assert_eq!(fix_id("FIX_ADD_FK"), "FIX_ADD_FK");
        assert_eq!(fix_id("FIX_NOT_A_FIX"), "FIX_MANUAL");
    }
}
