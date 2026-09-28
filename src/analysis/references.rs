//! Finding references the schemas do not declare.
//!
//! A value is a reference when it names a row: every value found at some place
//! in a table's rows is the key of a row somewhere. The place can be anywhere
//! -- a top-level column, each element of an array, a member of an object
//! nested in an array of objects -- so candidates are every *scalar leaf path*
//! the rows contain, spelled in the reference-path grammar
//! (`modules[].lessons[].lesson_ref`).
//!
//! A candidate becomes a proposal when all of its values resolve:
//!
//! - into one table's primary key: a reference to that table;
//! - across several tables whose keys are of one type and never collide: a
//!   reference into an **identity domain** over those tables -- the shape of a
//!   labelled edge list whose targets are rows of any kind.
//!
//! A proposal is *conventional* when the leaf's name is one the configured
//! `reference_naming` patterns produce for its target (`subject_id`,
//! `subject_refs`, ...). Inference declares conventional proposals on its own;
//! lint reports every proposal, and doctor applies one when asked.

use crate::{
    canonical,
    config::Config,
    mirror,
    schema::{ColumnType, RefPath, Schema, path::Step},
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The most distinct leaf paths recorded per table. Objects used as maps with
/// data-dependent member names would otherwise produce one path per member.
const MAX_LEAVES: usize = 4096;

/// What analysis needs to know about one table.
#[derive(Debug, Clone)]
pub struct TableFacts {
    pub table: String,
    /// The single-column primary key and its type. Tables keyed on several
    /// columns cannot be the target of a one-path reference.
    pub key: Option<(String, ColumnType)>,
    pub domain: Option<String>,
    /// Every primary key the table holds, rendered as keys are.
    pub keys: BTreeSet<String>,
    /// Scalar leaves, by path text.
    pub leaves: BTreeMap<String, Leaf>,
    /// Leaf paths already declared as references, by path text.
    pub declared: BTreeSet<String>,
    /// Whether leaves beyond [`MAX_LEAVES`] were not recorded.
    pub truncated: bool,
}

/// Every value found at one leaf path.
#[derive(Debug, Clone)]
pub struct Leaf {
    pub path: RefPath,
    /// Distinct string and integer values, rendered as keys are.
    pub values: BTreeSet<String>,
    /// Occurrences that were not a string or an integer -- a float, a bool, an
    /// object or an array where other rows hold a scalar. Such a place is not
    /// a reference.
    pub other: usize,
}

impl TableFacts {
    /// Gather facts from a table's rows. `schema` supplies the key and the
    /// declared references; `None` when the table's schema is still being
    /// inferred, in which case `key` names its chosen primary key.
    pub fn gather<'r>(
        table: &str,
        key: Option<(String, ColumnType)>,
        schema: Option<&Schema>,
        rows: impl IntoIterator<Item = &'r Map<String, Value>>,
    ) -> Self {
        let mut facts = Self {
            table: table.to_string(),
            key: key.clone(),
            domain: schema.and_then(Schema::identity_domain).map(String::from),
            keys: BTreeSet::new(),
            leaves: BTreeMap::new(),
            declared: schema
                .map(|schema| {
                    schema
                        .foreign_keys()
                        .iter()
                        .filter(|fk| fk.from().len() == 1)
                        .map(|fk| fk.from()[0].to_string())
                        .collect()
                })
                .unwrap_or_default(),
            truncated: false,
        };
        for row in rows {
            if let Some((column, kind)) = &key
                && let Some(value) = row.get(column)
                && !value.is_null()
            {
                facts
                    .keys
                    .insert(render(&mirror::key_component(value, kind)));
            }
            for (name, value) in row {
                let steps = vec![Step::Member(name.clone())];
                facts.walk(steps, value);
            }
        }
        facts
    }

    fn walk(&mut self, steps: Vec<Step>, value: &Value) {
        match value {
            Value::Null => {}
            Value::Object(object) => {
                for (name, child) in object {
                    let mut next = steps.clone();
                    next.push(Step::Member(name.clone()));
                    self.walk(next, child);
                }
                self.note_other(&steps);
            }
            Value::Array(items) => {
                let mut next = steps.clone();
                next.push(Step::Each);
                for item in items {
                    self.walk(next.clone(), item);
                }
                self.note_other(&steps);
            }
            Value::String(_) => self.note_value(steps, value),
            Value::Number(number) if number.as_i64().is_some() && !number.is_f64() => {
                self.note_value(steps, value)
            }
            Value::Number(_) | Value::Bool(_) => self.note_other(&steps),
        }
    }

    fn leaf(&mut self, steps: &[Step]) -> Option<&mut Leaf> {
        let path = RefPath::from_steps(steps.to_vec())?;
        let text = path.to_string();
        if !self.leaves.contains_key(&text) {
            if self.leaves.len() >= MAX_LEAVES {
                self.truncated = true;
                return None;
            }
            self.leaves.insert(
                text.clone(),
                Leaf {
                    path,
                    values: BTreeSet::new(),
                    other: 0,
                },
            );
        }
        self.leaves.get_mut(&text)
    }

    fn note_value(&mut self, steps: Vec<Step>, value: &Value) {
        if let Some(leaf) = self.leaf(&steps) {
            leaf.values.insert(render(value));
        }
    }

    /// Mark a place that held something other than a key-shaped scalar. Only
    /// places already known as leaves are marked: a path that is always an
    /// object is simply not a leaf.
    fn note_other(&mut self, steps: &[Step]) {
        let Some(path) = RefPath::from_steps(steps.to_vec()) else {
            return;
        };
        match self.leaves.get_mut(&path.to_string()) {
            Some(leaf) => leaf.other += 1,
            None => {
                if matches!(steps.last(), Some(Step::Member(_) | Step::Each))
                    && self.leaves.len() < MAX_LEAVES
                {
                    self.leaves.insert(
                        path.to_string(),
                        Leaf {
                            path,
                            values: BTreeSet::new(),
                            other: 1,
                        },
                    );
                }
            }
        }
    }
}

