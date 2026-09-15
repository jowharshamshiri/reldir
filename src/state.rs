//! Folder resolution: root discovery, observation, and prerequisite
//! establishment.
//!
//! The filesystem is the database, so opening one is not a ceremony the user
//! performs — it is a state the binary establishes. This module observes a
//! folder without side effects, decides what the requested command needs, and
//! performs only those transitions that cannot destroy authoritative intent.
//!
//! The division is by what a transition does, not by whether it writes:
//! creating a schema that does not exist is additive, replacing one a human
//! wrote is not. Everything additive happens silently; everything replacing
//! requires a decision.

use crate::{
    config::Config,
    diagnostic::{DbError, Diagnostic, Result},
    infer,
    schema::Schema,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How a root was selected, for `--verbose` reporting and for diagnostics that
/// must name the directory they are talking about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootOrigin {
    /// `--db` named it.
    Explicit,
    /// `DB_DIR` named it.
    Environment,
    /// An ancestor carried `.db/`.
    DiscoveredMetadata,
    /// An ancestor carried a recognizable `schema/`.
    DiscoveredSchema,
    /// Nothing was found; the working directory is the candidate.
    WorkingDirectory,
}

#[derive(Debug, Clone)]
pub struct ResolvedRoot {
    pub path: PathBuf,
    pub origin: RootOrigin,
}

/// Whether `.db/` exists and can be interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatState {
    /// No `.db/` at all. A normal bootstrap condition, not a fault.
    Absent,
    /// `.db/` exists and declares a format this binary reads.
    Supported,
    /// `.db/` exists but carries no format marker. Whether this is
    /// recoverable depends on what else is in there.
    MarkerMissing,
    /// `.db/` exists and declares a format this binary cannot read.
    Unsupported(String),
}

/// What the folder contains, judged only from its immediate children.
///
/// Recursive discovery is deliberately not performed: a `node_modules` deep in
/// a project is not a table, and a tool that guessed otherwise would be worse
/// than one that asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topology {
    /// Immediate child directories holding at least one `.json` file.
    pub table_candidates: Vec<String>,
    /// `.json` files sitting directly in the root, which have no table
    /// identity and cannot be adopted without a decision.
    pub loose_json: Vec<String>,
    /// Tables named by a schema file, whether or not rows exist yet.
    pub declared_tables: Vec<String>,
}

impl Topology {
    /// A folder with nothing for the binary to govern.
    pub fn is_empty(&self) -> bool {
        self.table_candidates.is_empty()
            && self.loose_json.is_empty()
            && self.declared_tables.is_empty()
    }
}

/// A folder as observed, without having changed anything.
#[derive(Debug, Clone)]
pub struct Observation {
    pub root: PathBuf,
    pub format: FormatState,
    pub topology: Topology,
    /// Whether the root can be written to. Advisory only: every actual write
    /// still handles denial, races, and quota exhaustion on its own.
    pub writable: bool,
}

impl Observation {
    /// Tables that have rows on disk but no schema to interpret them by.
    pub fn tables_needing_schema(&self) -> Vec<String> {
        self.topology
            .table_candidates
            .iter()
            .filter(|candidate| !self.topology.declared_tables.contains(candidate))
            .cloned()
            .collect()
    }
}

/// What a command needs before it can run.
///
/// Declared per command rather than inferred, so adding a command forces the
/// question to be answered rather than defaulting to the most permissive
/// behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requirements {
    /// The command reads or writes relations, so schemas must exist.
    pub relational_model: bool,
    /// The command may persist state to satisfy its own prerequisites.
    pub may_establish: bool,
}

impl Requirements {
    /// Reads and writes over the relational model: they need schemas, and in
    /// auto mode they may be established.
    pub const fn functional() -> Self {
        Self {
            relational_model: true,
            may_establish: true,
        }
    }

    /// Diagnostics: they need to see whatever is there, and must never change
    /// it. A command that reports what is wrong cannot alter what it reports
    /// on, or it could not be run twice for the same answer.
    pub const fn diagnostic() -> Self {
        Self {
            relational_model: true,
            may_establish: false,
        }
    }

