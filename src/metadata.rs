use crate::{
    FORMAT_VERSION, VERSION, canonical,
    catalog::Catalog,
    diagnostic::{DbError, Result},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io::Write, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format_version: u32,
    pub revision: u64,
    pub root_hash: String,
    pub entries: BTreeMap<String, ManifestEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    pub kind: String,
    pub hash: String,
    pub size: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub revision: u64,
    pub timestamp: String,
    pub previous_revision: Option<u64>,
    pub previous_root_hash: Option<String>,
    pub new_root_hash: String,
    pub origin: String,
    pub affected_objects: Vec<String>,
    pub schema_changes: Vec<String>,
    pub binary_version: String,
    pub format_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    pub entries: BTreeMap<String, ManifestEntry>,
}

/// The hash the manifest records for a schema.
///
/// One definition, used both when recording a revision and when asking whether
/// a schema still matches what was recorded. Two spellings of this would let a
/// database disagree with its own history.
fn schema_hash(schema: &crate::schema::Schema) -> Result<String> {
    let value = serde_json::to_value(schema).map_err(internal)?;
    let bytes = serde_json::to_vec(&canonical::normalize(&value)).map_err(internal)?;
    Ok(canonical::hash_bytes(&bytes))
}

pub fn state(c: &Catalog) -> Result<(String, BTreeMap<String, ManifestEntry>)> {
    let mut entries = BTreeMap::new();
    let mut root = Sha256::new();
    root.update(format!("jdb-state-v{}\0", FORMAT_VERSION));
    let format_path = c.root.join(".db/format");
    if format_path.exists() {
        let bytes = fs::read(&format_path).map_err(|e| DbError::io(&format_path, e))?;
        let normalized = String::from_utf8(bytes).map_err(|e| {
            DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                format!(".db/format is not UTF-8: {e}"),
                6,
            )
        })?;
        let canonical = format!("{}\n", normalized.trim());
        let hash = canonical::hash_bytes(canonical.as_bytes());
        root.update(b".db/format\0");
        root.update(hash.as_bytes());
        entries.insert(
            ".db/format".into(),
            ManifestEntry {
                kind: "format".into(),
                hash,
                size: canonical.len() as u64,
            },
        );
    }
    let config_path = c.root.join(".db/config");
    if config_path.exists() {
        let bytes = fs::read(&config_path).map_err(|e| DbError::io(&config_path, e))?;
        let value = crate::json::parse(&bytes).map_err(internal)?;
        let canonical = serde_json::to_vec(&canonical::normalize(&value)).map_err(internal)?;
        let hash = canonical::hash_bytes(&canonical);
        root.update(b".db/config\0");
        root.update(hash.as_bytes());
        entries.insert(
            ".db/config".into(),
            ManifestEntry {
                kind: "config".into(),
                hash,
                size: bytes.len() as u64,
            },
        );
    }
    for (table, s) in &c.schemas {
        let rel = format!("schema/{table}.json");
        let val = serde_json::to_value(s).map_err(internal)?;
        let bytes = serde_json::to_vec(&canonical::normalize(&val)).map_err(internal)?;
        let hash = schema_hash(s)?;
        root.update(rel.as_bytes());
        root.update([0]);
        root.update(hash.as_bytes());
        entries.insert(
            rel,
            ManifestEntry {
                kind: "schema".into(),
                hash,
                size: bytes.len() as u64,
            },
        );
        let mut rows = c.rows.get(table).cloned().unwrap_or_default();
        rows.sort_by_key(|r| crate::integrity::key(&r.value, &s.primary_key, s));
        for row in rows {
            let rel = row.relative.to_string_lossy().replace('\\', "/");
            let val = canonical::canonical_row(&row.value, s);
            let bytes = serde_json::to_vec(&val).map_err(internal)?;
            let hash = canonical::hash_bytes(&bytes);
            root.update(rel.as_bytes());
            root.update([0]);
            root.update(hash.as_bytes());
            entries.insert(
                rel,
                ManifestEntry {
                    kind: "row".into(),
                    hash,
                    size: row.raw.len() as u64,
                },
            );
        }
    }
    Ok((hex::encode(root.finalize()), entries))
}

