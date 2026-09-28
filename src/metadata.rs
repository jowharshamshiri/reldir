//! Recorded history: how the database came to be what it is.
//!
//! Each accepted state has a *root hash* over everything that decides the
//! logical database: the format, the configuration, each table's schema
//! identity, and each row's canonical hash. The root is built per table -- a
//! table digest over its rows, then a root over the tables -- and table digests
//! are cached in the mirror, so recognising that nothing changed costs one read
//! per table.
//!
//! Each accepted transition is a *provenance record* in `.db/provenance/`,
//! numbered from 1, naming its predecessor's root and its own, and carrying
//! only the entries that changed. The content of every entry a record adds is
//! kept, content-addressed, in `.db/objects/`, so any recorded state can be
//! reconstructed and any deleted row restored exactly.
//!
//! Provenance is not validity: a state can be valid without having been
//! recorded, and history never claims to know who made an external change.

use crate::{
    FORMAT_VERSION, VERSION, canonical,
    catalog::Catalog,
    diagnostic::{DbError, Result},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

/// What one path contributes to a state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// `format`, `config`, `schema`, or `row`.
    pub kind: String,
    pub hash: String,
}

/// Where a lineage began, when it began over a quarantined history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Lineage {
    /// The directory the previous history was moved to.
    pub quarantined: String,
    /// Why a new lineage began.
    pub reason: String,
}

/// One accepted transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub revision: u64,
    pub timestamp: String,
    pub previous_revision: Option<u64>,
    pub previous_root_hash: Option<String>,
    pub new_root_hash: String,
    /// `internal`, `external`, `recovery`, `repair`, `migration`, `import`,
    /// or `snapshot_restore`.
    pub origin: String,
    /// Entries added or changed (`Some`) and removed (`None`).
    pub changes: BTreeMap<String, Option<Entry>>,
    pub binary_version: String,
    pub format_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage: Option<Lineage>,
}

impl Provenance {
    /// The changes, spelled `A path`, `M path`, `D path` against the state
    /// before, for reports.
    pub fn summary(&self, before: &BTreeMap<String, Entry>) -> Vec<String> {
        self.changes
            .iter()
            .map(|(path, entry)| match (before.contains_key(path), entry) {
                (_, None) => format!("D {path}"),
                (false, Some(_)) => format!("A {path}"),
                (true, Some(_)) => format!("M {path}"),
            })
            .collect()
    }
}

pub const ORIGINS: &[&str] = &[
    "internal",
    "external",
    "recovery",
    "repair",
    "migration",
    "import",
    "snapshot_restore",
];

fn corrupt(message: impl Into<String>) -> DbError {
    DbError::new("INTERNAL_METADATA_CORRUPT", message, 6)
}

fn internal(e: serde_json::Error) -> DbError {
    corrupt(e.to_string())
}

/// The hash `.db/format` contributes: of its normalized text.
pub fn format_entry(root: &Path) -> Result<Option<Entry>> {
    let path = root.join(".db/format");
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(Entry {
            kind: "format".into(),
            hash: canonical::hash_bytes(format!("{}\n", text.trim()).as_bytes()),
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(DbError::io(&path, error)),
    }
}

/// The hash `.db/config` contributes: of its normalized JSON.
pub fn config_entry(root: &Path) -> Result<Option<Entry>> {
    let path = root.join(".db/config");
    match fs::read(&path) {
        Ok(bytes) => {
            let value = crate::json::parse(&bytes).map_err(|e| corrupt(format!(".db/config: {e}")))?;
            let normalized = serde_json::to_vec(&canonical::normalize(&value)).map_err(internal)?;
            Ok(Some(Entry {
                kind: "config".into(),
                hash: canonical::hash_bytes(&normalized),
            }))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(DbError::io(&path, error)),
    }
}

/// A table's digest over its rows.
pub fn table_digest<'a>(rows: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut hasher = Sha256::new();
    for (path, hash) in rows {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(hash.as_bytes());
        hasher.update([b'\n']);
    }
    hex::encode(hasher.finalize())
}

/// The root over a state's parts.
pub fn root_hash(
    format: Option<&Entry>,
    config: Option<&Entry>,
    tables: &BTreeMap<String, (Option<String>, String)>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("reldir-state-v{FORMAT_VERSION}\0").as_bytes());
    for (label, entry) in [("format", format), ("config", config)] {
        hasher.update(label.as_bytes());
        hasher.update([0]);
        hasher.update(entry.map_or("", |e| e.hash.as_str()).as_bytes());
        hasher.update([b'\n']);
    }
    for (table, (identity, digest)) in tables {
        hasher.update(table.as_bytes());
        hasher.update([0]);
        hasher.update(identity.as_deref().unwrap_or("").as_bytes());
        hasher.update([0]);
        hasher.update(digest.as_bytes());
        hasher.update([b'\n']);
    }
    hex::encode(hasher.finalize())
}

