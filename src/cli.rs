use crate::{
    canonical,
    db::{Database, ObserveMode},
    diagnostic::{DbError, Result},
    infer::{self, Strictness},
    metadata,
    output::{self, Format},
    schema::{Check, Column, ColumnType, ForeignKey, Generated, GeneratedKind, Schema},
    transaction::{self, Change},
};
use clap::{CommandFactory, Parser, Subcommand};
use rustyline::{
    Context as ReadlineContext, Editor, Helper,
    completion::{Completer, Pair},
    error::ReadlineError,
    highlight::Highlighter,
    hint::Hinter,
    history::DefaultHistory,
    validate::Validator,
};
use serde_json::{Map, Value};
use std::{
    fs,
    io::{self, BufRead, IsTerminal, Read, Write},
    path::{Path, PathBuf},
};

#[derive(Parser, Clone)]
#[command(
    name = "db",
    version,
    about = "Filesystem-native relational JSON database"
)]
pub struct Cli {
    #[arg(long, global = true, env = "DB_DIR")]
    db: Option<PathBuf>,
    #[arg(long, global = true)]
    readonly: bool,
    #[arg(long,global=true,value_parser=["table","json","jsonl","csv","sqlite"])]
    format: Option<String>,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    quiet: bool,
    #[arg(long, global = true)]
    verbose: bool,
    #[arg(long, global = true)]
    no_color: bool,
    #[arg(long, global = true)]
    yes: bool,
    #[arg(long, global = true)]
    dry_run: bool,
    #[arg(long, global = true)]
    timeout: Option<u64>,
    #[command(subcommand)]
    command: Command,
}
impl Cli {
    pub fn machine_error_format(&self) -> Option<&str> {
        if self.json {
            Some("json")
        } else {
            self.format
                .as_deref()
                .filter(|x| matches!(*x, "json" | "jsonl"))
        }
    }
}
#[derive(Subcommand, Clone)]
enum Command {
    #[command(
        about = "Initialize a database",
        after_help = "Example: db init ./data --adopt"
    )]
    Init {
        path: Option<PathBuf>,
        #[arg(long)]
        adopt: bool,
        #[arg(long)]
        track_provenance: bool,
    },
    #[command(about = "Show validity and changes", after_help = "Example: db status")]
    Status,
    #[command(
        about = "Inspect a directory without adoption",
        after_help = "Example: db inspect ./data"
    )]
    Inspect { path: Option<PathBuf> },
    #[command(
        about = "Fully validate the database",
        after_help = "Example: db check --no-write"
    )]
    Check {
        #[arg(long)]
        no_write: bool,
        #[arg(long)]
        strict: bool,
    },
    #[command(
        about = "Infer explicit schemas",
        after_help = "Example: db infer users --write"
    )]
    Infer {
        table: Option<String>,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        all: bool,
        #[arg(long, default_value = "balanced")]
        strictness: String,
        #[arg(long, value_delimiter = ',')]
        pk: Vec<String>,
    },
    #[command(
        about = "Report schema strengthening opportunities",
        after_help = "Example: db lint --strict"
    )]
    Lint {
        table: Option<String>,
        #[arg(long)]
        strict: bool,
        #[arg(long)]
        descriptions: bool,
    },
    #[command(
        about = "Diagnose and repair",
        after_help = "Example: db doctor --fix --yes"
    )]
    Doctor {
        #[arg(long)]
        fix: bool,
        #[arg(long)]
        allow_data: bool,
        #[arg(long)]
        only: Option<String>,
        #[arg(long)]
        explain: Option<String>,
        #[arg(long)]
        no_snapshot: bool,
    },
    #[command(about = "List tables", after_help = "Example: db tables")]
    Tables,
    #[command(about = "Describe a table", after_help = "Example: db describe users")]
    Describe { table: String },
    #[command(
        about = "Get a row by primary key",
        after_help = "Example: db get users abc"
    )]
    Get { table: String, key: String },
    #[command(
        about = "List table rows",
        after_help = "Example: db list users --limit 10"
    )]
    List {
        table: String,
        #[arg(long, name = "where")]
        where_expr: Option<String>,
        #[arg(long)]
        order: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    #[command(
        about = "Insert rows",
        after_help = "Example: db insert users '{\"id\":\"abc\"}'"
    )]
    Insert {
        table: String,
        json_value: Option<String>,
        #[arg(long)]
        from: Option<String>,
    },
    #[command(
        about = "Patch one row",
        after_help = "Example: db update users abc '{\"name\":\"Alice\"}'"
    )]
    Update {
        table: String,
        key: String,
        patch: String,
    },
    #[command(about = "Delete one row", after_help = "Example: db delete users abc")]
    Delete { table: String, key: String },
    #[command(
        about = "Execute SQL",
        after_help = "Example: db sql 'SELECT * FROM users'"
    )]
    Sql {
        sql: String,
        #[arg(long = "param")]
        params: Vec<String>,
        #[arg(long)]
        explain: bool,
        #[arg(long)]
        explain_analyze: bool,
    },
    #[command(
        about = "Explain SQL",
        after_help = "Example: db explain 'SELECT * FROM users'"
    )]
    Explain { sql: String },
    #[command(subcommand, about = "Manage schemas")]
    Schema(SchemaCommand),
    #[command(
        about = "Export a table",
        after_help = "Example: db export users --format csv --out users.csv"
    )]
    Export {
        table: String,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    #[command(
        about = "Import rows transactionally",
        after_help = "Example: db import users --from users.jsonl"
    )]
    Import {
        table: String,
        #[arg(long)]
        from: PathBuf,
    },
    #[command(about = "Show semantic changes", after_help = "Example: db diff")]
    Diff {
        args: Vec<String>,
        #[arg(long)]
        schema: bool,
    },
    #[command(about = "Show revision history", after_help = "Example: db log")]
    Log,
    #[command(about = "Show a revision", after_help = "Example: db show 1")]
    Show { revision: u64 },
    #[command(subcommand, about = "Manage snapshots")]
    Snapshot(SnapshotCommand),
    #[command(
        about = "Recover interrupted transactions",
        after_help = "Example: db recover"
    )]
    Recover,
    #[command(about = "Rebuild indexes", after_help = "Example: db reindex")]
    Reindex,
    #[command(about = "Rebuild query statistics", after_help = "Example: db analyze")]
    Analyze,
    #[command(
        about = "Collect unneeded internal state",
        after_help = "Example: db gc --dry-run"
    )]
    Gc,
    #[command(
        about = "Upgrade the on-disk format",
        after_help = "Example: db upgrade-format"
    )]
    UpgradeFormat,
    #[command(
        about = "Run an interactive SQL shell",
        after_help = "Example: db shell"
    )]
    Shell,
    #[command(
        about = "Generate shell completions",
        after_help = "Example: db completions zsh"
    )]
    Completions { shell: String },
    #[command(subcommand, about = "Apply schema migrations")]
    Migrate(MigrateCommand),
}
#[derive(Subcommand, Clone)]
enum SchemaCommand {
    #[command(about = "Show a schema")]
    Show { table: String },
    #[command(about = "Create a minimal schema")]
    New { table: String },
    #[command(about = "Accept an inferred schema")]
    Accept { table: String },
    #[command(about = "Validate all schemas")]
    Validate,
}
#[derive(Subcommand, Clone)]
enum SnapshotCommand {
    Create { name: String },
    List,
    Restore { name: String },
    Delete { name: String },
}
#[derive(Subcommand, Clone)]
enum MigrateCommand {
    AddTable {
        table: String,
        #[arg(long)]
        from: PathBuf,
    },
    DropTable {
        table: String,
    },
    RenameTable {
        table: String,
        new: String,
    },
    AddColumn {
        table: String,
        column: String,
        #[arg(long, name = "type")]
        kind: String,
        #[arg(long)]
        nullable: bool,
        #[arg(long)]
        default: Option<String>,
    },
    DropColumn {
        table: String,
        column: String,
    },
    RenameColumn {
        table: String,
        column: String,
        new: String,
    },
    ChangeType {
        table: String,
        column: String,
        kind: String,
        #[arg(long)]
        using: Option<String>,
    },
    AddConstraint {
        table: String,
        definition: String,
    },
    DropConstraint {
        table: String,
        name: String,
    },
    AddIndex {
        table: String,
        #[arg(value_delimiter = ',')]
        columns: Vec<String>,
    },
    DropIndex {
        table: String,
        #[arg(value_delimiter = ',')]
        columns: Vec<String>,
    },
    Apply {
        file: PathBuf,
    },
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ConstraintDefinition {
    Unique {
        columns: Vec<String>,
    },
    ForeignKey {
        #[serde(flatten)]
        foreign_key: ForeignKey,
    },
    Check {
        #[serde(flatten)]
        check: Check,
    },
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationDocument {
    operations: Vec<MigrationOperation>,
}
#[derive(serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum MigrationOperation {
    AddTable {
        table: String,
        schema: Schema,
    },
    DropTable {
        table: String,
    },
    RenameTable {
        table: String,
        new: String,
    },
    AddColumn {
        table: String,
        column: String,
        #[serde(rename = "type")]
        kind: ColumnType,
        #[serde(default)]
        nullable: bool,
        #[serde(default)]
        default: Option<Value>,
    },
    DropColumn {
        table: String,
        column: String,
    },
    RenameColumn {
        table: String,
        column: String,
        new: String,
    },
    ChangeType {
        table: String,
        column: String,
        #[serde(rename = "type")]
        kind: ColumnType,
        #[serde(default)]
        using: Option<String>,
    },
    AddConstraint {
        table: String,
        definition: ConstraintDefinition,
    },
    DropConstraint {
        table: String,
        name: String,
    },
    AddIndex {
        table: String,
        columns: Vec<String>,
    },
    DropIndex {
        table: String,
        columns: Vec<String>,
    },
}

pub fn run(cli: Cli) -> Result<i32> {
    let mut settings = cli.clone();
    let requested_root = cli
        .db
        .clone()
        .unwrap_or(std::env::current_dir().map_err(|e| DbError::io(Path::new("."), e))?);
    let format = if cli.json {
        Format::Json
    } else if let Some(f) = &cli.format {
        Format::parse(f)?
    } else if io::stdout().is_terminal() {
        Format::Table
    } else {
        Format::Jsonl
    };
    match cli.command {
        Command::Init {
            path,
            adopt,
            track_provenance,
        } => cmd_init(
            path.or(cli.db),
            adopt,
            track_provenance,
            cli.dry_run,
            format,
        ),
        Command::Inspect { path } => cmd_inspect(path.or(cli.db), format),
        Command::Completions { shell } => completions(&shell),
        Command::Infer {
            table,
            write,
            all,
            strictness,
            pk,
        } if !requested_root.join(".db").exists() => infer_standalone(
            &requested_root,
            table.as_deref(),
            write,
            all,
            &strictness,
            &pk,
            format,
        ),
        command => {
            let root = Database::discover(cli.db.as_deref())?;
            if !metadata_writable(&root) {
                settings.readonly = true;
            }
            let no_write = settings.readonly
                || matches!(
                    command,
                    Command::Check { no_write: true, .. }
                        | Command::Lint { .. }
                        | Command::Doctor { fix: false, .. }
                        | Command::Diff { .. }
                );
            let mode = if settings.readonly {
                ObserveMode::ReadOnly
            } else if no_write {
                ObserveMode::NoWrite
            } else {
                ObserveMode::Record
            };
            let mut db = Database::open(root, mode)?;
            if settings.readonly {
                for warning in db.catalog.warnings.iter().filter(|warning| {
                    matches!(
                        warning.code.as_str(),
                        "METADATA_STALE_READONLY" | "INDEX_STALE"
                    )
                }) {
                    output::diagnostic_notice(warning, format);
                }
            }
            dispatch(command, &mut db, format, &settings)
        }
    }
}

fn dispatch(command: Command, db: &mut Database, format: Format, cli: &Cli) -> Result<i32> {
    match command {
        Command::Status => status(db, format),
        Command::Check { strict, .. } => check(db, format, strict),
        Command::Lint {
            table,
            strict,
            descriptions,
        } => lint(db, format, table.as_deref(), strict, descriptions),
        Command::Doctor {
            fix,
            allow_data,
            only,
            explain,
            no_snapshot,
        } => doctor(
            db,
            format,
            fix,
            allow_data,
            only.as_deref(),
            explain.as_deref(),
            no_snapshot,
            cli,
        ),
        Command::Infer {
            table,
            write,
            all,
            strictness,
            pk,
        } => infer_cmd(
            db,
            table.as_deref(),
            write,
            all,
            &strictness,
            &pk,
            format,
            cli,
        ),
        Command::Tables => {
            db.require_valid()?;
            let rows = db
                .catalog
                .schemas
                .iter()
                .map(|(t, _)| {
                    obj([
                        ("kind", Value::String("table".into())),
                        ("table", Value::String(t.clone())),
                        ("rows", Value::from(db.catalog.rows[t].len())),
                    ])
                })
                .collect::<Vec<_>>();
            output::records(&rows, format)?;
            Ok(0)
        }
        Command::Describe { table } => {
            db.require_valid()?;
            let s = db.catalog.schemas.get(&table).ok_or_else(|| {
                DbError::new("UNKNOWN_TABLE", format!("unknown table {table:?}"), 4)
            })?;
            println!("{}", serde_json::to_string_pretty(s).unwrap());
            Ok(0)
        }
        Command::Get { table, key } => get(db, &table, &key, format),
        Command::List {
            table,
            where_expr,
            order,
            limit,
        } => list(
            db,
            &table,
            where_expr.as_deref(),
            order.as_deref(),
            limit,
            format,
            cli.timeout.or(db.config.timeout_seconds),
        ),
        Command::Insert {
            table,
            json_value,
            from,
        } => insert(db, &table, json_value, from, format, cli),
        Command::Update { table, key, patch } => update(db, &table, &key, &patch, format, cli),
        Command::Delete { table, key } => delete(db, &table, &key, format, cli),
        Command::Sql {
            sql: query,
            params,
            explain,
            explain_analyze,
        } => {
            if explain || explain_analyze {
                explain_sql(db, &query, &params, explain_analyze, format, cli)
            } else {
                sql(db, &query, &params, format, cli)
            }
        }
        Command::Explain { sql: s } => explain_sql(db, &s, &[], false, format, cli),
        Command::Schema(s) => schema_cmd(db, s, format, cli),
        Command::Export { table, out } => export(db, &table, out.as_deref(), format),
        Command::Import { table, from } => import(db, &table, &from, format, cli),
        Command::Diff { args, schema } => diff(db, &args, schema, format),
        Command::Log => log(db, format),
        Command::Show { revision } => show(db, revision, format),
        Command::Snapshot(s) => snapshot(db, s, format, cli),
        Command::Recover => {
            require_writable(cli)?;
            let changed = crate::db::recover(&db.root)?;
            println!(
                "{}",
                if changed {
                    "recovery complete"
                } else {
                    "no pending transactions"
                }
            );
            Ok(0)
        }
        Command::Reindex => {
            require_writable(cli)?;
            reindex(db, format)
        }
        Command::Analyze => {
            require_writable(cli)?;
            analyze(db, format)
        }
        Command::Gc => {
            require_writable(cli)?;
            gc(db, cli.dry_run, format)
        }
        Command::UpgradeFormat => {
            println!("format {} is current", crate::FORMAT_VERSION);
            Ok(0)
        }
        Command::Shell => shell(db, format, cli),
        Command::Migrate(m) => migrate(db, m, format, cli),
        Command::Init { .. } | Command::Inspect { .. } | Command::Completions { .. } => {
            unreachable!()
        }
    }
}

fn cmd_init(
    path: Option<PathBuf>,
    adopt: bool,
    track: bool,
    dry: bool,
    format: Format,
) -> Result<i32> {
    let root = path.unwrap_or(std::env::current_dir().map_err(|e| DbError::io(Path::new("."), e))?);
    if root.join(".db").exists() {
        return Err(DbError::new(
            "ALREADY_INITIALIZED",
            format!("{} is already initialized", root.display()),
            1,
        ));
    }
    if !adopt {
        if dry {
            println!("would initialize {}", root.display());
            return Ok(0);
        }
        crate::db::init_empty(&root, track)?;
        println!("initialized {} (revision 1)", root.display());
        return Ok(0);
    }
    let tables = infer::discover_tables(&root)?;
    let skipped = skipped_adoption_directories(&root)?;
    let existing = existing_schema_names(&root)?;
    let missing: Vec<_> = tables
        .iter()
        .filter(|t| !existing.contains(*t))
        .cloned()
        .collect();
    let schemas = infer::infer_all(
        &root,
        &missing,
        Strictness::Balanced,
        &crate::config::Config::default(),
        None,
    )?;
    let preflight = adoption_preflight(&root, &schemas)?;
    let errors = crate::integrity::validate(&preflight);
    if !errors.is_empty() {
        output::diagnostics(&errors, format);
        return Ok(2);
    }
    if dry {
        println!(
            "would adopt {} tables and infer {} schemas",
            tables.len(),
            schemas.len()
        );
        for (name, reason) in &skipped {
            println!("skipped {name}: {reason}");
        }
        return Ok(0);
    }
    crate::db::init_layout(&root, track)?;
    for s in schemas.values() {
        crate::db::write_schema(&root, s)?
    }
    let c = crate::catalog::Catalog::observe(&root, &crate::config::Config::default())?;
    let (hash, entries) = metadata::state(&c)?;
    metadata::record(&c, None, hash.clone(), entries, "import", None)?;
    println!(
        "Scanned {} directories, {} JSON files.\nVALID   revision 1   root {}",
        tables.len(),
        c.row_count(),
        &hash[..8]
    );
    for (name, reason) in &skipped {
        println!("Skipped {name}: {reason}");
    }
    Ok(0)
}
fn skipped_adoption_directories(root: &Path) -> Result<Vec<(String, String)>> {
    let mut skipped = vec![];
    for entry in fs::read_dir(root).map_err(|error| DbError::io(root, error))? {
        let path = entry.map_err(|error| DbError::io(root, error))?.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if matches!(name, "schema" | ".db" | ".git") || name.starts_with('.') || !path.is_dir() {
            continue;
        }
        let mut contains_json = false;
        for entry in fs::read_dir(&path).map_err(|error| DbError::io(&path, error))? {
            let entry = entry.map_err(|error| DbError::io(&path, error))?;
            contains_json |=
                entry.path().extension().and_then(|value| value.to_str()) == Some("json");
        }
        if !contains_json {
            skipped.push((name.into(), "contains no top-level .json files".into()));
        }
    }
    skipped.sort();
    Ok(skipped)
}
fn cmd_inspect(path: Option<PathBuf>, format: Format) -> Result<i32> {
    let root = path.unwrap_or(std::env::current_dir().map_err(|e| DbError::io(Path::new("."), e))?);
    let tables = infer::discover_tables(&root)?;
    let rows = tables
        .into_iter()
        .map(|t| {
            obj([
                ("kind", Value::String("directory".into())),
                ("table", Value::String(t)),
            ])
        })
        .collect::<Vec<_>>();
    output::records(&rows, format)?;
    Ok(0)
}
fn existing_schema_names(root: &Path) -> Result<std::collections::BTreeSet<String>> {
    let mut out = std::collections::BTreeSet::new();
    let dir = root.join("schema");
    if !dir.exists() {
        return Ok(out);
    }
    for e in fs::read_dir(&dir).map_err(|e| DbError::io(&dir, e))? {
        let p = e.map_err(|e| DbError::io(&dir, e))?.path();
        if p.extension().and_then(|x| x.to_str()) == Some("json") {
            if let Some(s) = p.file_stem().and_then(|x| x.to_str()) {
                out.insert(s.into());
            }
        }
    }
    Ok(out)
}
fn adoption_preflight(
    root: &Path,
    inferred: &std::collections::BTreeMap<String, Schema>,
) -> Result<crate::catalog::Catalog> {
    let temp = tempfile::tempdir().map_err(|e| DbError::io(Path::new("/tmp"), e))?;
    let shadow = temp.path();
    fs::create_dir(shadow.join("schema")).map_err(|e| DbError::io(shadow, e))?;
    let schema_dir = root.join("schema");
    if schema_dir.exists() {
        let metadata =
            fs::symlink_metadata(&schema_dir).map_err(|error| DbError::io(&schema_dir, error))?;
        if !metadata.file_type().is_dir() {
            return Err(DbError::from_diag(
                crate::diagnostic::Diagnostic::error(
                    "NON_REGULAR_FILE",
                    "schema/ must be a real directory",
                )
                .at("schema"),
                2,
            ));
        }
        for e in fs::read_dir(&schema_dir).map_err(|e| DbError::io(&schema_dir, e))? {
            let p = e.map_err(|e| DbError::io(&schema_dir, e))?.path();
            let metadata = fs::symlink_metadata(&p).map_err(|error| DbError::io(&p, error))?;
            if metadata.file_type().is_file() && !has_multiple_links(&metadata) {
                fs::copy(
                    &p,
                    shadow.join("schema").join(p.file_name().ok_or_else(|| {
                        DbError::new("PATH_VIOLATION", "schema entry has no filename", 2)
                    })?),
                )
                .map_err(|e| DbError::io(&p, e))?;
            } else {
                fs::create_dir(shadow.join("schema").join(p.file_name().ok_or_else(|| {
                    DbError::new("PATH_VIOLATION", "schema entry has no filename", 2)
                })?))
                .map_err(|error| DbError::io(&p, error))?;
            }
        }
    }
    for (t, s) in inferred {
        metadata::write_json_atomic(&shadow.join(format!("schema/{t}.json")), s)?
    }
    let mut table_names = infer::discover_tables(root)?;
    table_names.extend(existing_schema_names(root)?);
    table_names.sort();
    table_names.dedup();
    for t in table_names {
        let src = root.join(&t);
        if !src.exists() {
            continue;
        }
        let table_metadata =
            fs::symlink_metadata(&src).map_err(|error| DbError::io(&src, error))?;
        if !table_metadata.file_type().is_dir() {
            fs::write(shadow.join(&t), b"unsupported table path\n")
                .map_err(|error| DbError::io(&shadow.join(&t), error))?;
            continue;
        }
        fs::create_dir(shadow.join(&t)).map_err(|e| DbError::io(&shadow.join(&t), e))?;
        for e in fs::read_dir(&src).map_err(|e| DbError::io(&src, e))? {
            let p = e.map_err(|e| DbError::io(&src, e))?.path();
            let metadata = fs::symlink_metadata(&p).map_err(|e| DbError::io(&p, e))?;
            if metadata.file_type().is_file() && !has_multiple_links(&metadata) {
                fs::copy(
                    &p,
                    shadow.join(&t).join(p.file_name().ok_or_else(|| {
                        DbError::new("PATH_VIOLATION", "table entry has no filename", 2)
                    })?),
                )
                .map_err(|e| DbError::io(&p, e))?;
            } else {
                fs::create_dir(shadow.join(&t).join(p.file_name().ok_or_else(|| {
                    DbError::new("PATH_VIOLATION", "table entry has no filename", 2)
                })?))
                .map_err(|e| DbError::io(&p, e))?;
            }
        }
    }
    crate::catalog::Catalog::observe(shadow, &crate::config::Config::default())
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
fn infer_standalone(
    root: &Path,
    table: Option<&str>,
    write: bool,
    _all: bool,
    strictness: &str,
    pk: &[String],
    _format: Format,
) -> Result<i32> {
    let strict = match strictness {
        "strict" => Strictness::Strict,
        "balanced" => Strictness::Balanced,
        "loose" => Strictness::Loose,
        _ => {
            return Err(DbError::usage(
                "strictness must be strict, balanced, or loose",
            ));
        }
    };
    let tables = if let Some(t) = table {
        vec![t.into()]
    } else {
        infer::discover_tables(root)?
    };
    let schemas = infer::infer_all(
        root,
        &tables,
        strict,
        &crate::config::Config::default(),
        if pk.is_empty() { None } else { Some(pk) },
    )?;
    if write {
        fs::create_dir_all(root.join("schema"))
            .map_err(|e| DbError::io(&root.join("schema"), e))?;
        for s in schemas.values() {
            let p = root.join(format!("schema/{}.json", s.table));
            if p.exists() {
                return Err(DbError::usage(format!(
                    "refusing to overwrite {}",
                    p.display()
                )));
            }
            metadata::write_json_atomic(&p, s)?
        }
    } else {
        for s in schemas.values() {
            println!("{}", serde_json::to_string_pretty(s).unwrap())
        }
    }
    Ok(0)
}
fn status(db: &Database, format: Format) -> Result<i32> {
    if !db.diagnostics.is_empty() {
        output::diagnostics(&db.diagnostics, format);
        eprintln!(
            "INVALID   revision {}   ({} external changes, {} violations)\nrun `db doctor` for fix options",
            db.manifest.as_ref().map_or(0, |m| m.revision),
            db.external_changes.len(),
            db.diagnostics.len()
        );
        return Ok(
            if db
                .diagnostics
                .iter()
                .any(|d| d.code == "TRANSACTION_INCOMPLETE")
            {
                5
            } else {
                2
            },
        );
    }
    if !db.catalog.warnings.is_empty() {
        output::diagnostics(&db.catalog.warnings, format);
    }
    let m = db.manifest.as_ref();
    if format == Format::Table {
        println!(
            "VALID   revision {}   root {}   external changes: {}",
            m.map_or(0, |m| m.revision),
            m.map_or("unknown", |m| &m.root_hash[..8]),
            if db.external_changes.is_empty() {
                "none"
            } else {
                "accepted"
            }
        );
        if !db.external_changes.is_empty() {
            println!("changed:");
            for p in &db.external_changes {
                println!("  {p}");
            }
        }
        let findings = crate::lint::lint(&db.catalog, &db.config, false);
        if !findings.is_empty() {
            println!("lint: {} findings (run `db lint`)", findings.len());
        }
    } else {
        output::records(
            &[obj([
                ("kind", Value::String("status".into())),
                ("valid", Value::Bool(true)),
                ("revision", Value::from(m.map_or(0, |m| m.revision))),
                (
                    "root",
                    Value::String(m.map_or("", |m| m.root_hash.as_str()).into()),
                ),
            ])],
            format,
        )?
    }
    Ok(0)
}
fn check(db: &Database, format: Format, strict: bool) -> Result<i32> {
    let lint = crate::lint::lint(&db.catalog, &db.config, false);
    let mut violations = std::collections::BTreeMap::<String, usize>::new();
    for diagnostic in &db.diagnostics {
        *violations.entry(diagnostic.code.clone()).or_default() += 1;
    }
    let mut lint_counts = std::collections::BTreeMap::<String, usize>::new();
    for diagnostic in &lint {
        *lint_counts
            .entry(format!("{:?}", diagnostic.severity).to_ascii_lowercase())
            .or_default() += 1;
    }
    let summary = obj([
        ("kind", Value::String("check_summary".into())),
        ("valid", Value::Bool(db.diagnostics.is_empty())),
        ("tables", Value::from(db.catalog.schemas.len())),
        ("rows", Value::from(db.catalog.row_count())),
        (
            "violations",
            serde_json::to_value(&violations)
                .map_err(|error| DbError::new("INTERNAL_METADATA_CORRUPT", error.to_string(), 6))?,
        ),
        (
            "lint",
            serde_json::to_value(&lint_counts)
                .map_err(|error| DbError::new("INTERNAL_METADATA_CORRUPT", error.to_string(), 6))?,
        ),
        (
            "elapsed_ms",
            Value::from(db.validation_elapsed.as_millis() as u64),
        ),
    ]);
    if matches!(format, Format::Json | Format::Jsonl) {
        let mut reported = db.diagnostics.clone();
        if strict {
            reported.extend(db.catalog.warnings.clone());
            reported.extend(lint.clone());
        }
        output::check_result(&reported, summary, format)?;
        if !db.diagnostics.is_empty() {
            return Ok(
                if db
                    .diagnostics
                    .iter()
                    .any(|d| d.code == "TRANSACTION_INCOMPLETE")
                {
                    5
                } else {
                    2
                },
            );
        }
        if strict && (!lint.is_empty() || !db.catalog.warnings.is_empty()) {
            return Ok(7);
        }
        return Ok(0);
    }
    if !db.diagnostics.is_empty() {
        output::diagnostics(&db.diagnostics, format);
        eprintln!(
            "{} tables, {} rows, {} violations ({:?}), {} lint findings ({:?}), {} ms",
            db.catalog.schemas.len(),
            db.catalog.row_count(),
            db.diagnostics.len(),
            violations,
            lint.len(),
            lint_counts,
            db.validation_elapsed.as_millis(),
        );
        return Ok(
            if db
                .diagnostics
                .iter()
                .any(|d| d.code == "TRANSACTION_INCOMPLETE")
            {
                5
            } else {
                2
            },
        );
    }
    if strict && (!lint.is_empty() || !db.catalog.warnings.is_empty()) {
        let mut findings = db.catalog.warnings.clone();
        findings.extend(lint);
        output::diagnostics(&findings, format);
        return Ok(7);
    }
    println!(
        "VALID: {} tables, {} rows, 0 violations, {} lint findings ({:?}), {} ms",
        db.catalog.schemas.len(),
        db.catalog.row_count(),
        lint.len(),
        lint_counts,
        db.validation_elapsed.as_millis(),
    );
    Ok(0)
}
fn lint(
    db: &Database,
    format: Format,
    table: Option<&str>,
    strict: bool,
    descriptions: bool,
) -> Result<i32> {
    let mut x = crate::lint::lint(&db.catalog, &db.config, descriptions);
    if let Some(t) = table {
        x.retain(|d| d.table.as_deref() == Some(t))
    }
    output::diagnostics(&x, format);
    Ok(if strict && !x.is_empty() { 7 } else { 0 })
}

fn doctor(
    db: &mut Database,
    format: Format,
    fix: bool,
    allow_data: bool,
    only: Option<&str>,
    explain: Option<&str>,
    no_snapshot: bool,
    cli: &Cli,
) -> Result<i32> {
    let plan = crate::doctor::plan(db);
    if let Some(id) = explain {
        if let Some(f) = plan.iter().find(|f| f.id == id) {
            println!(
                "{} [{}]: {}\npaths: {}",
                f.id,
                f.class,
                f.description,
                f.paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Ok(0);
        }
        return Err(DbError::usage(format!(
            "unknown or inapplicable fix {id:?}"
        )));
    }
    if format == Format::Table {
        println!("Doctor plan:");
        for class in ["derived", "schema", "layout", "data", "manual"] {
            let items: Vec<_> = plan
                .iter()
                .filter(|f| f.class == class && only.is_none_or(|o| o == f.id))
                .collect();
            if !items.is_empty() {
                println!("  {class} ({}):", items.len());
                for f in items {
                    println!("    {}  {}", f.id, f.description)
                }
            }
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&plan).unwrap())
    }
    if !fix {
        return Ok(if db.diagnostics.is_empty() { 0 } else { 2 });
    }
    require_writable(cli)?;
    if format != Format::Table && !cli.yes {
        return Err(DbError::new(
            "CONFIRMATION_REQUIRED",
            "--yes is required to apply doctor fixes with machine-readable output",
            9,
        ));
    }
    if !cli.yes {
        return Err(DbError::new(
            "CONFIRMATION_REQUIRED",
            "doctor fixes require --yes",
            9,
        ));
    }
    let changes = crate::doctor::repair_changes(db, only, allow_data)?;
    if changes.is_empty() {
        println!("no applicable automatic fixes");
        return Ok(if db.diagnostics.is_empty() { 0 } else { 2 });
    }
    let touches_rows = changes.iter().any(|c| match c {
        Change::Write { path, .. } | Change::Delete { path } => !path.starts_with("schema"),
    });
    if touches_rows && !no_snapshot && !cli.dry_run {
        let name = format!(
            "pre-doctor-{}",
            db.manifest.as_ref().map_or(0, |m| m.revision)
        );
        let dest = db.root.join(".db/snapshots").join(&name);
        if dest.exists() {
            return Err(DbError::new(
                "SNAPSHOT_EXISTS",
                format!(
                    "required safety snapshot {name:?} already exists; delete it or use --no-snapshot"
                ),
                1,
            ));
        }
        create_snapshot(db, &name)?;
        println!("created snapshot {name}; restore with `db snapshot restore {name} --yes`");
    }
    let start = current_root(db)?;
    let paths = transaction::commit(
        &db.root,
        &db.config,
        &start,
        &changes,
        "repair",
        cli.dry_run,
    )?;
    if cli.dry_run && origin == "migration" && format == Format::Table {
        let row_files = paths
            .iter()
            .filter(|path| {
                path.extension().and_then(|extension| extension.to_str()) == Some("json")
                    && !path.starts_with("schema")
                    && !path.starts_with(".db")
            })
            .count();
        let schema_files = paths.iter().filter(|path| path.starts_with("schema")).count();
        println!(
            "migration plan: {row_files} row file(s), {schema_files} schema file(s)"
        );
    }
    print_mutation(
        &paths,
        db.manifest.as_ref().map_or(0, |m| m.revision + 1),
        cli.dry_run,
        format,
    )?;
    Ok(0)
}

fn infer_cmd(
    db: &mut Database,
    table: Option<&str>,
    write: bool,
    all: bool,
    strictness: &str,
    pk: &[String],
    format: Format,
    cli: &Cli,
) -> Result<i32> {
    let strict = match strictness {
        "strict" => Strictness::Strict,
        "balanced" => Strictness::Balanced,
        "loose" => Strictness::Loose,
        _ => {
            return Err(DbError::usage(
                "strictness must be strict, balanced, or loose",
            ));
        }
    };
    let tables = if let Some(t) = table {
        vec![t.into()]
    } else {
        infer::discover_tables(&db.root)?
    };
    let wanted: Vec<_> = tables
        .into_iter()
        .filter(|t| all || !db.catalog.schemas.contains_key(t))
        .collect();
    let schemas = infer::infer_all(
        &db.root,
        &wanted,
        strict,
        &db.config,
        if pk.is_empty() { None } else { Some(pk) },
    )?;
    if !write {
        for s in schemas.values() {
            println!("{}", serde_json::to_string_pretty(s).unwrap())
        }
        return Ok(0);
    }
    let mut changes = vec![];
    for (t, s) in schemas {
        let name = if all && db.catalog.schemas.contains_key(&t) {
            format!("schema/{t}.inferred.json")
        } else {
            format!("schema/{t}.json")
        };
        if db.root.join(&name).exists() {
            return Err(DbError::new(
                "SCHEMA_MISSING_REQUIRED",
                format!("refusing to overwrite {name}"),
                1,
            ));
        }
        changes.push(Change::Write {
            path: name.into(),
            bytes: canonical::pretty(&serde_json::to_value(s).unwrap()),
        })
    }
    let paths = transaction::commit(
        &db.root,
        &db.config,
        &current_root(db)?,
        &changes,
        "internal",
        cli.dry_run,
    )?;
    print_mutation(
        &paths,
        db.manifest.as_ref().map_or(1, |m| m.revision + 1),
        cli.dry_run,
        format,
    )?;
    Ok(0)
}

fn get(db: &Database, table: &str, key: &str, format: Format) -> Result<i32> {
    db.require_valid()?;
    let s = schema_for(db, table)?;
    let values = key_values(key, s.primary_key.len())?;
    let k = canonical::compact(&Value::Array(values));
    let row = crate::integrity::rows_by_key(&db.catalog, table)
        .get(&k)
        .copied()
        .ok_or_else(|| DbError::new("UNKNOWN_ROW", format!("no {table} row with key {key}"), 4))?;
    output::records(&[row.value.clone()], format)?;
    Ok(0)
}
fn list(
    db: &Database,
    table: &str,
    where_expr: Option<&str>,
    order: Option<&str>,
    limit: Option<usize>,
    format: Format,
    timeout_seconds: Option<u64>,
) -> Result<i32> {
    db.require_valid()?;
    schema_for(db, table)?;
    let mut sql = format!("SELECT * FROM {}", quote(table));
    if let Some(w) = where_expr {
        sql.push_str(" WHERE ");
        sql.push_str(w)
    }
    if let Some(o) = order {
        sql.push_str(" ORDER BY ");
        sql.push_str(&quote(o))
    }
    if let Some(n) = limit {
        sql.push_str(&format!(" LIMIT {n}"))
    }
    let timeout = timeout_seconds.map(std::time::Duration::from_secs);
    if stream_query_if_supported(db, &sql, &[], timeout, format)? {
        return Ok(0);
    }
    let r = crate::sql::execute_params_with_limits(
        &db.catalog,
        &sql,
        &[],
        timeout,
        db.config.max_result_rows,
        db.config.max_query_memory,
    )?;
    if r.rows.len() > db.config.max_result_rows {
        return Err(DbError::new(
            "RESOURCE_LIMIT",
            format!(
                "query returned more than {} rows",
                db.config.max_result_rows
            ),
            4,
        ));
    }
    output::records(&r.rows, format)?;
    Ok(0)
}
fn insert(
    db: &Database,
    table: &str,
    json_value: Option<String>,
    from: Option<String>,
    format: Format,
    cli: &Cli,
) -> Result<i32> {
    db.require_valid()?;
    let text = if let Some(f) = from {
        if f == "-" {
            read_limited(
                io::stdin().lock(),
                db.config.max_transaction_size,
                Path::new("stdin"),
            )?
        } else {
            let path = Path::new(&f);
            let file = fs::File::open(path).map_err(|error| DbError::io(path, error))?;
            read_limited(file, db.config.max_transaction_size, path)?
        }
    } else {
        json_value.ok_or_else(|| DbError::usage("insert requires JSON or --from"))?
    };
    let v: Value =
        serde_json::from_str(&text).map_err(|e| DbError::usage(format!("invalid JSON: {e}")))?;
    let rows = match v {
        Value::Array(a) => a,
        x => vec![x],
    };
    let s = schema_for(db, table)?;
    let first_sequence = next_sequence(db, table, s)?;
    let mut changes = vec![];
    for (v_i, v) in rows.into_iter().enumerate() {
        let mut row = v
            .as_object()
            .cloned()
            .ok_or_else(|| DbError::usage("insert input must be an object or array of objects"))?;
        let sequence = first_sequence
            .checked_add(v_i as i64)
            .ok_or_else(|| DbError::new("RESOURCE_LIMIT", "generated sequence exhausted i64", 2))?;
        materialize(&mut row, s, sequence);
        let path = PathBuf::from(table).join(canonical::filename(s, &row).ok_or_else(|| {
            DbError::new("IDENTITY_MISMATCH", "cannot derive filename from row", 2)
        })?);
        if db.root.join(&path).exists() {
            return Err(DbError::new(
                "PRIMARY_KEY_VIOLATION",
                format!("row path {} already exists", path.display()),
                2,
            ));
        }
        changes.push(Change::Write {
            path,
            bytes: canonical::pretty(&canonical::canonical_row(&row, s)),
        })
    }
    commit_changes(db, changes, "internal", format, cli)
}
fn update(
    db: &Database,
    table: &str,
    key: &str,
    patch: &str,
    format: Format,
    cli: &Cli,
) -> Result<i32> {
    db.require_valid()?;
    let s = schema_for(db, table)?;
    // Resolve the row first so a missing key is reported distinctly instead of
    // turning into a successful zero-row SQL update.
    find_row(db, table, key)?;
    let p: Value = serde_json::from_str(patch)
        .map_err(|e| DbError::usage(format!("invalid patch JSON: {e}")))?;
    let p = p
        .as_object()
        .ok_or_else(|| DbError::usage("patch must be a JSON object"))?;
    if p.is_empty() {
        println!("no change: patch is empty");
        return Ok(0);
    }
    for name in p.keys() {
        if !s.columns.contains_key(name) {
            return Err(DbError::new(
                "UNKNOWN_COLUMN",
                format!("unknown column {table}.{name}"),
                4,
            ));
        }
    }
    let key_values = key_values(key, s.primary_key.len())?;
    let mut params = p.values().cloned().collect::<Vec<_>>();
    params.extend(key_values);
    let assignments = p
        .keys()
        .map(|name| format!("{} = ?", quote(name)))
        .collect::<Vec<_>>()
        .join(", ");
    let predicate = s
        .primary_key
        .iter()
        .map(|name| format!("{} = ?", quote(name)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let statement = format!(
        "UPDATE {} SET {assignments} WHERE {predicate}",
        quote(table)
    );
    let result = crate::sql::execute(&db.catalog, &statement, &params)?;
    commit_changes(db, result.changes, "internal", format, cli)
}
fn delete(db: &Database, table: &str, key: &str, format: Format, cli: &Cli) -> Result<i32> {
    db.require_valid()?;
    let s = schema_for(db, table)?;
    let values = key_values(key, s.primary_key.len())?;
    let where_sql = s
        .primary_key
        .iter()
        .map(|c| format!("{} = ?", quote(c)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!("DELETE FROM {} WHERE {where_sql}", quote(table));
    let r = crate::sql::execute(&db.catalog, &sql, &values)?;
    commit_changes(db, r.changes, "internal", format, cli)
}
fn sql(db: &Database, text: &str, param_text: &[String], format: Format, cli: &Cli) -> Result<i32> {
    db.require_valid()?;
    let params = param_text
        .iter()
        .map(|p| {
            let (name, text) = p
                .split_once('=')
                .map_or((None, p.as_str()), |(n, v)| (Some(n.to_string()), v));
            serde_json::from_str(text)
                .map(|value| crate::sql::SqlParam { name, value })
                .map_err(|e| DbError::usage(format!("invalid parameter {p:?}: {e}")))
        })
        .collect::<Result<Vec<_>>>()?;
    let actual = text.to_string();
    if stream_query_if_supported(
        db,
        &actual,
        &params,
        cli.timeout
            .or(db.config.timeout_seconds)
            .map(std::time::Duration::from_secs),
        format,
    )? {
        return Ok(0);
    }
    let r = crate::sql::execute_params_with_limits(
        &db.catalog,
        &actual,
        &params,
        cli.timeout
            .or(db.config.timeout_seconds)
            .map(std::time::Duration::from_secs),
        db.config.max_result_rows,
        db.config.max_query_memory,
    )?;
    if r.rows.len() > db.config.max_result_rows {
        return Err(DbError::new(
            "RESOURCE_LIMIT",
            format!(
                "query returned more than {} rows",
                db.config.max_result_rows
            ),
            4,
        ));
    }
    if r.mutation {
        commit_changes(db, r.changes, "internal", format, cli)
    } else {
        output::records(&r.rows, format)?;
        Ok(0)
    }
}

fn explain_sql(
    db: &Database,
    text: &str,
    param_text: &[String],
    analyze: bool,
    format: Format,
    cli: &Cli,
) -> Result<i32> {
    db.require_valid()?;
    let params = parse_sql_params(param_text)?;
    if analyze && !crate::sql::is_read_statement(text)? {
        return Err(DbError::new(
            "QUERY_UNSUPPORTED",
            "--explain-analyze is restricted to read-only statements",
            4,
        ));
    }
    let physical = crate::sql::execute_params_with_limits(
        &db.catalog,
        &format!("EXPLAIN QUERY PLAN {text}"),
        &params,
        cli.timeout
            .or(db.config.timeout_seconds)
            .map(std::time::Duration::from_secs),
        db.config.max_result_rows,
        db.config.max_query_memory,
    )?
    .rows;
    let details = physical
        .iter()
        .filter_map(|row| row.get("detail").and_then(Value::as_str))
        .map(String::from)
        .collect::<Vec<_>>();
    let declared_indexes = db
        .catalog
        .schemas
        .iter()
        .flat_map(|(table, schema)| {
            schema
                .indexes
                .iter()
                .map(move |columns| crate::sql::index_name(table, columns))
        })
        .collect::<Vec<_>>();
    let selected = declared_indexes
        .iter()
        .filter(|name| details.iter().any(|detail| detail.contains(*name)))
        .cloned()
        .collect::<Vec<_>>();
    let rejected = declared_indexes
        .iter()
        .filter(|name| !selected.contains(name))
        .map(|name| {
            obj([
                ("index", Value::String(name.clone())),
                (
                    "reason",
                    Value::String("SQLite costed another access path lower".into()),
                ),
            ])
        })
        .map(Value::Object)
        .collect::<Vec<_>>();
    let mut record = obj([
        ("kind", Value::String("query_plan".into())),
        (
            "logical_plan",
            Value::String(crate::sql::normalized_statement(text)?),
        ),
        (
            "physical_plan",
            Value::Array(physical.into_iter().map(Value::Object).collect()),
        ),
        (
            "selected_indexes",
            Value::Array(selected.into_iter().map(Value::String).collect()),
        ),
        ("rejected_indexes", Value::Array(rejected)),
        (
            "estimated_rows_upper_bound",
            Value::from(db.catalog.row_count()),
        ),
    ]);
    if analyze {
        let started = std::time::Instant::now();
        let result = crate::sql::execute_params_with_limits(
            &db.catalog,
            text,
            &params,
            cli.timeout
                .or(db.config.timeout_seconds)
                .map(std::time::Duration::from_secs),
            db.config.max_result_rows,
            db.config.max_query_memory,
        )?;
        record.insert("actual_rows".into(), Value::from(result.rows.len()));
        record.insert(
            "actual_elapsed_us".into(),
            Value::from(started.elapsed().as_micros() as u64),
        );
    }
    output::records(&[record], format)?;
    Ok(0)
}

fn parse_sql_params(param_text: &[String]) -> Result<Vec<crate::sql::SqlParam>> {
    param_text
        .iter()
        .map(|parameter| {
            let (name, text) = parameter
                .split_once('=')
                .map_or((None, parameter.as_str()), |(name, value)| {
                    (Some(name.to_string()), value)
                });
            serde_json::from_str(text)
                .map(|value| crate::sql::SqlParam { name, value })
                .map_err(|error| {
                    DbError::usage(format!("invalid parameter {parameter:?}: {error}"))
                })
        })
        .collect()
}

fn stream_query_if_supported(
    db: &Database,
    statement: &str,
    params: &[crate::sql::SqlParam],
    timeout: Option<std::time::Duration>,
    format: Format,
) -> Result<bool> {
    if !matches!(format, Format::Jsonl | Format::Csv) || !crate::sql::is_read_statement(statement)?
    {
        return Ok(false);
    }
    match format {
        Format::Jsonl => {
            crate::sql::query_each_timeout(
                &db.catalog,
                statement,
                params,
                timeout,
                db.config.max_result_rows,
                db.config.max_query_memory,
                output::jsonl_record,
            )?;
        }
        Format::Csv => {
            let mut writer = csv::Writer::from_writer(io::stdout().lock());
            let mut headers: Option<Vec<String>> = None;
            crate::sql::query_each_timeout(
                &db.catalog,
                statement,
                params,
                timeout,
                db.config.max_result_rows,
                db.config.max_query_memory,
                |row| {
                    if headers.is_none() {
                        let row_headers = row.keys().cloned().collect::<Vec<_>>();
                        writer
                            .write_record(&row_headers)
                            .map_err(|error| DbError::new("IO_ERROR", error.to_string(), 6))?;
                        headers = Some(row_headers);
                    }
                    let row_headers = headers.as_ref().ok_or_else(|| {
                        DbError::new("IO_ERROR", "CSV header state was not initialized", 6)
                    })?;
                    writer
                        .write_record(
                            row_headers
                                .iter()
                                .map(|name| output_cell(row.get(name).unwrap_or(&Value::Null))),
                        )
                        .map_err(|error| DbError::new("IO_ERROR", error.to_string(), 6))
                },
            )?;
            writer
                .flush()
                .map_err(|error| DbError::io(Path::new("stdout"), error))?;
        }
        _ => unreachable!(),
    }
    Ok(true)
}

fn output_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        _ => canonical::compact(value),
    }
}

fn schema_cmd(db: &Database, cmd: SchemaCommand, format: Format, cli: &Cli) -> Result<i32> {
    match cmd {
        SchemaCommand::Show { table } => {
            let s = schema_for(db, &table)?;
            println!("{}", serde_json::to_string_pretty(s).unwrap());
            Ok(0)
        }
        SchemaCommand::Validate => check(db, format, false),
        SchemaCommand::New { table } => {
            if !crate::schema::valid_name(&table) {
                return Err(DbError::new(
                    "SCHEMA_INVALID_TABLE_NAME",
                    format!("invalid table name {table:?}"),
                    2,
                ));
            }
            if db.catalog.schemas.contains_key(&table) {
                return Err(DbError::new(
                    "SCHEMA_MISSING_REQUIRED",
                    "schema already exists",
                    1,
                ));
            }
            let mut cols = indexmap::IndexMap::new();
            cols.insert(
                "id".into(),
                Column {
                    kind: ColumnType::Uuid,
                    nullable: false,
                    default: None,
                    generated: Some(Generated {
                        kind: GeneratedKind::Uuid,
                    }),
                    values: None,
                    items: None,
                    properties: None,
                    description: None,
                },
            );
            let s = Schema {
                table: table.clone(),
                schema_version: 1,
                schema_format: None,
                description: None,
                primary_key: vec!["id".into()],
                columns: cols,
                unique: vec![],
                foreign_keys: vec![],
                check: vec![],
                indexes: vec![],
                storage: None,
                additional_fields: crate::schema::AdditionalFields::Reject,
                inferred: None,
            };
            commit_changes(
                db,
                vec![Change::Write {
                    path: format!("schema/{table}.json").into(),
                    bytes: canonical::pretty(&serde_json::to_value(s).unwrap()),
                }],
                "internal",
                format,
                cli,
            )
        }
        SchemaCommand::Accept { table } => {
            let mut s = schema_for(db, &table)?.clone();
            if s.inferred.take().is_none() {
                println!("no change: schema {table} is already accepted");
                return Ok(0);
            }
            commit_changes(
                db,
                vec![Change::Write {
                    path: format!("schema/{table}.json").into(),
                    bytes: canonical::pretty(&serde_json::to_value(s).unwrap()),
                }],
                "internal",
                format,
                cli,
            )
        }
    }
}

fn export(db: &Database, table: &str, out: Option<&Path>, format: Format) -> Result<i32> {
    db.require_valid()?;
    schema_for(db, table)?;
    if format == Format::Sqlite {
        let path = out.ok_or_else(|| DbError::usage("sqlite export requires --out"))?;
        crate::sql::export_sqlite(&db.catalog, table, path)?;
        println!("exported database to {}", path.display());
        return Ok(0);
    }
    let rows: Vec<_> = db.catalog.rows[table]
        .iter()
        .map(|r| r.value.clone())
        .collect();
    if let Some(path) = out {
        let bytes = match format {
            Format::Json => {
                let mut b = serde_json::to_vec_pretty(&rows).unwrap();
                b.push(b'\n');
                b
            }
            Format::Jsonl => rows
                .iter()
                .flat_map(|r| {
                    let mut b = serde_json::to_vec(r).unwrap();
                    b.push(b'\n');
                    b
                })
                .collect(),
            Format::Csv => {
                let mut w = csv::Writer::from_writer(vec![]);
                let heads: Vec<_> = db.catalog.schemas[table].columns.keys().cloned().collect();
                w.write_record(&heads)
                    .map_err(|e| DbError::new("IO_ERROR", e.to_string(), 6))?;
                for r in &rows {
                    w.write_record(heads.iter().map(|h| {
                        r.get(h)
                            .map(|v| match v {
                                Value::String(s) => s.clone(),
                                _ => canonical::compact(v),
                            })
                            .unwrap_or_default()
                    }))
                    .map_err(|e| DbError::new("IO_ERROR", e.to_string(), 6))?
                }
                w.into_inner()
                    .map_err(|e| DbError::new("IO_ERROR", e.to_string(), 6))?
            }
            Format::Table | Format::Sqlite => {
                return Err(DbError::usage(
                    "file export format must be json, jsonl, or csv",
                ));
            }
        };
        fs::write(path, bytes).map_err(|e| DbError::io(path, e))?;
        println!("exported {} rows to {}", rows.len(), path.display())
    } else {
        output::records(&rows, format)?
    }
    Ok(0)
}
fn import(db: &Database, table: &str, path: &Path, format: Format, cli: &Cli) -> Result<i32> {
    let input_size = fs::metadata(path)
        .map_err(|error| DbError::io(path, error))?
        .len();
    if input_size > db.config.max_transaction_size {
        return Err(DbError::new(
            "RESOURCE_LIMIT",
            format!(
                "import input is {input_size} bytes, exceeding the {} byte transaction limit",
                db.config.max_transaction_size
            ),
            2,
        ));
    }
    let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("");
    let values = if ext == "csv" {
        let mut rdr = csv::Reader::from_path(path)
            .map_err(|e| DbError::new("INVALID_JSON", e.to_string(), 2))?;
        let headers = rdr
            .headers()
            .map_err(|e| DbError::new("INVALID_JSON", e.to_string(), 2))?
            .clone();
        let s = schema_for(db, table)?;
        let mut out = vec![];
        for row in rdr.records() {
            let row = row.map_err(|e| DbError::new("INVALID_JSON", e.to_string(), 2))?;
            let mut m = Map::new();
            for (h, v) in headers.iter().zip(row.iter()) {
                let column = s.columns.get(h).ok_or_else(|| {
                    DbError::new(
                        "UNKNOWN_COLUMN",
                        format!("CSV header names unknown column {table}.{h}"),
                        4,
                    )
                })?;
                m.insert(h.into(), parse_csv(v, column)?);
            }
            out.push(Value::Object(m))
        }
        out
    } else {
        let text = fs::read_to_string(path).map_err(|e| DbError::io(path, e))?;
        if ext == "jsonl" {
            text.lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| {
                    serde_json::from_str(l)
                        .map_err(|e| DbError::new("INVALID_JSON", e.to_string(), 2))
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            match serde_json::from_str::<Value>(&text)
                .map_err(|e| DbError::new("INVALID_JSON", e.to_string(), 2))?
            {
                Value::Array(a) => a,
                v => vec![v],
            }
        }
    };
    let s = schema_for(db, table)?;
    let first_sequence = next_sequence(db, table, s)?;
    let mut changes = vec![];
    for (i, v) in values.into_iter().enumerate() {
        let mut r = v
            .as_object()
            .cloned()
            .ok_or_else(|| DbError::new("ROW_ROOT_NOT_OBJECT", "import rows must be objects", 2))?;
        let sequence = first_sequence
            .checked_add(i as i64)
            .ok_or_else(|| DbError::new("RESOURCE_LIMIT", "generated sequence exhausted i64", 2))?;
        materialize(&mut r, s, sequence);
        let path = PathBuf::from(table).join(canonical::filename(s, &r).ok_or_else(|| {
            DbError::new(
                "IDENTITY_MISMATCH",
                "cannot derive imported row filename",
                2,
            )
        })?);
        if db.root.join(&path).exists() {
            return Err(DbError::new(
                "PRIMARY_KEY_VIOLATION",
                format!("{} already exists", path.display()),
                2,
            ));
        }
        changes.push(Change::Write {
            path,
            bytes: canonical::pretty(&canonical::canonical_row(&r, s)),
        })
    }
    commit_changes(db, changes, "import", format, cli)
}
fn diff(db: &Database, args: &[String], schema_only: bool, format: Format) -> Result<i32> {
    let table_filter = if args.len() == 1 && args[0].parse::<u64>().is_err() {
        Some(args[0].as_str())
    } else {
        None
    };
    let working = args.len() <= 1;
    let (old, new) = if args.len() == 2 {
        let a = load_revision(
            db,
            args[0]
                .parse()
                .map_err(|_| DbError::usage("revision must be an integer"))?,
        )?;
        let b = load_revision(
            db,
            args[1]
                .parse()
                .map_err(|_| DbError::usage("revision must be an integer"))?,
        )?;
        (a.entries, b.entries)
    } else if working {
        let (_, current) = metadata::state(&db.catalog)?;
        (
            db.manifest
                .as_ref()
                .map(|m| m.entries.clone())
                .unwrap_or_default(),
            current,
        )
    } else {
        return Err(DbError::usage(
            "diff accepts a table or exactly two revisions",
        ));
    };
    let mut rows = vec![];
    let changes = metadata::diff_entries(Some(&old), &new);
    let mut removed = std::collections::BTreeMap::new();
    let mut added = std::collections::BTreeMap::new();
    for x in &changes {
        let path = x[2..].to_string();
        if x.starts_with("D ") {
            removed.insert(old[&path].hash.clone(), path);
        } else if x.starts_with("A ") {
            added.insert(new[&path].hash.clone(), path);
        }
    }
    for x in changes {
        let path = x[2..].to_string();
        if schema_only && !path.starts_with("schema/") {
            continue;
        }
        if let Some(t) = table_filter {
            if !path.starts_with(&format!("{t}/")) && !path.starts_with(&format!("schema/{t}.")) {
                continue;
            }
        }
        if x.starts_with("D ") {
            let hash = &old[&path].hash;
            if let Some(to) = added.get(hash) {
                rows.push(obj([
                    ("kind", Value::String("rename".into())),
                    ("from", Value::String(path)),
                    ("to", Value::String(to.clone())),
                ]));
                continue;
            }
            rows.push(obj([
                ("kind", Value::String("row_removed".into())),
                ("path", Value::String(path)),
            ]));
        } else if x.starts_with("A ") {
            if removed.contains_key(&new[&path].hash) {
                continue;
            }
            rows.push(obj([
                ("kind", Value::String("row_added".into())),
                ("path", Value::String(path)),
            ]));
        } else {
            let old_value = load_object(db, &old[&path].hash)?;
            let new_value = if working {
                current_object(db, &path)?
            } else {
                load_object(db, &new[&path].hash)?
            };
            rows.extend(field_diff(&path, &old_value, &new_value));
        }
    }
    output::records(&rows, format)?;
    Ok(0)
}
fn load_revision(db: &Database, revision: u64) -> Result<metadata::Provenance> {
    let p = db.root.join(format!(".db/provenance/{revision:020}.json"));
    if !p.exists() {
        return Err(DbError::usage(format!("unknown revision {revision}")));
    }
    serde_json::from_slice(&fs::read(&p).map_err(|e| DbError::io(&p, e))?)
        .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))
}
fn load_object(db: &Database, hash: &str) -> Result<Value> {
    let p = db.root.join(format!(".db/objects/{hash}.json"));
    serde_json::from_slice(&fs::read(&p).map_err(|e| DbError::io(&p, e))?)
        .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))
}
fn current_object(db: &Database, path: &str) -> Result<Value> {
    if path == ".db/config" {
        return serde_json::from_slice(
            &fs::read(db.root.join(path)).map_err(|e| DbError::io(&db.root.join(path), e))?,
        )
        .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6));
    }
    if path == ".db/format" {
        return Ok(Value::String(
            fs::read_to_string(db.root.join(path))
                .map_err(|e| DbError::io(&db.root.join(path), e))?
                .trim()
                .into(),
        ));
    }
    if let Some(table) = path
        .strip_prefix("schema/")
        .and_then(|x| x.strip_suffix(".json"))
    {
        return serde_json::to_value(&db.catalog.schemas[table])
            .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6));
    }
    let table = path
        .split('/')
        .next()
        .ok_or_else(|| DbError::new("PATH_VIOLATION", "invalid manifest path", 6))?;
    let row = db
        .catalog
        .rows
        .get(table)
        .into_iter()
        .flatten()
        .find(|r| r.relative.to_string_lossy() == path)
        .ok_or_else(|| {
            DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                format!("manifest row {path} is missing"),
                6,
            )
        })?;
    Ok(canonical::canonical_row(
        &row.value,
        &db.catalog.schemas[table],
    ))
}
fn field_diff(path: &str, old: &Value, new: &Value) -> Vec<Map<String, Value>> {
    let mut out = vec![];
    if let (Some(a), Some(b)) = (old.as_object(), new.as_object()) {
        let keys: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).collect();
        for key in keys {
            let before = a.get(key).cloned().unwrap_or(Value::Null);
            let after = b.get(key).cloned().unwrap_or(Value::Null);
            if before != after {
                out.push(obj([
                    ("kind", Value::String("field_change".into())),
                    ("path", Value::String(path.into())),
                    ("field", Value::String(key.clone())),
                    ("old", before),
                    ("new", after),
                ]));
            }
        }
    } else if old != new {
        out.push(obj([
            ("kind", Value::String("change".into())),
            ("path", Value::String(path.into())),
            ("old", old.clone()),
            ("new", new.clone()),
        ]));
    }
    out
}
fn log(db: &Database, format: Format) -> Result<i32> {
    let dir = db.root.join(".db/provenance");
    let mut paths = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| DbError::io(&dir, e))? {
        paths.push(entry.map_err(|e| DbError::io(&dir, e))?.path());
    }
    paths.sort_by(|a, b| b.cmp(a));
    let mut rows = vec![];
    for p in paths {
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let v: Value = serde_json::from_slice(&fs::read(&p).map_err(|e| DbError::io(&p, e))?)
            .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))?;
        rows.push(v.as_object().unwrap().clone())
    }
    output::records(&rows, format)?;
    Ok(0)
}
fn show(db: &Database, revision: u64, _format: Format) -> Result<i32> {
    let p = db.root.join(format!(".db/provenance/{revision:020}.json"));
    if !p.exists() {
        return Err(DbError::usage(format!("unknown revision {revision}")));
    }
    print!(
        "{}",
        fs::read_to_string(&p).map_err(|e| DbError::io(&p, e))?
    );
    Ok(0)
}

