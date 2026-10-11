//! Recorded history, snapshots, recovery and upkeep.

use super::{Context, Intent};
use crate::output::{Finish, Sink};
use reldir_core::code::Code;
use reldir_core::{
    db::{Database, RevisionName},
    diagnostic::{DbError, Result},
    history,
    store::{Id, revision::Origin},
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn name_of(numbers: &BTreeMap<Id, u64>, id: &Id) -> RevisionName {
    RevisionName {
        seq: numbers.get(id).copied().unwrap_or_default(),
        id: *id,
    }
}

pub fn log(context: &Context, sink: &mut dyn Sink, limit: Option<usize>) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    let history = database.history()?;
    let numbers = database.revision_numbers()?;
    let revisions = history.log()?;
    let total = revisions.len();
    let mut shown = 0;
    for (id, revision) in revisions.into_iter().take(limit.unwrap_or(usize::MAX)) {
        let mut out = Map::new();
        out.insert("kind".into(), json!("revision"));
        out.insert("revision".into(), name_of(&numbers, &id).to_json());
        out.insert(
            "parents".into(),
            json!(
                revision
                    .parents
                    .iter()
                    .map(|parent| name_of(&numbers, parent).to_json())
                    .collect::<Vec<_>>()
            ),
        );
        out.insert("timestamp".into(), json!(revision.timestamp.to_rfc3339()));
        out.insert("origin".into(), json!(revision.origin.as_str()));
        out.insert(
            "rows".into(),
            json!({"added": revision.stats.added, "modified": revision.stats.modified,
                   "removed": revision.stats.removed}),
        );
        out.insert("state".into(), json!(&revision.state.to_hex()[..12]));
        if let Some(lineage) = &revision.lineage {
            out.insert(
                "lineage".into(),
                json!({"quarantined": lineage.quarantined, "reason": lineage.reason}),
            );
        }
        if let Some(imported) = &revision.imported {
            out.insert(
                "imported_from".into(),
                json!({"format": 2, "revision": imported.revision}),
            );
        }
        sink.record(out)?;
        shown += 1;
    }
    Ok(Finish::ok(format!("{shown} of {total} revision(s)")))
}

/// The rows and other files a revision changed, against its first parent.
/// Put row files back as history recorded them, as one change: from the
/// revision named, or each from the newest revision that holds it.
pub fn restore(
    context: &Context,
    sink: &mut dyn Sink,
    paths: &[String],
    at: Option<&str>,
) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    if let Some(finish) = super::replayed(context, sink, &database, Origin::User)? {
        return Ok(finish);
    }
    let numbers = database.revision_numbers()?;
    let history = database.history()?;
    let at = at
        .map(|text| history::resolve(&numbers, text))
        .transpose()?;
    let mut changes = vec![];
    let mut expected = reldir_core::db::Expected::new();
    let mut restored = vec![];
    for path in paths {
        let relative = std::path::Path::new(path);
        let parts: Vec<&str> = path.split('/').collect();
        let [table, file] = parts.as_slice() else {
            return Err(DbError::usage(format!(
                "{path:?} is not a row file: name it as `show` and `diff` do, `<table>/<file>.json`"
            )));
        };
        if !file.ends_with(".json") || matches!(*file, "." | "..") || table.starts_with('.') {
            return Err(DbError::usage(format!(
                "{path:?} is not a row file: name it as `show` and `diff` do, `<table>/<file>.json`"
            )));
        }
        if !database.catalog.schemas.contains_key(*table) {
            return Err(database.catalog.unknown_table(table));
        }
        let (revision, bytes) = match at {
            Some(id) => match history.file_at(&id, path)? {
                Some(bytes) => (id, bytes),
                None => {
                    return Err(DbError::new(
                        Code::UNKNOWN_ROW,
                        format!("revision {} holds no {path}", name_of(&numbers, &id)),
                    )
                    .with_help("`reldir log` and `reldir show` say which revisions hold it"));
                }
            },
            None => history.last_known(path)?.ok_or_else(|| {
                DbError::new(Code::UNKNOWN_ROW, format!("no revision holds {path}"))
            })?,
        };
        let mut record = Map::new();
        record.insert("kind".into(), json!("restored"));
        record.insert("path".into(), json!(path));
        record.insert("revision".into(), name_of(&numbers, &revision).to_json());
        restored.push(record);
        expected.insert(relative.to_path_buf(), database.fingerprint(relative)?);
        let current = match std::fs::read(database.root.join(relative)) {
            Ok(current) => Some(current),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(DbError::io(&database.root.join(relative), error)),
        };
        if current.as_deref() != Some(bytes.as_slice()) {
            changes.push(reldir_core::plan::FileChange::Write {
                path: relative.to_path_buf(),
                bytes,
            });
        }
    }
    for record in restored {
        sink.record(record)?;
    }
    if changes.is_empty() {
        return Ok(Finish::ok(
            "every file already holds the bytes history recorded; nothing was written",
        ));
    }
    let outcome = database.commit(changes, &expected, context.request(Origin::User))?;
    super::committed(sink, &outcome, "restored")
}