    /// Commands that operate on the folder itself rather than on relations.
    pub const fn structural() -> Self {
        Self {
            relational_model: false,
            may_establish: false,
        }
    }
}

/// A transition the planner selected, reported after it is performed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// Created `.db/` and its metadata where none existed.
    Bootstrapped,
    /// Inferred and wrote schemas for tables that had none.
    InferredSchemas(Vec<String>),
}

impl Transition {
    pub fn describe(&self) -> String {
        match self {
            Self::Bootstrapped => "initialized database".into(),
            Self::InferredSchemas(tables) => {
                format!("inferred schemas for {}", tables.join(", "))
            }
        }
    }

    /// The machine-readable name, part of the structured output contract.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Bootstrapped => "bootstrapped",
            Self::InferredSchemas(_) => "schemas_inferred",
        }
    }
}

/// Resolve the database root.
///
/// An explicitly named root is exact: the binary never walks upward from a path
/// the user supplied and silently operates on a different database. Discovery
/// walks upward only when nothing was named, and the nearest credible marker
/// wins so that a nested database is never skipped in favour of an outer one.
pub fn resolve_root(explicit: Option<&Path>) -> Result<ResolvedRoot> {
    if let Some(path) = explicit {
        return Ok(ResolvedRoot {
            path: absolute(path)?,
            origin: RootOrigin::Explicit,
        });
    }
    if let Some(value) = std::env::var_os("DB_DIR") {
        return Ok(ResolvedRoot {
            path: absolute(Path::new(&value))?,
            origin: RootOrigin::Environment,
        });
    }
    let start = std::env::current_dir().map_err(|error| DbError::io(Path::new("."), error))?;
    let mut current = start.clone();
    loop {
        if current.join(".db").is_dir() {
            return Ok(ResolvedRoot {
                path: current,
                origin: RootOrigin::DiscoveredMetadata,
            });
        }
        if holds_recognizable_schema(&current)? {
            return Ok(ResolvedRoot {
                path: current,
                origin: RootOrigin::DiscoveredSchema,
            });
        }
        if !current.pop() {
            break;
        }
    }
    // Nothing above declares a database, so this directory is the candidate.
    // Absence of metadata is a state to establish, not a failure to report.
    Ok(ResolvedRoot {
        path: start,
        origin: RootOrigin::WorkingDirectory,
    })
}

/// Whether `schema/` here holds at least one file that is actually a schema.
///
/// The test is structural rather than positional: a directory named `schema`
/// full of JSON Schema documents, API definitions, or migrations is not a
/// database root, and re-rooting onto one would silently operate on the wrong
/// directory.
fn holds_recognizable_schema(root: &Path) -> Result<bool> {
    let directory = root.join("schema");
    if !directory.is_dir() {
        return Ok(false);
    }
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        // An unreadable directory is not evidence of a database.
        Err(_) => return Ok(false),
    };
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(_) => continue,
        };
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(value) = crate::json::parse(&bytes) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        let names_its_file = object.get("table").and_then(|t| t.as_str()) == Some(stem);
        let has_key = object
            .get("primary_key")
            .and_then(|k| k.as_array())
            .is_some_and(|k| !k.is_empty());
        let has_columns = object
            .get("columns")
            .and_then(|c| c.as_object())
            .is_some_and(|c| !c.is_empty());
        if names_its_file && has_key && has_columns {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Observe a folder. Performs no writes.
pub fn observe(root: &Path) -> Result<Observation> {
    let metadata_directory = root.join(".db");
    let format = if !metadata_directory.exists() {
        FormatState::Absent
    } else if !metadata_directory.is_dir() {
        return Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            ".db must be a real directory, not a symlink or special file",
            6,
        ));
    } else {
        classify_format(&metadata_directory)?
    };
    Ok(Observation {
        root: root.to_path_buf(),
        format,
        topology: survey(root)?,
        writable: writable(root),
    })
}

