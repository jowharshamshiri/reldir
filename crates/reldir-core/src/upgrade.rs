//! `reldir upgrade`: a format 2 database made format 3. The only code that
//! reads format 2.
//!
//! What format 2 kept is carried over, and what it decided is kept:
//!
//! - **History.** Every format 2 revision is verified as format 2 verified it
//!   -- each names its predecessor, every object it adds hashes to its entry,
//!   each recorded root is the root of the entries replayed to that point --
//!   and becomes a format 3 revision with the same time, origin, transaction
//!   and binary, marked as imported from its format 2 number. A history that
//!   does not verify is never converted into one that seems to; `--new-lineage`
//!   begins format 3 history from the files instead.
//! - **Snapshots** become snapshot refs.
//! - **Schemas** are rewritten in the format 3 dialect: `primaryKey` and
//!   `filename` become `identity`. A pin keeps every rule it states. A working
//!   schema that only inference ever wrote is inferred again as the folder's
//!   observed contract; one a migration, a repair or a hand edit changed is
//!   converted as written, so no decision anyone made is dropped.
//! - **Configuration** loses the settings format 3 no longer has.
//!
//! Format 2's metadata is moved, not deleted, to `.db/format-2/`, and every
//! pin's original to `.db/format-2/pins/`. The new metadata is built apart in
//! `.db/upgrade/` and put in place step by step, each step checking whether
//! an interrupted run already took it; `.db/format` is written last, so until
//! then the database is format 2 and running `reldir upgrade` again resumes.

use crate::{
    code::Code,
    config::Config,
    diagnostic::{DbError, Diagnostic, Result},
    engine::fs::{Disk, Fs, Source, ensure_dir, replace_durably},
    history::{self, Delta, History, Recorded, RowDelta, State},
    store::{
        Hash256, Id, Layout,
        node::Entry,
        refs::SnapshotRef,
        revision::{Lineage, Origin},
    },
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// What an upgrade did.
#[derive(Debug, Default)]
pub struct Report {
    /// Format 2 revisions brought over.
    pub revisions: usize,
    /// Snapshots brought over.
    pub snapshots: Vec<String>,
    /// Tables whose inferred schema was inferred again as its contract.
    pub reinferred: Vec<String>,
    /// Tables whose working schema a decision had shaped, converted as written.
    pub converted: Vec<String>,
    /// Pins rewritten in the format 3 dialect.
    pub pins: Vec<String>,
    /// Where format 2's metadata now is.
    pub backup: PathBuf,
    /// Whether history began anew rather than being brought over.
    pub new_lineage: bool,
}

/// One path's entry in a format 2 state.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Entry2 {
    kind: String,
    hash: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Lineage2 {
    quarantined: String,
    reason: String,
}

/// One format 2 revision.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Provenance {
    revision: u64,
    timestamp: String,
    previous_revision: Option<u64>,
    previous_root_hash: Option<String>,
    new_root_hash: String,
    origin: String,
    changes: BTreeMap<String, Option<Entry2>>,
    binary_version: String,
    format_version: u32,
    #[serde(default)]
    transaction_id: Option<String>,
    #[serde(default)]
    lineage: Option<Lineage2>,
}

fn corrupt(message: impl Into<String>) -> DbError {
    DbError::from_diag(
        Diagnostic::error(Code::INTERNAL_METADATA_CORRUPT, message.into()).help(
            "format 2 history that does not verify is not brought over; `reldir upgrade \
             --new-lineage` keeps it aside and begins format 3 history from the files",
        ),
    )
}

fn io(path: &Path, error: std::io::Error) -> DbError {
    DbError::io(path, error)
}

/// What `.db/format` says, if anything.
fn format_version(root: &Path) -> Result<Option<u32>> {
    let path = root.join(".db/format");
    match std::fs::read_to_string(&path) {
        Ok(text) => text
            .trim()
            .strip_prefix("format_version = ")
            .and_then(|version| version.parse().ok())
            .map(Some)
            .ok_or_else(|| {
                DbError::new(
                    Code::FORMAT_UNSUPPORTED,
                    format!(".db/format holds {:?}, which names no format", text.trim()),
                )
            }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io(&path, error)),
    }
}

