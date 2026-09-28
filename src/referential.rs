//! Referential actions: what happens to references when the row they name
//! goes away or changes its key.
//!
//! Given the rows a mutation changes, this computes everything else that must
//! change for every reference in the database to remain true -- or refuses the
//! mutation, naming every reference that forbids it. The closure is complete
//! before anything is written:
//!
//! - a row that is deleted, or whose key changes, *vacates* its key, unless
//!   another row in the same target set holds it once the plan is applied;
//! - every reference naming a vacated key is resolved by its foreign key's
//!   `onDelete` action (the row went away) or `onUpdate` action (the same row
//!   now answers to a new key);
//! - actions produce further changes -- a cascade deletes a row, which may
//!   vacate keys of its own -- and those are resolved the same way until
//!   nothing more follows. Each row is deleted at most once, so the closure
//!   terminates on every schema, self-references and cycles included.
//!
//! References are found through the mirror's edge index and, for rows the
//! plan itself creates or changes, by reading the planned rows; so a
//! reference is never missed because it was written in the same statement.

use crate::{
    canonical,
    catalog::{Catalog, Row},
    diagnostic::{DbError, Diagnostic, Result},
    integrity, mirror,
    plan::{RowChange, row_path},
    schema::{Action, ColumnType, ForeignKey, Schema},
};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
};

/// A change the engine made on the plan's behalf.
#[derive(Debug, Clone)]
pub struct Induced {
    /// The row changed.
    pub path: PathBuf,
    /// What was done to it.
    pub action: Action,
    /// The foreign key that required it.
    pub constraint: String,
    /// The key whose disappearance or change caused it.
    pub key: String,
    /// The row that held that key.
    pub because: PathBuf,
}

/// A plan with its referential consequences.
#[derive(Debug)]
pub struct Expansion {
    pub rows: Vec<RowChange>,
    pub induced: Vec<Induced>,
}

/// The largest number of vacated keys one mutation may resolve before the
/// engine stops and says so, rather than running unbounded.
const MAX_EVENTS: usize = 10_000_000;

/// The working state: what each touched path will hold.
struct State<'c> {
    catalog: &'c Catalog,
    /// Path → the row there before the plan, if one.
    before: BTreeMap<PathBuf, Row>,
    /// Path → what will be there after (None: deleted).
    after: BTreeMap<PathBuf, Option<(String, Map<String, Value>)>>,
}

impl<'c> State<'c> {
    fn current(&self, path: &PathBuf) -> Result<Option<(String, Map<String, Value>)>> {
        if let Some(state) = self.after.get(path) {
            return Ok(state.clone());
        }
        Ok(self.catalog.row_at(path)?.map(|row| (row.table, row.value)))
    }

    fn original(&mut self, path: &PathBuf) -> Result<Option<Row>> {
        if let Some(row) = self.before.get(path) {
            return Ok(Some(row.clone()));
        }
        let row = self.catalog.row_at(path)?;
        if let Some(row) = &row {
            self.before.insert(path.clone(), row.clone());
        }
        Ok(row)
    }
}

/// A key a row gives up.
struct Vacated {
    table: String,
    columns: Vec<String>,
    key: String,
    /// The same row's key after the change, when it changed rather than went
    /// away.
    replacement: Option<Map<String, Value>>,
    holder: PathBuf,
}

