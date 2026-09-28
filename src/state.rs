//! Finding the database, and establishing it where it does not exist yet.
//!
//! The filesystem is the database, so opening one is not a ceremony: the binary
//! observes the folder without side effects, decides what the command needs,
//! and performs only transitions that cannot destroy anything a person wrote.
//! Creating metadata that does not exist is additive and happens on its own;
//! discarding metadata that holds history requires an explicit decision.

use crate::{
    config::{Config, ResourceOverrides},
    db::{Access, Database},
    diagnostic::{DbError, Diagnostic, Result},
    schema::Schema,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// How the root was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootOrigin {
    /// `--db` named it.
    Explicit,
    /// `RELDIR_DB` named it.
    Environment,
    /// An ancestor of the working directory holds `.db/`.
    Discovered,
    /// Nothing declares a database; the working directory is the candidate.
    WorkingDirectory,
}

#[derive(Debug, Clone)]
pub struct ResolvedRoot {
    pub path: PathBuf,
    pub origin: RootOrigin,
}

/// Resolve the database root.
///
/// A named root is exact and must exist: a mistyped `--db` is an error, never
/// an empty database that reports itself valid. Discovery walks upward from the
/// working directory to the nearest `.db/`, and refuses to guess when a folder
/// of pins without metadata sits inside another database.
pub fn resolve_root(explicit: Option<&Path>) -> Result<ResolvedRoot> {
    let named = match explicit {
        Some(path) => Some((path.to_path_buf(), RootOrigin::Explicit, "--db")),
        None => std::env::var_os("RELDIR_DB")
            .filter(|value| !value.is_empty())
            .map(|value| (PathBuf::from(value), RootOrigin::Environment, "RELDIR_DB")),
    };
    if let Some((path, origin, source)) = named {
        let path = absolute(&path)?;
        require_directory(&path, source)?;
        return Ok(ResolvedRoot { path, origin });
    }
    let start = std::env::current_dir().map_err(|error| DbError::io(Path::new("."), error))?;
    let mut pinned_without_metadata: Option<PathBuf> = None;
    let mut current = start.clone();
    loop {
        if current.join(".db").is_dir() {
            if let Some(inner) = pinned_without_metadata {
                return Err(DbError::from_diag(
                    Diagnostic::error(
                        "ROOT_AMBIGUOUS",
                        format!(
                            "{} holds reldir schemas but no .db/, inside the database at {}; it could be a \
                             table of that database or a database of its own",
                            inner.display(),
                            current.display()
                        ),
                    )
                    .help(format!(
                        "name the one you mean: --db {} or --db {}",
                        current.display(),
                        inner.display()
                    )),
                    1,
                ));
            }
            return Ok(ResolvedRoot {
                path: current,
                origin: RootOrigin::Discovered,
            });
        }
        if pinned_without_metadata.is_none() && holds_pins(&current) {
            pinned_without_metadata = Some(current.clone());
        }
        if !current.pop() {
            break;
        }
    }
    Ok(ResolvedRoot {
        path: pinned_without_metadata.unwrap_or(start),
        origin: RootOrigin::WorkingDirectory,
    })
}

fn require_directory(path: &Path, source: &str) -> Result<()> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(DbError::from_diag(
            Diagnostic::error(
                "PATH_NOT_DIRECTORY",
                format!(
                    "{source} names {}, which is not a directory",
                    path.display()
                ),
            )
            .at(path),
            1,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(DbError::from_diag(
            Diagnostic::error(
                "PATH_NOT_FOUND",
                format!("{source} names {}, which does not exist", path.display()),
            )
            .at(path)
            .help("check the path; `reldir init <path>` creates a new database"),
            1,
        )),
        Err(error) => Err(DbError::io(path, error)),
    }
}

/// Whether `schema/` here holds at least one document in reldir's dialect whose
/// table matches its file. Ordinary JSON Schema documents are not pins.
fn holds_pins(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root.join("schema")) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(String::from);
        path.extension().and_then(|value| value.to_str()) == Some("json")
            && std::fs::read(&path)
                .ok()
                .and_then(|bytes| crate::json::parse(&bytes).ok())
                .is_some_and(|value| {
                    value.get("$schema").and_then(Value::as_str)
                        == Some(crate::schema::meta::DIALECT_URI)
                        && value.pointer("/x-reldir/table").and_then(Value::as_str)
                            == stem.as_deref()
                })
    })
}