/// The root of a set of entries, however they were obtained. The same function
/// the live state's root is built from, applied to a reconstructed history.
pub fn root_of(entries: &BTreeMap<String, Entry>) -> String {
    let mut tables: BTreeMap<String, (Option<String>, Vec<(&str, &str)>)> = BTreeMap::new();
    for (path, entry) in entries {
        match entry.kind.as_str() {
            "schema" => {
                if let Some(table) = path.strip_prefix("schema/").and_then(|p| p.strip_suffix(".json")) {
                    tables.entry(table.to_string()).or_default().0 = Some(entry.hash.clone());
                }
            }
            "row" => {
                if let Some((table, _)) = path.split_once('/') {
                    tables
                        .entry(table.to_string())
                        .or_default()
                        .1
                        .push((path.as_str(), entry.hash.as_str()));
                }
            }
            _ => {}
        }
    }
    let digests = tables
        .into_iter()
        .map(|(table, (identity, rows))| (table, (identity, table_digest(rows))))
        .collect();
    root_hash(entries.get(".db/format"), entries.get(".db/config"), &digests)
}

/// The live state: its root, and -- on request -- its entries.
pub fn live_root(catalog: &Catalog) -> Result<String> {
    let format = format_entry(&catalog.root)?;
    let config = config_entry(&catalog.root)?;
    let mut tables = BTreeMap::new();
    for (table, schema) in &catalog.schemas {
        tables.insert(
            table.clone(),
            (Some(schema.identity().to_string()), catalog.mirror.table_digest(table)?),
        );
    }
    Ok(root_hash(format.as_ref(), config.as_ref(), &tables))
}

/// Every entry of the live state.
pub fn live_entries(catalog: &Catalog) -> Result<BTreeMap<String, Entry>> {
    let mut entries = BTreeMap::new();
    if let Some(format) = format_entry(&catalog.root)? {
        entries.insert(".db/format".into(), format);
    }
    if let Some(config) = config_entry(&catalog.root)? {
        entries.insert(".db/config".into(), config);
    }
    for (table, schema) in &catalog.schemas {
        entries.insert(
            crate::schema_store::pin_relative(table),
            Entry {
                kind: "schema".into(),
                hash: schema.identity().to_string(),
            },
        );
        for (path, hash) in catalog.mirror.row_hashes(table)? {
            if let Some(hash) = hash {
                entries.insert(path, Entry { kind: "row".into(), hash });
            }
        }
    }
    Ok(entries)
}

fn provenance_dir(root: &Path) -> PathBuf {
    root.join(".db/provenance")
}

fn record_path(root: &Path, revision: u64) -> PathBuf {
    provenance_dir(root).join(format!("{revision:020}.json"))
}

/// The revisions recorded, in order.
pub fn revisions(root: &Path) -> Result<Vec<u64>> {
    let dir = provenance_dir(root);
    if !ensure_real_directory(&dir, false, "provenance")? {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for entry in fs::read_dir(&dir).map_err(|e| DbError::io(&dir, e))? {
        let path = entry.map_err(|e| DbError::io(&dir, e))?.path();
        if is_in_progress_write(&path) {
            continue;
        }
        let revision = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".json"))
            .filter(|stem| stem.len() == 20)
            .and_then(|stem| stem.parse::<u64>().ok())
            .ok_or_else(|| corrupt(format!("unexpected provenance entry {}", path.display())))?;
        out.push(revision);
    }
    out.sort_unstable();
    Ok(out)
}

