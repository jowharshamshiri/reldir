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

use crate::code::Code;
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
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
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

/// Path → what will be there after the plan (None: deleted).
type Planned = BTreeMap<PathBuf, Option<(String, Map<String, Value>)>>;

/// Planned rows by what they hold: (table, constraint) → key → paths, and
/// foreign-key rule → referenced key → paths. Kept in step with
/// [`State::after`], so finding who holds or names a key costs a lookup, not
/// a pass over every planned row.
type KeyIndex = HashMap<(String, String), HashMap<String, BTreeSet<PathBuf>>>;
type ReferenceIndex = HashMap<String, HashMap<String, BTreeSet<PathBuf>>>;

/// The working state: what each touched path will hold.
struct State<'c> {
    catalog: &'c Catalog,
    /// Path → the row there before the plan, if one.
    before: BTreeMap<PathBuf, Row>,
    /// Path → what will be there after (None: deleted). Written only through
    /// [`State::plan`], which keeps the indexes.
    after: Planned,
    holders: KeyIndex,
    references: ReferenceIndex,
}

impl<'c> State<'c> {
    /// Record what a path will hold, keeping the indexes in step.
    fn plan(&mut self, path: PathBuf, value: Option<(String, Map<String, Value>)>) -> Result<()> {
        if let Some(Some((table, row))) = self.after.get(&path) {
            let (table, row) = (table.clone(), row.clone());
            self.index(&path, &table, &row, false)?;
        }
        if let Some((table, row)) = &value {
            self.index(&path, table, row, true)?;
        }
        self.after.insert(path, value);
        Ok(())
    }