/// Whether `.db/` exists and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatState {
    Absent,
    /// A format marker is present; [`Database::open`] judges its version.
    Present,
    /// No marker, and nothing that could not be rebuilt from the files.
    MarkerMissingRecoverable,
    /// No marker, and state only this directory holds: history, snapshots,
    /// configuration, or entries reldir does not recognise.
    MarkerMissingUnrecoverable(Vec<String>),
}

/// The folder's immediate contents. Discovery is not recursive: a
/// `node_modules` deep in a project is not a table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Topology {
    /// Child directories holding at least one `.json` file.
    pub table_candidates: Vec<String>,
    /// `.json` files directly in the root, which belong to no table.
    pub loose_json: Vec<String>,
    /// Tables declared by a pin.
    pub pinned: Vec<String>,
}

impl Topology {
    pub fn is_empty(&self) -> bool {
        self.table_candidates.is_empty() && self.loose_json.is_empty() && self.pinned.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct Observation {
    pub root: PathBuf,
    pub format: FormatState,
    pub topology: Topology,
    /// Whether the root's permissions allow writing. Advisory: every write
    /// still handles refusal on its own.
    pub writable: bool,
}

/// Observe a folder. Writes nothing.
pub fn observe(root: &Path) -> Result<Observation> {
    let meta = root.join(".db");
    let format = match std::fs::symlink_metadata(&meta) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => FormatState::Absent,
        Err(error) => return Err(DbError::io(&meta, error)),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                ".db must be a real directory, not a symlink or special file",
                6,
            ));
        }
        Ok(_) if meta.join("format").exists() => FormatState::Present,
        Ok(_) => {
            let lost = irreplaceable(&meta)?;
            if lost.is_empty() {
                FormatState::MarkerMissingRecoverable
            } else {
                FormatState::MarkerMissingUnrecoverable(lost)
            }
        }
    };
    Ok(Observation {
        root: root.to_path_buf(),
        format,
        topology: survey(root)?,
        writable: writable(root),
    })
}

/// Entries of `.db/` that are derived from the files and can be rebuilt.
const REBUILDABLE: &[&str] = &[
    "format",
    "mirror.sqlite",
    "mirror.sqlite-journal",
    "transactions",
    "schema",
    "lock",
    ".gitignore",
];

fn irreplaceable(meta: &Path) -> Result<Vec<String>> {
    let mut out = vec![];
    for entry in std::fs::read_dir(meta).map_err(|error| DbError::io(meta, error))? {
        let path = entry.map_err(|error| DbError::io(meta, error))?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if REBUILDABLE.contains(&name) {
            continue;
        }
        let empty_directory = matches!(name, "provenance" | "objects" | "snapshots")
            && std::fs::read_dir(&path).is_ok_and(|mut entries| entries.next().is_none());
        if !empty_directory {
            out.push(name.to_string());
        }
    }
    out.sort();
    Ok(out)
}

fn survey(root: &Path) -> Result<Topology> {
    let mut topology = Topology::default();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(topology),
        Err(error) => return Err(DbError::io(root, error)),
    };
    for entry in entries {
        let entry = entry.map_err(|error| DbError::io(root, error))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || name == "schema" {
            continue;
        }
        let kind = entry
            .file_type()
            .map_err(|error| DbError::io(&path, error))?;
        if kind.is_dir() {
            let holds_json = std::fs::read_dir(&path).is_ok_and(|entries| {
                entries.flatten().any(|child| {
                    child.path().extension().and_then(|value| value.to_str()) == Some("json")
                        && child.file_type().is_ok_and(|kind| kind.is_file())
                })
            });
            if holds_json {
                topology.table_candidates.push(name.to_string());
            }
        } else if kind.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some("json")
        {
            topology.loose_json.push(name.to_string());
        }
    }
    if let Ok(entries) = std::fs::read_dir(crate::schema_store::pin_dir(root)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
            {
                topology.pinned.push(stem.to_string());
            }
        }
    }
    topology.table_candidates.sort();
    topology.loose_json.sort();
    topology.pinned.sort();
    Ok(topology)
}

fn writable(root: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(root) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o200 != 0
    }
    #[cfg(not(unix))]
    {
        !metadata.permissions().readonly()
    }
}

/// How a command wants the database opened.
#[derive(Debug, Clone, Copy)]
pub struct Opening {
    pub access: Access,
    /// May establish a database where none exists.
    pub establish: bool,
    /// May discard a `.db/` whose format marker is missing even though it holds
    /// history -- the user's explicit decision.
    pub rebuild_metadata: bool,
    /// Report what establishing would do without doing it.
    pub dry_run: bool,
}