/// Complete a plan with the actions its foreign keys require.
pub fn expand(catalog: &Catalog, rows: Vec<RowChange>) -> Result<Expansion> {
    let mut state = State {
        catalog,
        before: BTreeMap::new(),
        after: BTreeMap::new(),
    };
    // Rows whose keys must be examined, as (path before, path after).
    let mut pending: VecDeque<(Option<PathBuf>, Option<PathBuf>)> = VecDeque::new();
    for change in rows {
        let schema = schema_of(catalog, &change.table)?;
        let before_path = change.before.as_ref().map(|row| row.relative.clone());
        if let Some(before) = &change.before {
            state.before.insert(before.relative.clone(), before.clone());
        }
        let after_path = match &change.after {
            Some(after) => Some(row_path(schema, after)?),
            None => None,
        };
        if let Some(path) = &before_path
            && after_path.as_ref() != Some(path)
        {
            state.after.insert(path.clone(), None);
        }
        if let (Some(path), Some(after)) = (&after_path, change.after) {
            state.after.insert(path.clone(), Some((change.table.clone(), after)));
        }
        pending.push_back((before_path, after_path));
    }

    let mut induced = vec![];
    let mut refusals: Vec<Diagnostic> = vec![];
    let mut events = 0usize;
    while let Some((before_path, after_path)) = pending.pop_front() {
        let Some(before_path) = before_path else { continue };
        let Some(original) = state.original(&before_path)? else { continue };
        let schema = schema_of(catalog, &original.table)?;
        let after_row = match &after_path {
            Some(path) => state.current(path)?.map(|(_, row)| row),
            None => None,
        };
        for columns in schema.candidate_keys() {
            let Some(old_key) = mirror::key(&original.value, columns, schema) else {
                continue;
            };
            let new_key = after_row.as_ref().and_then(|row| mirror::key(row, columns, schema));
            if new_key.as_deref() == Some(old_key.as_str()) {
                continue;
            }
            events += 1;
            if events > MAX_EVENTS {
                return Err(DbError::new(
                    "RESOURCE_LIMIT",
                    format!("referential actions did not settle within {MAX_EVENTS} steps"),
                    2,
                ));
            }
            let vacated = Vacated {
                table: original.table.clone(),
                columns: columns.to_vec(),
                key: old_key,
                replacement: after_row.clone().filter(|_| new_key.is_some()),
                holder: before_path.clone(),
            };
            resolve(&mut state, &vacated, &mut pending, &mut induced, &mut refusals)?;
        }
    }

    if let Some(first) = refusals.first() {
        let mut diagnostic = first.clone();
        if refusals.len() > 1 {
            let listed: Vec<String> = refusals
                .iter()
                .take(20)
                .map(|d| {
                    format!(
                        "{}{}",
                        d.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                        d.pointer.as_deref().unwrap_or("")
                    )
                })
                .collect();
            diagnostic.help = Some(format!(
                "{} reference(s) forbid the change: {}{}",
                refusals.len(),
                listed.join(", "),
                if refusals.len() > 20 { ", ..." } else { "" }
            ));
        }
        return Err(DbError::from_diag(diagnostic, 2));
    }

    let mut out = vec![];
    for (path, after) in state.after {
        let before = state.before.get(&path).cloned();
        match after {
            Some((table, value)) => out.push(RowChange {
                table,
                before,
                after: Some(value),
            }),
            None => {
                if let Some(before) = before {
                    out.push(RowChange::delete(before));
                }
            }
        }
    }
    Ok(Expansion { rows: out, induced })
}

fn schema_of<'c>(catalog: &'c Catalog, table: &str) -> Result<&'c Schema> {
    catalog.schemas.get(table).ok_or_else(|| catalog.unknown_table(table))
}

/// Resolve every reference to a vacated key.
fn resolve(
    state: &mut State<'_>,
    vacated: &Vacated,
    pending: &mut VecDeque<(Option<PathBuf>, Option<PathBuf>)>,
    induced: &mut Vec<Induced>,
    refusals: &mut Vec<Diagnostic>,
) -> Result<()> {
    let catalog = state.catalog;
    for (referencing_table, referencing) in &catalog.schemas {
        for fk in referencing.foreign_keys() {
            let targets = integrity::target_tables(&catalog.schemas, fk.to());
            if !targets.contains(&vacated.table) {
                continue;
            }
            let target_schema = schema_of(catalog, &vacated.table)?;
            if integrity::target_columns(fk, target_schema) != vacated.columns {
                continue;
            }
            if still_held(state, &vacated.key, &vacated.columns, &targets)? {
                continue;
            }
            let rule = mirror::fk_rule(referencing_table, fk.name());
            let mut referrers: BTreeSet<PathBuf> = catalog
                .mirror
                .referrers(&vacated.key, std::slice::from_ref(&rule))?
                .into_iter()
                .map(|edge| PathBuf::from(edge.path))
                .collect();
            // Rows the plan writes are not yet in the edge index.
            for (path, planned) in &state.after {
                if let Some((table, row)) = planned
                    && table == referencing_table
                    && !matching(referencing, fk, row, &vacated.key).is_empty()
                {
                    referrers.insert(path.clone());
                }
            }
            let action = if vacated.replacement.is_some() {
                fk.on_update()
            } else {
                fk.on_delete()
            };
            for path in referrers {
                let Some((table, mut row)) = state.current(&path)? else {
                    continue;
                };
                if &table != referencing_table {
                    continue;
                }
                let pointers = matching(referencing, fk, &row, &vacated.key);
                if pointers.is_empty() {
                    continue;
                }
                let constraint = fk.describe(referencing_table);
                if action.refuses() {
                    for pointer in pointers {
                        let mut diagnostic = Diagnostic::error(
                            "FOREIGN_KEY_VIOLATION",
                            format!(
                                "{} {} is still referenced by {}{pointer} ({} {})",
                                if vacated.replacement.is_some() { "changing the key of" } else { "removing" },
                                vacated.holder.display(),
                                path.display(),
                                if vacated.replacement.is_some() { "onUpdate" } else { "onDelete" },
                                action.name()
                            ),
                        )
                        .at(path.clone())
                        .table(referencing_table)
                        .pointer(pointer)
                        .observed(vacated.key.clone())
                        .help("change or remove the references first, or declare an action that resolves them");
                        diagnostic.constraint = Some(constraint.clone());
                        refusals.push(diagnostic);
                    }
                    continue;
                }
                let mut record = |action: Action| {
                    induced.push(Induced {
                        path: path.clone(),
                        action,
                        constraint: constraint.clone(),
                        key: vacated.key.clone(),
                        because: vacated.holder.clone(),
                    });
                };
                match action {
                    Action::Cascade if vacated.replacement.is_none() => {
                        state.original(&path)?;
                        if state.after.get(&path).is_some_and(Option::is_none) {
                            continue;
                        }
                        state.after.insert(path.clone(), None);
                        record(Action::Cascade);
                        pending.push_back((Some(path.clone()), None));
                        continue;
                    }
                    Action::Cascade => {
                        let replacement = vacated.replacement.as_ref().expect("an update carries its new row");
                        for (pointer, value) in carried(fk, &vacated.columns, replacement, &pointers) {
                            set(&mut row, &pointer, value);
                        }
                    }
                    Action::Remove => remove(referencing, fk, &mut row, &pointers),
                    Action::SetNull => {
                        for pointer in &pointers {
                            set(&mut row, pointer, Value::Null);
                        }
                    }
                    Action::SetDefault => {
                        for (pointer, path) in pointers.iter().zip(fk.from().iter().cycle()) {
                            let default = crate::schema::document::resolve_path(referencing, path)
                                .ok()
                                .and_then(|leaf| leaf.column.default().cloned())
                                .unwrap_or(Value::Null);
                            set(&mut row, pointer, default);
                        }
                    }
                    Action::Restrict | Action::NoAction => unreachable!("refusing actions return above"),
                }
                record(action);
                state.original(&path)?;
                let new_path = row_path(referencing, &row)?;
                if new_path != path {
                    state.after.insert(path.clone(), None);
                }
                state.after.insert(new_path.clone(), Some((table, row)));
                pending.push_back((Some(path.clone()), Some(new_path)));
            }
        }
    }
    Ok(())
}