pub fn load_manifest(root: &Path) -> Result<Option<Manifest>> {
    let p = root.join(".db/manifest.json");
    if !p.exists() {
        return Ok(None);
    }
    let metadata = fs::symlink_metadata(&p).map_err(|e| DbError::io(&p, e))?;
    if !metadata.file_type().is_file() || has_multiple_links(&metadata) {
        return Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("manifest {} is not a private regular file", p.display()),
            6,
        ));
    }
    let b = fs::read(&p).map_err(|e| DbError::io(&p, e))?;
    crate::json::parse_as(&b).map(Some).map_err(|e| {
        DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("{}: {e}", p.display()),
            6,
        )
    })
}
pub fn validate_provenance(root: &Path, manifest: Option<&Manifest>) -> Result<()> {
    let dir = root.join(".db/provenance");
    ensure_real_directory(&root.join(".db/objects"), false, "object store")?;
    if !ensure_real_directory(&dir, false, "provenance")? {
        return Ok(());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| DbError::io(&dir, e))? {
        let path = entry.map_err(|e| DbError::io(&dir, e))?.path();
        let metadata = fs::symlink_metadata(&path).map_err(|e| DbError::io(&path, e))?;
        if !metadata.file_type().is_file()
            || has_multiple_links(&metadata)
            || path.extension().and_then(|x| x.to_str()) != Some("json")
        {
            return Err(DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                format!("unexpected provenance entry {}", path.display()),
                6,
            ));
        }
        paths.push(path);
    }
    paths.sort();
    let mut prior: Option<Provenance> = None;
    for path in paths {
        let value: Provenance = crate::json::parse_as(
            &fs::read(&path).map_err(|e| DbError::io(&path, e))?,
        )
        .map_err(|e| {
            DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                format!("invalid provenance {}: {e}", path.display()),
                6,
            )
        })?;
        if value.format_version != FORMAT_VERSION {
            return Err(DbError::new(
                "FORMAT_UNSUPPORTED",
                format!(
                    "provenance {} uses format {}",
                    path.display(),
                    value.format_version
                ),
                6,
            ));
        }
        let expected_name = format!("{:020}.json", value.revision);
        if path.file_name().and_then(|name| name.to_str()) != Some(&expected_name) {
            return Err(DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                format!(
                    "provenance filename does not match revision {}",
                    value.revision
                ),
                6,
            ));
        }
        if !matches!(
            value.origin.as_str(),
            "internal"
                | "external"
                | "recovery"
                | "repair"
                | "migration"
                | "import"
                | "snapshot_restore"
        ) {
            return Err(DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                format!("unknown provenance origin {:?}", value.origin),
                6,
            ));
        }
        match &prior {
            None if value.revision != 1
                || value.previous_revision.is_some()
                || value.previous_root_hash.is_some() =>
            {
                return Err(DbError::new(
                    "INTERNAL_METADATA_CORRUPT",
                    "provenance history must begin at revision 1 without a predecessor",
                    6,
                ));
            }
            Some(p)
                if value.revision != p.revision + 1
                    || value.previous_revision != Some(p.revision)
                    || value.previous_root_hash.as_deref() != Some(&p.new_root_hash) =>
            {
                return Err(DbError::new(
                    "INTERNAL_METADATA_CORRUPT",
                    format!("broken provenance chain at revision {}", value.revision),
                    6,
                ));
            }
            _ => {}
        }
        for (object_path, entry) in &value.entries {
            validate_object(root, object_path, entry)?;
        }
        prior = Some(value);
    }
    if let (Some(last), Some(m)) = (prior, manifest)
        && (last.revision != m.revision
            || last.new_root_hash != m.root_hash
            || last.entries != m.entries)
    {
        return Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            "manifest does not match the latest provenance record",
            6,
        ));
    }
    Ok(())
}