pub fn show(context: &Context, sink: &mut dyn Sink, revision: String) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    let history = database.history()?;
    let numbers = database.revision_numbers()?;
    let id = history::resolve(&numbers, &revision)?;
    let record = history.revision(&id)?;
    let changes = match record.parents.first() {
        Some(parent) => history.diff(&history.revision(parent)?.tree, &record.tree)?,
        None => history
            .entries(&record.tree)?
            .into_iter()
            .map(|entry| reldir_core::store::tree::Change {
                path: entry.path,
                before: None,
                after: Some((entry.row_hash, entry.blob)),
            })
            .collect(),
    };
    for change in &changes {
        let mut out = Map::new();
        out.insert("kind".into(), json!("revision_change"));
        out.insert(
            "action".into(),
            json!(match (&change.before, &change.after) {
                (None, _) => "added",
                (_, None) => "removed",
                _ => "modified",
            }),
        );
        out.insert("path".into(), json!(change.path));
        sink.record(out)?;
    }
    let name = name_of(&numbers, &id);
    let mut finish = Finish::ok(format!(
        "revision {name} ({}, {}): {} row(s) changed",
        record.origin.as_str(),
        record.timestamp.to_rfc3339(),
        changes.len()
    ))
    .with("revision", name.to_json())
    .with("origin", record.origin.as_str())
    .with("timestamp", record.timestamp.to_rfc3339())
    .with("state_identity", record.state.to_hex());
    if let Some(id) = &record.transaction_id {
        finish = finish.with("transaction", id.clone());
    }
    if let Some(imported) = &record.imported {
        finish = finish.with(
            "imported_from",
            json!({"format": 2, "revision": imported.revision}),
        );
    }
    Ok(finish)
}

/// How two JSON documents differ, member by member. Arrays are compared
/// whole: an element's position is not an identity.
pub fn document_diff(pointer: &str, old: &Value, new: &Value) -> Vec<Map<String, Value>> {
    let mut out = vec![];
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for key in keys {
                let at = format!(
                    "{pointer}/{}",
                    reldir_core::schema::path::escape_pointer(key)
                );
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => out.extend(document_diff(&at, x, y)),
                    (x, y) => out.push(change(&at, x.cloned(), y.cloned())),
                }
            }
        }
        (x, y) if reldir_core::canonical::compact(x) != reldir_core::canonical::compact(y) => {
            out.push(change(pointer, Some(x.clone()), Some(y.clone())))
        }
        _ => {}
    }
    out
}

fn change(pointer: &str, old: Option<Value>, new: Option<Value>) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("kind".into(), json!("value_change"));
    out.insert(
        "change".into(),
        json!(match (&old, &new) {
            (None, _) => "added",
            (_, None) => "removed",
            _ => "changed",
        }),
    );
    out.insert("pointer".into(), json!(pointer));
    out.insert("old".into(), old.unwrap_or(Value::Null));
    out.insert("new".into(), new.unwrap_or(Value::Null));
    out
}

pub struct DiffOptions {
    /// Two revisions to compare; none compares the recorded head with the files.
    pub revisions: Option<(String, String)>,
    /// A git revision to compare the files with, instead of recorded history.
    pub git: Option<String>,
    pub table: Option<String>,
    pub schema_only: bool,
}

/// One state's files, by path: a row's canonical hash (`None` for a file
/// that is not a row) and how to get its bytes.
struct Side {
    files: BTreeMap<String, (Option<String>, Source)>,
}

#[derive(Clone)]
enum Source {
    Blob(Id),
    File(std::path::PathBuf),
    /// A file as a git revision holds it: its bytes, and where it came from.
    Git(std::sync::Arc<Vec<u8>>, String),
}

fn recorded_side(database: &Database, tree: &Id) -> Result<Side> {
    let history = database.history()?;
    let mut files = BTreeMap::new();
    for entry in history.entries(tree)? {
        files.insert(
            entry.path,
            (Some(entry.row_hash.to_hex()), Source::Blob(entry.blob)),
        );
    }
    for (path, blob) in history.extras(tree)? {
        files.insert(path, (None, Source::Blob(blob)));
    }
    Ok(Side { files })
}

fn live_side(database: &Database) -> Result<Side> {
    let mut files = BTreeMap::new();
    for table in database.catalog.schemas.keys() {
        for (path, row_hash, _) in database.catalog.mirror.row_entries(table)? {
            let full = database.root.join(&path);
            files.insert(path, (Some(row_hash), Source::File(full)));
        }
    }
    for file in database.catalog.schema_files.values() {
        let path = reldir_core::catalog::slash(&file.relative);
        files.insert(
            path,
            (None, Source::File(database.root.join(&file.relative))),
        );
    }
    Ok(Side { files })
}