fn classify_format(metadata_directory: &Path) -> Result<FormatState> {
    let marker = metadata_directory.join("format");
    if !marker.exists() {
        return Ok(FormatState::MarkerMissing);
    }
    let text = std::fs::read_to_string(&marker).map_err(|error| DbError::io(&marker, error))?;
    let declared = text
        .lines()
        .find_map(|line| line.strip_prefix("format_version = "))
        .and_then(|value| value.trim().parse::<u32>().ok());
    match declared {
        Some(version) if version == crate::FORMAT_VERSION => Ok(FormatState::Supported),
        Some(version) => Ok(FormatState::Unsupported(version.to_string())),
        None => Ok(FormatState::MarkerMissing),
    }
}

/// Survey the immediate children of the root.
fn survey(root: &Path) -> Result<Topology> {
    let mut table_candidates = vec![];
    let mut loose_json = vec![];
    let mut declared_tables = vec![];

    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Topology {
                table_candidates,
                loose_json,
                declared_tables,
            });
        }
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
        let file_type = entry
            .file_type()
            .map_err(|error| DbError::io(&path, error))?;
        if file_type.is_dir() {
            if directory_holds_json(&path)? {
                table_candidates.push(name.to_string());
            }
        } else if file_type.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some("json")
        {
            loose_json.push(name.to_string());
        }
    }

    let schema_directory = root.join("schema");
    if schema_directory.is_dir() {
        for entry in std::fs::read_dir(&schema_directory)
            .map_err(|error| DbError::io(&schema_directory, error))?
        {
            let path = entry
                .map_err(|error| DbError::io(&schema_directory, error))?
                .path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            // `<table>.inferred.json` is a comparison artefact, not a schema.
            if stem.ends_with(".inferred") {
                continue;
            }
            declared_tables.push(stem.to_string());
        }
    }

    table_candidates.sort();
    loose_json.sort();
    declared_tables.sort();
    Ok(Topology {
        table_candidates,
        loose_json,
        declared_tables,
    })
}

fn directory_holds_json(directory: &Path) -> Result<bool> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => return Ok(false),
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("json")
            && entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            return Ok(true);
        }
    }
    Ok(false)
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