fn snapshot(db: &Database, cmd: SnapshotCommand, format: Format, cli: &Cli) -> Result<i32> {
    let base = db.root.join(".db/snapshots");
    match cmd {
        SnapshotCommand::Create { name } => {
            require_writable(cli)?;
            db.require_valid()?;
            valid_snapshot(&name)?;
            let dest = base.join(&name);
            if dest.exists() {
                return Err(DbError::usage("snapshot already exists"));
            }
            if cli.dry_run {
                println!("would create snapshot {name}");
                return Ok(0);
            }
            create_snapshot(db, &name)?;
            println!("created snapshot {name}");
            Ok(0)
        }
        SnapshotCommand::List => {
            let mut rows = vec![];
            for e in fs::read_dir(&base).map_err(|e| DbError::io(&base, e))? {
                let e = e.map_err(|e| DbError::io(&base, e))?;
                if e.path().is_dir() {
                    rows.push(obj([
                        ("kind", Value::String("snapshot".into())),
                        (
                            "name",
                            Value::String(e.file_name().to_string_lossy().into()),
                        ),
                    ]))
                }
            }
            output::records(&rows, format)?;
            Ok(0)
        }
        SnapshotCommand::Restore { name } => {
            require_writable(cli)?;
            valid_snapshot(&name)?;
            if !cli.yes {
                return Err(DbError::new(
                    "CONFIRMATION_REQUIRED",
                    "snapshot restore requires --yes",
                    9,
                ));
            }
            let src = base.join(&name);
            if !src.is_dir() {
                return Err(DbError::usage("snapshot does not exist"));
            }
            let changes = changes_from_snapshot(db, &src)?;
            commit_changes(db, changes, "snapshot_restore", format, cli)
        }
        SnapshotCommand::Delete { name } => {
            require_writable(cli)?;
            valid_snapshot(&name)?;
            if !cli.yes {
                return Err(DbError::new(
                    "CONFIRMATION_REQUIRED",
                    "snapshot delete requires --yes",
                    9,
                ));
            }
            let p = base.join(name);
            if !cli.dry_run && p.exists() {
                fs::remove_dir_all(&p).map_err(|e| DbError::io(&p, e))?
            }
            println!(
                "{} snapshot {}",
                if cli.dry_run {
                    "would delete"
                } else {
                    "deleted"
                },
                p.file_name().unwrap().to_string_lossy()
            );
            Ok(0)
        }
    }
}
fn reindex(db: &Database, _format: Format) -> Result<i32> {
    db.require_valid()?;
    crate::index::rebuild(&db.root, &db.catalog)?;
    println!("rebuilt indexes");
    Ok(0)
}
fn analyze(db: &Database, _format: Format) -> Result<i32> {
    db.require_valid()?;
    let stats: std::collections::BTreeMap<_, _> = db
        .catalog
        .rows
        .iter()
        .map(|(t, r)| (t.clone(), obj([("rows", Value::from(r.len()))])))
        .collect();
    metadata::write_json_atomic(&db.root.join(".db/statistics/catalog.json"), &stats)?;
    println!("rebuilt statistics");
    Ok(0)
}
fn gc(db: &Database, dry: bool, _format: Format) -> Result<i32> {
    let dir = db.root.join(".db/transactions");
    let mut targets = vec![];
    for e in fs::read_dir(&dir).map_err(|e| DbError::io(&dir, e))? {
        let p = e.map_err(|e| DbError::io(&dir, e))?.path();
        if p.is_dir() && !p.join("COMMITTING").exists() {
            targets.push(p)
        }
    }
    for p in &targets {
        println!(
            "{} {}",
            if dry { "would remove" } else { "remove" },
            p.display()
        );
        if !dry {
            fs::remove_dir_all(p).map_err(|e| DbError::io(p, e))?
        }
    }
    println!(
        "{} transaction staging directories reclaimable",
        targets.len()
    );
    Ok(0)
}