/// The files a git revision holds, as one side of a diff.
fn git_side(database: &Database, revision: &str) -> Result<Side> {
    let tables: Vec<String> = database.catalog.schemas.keys().cloned().collect();
    let mut files = BTreeMap::new();
    for (path, bytes) in super::git::files_at(&database.root, revision, &tables)? {
        let named = format!("{path} at {revision}");
        files.insert(path, (None, Source::Git(std::sync::Arc::new(bytes), named)));
    }
    Ok(Side { files })
}

fn read(database: &Database, source: &Source) -> Result<Value> {
    let bytes = match source {
        Source::Blob(id) => database.history()?.blob(id)?,
        Source::File(path) => std::fs::read(path).map_err(|error| DbError::io(path, error))?,
        Source::Git(bytes, named) => {
            return reldir_core::json::parse(bytes).map_err(|error| {
                DbError::new(Code::INPUT_INVALID, format!("{named} is not JSON: {error}"))
            });
        }
    };
    reldir_core::json::parse(&bytes).map_err(|error| {
        DbError::new(
            Code::INTERNAL_METADATA_CORRUPT,
            format!("a recorded document is not JSON: {error}"),
        )
    })
}

/// What changed, in terms of rows and schemas.
pub fn diff(context: &Context, sink: &mut dyn Sink, options: DiffOptions) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    let (old, new) = match (&options.revisions, &options.git) {
        (_, Some(revision)) => (git_side(&database, revision)?, live_side(&database)?),
        (Some((a, b)), None) => {
            let history = database.history()?;
            let numbers = database.revision_numbers()?;
            let a = history.revision(&history::resolve(&numbers, a)?)?.tree;
            let b = history.revision(&history::resolve(&numbers, b)?)?.tree;
            (recorded_side(&database, &a)?, recorded_side(&database, &b)?)
        }
        (None, None) => {
            let old = match database.head.revision() {
                Some(revision) => recorded_side(&database, &revision.tree)?,
                None => Side {
                    files: BTreeMap::new(),
                },
            };
            (old, live_side(&database)?)
        }
    };
    let is_schema = |path: &str| path.starts_with("schema/") || path.starts_with(".db/schema/");
    let wanted = |path: &str| {
        if options.schema_only && !is_schema(path) {
            return false;
        }
        if path == ".db/config" || path == ".db/format" {
            return false;
        }
        match &options.table {
            Some(table) => {
                path.starts_with(&format!("{table}/"))
                    || path == format!("schema/{table}.json")
                    || path == format!(".db/schema/{table}.json")
            }
            None => true,
        }
    };
    // A row whose content moved to another path is a rename, not a removal
    // and an addition.
    let added_by_hash: BTreeMap<&str, &String> = new
        .files
        .iter()
        .filter(|(path, _)| !old.files.contains_key(*path))
        .filter_map(|(path, (hash, _))| hash.as_deref().map(|hash| (hash, path)))
        .collect();
    let mut renamed_to = BTreeSet::new();
    let mut count = 0;
    for (path, (hash, _)) in &old.files {
        if new.files.contains_key(path) || !wanted(path) {
            continue;
        }
        let mut out = Map::new();
        match hash.as_deref().and_then(|hash| added_by_hash.get(hash)) {
            Some(to) => {
                renamed_to.insert((*to).clone());
                out.insert("kind".into(), json!("renamed"));
                out.insert("change".into(), json!("renamed"));
                out.insert("from".into(), json!(path));
                out.insert("to".into(), json!(to));
            }
            None => {
                out.insert("kind".into(), json!("removed"));
                out.insert("change".into(), json!("removed"));
                out.insert("path".into(), json!(path));
            }
        }
        sink.record(out)?;
        count += 1;
    }
    for (path, (hash, source)) in &new.files {
        if !wanted(path) {
            continue;
        }
        match old.files.get(path) {
            None if renamed_to.contains(path) => {}
            None => {
                let mut out = Map::new();
                out.insert("kind".into(), json!("added"));
                out.insert("change".into(), json!("added"));
                out.insert("path".into(), json!(path));
                sink.record(out)?;
                count += 1;
            }
            Some((before_hash, before)) => {
                let same = match (before_hash, hash) {
                    (Some(left), Some(right)) => left == right,
                    _ => {
                        reldir_core::canonical::compact(&read(&database, before)?)
                            == reldir_core::canonical::compact(&read(&database, source)?)
                    }
                };
                if same {
                    continue;
                }
                for mut change in
                    document_diff("", &read(&database, before)?, &read(&database, source)?)
                {
                    change.insert("path".into(), json!(path));
                    sink.record(change)?;
                    count += 1;
                }
            }
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
            let database = context.open_database(sink, Intent::Read)?;
            let numbers = database.revision_numbers()?;
            let snapshots = reldir_core::snapshot::list(&database)?;
            for (name, snapshot) in &snapshots {
                let mut out = Map::new();
                out.insert("kind".into(), json!("snapshot"));
                out.insert("name".into(), json!(name));
                out.insert("created".into(), json!(snapshot.created.to_rfc3339()));
                out.insert("valid".into(), json!(snapshot.valid));
                out.insert(
                    "revision".into(),
                    snapshot
                        .revision
                        .map_or(Value::Null, |id| name_of(&numbers, &id).to_json()),
                );
                sink.record(out)?;
            }
            Ok(Finish::ok(format!("{} snapshot(s)", snapshots.len())))
        }
        SnapshotAction::Create(name) => {
            let mut database = context.open_database(sink, Intent::Write)?;
            reldir_core::snapshot::validate_name(&name)?;
            if context.dry_run {
                return Ok(Finish::ok(format!("would take snapshot {name}")));
            }
            database.exclusive(|database| reldir_core::snapshot::create(database, &name))?;
            Ok(Finish::ok(format!("took snapshot {name}")).with("name", name))
        }
        SnapshotAction::Restore(name) => {
            let mut database = context.open_database(sink, Intent::Write)?;
            if let Some(finish) =
                super::replayed(context, sink, &database, Origin::SnapshotRestore)?
            {
                return Ok(finish);
            }
            let restore = reldir_core::snapshot::restore(&database, &name)?;
            if restore.changes.is_empty() {
                return Ok(Finish::ok(format!(
                    "the files already hold snapshot {name}; nothing was written"
                )));
            }
            super::confirm(
                context,
                &format!(
                    "restore snapshot {name}, changing {} file(s)",
                    restore.changes.len()
                ),
            )?;
            let outcome = database.commit(
                restore.changes,
                &reldir_core::db::Expected::new(),
                reldir_core::db::Request {
                    admission: reldir_core::db::Admission::Restores(restore.state),
                    ..context.request(Origin::SnapshotRestore)
                },
            )?;
            super::committed(sink, &outcome, "restored")
        }
        SnapshotAction::Delete(name) => {
            let mut database = context.open_database(sink, Intent::Write)?;
            super::confirm(context, &format!("delete snapshot {name}"))?;
            if context.dry_run {
                return Ok(Finish::ok(format!("would delete snapshot {name}")));
            }
            database.exclusive(|database| reldir_core::snapshot::delete(database, &name))?;
            Ok(Finish::ok(format!("deleted snapshot {name}")))
        }
    }
}

