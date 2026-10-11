//! Schemas: showing, declaring, pinning, inferring and migrating them.

use super::{Context, Intent, require_valid, schema_of};
use crate::output::{Finish, Sink};
use reldir_core::code::Code;
use reldir_core::{
    db::Expected,
    diagnostic::{DbError, Diagnostic, Result},
    migrate::{Migration, Operation},
    plan::FileChange,
    schema::{
        ColumnType,
        document::{TableBuilder, subschema},
    },
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
    sink.document("dialect", reldir_core::schema::meta::meta_schema().clone())?;
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
            d.code.as_str().starts_with("SCHEMA_")
                || d.path
                    .as_deref()
                    .is_some_and(|p| reldir_core::schema_store::schema_table(p).is_some())
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
    if let Some(finish) = super::replayed(
        context,
        sink,
        &database,
        reldir_core::store::revision::Origin::Migration,
    )? {
        return Ok(finish);
    }
    if database.catalog.schema_files.contains_key(table) {
        return Err(DbError::new(
            Code::SCHEMA_TABLE_EXISTS,
            format!("{table} already has a schema"),
        ));
    }
    if !reldir_core::schema::valid_name(table) {
        return Err(DbError::new(
            Code::SCHEMA_INVALID_TABLE_NAME,
            format!("{table:?} cannot be a table name"),
        ));
    }
    let schema = TableBuilder::new(table)
        .column("id", subschema(&ColumnType::String, false), true)
        .key(vec!["id".into()])
        .build()
        .map_err(|problems| {
            DbError::from_diag(
                problems
                    .into_iter()
                    .next()
                    .expect("a failed build reports why"),
            )
        })?;
    let path = PathBuf::from(reldir_core::schema_store::pin_relative(table));
    let expected = Expected::from([(path.clone(), None)]);
    let outcome = database.commit(
        vec![FileChange::Write {
            path,
            bytes: schema.render(None)?,
        }],
        &expected,
        context.request(reldir_core::store::revision::Origin::Migration),
    )?;
    super::committed(sink, &outcome, "declared")
}

/// Make a table's inferred working schema its pin.
pub fn pin(context: &Context, sink: &mut dyn Sink, table: &str) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    if let Some(finish) = super::replayed(
        context,
        sink,
        &database,
        reldir_core::store::revision::Origin::Migration,
    )? {
        return Ok(finish);
    }
    let schema = schema_of(&database, table)?.clone();
    if database.catalog.pinned.contains(table) {
        return Ok(Finish::ok(format!("{table} is already pinned")));
    }
    let pin = PathBuf::from(reldir_core::schema_store::pin_relative(table));
    let working = reldir_core::schema_store::working_relative(table);
    let expected = Expected::from([
        (pin.clone(), None),
        (working.clone(), database.fingerprint(&working)?),
    ]);
    let outcome = database.commit(
        vec![
            FileChange::Write {
                path: pin,
                bytes: schema.render(None)?,
            },
            FileChange::Delete { path: working },
        ],
        &expected,
        context.request(reldir_core::store::revision::Origin::Migration),
    )?;
    super::committed(sink, &outcome, "pinned")
}