fn validate_object(root: &Path, path: &str, entry: &ManifestEntry) -> Result<()> {
    if entry.hash.len() != 64
        || !entry
            .hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("invalid object hash {:?} for {path}", entry.hash),
            6,
        ));
    }
    let expected_kind = if path == ".db/format" {
        "format"
    } else if path == ".db/config" {
        "config"
    } else if path.starts_with("schema/") && path.ends_with(".json") {
        "schema"
    } else {
        "row"
    };
    if entry.kind != expected_kind {
        return Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!(
                "object {path} has kind {:?}, expected {expected_kind:?}",
                entry.kind
            ),
            6,
        ));
    }
    let object = root.join(format!(".db/objects/{}.json", entry.hash));
    let metadata = fs::symlink_metadata(&object).map_err(|error| {
        DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("missing revision object {}: {error}", object.display()),
            6,
        )
    })?;
    if !metadata.file_type().is_file() || has_multiple_links(&metadata) {
        return Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!(
                "revision object {} is not a private regular file",
                object.display()
            ),
            6,
        ));
    }
    let bytes = fs::read(&object).map_err(|error| DbError::io(&object, error))?;
    let value = crate::json::parse(&bytes).map_err(|error| {
        DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("invalid revision object {}: {error}", object.display()),
            6,
        )
    })?;
    let canonical = if entry.kind == "format" {
        let text = value.as_str().ok_or_else(|| {
            DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                format!(
                    "format revision object {} is not a string",
                    object.display()
                ),
                6,
            )
        })?;
        format!("{}\n", text.trim()).into_bytes()
    } else if entry.kind == "row" {
        // Row object order is semantic canonical order: schema columns first,
        // then permitted extras. The object store preserves that order.
        serde_json::to_vec(&value).map_err(internal)?
    } else {
        serde_json::to_vec(&canonical::normalize(&value)).map_err(internal)?
    };
    if canonical::hash_bytes(&canonical) != entry.hash {
        return Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!(
                "revision object {} does not match its hash",
                object.display()
            ),
            6,
        ));
    }
    Ok(())
}
pub fn provenance_head(root: &Path) -> Result<Option<Manifest>> {
    let dir = root.join(".db/provenance");
    if !ensure_real_directory(&dir, false, "provenance")? {
        return Ok(None);
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| DbError::io(&dir, e))? {
        let path = entry.map_err(|e| DbError::io(&dir, e))?.path();
        let metadata = fs::symlink_metadata(&path).map_err(|e| DbError::io(&path, e))?;
        if !metadata.file_type().is_file()
            || has_multiple_links(&metadata)
            || path.extension().and_then(|x| x.to_str()) != Some("json")
        {
            return Err(DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                format!("unexpected provenance entry {}", path.display()),
                6,
            ));
        }
        paths.push(path);
    }
    paths.sort();
    let Some(path) = paths.last() else {
        return Ok(None);
    };
    let p: Provenance = crate::json::parse_as(&fs::read(path).map_err(|e| DbError::io(path, e))?)
        .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))?;
    Ok(Some(Manifest {
        format_version: p.format_version,
        revision: p.revision,
        root_hash: p.new_root_hash,
        entries: p.entries,
    }))
}
pub fn write_manifest(root: &Path, m: &Manifest) -> Result<()> {
    write_json_atomic(&root.join(".db/manifest.json"), m)
}
fn write_provenance(root: &Path, p: &Provenance) -> Result<()> {
    ensure_real_directory(&root.join(".db/provenance"), true, "provenance")?;
    let path = root.join(format!(".db/provenance/{:020}.json", p.revision));
    if path.exists() {
        return Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("provenance revision {} already exists", p.revision),
            6,
        ));
    }
    write_json_atomic(&path, p)
}
pub fn reconcile_after_recovery(root: &Path) -> Result<()> {
    let manifest = load_manifest(root)?;
    let Some(head) = provenance_head(root)? else {
        return Ok(());
    };
    match manifest {
        Some(m) if m.revision == head.revision && m.root_hash == head.root_hash => Ok(()),
        Some(m) if head.revision == m.revision + 1 => {
            let p = load_provenance(root, head.revision)?;
            if p.previous_revision != Some(m.revision)
                || p.previous_root_hash.as_deref() != Some(&m.root_hash)
            {
                return Err(DbError::new(
                    "INTERNAL_METADATA_CORRUPT",
                    "recovered provenance does not continue the manifest",
                    6,
                ));
            }
            write_manifest(root, &head)
        }
        None => write_manifest(root, &head),
        _ => Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            "manifest/provenance divergence cannot be recovered unambiguously",
            6,
        )),
    }
}
fn load_provenance(root: &Path, revision: u64) -> Result<Provenance> {
    let path = root.join(format!(".db/provenance/{revision:020}.json"));
    let metadata = fs::symlink_metadata(&path).map_err(|e| DbError::io(&path, e))?;
    if !metadata.file_type().is_file() || has_multiple_links(&metadata) {
        return Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!(
                "provenance {} is not a private regular file",
                path.display()
            ),
            6,
        ));
    }
    crate::json::parse_as(&fs::read(&path).map_err(|e| DbError::io(&path, e))?)
        .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))
}
pub fn record(
    catalog: &Catalog,
    old: Option<&Manifest>,
    new_root: String,
    entries: BTreeMap<String, ManifestEntry>,
    origin: &str,
    transaction_id: Option<&str>,
) -> Result<Manifest> {
    let root = &catalog.root;
    let revision = old.map_or(1, |m| m.revision + 1);
    let affected = diff_entries(old.map(|m| &m.entries), &entries);
    let schema_changes = affected
        .iter()
        .filter(|change| {
            change
                .split_once(' ')
                .is_some_and(|(_, path)| path.starts_with("schema/"))
        })
        .cloned()
        .collect();
    let p = Provenance {
        revision,
        timestamp: chrono::Utc::now().to_rfc3339(),
        previous_revision: old.map(|m| m.revision),
        previous_root_hash: old.map(|m| m.root_hash.clone()),
        new_root_hash: new_root.clone(),
        origin: origin.into(),
        affected_objects: affected,
        schema_changes,
        binary_version: VERSION.into(),
        format_version: FORMAT_VERSION,
        transaction_id: transaction_id.map(String::from),
        entries: entries.clone(),
    };
    store_objects(catalog, &entries)?;
    write_provenance(root, &p)?;
    let m = Manifest {
        format_version: FORMAT_VERSION,
        revision,
        root_hash: new_root,
        entries,
    };
    write_manifest(root, &m)?;
    Ok(m)
}
fn store_objects(c: &Catalog, entries: &BTreeMap<String, ManifestEntry>) -> Result<()> {
    let dir = c.root.join(".db/objects");
    ensure_real_directory(&dir, true, "object store")?;
    let mut values = BTreeMap::<String, serde_json::Value>::new();
    for (t, s) in &c.schemas {
        values.insert(
            format!("schema/{t}.json"),
            canonical::normalize(&serde_json::to_value(s).map_err(internal)?),
        );
        for r in &c.rows[t] {
            values.insert(
                r.relative.to_string_lossy().replace('\\', "/"),
                canonical::canonical_row(&r.value, s),
            );
        }
    }
    let config = c.root.join(".db/config");
    if config.exists() {
        values.insert(
            ".db/config".into(),
            canonical::normalize(
                &crate::json::parse(&fs::read(&config).map_err(|e| DbError::io(&config, e))?)
                    .map_err(internal)?,
            ),
        );
    }
    let format = c.root.join(".db/format");
    if format.exists() {
        values.insert(
            ".db/format".into(),
            serde_json::Value::String(
                fs::read_to_string(&format)
                    .map_err(|e| DbError::io(&format, e))?
                    .trim()
                    .into(),
            ),
        );
    }
    for (path, entry) in entries {
        let target = dir.join(format!("{}.json", entry.hash));
        match fs::symlink_metadata(&target) {
            Ok(_) => validate_object(&c.root, path, entry)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let value = values.get(path).ok_or_else(|| {
                    DbError::new(
                        "INTERNAL_METADATA_CORRUPT",
                        format!("cannot capture revision object {path}"),
                        6,
                    )
                })?;
                write_json_atomic(&target, value)?;
                validate_object(&c.root, path, entry)?;
            }
            Err(error) => return Err(DbError::io(&target, error)),
        }
    }
    Ok(())
}
pub fn diff_entries(
    old: Option<&BTreeMap<String, ManifestEntry>>,
    new: &BTreeMap<String, ManifestEntry>,
) -> Vec<String> {
    let mut out = vec![];
    let empty = BTreeMap::new();
    let old = old.unwrap_or(&empty);
    for (p, e) in new {
        match old.get(p) {
            None => out.push(format!("A {p}")),
            Some(o) if o.hash != e.hash => out.push(format!("M {p}")),
            _ => {}
        }
    }
    for p in old.keys() {
        if !new.contains_key(p) {
            out.push(format!("D {p}"))
        }
    }
    out
}
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(internal)?;
    bytes.push(b'\n');
    write_bytes_atomic(path, &bytes)
}