    fn index(
        &mut self,
        path: &PathBuf,
        table: &str,
        row: &Map<String, Value>,
        add: bool,
    ) -> Result<()> {
        let schema = schema_of(self.catalog, table)?;
        for columns in schema.candidate_keys() {
            if let Some(key) = mirror::key(row, columns, schema) {
                let slot = self
                    .holders
                    .entry((table.to_string(), mirror::constraint_name(schema, columns)))
                    .or_default()
                    .entry(key)
                    .or_default();
                if add {
                    slot.insert(path.clone());
                } else {
                    slot.remove(path);
                }
            }
        }
        for edge in mirror::edges(row, schema) {
            let slot = self
                .references
                .entry(edge.rule)
                .or_default()
                .entry(edge.target)
                .or_default();
            if add {
                slot.insert(path.clone());
            } else {
                slot.remove(path);
            }
        }
        Ok(())
    }

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

/// A `no_action` reference to a vacated key, judged when the plan is done.
struct Deferred {
    path: PathBuf,
    foreign_key: String,
    vacated: Vacated,
}

/// The refusal of a change by a reference that forbids it.
fn refusal(
    catalog: &Catalog,
    vacated: &Vacated,
    path: &PathBuf,
    referencing_table: &str,
    constraint: &str,
    action: Action,
    pointer: String,
) -> Diagnostic {
    let updating = vacated.replacement.is_some();
    let mut diagnostic = Diagnostic::error(
        Code::FOREIGN_KEY_VIOLATION,
        format!(
            "{} {} refused: {} still references it at {pointer} ({constraint}, {} {})",
            if updating {
                "changing the key of"
            } else {
                "deleting"
            },
            vacated.holder.display(),
            path.display(),
            if updating { "onUpdate" } else { "onDelete" },
            action.name(),
        ),
    )
    .at(path.clone())
    .table(referencing_table)
    .pointer(pointer)
    .observed(vacated.key.clone())
    .help("change or remove the references first, or declare an action that resolves them");
    diagnostic.constraint = Some(constraint.to_string());
    if let Ok(raw) = std::fs::read(catalog.root.join(path)) {
        diagnostic = diagnostic.locate_in(&raw, &crate::locate::Spans::of(&raw));
    }
    diagnostic
}

/// A key a row gives up.
#[derive(Clone)]
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
        holders: HashMap::new(),
        references: HashMap::new(),
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
            state.plan(path.clone(), None)?;
        }
        if let (Some(path), Some(after)) = (&after_path, change.after) {
            state.plan(path.clone(), Some((change.table.clone(), after)))?;
        }
        pending.push_back((before_path, after_path));
    }

    let mut induced = vec![];
    let mut refusals: Vec<Diagnostic> = vec![];
    let mut deferred: Vec<Deferred> = vec![];
    let mut events = 0usize;
    while let Some((before_path, after_path)) = pending.pop_front() {
        let Some(before_path) = before_path else {
            continue;
        };
        let Some(original) = state.original(&before_path)? else {
            continue;
        };
        let schema = schema_of(catalog, &original.table)?;
        let after_row = match &after_path {
            Some(path) => state.current(path)?.map(|(_, row)| row),
            None => None,
        };
        for columns in schema.candidate_keys() {
            let Some(old_key) = mirror::key(&original.value, columns, schema) else {
                continue;
            };
            let new_key = after_row
                .as_ref()
                .and_then(|row| mirror::key(row, columns, schema));
            if new_key.as_deref() == Some(old_key.as_str()) {
                continue;
            }
            events += 1;
            if events > MAX_EVENTS {
                return Err(DbError::new(
                    Code::RESOURCE_LIMIT,
                    format!("referential actions did not settle within {MAX_EVENTS} steps"),
                ));
            }
            // A row that stays answers to a new key -- null included -- so its
            // references follow `onUpdate`; only a row that goes is a delete.
            let vacated = Vacated {
                table: original.table.clone(),
                columns: columns.to_vec(),
                key: old_key,
                replacement: after_row.clone(),
                holder: before_path.clone(),
            };
            resolve(
                &mut state,
                &vacated,
                &mut pending,
                &mut induced,
                &mut refusals,
                &mut deferred,
            )?;
        }
    }

    // `no_action` is judged once every action has run: a reference the rest
    // of the plan removed, or a key another row took over, is no fault.
    for check in deferred {
        let Some((table, row)) = state.current(&check.path)? else {
            continue;
        };
        let schema = schema_of(catalog, &table)?;
        let Some(fk) = schema
            .foreign_keys()
            .iter()
            .find(|fk| fk.name() == check.foreign_key)
        else {
            continue;
        };
        let targets = integrity::target_tables(&catalog.schemas, fk.to());
        if still_held(&state, &check.vacated.key, &check.vacated.columns, &targets)? {
            continue;
        }
        for pointer in matching(schema, fk, &row, &check.vacated.key) {
            refusals.push(refusal(
                catalog,
                &check.vacated,
                &check.path,
                &table,
                &fk.describe(&table),
                Action::NoAction,
                pointer,
            ));
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
                        d.path
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
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
        return Err(DbError::from_diag(diagnostic));
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
    catalog
        .schemas
        .get(table)
        .ok_or_else(|| catalog.unknown_table(table))
}

/// Resolve every reference to a vacated key.
fn resolve(
    state: &mut State<'_>,
    vacated: &Vacated,
    pending: &mut VecDeque<(Option<PathBuf>, Option<PathBuf>)>,
    induced: &mut Vec<Induced>,
    refusals: &mut Vec<Diagnostic>,
    deferred: &mut Vec<Deferred>,
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
            // Rows the plan writes are not yet in the mirror's edge index.
            if let Some(planned) = state
                .references
                .get(&rule)
                .and_then(|targets| targets.get(&vacated.key))
            {
                referrers.extend(planned.iter().cloned());
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
                match action {
                    // Refused now, whatever else the plan does.
                    Action::Restrict => {
                        for pointer in pointers {
                            refusals.push(refusal(
                                catalog,
                                vacated,
                                &path,
                                referencing_table,
                                &constraint,
                                action,
                                pointer,
                            ));
                        }
                        continue;
                    }
                    // Refused only if it still holds once the plan is done.
                    Action::NoAction => {
                        deferred.push(Deferred {
                            path: path.clone(),
                            foreign_key: fk.name().to_string(),
                            vacated: vacated.clone(),
                        });
                        continue;
                    }
                    _ => {}
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
                        state.plan(path.clone(), None)?;
                        record(Action::Cascade);
                        pending.push_back((Some(path.clone()), None));
                        continue;
                    }
                    Action::Cascade => {
                        let replacement = vacated
                            .replacement
                            .as_ref()
                            .expect("an update carries its new row");
                        for (pointer, value) in
                            carried(fk, &vacated.columns, replacement, &pointers)
                        {
                            set(&mut row, &pointer, value)?;
                        }
                    }
                    Action::Remove => remove(fk, &mut row, &pointers)?,
                    Action::SetNull => {
                        for pointer in &pointers {
                            set(&mut row, pointer, Value::Null)?;
                        }
                    }
                    Action::SetDefault => {
                        for (pointer, path) in pointers.iter().zip(fk.from().iter().cycle()) {
                            let default = crate::schema::document::resolve_path(referencing, path)
                                .ok()
                                .and_then(|leaf| leaf.column.default().cloned())
                                .unwrap_or(Value::Null);
                            set(&mut row, pointer, default)?;
                        }
                    }
                    Action::Restrict | Action::NoAction => {
                        unreachable!("refusing actions are handled above")
                    }
                }
                record(action);
                state.original(&path)?;
                let new_path = row_path(referencing, &row)?;
                if new_path != path {
                    state.plan(path.clone(), None)?;
                }
                state.plan(new_path.clone(), Some((table, row)))?;
                pending.push_back((Some(path.clone()), Some(new_path)));
            }
        }
    }
    Ok(())
}

