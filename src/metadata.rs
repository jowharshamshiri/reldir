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
    let value = crate::schema::semantic::encode_v1(schema);
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
        let val = crate::schema::semantic::encode_v1(s);
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
            canonical::normalize(&crate::schema::semantic::encode_v1(s)),
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

    // ---------------------------------------------------------------------
    // Golden schema hashes.
    //
    // The schema file format is being replaced by a JSON Schema dialect. That
    // is a change to how a schema is *written*, not to what it *is*, so every
    // hash below must survive it unchanged: a revision's identity is its
    // logical content, never its serialization.
    //
    // These are frozen literals rather than values recomputed on both sides of
    // an assertion. A test that hashes the same input twice and compares the
    // results passes no matter what the encoding does, which would make it
    // worthless for exactly the change it exists to guard.
    // ---------------------------------------------------------------------

    use crate::schema::{
        AdditionalFields, Action, Check, Column, ColumnType, ForeignKey, Generated, GeneratedKind,
        Reference, Schema, Storage,
    };
    use indexmap::IndexMap;
    use serde_json::json;

    fn col(kind: ColumnType) -> Column {
        Column {
            kind,
            nullable: false,
            default: None,
            generated: None,
            values: None,
            items: None,
            properties: None,
            pattern: None,
            additional_properties: true,
            description: None,
            annotations: Default::default(),
        }
    }

    fn table(name: &str, columns: Vec<(&str, Column)>, primary_key: &[&str]) -> Schema {
        let mut map = IndexMap::new();
        for (column_name, column) in columns {
            map.insert(column_name.to_string(), column);
        }
        Schema {
            table: name.into(),
            schema_version: 1,
            schema_format: None,
            description: None,
            primary_key: primary_key.iter().map(|k| (*k).to_string()).collect(),
            columns: map,
            unique: vec![],
            foreign_keys: vec![],
            check: vec![],
            indexes: vec![],
            storage: None,
            additional_fields: AdditionalFields::Reject,
            annotations: Default::default(),
        }
    }

    /// Every fixture the golden hashes cover, built once so the hash test and
    /// any future encoder test describe the same schemas.
    fn golden_fixtures() -> Vec<(&'static str, Schema)> {
        let mut out = vec![];

        // One fixture per column type, so a change to any type's encoding shows
        // up as exactly one failing row.
        for (label, kind) in [
            ("bool", ColumnType::Bool),
            ("int", ColumnType::Int),
            ("float", ColumnType::Float),
            ("decimal", ColumnType::Decimal),
            ("string", ColumnType::String),
            ("bytes", ColumnType::Bytes),
            ("date", ColumnType::Date),
            ("timestamp", ColumnType::Timestamp),
            ("uuid", ColumnType::Uuid),
            ("ulid", ColumnType::Ulid),
            ("json", ColumnType::Json),
        ] {
            out.push((
                label,
                table("t", vec![("id", col(ColumnType::String)), ("v", col(kind))], &["id"]),
            ));
        }

        // enum carries its values; array and object carry nested shape.
        let mut enumerated = col(ColumnType::Enum);
        enumerated.values = Some(vec!["a".into(), "b".into()]);
        out.push((
            "enum",
            table("t", vec![("id", col(ColumnType::String)), ("v", enumerated)], &["id"]),
        ));

        let mut nested_array = col(ColumnType::Array);
        nested_array.items = Some(Box::new(col(ColumnType::Decimal)));
        out.push((
            "array_of_decimal",
            table("t", vec![("id", col(ColumnType::String)), ("v", nested_array)], &["id"]),
        ));

        let mut properties = IndexMap::new();
        properties.insert("inner".to_string(), col(ColumnType::Ulid));
        let mut nested_object = col(ColumnType::Object);
        nested_object.properties = Some(properties);
        out.push((
            "object_with_ulid_property",
            table("t", vec![("id", col(ColumnType::String)), ("v", nested_object)], &["id"]),
        ));

        // nullable x default x generated, the axes that decide `required`.
        let mut nullable = col(ColumnType::String);
        nullable.nullable = true;
        out.push((
            "nullable",
            table("t", vec![("id", col(ColumnType::String)), ("v", nullable)], &["id"]),
        ));

        let mut defaulted = col(ColumnType::String);
        defaulted.default = Some(json!("fixed"));
        out.push((
            "default_not_null",
            table("t", vec![("id", col(ColumnType::String)), ("v", defaulted)], &["id"]),
        ));

        let mut nullable_defaulted = col(ColumnType::String);
        nullable_defaulted.nullable = true;
        nullable_defaulted.default = Some(json!("fixed"));
        out.push((
            "nullable_and_default",
            table("t", vec![("id", col(ColumnType::String)), ("v", nullable_defaulted)], &["id"]),
        ));

        for (label, kind, generated) in [
            ("generated_uuid", ColumnType::Uuid, GeneratedKind::Uuid),
            ("generated_ulid", ColumnType::Ulid, GeneratedKind::Ulid),
            ("generated_now", ColumnType::Timestamp, GeneratedKind::Now),
            ("generated_sequence", ColumnType::Int, GeneratedKind::Sequence),
        ] {
            let mut column = col(kind);
            column.generated = Some(Generated { kind: generated });
            out.push((
                label,
                table("t", vec![("id", col(ColumnType::String)), ("v", column)], &["id"]),
            ));
        }

        let mut described = col(ColumnType::String);
        described.description = Some("what it holds".into());
        let mut with_description =
            table("t", vec![("id", col(ColumnType::String)), ("v", described)], &["id"]);
        with_description.description = Some("the table".into());
        out.push(("descriptions", with_description));

        // Relational facts: each one alone, so a failure names the culprit.
        let mut unique = table(
            "t",
            vec![("id", col(ColumnType::String)), ("v", col(ColumnType::String))],
            &["id"],
        );
        unique.unique = vec![vec!["v".into()]];
        out.push(("unique", unique));

        let mut indexes = table(
            "t",
            vec![("id", col(ColumnType::String)), ("v", col(ColumnType::String))],
            &["id"],
        );
        indexes.indexes = vec![vec!["v".into()]];
        out.push(("indexes", indexes));

        let mut checked = table(
            "t",
            vec![("id", col(ColumnType::String)), ("n", col(ColumnType::Int))],
            &["id"],
        );
        checked.check = vec![Check {
            name: "positive".into(),
            expr: "n > 0".into(),
        }];
        out.push(("check", checked));

        let mut stored = table(
            "t",
            vec![("id", col(ColumnType::String)), ("slug", col(ColumnType::String))],
            &["id"],
        );
        stored.unique = vec![vec!["slug".into()]];
        stored.storage = Some(Storage {
            filename: vec!["slug".into()],
        });
        out.push(("storage_filename", stored));

        let mut permissive = table("t", vec![("id", col(ColumnType::String))], &["id"]);
        permissive.additional_fields = AdditionalFields::Allow;
        out.push(("additional_fields_allow", permissive));

        // A pattern decides which rows a column admits, so it must be part of
        // what the schema *is*. Without these fixtures the identity change
        // would be untested: no other fixture carries a user pattern, and jdb's
        // own decimal and ulid patterns are deliberately excluded from identity.
        let mut patterned_column = col(ColumnType::String);
        patterned_column.pattern = Some("^[a-z][a-z0-9._-]{2,127}$".into());
        out.push((
            "patterned",
            table(
                "t",
                vec![("id", col(ColumnType::String)), ("v", patterned_column)],
                &["id"],
            ),
        ));

        let mut closed_properties = IndexMap::new();
        closed_properties.insert("source".to_string(), col(ColumnType::String));
        let mut closed_column = col(ColumnType::Object);
        closed_column.properties = Some(closed_properties);
        closed_column.additional_properties = false;
        out.push((
            "closed_nested_object",
            table(
                "t",
                vec![("id", col(ColumnType::String)), ("v", closed_column)],
                &["id"],
            ),
        ));

        let mut composite = table(
            "t",
            vec![("a", col(ColumnType::String)), ("b", col(ColumnType::String))],
            &["a", "b"],
        );
        composite.unique = vec![vec!["a".into(), "b".into()]];
        out.push(("composite_primary_key", composite));

        let mut versioned = table("t", vec![("id", col(ColumnType::String))], &["id"]);
        versioned.schema_version = 7;
        versioned.schema_format = Some(crate::FORMAT_VERSION);
        out.push(("schema_version_and_format", versioned));

        // Every referential action, including the unset pair that defaults to
        // restrict without being written.
        for (label, on_delete, on_update) in [
            ("fk_unset_actions", None, None),
            ("fk_restrict", Some(Action::Restrict), Some(Action::Restrict)),
            ("fk_cascade", Some(Action::Cascade), Some(Action::Cascade)),
            ("fk_set_null", Some(Action::SetNull), Some(Action::Restrict)),
            ("fk_set_default", Some(Action::SetDefault), Some(Action::Restrict)),
            ("fk_no_action", Some(Action::NoAction), Some(Action::NoAction)),
        ] {
            let mut referencing = col(ColumnType::String);
            referencing.nullable = true;
            referencing.default = Some(json!("x"));
            let mut child = table(
                "child",
                vec![("id", col(ColumnType::String)), ("parent_id", referencing)],
                &["id"],
            );
            child.foreign_keys = vec![ForeignKey {
                columns: vec!["parent_id".into()],
                references: Reference {
                    table: "parent".into(),
                    columns: vec!["id".into()],
                },
                on_delete,
                on_update,
            }];
            out.push((label, child));
        }

        // Column order is part of the logical schema: canonical rows are
        // written in it, so two schemas differing only by order are different.
        out.push((
            "column_order_ab",
            table(
                "t",
                vec![
                    ("id", col(ColumnType::String)),
                    ("a", col(ColumnType::String)),
                    ("b", col(ColumnType::String)),
                ],
                &["id"],
            ),
        ));
        out.push((
            "column_order_ba",
            table(
                "t",
                vec![
                    ("id", col(ColumnType::String)),
                    ("b", col(ColumnType::String)),
                    ("a", col(ColumnType::String)),
                ],
                &["id"],
            ),
        ));

        out
    }

    /// The logical identity of a schema, frozen.
    ///
    /// A schema's hash is what a revision is recorded against, so it must not
    /// move when the file format changes. These are taken over
    /// `semantic::encode_v1`, which is the whole point: identity follows the
    /// relational content, so replacing the on-disk grammar with JSON Schema
    /// must leave every one of these untouched. A failure here after the
    /// migration means the serialization has leaked into semantic identity.
    #[test]
    fn test1125_schema_hashes_are_independent_of_the_file_format() {
        const GOLDEN: &[(&str, &str)] = &[
            ("bool", "0bf55eb68c2708f1a4ad0b6686c189a73fb0f71e8ea21be4e907def78ea5fc44"),
            ("int", "a939dcce7c26dacdb5cfc1d32420e0a078f8f63c3bc886cd3786a92b73646810"),
            ("float", "ddadf6eb139973d09dd25eacedcf6c3bbec6ee87df3d8346fe85631fd6296f37"),
            ("decimal", "f26399f7a7c29ccf35da54e72f2d3b6eb93441428b64739d1373f77e71363d26"),
            ("string", "bacaecf137c9d46af4532595b9d6d136d67d14fb83991796da4ecb5e65a531ce"),
            ("bytes", "8280ca26faf8dab8843f167d43d595b44b7eafed2edfc55bebe10eec39d20d34"),
            ("date", "605663fb2cee57cb3844b92a915d25f3aba9cfe0ea0c32f4133268474babb855"),
            ("timestamp", "20983e49021d1239d9d8ca8bedbb73e0d2ee6812576c2877082f326ed13b9f54"),
            ("uuid", "714b623636f681429e72a91fc2e24ca239b9f65553f18e730d76fe2e41c3fc70"),
            ("ulid", "d78e2bc72249d0b71b8ff7ae7e1a46ff0f70379fab49b2a6f30e9f13e674ca7a"),
            ("json", "7b18bcd514f76d114189ee328ce59a83a76e0741aa666d2c86a9b3b7dc459364"),
            ("enum", "4fc4fe783a3d44f354ff6554a86e9df9f4c71574ac59eb52586d0d5c89375314"),
            ("array_of_decimal", "f84a8ff25795181f16bbfd6740199fdaa1b85cddcd2902dfbba2fd80fbe1344f"),
            ("object_with_ulid_property", "444980ee96c1f131a9684f3708f08ef8fab54165763f00930d56fd75a798dfeb"),
            ("nullable", "be3fc5ed17bc06a9a3e0d6d2d98c16451b46d5439febd274192afb2126eb3800"),
            ("default_not_null", "706ec91ff11e15d5d590052842a077cdfc8aca11c824c2c67483073828ae47ab"),
            ("nullable_and_default", "10cee2f93793a85a5ab89d51afb45588bc7c376873403c7cda9f3bd82e183288"),
            ("generated_uuid", "43ea5b01e8a2bb7dbd6c14c0ba7dcdc81bd50f127e6bc45ea5fab975e95bf002"),
            ("generated_ulid", "b2d142c6febf8227bf61cf7bba1266785a3f79aa9a7234d319d6fab5cea4f1cb"),
            ("generated_now", "2a4c033cad2310f2ae59815c808d50145b1d5d059ed994a0de20fd74b0d574f5"),
            ("generated_sequence", "26e3e6b8cef0db3a27abd3eb65f4760ad8a2bc7667278463e00055e6bdb75627"),
            ("descriptions", "10b4bd0a2fe9620e5e51a8e8e0b4673d8f0a547d4917f80f7dd5d71af5ba4494"),
            ("unique", "6ce26284c29c46747a81345937b4bc537e3163a45fa229a24047511b4cb11289"),
            ("indexes", "92bf7233758f91d742f5ae852720e7fcfc3f59b1fbd68f9b9df61f9f10f03a31"),
            ("check", "4e61b420e65cf6b0a35b6ceb449770f039d6a67dce2f384ede06f51837cd1608"),
            ("storage_filename", "2c53387a296180351c470dc11f79471b2ebec6c7917a89346ddea40c2540b0aa"),
            ("additional_fields_allow", "df38fd9bf16cd9108c27f23adf84df9d62d5ca8876c1d780e7496d4307ff391c"),
            // A pattern and a closed object each decide which rows a schema
            // admits, so each earns an identity of its own. Every hash above
            // and below is unchanged: jdb's own decimal and ulid patterns are
            // excluded from identity, so adding the keyword moved nothing that
            // already existed.
            ("patterned", "7f486b0cab18379e768dff0063a3b0d16d4945e5514fac53cdc07c6fc75f9a9c"),
            ("closed_nested_object", "5352c322653b2cc854c596064fdab7bbbb1b5d5f4fcef4ff56d3e78948d70fe3"),
            ("composite_primary_key", "121470cfcceecef0f75cbc824d0f2778c4877628860cad456ac22b425c9793f2"),
            ("schema_version_and_format", "1b63cf55a764d6f0ec9783340577add6d7179ed5a9c48d5322d9568f6458aa49"),
            ("fk_unset_actions", "8888df83fa328bcfb51d474610539db53e00d5f4fc343dc992b3151267f5f426"),
            ("fk_restrict", "8888df83fa328bcfb51d474610539db53e00d5f4fc343dc992b3151267f5f426"),
            ("fk_cascade", "388bc55a325d634c863da9dfef062388a6e7a7fe35a5b263546f007a56a9c829"),
            ("fk_set_null", "b0cbb20d6ecf9914aa3a3c792a954c655bbb0c33b935efb752f7b8dc9ca250cd"),
            ("fk_set_default", "7ceebe5a46548fdaae78d3074dea9f9dda6c08c5875251330ec2fc1a3dcb15a2"),
            ("fk_no_action", "128e213e5c156b9c09f26c262555aa10b37243e938761c8a9bba967dacc77fe2"),
            ("column_order_ab", "06bfe85c793f5868313e8d2b6326b7873335c11506f962e1e1cf71d08d9c874f"),
            ("column_order_ba", "21d92c54ff542ff3e6c89132d35f77350c23859a63c2d0145ddd069a85ea6ab5"),
        ];

        let fixtures = golden_fixtures();
        let computed: Vec<(String, String)> = fixtures
            .iter()
            .map(|(label, schema)| {
                (
                    (*label).to_string(),
                    {
                        let value = crate::schema::semantic::encode_v1(schema);
                        let bytes =
                            serde_json::to_vec(&canonical::normalize(&value)).expect("encodes");
                        canonical::hash_bytes(&bytes)
                    },
                )
            })
            .collect();

        if GOLDEN.is_empty() {
            let rendered = computed
                .iter()
                .map(|(label, hash)| format!("        (\"{label}\", \"{hash}\"),"))
                .collect::<Vec<_>>()
                .join("\n");
            panic!("golden hashes are unrecorded; freeze these:\n{rendered}");
        }

        assert_eq!(
            GOLDEN.len(),
            computed.len(),
            "every fixture must be pinned: the golden table has drifted from the fixtures"
        );
        for ((label, expected), (computed_label, actual)) in GOLDEN.iter().zip(&computed) {
            assert_eq!(label, computed_label, "fixture order must be stable");
            assert_eq!(
                expected, actual,
                "{label}: schema hash moved, so serialization has changed logical identity"
            );
        }
    }

    /// Column order is part of a schema's identity.
    ///
    /// `canonical_row` writes a row's keys in schema column order, so two
    /// schemas differing only in that order write different bytes for the same
    /// logical row -- `test1003` pins exactly that. Identity has to agree with
    /// what the database actually writes, or the state root cannot distinguish
    /// two databases whose files genuinely differ.
    ///
    /// The derived encoding lost this: it rendered `columns` as an object, and
    /// `normalize` sorts object keys, so the ordering was erased before the
    /// digest. `semantic::encode_v1` carries the order explicitly, which is the
    /// one respect in which it departs from what the derive produced.
    #[test]
    fn test1126_column_order_changes_a_schemas_identity() {
        let fixtures = golden_fixtures();
        let find = |name: &str| {
            fixtures
                .iter()
                .find(|(label, _)| *label == name)
                .map(|(_, schema)| schema.clone())
                .expect("fixture exists")
        };
        let ab = find("column_order_ab");
        let ba = find("column_order_ba");

        // The two schemas genuinely differ: they serialize the same row to
        // different bytes, which is what makes sharing one hash a defect.
        let mut row = serde_json::Map::new();
        row.insert("id".into(), json!("r"));
        row.insert("a".into(), json!("A"));
        row.insert("b".into(), json!("B"));
        assert_ne!(
            serde_json::to_vec(&canonical::canonical_row(&row, &ab)).unwrap(),
            serde_json::to_vec(&canonical::canonical_row(&row, &ba)).unwrap(),
            "the fixtures must write different rows, or they are not a witness"
        );

        let identity = |schema: &crate::schema::Schema| {
            let value = crate::schema::semantic::encode_v1(schema);
            canonical::hash_bytes(&serde_json::to_vec(&canonical::normalize(&value)).unwrap())
        };
        assert_ne!(
            identity(&ab),
            identity(&ba),
            "column order must change a schema's identity"
        );

        // An unstated referential action and its written equivalent impose the
        // same behaviour, so they are one schema with one identity.
        assert_eq!(
            identity(&find("fk_unset_actions")),
            identity(&find("fk_restrict")),
            "an omitted on_delete is restrict"
        );
    }
}