pub fn load_record(root: &Path, revision: u64) -> Result<Provenance> {
    let path = record_path(root, revision);
    let metadata = fs::symlink_metadata(&path).map_err(|e| DbError::io(&path, e))?;
    if !metadata.file_type().is_file() || crate::catalog::has_multiple_links(&metadata) {
        return Err(corrupt(format!("provenance {} is not a private regular file", path.display())));
    }
    let record: Provenance = crate::json::parse_as(&fs::read(&path).map_err(|e| DbError::io(&path, e))?)
        .map_err(|e| corrupt(format!("invalid provenance {}: {e}", path.display())))?;
    if record.revision != revision {
        return Err(corrupt(format!("provenance {} records revision {}", path.display(), record.revision)));
    }
    if record.format_version != FORMAT_VERSION {
        return Err(DbError::new(
            "FORMAT_UNSUPPORTED",
            format!(
                "provenance revision {revision} is format {}; this binary reads only format {FORMAT_VERSION}",
                record.format_version
            ),
            6,
        ));
    }
    if !ORIGINS.contains(&record.origin.as_str()) {
        return Err(corrupt(format!("provenance revision {revision} has unknown origin {:?}", record.origin)));
    }
    Ok(record)
}

/// The latest record, if any.
pub fn head(root: &Path) -> Result<Option<Provenance>> {
    match revisions(root)?.last() {
        Some(revision) => load_record(root, *revision).map(Some),
        None => Ok(None),
    }
}

/// Replay the whole history, verifying that each record continues its
/// predecessor, that each recorded root is the root of the entries it
/// describes, and that every object it references is present and intact.
/// Returns the entries of the head state.
pub fn verify_history(root: &Path) -> Result<BTreeMap<String, Entry>> {
    let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
    let mut prior: Option<Provenance> = None;
    for revision in revisions(root)? {
        let record = load_record(root, revision)?;
        match &prior {
            None if record.previous_revision.is_some() || record.previous_root_hash.is_some() => {
                return Err(corrupt(format!(
                    "provenance begins at revision {revision}, which claims a predecessor"
                )));
            }
            Some(previous)
                if record.revision != previous.revision + 1
                    || record.previous_revision != Some(previous.revision)
                    || record.previous_root_hash.as_deref() != Some(previous.new_root_hash.as_str()) =>
            {
                return Err(corrupt(format!("provenance is broken at revision {revision}")));
            }
            _ => {}
        }
        for (path, entry) in &record.changes {
            match entry {
                Some(entry) => {
                    verify_object(root, path, entry)?;
                    entries.insert(path.clone(), entry.clone());
                }
                None => {
                    entries.remove(path);
                }
            }
        }
        if root_of(&entries) != record.new_root_hash {
            return Err(corrupt(format!(
                "provenance revision {revision} records a root its changes do not produce"
            )));
        }
        prior = Some(record);
    }
    Ok(entries)
}

/// The entries of the state a revision recorded, replayed from the first.
pub fn entries_at(root: &Path, revision: u64) -> Result<BTreeMap<String, Entry>> {
    let revisions = revisions(root)?;
    if !revisions.contains(&revision) {
        return Err(DbError::new("UNKNOWN_REVISION", format!("there is no revision {revision}"), 4).with_help(
            match revisions.last() {
                Some(last) => format!("revisions run from {} to {last}; `reldir log` lists them", revisions[0]),
                None => "nothing has been recorded yet".into(),
            },
        ));
    }
    let mut entries = BTreeMap::new();
    for number in revisions.into_iter().take_while(|number| *number <= revision) {
        for (path, entry) in load_record(root, number)?.changes {
            match entry {
                Some(entry) => {
                    entries.insert(path, entry);
                }
                None => {
                    entries.remove(&path);
                }
            }
        }
    }
    Ok(entries)
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

fn object_path(root: &Path, hash: &str) -> PathBuf {
    root.join(format!(".db/objects/{hash}.json"))
}

/// Check one recorded object against its entry.
pub fn verify_object(root: &Path, path: &str, entry: &Entry) -> Result<()> {
    if entry.hash.len() != 64 || !entry.hash.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return Err(corrupt(format!("invalid object hash {:?} for {path}", entry.hash)));
    }
    let expected = expected_kind(path);
    if entry.kind != expected {
        return Err(corrupt(format!("object {path} has kind {:?}, expected {expected:?}", entry.kind)));
    }
    let value = load_object(root, &entry.hash)?;
    if object_hash(&entry.kind, &value)? != entry.hash {
        return Err(corrupt(format!("revision object {} does not match its hash", entry.hash)));
    }
    Ok(())
}

