use crate::{
    FORMAT_VERSION, VERSION, canonical,
    catalog::Catalog,
    diagnostic::{DbError, Result},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io::Write, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Manifest {
    pub format_version: u32,
    pub revision: u64,
    pub root_hash: String,
    pub entries: BTreeMap<String, ManifestEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub kind: String,
    pub hash: String,
    pub size: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        let hash = canonical::hash_bytes(&bytes);
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
    if !dir.exists() {
        return Ok(());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| DbError::io(&dir, e))? {
        let path = entry.map_err(|e| DbError::io(&dir, e))?.path();
        if path.extension().and_then(|x| x.to_str()) == Some("json") {
            paths.push(path);
        }
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
        if let Some(p) = &prior {
            if value.revision != p.revision + 1
                || value.previous_revision != Some(p.revision)
                || value.previous_root_hash.as_deref() != Some(&p.new_root_hash)
            {
                return Err(DbError::new(
                    "INTERNAL_METADATA_CORRUPT",
                    format!("broken provenance chain at revision {}", value.revision),
                    6,
                ));
            }
        }
        prior = Some(value);
    }
    if let (Some(last), Some(m)) = (prior, manifest) {
        if last.revision != m.revision || last.new_root_hash != m.root_hash {
            return Err(DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                "manifest does not match the latest provenance record",
                6,
            ));
        }
    }
    Ok(())
}
pub fn provenance_head(root: &Path) -> Result<Option<Manifest>> {
    let dir = root.join(".db/provenance");
    if !dir.exists() {
        return Ok(None);
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| DbError::io(&dir, e))? {
        let path = entry.map_err(|e| DbError::io(&dir, e))?.path();
        if path.extension().and_then(|x| x.to_str()) == Some("json") {
            paths.push(path);
        }
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
pub fn write_provenance(root: &Path, p: &Provenance) -> Result<()> {
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
    fs::create_dir_all(&dir).map_err(|e| DbError::io(&dir, e))?;
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
        if !target.exists() {
            let value = values.get(path).ok_or_else(|| {
                DbError::new(
                    "INTERNAL_METADATA_CORRUPT",
                    format!("cannot capture revision object {path}"),
                    6,
                )
            })?;
            write_json_atomic(&target, value)?;
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
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).map_err(|e| DbError::io(p, e))?
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let mut f = fs::File::create(&tmp).map_err(|e| DbError::io(&tmp, e))?;
    let mut b = serde_json::to_vec_pretty(value).map_err(internal)?;
    b.push(b'\n');
    f.write_all(&b).map_err(|e| DbError::io(&tmp, e))?;
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
fn internal(e: serde_json::Error) -> DbError {
    DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6)
}