/// Establish what the command needs, performing only additive transitions.
///
/// Returns the transitions performed, for reporting. An empty result means the
/// folder already satisfied the command.
pub fn establish(
    observation: &Observation,
    requirements: Requirements,
    overrides: &crate::config::ResourceOverrides,
) -> Result<Vec<Transition>> {
    if !requirements.relational_model || !requirements.may_establish {
        return Ok(vec![]);
    }
    // A folder with nothing in it has nothing to establish. Reporting that it
    // is empty is the correct answer, and creating metadata to say so would be
    // ceremony for its own sake.
    if observation.topology.is_empty() && observation.format == FormatState::Absent {
        return Ok(vec![]);
    }
    if !observation.writable {
        // A read-only medium is not a failure for a read; the caller operates
        // from an in-memory model instead.
        return Ok(vec![]);
    }
    match &observation.format {
        FormatState::Unsupported(version) => {
            return Err(DbError::new(
                "FORMAT_UNSUPPORTED",
                format!(
                    "format {version} is unsupported; this binary supports format {}",
                    crate::FORMAT_VERSION
                ),
                6,
            ));
        }
        FormatState::MarkerMissing => {
            // `.db/` exists but does not say what it is. Guessing a version
            // could misread every byte in the database.
            return Err(DbError::from_diag(
                Diagnostic::error(
                    "FORMAT_MISSING",
                    ".db exists but declares no format version",
                )
                .help("remove .db to re-establish it, or restore .db/format"),
                6,
            ));
        }
        FormatState::Absent | FormatState::Supported => {}
    }

    let bootstrapping = observation.format == FormatState::Absent;

    // Rows sitting at the root have no table identity, and inventing one would
    // be a guess about the user's intent that cannot be undone. This applies
    // only while bootstrapping: in an established database a root-level JSON
    // file is an ordinary file -- a migration document, a note -- and refusing
    // to operate because one exists would govern files the database never
    // claimed.
    if bootstrapping && !observation.topology.loose_json.is_empty() {
        return Err(DbError::from_diag(
            Diagnostic::error(
                "ROOT_JSON_AMBIGUOUS",
                format!(
                    "{} JSON file(s) at the database root have no table identity",
                    observation.topology.loose_json.len()
                ),
            )
            .at(observation.root.clone())
            .help("move them into a named table directory, then rerun"),
            1,
        ));
    }

    let mut transitions = vec![];
    let mut config = Config::default();
    config.apply_overrides(overrides);
    config
        .validate()
        .map_err(|message| DbError::new("RESOURCE_LIMIT", message, 1))?;

    // Inference applies only while bootstrapping. Once a database exists its
    // table set is authoritative intent: a new JSON-bearing directory beside it
    // is an ungoverned sibling the user may or may not want governed, so it is
    // surfaced as UNGOVERNED_DIRECTORY rather than silently adopted. Inferring
    // there would let an unrelated folder dropped into the tree quietly become
    // part of the database.
    let missing = if bootstrapping {
        observation.tables_needing_schema()
    } else {
        vec![]
    };

    // Inference runs before anything is written, so a folder that cannot be
    // interpreted leaves no partial database behind.
    let inferred = if missing.is_empty() {
        BTreeMap::new()
    } else {
        let references = crate::catalog::Catalog::observe(&observation.root, &config)?;
        infer::infer_all_with_references(
            &observation.root,
            &missing,
            infer::Strictness::Balanced,
            &config,
            None,
            Some(&references),
        )?
    };

    if bootstrapping {
        crate::db::init_layout(&observation.root, false)?;
        transitions.push(Transition::Bootstrapped);
    }

    if !inferred.is_empty() {
        for schema in inferred.values() {
            crate::db::write_schema(&observation.root, schema)?;
        }
        transitions.push(Transition::InferredSchemas(
            inferred.keys().cloned().collect(),
        ));
    }

    if bootstrapping {
        // The first revision records what was adopted, with no predecessor it
        // cannot substantiate.
        let catalog = crate::catalog::Catalog::observe(&observation.root, &config)?;
        let (hash, entries) = crate::metadata::state(&catalog)?;
        crate::metadata::record(&catalog, None, hash, entries, "import", None)?;
    }

    Ok(transitions)
}

