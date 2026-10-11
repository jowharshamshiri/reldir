//! Creating a database, and looking at a folder before making it one.

use super::Context;
use crate::output::{Finish, Sink};
use reldir_core::code::Code;
use reldir_core::{
    db::{Access, Database},
    diagnostic::{DbError, Result},
    state::FormatState,
};
use serde_json::{Map, json};
use std::path::{Path, PathBuf};

fn target(context: &Context, path: Option<&Path>) -> Result<PathBuf> {
    let path = path.map(Path::to_path_buf).or_else(|| context.db.clone());
    match path {
        Some(path) if path.is_absolute() => Ok(path),
        Some(path) => Ok(std::env::current_dir()
            .map_err(|error| DbError::io(Path::new("."), error))?
            .join(path)),
        None => std::env::current_dir().map_err(|error| DbError::io(Path::new("."), error)),
    }
}

/// Make a folder a database. A folder that already holds data is adopted only
/// with `adopt`: its tables' schemas are inferred, and the first revision
/// records what was there.
pub fn init(
    context: &Context,
    sink: &mut dyn Sink,
    path: Option<&Path>,
    adopt: bool,
    track_provenance: bool,
    ignore: &[String],
) -> Result<Finish> {
    let root = target(context, path)?;
    if context.readonly {
        return Err(DbError::new(
            Code::READ_ONLY,
            "init creates a database, and --readonly forbids any write",
        ));
    }
    let observation = reldir_core::state::observe(&root)?;
    if observation.format != FormatState::Absent {
        return Err(DbError::new(
            Code::ALREADY_INITIALIZED,
            format!("{} already has .db/", root.display()),
        ));
    }
    // The settings the database begins with: what adoption reads already
    // leaves out what they ignore.
    let mut settings = reldir_core::config::Config::default();
    settings.ignore.extend(ignore.iter().cloned());
    let ignored = settings
        .ignore_set()
        .map_err(|reason| DbError::usage(format!("--ignore: {reason}")))?;
    let candidates: Vec<String> = observation
        .topology
        .table_candidates
        .iter()
        .filter(|table| !observation.topology.pinned.contains(table))
        .filter(|table| !ignored.is_match(table.as_str()))
        .cloned()
        .collect();
    let loose: Vec<&str> = observation
        .topology
        .loose_json
        .iter()
        .map(String::as_str)
        .filter(|name| !ignored.is_match(name))
        .collect();
    if !loose.is_empty() {
        return Err(DbError::new(
            Code::ROOT_JSON_AMBIGUOUS,
            format!(
                "JSON files sit at the root and belong to no table: {}",
                loose.join(", ")
            ),
        )
        .with_help("move them into a table directory, or leave them out with --ignore"));
    }
    if !candidates.is_empty() && !adopt {
        return Err(DbError::new(
            Code::USAGE,
            format!("{} already holds data ({}); adopting it infers schemas for it", root.display(), candidates.join(", ")),
        )
        .with_help("pass --adopt to govern the existing data, or `reldir inspect` to see what adoption would infer"));
    }
    let mut config = settings.clone();
    config.apply_overrides(&context.overrides);
    let inferred = if candidates.is_empty() {
        Default::default()
    } else {
        reldir_core::infer::infer_all(
            &root,
            &candidates,
            reldir_core::infer::Options::default(),
            &config,
            None,
        )?
    };
    for (table, schema) in &inferred {
        let mut record = Map::new();
        record.insert("kind".into(), json!("inferred_table"));
        record.insert("table".into(), json!(table));
        record.insert("primary_key".into(), json!(schema.primary_key()));
        record.insert("references".into(), json!(schema.foreign_keys().len()));
        sink.record(record)?;
    }
    if context.dry_run {
        return Ok(Finish::ok(format!(
            "would initialize {} with {} inferred table(s)",
            root.display(),
            inferred.len()
        )));
    }
    std::fs::create_dir_all(&root).map_err(|error| DbError::io(&root, error))?;
    let database = Database::create(
        root.clone(),
        &inferred,
        track_provenance,
        &settings,
        &context.overrides,
    )?;
    for diagnostic in &database.verdict.errors {
        sink.diagnostic(diagnostic)?;
    }
    let state = if database.is_valid() {
        "VALID"
    } else {
        "INVALID"
    };
    Ok(Finish::ok(format!(
        "initialized {} ({state}, {} table(s))",
        root.display(),
        database.catalog.schemas.len()
    ))
    .exit(if database.is_valid() { 0 } else { 2 })
    .with("valid", database.is_valid())
    .with("tables", database.catalog.schemas.len()))
}