/// Upgrade the database at `root` from format 2.
pub fn upgrade(root: &Path, new_lineage: bool, wait: std::time::Duration) -> Result<Report> {
    let lock_path = root.join(".db/lock");
    let _lock = crate::lock::acquire(&Disk, &lock_path, wait, crate::lock::Holder::Committing)?;
    let workspace = root.join(".db/upgrade");
    match format_version(root)? {
        Some(2) => {}
        Some(3) => {
            if workspace.exists() {
                std::fs::remove_dir_all(&workspace).map_err(|error| io(&workspace, error))?;
            }
            return Err(DbError::new(
                Code::USAGE,
                "the database is already format 3; there is nothing to upgrade",
            ));
        }
        Some(other) => {
            return Err(DbError::new(
                Code::FORMAT_UNSUPPORTED,
                format!(
                    "format {other} is not one `reldir upgrade` converts; it converts format 2"
                ),
            ));
        }
        None => {
            return Err(DbError::new(
                Code::FORMAT_MISSING,
                ".db has no format marker, so what to convert from is unknown",
            ));
        }
    }
    let ready = workspace.join("READY");
    let report = if ready.exists() {
        serde_json::from_slice::<Value>(&std::fs::read(&ready).map_err(|error| io(&ready, error))?)
            .ok()
            .and_then(|record| report_from(&record))
            .ok_or_else(|| corrupt("the upgrade's own progress record does not read back"))?
    } else {
        // A build that fails leaves nothing behind: the database is the
        // format 2 database it was.
        let report = match build(root, &workspace, new_lineage) {
            Ok(report) => report,
            Err(error) => {
                if workspace.exists() {
                    std::fs::remove_dir_all(&workspace)
                        .map_err(|removal| io(&workspace, removal))?;
                }
                return Err(error);
            }
        };
        Disk.write(&ready, report_json(&report).to_string().as_bytes())
            .map_err(|error| io(&ready, error))?;
        sync_tree(&workspace)?;
        report
    };
    swap(root, &workspace, &report)?;
    Ok(report)
}

fn report_json(report: &Report) -> Value {
    json!({
        "revisions": report.revisions,
        "snapshots": report.snapshots,
        "reinferred": report.reinferred,
        "converted": report.converted,
        "pins": report.pins,
        "new_lineage": report.new_lineage,
    })
}

fn report_from(record: &Value) -> Option<Report> {
    let strings =
        |key: &str| -> Option<Vec<String>> { serde_json::from_value(record[key].clone()).ok() };
    Some(Report {
        revisions: record["revisions"].as_u64()? as usize,
        snapshots: strings("snapshots")?,
        reinferred: strings("reinferred")?,
        converted: strings("converted")?,
        pins: strings("pins")?,
        backup: PathBuf::from(".db/format-2"),
        new_lineage: record["new_lineage"].as_bool()?,
    })
}

/// Everything format 3 needs, built in the workspace; nothing outside it is
/// touched.
fn build(root: &Path, workspace: &Path, new_lineage: bool) -> Result<Report> {
    let transactions = root.join(".db/transactions");
    if std::fs::read_dir(&transactions).is_ok_and(|mut entries| entries.next().is_some()) {
        return Err(DbError::from_diag(
            Diagnostic::error(
                Code::TRANSACTION_INCOMPLETE,
                "a format 2 transaction was interrupted and is not finished",
            )
            .at(".db/transactions")
            .help("finish it with the reldir that started it (`reldir recover`), then upgrade"),
        ));
    }
    // Everything is read and verified before anything is written.
    let records = if new_lineage {
        vec![]
    } else {
        verified_history(root)?
    };
    if workspace.exists() {
        std::fs::remove_dir_all(workspace).map_err(|error| io(workspace, error))?;
    }
    let scratch = workspace.join("root");
    for directory in [
        workspace.to_path_buf(),
        scratch.clone(),
        scratch.join(".db"),
    ] {
        ensure_dir(&Disk, &directory).map_err(|error| io(&directory, error))?;
    }
    History::init(&scratch)?;
    let pinned: BTreeSet<String> = list_json(&root.join("schema"))?
        .into_iter()
        .map(|(table, _)| table)
        .collect();
    let mut report = Report {
        new_lineage,
        backup: PathBuf::from(".db/format-2"),
        ..Report::default()
    };

    import_history(root, &scratch, &records, &pinned)?;
    report.revisions = records.len();

    for (name, directory) in list_dirs(&root.join(".db/snapshots"))? {
        import_snapshot(&scratch, &name, &directory, &pinned)?;
        report.snapshots.push(name);
    }

    // The schemas of the state as it is now.
    let converted_pins = workspace.join("pins");
    ensure_dir(&Disk, &converted_pins).map_err(|error| io(&converted_pins, error))?;
    for (table, path) in list_json(&root.join("schema"))? {
        let bytes = std::fs::read(&path).map_err(|error| io(&path, error))?;
        let document = convert_schema(&bytes, &path)?;
        let target = converted_pins.join(format!("{table}.json"));
        Disk.write(&target, &render(&document)?)
            .map_err(|error| io(&target, error))?;
        report.pins.push(table);
    }
    let config = convert_config(root)?;
    let config_path = workspace.join("config");
    Disk.write(&config_path, &config)
        .map_err(|error| io(&config_path, error))?;
    let config: Config = serde_json::from_slice(&config)
        .map_err(|error| DbError::new(Code::CONFIG_INVALID, format!(".db/config: {error}")))?;
    let working = workspace.join("schema");
    ensure_dir(&Disk, &working).map_err(|error| io(&working, error))?;
    for (table, path) in list_json(&root.join(".db/schema"))? {
        if pinned.contains(&table) {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|error| io(&path, error))?;
        let document = convert_schema(&bytes, &path)?;
        let only_inferred = !new_lineage && inferred_only(&records, &table);
        let output = if only_inferred {
            let identity = document["x-reldir"]["identity"]["columns"]
                .as_array()
                .map(|columns| {
                    columns
                        .iter()
                        .filter_map(|column| column.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let inferred = crate::infer::infer_all(
                root,
                std::slice::from_ref(&table),
                crate::infer::Options {
                    tighten: crate::infer::Tighten::default(),
                    key: Some(&identity),
                },
                &config,
                None,
            )?;
            report.reinferred.push(table.clone());
            inferred[&table].render(None)?
        } else {
            report.converted.push(table.clone());
            render(&document)?
        };
        let target = working.join(format!("{table}.json"));
        Disk.write(&target, &output)
            .map_err(|error| io(&target, error))?;
    }
    Ok(report)
}

/// Whether only adoption ever wrote a table's working schema: every format 2
/// revision that changed it is the one that imported the folder.
fn inferred_only(records: &[Provenance], table: &str) -> bool {
    let key = format!("schema/{table}.json");
    records
        .iter()
        .filter(|record| record.changes.contains_key(&key))
        .all(|record| record.origin == "import")
}

/// The `<name>.json` files of a directory, by stem; none when it is absent.
fn list_json(directory: &Path) -> Result<Vec<(String, PathBuf)>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(io(directory, error)),
    };
    let mut out = vec![];
    for entry in entries {
        let path = entry.map_err(|error| io(directory, error))?.path();
        if let Some(stem) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".json"))
        {
            out.push((stem.to_string(), path.clone()));
        }
    }
    out.sort();
    Ok(out)
}

