//! Schemas: showing, declaring, pinning, inferring and migrating them.

use super::{Context, Intent, require_valid, schema_of};
use crate::{
    db::{Admission, Expected, Request},
    diagnostic::{DbError, Diagnostic, Result},
    infer::Strictness,
    migrate::{Migration, Operation},
    output::{Finish, Sink},
    schema::{
        ColumnType,
        document::{TableBuilder, subschema},
    },
    transaction::Change,
};
use serde_json::{Map, Value, json};
use std::path::PathBuf;

pub fn show(context: &Context, sink: &mut dyn Sink, table: &str) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    let schema = schema_of(&database, table)?;
    sink.document("schema", schema.document().clone())?;
    Ok(Finish::ok(String::new()).with("identity", schema.identity()))
}

/// The dialect every schema is written in, as a JSON Schema document an
/// editor can validate against. Needs no database.
pub fn dialect(sink: &mut dyn Sink) -> Result<Finish> {
    sink.document("dialect", crate::schema::meta::meta_schema().clone())?;
    Ok(Finish::ok(String::new()))
}

/// Every fault in the schemas alone, without judging rows.
pub fn validate(context: &Context, sink: &mut dyn Sink) -> Result<Finish> {
    let Some(database) = context.open(sink, Intent::Read)? else {
        return Ok(super::empty());
    };
    let faults: Vec<&Diagnostic> = database
        .verdict
        .errors
        .iter()
        .filter(|d| {
            d.code.starts_with("SCHEMA_")
                || d.path
                    .as_deref()
                    .is_some_and(|p| crate::schema_store::schema_table(p).is_some())
        })
        .collect();
    for fault in &faults {
        sink.diagnostic(fault)?;
    }
    Ok(Finish::ok(format!(
        "{} schema(s), {} fault(s)",
        database.catalog.schema_files.len(),
        faults.len()
    ))
    .exit(if faults.is_empty() { 0 } else { 2 })
    .with("valid", faults.is_empty()))
}

/// Declare a new, empty table as a pin: a string primary key `id`, nothing
/// else, ready to be edited.
pub fn new(context: &Context, sink: &mut dyn Sink, table: &str) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    if database.catalog.schema_files.contains_key(table) {
        return Err(DbError::new(
            "SCHEMA_TABLE_EXISTS",
            format!("{table} already has a schema"),
            2,
        ));
    }
    if !crate::schema::valid_name(table) {
        return Err(DbError::new(
            "SCHEMA_INVALID_TABLE_NAME",
            format!("{table:?} cannot be a table name"),
            2,
        ));
    }
    let schema = TableBuilder::new(table)
        .column("id", subschema(&ColumnType::String, false), true)
        .primary_key(vec!["id".into()])
        .build()
        .map_err(|problems| {
            DbError::from_diag(
                problems
                    .into_iter()
                    .next()
                    .expect("a failed build reports why"),
                2,
            )
        })?;
    let path = PathBuf::from(crate::schema_store::pin_relative(table));
    let expected = Expected::from([(path.clone(), None)]);
    let outcome = database.commit(
        vec![Change::Write {
            path,
            bytes: schema.bytes(database.config.indentation_width),
        }],
        &expected,
        Request {
            origin: "migration",
            ..Request::internal(context.dry_run)
        },
    )?;
    super::committed(sink, &outcome, "declared")
}

/// Make a table's inferred working schema its pin.
pub fn pin(context: &Context, sink: &mut dyn Sink, table: &str) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    let schema = schema_of(&database, table)?.clone();
    if database.catalog.pinned.contains(table) {
        return Ok(Finish::ok(format!("{table} is already pinned")));
    }
    let pin = PathBuf::from(crate::schema_store::pin_relative(table));
    let working = crate::schema_store::working_relative(table);
    let expected = Expected::from([
        (pin.clone(), None),
        (working.clone(), database.fingerprint(&working)?),
    ]);
    let outcome = database.commit(
        vec![
            Change::Write {
                path: pin,
                bytes: schema.bytes(database.config.indentation_width),
            },
            Change::Delete { path: working },
        ],
        &expected,
        Request {
            origin: "migration",
            ..Request::internal(context.dry_run)
        },
    )?;
    super::committed(sink, &outcome, "pinned")
}

/// What to do when inference meets a table that already has a schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnConflict {
    /// Ask on a terminal; elsewhere, stop with `DECISION_REQUIRED`.
    Ask,
    /// Stop with `SCHEMA_CONFLICT`.
    Fail,
    /// Show how the inferred schema differs, and change nothing.
    Compare,
    /// Replace it. Replacing a pin also needs `--allow-destructive`.
    Reinfer,
}

impl OnConflict {
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "ask" => Ok(Self::Ask),
            "fail" => Ok(Self::Fail),
            "compare" => Ok(Self::Compare),
            "reinfer" => Ok(Self::Reinfer),
            other => Err(DbError::usage(format!(
                "--on-schema-conflict is ask, fail, compare or reinfer, not {other:?}"
            ))),
        }
    }
}

pub struct InferOptions<'a> {
    pub tables: Vec<String>,
    pub all: bool,
    pub write: bool,
    pub strictness: Strictness,
    pub primary_key: Option<&'a [String]>,
    pub on_conflict: OnConflict,
}

pub fn parse_strictness(text: &str) -> Result<Strictness> {
    match text {
        "strict" => Ok(Strictness::Strict),
        "balanced" => Ok(Strictness::Balanced),
        "loose" => Ok(Strictness::Loose),
        other => Err(DbError::usage(format!(
            "--strictness is strict, balanced or loose, not {other:?}"
        ))),
    }
}