/// A key rendered as the mirror renders a one-column key.
fn render(value: &Value) -> String {
    canonical::compact(&Value::Array(vec![value.clone()]))
}

/// Where a proposed reference points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposedTarget {
    Table(String),
    /// An identity domain. `join` lists tables that must be declared members
    /// for the reference to resolve; empty when the domain already exists as
    /// needed. `new` says the domain does not exist yet.
    Domain {
        name: String,
        join: Vec<String>,
        new: bool,
    },
}

/// A reference the data supports and no schema declares.
#[derive(Debug, Clone)]
pub struct Proposal {
    pub table: String,
    pub path: RefPath,
    pub target: ProposedTarget,
    /// Whether the leaf is named as `reference_naming` names a reference to
    /// the target.
    pub conventional: bool,
    /// How many distinct values resolve.
    pub distinct: usize,
}

impl Proposal {
    /// The `foreignKeys` entry that declares it. Deleting a referenced row is
    /// refused and changing its key is refused, until someone decides
    /// otherwise: a proposal never chooses to lose data.
    pub fn definition(&self) -> Value {
        let to = match &self.target {
            ProposedTarget::Table(table) => json!({ "table": table }),
            ProposedTarget::Domain { name, .. } => json!({ "domain": name }),
        };
        json!({
            "from": [self.path.to_string()],
            "to": to,
            "onDelete": "restrict",
            "onUpdate": "restrict"
        })
    }

    pub fn describe(&self) -> String {
        match &self.target {
            ProposedTarget::Table(table) => format!(
                "{}.{} holds {} distinct value(s), every one a key of {table}",
                self.table, self.path, self.distinct
            ),
            ProposedTarget::Domain { name, join, new } => {
                let mut text = format!(
                    "{}.{} holds {} distinct value(s), each a key of exactly one table of identity domain {name:?}",
                    self.table, self.path, self.distinct
                );
                if *new {
                    text.push_str(&format!(" (a new domain over {})", join.join(", ")));
                } else if !join.is_empty() {
                    text.push_str(&format!(" once {} join it", join.join(", ")));
                }
                text
            }
        }
    }
}

/// Every reference the data supports and no schema declares, in table and
/// path order.
pub fn detect(facts: &BTreeMap<String, TableFacts>, config: &Config) -> Vec<Proposal> {
    // Key → the tables holding it.
    let mut holders: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for facts in facts.values().filter(|facts| facts.key.is_some()) {
        for key in &facts.keys {
            holders
                .entry(key.as_str())
                .or_default()
                .push(facts.table.as_str());
        }
    }
    let mut new_domains: Vec<(BTreeSet<String>, String)> = vec![];
    let mut out = vec![];
    for table in facts.values() {
        for (text, leaf) in &table.leaves {
            if leaf.other > 0
                || leaf.values.len() < config.reference_min_values
                || table.declared.contains(text)
                || table.key.as_ref().is_some_and(|(column, _)| text == column)
            {
                continue;
            }
            let Some(target) = resolve(facts, &holders, leaf, &mut new_domains) else {
                continue;
            };
            let leaf_name = leaf
                .path
                .steps()
                .iter()
                .rev()
                .find_map(|step| match step {
                    Step::Member(name) => Some(name.as_str()),
                    _ => None,
                })
                .unwrap_or_default();
            let named_for = |table: &str| {
                config
                    .reference_names(table)
                    .iter()
                    .any(|name| name == leaf_name)
            };
            let conventional = match &target {
                ProposedTarget::Table(target) => named_for(target),
                ProposedTarget::Domain { name, join, .. } => facts
                    .values()
                    .filter(|facts| {
                        facts.domain.as_deref() == Some(name.as_str())
                            || join.contains(&facts.table)
                    })
                    .any(|facts| named_for(&facts.table)),
            };
            out.push(Proposal {
                table: table.table.clone(),
                path: leaf.path.clone(),
                target,
                conventional,
                distinct: leaf.values.len(),
            });
        }
    }
    out
}