/// Something establishing did, or -- in a dry run -- would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// Created `.db/`, inferring schemas for these tables.
    Bootstrapped { inferred: Vec<String> },
    /// Discarded an unlabelled `.db/`, and what it held.
    RebuiltMetadata(Vec<String>),
}

impl Transition {
    pub fn describe(&self, planned: bool) -> String {
        let (verb_init, verb_rebuild) = if planned {
            ("would initialize", "would rebuild")
        } else {
            ("initialized", "rebuilt")
        };
        match self {
            Self::Bootstrapped { inferred } if inferred.is_empty() => {
                format!("{verb_init} the database")
            }
            Self::Bootstrapped { inferred } => {
                format!(
                    "{verb_init} the database, inferring schemas for {}",
                    inferred.join(", ")
                )
            }
            Self::RebuiltMetadata(lost) if lost.is_empty() => {
                format!("{verb_rebuild} unlabelled metadata")
            }
            Self::RebuiltMetadata(lost) => format!(
                "{verb_rebuild} unlabelled metadata, discarding {}",
                lost.join(", ")
            ),
        }
    }

    pub fn to_json(&self, planned: bool) -> Value {
        match self {
            Self::Bootstrapped { inferred } => {
                json!({"transition": "bootstrapped", "inferred": inferred, "planned": planned})
            }
            Self::RebuiltMetadata(lost) => {
                json!({"transition": "metadata_rebuilt", "discarded": lost, "planned": planned})
            }
        }
    }
}

/// What opening found.
pub enum Opened {
    /// Nothing to govern: no metadata, no tables, no pins.
    Empty,
    Database {
        database: Box<Database>,
        transitions: Vec<Transition>,
        planned: bool,
    },
}

/// Open the database at `root`, establishing it when the command may.
pub fn open(root: &Path, opening: Opening, overrides: &ResourceOverrides) -> Result<Opened> {
    let observation = observe(root)?;
    let access = if observation.writable {
        opening.access
    } else {
        Access::Read
    };
    let may_write = access == Access::Write && !opening.dry_run;
    let mut transitions = vec![];
    match &observation.format {
        FormatState::Present => {
            let database = Database::open(root.to_path_buf(), access, overrides)?;
            return Ok(Opened::Database {
                database: Box::new(database),
                transitions,
                planned: false,
            });
        }
        FormatState::MarkerMissingUnrecoverable(lost) if !opening.rebuild_metadata => {
            return Err(DbError::from_diag(
                Diagnostic::error(
                    "FORMAT_MISSING",
                    format!(".db declares no format version and holds state that cannot be rebuilt: {}", lost.join(", ")),
                )
                .help("restore .db/format, or pass --rebuild-metadata to discard .db and establish it again from your files"),
                6,
            ));
        }
        FormatState::MarkerMissingRecoverable | FormatState::MarkerMissingUnrecoverable(_) => {
            let lost = match &observation.format {
                FormatState::MarkerMissingUnrecoverable(lost) => lost.clone(),
                _ => vec![],
            };
            if !may_write || !opening.establish {
                return Err(DbError::from_diag(
                    Diagnostic::error("FORMAT_MISSING", ".db declares no format version")
                        .help("run a writing command without --readonly or --no-auto to rebuild it from your files"),
                    6,
                ));
            }
            let meta = root.join(".db");
            std::fs::remove_dir_all(&meta).map_err(|error| DbError::io(&meta, error))?;
            transitions.push(Transition::RebuiltMetadata(lost));
        }
        FormatState::Absent => {}
    }
    if observation.topology.is_empty() {
        return Ok(Opened::Empty);
    }
    if !opening.establish {
        return Err(DbError::from_diag(
            Diagnostic::error(
                "UNINITIALIZED",
                format!("{} has no .db metadata", root.display()),
            )
            .help(format!(
                "run `reldir init {}`, or omit --no-auto to establish it on first use",
                root.display()
            )),
            10,
        ));
    }
    if !observation.topology.loose_json.is_empty() {
        return Err(DbError::from_diag(
            Diagnostic::error(
                "ROOT_JSON_AMBIGUOUS",
                format!(
                    "{} JSON file(s) sit at the database root and belong to no table: {}",
                    observation.topology.loose_json.len(),
                    observation.topology.loose_json.join(", ")
                ),
            )
            .at(root)
            .help("move them into a table directory, or add them to .db/config's ignore list after init"),
            1,
        ));
    }
    let schemas = schemas_for(&observation, overrides)?;
    let inferred: Vec<String> = observation
        .topology
        .table_candidates
        .iter()
        .filter(|table| !observation.topology.pinned.contains(table))
        .cloned()
        .collect();
    transitions.push(Transition::Bootstrapped {
        inferred: inferred.clone(),
    });
    if may_write {
        let database = Database::create(root.to_path_buf(), &schemas, false, overrides)?;
        Ok(Opened::Database {
            database: Box::new(database),
            transitions,
            planned: false,
        })
    } else {
        let database = Database::ephemeral(root.to_path_buf(), schemas, overrides)?;
        Ok(Opened::Database {
            database: Box::new(database),
            transitions,
            planned: true,
        })
    }
}