/// The hash an object of a kind has.
pub fn object_hash(kind: &str, value: &Value) -> Result<String> {
    Ok(match kind {
        "format" => canonical::hash_bytes(
            format!("{}\n", value.as_str().ok_or_else(|| corrupt("a format object is a string"))?.trim()).as_bytes(),
        ),
        "schema" => crate::schema::identity::identity(value),
        "row" => canonical::row_hash(value),
        _ => canonical::hash_bytes(&serde_json::to_vec(&canonical::normalize(value)).map_err(internal)?),
    })
}

pub fn load_object(root: &Path, hash: &str) -> Result<Value> {
    let path = object_path(root, hash);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| corrupt(format!("missing revision object {}: {error}", path.display())))?;
    if !metadata.file_type().is_file() || crate::catalog::has_multiple_links(&metadata) {
        return Err(corrupt(format!("revision object {} is not a private regular file", path.display())));
    }
    crate::json::parse(&fs::read(&path).map_err(|e| DbError::io(&path, e))?)
        .map_err(|error| corrupt(format!("invalid revision object {}: {error}", path.display())))
}

/// The content a path last had in recorded history, and the revision that
/// recorded it: what doctor restores when a row was removed by mistake.
pub fn last_known(root: &Path, path: &str) -> Result<Option<(u64, Value)>> {
    for revision in revisions(root)?.into_iter().rev() {
        let record = load_record(root, revision)?;
        match record.changes.get(path) {
            Some(Some(entry)) => return Ok(Some((revision, load_object(root, &entry.hash)?))),
            Some(None) => continue,
            None => continue,
        }
    }
    Ok(None)
}

/// The content of each changed entry, as its object.
fn object_value(catalog: &Catalog, path: &str, entry: &Entry) -> Result<Value> {
    match entry.kind.as_str() {
        "format" => Ok(Value::String(
            fs::read_to_string(catalog.root.join(".db/format"))
                .map_err(|e| DbError::io(&catalog.root.join(".db/format"), e))?
                .trim()
                .to_string(),
        )),
        "config" => {
            let path = catalog.root.join(".db/config");
            let value = crate::json::parse(&fs::read(&path).map_err(|e| DbError::io(&path, e))?)
                .map_err(|e| corrupt(e.to_string()))?;
            Ok(canonical::normalize(&value))
        }
        "schema" => {
            let table = path
                .strip_prefix("schema/")
                .and_then(|p| p.strip_suffix(".json"))
                .ok_or_else(|| corrupt(format!("schema entry {path}")))?;
            Ok(catalog
                .schemas
                .get(table)
                .ok_or_else(|| corrupt(format!("no schema for {table}")))?
                .document()
                .clone())
        }
        _ => {
            let row = catalog
                .row_at(Path::new(path))?
                .ok_or_else(|| corrupt(format!("no row at {path}")))?;
            let schema = &catalog.schemas[&row.table];
            Ok(canonical::canonical_row(&row.value, schema))
        }
    }
}

/// What differs between the live state and the recorded head, entry by entry.
/// Rows are compared inside the mirror, so the cost follows what changed.
pub fn pending_changes(catalog: &Catalog) -> Result<BTreeMap<String, Option<Entry>>> {
    let mut changes = catalog.mirror.row_delta()?;
    let recorded = catalog.mirror.recorded_non_rows()?;
    let mut current = BTreeMap::new();
    if let Some(format) = format_entry(&catalog.root)? {
        current.insert(".db/format".to_string(), format);
    }
    if let Some(config) = config_entry(&catalog.root)? {
        current.insert(".db/config".to_string(), config);
    }
    for (table, schema) in &catalog.schemas {
        current.insert(
            crate::schema_store::pin_relative(table),
            Entry {
                kind: "schema".into(),
                hash: schema.identity().to_string(),
            },
        );
    }
    for (path, entry) in &current {
        if recorded.get(path) != Some(entry) {
            changes.insert(path.clone(), Some(entry.clone()));
        }
    }
    for path in recorded.keys() {
        if !current.contains_key(path) {
            changes.insert(path.clone(), None);
        }
    }
    Ok(changes)
}