/// What a folder holds and what reldir would make of it. Writes nothing.
pub fn inspect(
    context: &Context,
    sink: &mut dyn Sink,
    path: Option<&Path>,
    ignore: &[String],
) -> Result<Finish> {
    let root = target(context, path)?;
    if !root.is_dir() {
        return Err(DbError::new(
            Code::PATH_NOT_FOUND,
            format!("{} is not a directory", root.display()),
        ));
    }
    let observation = reldir_core::state::observe(&root)?;
    let format = match &observation.format {
        FormatState::Absent => "absent".to_string(),
        FormatState::Present => format!("format {}", reldir_core::db::read_format(&root)?),
        FormatState::MarkerMissingRecoverable => "unlabelled, rebuildable".into(),
        FormatState::MarkerMissingUnrecoverable(lost) => {
            format!("unlabelled, holding {}", lost.join(", "))
        }
    };
    let opening = reldir_core::state::Opening {
        access: Access::Read,
        establish: true,
        rebuild_metadata: false,
        dry_run: true,
        ignore: ignore.to_vec(),
    };
    let database = match reldir_core::state::open(&root, opening, &context.overrides)? {
        reldir_core::state::Opened::Empty => None,
        reldir_core::state::Opened::Database { database, .. } => Some(database),
    };
    if let Some(database) = &database {
        for (table, schema) in &database.catalog.schemas {
            let mut record = Map::new();
            record.insert("kind".into(), json!("table"));
            record.insert("table".into(), json!(table));
            record.insert("rows".into(), json!(database.catalog.mirror.count(table)?));
            record.insert("primary_key".into(), json!(schema.primary_key()));
            record.insert(
                "declared".into(),
                json!(database.catalog.schema_files.contains_key(table)),
            );
            sink.record(record)?;
        }
        for diagnostic in &database.verdict.errors {
            sink.diagnostic(diagnostic)?;
        }
    }
    Ok(Finish::ok(format!(
        "{}: metadata {format}; {} table(s){}",
        root.display(),
        database
            .as_ref()
            .map_or(0, |database| database.catalog.schemas.len()),
        match &database {
            Some(database) if !database.is_valid() =>
                format!(", {} violation(s)", database.verdict.errors.len()),
            _ => String::new(),
        }
    ))
    .with("metadata", format)
    .with(
        "valid",
        database.as_ref().is_none_or(|database| database.is_valid()),
    ))
}

/// Convert a format 2 database to format 3, and record the change.
pub fn upgrade(context: &Context, sink: &mut dyn Sink, new_lineage: bool) -> Result<Finish> {
    let root = context.root()?;
    let wait = std::time::Duration::from_secs_f64(
        context
            .overrides
            .wait_seconds
            .unwrap_or(reldir_core::config::Config::default().wait_seconds),
    );
    let report = reldir_core::upgrade::upgrade(&root, new_lineage, wait)?;
    let database = reldir_core::db::Database::open_upgraded(root, &context.overrides)?;
    for event in &database.events {
        let value = event.to_json();
        let kind = value["kind"].as_str().unwrap_or("event").to_string();
        sink.event(&kind, value, &event.describe())?;
    }
    let mut record = serde_json::Map::new();
    record.insert("kind".into(), serde_json::json!("upgrade"));
    record.insert("revisions".into(), serde_json::json!(report.revisions));
    record.insert("snapshots".into(), serde_json::json!(report.snapshots));
    record.insert("reinferred".into(), serde_json::json!(report.reinferred));
    record.insert("converted".into(), serde_json::json!(report.converted));
    record.insert("pins".into(), serde_json::json!(report.pins));
    record.insert("backup".into(), serde_json::json!(report.backup));
    record.insert("new_lineage".into(), serde_json::json!(report.new_lineage));
    sink.record(record)?;
    if !report.converted.is_empty() {
        sink.note(&format!(
            "{} kept the working schema format 2 had, which a migration, a repair or a hand edit \
             shaped; `reldir lint` shows any rule it states that the rows only happen to satisfy",
            report.converted.join(", ")
        ));
    }
    let history = if report.new_lineage {
        "format 2 history kept aside".to_string()
    } else {
        format!("{} revision(s) brought over", report.revisions)
    };
    let mut finish = Finish::ok(format!(
        "upgraded to format 3: {history}, {} snapshot(s), {} pin(s) rewritten; format 2's metadata is in .db/format-2/",
        report.snapshots.len(),
        report.pins.len()
    ))
    .with("valid", database.is_valid());
    if let Some(name) = database.head.name() {
        finish = finish.with("revision", name.to_json());
    }
    Ok(finish)
}