/// Infer schemas from rows. Without `--write` the schemas are only shown.
pub fn infer(context: &Context, sink: &mut dyn Sink, options: InferOptions<'_>) -> Result<Finish> {
    let root = context.root()?;
    let has_database = root.join(".db").is_dir();
    let database = if has_database {
        Some(context.open_database(
            sink,
            if options.write {
                Intent::Write
            } else {
                Intent::Read
            },
        )?)
    } else {
        None
    };
    let config = match &database {
        Some(database) => database.config.clone(),
        None => {
            let mut config = crate::config::Config::default();
            config.apply_overrides(&context.overrides);
            config
        }
    };
    let tables = if options.all || options.tables.is_empty() {
        crate::infer::discover_tables(&root)?
    } else {
        options.tables.clone()
    };
    if options.primary_key.is_some() && tables.len() != 1 {
        return Err(DbError::usage(
            "--pk chooses one table's key; name exactly one table",
        ));
    }
    let governed = database.as_ref().map(|database| &database.catalog);
    let inferred = crate::infer::infer_all(
        &root,
        &tables,
        options.strictness,
        &config,
        options.primary_key,
        governed,
    )?;
    if !options.write {
        for schema in inferred.values() {
            sink.document("schema", schema.document().clone())?;
        }
        return Ok(Finish::ok(format!(
            "inferred {} schema(s); add --write to keep them",
            inferred.len()
        )));
    }
    let Some(mut database) = database else {
        // Nothing governs this folder yet: establishing the database infers
        // and records exactly this.
        let established = context.open_database(sink, Intent::Write)?;
        return Ok(Finish::ok(format!(
            "established the database with {} table(s)",
            established.catalog.schemas.len()
        )));
    };
    let mut changes = vec![];
    let mut expected = Expected::new();
    let mut replaced_pins = vec![];
    for (table, schema) in &inferred {
        let current = database.catalog.schemas.get(table);
        if let Some(current) = current {
            if crate::schema_store::equivalent(current, schema) {
                continue;
            }
            match options.on_conflict {
                OnConflict::Fail => {
                    return Err(DbError::from_diag(
                        Diagnostic::error("SCHEMA_CONFLICT", format!("{table} already has a schema, and the inferred one differs"))
                            .table(table.as_str())
                            .help("--on-schema-conflict compare shows the difference; reinfer replaces it"),
                        2,
                    ));
                }
                OnConflict::Compare => {
                    for change in crate::command::history::document_diff(
                        "",
                        current.document(),
                        schema.document(),
                    ) {
                        let mut record = change;
                        record.insert("table".into(), json!(table));
                        sink.record(record)?;
                    }
                    continue;
                }
                OnConflict::Ask => super::confirm(
                    context,
                    &format!("replace the schema of {table} with the inferred one"),
                )?,
                OnConflict::Reinfer => {}
            }
            if database.catalog.pinned.contains(table) {
                replaced_pins.push(table.clone());
            }
        }
        let path = if database.catalog.pinned.contains(table) {
            PathBuf::from(crate::schema_store::pin_relative(table))
        } else {
            crate::schema_store::working_relative(table)
        };
        expected.insert(path.clone(), database.fingerprint(&path)?);
        changes.push(Change::Write {
            path,
            bytes: schema.bytes(database.config.indentation_width),
        });
    }
    if options.on_conflict == OnConflict::Compare && changes.is_empty() {
        return Ok(Finish::ok("compared; nothing was written"));
    }
    if !replaced_pins.is_empty() && !context.allow_destructive {
        return Err(DbError::from_diag(
            Diagnostic::error(
                "DECISION_REQUIRED",
                format!(
                    "re-inferring would replace the pinned declaration of {}",
                    replaced_pins.join(", ")
                ),
            )
            .help("a pin is what someone wrote; pass --allow-destructive to replace it"),
            9,
        ));
    }
    let outcome = database.commit(
        changes,
        &expected,
        Request {
            origin: "migration",
            ..Request::internal(context.dry_run)
        },
    )?;
    super::committed(sink, &outcome, "inferred")
}

/// Run a migration: one operation from the command line, or a file of them.
pub fn migrate(context: &Context, sink: &mut dyn Sink, migration: Migration) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    require_valid(&database)?;
    let destructive: Vec<String> = migration
        .operations
        .iter()
        .filter(|operation| {
            matches!(
                operation,
                Operation::DropTable { .. } | Operation::DropColumn { .. }
            )
        })
        .map(Operation::describe)
        .collect();
    let (changes, expected) = crate::migrate::plan(&database, &migration)?;
    for operation in &migration.operations {
        let mut record = Map::new();
        record.insert("kind".into(), json!("operation"));
        record.insert("description".into(), json!(operation.describe()));
        sink.record(record)?;
    }
    if !destructive.is_empty() {
        super::confirm(
            context,
            &format!("this discards data: {}; continue", destructive.join("; ")),
        )?;
    }
    let outcome = database.commit(
        changes,
        &expected,
        Request {
            origin: "migration",
            admission: Admission::Valid,
            dry_run: context.dry_run,
        },
    )?;
    super::committed(sink, &outcome, "migrated")
}

/// A migration file, read and checked.
pub fn read_migration(path: &str) -> Result<Migration> {
    let text = super::read_input(path, 64 * 1024 * 1024)?;
    let value: Value = crate::json::parse_str(&text)
        .map_err(|error| DbError::usage(format!("{path} is not JSON: {error}")))?;
    serde_json::from_value(value)
        .map_err(|error| DbError::usage(format!("{path} is not a migration: {error}")))
}
