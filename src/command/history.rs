//! Recorded history, snapshots, recovery and upkeep.

use super::{Context, Intent};
use crate::{
    db::{Admission, Request},
    diagnostic::{DbError, Result},
    metadata::{self, Entry},
    output::{Finish, Sink},
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub fn log(context: &Context, sink: &mut dyn Sink, limit: Option<usize>) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    let revisions = metadata::revisions(&database.root)?;
    let mut shown = 0;
    for revision in revisions.iter().rev().take(limit.unwrap_or(usize::MAX)) {
        let record = metadata::load_record(&database.root, *revision)?;
        let mut out = Map::new();
        out.insert("kind".into(), json!("revision"));
        out.insert("revision".into(), json!(record.revision));
        out.insert("timestamp".into(), json!(record.timestamp));
        out.insert("origin".into(), json!(record.origin));
        out.insert("changes".into(), json!(record.changes.len()));
        out.insert("root".into(), json!(&record.new_root_hash[..12]));
        if let Some(lineage) = &record.lineage {
            out.insert("lineage".into(), json!(lineage));
        }
        sink.record(out)?;
        shown += 1;
    }
    Ok(Finish::ok(format!("{shown} of {} revision(s)", revisions.len())))
}

pub fn show(context: &Context, sink: &mut dyn Sink, revision: u64) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    let before = if revision > 1 { metadata::entries_at(&database.root, revision - 1)? } else { BTreeMap::new() };
    metadata::entries_at(&database.root, revision)?;
    let record = metadata::load_record(&database.root, revision)?;
    for line in record.summary(&before) {
        let (action, path) = line.split_once(' ').unwrap_or(("?", line.as_str()));
        let mut out = Map::new();
        out.insert("kind".into(), json!("revision_change"));
        out.insert("action".into(), json!(match action {
            "A" => "added",
            "D" => "removed",
            _ => "modified",
        }));
        out.insert("path".into(), json!(path));
        sink.record(out)?;
    }
    let mut finish = Finish::ok(format!(
        "revision {} ({}, {}): {} change(s)",
        record.revision,
        record.origin,
        record.timestamp,
        record.changes.len()
    ))
    .with("revision", record.revision)
    .with("origin", record.origin.clone())
    .with("timestamp", record.timestamp.clone())
    .with("root", record.new_root_hash.clone());
    if let Some(id) = &record.transaction_id {
        finish = finish.with("transaction", id.clone());
    }
    Ok(finish)
}

/// How two JSON documents differ, member by member. Arrays are compared
/// whole: an element's position is not an identity.
pub fn document_diff(pointer: &str, old: &Value, new: &Value) -> Vec<Map<String, Value>> {
    let mut out = vec![];
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for key in keys {
                let at = format!("{pointer}/{}", crate::schema::path::escape_pointer(key));
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => out.extend(document_diff(&at, x, y)),
                    (x, y) => out.push(change(&at, x.cloned(), y.cloned())),
                }
            }
        }
        (x, y) if x != y => out.push(change(pointer, Some(x.clone()), Some(y.clone()))),
        _ => {}
    }
    out
}

fn change(pointer: &str, old: Option<Value>, new: Option<Value>) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("kind".into(), json!(match (&old, &new) {
        (None, _) => "added",
        (_, None) => "removed",
        _ => "changed",
    }));
    out.insert("pointer".into(), json!(pointer));
    out.insert("old".into(), old.unwrap_or(Value::Null));
    out.insert("new".into(), new.unwrap_or(Value::Null));
    out
}

pub struct DiffOptions {
    /// Two revisions to compare; none compares the recorded head with the files.
    pub revisions: Option<(u64, u64)>,
    pub table: Option<String>,
    pub schema_only: bool,
}

/// What changed, in terms of rows and schemas.
pub fn diff(context: &Context, sink: &mut dyn Sink, options: DiffOptions) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    let root = database.root.clone();
    let object = |entry: &Entry| metadata::load_object(&root, &entry.hash);
    let (old, new, live) = match options.revisions {
        Some((a, b)) => (metadata::entries_at(&root, a)?, metadata::entries_at(&root, b)?, false),
        None => {
            let head = match database.history.head() {
                Some(head) => metadata::entries_at(&root, head.revision)?,
                None => BTreeMap::new(),
            };
            (head, metadata::live_entries(&database.catalog)?, true)
        }
    };
    let current = |path: &str, entry: &Entry| -> Result<Value> {
        if !live {
            return object(entry);
        }
        if let Some(table) = path.strip_prefix("schema/").and_then(|p| p.strip_suffix(".json")) {
            return Ok(database.catalog.schemas[table].document().clone());
        }
        match database.catalog.row_at(std::path::Path::new(path))? {
            Some(row) => Ok(crate::canonical::canonical_row(&row.value, &database.catalog.schemas[&row.table])),
            None => object(entry),
        }
    };
    let wanted = |path: &str| {
        let is_schema = path.starts_with("schema/");
        if options.schema_only && !is_schema {
            return false;
        }
        match &options.table {
            Some(table) => path.starts_with(&format!("{table}/")) || path == format!("schema/{table}.json"),
            None => !path.starts_with(".db/"),
        }
    };
    // A file whose content moved to another path is a rename, not a removal
    // and an addition.
    let added_by_hash: BTreeMap<&str, &String> = new
        .iter()
        .filter(|(path, _)| !old.contains_key(*path))
        .map(|(path, entry)| (entry.hash.as_str(), path))
        .collect();
    let mut renamed_to = std::collections::BTreeSet::new();
    let mut count = 0;
    for (path, entry) in &old {
        if new.contains_key(path) || !wanted(path) {
            continue;
        }
        let mut out = Map::new();
        if let Some(to) = added_by_hash.get(entry.hash.as_str()) {
            renamed_to.insert((*to).clone());
            out.insert("kind".into(), json!("renamed"));
            out.insert("from".into(), json!(path));
            out.insert("to".into(), json!(to));
        } else {
            out.insert("kind".into(), json!("removed"));
            out.insert("path".into(), json!(path));
        }
        sink.record(out)?;
        count += 1;
    }
    for (path, entry) in &new {
        if !wanted(path) {
            continue;
        }
        match old.get(path) {
            None if renamed_to.contains(path) => {}
            None => {
                let mut out = Map::new();
                out.insert("kind".into(), json!("added"));
                out.insert("path".into(), json!(path));
                sink.record(out)?;
                count += 1;
            }
            Some(before) if before.hash != entry.hash => {
                for mut change in document_diff("", &object(before)?, &current(path, entry)?) {
                    change.insert("path".into(), json!(path));
                    sink.record(change)?;
                    count += 1;
                }
            }
            Some(_) => {}
        }
    }
    Ok(Finish::ok(format!("{count} change(s)")))
}