/// Whether any row of the target tables holds the key once the plan applies:
/// a row the plan leaves alone, or one it writes.
fn still_held(
    state: &State<'_>,
    key: &str,
    columns: &[String],
    targets: &[String],
) -> Result<bool> {
    let catalog = state.catalog;
    for target in targets {
        let Some(schema) = catalog.schemas.get(target) else {
            continue;
        };
        if schema
            .candidate_keys()
            .all(|candidate| candidate != columns)
            && columns != schema.primary_key()
        {
            continue;
        }
        let constraint = mirror::constraint_name(schema, columns);
        for (_, path) in catalog
            .mirror
            .holders(key, &constraint, std::slice::from_ref(target))?
        {
            if !state.after.contains_key(&PathBuf::from(path)) {
                return Ok(true);
            }
        }
        if state
            .holders
            .get(&(target.clone(), constraint))
            .and_then(|keys| keys.get(key))
            .is_some_and(|paths| !paths.is_empty())
        {
            return Ok(true);
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
                canonical::compact(&Value::Array(vec![crate::value::key_component(
                    occurrence.value,
                    &kind,
                )])) == key
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
                    values.push(crate::value::key_component(occurrence.value, &kind));
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
        pointers
            .iter()
            .map(|pointer| (pointer.clone(), value.clone()))
            .collect()
    } else {
        pointers
            .iter()
            .zip(columns)
            .map(|(pointer, column)| {
                (
                    pointer.clone(),
                    replacement.get(column).cloned().unwrap_or(Value::Null),
                )
            })
            .collect()
    }
}

/// Remove references: the array element holding each, or -- where the path
/// crosses no array -- the member itself.
fn remove(fk: &ForeignKey, row: &mut Map<String, Value>, pointers: &[String]) -> Result<()> {
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
            None => elements.push(crate::schema::path::pointer_tokens(pointer)),
        }
    }
    // Remove from the end, so removing one element does not shift the index
    // of another still to be removed.
    elements.sort_by(|left, right| compare_tokens(right, left));
    elements.dedup();
    for tokens in elements {
        remove_at(row, &tokens)?;
    }
    Ok(())
}