struct ShellHelper {
    candidates: Vec<String>,
}
impl Completer for ShellHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        position: usize,
        _context: &ReadlineContext<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let start = line[..position]
            .rfind(|character: char| character.is_whitespace() || matches!(character, ',' | '('))
            .map_or(0, |index| index + 1);
        let prefix = line[start..position].to_ascii_lowercase();
        let matches = self
            .candidates
            .iter()
            .filter(|candidate| candidate.to_ascii_lowercase().starts_with(&prefix))
            .map(|candidate| Pair {
                display: candidate.clone(),
                replacement: candidate.clone(),
            })
            .collect();
        Ok((start, matches))
    }
}
impl Hinter for ShellHelper {
    type Hint = String;
}
impl Highlighter for ShellHelper {}
impl Validator for ShellHelper {}
impl Helper for ShellHelper {}

fn shell(db: &mut Database, format: Format, cli: &Cli) -> Result<i32> {
    db.require_valid()?;
    if !io::stdin().is_terminal() {
        for line in io::stdin().lock().lines() {
            let line = line.map_err(|e| DbError::io(Path::new("stdin"), e))?;
            if !shell_line(db, line.trim(), format, cli)? {
                break;
            }
        }
        return Ok(0);
    }
    let mut candidates = vec![
        "SELECT",
        "INSERT",
        "UPDATE",
        "DELETE",
        "FROM",
        "WHERE",
        "INNER JOIN",
        "LEFT JOIN",
        "GROUP BY",
        "HAVING",
        "ORDER BY",
        "LIMIT",
        "OFFSET",
        "DISTINCT",
        "COUNT",
        "SUM",
        "AVG",
        "MIN",
        "MAX",
        ".tables",
        ".describe",
        ".status",
        ".quit",
    ]
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();
    for (table, schema) in &db.catalog.schemas {
        candidates.push(table.clone());
        candidates.extend(schema.columns.keys().cloned());
    }
    candidates.sort();
    candidates.dedup();
    let mut editor = Editor::<ShellHelper, DefaultHistory>::new()
        .map_err(|error| DbError::new("IO_ERROR", error.to_string(), 6))?;
    editor.set_helper(Some(ShellHelper { candidates }));
    let history = db.root.join(".db/shell-history");
    if history.exists() {
        editor
            .load_history(&history)
            .map_err(|error| DbError::new("IO_ERROR", error.to_string(), 6))?;
    }
    loop {
        match editor.readline("db> ") {
            Ok(line) => {
                let query = line.trim();
                if query.is_empty() {
                    continue;
                }
                editor
                    .add_history_entry(query)
                    .map_err(|error| DbError::new("IO_ERROR", error.to_string(), 6))?;
                if !shell_line(db, query, format, cli)? {
                    break;
                }
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            Err(error) => return Err(DbError::new("IO_ERROR", error.to_string(), 6)),
        }
    }
    if !cli.readonly {
        editor
            .save_history(&history)
            .map_err(|error| DbError::new("IO_ERROR", error.to_string(), 6))?;
    }
    Ok(0)
}

fn shell_line(db: &mut Database, query: &str, format: Format, cli: &Cli) -> Result<bool> {
    if query.is_empty() {
        return Ok(true);
    }
    if matches!(query, ".quit" | ".exit") {
        return Ok(false);
    }
    if query == ".tables" {
        println!(
            "{}",
            db.catalog
                .schemas
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        );
        return Ok(true);
    }
    if query == ".status" {
        status(db, format)?;
        return Ok(true);
    }
    if let Some(table) = query.strip_prefix(".describe ") {
        println!(
            "{}",
            serde_json::to_string_pretty(schema_for(db, table)?).map_err(|error| {
                DbError::new("INTERNAL_METADATA_CORRUPT", error.to_string(), 6)
            })?
        );
        return Ok(true);
    }
    let read = crate::sql::is_read_statement(query)?;
    match sql(db, query, &[], format, cli) {
        Ok(_) => {
            if !read {
                db.refresh(if cli.readonly {
                    ObserveMode::ReadOnly
                } else {
                    ObserveMode::Record
                })?;
            }
        }
        Err(error) => error.render_human(),
    }
    io::stdout()
        .flush()
        .map_err(|error| DbError::io(Path::new("stdout"), error))?;
    Ok(true)
}

fn migrate(db: &Database, cmd: MigrateCommand, format: Format, cli: &Cli) -> Result<i32> {
    db.require_valid()?;
    match cmd {
        MigrateCommand::AddTable { table, from } => {
            let mut s = crate::schema::load(&from)?;
            if s.table != table {
                return Err(DbError::new(
                    "SCHEMA_TABLE_NAME_MISMATCH",
                    "--from schema table does not match requested table",
                    2,
                ));
            }
            commit_changes(
                db,
                vec![Change::Write {
                    path: format!("schema/{table}.json").into(),
                    bytes: canonical::pretty(&serde_json::to_value(&mut s).unwrap()),
                }],
                "migration",
                format,
                cli,
            )
        }
        MigrateCommand::DropTable { table } => {
            let _ = schema_for(db, &table)?;
            let mut changes = vec![Change::Delete {
                path: format!("schema/{table}.json").into(),
            }];
            changes.extend(db.catalog.rows[&table].iter().map(|r| Change::Delete {
                path: r.relative.clone(),
            }));
            commit_changes(db, changes, "migration", format, cli)
        }
        MigrateCommand::RenameTable { table, new } => {
            if db.catalog.schemas.contains_key(&new) {
                return Err(DbError::new(
                    "SCHEMA_INVALID_TABLE_NAME",
                    "target table already exists",
                    2,
                ));
            }
            let mut schemas = db.catalog.schemas.clone();
            let mut renamed = schemas.remove(&table).ok_or_else(|| {
                DbError::new("UNKNOWN_TABLE", format!("unknown table {table}"), 4)
            })?;
            renamed.table = new.clone();
            schemas.insert(new.clone(), renamed);
            for schema in schemas.values_mut() {
                for fk in &mut schema.foreign_keys {
                    if fk.references.table == table {
                        fk.references.table = new.clone();
                    }
                }
            }
            let mut changes = vec![Change::Delete {
                path: format!("schema/{table}.json").into(),
            }];
            for (name, schema) in schemas {
                let old = db
                    .catalog
                    .schemas
                    .get(&name)
                    .and_then(|s| serde_json::to_value(s).ok());
                let value = serde_json::to_value(&schema)
                    .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))?;
                if old.as_ref() != Some(&value) {
                    changes.push(Change::Write {
                        path: format!("schema/{name}.json").into(),
                        bytes: canonical::pretty(&value),
                    });
                }
            }
            for r in &db.catalog.rows[&table] {
                changes.push(Change::Delete {
                    path: r.relative.clone(),
                });
                changes.push(Change::Write {
                    path: PathBuf::from(&new).join(r.relative.file_name().unwrap()),
                    bytes: r.raw.clone(),
                })
            }
            commit_changes(db, changes, "migration", format, cli)
        }
        MigrateCommand::AddColumn {
            table,
            column,
            kind,
            nullable,
            default,
        } => {
            let mut s = schema_for(db, &table)?.clone();
            if s.columns.contains_key(&column) {
                return Err(DbError::new(
                    "SCHEMA_COLUMN_UNKNOWN",
                    "column already exists",
                    2,
                ));
            }
            let kind = parse_type(&kind)?;
            let default = default
                .map(|x| {
                    serde_json::from_str(&x)
                        .map_err(|e| DbError::usage(format!("invalid default: {e}")))
                })
                .transpose()?;
            if !nullable && default.is_none() && !db.catalog.rows[&table].is_empty() {
                return Err(DbError::new(
                    "ROW_MISSING_FIELD",
                    "non-null column on a nonempty table requires --default",
                    2,
                ));
            }
            s.columns.insert(
                column.clone(),
                Column {
                    kind,
                    nullable,
                    default: default.clone(),
                    generated: None,
                    values: None,
                    items: None,
                    properties: None,
                    description: None,
                },
            );
            rewrite_schema_rows(
                db,
                s,
                move |r| {
                    r.insert(column.clone(), default.clone().unwrap_or(Value::Null));
                    Ok(())
                },
                format,
                cli,
            )
        }
        MigrateCommand::DropColumn { table, column } => {
            let mut s = schema_for(db, &table)?.clone();
            if s.primary_key.contains(&column)
                || s.unique.iter().any(|x| x.contains(&column))
                || s.foreign_keys.iter().any(|x| x.columns.contains(&column))
            {
                return Err(DbError::new(
                    "SCHEMA_COLUMN_UNKNOWN",
                    "drop dependent constraints in the same declarative migration",
                    2,
                ));
            }
            s.columns.shift_remove(&column).ok_or_else(|| {
                DbError::new("UNKNOWN_COLUMN", format!("unknown column {column}"), 4)
            })?;
            rewrite_schema_rows(
                db,
                s,
                move |r| {
                    r.remove(&column);
                    Ok(())
                },
                format,
                cli,
            )
        }
        MigrateCommand::RenameColumn { table, column, new } => {
            let mut s = schema_for(db, &table)?.clone();
            if !s.columns.contains_key(&column) || s.columns.contains_key(&new) {
                return Err(DbError::new(
                    "UNKNOWN_COLUMN",
                    "source missing or target already exists",
                    4,
                ));
            }
            let index = s.columns.get_index_of(&column).unwrap();
            let col = s.columns.shift_remove(&column).unwrap();
            s.columns.shift_insert(index, new.clone(), col);
            for x in &mut s.primary_key {
                if x == &column {
                    *x = new.clone()
                }
            }
            for set in s.unique.iter_mut().chain(s.indexes.iter_mut()) {
                for x in set {
                    if x == &column {
                        *x = new.clone()
                    }
                }
            }
            for fk in &mut s.foreign_keys {
                for x in &mut fk.columns {
                    if x == &column {
                        *x = new.clone()
                    }
                }
                if fk.references.table == table {
                    for x in &mut fk.references.columns {
                        if x == &column {
                            *x = new.clone()
                        }
                    }
                }
            }
            if let Some(storage) = &mut s.storage {
                for x in &mut storage.filename {
                    if x == &column {
                        *x = new.clone()
                    }
                }
            }
            for check in &mut s.check {
                check.expr = rename_expression_identifier(&check.expr, &column, &new)?;
            }
            let mut changes = schema_row_changes(db, &s, |r| {
                if let Some(v) = r.remove(&column) {
                    r.insert(new.clone(), v);
                }
                Ok(())
            })?;
            for (name, other) in &db.catalog.schemas {
                if name == &table {
                    continue;
                }
                let mut updated = other.clone();
                for fk in &mut updated.foreign_keys {
                    if fk.references.table == table {
                        for target in &mut fk.references.columns {
                            if target == &column {
                                *target = new.clone()
                            }
                        }
                    }
                }
                if serde_json::to_value(other).ok() != serde_json::to_value(&updated).ok() {
                    let value = serde_json::to_value(updated)
                        .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))?;
                    changes.push(Change::Write {
                        path: format!("schema/{name}.json").into(),
                        bytes: canonical::pretty(&value),
                    });
                }
            }
            commit_changes(db, changes, "migration", format, cli)
        }
        MigrateCommand::ChangeType {
            table,
            column,
            kind,
            using,
        } => {
            let mut s = schema_for(db, &table)?.clone();
            let target = parse_type(&kind)?;
            s.columns
                .get_mut(&column)
                .ok_or_else(|| {
                    DbError::new("UNKNOWN_COLUMN", format!("unknown column {column}"), 4)
                })?
                .kind = target.clone();
            if let Some(expr) = using {
                let old = schema_for(db, &table)?;
                let where_sql = old
                    .primary_key
                    .iter()
                    .map(|c| format!("{} = ?", quote(c)))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                rewrite_schema_rows(
                    db,
                    s,
                    |row| {
                        let params: Vec<_> = old
                            .primary_key
                            .iter()
                            .map(|c| row.get(c).cloned().unwrap_or(Value::Null))
                            .collect();
                        let query = format!(
                            "SELECT ({expr}) AS value FROM {} WHERE {where_sql}",
                            quote(&table)
                        );
                        let result = crate::sql::execute(&db.catalog, &query, &params)?;
                        let value = result
                            .rows
                            .first()
                            .and_then(|r| r.get("value"))
                            .cloned()
                            .ok_or_else(|| {
                                DbError::new(
                                    "QUERY_TYPE_ERROR",
                                    "conversion expression produced no value",
                                    4,
                                )
                            })?;
                        row.insert(column.clone(), convert_query_value(value, &target)?);
                        Ok(())
                    },
                    format,
                    cli,
                )
            } else {
                commit_changes(
                    db,
                    vec![Change::Write {
                        path: format!("schema/{table}.json").into(),
                        bytes: canonical::pretty(&serde_json::to_value(&s).map_err(|e| {
                            DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6)
                        })?),
                    }],
                    "migration",
                    format,
                    cli,
                )
            }
        }
        MigrateCommand::AddConstraint { table, definition } => {
            let mut s = schema_for(db, &table)?.clone();
            let def: ConstraintDefinition = serde_json::from_str(&definition)
                .map_err(|e| DbError::usage(format!("invalid constraint definition: {e}")))?;
            match def {
                ConstraintDefinition::Unique { columns } => {
                    if s.unique.contains(&columns) {
                        return Err(DbError::usage("unique constraint already exists"));
                    }
                    s.unique.push(columns)
                }
                ConstraintDefinition::ForeignKey { foreign_key } => {
                    s.foreign_keys.push(foreign_key)
                }
                ConstraintDefinition::Check { check } => {
                    if s.check.iter().any(|c| c.name == check.name) {
                        return Err(DbError::usage("check name already exists"));
                    }
                    s.check.push(check)
                }
            }
            commit_schema(db, s, format, cli)
        }
        MigrateCommand::DropConstraint { table, name } => {
            let mut s = schema_for(db, &table)?.clone();
            let before = (s.unique.len(), s.foreign_keys.len(), s.check.len());
            s.check.retain(|c| c.name != name);
            s.unique
                .retain(|c| format!("unique_{}", c.join("_")) != name);
            s.foreign_keys
                .retain(|c| format!("fk_{}", c.columns.join("_")) != name);
            if before == (s.unique.len(), s.foreign_keys.len(), s.check.len()) {
                return Err(DbError::usage(format!("unknown constraint {name:?}")));
            }
            commit_schema(db, s, format, cli)
        }
        MigrateCommand::AddIndex { table, columns } => {
            let mut s = schema_for(db, &table)?.clone();
            if columns.is_empty() {
                return Err(DbError::usage("index requires columns"));
            }
            if s.indexes.contains(&columns) {
                println!("no change: index already exists");
                return Ok(0);
            }
            s.indexes.push(columns);
            commit_schema(db, s, format, cli)
        }
        MigrateCommand::DropIndex { table, columns } => {
            let mut s = schema_for(db, &table)?.clone();
            let before = s.indexes.len();
            s.indexes.retain(|x| x != &columns);
            if before == s.indexes.len() {
                return Err(DbError::usage("index does not exist"));
            }
            commit_schema(db, s, format, cli)
        }
        MigrateCommand::Apply { file } => {
            let document: MigrationDocument =
                serde_json::from_slice(&fs::read(&file).map_err(|e| DbError::io(&file, e))?)
                    .map_err(|e| DbError::usage(format!("invalid migration JSON: {e}")))?;
            let changes = declarative_migration_changes(db, document)?;
            commit_changes(db, changes, "migration", format, cli)
        }
    }
}
fn commit_schema(db: &Database, s: Schema, format: Format, cli: &Cli) -> Result<i32> {
    let table = s.table.clone();
    let value = serde_json::to_value(s)
        .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))?;
    commit_changes(
        db,
        vec![Change::Write {
            path: format!("schema/{table}.json").into(),
            bytes: canonical::pretty(&value),
        }],
        "migration",
        format,
        cli,
    )
}
fn declarative_migration_changes(db: &Database, doc: MigrationDocument) -> Result<Vec<Change>> {
    let mut schemas = db.catalog.schemas.clone();
    let mut rows: std::collections::BTreeMap<String, Vec<Map<String, Value>>> = db
        .catalog
        .rows
        .iter()
        .map(|(t, rs)| (t.clone(), rs.iter().map(|r| r.value.clone()).collect()))
        .collect();
    for op in doc.operations {
        match op {
            MigrationOperation::AddTable { table, schema } => {
                if schemas.contains_key(&table) || schema.table != table {
                    return Err(DbError::new(
                        "SCHEMA_TABLE_NAME_MISMATCH",
                        format!("cannot add table {table:?}: schema identity conflicts"),
                        2,
                    ));
                }
                schemas.insert(table.clone(), schema);
                rows.insert(table, vec![]);
            }
            MigrationOperation::DropTable { table } => {
                if schemas.remove(&table).is_none() {
                    return Err(DbError::new(
                        "UNKNOWN_TABLE",
                        format!("unknown table {table}"),
                        4,
                    ));
                }
                rows.remove(&table);
            }
            MigrationOperation::RenameTable { table, new } => {
                if schemas.contains_key(&new) {
                    return Err(DbError::new(
                        "SCHEMA_INVALID_TABLE_NAME",
                        format!("table {new} already exists"),
                        2,
                    ));
                }
                let mut s = schemas.remove(&table).ok_or_else(|| {
                    DbError::new("UNKNOWN_TABLE", format!("unknown table {table}"), 4)
                })?;
                s.table = new.clone();
                schemas.insert(new.clone(), s);
                let moved = rows.remove(&table).unwrap_or_default();
                rows.insert(new.clone(), moved);
                for s in schemas.values_mut() {
                    for fk in &mut s.foreign_keys {
                        if fk.references.table == table {
                            fk.references.table = new.clone()
                        }
                    }
                }
            }
            MigrationOperation::AddColumn {
                table,
                column,
                kind,
                nullable,
                default,
            } => {
                let s = schemas.get_mut(&table).ok_or_else(|| {
                    DbError::new("UNKNOWN_TABLE", format!("unknown table {table}"), 4)
                })?;
                if s.columns.contains_key(&column) {
                    return Err(DbError::new(
                        "SCHEMA_COLUMN_UNKNOWN",
                        format!("column {column} already exists"),
                        2,
                    ));
                }
                if !nullable && default.is_none() && !rows[&table].is_empty() {
                    return Err(DbError::new(
                        "ROW_MISSING_FIELD",
                        "non-null column on a nonempty table requires a default",
                        2,
                    ));
                }
                s.columns.insert(
                    column.clone(),
                    Column {
                        kind,
                        nullable,
                        default: default.clone(),
                        generated: None,
                        values: None,
                        items: None,
                        properties: None,
                        description: None,
                    },
                );
                for row in rows.get_mut(&table).unwrap() {
                    row.insert(column.clone(), default.clone().unwrap_or(Value::Null));
                }
            }
            MigrationOperation::DropColumn { table, column } => {
                let s = schemas.get_mut(&table).ok_or_else(|| {
                    DbError::new("UNKNOWN_TABLE", format!("unknown table {table}"), 4)
                })?;
                if s.columns.shift_remove(&column).is_none() {
                    return Err(DbError::new(
                        "UNKNOWN_COLUMN",
                        format!("unknown column {column}"),
                        4,
                    ));
                }
                for row in rows.get_mut(&table).unwrap() {
                    row.remove(&column);
                }
            }
            MigrationOperation::RenameColumn { table, column, new } => {
                let s = schemas.get_mut(&table).ok_or_else(|| {
                    DbError::new("UNKNOWN_TABLE", format!("unknown table {table}"), 4)
                })?;
                let index = s.columns.get_index_of(&column).ok_or_else(|| {
                    DbError::new("UNKNOWN_COLUMN", format!("unknown column {column}"), 4)
                })?;
                if s.columns.contains_key(&new) {
                    return Err(DbError::new(
                        "SCHEMA_COLUMN_UNKNOWN",
                        format!("column {new} already exists"),
                        2,
                    ));
                }
                let col = s.columns.shift_remove(&column).unwrap();
                s.columns.shift_insert(index, new.clone(), col);
                rename_schema_column(s, &column, &new)?;
                for row in rows.get_mut(&table).unwrap() {
                    if let Some(v) = row.remove(&column) {
                        row.insert(new.clone(), v);
                    }
                }
                for (other_name, other) in schemas.iter_mut() {
                    if other_name == &table {
                        continue;
                    }
                    for fk in &mut other.foreign_keys {
                        if fk.references.table == table {
                            for x in &mut fk.references.columns {
                                if x == &column {
                                    *x = new.clone()
                                }
                            }
                        }
                    }
                }
            }
            MigrationOperation::ChangeType {
                table,
                column,
                kind,
                using,
            } => {
                if let Some(expr) = using {
                    let cat = virtual_catalog(&schemas, &rows)?;
                    let s = schemas
                        .get(&table)
                        .ok_or_else(|| {
                            DbError::new("UNKNOWN_TABLE", format!("unknown table {table}"), 4)
                        })?
                        .clone();
                    let where_sql = s
                        .primary_key
                        .iter()
                        .map(|c| format!("{} = ?", quote(c)))
                        .collect::<Vec<_>>()
                        .join(" AND ");
                    for row in rows.get_mut(&table).ok_or_else(|| {
                        DbError::new("UNKNOWN_TABLE", format!("unknown table {table}"), 4)
                    })? {
                        let params: Vec<_> = s
                            .primary_key
                            .iter()
                            .map(|c| row.get(c).cloned().unwrap_or(Value::Null))
                            .collect();
                        let q = format!(
                            "SELECT ({expr}) AS value FROM {} WHERE {where_sql}",
                            quote(&table)
                        );
                        let result = crate::sql::execute(&cat, &q, &params)?;
                        let value = result
                            .rows
                            .first()
                            .and_then(|r| r.get("value"))
                            .cloned()
                            .ok_or_else(|| {
                                DbError::new(
                                    "QUERY_TYPE_ERROR",
                                    "conversion expression produced no value",
                                    4,
                                )
                            })?;
                        row.insert(column.clone(), convert_query_value(value, &kind)?);
                    }
                }
                schemas
                    .get_mut(&table)
                    .and_then(|s| s.columns.get_mut(&column))
                    .ok_or_else(|| {
                        DbError::new("UNKNOWN_COLUMN", format!("unknown {table}.{column}"), 4)
                    })?
                    .kind = kind;
            }
            MigrationOperation::AddConstraint { table, definition } => {
                let s = schemas.get_mut(&table).ok_or_else(|| {
                    DbError::new("UNKNOWN_TABLE", format!("unknown table {table}"), 4)
                })?;
                add_constraint(s, definition)?;
            }
            MigrationOperation::DropConstraint { table, name } => {
                let s = schemas.get_mut(&table).ok_or_else(|| {
                    DbError::new("UNKNOWN_TABLE", format!("unknown table {table}"), 4)
                })?;
                drop_constraint(s, &name)?;
            }
            MigrationOperation::AddIndex { table, columns } => {
                let s = schemas.get_mut(&table).ok_or_else(|| {
                    DbError::new("UNKNOWN_TABLE", format!("unknown table {table}"), 4)
                })?;
                if !s.indexes.contains(&columns) {
                    s.indexes.push(columns)
                }
            }
            MigrationOperation::DropIndex { table, columns } => {
                let s = schemas.get_mut(&table).ok_or_else(|| {
                    DbError::new("UNKNOWN_TABLE", format!("unknown table {table}"), 4)
                })?;
                let n = s.indexes.len();
                s.indexes.retain(|x| x != &columns);
                if n == s.indexes.len() {
                    return Err(DbError::usage("index does not exist"));
                }
            }
        }
    }
    authoritative_diff(db, &schemas, &rows)
}
fn add_constraint(s: &mut Schema, definition: ConstraintDefinition) -> Result<()> {
    match definition {
        ConstraintDefinition::Unique { columns } => {
            if s.unique.contains(&columns) {
                return Err(DbError::usage("unique constraint already exists"));
            }
            s.unique.push(columns)
        }
        ConstraintDefinition::ForeignKey { foreign_key } => s.foreign_keys.push(foreign_key),
        ConstraintDefinition::Check { check } => {
            if s.check.iter().any(|x| x.name == check.name) {
                return Err(DbError::usage("check name already exists"));
            }
            s.check.push(check)
        }
    }
    Ok(())
}
fn drop_constraint(s: &mut Schema, name: &str) -> Result<()> {
    let before = (s.unique.len(), s.foreign_keys.len(), s.check.len());
    s.check.retain(|c| c.name != name);
    s.unique
        .retain(|c| format!("unique_{}", c.join("_")) != name);
    s.foreign_keys
        .retain(|c| format!("fk_{}", c.columns.join("_")) != name);
    if before == (s.unique.len(), s.foreign_keys.len(), s.check.len()) {
        return Err(DbError::usage(format!("unknown constraint {name:?}")));
    }
    Ok(())
}
fn rename_schema_column(s: &mut Schema, old: &str, new: &str) -> Result<()> {
    for x in &mut s.primary_key {
        if x == old {
            *x = new.into()
        }
    }
    for set in s.unique.iter_mut().chain(s.indexes.iter_mut()) {
        for x in set {
            if x == old {
                *x = new.into()
            }
        }
    }
    for fk in &mut s.foreign_keys {
        for x in &mut fk.columns {
            if x == old {
                *x = new.into()
            }
        }
        if fk.references.table == s.table {
            for x in &mut fk.references.columns {
                if x == old {
                    *x = new.into()
                }
            }
        }
    }
    if let Some(storage) = &mut s.storage {
        for x in &mut storage.filename {
            if x == old {
                *x = new.into()
            }
        }
    }
    for check in &mut s.check {
        check.expr = rename_expression_identifier(&check.expr, old, new)?
    }
    Ok(())
}
fn virtual_catalog(
    schemas: &std::collections::BTreeMap<String, Schema>,
    rows: &std::collections::BTreeMap<String, Vec<Map<String, Value>>>,
) -> Result<crate::catalog::Catalog> {
    let temp = tempfile::tempdir().map_err(|e| DbError::io(Path::new("/tmp"), e))?;
    fs::create_dir(temp.path().join("schema")).map_err(|e| DbError::io(temp.path(), e))?;
    for (t, s) in schemas {
        metadata::write_json_atomic(&temp.path().join(format!("schema/{t}.json")), s)?;
        fs::create_dir(temp.path().join(t)).map_err(|e| DbError::io(&temp.path().join(t), e))?;
        for row in &rows[t] {
            let name = canonical::filename(s, row).ok_or_else(|| {
                DbError::new(
                    "IDENTITY_MISMATCH",
                    format!("cannot derive filename for {t}"),
                    2,
                )
            })?;
            fs::write(
                temp.path().join(t).join(name),
                canonical::pretty(&canonical::canonical_row(row, s)),
            )
            .map_err(|e| DbError::io(temp.path(), e))?;
        }
    }
    crate::catalog::Catalog::observe(temp.path(), &crate::config::Config::default())
}
fn authoritative_diff(
    db: &Database,
    schemas: &std::collections::BTreeMap<String, Schema>,
    rows: &std::collections::BTreeMap<String, Vec<Map<String, Value>>>,
) -> Result<Vec<Change>> {
    let mut changes = vec![];
    for t in db.catalog.schemas.keys() {
        if !schemas.contains_key(t) {
            changes.push(Change::Delete {
                path: format!("schema/{t}.json").into(),
            })
        }
    }
    for (t, s) in schemas {
        let value = serde_json::to_value(s)
            .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))?;
        if db
            .catalog
            .schemas
            .get(t)
            .and_then(|x| serde_json::to_value(x).ok())
            .as_ref()
            != Some(&value)
        {
            changes.push(Change::Write {
                path: format!("schema/{t}.json").into(),
                bytes: canonical::pretty(&value),
            })
        }
    }
    let old: std::collections::BTreeMap<_, _> = db
        .catalog
        .rows
        .values()
        .flatten()
        .map(|r| (r.relative.clone(), r.value.clone()))
        .collect();
    let mut new = std::collections::BTreeMap::new();
    for (t, rs) in rows {
        let s = &schemas[t];
        for row in rs {
            let path = PathBuf::from(t).join(canonical::filename(s, row).ok_or_else(|| {
                DbError::new(
                    "IDENTITY_MISMATCH",
                    format!("cannot derive filename for {t}"),
                    2,
                )
            })?);
            new.insert(path, row.clone());
        }
    }
    for p in old.keys() {
        if !new.contains_key(p) {
            changes.push(Change::Delete { path: p.clone() })
        }
    }
    for (p, row) in new {
        if old.get(&p) != Some(&row) {
            let t = p
                .components()
                .next()
                .unwrap()
                .as_os_str()
                .to_string_lossy()
                .to_string();
            changes.push(Change::Write {
                path: p,
                bytes: canonical::pretty(&canonical::canonical_row(&row, &schemas[&t])),
            })
        }
    }
    Ok(changes)
}
fn rewrite_schema_rows<F: Fn(&mut Map<String, Value>) -> Result<()>>(
    db: &Database,
    s: Schema,
    edit: F,
    format: Format,
    cli: &Cli,
) -> Result<i32> {
    let changes = schema_row_changes(db, &s, edit)?;
    commit_changes(db, changes, "migration", format, cli)
}
fn schema_row_changes<F: Fn(&mut Map<String, Value>) -> Result<()>>(
    db: &Database,
    s: &Schema,
    edit: F,
) -> Result<Vec<Change>> {
    let table = s.table.clone();
    let mut changes = vec![Change::Write {
        path: format!("schema/{table}.json").into(),
        bytes: canonical::pretty(
            &serde_json::to_value(s)
                .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))?,
        ),
    }];
    for row in &db.catalog.rows[&table] {
        let mut r = row.value.clone();
        edit(&mut r)?;
        let new = PathBuf::from(&table).join(canonical::filename(&s, &r).ok_or_else(|| {
            DbError::new(
                "IDENTITY_MISMATCH",
                "cannot derive migrated row filename",
                2,
            )
        })?);
        if new != row.relative {
            changes.push(Change::Delete {
                path: row.relative.clone(),
            })
        }
        changes.push(Change::Write {
            path: new,
            bytes: canonical::pretty(&canonical::canonical_row(&r, &s)),
        })
    }
    Ok(changes)
}