/// Schemas inferred for the tables of a folder without metadata that nobody
/// pinned. Pins are read by the catalog itself, so a fault in one is reported
/// like any other schema fault.
fn schemas_for(
    observation: &Observation,
    overrides: &ResourceOverrides,
) -> Result<BTreeMap<String, Schema>> {
    let mut config = Config::default();
    config.apply_overrides(overrides);
    config
        .validate()
        .map_err(|message| DbError::new("RESOURCE_LIMIT", message, 1))?;
    let mut out = BTreeMap::new();
    let missing: Vec<String> = observation
        .topology
        .table_candidates
        .iter()
        .filter(|table| !observation.topology.pinned.contains(table))
        .cloned()
        .collect();
    if !missing.is_empty() {
        out.extend(crate::infer::infer_all(
            &observation.root,
            &missing,
            crate::infer::Strictness::Balanced,
            &config,
            None,
            None,
        )?);
    }
    Ok(out)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()
        .map_err(|error| DbError::io(Path::new("."), error))?
        .join(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn opening(access: Access) -> Opening {
        Opening {
            access,
            establish: true,
            rebuild_metadata: false,
            dry_run: false,
        }
    }

    /// The false green a mistyped `--db` used to produce: an empty, valid
    /// database where the user meant their data.
    #[test]
    fn test1104_a_named_root_must_exist_and_be_a_directory() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("typo");
        assert_eq!(
            resolve_root(Some(&missing)).unwrap_err().diagnostic.code,
            "PATH_NOT_FOUND"
        );
        write(&directory.path().join("file"), "x");
        assert_eq!(
            resolve_root(Some(&directory.path().join("file")))
                .unwrap_err()
                .diagnostic
                .code,
            "PATH_NOT_DIRECTORY"
        );
        let child = directory.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::create_dir_all(directory.path().join(".db")).unwrap();
        let resolved = resolve_root(Some(&child)).unwrap();
        assert_eq!(
            resolved.path, child,
            "a named root is exact, never walked upward from"
        );
        assert_eq!(resolved.origin, RootOrigin::Explicit);
    }

    #[test]
    fn test1093_table_candidates_are_immediate_children_only() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(&root.join("users/u1.json"), "{\"id\":\"u1\"}\n");
        write(&root.join("deep/nested/inside.json"), "{}\n");
        write(&root.join("empty_dir/readme.txt"), "not json\n");
        assert_eq!(
            observe(root).unwrap().topology.table_candidates,
            vec!["users".to_string()]
        );
    }

    #[test]
    fn test1094_loose_root_json_is_refused_and_nothing_is_created() {
        let directory = tempfile::tempdir().unwrap();
        write(&directory.path().join("a.json"), "{\"id\":\"a\"}\n");
        let error = open(
            directory.path(),
            opening(Access::Write),
            &Default::default(),
        )
        .err()
        .unwrap();
        assert_eq!(error.diagnostic.code, "ROOT_JSON_AMBIGUOUS");
        assert!(!directory.path().join(".db").exists());
    }

    #[test]
    fn test1097_an_empty_folder_is_empty_and_establishes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        assert!(matches!(
            open(
                directory.path(),
                opening(Access::Write),
                &Default::default()
            )
            .unwrap(),
            Opened::Empty
        ));
        assert!(!directory.path().join(".db").exists());
    }

    #[test]
    fn test1098_ungoverned_data_is_adopted_in_one_step() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(
            &root.join("users/u1.json"),
            "{\"id\":\"u1\",\"name\":\"A\"}\n",
        );
        write(
            &root.join("users/u2.json"),
            "{\"id\":\"u2\",\"name\":\"B\"}\n",
        );
        let Opened::Database {
            database,
            transitions,
            planned,
        } = open(root, opening(Access::Write), &Default::default()).unwrap()
        else {
            panic!("a folder with data is a database")
        };
        assert!(!planned);
        assert_eq!(
            transitions,
            vec![Transition::Bootstrapped {
                inferred: vec!["users".into()]
            }]
        );
        assert!(database.is_valid(), "{:?}", database.verdict.errors);
        assert!(root.join(".db/schema/users.json").exists());
        assert!(
            !root.join("schema").exists(),
            "adoption infers; it does not declare"
        );
        assert_eq!(crate::metadata::revisions(root).unwrap(), vec![1]);
        assert_eq!(
            crate::metadata::head(root).unwrap().unwrap().origin,
            "import"
        );
    }

    #[test]
    fn test1099_a_pin_is_read_where_it_lies_and_never_rewritten() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let handwritten = format!(
            "{{\n    \"$schema\": \"{}\",\n    \"type\": \"object\",\n    \"properties\": {{\"id\": {{\"type\": \"string\"}}}},\n    \"required\": [\"id\"],\n    \"additionalProperties\": false,\n    \"x-reldir\": {{\"table\": \"users\", \"primaryKey\": [\"id\"]}}\n}}\n",
            crate::schema::meta::DIALECT_URI
        );
        write(&root.join("schema/users.json"), &handwritten);
        write(&root.join("users/u1.json"), "{\"id\":\"u1\"}\n");
        write(&root.join("posts/p1.json"), "{\"id\":\"p1\"}\n");
        let Opened::Database { database, .. } =
            open(root, opening(Access::Write), &Default::default()).unwrap()
        else {
            panic!()
        };
        assert!(database.is_valid(), "{:?}", database.verdict.errors);
        assert_eq!(
            std::fs::read_to_string(root.join("schema/users.json")).unwrap(),
            handwritten
        );
        assert!(
            !root.join(".db/schema/users.json").exists(),
            "a pinned table has no second copy"
        );
        assert!(root.join(".db/schema/posts.json").exists());
    }

    #[test]
    fn test1100_failed_inference_writes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        write(&directory.path().join("things/a.json"), "[1,2]\n");
        let error = open(
            directory.path(),
            opening(Access::Write),
            &Default::default(),
        )
        .err()
        .unwrap();
        assert!(
            error.diagnostic.code.starts_with("INFER_"),
            "{}",
            error.diagnostic.code
        );
        assert!(!directory.path().join(".db").exists());
    }

    #[test]
    fn test1101_unlabelled_metadata_is_rebuilt_only_when_nothing_is_lost() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join(".db/transactions")).unwrap();
        write(&root.join("users/u1.json"), "{\"id\":\"u1\"}\n");
        let Opened::Database { transitions, .. } =
            open(root, opening(Access::Write), &Default::default()).unwrap()
        else {
            panic!()
        };
        assert_eq!(transitions[0], Transition::RebuiltMetadata(vec![]));

        let historic = tempfile::tempdir().unwrap();
        let root = historic.path();
        write(
            &root.join(".db/provenance/00000000000000000001.json"),
            "{}\n",
        );
        write(&root.join("users/u1.json"), "{\"id\":\"u1\"}\n");
        let error = open(root, opening(Access::Write), &Default::default())
            .err()
            .unwrap();
        assert_eq!(error.diagnostic.code, "FORMAT_MISSING");
        assert!(error.diagnostic.message.contains("provenance"));
        let authorized = Opening {
            rebuild_metadata: true,
            ..opening(Access::Write)
        };
        let Opened::Database { transitions, .. } =
            open(root, authorized, &Default::default()).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            transitions[0],
            Transition::RebuiltMetadata(vec!["provenance".into()])
        );
    }

    #[test]
    fn test1103_read_access_answers_without_touching_the_folder() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(
            &root.join("users/u1.json"),
            "{\"id\":\"u1\",\"name\":\"A\"}\n",
        );
        let Opened::Database {
            database, planned, ..
        } = open(root, opening(Access::Read), &Default::default()).unwrap()
        else {
            panic!()
        };
        assert!(planned);
        assert!(database.catalog.schemas.contains_key("users"));
        assert!(!root.join(".db").exists());
    }
}