pub(crate) fn compare_tokens(left: &[String], right: &[String]) -> std::cmp::Ordering {
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

/// The value a pointer's parent tokens reach in a row, for an action to edit:
/// `None` stands for the row itself.
fn parent_mut<'r>(
    row: &'r mut Map<String, Value>,
    parents: &[String],
) -> std::result::Result<Option<&'r mut Value>, ()> {
    let Some((first, rest)) = parents.split_first() else {
        return Ok(None);
    };
    let mut current = row.get_mut(first).ok_or(())?;
    for token in rest {
        current = match current {
            Value::Object(object) => object.get_mut(token),
            Value::Array(items) => token.parse::<usize>().ok().and_then(|i| items.get_mut(i)),
            _ => None,
        }
        .ok_or(())?;
    }
    Ok(Some(current))
}

/// A pointer an action was given that does not lead into the row it was read
/// from: a defect, never a row to write.
fn unreachable_pointer(pointer: &[String]) -> DbError {
    DbError::new(
        Code::INTERNAL_ERROR,
        format!(
            "a referential action was given /{}, which is not in the row it was read from",
            pointer.join("/")
        ),
    )
}

pub(crate) fn set(row: &mut Map<String, Value>, pointer: &str, value: Value) -> Result<()> {
    let tokens = crate::schema::path::pointer_tokens(pointer);
    let Some((last, parents)) = tokens.split_last() else {
        return Err(unreachable_pointer(&tokens));
    };
    match parent_mut(row, parents).map_err(|()| unreachable_pointer(&tokens))? {
        None => {
            row.insert(last.clone(), value);
        }
        Some(Value::Object(object)) => {
            object.insert(last.clone(), value);
        }
        Some(Value::Array(items)) => {
            let slot = last
                .parse::<usize>()
                .ok()
                .and_then(|index| items.get_mut(index))
                .ok_or_else(|| unreachable_pointer(&tokens))?;
            *slot = value;
        }
        Some(_) => return Err(unreachable_pointer(&tokens)),
    }
    Ok(())
}

/// Remove what a pointer names: an array element, or an object member --
/// keeping the order of the members that stay.
pub(crate) fn remove_at(row: &mut Map<String, Value>, tokens: &[String]) -> Result<()> {
    let Some((last, parents)) = tokens.split_last() else {
        return Err(unreachable_pointer(tokens));
    };
    let removed = match parent_mut(row, parents).map_err(|()| unreachable_pointer(tokens))? {
        None => row.shift_remove(last).is_some(),
        Some(Value::Object(object)) => object.shift_remove(last).is_some(),
        Some(Value::Array(items)) => match last.parse::<usize>() {
            Ok(index) if index < items.len() => {
                items.remove(index);
                true
            }
            _ => false,
        },
        Some(_) => false,
    };
    if removed {
        Ok(())
    } else {
        Err(unreachable_pointer(tokens))
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
        let mut data = row(json!({"a": {"b": [1, {"c": 2, "d": 3, "e": 4}]}, "z": 0}));
        set(&mut data, "/a/b/1/c", json!(9)).unwrap();
        set(&mut data, "/a/b/0", json!(null)).unwrap();
        assert_eq!(
            Value::Object(data.clone()),
            json!({"a": {"b": [null, {"c": 9, "d": 3, "e": 4}]}, "z": 0})
        );
        remove_at(&mut data, &["a".into(), "b".into(), "0".into()]).unwrap();
        remove_at(&mut data, &["a".into(), "b".into(), "0".into(), "d".into()]).unwrap();
        assert_eq!(
            serde_json::to_string(&data).unwrap(),
            r#"{"a":{"b":[{"c":9,"e":4}]},"z":0}"#,
            "a removed member leaves the others in their order"
        );
        remove_at(&mut data, &["z".into()]).unwrap();
        assert!(!data.contains_key("z"));
        // A pointer that is not in the row is a defect to report, never a
        // change to skip.
        assert!(set(&mut data, "/a/missing/0", json!(1)).is_err());
        assert!(remove_at(&mut data, &["a".into(), "b".into(), "7".into()]).is_err());
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
            remove_at(&mut data, tokens).unwrap();
        }
        assert_eq!(Value::Object(data), json!({"r": [0,1,3,4,5,6,7,8]}));
    }
}