/// Finish interrupted transactions; with `new_lineage`, also move unverifiable
/// history aside and begin a new one.
pub fn recover(context: &Context, sink: &mut dyn Sink, new_lineage: bool) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    if !new_lineage {
        let recovered = database
            .events
            .iter()
            .any(|event| matches!(event, reldir_core::db::Event::Recovered { .. }));
        return Ok(Finish::ok(if recovered {
            "recovered"
        } else {
            "nothing to recover"
        })
        .with("recovered", recovered));
    }
    if !context.allow_destructive {
        return Err(DbError::new(
            Code::DECISION_REQUIRED,
            "a new lineage moves every recorded head aside; the old history stays on disk but no \
             longer describes the database",
        )
        .with_help("pass --allow-destructive to begin a new lineage"));
    }
    if context.dry_run {
        return Ok(Finish::ok(
            "would move history's heads to .db/history/quarantine/ and begin a new lineage",
        ));
    }
    let reason = match &database.head {
        reldir_core::db::Head::Degraded(error) => error.diagnostic.message.clone(),
        _ => "requested".to_string(),
    };
    let name = database.begin_new_lineage(&reason)?;
    sink.event(
        "new_lineage",
        json!({"revision": name.to_json(), "reason": reason}),
        "history's heads were moved to .db/history/quarantine/",
    )?;
    Ok(
        Finish::ok(format!("began a new lineage at revision {name}"))
            .with("revision", name.to_json()),
    )
}

pub fn gc(context: &Context, sink: &mut dyn Sink) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    let report = database.collect_garbage(context.dry_run)?;
    Ok(Finish::ok(format!(
        "{} {} object(s) nothing reaches; kept {}",
        if context.dry_run {
            "would remove"
        } else {
            "removed"
        },
        report.dropped,
        report.kept
    ))
    .with("dropped", report.dropped)
    .with("kept", report.kept)
    .with("packs_before", report.packs_before))
}

/// Refresh the query planner's statistics.
pub fn analyze(context: &Context, sink: &mut dyn Sink) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    database.catalog.mirror.analyze()?;
    Ok(Finish::ok("refreshed query statistics"))
}