fn list_dirs(directory: &Path) -> Result<Vec<(String, PathBuf)>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(io(directory, error)),
    };
    let mut out = vec![];
    for entry in entries {
        let entry = entry.map_err(|error| io(directory, error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // A snapshot format 2 was still copying is no snapshot.
        if entry.path().is_dir() && !name.starts_with('.') {
            out.push((name, entry.path()));
        }
    }
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Format 2's history, read and verified as format 2 verified it

/// Format 2's normalization: members sorted and NFC-named, strings NFC, and
/// a zero `f64` -- of either sign -- written as the integer `0`.
fn normalize2(value: &Value) -> Value {
    use unicode_normalization::UnicodeNormalization;
    match value {
        Value::Object(members) => {
            let mut entries: Vec<(String, &Value)> = members
                .iter()
                .map(|(key, value)| (key.nfc().collect::<String>(), value))
                .collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, normalize2(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize2).collect()),
        Value::String(text) => Value::String(text.nfc().collect()),
        Value::Number(number) if number.is_f64() && number.as_f64() == Some(-0.0) => {
            Value::Number(0.into())
        }
        other => other.clone(),
    }
}

fn compact2(value: &Value) -> String {
    serde_json::to_string(&normalize2(value)).expect("JSON values serialize")
}

fn sha256(bytes: &[u8]) -> String {
    crate::canonical::hash_bytes(bytes)
}

/// The hash format 2 gave an object of a kind.
fn object_hash2(kind: &str, value: &Value) -> Result<String> {
    Ok(match kind {
        "format" => sha256(
            format!(
                "{}\n",
                value
                    .as_str()
                    .ok_or_else(|| corrupt("a format 2 format object is not a string"))?
                    .trim()
            )
            .as_bytes(),
        ),
        "schema" => crate::schema::identity::identity(value),
        _ => sha256(compact2(value).as_bytes()),
    })
}

/// Format 2's root over a state's entries.
fn root2(entries: &BTreeMap<String, Entry2>) -> String {
    use sha2::{Digest, Sha256};
    type Grouped<'a> = BTreeMap<String, (Option<&'a str>, Vec<(&'a str, &'a str)>)>;
    let mut tables: Grouped<'_> = BTreeMap::new();
    for (path, entry) in entries {
        match entry.kind.as_str() {
            "schema" => {
                if let Some(table) = path
                    .strip_prefix("schema/")
                    .and_then(|rest| rest.strip_suffix(".json"))
                {
                    tables.entry(table.to_string()).or_default().0 = Some(&entry.hash);
                }
            }
            "row" => {
                if let Some((table, _)) = path.split_once('/') {
                    tables
                        .entry(table.to_string())
                        .or_default()
                        .1
                        .push((path, &entry.hash));
                }
            }
            _ => {}
        }
    }
    let mut hasher = Sha256::new();
    hasher.update(b"reldir-state-v2\0");
    for label in ["format", "config"] {
        hasher.update(label.as_bytes());
        hasher.update([0]);
        hasher.update(
            entries
                .get(&format!(".db/{label}"))
                .map_or("", |entry| entry.hash.as_str())
                .as_bytes(),
        );
        hasher.update(b"\n");
    }
    for (table, (identity, rows)) in tables {
        let mut digest = Sha256::new();
        for (path, hash) in rows {
            digest.update(path.as_bytes());
            digest.update([0]);
            digest.update(hash.as_bytes());
            digest.update(b"\n");
        }
        hasher.update(table.as_bytes());
        hasher.update([0]);
        hasher.update(identity.unwrap_or("").as_bytes());
        hasher.update([0]);
        hasher.update(hex::encode(digest.finalize()).as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

fn expected_kind(path: &str) -> &'static str {
    if path == ".db/format" {
        "format"
    } else if path == ".db/config" {
        "config"
    } else if path.starts_with("schema/") && path.ends_with(".json") {
        "schema"
    } else {
        "row"
    }
}

/// A format 2 object: its value, and the bytes it is kept as.
fn object2(root: &Path, entry: &Entry2) -> Result<(Value, Vec<u8>)> {
    if entry.hash.len() != 64 || !entry.hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(corrupt(format!(
            "{:?} is not a format 2 object hash",
            entry.hash
        )));
    }
    let path = root.join(format!(".db/objects/{}.json", entry.hash));
    let bytes = std::fs::read(&path).map_err(|error| {
        corrupt(format!(
            "format 2 object {} cannot be read: {error}",
            path.display()
        ))
    })?;
    let value = crate::json::parse(&bytes).map_err(|error| {
        corrupt(format!(
            "format 2 object {} is not JSON: {error}",
            path.display()
        ))
    })?;
    Ok((value, bytes))
}

/// Every format 2 revision, in order, each checked.
fn verified_history(root: &Path) -> Result<Vec<Provenance>> {
    let directory = root.join(".db/provenance");
    let mut numbered = vec![];
    for (stem, path) in list_json(&directory)? {
        let number: u64 = stem
            .parse()
            .map_err(|_| corrupt(format!("{} is not a format 2 revision", path.display())))?;
        numbered.push((number, path));
    }
    numbered.sort();
    let mut entries: BTreeMap<String, Entry2> = BTreeMap::new();
    let mut records: Vec<Provenance> = vec![];
    for (number, path) in numbered {
        let bytes = std::fs::read(&path).map_err(|error| io(&path, error))?;
        let record: Provenance = serde_json::from_slice(&bytes).map_err(|error| {
            corrupt(format!("format 2 revision {number} does not read: {error}"))
        })?;
        if record.revision != number || record.format_version != 2 {
            return Err(corrupt(format!(
                "{} holds revision {} of format {}",
                path.display(),
                record.revision,
                record.format_version
            )));
        }
        match records.last() {
            None if record.previous_revision.is_some() || record.previous_root_hash.is_some() => {
                return Err(corrupt(format!(
                    "format 2 history begins at revision {number}, which claims a predecessor"
                )));
            }
            Some(previous)
                if record.revision != previous.revision + 1
                    || record.previous_revision != Some(previous.revision)
                    || record.previous_root_hash.as_deref()
                        != Some(previous.new_root_hash.as_str()) =>
            {
                return Err(corrupt(format!(
                    "format 2 history is broken at revision {number}"
                )));
            }
            _ => {}
        }
        for (path, change) in &record.changes {
            match change {
                Some(entry) => {
                    if entry.kind != expected_kind(path) {
                        return Err(corrupt(format!(
                            "format 2 revision {number} records {path} as a {}",
                            entry.kind
                        )));
                    }
                    let (value, _) = object2(root, entry)?;
                    if object_hash2(&entry.kind, &value)? != entry.hash {
                        return Err(corrupt(format!(
                            "format 2 object {} does not hash to its entry",
                            entry.hash
                        )));
                    }
                    entries.insert(path.clone(), entry.clone());
                }
                None => {
                    entries.remove(path);
                }
            }
        }
        if root2(&entries) != record.new_root_hash {
            return Err(corrupt(format!(
                "format 2 revision {number} records a root its changes do not produce"
            )));
        }
        records.push(record);
    }
    Ok(records)
}

// ---------------------------------------------------------------------------
// Format 3, built from what format 2 recorded

fn origin3(origin: &str) -> Result<Origin> {
    Ok(match origin {
        // Format 2 said "internal" of every change made through reldir.
        "internal" => Origin::User,
        "external" => Origin::External,
        "recovery" => Origin::Recovery,
        "repair" => Origin::Repair,
        "migration" => Origin::Migration,
        "import" => Origin::Inference,
        "snapshot_restore" => Origin::SnapshotRestore,
        other => {
            return Err(corrupt(format!(
                "format 2 origin {other:?} is not one it had"
            )));
        }
    })
}

/// Where a table's schema is recorded: its pin if it has one now, its working
/// schema otherwise. Format 2 named both `schema/<table>.json`.
fn schema_home(table: &str, pinned: &BTreeSet<String>) -> String {
    if pinned.contains(table) {
        crate::schema_store::pin_relative(table)
    } else {
        crate::catalog::slash(&crate::schema_store::working_relative(table))
    }
}

fn hash256(text: &str) -> Result<Hash256> {
    Hash256::from_hex(text).ok_or_else(|| corrupt(format!("{text:?} is not a hash")))
}

/// A format 2 entry as format 3 records it.
enum Converted {
    Format(Recorded),
    Config(Recorded),
    Schema(String, Recorded),
    Row(Entry, Vec<u8>),
}

fn convert_entry(
    root: &Path,
    path: &str,
    entry: &Entry2,
    pinned: &BTreeSet<String>,
) -> Result<Converted> {
    let (value, bytes) = object2(root, entry)?;
    Ok(match entry.kind.as_str() {
        "format" => {
            let text = value
                .as_str()
                .ok_or_else(|| corrupt("a format 2 format object is not a string"))?;
            let bytes = format!("{}\n", text.trim()).into_bytes();
            Converted::Format(Recorded {
                path: ".db/format".into(),
                hash: history::format_hash(text),
                bytes,
            })
        }
        "config" => {
            let bytes = convert_config_value(&value)?;
            Converted::Config(Recorded {
                path: ".db/config".into(),
                hash: history::config_hash(&bytes)?,
                bytes,
            })
        }
        "schema" => {
            let table = path
                .strip_prefix("schema/")
                .and_then(|rest| rest.strip_suffix(".json"))
                .ok_or_else(|| corrupt(format!("{path} names no table")))?
                .to_string();
            let document = convert_schema_value(&value, Path::new(path))?;
            let schema = crate::schema::Schema::from_document(document.clone(), None).map_err(
                |problems| {
                    corrupt(format!(
                        "format 2's schema of {table} does not load as format 3: {}",
                        problems
                            .first()
                            .map(|problem| problem.message.clone())
                            .unwrap_or_default()
                    ))
                },
            )?;
            Converted::Schema(
                table.clone(),
                Recorded {
                    path: schema_home(&table, pinned),
                    hash: hash256(schema.identity())?,
                    bytes: render(&document)?,
                },
            )
        }
        _ => Converted::Row(
            Entry {
                path: path.to_string(),
                row_hash: hash256(&crate::canonical::row_hash(&value))?,
                blob: Id::of(&bytes),
            },
            bytes,
        ),
    })
}

/// Write a prepared revision's or a snapshot's files into the scratch root.
fn place(scratch: &Path, relative: &Path, bytes: &[u8]) -> Result<()> {
    let path = scratch.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
    }
    Disk.write(&path, bytes).map_err(|error| io(&path, error))
}

fn import_history(
    root: &Path,
    scratch: &Path,
    records: &[Provenance],
    pinned: &BTreeSet<String>,
) -> Result<()> {
    let mut state = State {
        format: None,
        config: None,
        tables: BTreeMap::new(),
        rows: vec![],
    };
    let mut rows: BTreeMap<String, Entry> = BTreeMap::new();
    for record in records {
        let mut delta = Delta::default();
        let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for (path, change) in &record.changes {
            match change {
                Some(entry) => match convert_entry(root, path, entry, pinned)? {
                    Converted::Format(recorded) => {
                        state.format = Some(recorded.clone());
                        delta.format = Some(Some(recorded));
                    }
                    Converted::Config(recorded) => {
                        state.config = Some(recorded.clone());
                        delta.config = Some(Some(recorded));
                    }
                    Converted::Schema(table, recorded) => {
                        state.tables.insert(table.clone(), recorded.clone());
                        delta.tables.insert(table, Some(recorded));
                    }
                    Converted::Row(row, bytes) => {
                        blobs.insert(row.path.clone(), bytes);
                        delta.rows.push(if rows.contains_key(path) {
                            RowDelta::Modified(row.clone())
                        } else {
                            RowDelta::Added(row.clone())
                        });
                        rows.insert(path.clone(), row);
                    }
                },
                None => match expected_kind(path) {
                    "format" => {
                        state.format = None;
                        delta.format = Some(None);
                    }
                    "config" => {
                        state.config = None;
                        delta.config = Some(None);
                    }
                    "schema" => {
                        let table = path
                            .strip_prefix("schema/")
                            .and_then(|rest| rest.strip_suffix(".json"))
                            .unwrap_or_default()
                            .to_string();
                        state.tables.remove(&table);
                        delta.tables.insert(table, None);
                    }
                    _ => {
                        rows.remove(path);
                        delta.rows.push(RowDelta::Removed(path.clone()));
                    }
                },
            }
        }
        state.rows = rows.values().cloned().collect();
        let history = History::open(scratch)?;
        let timestamp = chrono::DateTime::parse_from_rfc3339(&record.timestamp)
            .map_err(|error| {
                corrupt(format!(
                    "format 2 revision {} has no readable time: {error}",
                    record.revision
                ))
            })?
            .with_timezone(&chrono::Utc);
        let whole = state.clone();
        let prepared = history.prepare(
            &delta,
            &mut || Ok(whole.clone()),
            &mut |path| {
                blobs
                    .get(path)
                    .cloned()
                    .ok_or_else(|| corrupt(format!("no recorded bytes for {path}")))
            },
            history::Recording {
                origin: origin3(&record.origin)?,
                transaction_id: record.transaction_id.as_deref(),
                idempotency_key: None,
                lineage: record.lineage.as_ref().map(|lineage| Lineage {
                    quarantined: lineage.quarantined.clone(),
                    reason: lineage.reason.clone(),
                }),
                imported: Some(history::Imported {
                    revision: record.revision,
                    timestamp,
                    binary_version: &record.binary_version,
                }),
            },
        )?;
        for artifact in &prepared.artifacts {
            place(scratch, &artifact.to, &artifact.bytes)?;
        }
        for retired in &prepared.retire {
            let path = scratch.join(retired);
            std::fs::remove_file(&path).map_err(|error| io(&path, error))?;
        }
    }
    Ok(())
}

/// A format 2 snapshot -- a copy of the authoritative files -- as a format 3
/// snapshot ref.
fn import_snapshot(
    scratch: &Path,
    name: &str,
    directory: &Path,
    pinned: &BTreeSet<String>,
) -> Result<()> {
    crate::snapshot::validate_name(name)?;
    let read = |relative: &str| -> Result<Option<Vec<u8>>> {
        let path = directory.join(relative);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io(&path, error)),
        }
    };
    // A snapshot holds data, not a binary's format: restored, it must leave
    // the database the format it is in.
    let format = format!("format_version = {}\n", crate::FORMAT_VERSION);
    let mut state = State {
        format: read(".db/format")?.map(|_| Recorded {
            path: ".db/format".into(),
            hash: history::format_hash(&format),
            bytes: format.clone().into_bytes(),
        }),
        config: match read(".db/config")? {
            Some(bytes) => {
                let value = crate::json::parse(&bytes)
                    .map_err(|error| corrupt(format!("snapshot {name}'s config: {error}")))?;
                let bytes = convert_config_value(&value)?;
                Some(Recorded {
                    path: ".db/config".into(),
                    hash: history::config_hash(&bytes)?,
                    bytes,
                })
            }
            None => None,
        },
        tables: BTreeMap::new(),
        rows: vec![],
    };
    let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut extras: Vec<(String, Vec<u8>)> = vec![];
    for schemas in ["schema", ".db/schema"] {
        for (table, path) in list_json(&directory.join(schemas))? {
            let bytes = std::fs::read(&path).map_err(|error| io(&path, error))?;
            let document = convert_schema(&bytes, &path)?;
            let schema =
                crate::schema::Schema::from_document(document.clone(), None).map_err(|_| {
                    corrupt(format!(
                        "snapshot {name}'s schema of {table} does not load as format 3"
                    ))
                })?;
            state.tables.insert(
                table.clone(),
                Recorded {
                    path: schema_home(&table, pinned),
                    hash: hash256(schema.identity())?,
                    bytes: render(&document)?,
                },
            );
        }
    }
    for table in state.tables.keys().cloned().collect::<Vec<_>>() {
        for (stem, path) in list_json(&directory.join(&table))? {
            let relative = format!("{table}/{stem}.json");
            let bytes = std::fs::read(&path).map_err(|error| io(&path, error))?;
            match crate::json::parse(&bytes) {
                Ok(value) if value.is_object() => {
                    state.rows.push(Entry {
                        path: relative.clone(),
                        row_hash: hash256(&crate::canonical::row_hash(&value))?,
                        blob: Id::of(&bytes),
                    });
                    blobs.insert(relative, bytes);
                }
                _ => extras.push((relative, bytes)),
            }
        }
    }
    state.rows.sort();
    let history = History::open(scratch)?;
    let (built, pack) = history.build(&state, &extras, &mut |path| {
        blobs
            .get(path)
            .cloned()
            .ok_or_else(|| corrupt(format!("snapshot {name} has no bytes for {path}")))
    })?;
    if let Some((pack_bytes, idx_bytes, pack_name)) = pack {
        let packs = Path::new(".db/objects/pack");
        place(scratch, &packs.join(pack_name.pack_file()), &pack_bytes)?;
        place(scratch, &packs.join(pack_name.idx_file()), &idx_bytes)?;
    }
    let created = std::fs::metadata(directory)
        .and_then(|meta| meta.modified())
        .map(chrono::DateTime::<chrono::Utc>::from)
        .map_err(|error| io(directory, error))?;
    let snapshot = SnapshotRef {
        tree: built.root,
        state: built.identity,
        revision: None,
        created,
        valid: true,
    };
    place(
        scratch,
        &Path::new(".db/history/refs/snapshots").join(name),
        &snapshot.encode().map_err(history::store_error)?,
    )
}