fn resolve(
    facts: &BTreeMap<String, TableFacts>,
    holders: &BTreeMap<&str, Vec<&str>>,
    leaf: &Leaf,
    new_domains: &mut Vec<(BTreeSet<String>, String)>,
) -> Option<ProposedTarget> {
    let mut per_value: Vec<&Vec<&str>> = Vec::with_capacity(leaf.values.len());
    for value in &leaf.values {
        per_value.push(holders.get(value.as_str())?);
    }
    // Tables that hold every value.
    let mut whole: BTreeSet<&str> = per_value[0].iter().copied().collect();
    for tables in &per_value[1..] {
        let these: BTreeSet<&str> = tables.iter().copied().collect();
        whole = whole.intersection(&these).copied().collect();
    }
    match whole.len() {
        1 => return Some(ProposedTarget::Table(whole.into_iter().next()?.to_string())),
        // Several tables hold every value: which one is meant cannot be told
        // from the data.
        n if n > 1 => return None,
        _ => {}
    }
    // Each value in exactly one table, all of one key type.
    if per_value.iter().any(|tables| tables.len() != 1) {
        return None;
    }
    let cover: BTreeSet<&str> = per_value.iter().map(|tables| tables[0]).collect();
    let kinds: BTreeSet<&str> = cover
        .iter()
        .filter_map(|table| facts.get(*table)?.key.as_ref().map(|(_, kind)| kind.name()))
        .collect();
    if kinds.len() != 1 {
        return None;
    }
    let domains: BTreeSet<&str> = cover
        .iter()
        .filter_map(|table| facts.get(*table)?.domain.as_deref())
        .collect();
    let (name, new) = match domains.len() {
        0 => {
            let set: BTreeSet<String> = cover.iter().map(|t| t.to_string()).collect();
            match new_domains.iter().find(|(tables, _)| tables == &set) {
                Some((_, name)) => (name.clone(), true),
                None => {
                    let name = if new_domains.is_empty() {
                        "shared_keys".to_string()
                    } else {
                        format!("shared_keys_{}", new_domains.len() + 1)
                    };
                    new_domains.push((set, name.clone()));
                    (name, true)
                }
            }
        }
        1 => (domains.into_iter().next()?.to_string(), false),
        _ => return None,
    };
    let join: Vec<String> = cover
        .iter()
        .filter(|table| {
            facts
                .get(**table)
                .is_some_and(|facts| facts.domain.as_deref() != Some(name.as_str()))
        })
        .map(|table| table.to_string())
        .collect();
    // The domain, once joined, must still be one namespace: no key held twice.
    let members: Vec<&TableFacts> = facts
        .values()
        .filter(|facts| {
            facts.domain.as_deref() == Some(name.as_str()) || join.contains(&facts.table)
        })
        .collect();
    let mut seen = BTreeSet::new();
    for member in &members {
        if member.key.as_ref().map(|(_, kind)| kind.name()) != kinds.iter().next().copied() {
            return None;
        }
        for key in &member.keys {
            if !seen.insert(key.as_str()) {
                return None;
            }
        }
    }
    Some(ProposedTarget::Domain { name, join, new })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(values: &[Value]) -> Vec<Map<String, Value>> {
        values
            .iter()
            .map(|value| value.as_object().unwrap().clone())
            .collect()
    }

    fn facts(table: &str, key: &str, data: &[Value]) -> TableFacts {
        let rows = rows(data);
        TableFacts::gather(
            table,
            Some((key.to_string(), ColumnType::String)),
            None,
            &rows,
        )
    }

    fn all(list: Vec<TableFacts>) -> BTreeMap<String, TableFacts> {
        list.into_iter()
            .map(|facts| (facts.table.clone(), facts))
            .collect()
    }

    #[test]
    fn test2170_references_are_found_at_any_depth_and_named_by_path() {
        let catalog = all(vec![
            facts("lessons", "id", &[json!({"id": "l1"}), json!({"id": "l2"})]),
            facts(
                "courses",
                "id",
                &[
                    json!({"id": "c1", "modules": [{"lessons": [{"lesson_ref": "l1"}, {"lesson_ref": "l2"}]}]}),
                ],
            ),
        ]);
        let found = detect(&catalog, &Config::default());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path.to_string(), "modules[].lessons[].lesson_ref");
        assert_eq!(found[0].target, ProposedTarget::Table("lessons".into()));
        assert!(
            found[0].conventional,
            "lesson_ref is how a reference to lessons is named"
        );
        assert_eq!(
            found[0].definition()["from"][0],
            "modules[].lessons[].lesson_ref"
        );
    }

    #[test]
    fn test2171_targets_spread_across_disjoint_tables_are_a_domain() {
        let catalog = all(vec![
            facts("objectives", "id", &[json!({"id": "objective.a"})]),
            facts("knowledge", "id", &[json!({"id": "knowledge.b"})]),
            facts(
                "items",
                "id",
                &[
                    json!({"id": "item.c", "relations": [{"target": "objective.a"}, {"target": "knowledge.b"}]}),
                ],
            ),
        ]);
        let found = detect(&catalog, &Config::default());
        let proposal = found
            .iter()
            .find(|p| p.path.to_string() == "relations[].target")
            .expect("found");
        match &proposal.target {
            ProposedTarget::Domain { join, new, .. } => {
                assert!(*new);
                assert_eq!(
                    join,
                    &vec!["knowledge".to_string(), "objectives".to_string()]
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(
            !proposal.conventional,
            "`target` names no table, so inference would not declare it"
        );
    }

    #[test]
    fn test2172_a_value_that_resolves_nowhere_is_not_a_reference() {
        let catalog = all(vec![
            facts("users", "id", &[json!({"id": "u1"})]),
            facts(
                "posts",
                "id",
                &[
                    json!({"id": "p1", "user_id": "u1"}),
                    json!({"id": "p2", "user_id": "ghost"}),
                ],
            ),
        ]);
        assert!(detect(&catalog, &Config::default()).is_empty());
    }

    #[test]
    fn test2173_mixed_places_and_ambiguous_targets_are_not_proposed() {
        let catalog = all(vec![
            facts("users", "id", &[json!({"id": "u1"})]),
            facts("admins", "id", &[json!({"id": "u1"})]),
            facts("posts", "id", &[json!({"id": "p1", "owner": "u1"})]),
            facts(
                "notes",
                "id",
                &[
                    json!({"id": "n1", "user_id": "u1"}),
                    json!({"id": "n2", "user_id": {"x": 1}}),
                ],
            ),
        ]);
        let found = detect(&catalog, &Config::default());
        assert!(
            found.iter().all(|p| p.table != "posts"),
            "two tables hold u1: {found:?}"
        );
        assert!(
            found.iter().all(|p| p.table != "notes"),
            "user_id is sometimes an object: {found:?}"
        );
    }

    #[test]
    fn test2174_declared_references_and_the_rows_own_key_are_not_proposed() {
        let catalog = all(vec![facts(
            "users",
            "id",
            &[json!({"id": "u1", "manager_id": "u1"})],
        )]);
        let found = detect(&catalog, &Config::default());
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].path.to_string(),
            "manager_id",
            "a self-reference, not the key itself"
        );
        let mut declared = all(vec![facts(
            "users",
            "id",
            &[json!({"id": "u1", "manager_id": "u1"})],
        )]);
        declared
            .get_mut("users")
            .unwrap()
            .declared
            .insert("manager_id".into());
        assert!(detect(&declared, &Config::default()).is_empty());
    }

    #[test]
    fn test2175_a_domain_whose_keys_would_collide_is_not_proposed() {
        let catalog = all(vec![
            facts("a", "id", &[json!({"id": "x"}), json!({"id": "shared"})]),
            facts("b", "id", &[json!({"id": "y"}), json!({"id": "shared"})]),
            facts(
                "c",
                "id",
                &[
                    json!({"id": "c1", "target": "x"}),
                    json!({"id": "c2", "target": "y"}),
                ],
            ),
        ]);
        assert!(
            detect(&catalog, &Config::default()).is_empty(),
            "a and b both hold `shared`"
        );
    }
}