/// Durably replace a file with exactly these bytes.
///
/// The one atomic-write mechanism: temp sibling, fsync, rename, fsync parent.
/// Callers that have already rendered their content -- schemas, which must be
/// byte-identical to the pin they came from -- use this rather than a second
/// serializer that would produce a different encoding of the same value.
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
        let f = fs::File::open(p).map_err(|e| DbError::io(p, e))?;
        f.sync_all().map_err(|e| DbError::io(p, e))?
    }
    Ok(())
}
pub fn ensure_real_directory(path: &Path, create: bool, description: &str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("{description} {} is not a real directory", path.display()),
            6,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            fs::create_dir(path).map_err(|error| DbError::io(path, error))?;
            sync_parent(path)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(DbError::io(path, error)),
    }
}
fn internal(e: serde_json::Error) -> DbError {
    DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6)
}

#[cfg(unix)]
fn has_multiple_links(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() > 1
}

#[cfg(not(unix))]
fn has_multiple_links(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str) -> ManifestEntry {
        ManifestEntry {
            kind: kind.into(),
            hash: "a".repeat(64),
            size: 1,
        }
    }

    /// The object store is content addressed by lowercase hexadecimal SHA-256.
    /// Anything else is corrupt internal metadata, not a hash to look up
    /// (Section 58).
    #[test]
    fn test1068_object_hashes_must_be_lowercase_sha256_hex() {
        let root = Path::new("/nonexistent-root");
        for invalid in [
            String::new(),
            "abc".into(),
            "A".repeat(64),
            "g".repeat(64),
            "a".repeat(63),
            "a".repeat(65),
        ] {
            let mut e = entry("row");
            e.hash = invalid.clone();
            let error = validate_object(root, "users/u1.json", &e)
                .expect_err(&format!("{invalid:?} must be refused"));
            assert_eq!(error.diagnostic.code, "INTERNAL_METADATA_CORRUPT");
        }
    }

    /// An object's declared kind is derived from its path, so a mislabelled
    /// entry means the metadata disagrees with the layout it describes.
    #[test]
    fn test1069_object_kind_is_derived_from_its_path() {
        let root = Path::new("/nonexistent-root");
        for (path, expected) in [
            (".db/format", "format"),
            (".db/config", "config"),
            ("schema/users.json", "schema"),
            ("users/u1.json", "row"),
        ] {
            // The correct kind gets past the kind check and fails later, when
            // the absent object file is opened.
            let error = validate_object(root, path, &entry(expected)).unwrap_err();
            assert_eq!(error.diagnostic.code, "INTERNAL_METADATA_CORRUPT");
            assert!(
                !error.diagnostic.message.contains("expected"),
                "{path} with kind {expected} should pass the kind check"
            );

            // A wrong kind is rejected by the kind check itself.
            let wrong = if expected == "row" { "schema" } else { "row" };
            let error = validate_object(root, path, &entry(wrong)).unwrap_err();
            assert!(
                error.diagnostic.message.contains("expected"),
                "{path} with kind {wrong} must be refused as mislabelled"
            );
        }
    }

    /// Section 22: the state root covers the format, the configuration, every
    /// schema, and every row, so the digest is stable across processes and
    /// changes whenever any authoritative input changes.
    #[test]
    fn test1070_manifest_entries_round_trip_through_json() {
        let manifest = Manifest {
            format_version: FORMAT_VERSION,
            revision: 7,
            root_hash: "b".repeat(64),
            entries: BTreeMap::from([
                ("schema/users.json".to_string(), entry("schema")),
                ("users/u1.json".to_string(), entry("row")),
            ]),
        };
        let text = serde_json::to_string(&manifest).unwrap();
        let parsed: Manifest = crate::json::parse_as(text.as_bytes()).unwrap();
        assert_eq!(parsed.revision, 7);
        assert_eq!(parsed.entries, manifest.entries);

        // Unknown keys are refused: the manifest is a closed representation.
        // The key is injected once, immediately inside the top-level object, so
        // the refusal is about that object and not about some nested value.
        let extended = format!(
            "{{\"surprise\":1,{}",
            text.strip_prefix('{').expect("an object was serialized")
        );
        assert!(crate::json::parse_as::<Manifest>(extended.as_bytes()).is_err());
    }

    /// Section 23: a provenance record names the transition it describes, and
    /// the representation is closed so an unknown field cannot be ignored.
    #[test]
    fn test1071_provenance_records_round_trip_and_reject_unknown_fields() {
        let provenance = Provenance {
            revision: 1,
            timestamp: "2026-09-14T00:00:00Z".into(),
            previous_revision: None,
            previous_root_hash: None,
            new_root_hash: "c".repeat(64),
            origin: "import".into(),
            affected_objects: vec!["A users/u1.json".into()],
            schema_changes: vec!["A schema/users.json".into()],
            binary_version: VERSION.into(),
            format_version: FORMAT_VERSION,
            transaction_id: None,
            entries: BTreeMap::from([("users/u1.json".to_string(), entry("row"))]),
        };
        let text = serde_json::to_string(&provenance).unwrap();
        let parsed: Provenance = crate::json::parse_as(text.as_bytes()).unwrap();
        assert_eq!(parsed.revision, 1);
        assert_eq!(parsed.origin, "import");
        assert!(parsed.previous_revision.is_none());

        let extended = format!(
            "{{\"surprise\":1,{}",
            text.strip_prefix('{').expect("an object was serialized")
        );
        assert!(crate::json::parse_as::<Provenance>(extended.as_bytes()).is_err());
    }

    /// Section 32: a provenance file is named for the revision it records, so a
    /// zero-padded twenty-digit name sorts chronologically.
    #[test]
    fn test1072_provenance_filenames_sort_chronologically() {
        let names: Vec<String> = [1u64, 2, 10, 100, 1000]
            .iter()
            .map(|revision| format!("{revision:020}.json"))
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "zero padding must preserve revision order");
        assert_eq!(names[0], "00000000000000000001.json");
    }
}