// ---------------------------------------------------------------------------
// Schemas and configuration in format 3's terms

/// A format 2 schema document in the format 3 dialect: `primaryKey` and
/// `filename` become `identity` -- the file named by the filename columns,
/// under reldir's own encoding, as format 2 named it -- and a primary key
/// that was not also the file name stays a unique key.
fn convert_schema_value(document: &Value, path: &Path) -> Result<Value> {
    let mut document = document.clone();
    let object = document
        .as_object_mut()
        .ok_or_else(|| corrupt(format!("{} is not a schema document", path.display())))?;
    object.insert("$schema".into(), json!(crate::schema::meta::DIALECT_URI));
    let extension = object
        .get_mut("x-reldir")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| corrupt(format!("{} has no x-reldir", path.display())))?;
    let strings = |value: Option<Value>| -> Option<Vec<String>> {
        value.and_then(|value| serde_json::from_value(value).ok())
    };
    let primary = strings(extension.shift_remove("primaryKey"))
        .ok_or_else(|| corrupt(format!("{} names no primary key", path.display())))?;
    let filename = strings(extension.shift_remove("filename")).unwrap_or_else(|| primary.clone());
    if filename != primary {
        let unique = extension
            .entry("unique")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| corrupt(format!("{}'s unique keys are not a list", path.display())))?;
        if !unique.contains(&json!(primary)) {
            unique.push(json!(primary));
        }
    }
    let mut rebuilt = Map::new();
    for (key, value) in std::mem::take(extension) {
        rebuilt.insert(key.clone(), value);
        if key == "table" {
            rebuilt.insert(
                "identity".into(),
                json!({"source": "body", "columns": filename, "naming": {"rule": "reldir"}}),
            );
        }
    }
    *extension = rebuilt;
    Ok(document)
}