/// Bring the mirror's copy of the recorded head in line with history, by
/// replaying it when the mirror was rebuilt or lost.
pub fn sync_recorded(catalog: &Catalog, head: Option<&Provenance>) -> Result<()> {
    let known = catalog.mirror.meta("recorded_revision")?;
    let expected = head.map(|h| h.revision.to_string());
    if known == expected {
        return Ok(());
    }
    let entries = match head {
        Some(_) => verify_history(&catalog.root)?,
        None => BTreeMap::new(),
    };
    catalog.mirror.replace_recorded(
        &entries,
        head.map(|h| h.revision),
        head.map(|h| h.new_root_hash.as_str()),
    )
}

/// Record the live state as the next revision, carrying only what changed
/// since the head.
pub fn record(
    catalog: &Catalog,
    head: Option<&Provenance>,
    origin: &str,
    transaction_id: Option<&str>,
    lineage: Option<Lineage>,
) -> Result<Provenance> {
    let root = &catalog.root;
    let changes = pending_changes(catalog)?;
    let objects = root.join(".db/objects");
    ensure_real_directory(&objects, true, "object store")?;
    // Objects are written without a flush each and made durable together
    // before the record that names them is written: a crash in between leaves
    // objects no record references, which `reldir gc` collects. An object is
    // content-addressed, so one found damaged -- torn by such a crash -- is
    // simply written again.
    let mut written = false;
    for (path, entry) in changes.iter().filter_map(|(p, e)| e.as_ref().map(|e| (p, e))) {
        let target = object_path(root, &entry.hash);
        if target.exists() && verify_object(root, path, entry).is_ok() {
            continue;
        }
        let value = object_value(catalog, path, entry)?;
        if object_hash(&entry.kind, &value)? != entry.hash {
            return Err(corrupt(format!("the object for {path} does not hash to its entry")));
        }
        let mut bytes = serde_json::to_vec_pretty(&value).map_err(internal)?;
        bytes.push(b'\n');
        let temp = target.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
        fs::write(&temp, &bytes).map_err(|error| DbError::io(&temp, error))?;
        fs::rename(&temp, &target).map_err(|error| DbError::io(&target, error))?;
        written = true;
    }
    if written {
        crate::fs::flush_everything_under(&objects).map_err(|error| DbError::io(&objects, error))?;
    }
    let record = Provenance {
        revision: head.map_or(1, |h| h.revision + 1),
        timestamp: chrono::Utc::now().to_rfc3339(),
        previous_revision: head.map(|h| h.revision),
        previous_root_hash: head.map(|h| h.new_root_hash.clone()),
        new_root_hash: live_root(catalog)?,
        origin: origin.into(),
        changes,
        binary_version: VERSION.into(),
        format_version: FORMAT_VERSION,
        transaction_id: transaction_id.map(String::from),
        lineage,
    };
    ensure_real_directory(&provenance_dir(root), true, "provenance")?;
    let path = record_path(root, record.revision);
    if path.exists() {
        return Err(corrupt(format!("provenance revision {} already exists", record.revision)));
    }
    write_json_atomic(&path, &record)?;
    catalog
        .mirror
        .apply_recorded(&record.changes, record.revision, &record.new_root_hash)?;
    Ok(record)
}

/// Move the whole history aside and begin a new lineage from nothing. The
/// quarantined bytes are kept, never deleted.
pub fn quarantine_history(root: &Path) -> Result<String> {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.fZ").to_string();
    let destination = root.join(".db/provenance-quarantine").join(&stamp);
    fs::create_dir_all(&destination).map_err(|e| DbError::io(&destination, e))?;
    let source = provenance_dir(root);
    if source.exists() {
        fs::rename(&source, destination.join("provenance")).map_err(|e| DbError::io(&source, e))?;
        sync_parent(&source)?;
    }
    Ok(format!(".db/provenance-quarantine/{stamp}"))
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(internal)?;
    bytes.push(b'\n');
    write_bytes_atomic(path, &bytes)
}

/// Whether this path is a temp sibling some writer is still filling in:
/// `<name>.tmp-<uuid>` for metadata, `<name>.reldir-tmp-<uuid>` for rows.
/// Readers look past both; the writer holding the lock renames or removes it.
pub fn is_in_progress_write(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.starts_with("tmp-") || extension.starts_with("reldir-tmp-"))
}