/// Widen a table's contract: with no member named, just enough to admit what
/// its files hold where they break it; at a member, as stated.
pub fn widen(
    context: &Context,
    sink: &mut dyn Sink,
    target: &str,
    stated: reldir_core::schema::widen::Stated,
) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    if let Some(finish) = super::replayed(
        context,
        sink,
        &database,
        reldir_core::store::revision::Origin::User,
    )? {
        return Ok(finish);
    }
    let (table, member) = match target.split_once('.') {
        Some((table, member)) => (table, Some(member)),
        None => (target, None),
    };
    let schema = schema_of(&database, table)?.clone();
    let says_how = stated.null || stated.optional || !stated.types.is_empty() || stated.any;
    let (document, made) = match member {
        None => {
            if says_how {
                return Err(DbError::usage(format!(
                    "--null, --optional, --type and --any widen one member; name it, as in {table}.<member>"
                )));
            }
            let faults: Vec<Diagnostic> = database
                .verdict
                .errors
                .iter()
                .chain(&database.verdict.warnings)
                .filter(|fault| {
                    fault.table.as_deref() == Some(table) && reldir_core::contract::is_fault(fault)
                })
                .cloned()
                .collect();
            reldir_core::contract::admitting_files(&database.catalog, table, &faults)?
        }
        Some(member) => {
            if !says_how {
                return Err(DbError::usage(
                    "say how to widen the member: --null, --optional, --type TYPE or --any",
                ));
            }
            let path = reldir_core::schema::RefPath::parse(member).map_err(|error| {
                DbError::usage(format!("{member:?} is not a member path: {error}"))
            })?;
            reldir_core::schema::widen::stated(schema.document(), &path, &stated)
                .map_err(|reason| DbError::usage(format!("{target} cannot be widened: {reason}")))?
        }
    };
    if made.is_empty() {
        return Ok(Finish::ok(format!(
            "the contract of {table} already admits it"
        )));
    }
    let widened =
        reldir_core::schema::Schema::from_document(document, None).map_err(|problems| {
            DbError::from_diag(
                problems
                    .into_iter()
                    .next()
                    .expect("a schema that does not load says why"),
            )
        })?;
    let file = database
        .catalog
        .schema_files
        .get(table)
        .cloned()
        .ok_or_else(|| database.catalog.unknown_table(table))?;
    let expected = Expected::from([(file.relative.clone(), database.fingerprint(&file.relative)?)]);
    let outcome = database.commit(
        vec![FileChange::Write {
            path: file.relative.clone(),
            bytes: widened.render(Some(&file.bytes))?,
        }],
        &expected,
        reldir_core::db::Request {
            // Widening only ever removes faults; a widened schema that would
            // add one -- an identity made optional -- is refused.
            admission: reldir_core::db::Admission::NoNewFaults,
            ..context.request(reldir_core::store::revision::Origin::User)
        },
    )?;
    let edits: Vec<String> = made.iter().map(ToString::to_string).collect();
    sink.event(
        "contract_widened",
        json!({"table": table, "edits": edits}),
        &format!("widened the contract of {table}: {}", edits.join("; ")),
    )?;
    super::committed(sink, &outcome, "widened")
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
    pub tighten: reldir_core::infer::Tighten,
    pub primary_key: Option<&'a [String]>,
    pub on_conflict: OnConflict,
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
            let mut config = reldir_core::config::Config::default();
            config.apply_overrides(&context.overrides);
            config
        }
    };
    let tables = if options.all || options.tables.is_empty() {
        reldir_core::infer::discover_tables(&root)?
    } else {
        options.tables.clone()
    };
    if options.primary_key.is_some() && tables.len() != 1 {
        return Err(DbError::usage(
            "--pk chooses one table's key; name exactly one table",
        ));
    }
    let governed = database.as_ref().map(|database| &database.catalog);
    let inferred = reldir_core::infer::infer_all(
        &root,
        &tables,
        reldir_core::infer::Options {
            tighten: options.tighten,
            key: options.primary_key,
        },
        &config,
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
    if let Some(finish) = super::replayed(
        context,
        sink,
        &database,
        reldir_core::store::revision::Origin::Migration,
    )? {
        return Ok(finish);
    }
    let mut changes = vec![];
    let mut expected = Expected::new();
    let mut replaced_pins = vec![];
    for (table, schema) in &inferred {
        let current = database.catalog.schemas.get(table);
        if let Some(current) = current {
            if reldir_core::schema_store::equivalent(current, schema) {
                continue;
            }
            match options.on_conflict {
                OnConflict::Fail => {
                    return Err(DbError::from_diag(
                        Diagnostic::error(Code::SCHEMA_CONFLICT, format!("{table} already has a schema, and the inferred one differs"))
                            .table(table.as_str())
                            .help("--on-schema-conflict compare shows the difference; reinfer replaces it"),
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
            PathBuf::from(reldir_core::schema_store::pin_relative(table))
        } else {
            reldir_core::schema_store::working_relative(table)
        };
        expected.insert(path.clone(), database.fingerprint(&path)?);
        changes.push(FileChange::Write {
            path,
            bytes: schema.render(None)?,
        });
    }
    if options.on_conflict == OnConflict::Compare && changes.is_empty() {
        return Ok(Finish::ok("compared; nothing was written"));
    }
    if !replaced_pins.is_empty() && !context.allow_destructive {
        return Err(DbError::from_diag(
            Diagnostic::error(
                Code::DECISION_REQUIRED,
                format!(
                    "re-inferring would replace the pinned declaration of {}",
                    replaced_pins.join(", ")
                ),
            )
            .help("a pin is what someone wrote; pass --allow-destructive to replace it"),
        ));
    }
    let outcome = database.commit(
        changes,
        &expected,
        context.request(reldir_core::store::revision::Origin::Migration),
    )?;
    super::committed(sink, &outcome, "inferred")
}

/// Run a migration: one operation from the command line, or a file of them.
pub fn migrate(context: &Context, sink: &mut dyn Sink, migration: Migration) -> Result<Finish> {
    let mut database = context.open_database(sink, Intent::Write)?;
    if let Some(finish) = super::replayed(
        context,
        sink,
        &database,
        reldir_core::store::revision::Origin::Migration,
    )? {
        return Ok(finish);
    }
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
    let (changes, expected) = reldir_core::migrate::plan(&database, &migration)?;
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
        context.request(reldir_core::store::revision::Origin::Migration),
    )?;
    super::committed(sink, &outcome, "migrated")
}

/// A migration file, read and checked.
pub fn read_migration(path: &str) -> Result<Migration> {
    let text = super::read_input(path, 64 * 1024 * 1024)?;
    let value: Value = reldir_core::json::parse_str(&text).map_err(|error| {
        DbError::new(Code::INPUT_INVALID, format!("{path} is not JSON: {error}"))
    })?;
    serde_json::from_value(value)
        .map_err(|error| DbError::usage(format!("{path} is not a migration: {error}")))
}
