//! Creating a database, and looking at a folder before making it one.

use super::Context;
use crate::{
    db::{Access, Database},
    diagnostic::{DbError, Result},
    output::{Finish, Sink},
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
) -> Result<Finish> {
    let root = target(context, path)?;
    if context.readonly {
        return Err(DbError::new(
            "READ_ONLY",
            "init creates a database, and --readonly forbids any write",
            1,
        ));
    }
    let observation = crate::state::observe(&root)?;
    if observation.format != FormatState::Absent {
        return Err(DbError::new(
            "ALREADY_INITIALIZED",
            format!("{} already has .db/", root.display()),
            1,
        ));
    }
    let candidates: Vec<String> = observation
        .topology
        .table_candidates
        .iter()
        .filter(|table| !observation.topology.pinned.contains(table))
        .cloned()
        .collect();
    if !observation.topology.loose_json.is_empty() {
        return Err(DbError::new(
            "ROOT_JSON_AMBIGUOUS",
            format!(
                "JSON files sit at the root and belong to no table: {}",
                observation.topology.loose_json.join(", ")
            ),
            1,
        )
        .with_help("move them into a table directory first"));
    }
    if !candidates.is_empty() && !adopt {
        return Err(DbError::new(
            "USAGE",
            format!("{} already holds data ({}); adopting it infers schemas for it", root.display(), candidates.join(", ")),
            1,
        )
        .with_help("pass --adopt to govern the existing data, or `reldir inspect` to see what adoption would infer"));
    }
    let mut config = crate::config::Config::default();
    config.apply_overrides(&context.overrides);
    let inferred = if candidates.is_empty() {
        Default::default()
    } else {
        crate::infer::infer_all(
            &root,
            &candidates,
            crate::infer::Strictness::Balanced,
            &config,
            None,
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
pub fn inspect(context: &Context, sink: &mut dyn Sink, path: Option<&Path>) -> Result<Finish> {
    let root = target(context, path)?;
    if !root.is_dir() {
        return Err(DbError::new(
            "PATH_NOT_FOUND",
            format!("{} is not a directory", root.display()),
            1,
        ));
    }
    let observation = crate::state::observe(&root)?;
    let format = match &observation.format {
        FormatState::Absent => "absent".to_string(),
        FormatState::Present => format!("format {}", crate::db::read_format(&root)?),
        FormatState::MarkerMissingRecoverable => "unlabelled, rebuildable".into(),
        FormatState::MarkerMissingUnrecoverable(lost) => {
            format!("unlabelled, holding {}", lost.join(", "))
        }
    };
    let opening = crate::state::Opening {
        access: Access::Read,
        establish: true,
        rebuild_metadata: false,
        dry_run: true,
    };
    let database = match crate::state::open(&root, opening, &context.overrides)? {
        crate::state::Opened::Empty => None,
        crate::state::Opened::Database { database, .. } => Some(database),
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