/// Durably replace a file with exactly these bytes: temp sibling, fsync,
/// rename, fsync the directory.
pub fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).map_err(|e| DbError::io(p, e))?
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let mut f = fs::File::create(&tmp).map_err(|e| DbError::io(&tmp, e))?;
    f.write_all(bytes).map_err(|e| DbError::io(&tmp, e))?;
    f.sync_all().map_err(|e| DbError::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| DbError::io(path, e))?;
    sync_parent(path)?;
    Ok(())
}

pub fn sync_parent(path: &Path) -> Result<()> {
    if let Some(p) = path.parent() {
        crate::fs::Fs::sync_dir(&crate::fs::Disk, p).map_err(|e| DbError::io(p, e))?;
    }
    Ok(())
}

pub fn ensure_real_directory(path: &Path, create: bool, description: &str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(corrupt(format!("{description} {} is not a real directory", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            fs::create_dir_all(path).map_err(|error| DbError::io(path, error))?;
            sync_parent(path)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(DbError::io(path, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str, hash: &str) -> Entry {
        Entry { kind: kind.into(), hash: hash.into() }
    }

    #[test]
    fn test2120_the_root_is_a_function_of_the_entries_alone() {
        let mut entries = BTreeMap::from([
            (".db/format".to_string(), entry("format", &"a".repeat(64))),
            ("schema/users.json".to_string(), entry("schema", &"b".repeat(64))),
            ("users/u1.json".to_string(), entry("row", &"c".repeat(64))),
            ("users/u2.json".to_string(), entry("row", &"d".repeat(64))),
        ]);
        let first = root_of(&entries);
        assert_eq!(first, root_of(&entries.clone()), "deterministic");
        entries.insert("users/u2.json".into(), entry("row", &"e".repeat(64)));
        assert_ne!(first, root_of(&entries), "a changed row is a changed state");
        entries.insert("users/u2.json".into(), entry("row", &"d".repeat(64)));
        assert_eq!(first, root_of(&entries), "and changing it back restores the state");
        // The table grouping is by the first path component.
        let moved = BTreeMap::from([
            (".db/format".to_string(), entry("format", &"a".repeat(64))),
            ("schema/users.json".to_string(), entry("schema", &"b".repeat(64))),
            ("people/u1.json".to_string(), entry("row", &"c".repeat(64))),
            ("users/u2.json".to_string(), entry("row", &"d".repeat(64))),
        ]);
        assert_ne!(first, root_of(&moved));
    }

    #[test]
    fn test2121_provenance_records_round_trip_and_reject_unknown_fields() {
        let record = Provenance {
            revision: 1,
            timestamp: "2026-09-14T00:00:00Z".into(),
            previous_revision: None,
            previous_root_hash: None,
            new_root_hash: "c".repeat(64),
            origin: "import".into(),
            changes: BTreeMap::from([("users/u1.json".to_string(), Some(entry("row", &"a".repeat(64))))]),
            binary_version: VERSION.into(),
            format_version: FORMAT_VERSION,
            transaction_id: None,
            lineage: None,
        };
        let text = serde_json::to_string(&record).unwrap();
        let parsed: Provenance = crate::json::parse_as(text.as_bytes()).unwrap();
        assert_eq!(parsed.revision, 1);
        let extended = format!("{{\"surprise\":1,{}", text.strip_prefix('{').unwrap());
        assert!(crate::json::parse_as::<Provenance>(extended.as_bytes()).is_err());
    }

    #[test]
    fn test2122_object_hashes_and_kinds_are_checked() {
        let root = Path::new("/nonexistent-root");
        for invalid in [String::new(), "abc".into(), "A".repeat(64), "g".repeat(64)] {
            let error = verify_object(root, "users/u1.json", &entry("row", &invalid)).unwrap_err();
            assert_eq!(error.diagnostic.code, "INTERNAL_METADATA_CORRUPT");
        }
        let error = verify_object(root, "schema/users.json", &entry("row", &"a".repeat(64))).unwrap_err();
        assert!(error.diagnostic.message.contains("expected \"schema\""), "{}", error.diagnostic.message);
    }
}