fn commit_changes(
    db: &Database,
    changes: Vec<Change>,
    origin: &str,
    format: Format,
    cli: &Cli,
) -> Result<i32> {
    require_writable(cli)?;
    if changes.is_empty() {
        println!("no change");
        return Ok(0);
    }
    let paths = transaction::commit(
        &db.root,
        &db.config,
        &current_root(db)?,
        &changes,
        origin,
        cli.dry_run,
    )?;
    print_mutation(
        &paths,
        db.manifest.as_ref().map_or(1, |m| m.revision + 1),
        cli.dry_run,
        format,
    )?;
    Ok(0)
}
fn require_writable(cli: &Cli) -> Result<()> {
    if cli.readonly && !cli.dry_run {
        return Err(DbError::new(
            "READ_ONLY",
            "this operation would write, but the database is in read-only mode",
            1,
        ));
    }
    Ok(())
}
fn print_mutation(paths: &[PathBuf], revision: u64, dry: bool, format: Format) -> Result<()> {
    let rows = paths
        .iter()
        .map(|p| {
            obj([
                (
                    "kind",
                    Value::String(if dry { "planned_change" } else { "change" }.into()),
                ),
                ("path", Value::String(p.display().to_string())),
                ("revision", Value::from(revision)),
            ])
        })
        .collect::<Vec<_>>();
    if format == Format::Table {
        let suffix = if dry {
            String::new()
        } else {
            format!("; revision {revision}")
        };
        println!(
            "{} {} path(s){}",
            if dry { "would change" } else { "changed" },
            paths.len(),
            suffix
        );
        for p in paths {
            println!("  {}", p.display())
        }
    } else {
        output::records(&rows, format)?
    }
    Ok(())
}
fn current_root(db: &Database) -> Result<String> {
    Ok(metadata::state(&db.catalog)?.0)
}
fn schema_for<'a>(db: &'a Database, table: &str) -> Result<&'a Schema> {
    db.catalog
        .schemas
        .get(table)
        .ok_or_else(|| DbError::new("UNKNOWN_TABLE", format!("unknown table {table:?}"), 4))
}
fn find_row<'a>(db: &'a Database, table: &str, key: &str) -> Result<&'a crate::catalog::Row> {
    let s = schema_for(db, table)?;
    let vals = key_values(key, s.primary_key.len())?;
    let k = canonical::compact(&Value::Array(vals));
    crate::integrity::rows_by_key(&db.catalog, table)
        .get(&k)
        .copied()
        .ok_or_else(|| DbError::new("UNKNOWN_ROW", format!("no row with key {key}"), 4))
}
fn key_values(text: &str, count: usize) -> Result<Vec<Value>> {
    if count == 1 {
        return Ok(vec![
            serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.into())),
        ]);
    }
    let v: Value = serde_json::from_str(text)
        .map_err(|_| DbError::usage("composite primary keys must be a JSON array"))?;
    let a = v
        .as_array()
        .cloned()
        .ok_or_else(|| DbError::usage("composite primary keys must be a JSON array"))?;
    if a.len() != count {
        return Err(DbError::usage(format!(
            "primary key requires {count} values"
        )));
    }
    Ok(a)
}
fn materialize(row: &mut Map<String, Value>, s: &Schema, seq: i64) {
    for (n, c) in &s.columns {
        if !row.contains_key(n) {
            row.insert(
                n.clone(),
                c.default
                    .clone()
                    .or_else(|| crate::value::generate(c, seq))
                    .unwrap_or(Value::Null),
            );
        }
    }
}
fn read_limited(reader: impl Read, limit: u64, source: &Path) -> Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| DbError::io(source, error))?;
    if bytes.len() as u64 > limit {
        return Err(DbError::new(
            "RESOURCE_LIMIT",
            format!("input exceeds the configured {limit} byte transaction limit"),
            2,
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| DbError::usage(format!("input is not valid UTF-8: {error}")))
}
fn next_sequence(db: &Database, table: &str, schema: &Schema) -> Result<i64> {
    let maximum = schema
        .columns
        .iter()
        .filter(|(_, column)| {
            column
                .generated
                .as_ref()
                .is_some_and(|generated| matches!(generated.kind, GeneratedKind::Sequence))
        })
        .flat_map(|(name, _)| {
            db.catalog.rows[table]
                .iter()
                .filter_map(move |row| row.value.get(name).and_then(Value::as_i64))
        })
        .max()
        .unwrap_or(0);
    maximum
        .checked_add(1)
        .ok_or_else(|| DbError::new("RESOURCE_LIMIT", "generated sequence exhausted i64", 2))
}
fn parse_csv(v: &str, c: &Column) -> Result<Value> {
    if v.is_empty() && c.nullable {
        return Ok(Value::Null);
    }
    let parsed = match c.kind {
        ColumnType::Bool => Value::Bool(
            v.parse()
                .map_err(|_| DbError::new("TYPE_MISMATCH", format!("{v:?} is not bool"), 2))?,
        ),
        ColumnType::Int => Value::from(
            v.parse::<i64>()
                .map_err(|_| DbError::new("TYPE_MISMATCH", format!("{v:?} is not int"), 2))?,
        ),
        ColumnType::Float => Value::from(
            v.parse::<f64>()
                .map_err(|_| DbError::new("TYPE_MISMATCH", format!("{v:?} is not float"), 2))?,
        ),
        ColumnType::Array | ColumnType::Object | ColumnType::Json => {
            serde_json::from_str(v).map_err(|e| DbError::new("TYPE_MISMATCH", e.to_string(), 2))?
        }
        _ => Value::String(v.into()),
    };
    Ok(parsed)
}
fn parse_type(s: &str) -> Result<ColumnType> {
    serde_json::from_str(&format!("\"{s}\""))
        .map_err(|_| DbError::new("SCHEMA_TYPE_UNKNOWN", format!("unknown type {s:?}"), 2))
}
fn convert_query_value(v: Value, target: &ColumnType) -> Result<Value> {
    match target {
        ColumnType::Bool => match v {
            Value::Bool(_) => Ok(v),
            Value::Number(n) if n.as_i64() == Some(0) => Ok(Value::Bool(false)),
            Value::Number(n) if n.as_i64() == Some(1) => Ok(Value::Bool(true)),
            _ => Err(DbError::new(
                "TYPE_MISMATCH",
                "conversion result is not boolean",
                2,
            )),
        },
        ColumnType::Int => v
            .as_i64()
            .map(Value::from)
            .ok_or_else(|| DbError::new("TYPE_MISMATCH", "conversion result is not int", 2)),
        ColumnType::Float => v
            .as_f64()
            .map(Value::from)
            .ok_or_else(|| DbError::new("TYPE_MISMATCH", "conversion result is not float", 2)),
        _ => Ok(v),
    }
}
fn rename_expression_identifier(expr: &str, old: &str, new: &str) -> Result<String> {
    let chars: Vec<char> = expr.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\'' {
            out.push(chars[i]);
            i += 1;
            while i < chars.len() {
                out.push(chars[i]);
                if chars[i] == '\'' {
                    if i + 1 < chars.len() && chars[i + 1] == '\'' {
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1
            }
            continue;
        }
        if chars[i] == '"' {
            out.push('"');
            i += 1;
            let mut ident = String::new();
            while i < chars.len() {
                if chars[i] == '"' {
                    if i + 1 < chars.len() && chars[i + 1] == '"' {
                        ident.push('"');
                        i += 2;
                        continue;
                    }
                    break;
                }
                ident.push(chars[i]);
                i += 1
            }
            if i >= chars.len() {
                return Err(DbError::new(
                    "SCHEMA_CHECK_INVALID",
                    format!("unterminated quoted identifier in {expr:?}"),
                    2,
                ));
            }
            let value = if ident == old { new } else { &ident };
            out.push_str(&value.replace('"', "\"\""));
            out.push('"');
            i += 1;
            continue;
        }
        if chars[i].is_alphabetic() || chars[i] == '_' {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '$')) {
                i += 1
            }
            let word: String = chars[start..i].iter().collect();
            out.push_str(if word == old { new } else { &word });
            continue;
        }
        out.push(chars[i]);
        i += 1
    }
    Ok(out)
}
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
fn obj<const N: usize>(items: [(&str, Value); N]) -> Map<String, Value> {
    items.into_iter().map(|(k, v)| (k.into(), v)).collect()
}
fn valid_snapshot(s: &str) -> Result<()> {
    if s.is_empty() || s.contains('/') || s.contains('\\') || s == "." || s == ".." {
        return Err(DbError::new("PATH_VIOLATION", "invalid snapshot name", 2));
    }
    Ok(())
}
fn copy_authoritative(db: &Database, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest.join("schema")).map_err(|e| DbError::io(dest, e))?;
    fs::create_dir_all(dest.join(".db")).map_err(|e| DbError::io(dest, e))?;
    for name in ["format", "config"] {
        let source = db.root.join(".db").join(name);
        copy_file_synced(&source, &dest.join(".db").join(name))?;
    }
    for (t, rows) in &db.catalog.rows {
        fs::create_dir_all(dest.join(t)).map_err(|e| DbError::io(dest, e))?;
        for r in rows {
            copy_file_synced(&r.path, &dest.join(&r.relative))?;
        }
    }
    for t in db.catalog.schemas.keys() {
        let src = db.root.join(format!("schema/{t}.json"));
        copy_file_synced(&src, &dest.join(format!("schema/{t}.json")))?;
    }
    Ok(())
}
fn copy_file_synced(source: &Path, destination: &Path) -> Result<()> {
    fs::copy(source, destination).map_err(|error| DbError::io(source, error))?;
    fs::File::open(destination)
        .and_then(|file| file.sync_all())
        .map_err(|error| DbError::io(destination, error))
}
fn create_snapshot(db: &Database, name: &str) -> Result<()> {
    let base = db.root.join(".db/snapshots");
    fs::create_dir_all(&base).map_err(|error| DbError::io(&base, error))?;
    let destination = base.join(name);
    if destination.exists() {
        return Err(DbError::new(
            "SNAPSHOT_EXISTS",
            format!("snapshot {name:?} already exists"),
            1,
        ));
    }
    let staging = tempfile::Builder::new()
        .prefix(".creating-")
        .tempdir_in(&base)
        .map_err(|error| DbError::io(&base, error))?;
    copy_authoritative(db, staging.path())?;
    let staging = staging.keep();
    if let Err(error) = fs::rename(&staging, &destination) {
        let _ = fs::remove_dir_all(&staging);
        return Err(DbError::io(&destination, error));
    }
    metadata::sync_parent(&destination)
}
fn changes_from_snapshot(db: &Database, src: &Path) -> Result<Vec<Change>> {
    crate::db::validate_format(src)?;
    let snapshot_config = crate::db::load_config(src)?;
    let snap = crate::catalog::Catalog::observe(src, &snapshot_config)?;
    let errors = crate::integrity::validate(&snap);
    if let Some(d) = errors.into_iter().next() {
        return Err(DbError::from_diag(d, 2));
    }
    let mut changes = vec![];
    for name in ["format", "config"] {
        let rel = PathBuf::from(".db").join(name);
        let source = src.join(&rel);
        if !source.is_file() {
            return Err(DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                format!("snapshot is missing {}", rel.display()),
                6,
            ));
        }
        let bytes = fs::read(&source).map_err(|e| DbError::io(&source, e))?;
        if fs::read(db.root.join(&rel)).map_err(|e| DbError::io(&db.root.join(&rel), e))? != bytes {
            changes.push(Change::Write { path: rel, bytes });
        }
    }
    for (t, _) in &db.catalog.schemas {
        changes.push(Change::Delete {
            path: format!("schema/{t}.json").into(),
        });
        for r in &db.catalog.rows[t] {
            changes.push(Change::Delete {
                path: r.relative.clone(),
            })
        }
    }
    for (t, _) in &snap.schemas {
        let p = PathBuf::from(format!("schema/{t}.json"));
        changes.push(Change::Write {
            path: p.clone(),
            bytes: fs::read(src.join(&p)).map_err(|e| DbError::io(&src.join(&p), e))?,
        });
        for r in &snap.rows[t] {
            changes.push(Change::Write {
                path: r.relative.clone(),
                bytes: r.raw.clone(),
            })
        }
    }
    Ok(changes)
}
fn completions(shell: &str) -> Result<i32> {
    let shell: clap_complete::Shell = shell
        .parse()
        .map_err(|_| DbError::usage("shell must be bash, zsh, fish, elvish, or powershell"))?;
    clap_complete::generate(shell, &mut Cli::command(), "db", &mut io::stdout());
    Ok(0)
}

fn metadata_writable(root: &Path) -> bool {
    let Ok(md) = fs::metadata(root.join(".db")) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o222 != 0
    }
    #[cfg(not(unix))]
    {
        !md.permissions().readonly()
    }
}