/// Schemas a read needs but that must not be persisted.
///
/// Used where the command promises not to write: the relational model is built
/// in memory so the read can be answered, and the folder is left untouched.
pub fn ephemeral_schemas(
    observation: &Observation,
    overrides: &crate::config::ResourceOverrides,
) -> Result<BTreeMap<String, Schema>> {
    let missing = observation.tables_needing_schema();
    if missing.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut config = Config::default();
    config.apply_overrides(overrides);
    config
        .validate()
        .map_err(|message| DbError::new("RESOURCE_LIMIT", message, 1))?;
    let references = crate::catalog::Catalog::observe(&observation.root, &config)?;
    infer::infer_all_with_references(
        &observation.root,
        &missing,
        infer::Strictness::Balanced,
        &config,
        None,
        Some(&references),
    )
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn schema_json(table: &str) -> String {
        format!(
            r#"{{"table":"{table}","primary_key":["id"],"columns":{{"id":{{"type":"string"}}}}}}"#
        )
    }

    /// A folder with nothing in it is a legitimate state, not a fault. The
    /// observation must say so rather than reporting an error a user would
    /// have to resolve before asking their first question.
    #[test]
    fn test9999_an_empty_folder_observes_as_empty() {
        let directory = tempfile::tempdir().unwrap();
        let observed = observe(directory.path()).unwrap();
        assert_eq!(observed.format, FormatState::Absent);
        assert!(observed.topology.is_empty());
        assert!(observed.tables_needing_schema().is_empty());
    }

    /// Table candidates are immediate children holding JSON. Nested JSON deeper
    /// in a project is not a table: a tool that recursively adopted arbitrary
    /// files would consume directories the user never meant to govern.
    #[test]
    fn test9999_table_candidates_are_immediate_children_only() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(&root.join("users/u1.json"), "{\"id\":\"u1\"}\n");
        write(&root.join("deep/nested/inside.json"), "{\"id\":\"x\"}\n");
        write(&root.join("empty_dir/readme.txt"), "not json\n");

        let observed = observe(root).unwrap();
        assert_eq!(
            observed.topology.table_candidates,
            vec!["users".to_string()]
        );
        assert!(!observed.topology.table_candidates.contains(&"deep".into()));
        assert!(
            !observed
                .topology
                .table_candidates
                .contains(&"empty_dir".into())
        );
    }

    /// Rows at the root have no table identity. Inventing a name would be an
    /// irreversible guess, so establishment refuses and explains instead.
    #[test]
    fn test9999_loose_root_json_is_refused_rather_than_guessed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(&root.join("a.json"), "{\"id\":\"a\"}\n");

        let observed = observe(root).unwrap();
        assert_eq!(observed.topology.loose_json, vec!["a.json".to_string()]);

        let error = establish(&observed, Requirements::functional(), &Default::default())
            .expect_err("loose rows cannot be adopted");
        assert_eq!(error.diagnostic.code, "ROOT_JSON_AMBIGUOUS");
        // Nothing was created while refusing.
        assert!(!root.join(".db").exists());
    }

    /// A directory named `schema` only re-roots discovery when it actually
    /// holds schemas. JSON Schema documents and migrations must not capture an
    /// unrelated project.
    #[test]
    fn test9999_schema_marker_recognition_is_structural() {
        let unrelated = tempfile::tempdir().unwrap();
        write(
            &unrelated.path().join("schema/openapi.json"),
            r#"{"openapi":"3.0.0","paths":{}}"#,
        );
        assert!(
            !holds_recognizable_schema(unrelated.path()).unwrap(),
            "unrelated JSON must not be taken for a schema"
        );

        // A file whose `table` disagrees with its name is not a schema either.
        let mismatched = tempfile::tempdir().unwrap();
        write(
            &mismatched.path().join("schema/users.json"),
            &schema_json("other"),
        );
        assert!(!holds_recognizable_schema(mismatched.path()).unwrap());

        let real = tempfile::tempdir().unwrap();
        write(
            &real.path().join("schema/users.json"),
            &schema_json("users"),
        );
        assert!(holds_recognizable_schema(real.path()).unwrap());
    }

    /// Diagnostic commands observe without establishing. A command that reports
    /// what is wrong must not change what it reports on.
    #[test]
    fn test9999_diagnostic_requirements_establish_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(&root.join("users/u1.json"), "{\"id\":\"u1\"}\n");

        let observed = observe(root).unwrap();
        let transitions =
            establish(&observed, Requirements::diagnostic(), &Default::default()).unwrap();
        assert!(transitions.is_empty());
        assert!(!root.join(".db").exists(), "diagnosis must not bootstrap");
        assert!(!root.join("schema").exists());
    }

    /// An empty folder establishes nothing even for a functional command:
    /// there is nothing to govern, and creating metadata to say "empty" is
    /// ceremony.
    #[test]
    fn test9999_an_empty_folder_establishes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let observed = observe(directory.path()).unwrap();
        let transitions =
            establish(&observed, Requirements::functional(), &Default::default()).unwrap();
        assert!(transitions.is_empty());
        assert!(!directory.path().join(".db").exists());
    }

    /// Data with no metadata is adopted in one step, and the result is a real
    /// database: metadata, schemas, and a first revision.
    #[test]
    fn test9999_ungoverned_data_is_adopted_without_ceremony() {
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

        let observed = observe(root).unwrap();
        let transitions =
            establish(&observed, Requirements::functional(), &Default::default()).unwrap();

        assert!(transitions.contains(&Transition::Bootstrapped));
        assert!(transitions.iter().any(
            |t| matches!(t, Transition::InferredSchemas(tables) if tables == &["users".to_string()])
        ));
        assert!(root.join(".db/format").exists());
        assert!(root.join(".db/config").exists());
        assert!(root.join("schema/users.json").exists());
        assert_eq!(
            std::fs::read_dir(root.join(".db/provenance"))
                .unwrap()
                .count(),
            1,
            "adoption records exactly one initial revision"
        );
    }

    /// An existing schema is authoritative intent. Establishment fills the gaps
    /// around it and never rewrites it.
    #[test]
    fn test9999_existing_schemas_are_preserved_byte_for_byte() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        // Deliberately non-canonical formatting, so any rewrite is detectable.
        let handwritten = "{\n    \"table\": \"users\",\n    \"primary_key\": [\"id\"],\n    \"columns\": {\"id\": {\"type\": \"string\"}}\n}\n";
        write(&root.join("schema/users.json"), handwritten);
        write(&root.join("users/u1.json"), "{\"id\":\"u1\"}\n");
        write(&root.join("posts/p1.json"), "{\"id\":\"p1\"}\n");

        let observed = observe(root).unwrap();
        establish(&observed, Requirements::functional(), &Default::default()).unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("schema/users.json")).unwrap(),
            handwritten,
            "an existing schema must survive establishment untouched"
        );
        assert!(
            root.join("schema/posts.json").exists(),
            "only the missing schema is inferred"
        );
    }

    /// Inference failure leaves nothing behind. A folder that could not be
    /// interpreted must look exactly as it did before the attempt.
    #[test]
    fn test9999_failed_inference_writes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        // An array root cannot be a row, so inference cannot type this table.
        write(&root.join("things/a.json"), "[1,2]\n");

        let observed = observe(root).unwrap();
        let error = establish(&observed, Requirements::functional(), &Default::default())
            .expect_err("inference cannot succeed here");
        assert!(error.diagnostic.code.starts_with("INFER_"));
        assert!(!root.join(".db").exists(), "no partial bootstrap remains");
        assert!(!root.join("schema").exists());
    }

    /// `.db/` that does not declare its format is never guessed at: reading a
    /// database under the wrong format could misinterpret every byte.
    #[test]
    fn test9999_metadata_without_a_format_marker_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join(".db")).unwrap();
        write(&root.join("users/u1.json"), "{\"id\":\"u1\"}\n");

        let observed = observe(root).unwrap();
        assert_eq!(observed.format, FormatState::MarkerMissing);
        let error = establish(&observed, Requirements::functional(), &Default::default())
            .expect_err("an unlabelled database cannot be interpreted");
        assert_eq!(error.diagnostic.code, "FORMAT_MISSING");
    }

    /// A format this binary cannot read stops the operation rather than being
    /// bootstrapped over.
    #[test]
    fn test9999_unsupported_format_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(
            &root.join(".db/format"),
            &format!("format_version = {}\n", crate::FORMAT_VERSION + 1),
        );
        let observed = observe(root).unwrap();
        assert!(matches!(observed.format, FormatState::Unsupported(_)));
        let error = establish(&observed, Requirements::functional(), &Default::default())
            .expect_err("a newer format cannot be read");
        assert_eq!(error.diagnostic.code, "FORMAT_UNSUPPORTED");
    }

    /// Schemas needed only to answer a read can be produced without touching
    /// the folder, which is what makes read-only operation possible on
    /// ungoverned data.
    #[test]
    fn test9999_ephemeral_schemas_leave_the_folder_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(
            &root.join("users/u1.json"),
            "{\"id\":\"u1\",\"name\":\"A\"}\n",
        );

        let observed = observe(root).unwrap();
        let schemas = ephemeral_schemas(&observed, &Default::default()).unwrap();
        assert!(schemas.contains_key("users"));
        assert!(!root.join(".db").exists());
        assert!(!root.join("schema").exists());
    }

    /// An explicitly named root is used exactly. Walking upward from a path the
    /// user supplied could operate on a different database than the one they
    /// named.
    #[test]
    fn test9999_an_explicit_root_is_never_walked_upward_from() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join(".db")).unwrap();
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();

        let resolved = resolve_root(Some(&child)).unwrap();
        assert_eq!(resolved.path, child);
        assert_eq!(resolved.origin, RootOrigin::Explicit);
    }
}