fn convert_schema(bytes: &[u8], path: &Path) -> Result<Value> {
    let value = crate::json::parse(bytes)
        .map_err(|error| corrupt(format!("{} is not JSON: {error}", path.display())))?;
    convert_schema_value(&value, path)
}

fn render(document: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(document)
        .map_err(|error| DbError::new(Code::INTERNAL_ERROR, error.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// The settings format 2 had and format 3 does not.
const RETIRED_SETTINGS: &[&str] = &["indentation_width"];

fn convert_config_value(value: &Value) -> Result<Vec<u8>> {
    let mut settings = value
        .as_object()
        .cloned()
        .ok_or_else(|| corrupt("format 2's .db/config is not an object"))?;
    for retired in RETIRED_SETTINGS {
        settings.shift_remove(*retired);
    }
    let settings = Value::Object(settings);
    serde_json::from_value::<Config>(settings.clone()).map_err(|error| {
        DbError::from_diag(
            Diagnostic::error(
                Code::CONFIG_INVALID,
                format!("format 2's .db/config does not read as format 3's: {error}"),
            )
            .at(".db/config"),
        )
    })?;
    render(&settings)
}

fn convert_config(root: &Path) -> Result<Vec<u8>> {
    let path = root.join(".db/config");
    match std::fs::read(&path) {
        Ok(bytes) => {
            let value = crate::json::parse(&bytes)
                .map_err(|error| corrupt(format!(".db/config is not JSON: {error}")))?;
            convert_config_value(&value)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            render(&serde_json::to_value(Config::default()).expect("settings serialize"))
        }
        Err(error) => Err(io(&path, error)),
    }
}

// ---------------------------------------------------------------------------
// Putting it in place

/// Make everything under a directory durable before anything depends on it.
fn sync_tree(directory: &Path) -> Result<()> {
    use crate::engine::fs::Durability;
    for entry in std::fs::read_dir(directory).map_err(|error| io(directory, error))? {
        let path = entry.map_err(|error| io(directory, error))?.path();
        let meta = Disk.metadata(&path).map_err(|error| io(&path, error))?;
        if meta.kind == crate::engine::fs::Kind::Dir {
            sync_tree(&path)?;
        } else {
            Disk.sync_file(&path, Durability::Full)
                .map_err(|error| io(&path, error))?;
        }
    }
    Disk.sync_dir(directory)
        .map_err(|error| io(directory, error))
}

/// Move `from` to `to` unless an interrupted run already did.
fn move_once(from: &Path, to: &Path) -> Result<()> {
    if !from.exists() {
        return Ok(());
    }
    if to.exists() {
        return Err(corrupt(format!(
            "both {} and {} exist; an earlier upgrade was interrupted in a way it cannot resume \
             from -- restore .db from a backup",
            from.display(),
            to.display()
        )));
    }
    Disk.rename(from, to).map_err(|error| io(from, error))?;
    if let Some(parent) = to.parent() {
        Disk.sync_dir(parent).map_err(|error| io(parent, error))?;
    }
    if let Some(parent) = from.parent() {
        Disk.sync_dir(parent).map_err(|error| io(parent, error))?;
    }
    Ok(())
}

fn swap(root: &Path, workspace: &Path, report: &Report) -> Result<()> {
    let db = root.join(".db");
    let backup = db.join("format-2");
    ensure_dir(&Disk, &backup).map_err(|error| io(&backup, error))?;
    // Format 2's metadata, set aside.
    let tracked = std::fs::read_to_string(db.join(".gitignore"))
        .is_ok_and(|text| text.contains("provenance"));
    for name in [
        "provenance",
        "objects",
        "snapshots",
        "provenance-quarantine",
        "schema",
        "config",
        "mirror.sqlite",
        "mirror.sqlite-journal",
        "mirror.sqlite-wal",
        "mirror.sqlite-shm",
        "transactions",
        ".gitignore",
    ] {
        let from = db.join(name);
        let to = backup.join(name);
        // `objects` and `schema` are format 3 names too: once format 3's are
        // in place, the ones at the source are not format 2's.
        if to.exists() && matches!(name, "objects" | "schema" | "config" | ".gitignore") {
            continue;
        }
        move_once(&from, &to)?;
    }
    // Pins, rewritten in place, their originals kept.
    let originals = backup.join("pins");
    ensure_dir(&Disk, &originals).map_err(|error| io(&originals, error))?;
    for table in &report.pins {
        let pin = root.join(crate::schema_store::pin_relative(table));
        let original = originals.join(format!("{table}.json"));
        if !original.exists() {
            let bytes = std::fs::read(&pin).map_err(|error| io(&pin, error))?;
            replace_durably(&Disk, &original, &bytes).map_err(|error| io(&original, error))?;
        }
        let converted = workspace.join("pins").join(format!("{table}.json"));
        if converted.exists() {
            let bytes = std::fs::read(&converted).map_err(|error| io(&converted, error))?;
            replace_durably(&Disk, &pin, &bytes).map_err(|error| io(&pin, error))?;
            std::fs::remove_file(&converted).map_err(|error| io(&converted, error))?;
        }
    }
    // Format 3's metadata, in place.
    let scratch = Layout::new(workspace.join("root"));
    move_once(&scratch.objects(), &db.join("objects"))?;
    move_once(&scratch.history(), &db.join("history"))?;
    move_once(&workspace.join("schema"), &db.join("schema"))?;
    move_once(&workspace.join("config"), &db.join("config"))?;
    ensure_dir(&Disk, &db.join("transactions")).map_err(|error| io(&db, error))?;
    replace_durably(
        &Disk,
        &db.join(".gitignore"),
        crate::db::gitignore(tracked).as_bytes(),
    )
    .map_err(|error| io(&db, error))?;
    // The one step that makes it format 3, taken last.
    replace_durably(
        &Disk,
        &db.join("format"),
        format!("format_version = {}\n", crate::FORMAT_VERSION).as_bytes(),
    )
    .map_err(|error| io(&db, error))?;
    std::fs::remove_dir_all(workspace).map_err(|error| io(workspace, error))?;
    Ok(())
}