pub enum SnapshotAction {
    Create(String),
    List,
    Restore(String),
    Delete(String),
}

pub fn snapshot(context: &Context, sink: &mut dyn Sink, action: SnapshotAction) -> Result<Finish> {
    match action {
        SnapshotAction::List => {
            let root = context.root()?;
            let names = crate::snapshot::list(&root)?;
            for name in &names {
                let mut out = Map::new();
                out.insert("kind".into(), json!("snapshot"));
                out.insert("name".into(), json!(name));
                sink.record(out)?;
            }
            Ok(Finish::ok(format!("{} snapshot(s)", names.len())))
        }
        SnapshotAction::Create(name) => {
            let database = context.open_database(sink, Intent::Write)?;
            crate::snapshot::validate_name(&name)?;
            if context.dry_run {
                return Ok(Finish::ok(format!("would take snapshot {name}")));
            }
            crate::snapshot::create(&database, &name)?;
            Ok(Finish::ok(format!("took snapshot {name}")).with("name", name))
        }
        SnapshotAction::Restore(name) => {
            let mut database = context.open_database(sink, Intent::Write)?;
            let (changes, expected) = crate::snapshot::restore(&database, &name)?;
            super::confirm(context, &format!("restore snapshot {name}, replacing {} file(s)", changes.len()))?;
            let outcome = database.commit(
                changes,
                &expected,
                Request { origin: "snapshot_restore", admission: Admission::Valid, dry_run: context.dry_run },
            )?;
            super::committed(sink, &outcome, "restored")
        }
        SnapshotAction::Delete(name) => {
            let root = context.root()?;
            super::confirm(context, &format!("delete snapshot {name}"))?;
            if context.dry_run {
                return Ok(Finish::ok(format!("would delete snapshot {name}")));
            }
            crate::snapshot::delete(&root, &name)?;
            Ok(Finish::ok(format!("deleted snapshot {name}")))
        }
    }
}

/// Finish interrupted transactions; with `new_lineage`, also move unverifiable
/// history aside and begin a new one.
pub fn recover(context: &Context, sink: &mut dyn Sink, new_lineage: bool) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    if !new_lineage {
        let recovered = database.events.iter().any(|event| matches!(event, crate::db::Event::Recovered { .. }));
        return Ok(Finish::ok(if recovered { "recovered" } else { "nothing to recover" }).with("recovered", recovered));
    }
    if !context.allow_destructive {
        return Err(DbError::new(
            "DECISION_REQUIRED",
            "a new lineage moves every recorded revision aside; the history stays on disk but no longer describes the database",
            9,
        )
        .with_help("pass --allow-destructive to begin a new lineage"));
    }
    if context.dry_run {
        return Ok(Finish::ok("would move history to .db/provenance-quarantine/ and begin a new lineage"));
    }
    let reason = match &database.history {
        crate::db::History::Degraded(error) => error.diagnostic.message.clone(),
        _ => "requested".to_string(),
    };
    let record = database.begin_new_lineage(&reason)?;
    let quarantined = record.lineage.as_ref().map(|lineage| lineage.quarantined.clone()).unwrap_or_default();
    sink.event(
        "new_lineage",
        json!({"revision": record.revision, "quarantined": quarantined}),
        &format!("history moved to {quarantined}"),
    )?;
    Ok(Finish::ok(format!("began a new lineage at revision {}", record.revision)).with("revision", record.revision))
}

pub fn gc(context: &Context, sink: &mut dyn Sink) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    let removed = database.collect_garbage(context.dry_run)?;
    for path in &removed {
        let mut out = Map::new();
        out.insert("kind".into(), json!("garbage"));
        out.insert("path".into(), json!(path));
        out.insert("planned".into(), json!(context.dry_run));
        sink.record(out)?;
    }
    Ok(Finish::ok(format!(
        "{} {} unreferenced file(s)",
        if context.dry_run { "would remove" } else { "removed" },
        removed.len()
    )))
}

/// Refresh the query planner's statistics.
pub fn analyze(context: &Context, sink: &mut dyn Sink) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    database.catalog.mirror.analyze()?;
    Ok(Finish::ok("refreshed query statistics"))
}