/// Whether any row of the target tables holds the key once the plan applies.
fn still_held(state: &State<'_>, key: &str, columns: &[String], targets: &[String]) -> Result<bool> {
    let catalog = state.catalog;
    for target in targets {
        let Some(schema) = catalog.schemas.get(target) else { continue };
        if schema.candidate_keys().all(|candidate| candidate != columns) && columns != schema.primary_key() {
            continue;
        }
        let constraint = mirror::constraint_name(schema, columns);
        for (_, path) in catalog.mirror.holders(key, &constraint, std::slice::from_ref(target))? {
            let path = PathBuf::from(path);
            match state.after.get(&path) {
                None => return Ok(true),
                Some(Some((_, row))) if mirror::key(row, columns, schema).as_deref() == Some(key) => {
                    return Ok(true);
                }
                _ => {}
            }
        }
        for planned in state.after.values().flatten() {
            if &planned.0 == target && mirror::key(&planned.1, columns, schema).as_deref() == Some(key) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The pointers in a row whose references under `fk` name `key`.
fn matching(schema: &Schema, fk: &ForeignKey, row: &Map<String, Value>, key: &str) -> Vec<String> {
    if fk.from().len() == 1 {
        let path = &fk.from()[0];
        let kind = crate::schema::document::resolve_path(schema, path)
            .map(|leaf| leaf.column.kind().clone())
            .unwrap_or(ColumnType::Json);
        path.occurrences(row)
            .into_iter()
            .filter(|occurrence| {
                canonical::compact(&Value::Array(vec![mirror::key_component(occurrence.value, &kind)])) == key
            })
            .map(|occurrence| occurrence.pointer)
            .collect()
    } else {
        let mut values = vec![];
        let mut pointers = vec![];
        for path in fk.from() {
            let kind = crate::schema::document::resolve_path(schema, path)
                .map(|leaf| leaf.column.kind().clone())
                .unwrap_or(ColumnType::Json);
            match path.occurrences(row).into_iter().next() {
                Some(occurrence) => {
                    values.push(mirror::key_component(occurrence.value, &kind));
                    pointers.push(occurrence.pointer);
                }
                None => return vec![],
            }
        }
        if canonical::compact(&Value::Array(values)) == key {
            pointers
        } else {
            vec![]
        }
    }
}

/// The new values an update cascade writes at each matched pointer.
fn carried(
    fk: &ForeignKey,
    columns: &[String],
    replacement: &Map<String, Value>,
    pointers: &[String],
) -> Vec<(String, Value)> {
    if fk.from().len() == 1 {
        let value = replacement.get(&columns[0]).cloned().unwrap_or(Value::Null);
        pointers.iter().map(|pointer| (pointer.clone(), value.clone())).collect()
    } else {
        pointers
            .iter()
            .zip(columns)
            .map(|(pointer, column)| (pointer.clone(), replacement.get(column).cloned().unwrap_or(Value::Null)))
            .collect()
    }
}

/// Remove references: the array element holding each, or -- where the path
/// crosses no array -- the value, as null.
fn remove(schema: &Schema, fk: &ForeignKey, row: &mut Map<String, Value>, pointers: &[String]) {
    let mut elements: Vec<Vec<String>> = vec![];
    for pointer in pointers {
        let element = fk.from().iter().find_map(|path| {
            path.occurrences(row)
                .into_iter()
                .find(|occurrence| &occurrence.pointer == pointer)
                .and_then(|occurrence| occurrence.element)
        });
        match element {
            Some(element) => elements.push(crate::schema::path::pointer_tokens(&element)),
            None => set(row, pointer, Value::Null),
        }
    }
    let _ = schema;
    // Remove from the end, so removing one element does not shift the index
    // of another still to be removed.
    elements.sort_by(|left, right| compare_tokens(right, left));
    elements.dedup();
    for tokens in elements {
        remove_element(row, &tokens);
    }
}

fn compare_tokens(left: &[String], right: &[String]) -> std::cmp::Ordering {
    for (a, b) in left.iter().zip(right) {
        let ordering = match (a.parse::<usize>(), b.parse::<usize>()) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            _ => a.cmp(b),
        };
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

fn set(row: &mut Map<String, Value>, pointer: &str, value: Value) {
    let tokens = crate::schema::path::pointer_tokens(pointer);
    let Some((last, parents)) = tokens.split_last() else { return };
    let mut current: Option<&mut Value> = None;
    for (index, token) in parents.iter().enumerate() {
        current = match (index, current) {
            (0, _) => row.get_mut(token),
            (_, Some(Value::Object(object))) => object.get_mut(token),
            (_, Some(Value::Array(items))) => token.parse::<usize>().ok().and_then(|i| items.get_mut(i)),
            _ => None,
        };
        if current.is_none() {
            return;
        }
    }
    match current {
        None if parents.is_empty() => {
            row.insert(last.clone(), value);
        }
        Some(Value::Object(object)) => {
            object.insert(last.clone(), value);
        }
        Some(Value::Array(items)) => {
            if let Some(slot) = last.parse::<usize>().ok().and_then(|i| items.get_mut(i)) {
                *slot = value;
            }
        }
        _ => {}
    }
}

fn remove_element(row: &mut Map<String, Value>, tokens: &[String]) {
    let Some((last, parents)) = tokens.split_last() else { return };
    let Ok(index) = last.parse::<usize>() else { return };
    let mut current: Option<&mut Value> = None;
    for (position, token) in parents.iter().enumerate() {
        current = match (position, current) {
            (0, _) => row.get_mut(token),
            (_, Some(Value::Object(object))) => object.get_mut(token),
            (_, Some(Value::Array(items))) => token.parse::<usize>().ok().and_then(|i| items.get_mut(i)),
            _ => None,
        };
        if current.is_none() {
            return;
        }
    }
    if let Some(Value::Array(items)) = current
        && index < items.len()
    {
        items.remove(index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn test2110_pointers_address_nested_members_and_elements() {
        let mut data = row(json!({"a": {"b": [1, {"c": 2}]}}));
        set(&mut data, "/a/b/1/c", json!(9));
        set(&mut data, "/a/b/0", json!(null));
        assert_eq!(Value::Object(data.clone()), json!({"a": {"b": [null, {"c": 9}]}}));
        remove_element(&mut data, &["a".into(), "b".into(), "0".into()]);
        assert_eq!(Value::Object(data), json!({"a": {"b": [{"c": 9}]}}));
    }

    #[test]
    fn test2111_later_elements_are_removed_first() {
        let mut ordered = vec![
            vec!["r".to_string(), "2".to_string()],
            vec!["r".to_string(), "10".to_string()],
            vec!["r".to_string(), "9".to_string()],
        ];
        ordered.sort_by(|l, r| compare_tokens(r, l));
        assert_eq!(ordered[0][1], "10", "numeric, not lexicographic");
        let mut data = row(json!({"r": [0,1,2,3,4,5,6,7,8,9,10]}));
        for tokens in &ordered {
            remove_element(&mut data, tokens);
        }
        assert_eq!(Value::Object(data), json!({"r": [0,1,3,4,5,6,7,8]}));
    }
}
