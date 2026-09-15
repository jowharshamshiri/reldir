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
    about = "Filesystem-native relational JSON database",
    // `db 'SELECT ...'` runs the query and bare `db` opens the shell, so the
    // subcommand is optional and a leading positional that is not a subcommand
    // name is captured as SQL. Global flags must remain usable alongside a
    // subcommand, so the positional only negates the requirement -- it never
    // conflicts with subcommand use.
    subcommand_negates_reqs = true
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
    #[arg(long, global = true)]
    max_json_file_size: Option<u64>,
    #[arg(long, global = true)]
    max_nesting_depth: Option<usize>,
    #[arg(long, global = true)]
    max_query_memory: Option<u64>,
    #[arg(long, global = true)]
    max_sort_memory: Option<u64>,
    #[arg(long, global = true)]
    max_temporary_disk: Option<u64>,
    #[arg(long, global = true)]
    max_result_rows: Option<usize>,
    #[arg(long, global = true)]
    max_transaction_size: Option<u64>,
    /// Never establish prerequisites implicitly. Reports what would be needed
    /// instead of creating it, which is the posture CI and debugging want.
    #[arg(long, global = true)]
    no_auto: bool,
    /// SQL to execute when no subcommand is given.
    #[arg(value_name = "SQL")]
    sql: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
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
        #[arg(long = "where", value_name = "EXPR")]
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
    #[command(
        subcommand,
        about = "Manage schemas",
        after_help = "Example: db schema show users"
    )]
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
    #[command(
        subcommand,
        about = "Manage snapshots",
        after_help = "Example: db snapshot list"
    )]
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
    #[command(
        subcommand,
        about = "Apply schema migrations",
        after_help = "Example: db migrate apply migration.json"
    )]
    Migrate(MigrateCommand),
}
#[derive(Subcommand, Clone)]
enum SchemaCommand {
    #[command(about = "Show a schema", after_help = "Example: db schema show users")]
    Show { table: String },
    #[command(
        about = "Create a minimal schema",
        after_help = "Example: db schema new users"
    )]
    New { table: String },
    #[command(
        about = "Accept an inferred schema",
        after_help = "Example: db schema accept users"
    )]
    Accept { table: String },
    #[command(
        about = "Validate all schemas",
        after_help = "Example: db schema validate"
    )]
    Validate,
}
#[derive(Subcommand, Clone)]
enum SnapshotCommand {
    #[command(after_help = "Example: db snapshot create before-import")]
    Create { name: String },
    #[command(after_help = "Example: db snapshot list")]
    List,
    #[command(after_help = "Example: db snapshot restore before-import --yes")]
    Restore { name: String },
    #[command(after_help = "Example: db snapshot delete before-import --yes")]
    Delete { name: String },
}
#[derive(Subcommand, Clone)]
enum MigrateCommand {
    #[command(after_help = "Example: db migrate add-table users --from users-schema.json")]
    AddTable {
        table: String,
        #[arg(long)]
        from: PathBuf,
    },
    #[command(after_help = "Example: db migrate drop-table users --dry-run")]
    DropTable { table: String },
    #[command(after_help = "Example: db migrate rename-table users people")]
    RenameTable { table: String, new: String },
    #[command(
        after_help = "Example: db migrate add-column users active --type bool --default true"
    )]
    AddColumn {
        table: String,
        column: String,
        #[arg(long = "type", value_name = "TYPE")]
        kind: String,
        #[arg(long)]
        nullable: bool,
        #[arg(long)]
        default: Option<String>,
    },
    #[command(after_help = "Example: db migrate drop-column users legacy_name")]
    DropColumn { table: String, column: String },
    #[command(after_help = "Example: db migrate rename-column users name display_name")]
    RenameColumn {
        table: String,
        column: String,
        new: String,
    },
    #[command(after_help = "Example: db migrate change-type users score float")]
    ChangeType {
        table: String,
        column: String,
        kind: String,
        #[arg(long)]
        using: Option<String>,
    },
    #[command(
        after_help = "Example: db migrate add-constraint users '{\"kind\":\"unique\",\"columns\":[\"email\"]}'"
    )]
    AddConstraint { table: String, definition: String },
    #[command(after_help = "Example: db migrate drop-constraint users unique_email")]
    DropConstraint { table: String, name: String },
    #[command(after_help = "Example: db migrate add-index users email")]
    AddIndex {
        table: String,
        #[arg(value_delimiter = ',')]
        columns: Vec<String>,
    },
    #[command(after_help = "Example: db migrate drop-index users email")]
    DropIndex {
        table: String,
        #[arg(value_delimiter = ',')]
        columns: Vec<String>,
    },
    #[command(after_help = "Example: db migrate apply migration.json")]
    Apply { file: PathBuf },
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
        schema: Box<Schema>,
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

struct InferOptions<'a> {
    table: Option<&'a str>,
    write: bool,
    all: bool,
    strictness: &'a str,
    pk: &'a [String],
    format: Format,
}

struct DoctorOptions<'a> {
    fix: bool,
    allow_data: bool,
    only: Option<&'a str>,
    explain: Option<&'a str>,
    no_snapshot: bool,
}

pub fn run(cli: Cli) -> Result<i32> {
    let mut settings = cli.clone();
    let resource_overrides = crate::config::ResourceOverrides {
        max_json_file_size: cli.max_json_file_size,
        max_nesting_depth: cli.max_nesting_depth,
        max_query_memory: cli.max_query_memory,
        max_sort_memory: cli.max_sort_memory,
        max_temporary_disk: cli.max_temporary_disk,
        max_result_rows: cli.max_result_rows,
        max_transaction_size: cli.max_transaction_size,
        timeout_seconds: cli.timeout,
    };
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
    // Section 49: colour on a TTY, disabled by --no-color and by NO_COLOR.
    // Settled once, before any command can emit, so every writer agrees.
    output::set_presentation(output::Presentation {
        color: !cli.no_color
            && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
            && io::stderr().is_terminal(),
        quiet: cli.quiet,
        verbose: cli.verbose,
    });
    // No subcommand: run the SQL given as a bare positional, or open the shell.
    // A bare invocation carries no intent to make this folder a database, so it
    // establishes nothing up front; the first statement that needs a persistent
    // database performs its own establishment.
    let command = match cli.command.clone() {
        Some(command) => command,
        None => match cli.sql.clone() {
            Some(sql) => Command::Sql {
                sql,
                params: vec![],
                explain: false,
                explain_analyze: false,
            },
            None => Command::Shell,
        },
    };
    match command {
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
            &resource_overrides,
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
            InferOptions {
                table: table.as_deref(),
                write,
                all,
                strictness: &strictness,
                pk: &pk,
                format,
            },
            &resource_overrides,
        ),
        command => {
            let resolved = crate::state::resolve_root(cli.db.as_deref())?;
            let root = resolved.path.clone();
            let observation = crate::state::observe(&root)?;

            // A read that promises not to write must not acquire hidden write
            // side effects, and a diagnosis must not alter what it reports on:
            // `check` in either form leaves derived state exactly as it found it.
            // `status` is not in this set. Adopting a valid external change is
            // the system's central promise, and recording that transition is
            // authoritative work, not derived repair: a status that observed a
            // change without accepting it would leave the database permanently
            // behind its own files.
            let diagnostic_only = matches!(
                command,
                Command::Check { .. }
                    | Command::Lint { .. }
                    | Command::Doctor { fix: false, .. }
                    | Command::Infer { write: false, .. }
                    | Command::Diff { .. }
            );
            if !observation.writable {
                settings.readonly = true;
            }

            let requirements = command_requirements(&command, &settings, diagnostic_only);
            // A dry run plans what a real run would do, under the same
            // requirements. It must not plan work the invocation would refuse:
            // `--no-auto` establishes nothing, so it has nothing to promise, and
            // printing a plan before refusing would describe work that was never
            // going to happen.
            let transitions = if cli.dry_run {
                crate::state::plan(&observation, requirements, &resource_overrides)?
            } else {
                crate::state::establish(&observation, requirements, &resource_overrides)?
            };
            report_transitions(&transitions, format, cli.dry_run)?;

            // A folder holding nothing at all has nothing to govern, and saying
            // so is the answer rather than a prerequisite to satisfy first.
            // Emptiness is a fact about the folder's contents, not about
            // whether metadata happens to exist: a folder full of ungoverned
            // JSON is emphatically not empty, and reporting it as such would
            // be a lie that hides the user's own data from them.
            if observation.format == crate::state::FormatState::Absent
                && observation.topology.is_empty()
            {
                return empty_database_result(&command, format);
            }

            // A diagnosis still adopts a valid external change -- that is
            // authoritative, not derived -- but leaves indexes and the manifest
            // exactly as it found them.
            let mode = if settings.readonly {
                ObserveMode::READ_ONLY
            } else if diagnostic_only {
                ObserveMode::DIAGNOSE
            } else {
                ObserveMode::RECORD
            };
            // The folder holds data but carries no metadata. An invocation that
            // *cannot* write still owes an answer, so the relational model is
            // built in memory and the files are read exactly as they are.
            //
            // Being unable to write is not the same as having been told not to
            // establish. `--no-auto` asks to be shown what is missing rather
            // than have it worked around, and a diagnostic's whole job is to
            // report the folder's state -- answering from an invented model
            // would conceal the very thing they were run to reveal. Both fall
            // through and surface UNINITIALIZED.
            let cannot_write = settings.readonly || cli.dry_run;
            let establishes_on_demand = matches!(command, Command::Shell);
            let answers_ephemerally =
                !cli.no_auto && (cannot_write || establishes_on_demand) && !diagnostic_only;
            let mut db = if observation.format == crate::state::FormatState::Absent
                && answers_ephemerally
            {
                let schemas = crate::state::ephemeral_schemas(&observation, &resource_overrides)?;
                Database::ephemeral(root, schemas, &resource_overrides)?
            } else {
                Database::open_with_overrides(root, mode, &resource_overrides)?
            };
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

/// What a command needs established before it can run.
///
/// Declared per command rather than inferred, so a new command has to answer
/// the question instead of inheriting the most permissive behavior.
fn command_requirements(
    command: &Command,
    cli: &Cli,
    diagnostic_only: bool,
) -> crate::state::Requirements {
    // `--no-auto` is the "do not change my prerequisites" posture, and
    // `--readonly` cannot write at all: both establish nothing, so they require
    // nothing.
    //
    // `--dry-run` is not in that set. It reports what a real run would do, which
    // it can only compute by asking what that run would require. Planning is not
    // establishing -- `plan` writes nothing -- so a dry run keeps the command's
    // ordinary requirements and simply stops short of performing them.
    if cli.no_auto || cli.readonly {
        return crate::state::Requirements::structural();
    }
    match command {
        // These operate on the folder rather than on relations. `Shell` joins
        // them because a bare invocation carries no intent to make this folder
        // a database; the first statement that needs one establishes it.
        Command::Recover | Command::UpgradeFormat | Command::Shell => {
            crate::state::Requirements::structural()
        }
        _ if diagnostic_only => crate::state::Requirements::diagnostic(),
        _ => crate::state::Requirements::functional(),
    }
}

/// Report automatic establishment once, after it succeeded and before the
/// command's own output.
fn report_transitions(
    transitions: &[crate::state::Transition],
    format: Format,
    planned: bool,
) -> Result<()> {
    if transitions.is_empty() {
        return Ok(());
    }
    if matches!(format, Format::Json | Format::Jsonl) {
        // Establishment is part of the machine-readable contract, so it is
        // emitted as data rather than as prose a consumer would have to parse.
        // `planned` distinguishes what happened from what would happen, which a
        // consumer cannot infer from the wording.
        let records = transitions
            .iter()
            .map(|transition| {
                obj([
                    ("kind", Value::String("state_transition".into())),
                    ("transition", Value::String(transition.kind().into())),
                    ("planned", Value::Bool(planned)),
                    (
                        "detail",
                        Value::String(if planned {
                            transition.describe_planned()
                        } else {
                            transition.describe()
                        }),
                    ),
                ])
            })
            .collect::<Vec<_>>();
        output::records(&records, format)?;
        return Ok(());
    }
    let summary = transitions
        .iter()
        .map(|transition| {
            if planned {
                transition.describe_planned()
            } else {
                transition.describe()
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    output::notice_stderr(&summary);
    Ok(())
}

/// The answer for a folder that holds no database and no data.
///
/// Emptiness is a legitimate state with a truthful answer, not a fault the user
/// has to clear before asking their first question.
fn empty_database_result(command: &Command, format: Format) -> Result<i32> {
    match command {
        // Commands whose answer is "there is nothing here" can say so directly.
        Command::Status | Command::Tables | Command::Check { .. } | Command::Lint { .. } => {
            event(
                format,
                obj([
                    ("kind", Value::String("status".into())),
                    ("valid", Value::Bool(true)),
                    ("state", Value::String("EMPTY".into())),
                    ("tables", Value::from(0)),
                    ("rows", Value::from(0)),
                ]),
                "no tables, no data",
            )?;
            Ok(0)
        }
        // Everything else names a relation that cannot exist yet. Saying so is
        // the truthful answer, and it is not a failure of the invocation.
        _ => {
            event(
                format,
                obj([
                    ("kind", Value::String("status".into())),
                    ("valid", Value::Bool(true)),
                    ("state", Value::String("EMPTY".into())),
                    ("tables", Value::from(0)),
                    ("rows", Value::from(0)),
                ]),
                "no tables, no data",
            )?;
            Ok(0)
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
            DoctorOptions {
                fix,
                allow_data,
                only: only.as_deref(),
                explain: explain.as_deref(),
                no_snapshot,
            },
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
            InferOptions {
                table: table.as_deref(),
                write,
                all,
                strictness: &strictness,
                pk: &pk,
                format,
            },
            cli,
        ),
        Command::Tables => {
            db.require_valid()?;
            let rows = db
                .catalog
                .schemas
                .keys()
                .map(|t| {
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
            if format == Format::Table {
                println!("{}", serde_json::to_string_pretty(s).unwrap());
            } else {
                output::records(&[serialized_record("schema", s)?], format)?;
            }
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
            event(
                format,
                obj([
                    ("kind", Value::String("recovery".into())),
                    ("changed", Value::Bool(changed)),
                ]),
                if changed {
                    "recovery complete"
                } else {
                    "no pending transactions"
                },
            )?;
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
            gc(db, cli.dry_run, format, cli.yes)
        }
        Command::UpgradeFormat => {
            event(
                format,
                obj([
                    ("kind", Value::String("format_status".into())),
                    ("format_version", Value::from(crate::FORMAT_VERSION)),
                    ("current", Value::Bool(true)),
                ]),
                &format!("format {} is current", crate::FORMAT_VERSION),
            )?;
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
    resource_overrides: &crate::config::ResourceOverrides,
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
            event(
                format,
                obj([
                    ("kind", Value::String("initialization_plan".into())),
                    ("path", Value::String(root.display().to_string())),
                    ("adopt", Value::Bool(false)),
                ]),
                &format!("would initialize {}", root.display()),
            )?;
            return Ok(0);
        }
        crate::db::init_empty(&root, track)?;
        event(
            format,
            obj([
                ("kind", Value::String("initialization".into())),
                ("path", Value::String(root.display().to_string())),
                ("revision", Value::from(1)),
            ]),
            &format!("initialized {} (revision 1)", root.display()),
        )?;
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
    let mut inference_config = crate::config::Config::default();
    inference_config.apply_overrides(resource_overrides);
    inference_config
        .validate()
        .map_err(|message| DbError::new("RESOURCE_LIMIT", message, 1))?;
    let ignore_set = inference_config
        .ignore_set()
        .map_err(|message| DbError::new("INTERNAL_METADATA_CORRUPT", message, 6))?;
    let tables = tables
        .into_iter()
        .filter(|table| !ignore_set.is_match(table))
        .collect::<Vec<_>>();
    let missing = missing
        .into_iter()
        .filter(|table| !ignore_set.is_match(table))
        .collect::<Vec<_>>();
    let reference_catalog = crate::catalog::Catalog::observe(&root, &inference_config)?;
    let schemas = infer::infer_all_with_references(
        &root,
        &missing,
        Strictness::Balanced,
        &inference_config,
        None,
        Some(&reference_catalog),
    )?;
    let preflight = adoption_preflight(&root, &schemas, &inference_config)?;
    let errors = crate::integrity::validate(&preflight);
    if !errors.is_empty() {
        output::diagnostics(&errors, format);
        return Ok(2);
    }
    if dry {
        if format == Format::Table {
            output::notice(&format!(
                "would adopt {} tables and infer {} schemas",
                tables.len(),
                schemas.len()
            ));
            for (name, reason) in &skipped {
                output::notice(&format!("skipped {name}: {reason}"));
            }
        } else {
            let mut records = vec![obj([
                ("kind", Value::String("adoption_plan".into())),
                ("tables", Value::from(tables.len())),
                ("inferred_schemas", Value::from(schemas.len())),
            ])];
            records.extend(skipped.iter().map(|(name, reason)| {
                obj([
                    ("kind", Value::String("skipped_directory".into())),
                    ("path", Value::String(name.clone())),
                    ("reason", Value::String(reason.clone())),
                ])
            }));
            output::records(&records, format)?;
        }
        return Ok(0);
    }
    crate::db::init_layout(&root, track)?;
    for s in schemas.values() {
        crate::db::write_schema(&root, s)?
    }
    let c = crate::catalog::Catalog::observe(&root, &inference_config)?;
    let (hash, entries) = metadata::state(&c)?;
    metadata::record(&c, None, hash.clone(), entries, "import", None)?;
    // Adoption builds the indexes for everything it adopted, so the database is
    // complete when this returns rather than repairing itself on first read.
    crate::index::rebuild(&root, &c)?;
    if matches!(format, Format::Table | Format::Sqlite) {
        output::notice(&format!(
            "Scanned {} directories, {} JSON files.\nVALID   revision 1   root {}",
            tables.len(),
            c.row_count(),
            &hash[..8]
        ));
        for (name, reason) in &skipped {
            output::notice(&format!("Skipped {name}: {reason}"));
        }
    } else {
        let mut records = vec![obj([
            ("kind", Value::String("initialization".into())),
            ("tables", Value::from(tables.len())),
            ("rows", Value::from(c.row_count())),
            ("revision", Value::from(1)),
            ("root", Value::String(hash)),
        ])];
        records.extend(skipped.iter().map(|(name, reason)| {
            obj([
                ("kind", Value::String("skipped_directory".into())),
                ("path", Value::String(name.clone())),
                ("reason", Value::String(reason.clone())),
            ])
        }));
        output::records(&records, format)?;
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
        if matches!(name, "schema" | ".db" | ".git") || name.starts_with('.') {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|error| DbError::io(&path, error))?;
        if !metadata.file_type().is_dir() {
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
    let metadata = fs::symlink_metadata(&dir).map_err(|error| DbError::io(&dir, error))?;
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
    for e in fs::read_dir(&dir).map_err(|e| DbError::io(&dir, e))? {
        let p = e.map_err(|e| DbError::io(&dir, e))?.path();
        let metadata = fs::symlink_metadata(&p).map_err(|error| DbError::io(&p, error))?;
        if !metadata.file_type().is_file() || has_multiple_links(&metadata) {
            return Err(DbError::from_diag(
                crate::diagnostic::Diagnostic::error(
                    "NON_REGULAR_FILE",
                    "schema entries must be private regular files",
                )
                .at(p.strip_prefix(root).unwrap_or(&p)),
                2,
            ));
        }
        if p.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".inferred.json"))
        {
            continue;
        }
        if p.extension().and_then(|x| x.to_str()) == Some("json")
            && let Some(s) = p.file_stem().and_then(|x| x.to_str())
        {
            out.insert(s.into());
        }
    }
    Ok(out)
}
fn adoption_preflight(
    root: &Path,
    inferred: &std::collections::BTreeMap<String, Schema>,
    config: &crate::config::Config,
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
    crate::catalog::Catalog::observe(shadow, config)
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
    options: InferOptions<'_>,
    resource_overrides: &crate::config::ResourceOverrides,
) -> Result<i32> {
    let InferOptions {
        table,
        write,
        all: _,
        strictness,
        pk,
        format,
    } = options;
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
    let mut inference_config = crate::config::Config::default();
    inference_config.apply_overrides(resource_overrides);
    inference_config
        .validate()
        .map_err(|message| DbError::new("RESOURCE_LIMIT", message, 1))?;
    let ignore_set = inference_config
        .ignore_set()
        .map_err(|message| DbError::new("INTERNAL_METADATA_CORRUPT", message, 6))?;
    let tables = tables
        .into_iter()
        .filter(|table| !ignore_set.is_match(table))
        .collect::<Vec<_>>();
    let reference_catalog = crate::catalog::Catalog::observe(root, &inference_config)?;
    let schemas = infer::infer_all_with_references(
        root,
        &tables,
        strict,
        &inference_config,
        if pk.is_empty() { None } else { Some(pk) },
        Some(&reference_catalog),
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
        output_schemas(schemas.values(), format)?;
    }
    Ok(0)
}
fn status(db: &Database, format: Format) -> Result<i32> {
    if matches!(format, Format::Json | Format::Jsonl) {
        let manifest = db.manifest.as_ref();
        let summary = obj([
            ("kind", Value::String("status".into())),
            ("valid", Value::Bool(db.diagnostics.is_empty())),
            (
                "state",
                Value::String(
                    if db.diagnostics.is_empty() {
                        if db.external_changes.is_empty() {
                            "VALID_UNCHANGED"
                        } else {
                            "VALID_CHANGED_EXTERNALLY"
                        }
                    } else {
                        "INVALID"
                    }
                    .into(),
                ),
            ),
            (
                "revision",
                Value::from(manifest.map_or(0, |manifest| manifest.revision)),
            ),
            (
                "root",
                Value::String(
                    manifest
                        .map_or("", |manifest| manifest.root_hash.as_str())
                        .into(),
                ),
            ),
            (
                "external_changes",
                Value::Array(
                    db.external_changes
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            ),
        ]);
        let diagnostics = if db.diagnostics.is_empty() {
            &db.catalog.warnings
        } else {
            &db.diagnostics
        };
        output::check_result(diagnostics, summary, format)?;
        return Ok(crate::diagnostic::exit_code_for_diagnostics(
            &db.diagnostics,
        ));
    }
    if !db.diagnostics.is_empty() {
        output::diagnostics(&db.diagnostics, format);
        eprintln!(
            "INVALID   revision {}   ({} external changes, {} violations)\nrun `db doctor` for fix options",
            db.manifest.as_ref().map_or(0, |m| m.revision),
            db.external_changes.len(),
            db.diagnostics.len()
        );
        return Ok(crate::diagnostic::exit_code_for_diagnostics(
            &db.diagnostics,
        ));
    }
    if !db.catalog.warnings.is_empty() {
        output::diagnostics(&db.catalog.warnings, format);
    }
    let m = db.manifest.as_ref();
    if format == Format::Table {
        output::notice(&format!(
            "VALID   revision {}   root {}   external changes: {}",
            m.map_or(0, |m| m.revision),
            m.map_or("unknown", |m| &m.root_hash[..8]),
            if db.external_changes.is_empty() {
                "none"
            } else {
                "accepted"
            }
        ));
        if !db.external_changes.is_empty() {
            output::notice("changed:");
            for p in &db.external_changes {
                output::notice(&format!("  {p}"));
            }
        }
        let findings = crate::lint::lint(&db.catalog, &db.config, false);
        if !findings.is_empty() {
            output::notice(&format!(
                "lint: {} findings (run `db lint`)",
                findings.len()
            ));
        }
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
            return Ok(crate::diagnostic::exit_code_for_diagnostics(
                &db.diagnostics,
            ));
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
        return Ok(crate::diagnostic::exit_code_for_diagnostics(
            &db.diagnostics,
        ));
    }
    if strict && (!lint.is_empty() || !db.catalog.warnings.is_empty()) {
        let mut findings = db.catalog.warnings.clone();
        findings.extend(lint);
        output::diagnostics(&findings, format);
        return Ok(7);
    }
    output::notice(&format!(
        "VALID: {} tables, {} rows, 0 violations, {} lint findings ({:?}), {} ms",
        db.catalog.schemas.len(),
        db.catalog.row_count(),
        lint.len(),
        lint_counts,
        db.validation_elapsed.as_millis(),
    ));
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

fn doctor(db: &mut Database, format: Format, options: DoctorOptions<'_>, cli: &Cli) -> Result<i32> {
    let DoctorOptions {
        fix,
        allow_data,
        only,
        explain,
        no_snapshot,
    } = options;
    let plan = crate::doctor::plan(db);
    if let Some(id) = explain {
        if let Some(f) = plan.iter().find(|f| f.id == id) {
            if format == Format::Table {
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
            } else {
                output::records(&[serialized_record("doctor_fix", f)?], format)?;
            }
            return Ok(0);
        }
        return Err(DbError::usage(format!(
            "unknown or inapplicable fix {id:?}"
        )));
    }
    let mut machine_records = Vec::new();
    if format == Format::Table {
        output::notice("Doctor plan:");
        for class in ["derived", "schema", "layout", "data", "manual"] {
            let items: Vec<_> = plan
                .iter()
                .filter(|f| f.class == class && doctor_fix_matches(only, &f.id))
                .collect();
            if !items.is_empty() {
                output::notice(&format!("  {class} ({}):", items.len()));
                for f in items {
                    output::notice(&format!("    {}  {}", f.id, f.description));
                    for path in &f.paths {
                        output::notice(&format!("      -> {}", path.display()));
                    }
                }
            }
        }
    } else {
        machine_records = plan
            .iter()
            .filter(|fix| doctor_fix_matches(only, &fix.id))
            .map(|fix| serialized_record("doctor_fix", fix))
            .collect::<Result<Vec<_>>>()?;
    }
    if !fix {
        if format != Format::Table {
            output::records(&machine_records, format)?;
        }
        return Ok(crate::diagnostic::exit_code_for_diagnostics(
            &db.diagnostics,
        ));
    }
    require_writable(cli)?;
    let changes = crate::doctor::repair_changes(db, only, allow_data)?;
    if changes.is_empty() {
        let no_change = obj([
            ("kind", Value::String("no_change".into())),
            (
                "message",
                Value::String("no applicable automatic fixes".into()),
            ),
        ]);
        if format == Format::Table {
            event(format, no_change, "no applicable automatic fixes")?;
        } else {
            machine_records.push(no_change);
            output::records(&machine_records, format)?;
        }
        return Ok(crate::diagnostic::exit_code_for_diagnostics(
            &db.diagnostics,
        ));
    }
    let touches_rows = changes.iter().any(|c| match c {
        Change::Write { path, .. } | Change::Delete { path } => !path.starts_with("schema"),
    });
    if cli.dry_run || touches_rows {
        let diffs = doctor_diff_records(&db.root, &changes)?;
        if format == Format::Table {
            print_doctor_diffs(&diffs);
        } else {
            machine_records.extend(diffs);
        }
    }
    if !cli.dry_run && !cli.yes {
        if format != Format::Table {
            output::records(&machine_records, format)?;
        }
        return Err(DbError::new(
            "CONFIRMATION_REQUIRED",
            if format == Format::Table {
                "doctor fixes require --yes"
            } else {
                "--yes is required to apply doctor fixes with machine-readable output"
            },
            9,
        ));
    }
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
        let snapshot = obj([
            ("kind", Value::String("snapshot".into())),
            ("name", Value::String(name.clone())),
            ("action", Value::String("created".into())),
        ]);
        if format == Format::Table {
            event(
                format,
                snapshot,
                &format!(
                    "created snapshot {name}; restore with `db snapshot restore {name} --yes`"
                ),
            )?;
        } else {
            machine_records.push(snapshot);
        }
    }
    let start = current_root(db)?;
    let paths = transaction::commit(
        &db.root,
        &db.config,
        &start,
        &changes,
        "repair",
        cli.dry_run,
        db.resource_overrides(),
    )?;
    let revision = resulting_revision(db, cli.dry_run)?;
    if format == Format::Table {
        print_mutation(&paths, revision, cli.dry_run, format)?;
    } else {
        machine_records.extend(mutation_records(&paths, revision, cli.dry_run));
        output::records(&machine_records, format)?;
    }
    Ok(0)
}

fn infer_cmd(db: &mut Database, options: InferOptions<'_>, cli: &Cli) -> Result<i32> {
    let InferOptions {
        table,
        write,
        all,
        strictness,
        pk,
        format,
    } = options;
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
    let ignore_set = db
        .config
        .ignore_set()
        .map_err(|message| DbError::new("INTERNAL_METADATA_CORRUPT", message, 6))?;
    let wanted: Vec<_> = tables
        .into_iter()
        .filter(|table| !ignore_set.is_match(table))
        .filter(|t| all || !db.catalog.schemas.contains_key(t))
        .collect();
    let schemas = infer::infer_all_with_references(
        &db.root,
        &wanted,
        strict,
        &db.config,
        if pk.is_empty() { None } else { Some(pk) },
        Some(&db.catalog),
    )?;
    if !write {
        output_schemas(schemas.values(), format)?;
        return Ok(0);
    }
    let mut changes = vec![];
    for (t, s) in schemas {
        let name = if all && db.catalog.schemas.contains_key(&t) {
            format!("schema/{t}.inferred.json")
        } else {
            format!("schema/{t}.json")
        };
        if !name.ends_with(".inferred.json") && db.root.join(&name).exists() {
            return Err(DbError::new(
                "SCHEMA_MISSING_REQUIRED",
                format!("refusing to overwrite {name}"),
                1,
            ));
        }
        let bytes = canonical::pretty_with_indent(
            &serde_json::to_value(s)
                .map_err(|error| DbError::new("INTERNAL_METADATA_CORRUPT", error.to_string(), 6))?,
            db.config.indentation_width,
        );
        if fs::read(db.root.join(&name)).ok().as_deref() != Some(bytes.as_slice()) {
            changes.push(Change::Write {
                path: name.into(),
                bytes,
            });
        }
    }
    if changes.is_empty() {
        return commit_changes(db, changes, "internal", format, cli);
    }
    let paths = transaction::commit(
        &db.root,
        &db.config,
        &current_root(db)?,
        &changes,
        "internal",
        cli.dry_run,
        db.resource_overrides(),
    )?;
    print_mutation(
        &paths,
        resulting_revision(db, cli.dry_run)?,
        cli.dry_run,
        format,
    )?;
    Ok(0)
}

fn get(db: &Database, table: &str, key: &str, format: Format) -> Result<i32> {
    db.require_valid()?;
    let s = schema_for(db, table)?;
    let values = key_values(key, s)?;
    let k = canonical::compact(&Value::Array(values));
    let row = crate::integrity::rows_by_key(&db.catalog, table)
        .get(&k)
        .copied()
        .ok_or_else(|| DbError::new("UNKNOWN_ROW", format!("no {table} row with key {key}"), 4))?;
    output::records(std::slice::from_ref(&row.value), format)?;
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
        query_limits_with_timeout(db, timeout),
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
    let v =
        crate::json::parse_str(&text).map_err(|e| DbError::usage(format!("invalid JSON: {e}")))?;
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
            bytes: canonical::pretty_with_indent(
                &canonical::canonical_row(&row, s),
                db.config.indentation_width,
            ),
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
    let p = crate::json::parse_str(patch)
        .map_err(|e| DbError::usage(format!("invalid patch JSON: {e}")))?;
    let p = p
        .as_object()
        .ok_or_else(|| DbError::usage("patch must be a JSON object"))?;
    if p.is_empty() {
        event(
            format,
            obj([
                ("kind", Value::String("no_change".into())),
                ("message", Value::String("patch is empty".into())),
            ]),
            "no change: patch is empty",
        )?;
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
    let key_params = key_values(key, s)?;
    let mut params = p.values().cloned().collect::<Vec<_>>();
    params.extend(key_params);
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
    let result =
        crate::sql::execute_with_limits(&db.catalog, &statement, &params, query_limits(db, cli))?;
    commit_changes(db, result.changes, "internal", format, cli)
}
fn delete(db: &Database, table: &str, key: &str, format: Format, cli: &Cli) -> Result<i32> {
    db.require_valid()?;
    let s = schema_for(db, table)?;
    let values = key_values(key, s)?;
    let where_sql = s
        .primary_key
        .iter()
        .map(|c| format!("{} = ?", quote(c)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!("DELETE FROM {} WHERE {where_sql}", quote(table));
    let r = crate::sql::execute_with_limits(&db.catalog, &sql, &values, query_limits(db, cli))?;
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
            crate::json::parse_str(text)
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
        query_limits(db, cli),
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
        query_limits(db, cli),
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
            query_limits(db, cli),
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
            crate::json::parse_str(text)
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
                query_limits_with_timeout(db, timeout),
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
                query_limits_with_timeout(db, timeout),
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
            if format == Format::Table {
                println!("{}", serde_json::to_string_pretty(s).unwrap());
            } else {
                output::records(&[serialized_record("schema", s)?], format)?;
            }
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
                    bytes: canonical::pretty_with_indent(
                        &serde_json::to_value(s).unwrap(),
                        db.config.indentation_width,
                    ),
                }],
                "internal",
                format,
                cli,
            )
        }
        SchemaCommand::Accept { table } => {
            let mut s = schema_for(db, &table)?.clone();
            if s.inferred.take().is_none() {
                event(
                    format,
                    obj([
                        ("kind", Value::String("no_change".into())),
                        ("table", Value::String(table.clone())),
                        (
                            "message",
                            Value::String("schema is already accepted".into()),
                        ),
                    ]),
                    &format!("no change: schema {table} is already accepted"),
                )?;
                return Ok(0);
            }
            commit_changes(
                db,
                vec![Change::Write {
                    path: format!("schema/{table}.json").into(),
                    bytes: canonical::pretty_with_indent(
                        &serde_json::to_value(s).unwrap(),
                        db.config.indentation_width,
                    ),
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
        event(
            format,
            obj([
                ("kind", Value::String("export".into())),
                ("table", Value::String(table.into())),
                ("path", Value::String(path.display().to_string())),
                ("format", Value::String("sqlite".into())),
            ]),
            &format!("exported database to {}", path.display()),
        )?;
        return Ok(0);
    }
    let schema = &db.catalog.schemas[table];
    let rows = db.catalog.rows[table]
        .iter()
        .map(|row| {
            canonical::canonical_row(&row.value, schema)
                .as_object()
                .cloned()
                .ok_or_else(|| {
                    DbError::new(
                        "INTERNAL_METADATA_CORRUPT",
                        "canonical row serialization did not produce an object",
                        6,
                    )
                })
        })
        .collect::<Result<Vec<_>>>()?;
    if let Some(path) = out {
        if fs::symlink_metadata(path).is_ok() {
            return Err(DbError::usage(format!(
                "refusing to overwrite {}",
                path.display()
            )));
        }
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
                    w.write_record(
                        heads
                            .iter()
                            .map(|header| output_cell(r.get(header).unwrap_or(&Value::Null))),
                    )
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
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| DbError::io(path, error))?;
        file.write_all(&bytes)
            .map_err(|error| DbError::io(path, error))?;
        file.sync_all().map_err(|error| DbError::io(path, error))?;
        event(
            format,
            obj([
                ("kind", Value::String("export".into())),
                ("table", Value::String(table.into())),
                ("path", Value::String(path.display().to_string())),
                ("rows", Value::from(rows.len())),
            ]),
            &format!("exported {} rows to {}", rows.len(), path.display()),
        )?
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
                    crate::json::parse_str(l)
                        .map_err(|e| DbError::new("INVALID_JSON", e.to_string(), 2))
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            match crate::json::parse_str(&text)
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
            bytes: canonical::pretty_with_indent(
                &canonical::canonical_row(&r, s),
                db.config.indentation_width,
            ),
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
    let mut key_changes = std::collections::BTreeMap::<String, (String, Value, Value)>::new();
    let mut key_change_targets = std::collections::BTreeSet::new();
    let removed_paths = changes
        .iter()
        .filter_map(|change| change.strip_prefix("D "))
        .collect::<Vec<_>>();
    let added_paths = changes
        .iter()
        .filter_map(|change| change.strip_prefix("A "))
        .collect::<Vec<_>>();
    for old_path in removed_paths {
        if added.contains_key(&old[old_path].hash) {
            continue;
        }
        let Some(table) = authoritative_table(old_path) else {
            continue;
        };
        let Some(schema) = diff_schema(db, &new, table, working)? else {
            continue;
        };
        let old_value = load_object(db, &old[old_path].hash)?;
        let old_payload = without_fields(&old_value, &schema.primary_key);
        let candidates = added_paths
            .iter()
            .filter(|new_path| {
                authoritative_table(new_path) == Some(table)
                    && !removed.contains_key(&new[**new_path].hash)
                    && !key_change_targets.contains(**new_path)
            })
            .filter_map(|new_path| {
                let value = if working {
                    current_object(db, new_path)
                } else {
                    load_object(db, &new[*new_path].hash)
                };
                match value {
                    Ok(value) if without_fields(&value, &schema.primary_key) == old_payload => {
                        Some(Ok(((*new_path).to_string(), value)))
                    }
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                }
            })
            .collect::<Result<Vec<_>>>()?;
        if let [candidate] = candidates.as_slice() {
            let old_key = row_key_value(&old_value, &schema);
            let new_key = row_key_value(&candidate.1, &schema);
            key_change_targets.insert(candidate.0.clone());
            key_changes.insert(old_path.into(), (candidate.0.clone(), old_key, new_key));
        }
    }
    for x in changes {
        let path = x[2..].to_string();
        if schema_only && !path.starts_with("schema/") {
            continue;
        }
        if let Some(t) = table_filter
            && !path.starts_with(&format!("{t}/"))
            && !path.starts_with(&format!("schema/{t}."))
        {
            continue;
        }
        if x.starts_with("D ") {
            if let Some((to, old_key, new_key)) = key_changes.get(&path) {
                rows.push(obj([
                    ("kind", Value::String("key_change".into())),
                    ("from", Value::String(path)),
                    ("to", Value::String(to.clone())),
                    ("old", old_key.clone()),
                    ("new", new_key.clone()),
                ]));
                continue;
            }
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
            if removed.contains_key(&new[&path].hash) || key_change_targets.contains(&path) {
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
fn authoritative_table(path: &str) -> Option<&str> {
    let (table, _) = path.split_once('/')?;
    (!matches!(table, ".db" | "schema")).then_some(table)
}

fn diff_schema(
    db: &Database,
    entries: &std::collections::BTreeMap<String, metadata::ManifestEntry>,
    table: &str,
    working: bool,
) -> Result<Option<Schema>> {
    if working {
        return Ok(db.catalog.schemas.get(table).cloned());
    }
    let Some(entry) = entries.get(&format!("schema/{table}.json")) else {
        return Ok(None);
    };
    let value = load_object(db, &entry.hash)?;
    serde_json::from_value(value)
        .map(Some)
        .map_err(|error| DbError::new("INTERNAL_METADATA_CORRUPT", error.to_string(), 6))
}

fn without_fields(value: &Value, fields: &[String]) -> Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        for field in fields {
            object.remove(field);
        }
    }
    value
}

fn row_key_value(value: &Value, schema: &Schema) -> Value {
    Value::Array(
        schema
            .primary_key
            .iter()
            .map(|name| {
                value
                    .get(name)
                    .cloned()
                    .or_else(|| schema.columns[name].default.clone())
                    .unwrap_or(Value::Null)
            })
            .collect(),
    )
}
fn load_revision(db: &Database, revision: u64) -> Result<metadata::Provenance> {
    let p = db.root.join(format!(".db/provenance/{revision:020}.json"));
    if !p.exists() {
        return Err(DbError::usage(format!("unknown revision {revision}")));
    }
    crate::json::parse_as(&fs::read(&p).map_err(|e| DbError::io(&p, e))?)
        .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))
}
fn load_object(db: &Database, hash: &str) -> Result<Value> {
    let p = db.root.join(format!(".db/objects/{hash}.json"));
    crate::json::parse(&fs::read(&p).map_err(|e| DbError::io(&p, e))?)
        .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))
}
fn current_object(db: &Database, path: &str) -> Result<Value> {
    if path == ".db/config" {
        return crate::json::parse(
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
        let bytes = fs::read(&p).map_err(|e| DbError::io(&p, e))?;
        let revision: metadata::Provenance = crate::json::parse_as(&bytes)
            .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))?;
        rows.push(serialized_record("provenance", &revision)?)
    }
    output::records(&rows, format)?;
    Ok(0)
}
fn show(db: &Database, revision: u64, format: Format) -> Result<i32> {
    let p = db.root.join(format!(".db/provenance/{revision:020}.json"));
    if !p.exists() {
        return Err(DbError::usage(format!("unknown revision {revision}")));
    }
    let bytes = fs::read(&p).map_err(|e| DbError::io(&p, e))?;
    if format == Format::Table {
        print!(
            "{}",
            String::from_utf8(bytes).map_err(|error| {
                DbError::new("INTERNAL_METADATA_CORRUPT", error.to_string(), 6)
            })?
        );
    } else {
        let provenance: metadata::Provenance = crate::json::parse_as(&bytes)
            .map_err(|error| DbError::new("INTERNAL_METADATA_CORRUPT", error.to_string(), 6))?;
        output::records(&[serialized_record("provenance", &provenance)?], format)?;
    }
    Ok(0)
}

fn snapshot(db: &Database, cmd: SnapshotCommand, format: Format, cli: &Cli) -> Result<i32> {
    let base = db.root.join(".db/snapshots");
    match cmd {
        SnapshotCommand::Create { name } => {
            require_writable(cli)?;
            db.require_valid()?;
            valid_snapshot(&name)?;
            ensure_snapshot_base(&base, true)?;
            let dest = base.join(&name);
            if dest.exists() {
                return Err(DbError::usage("snapshot already exists"));
            }
            if cli.dry_run {
                event(
                    format,
                    obj([
                        ("kind", Value::String("snapshot".into())),
                        ("name", Value::String(name.clone())),
                        ("action", Value::String("create_planned".into())),
                    ]),
                    &format!("would create snapshot {name}"),
                )?;
                return Ok(0);
            }
            create_snapshot(db, &name)?;
            event(
                format,
                obj([
                    ("kind", Value::String("snapshot".into())),
                    ("name", Value::String(name.clone())),
                    ("action", Value::String("created".into())),
                ]),
                &format!("created snapshot {name}"),
            )?;
            Ok(0)
        }
        SnapshotCommand::List => {
            let mut rows = vec![];
            if !ensure_snapshot_base(&base, false)? {
                output::records(&rows, format)?;
                return Ok(0);
            }
            for e in fs::read_dir(&base).map_err(|e| DbError::io(&base, e))? {
                let e = e.map_err(|e| DbError::io(&base, e))?;
                let path = e.path();
                let metadata =
                    fs::symlink_metadata(&path).map_err(|error| DbError::io(&path, error))?;
                if !metadata.file_type().is_dir() {
                    return Err(DbError::new(
                        "INTERNAL_METADATA_CORRUPT",
                        format!("snapshot entry {} is not a real directory", path.display()),
                        6,
                    ));
                }
                rows.push(obj([
                    ("kind", Value::String("snapshot".into())),
                    (
                        "name",
                        Value::String(e.file_name().to_string_lossy().into()),
                    ),
                ]))
            }
            output::records(&rows, format)?;
            Ok(0)
        }
        SnapshotCommand::Restore { name } => {
            require_writable(cli)?;
            valid_snapshot(&name)?;
            if !cli.dry_run && !cli.yes {
                return Err(DbError::new(
                    "CONFIRMATION_REQUIRED",
                    "snapshot restore requires --yes",
                    9,
                ));
            }
            let src = base.join(&name);
            if !ensure_snapshot_base(&base, false)? || !snapshot_exists(&src)? {
                return Err(DbError::usage("snapshot does not exist"));
            }
            let changes = changes_from_snapshot(db, &src)?;
            commit_changes(db, changes, "snapshot_restore", format, cli)
        }
        SnapshotCommand::Delete { name } => {
            require_writable(cli)?;
            valid_snapshot(&name)?;
            if !cli.dry_run && !cli.yes {
                return Err(DbError::new(
                    "CONFIRMATION_REQUIRED",
                    "snapshot delete requires --yes",
                    9,
                ));
            }
            let p = base.join(name);
            let exists = ensure_snapshot_base(&base, false)? && snapshot_exists(&p)?;
            if !cli.dry_run && exists {
                fs::remove_dir_all(&p).map_err(|e| DbError::io(&p, e))?
            }
            let display_name = p
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| DbError::new("PATH_VIOLATION", "invalid snapshot path", 1))?;
            event(
                format,
                obj([
                    ("kind", Value::String("snapshot".into())),
                    ("name", Value::String(display_name.into())),
                    (
                        "action",
                        Value::String(
                            if cli.dry_run {
                                "delete_planned"
                            } else {
                                "deleted"
                            }
                            .into(),
                        ),
                    ),
                ]),
                &format!(
                    "{} snapshot {}",
                    if cli.dry_run {
                        "would delete"
                    } else {
                        "deleted"
                    },
                    display_name
                ),
            )?;
            Ok(0)
        }
    }
}
fn reindex(db: &Database, format: Format) -> Result<i32> {
    db.require_valid()?;
    crate::index::rebuild(&db.root, &db.catalog)?;
    event(
        format,
        obj([
            ("kind", Value::String("maintenance".into())),
            ("operation", Value::String("reindex".into())),
        ]),
        "rebuilt indexes",
    )?;
    Ok(0)
}
fn analyze(db: &Database, format: Format) -> Result<i32> {
    db.require_valid()?;
    let stats: std::collections::BTreeMap<_, _> = db
        .catalog
        .rows
        .iter()
        .map(|(t, r)| (t.clone(), obj([("rows", Value::from(r.len()))])))
        .collect();
    let statistics = db.root.join(".db/statistics");
    ensure_rebuildable_directory(&statistics)?;
    metadata::write_json_atomic(&statistics.join("catalog.json"), &stats)?;
    event(
        format,
        obj([
            ("kind", Value::String("maintenance".into())),
            ("operation", Value::String("analyze".into())),
        ]),
        "rebuilt statistics",
    )?;
    Ok(0)
}
fn ensure_rebuildable_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(file_metadata) if file_metadata.file_type().is_dir() => Ok(()),
        Ok(_) => {
            fs::remove_file(path).map_err(|error| DbError::io(path, error))?;
            fs::create_dir(path).map_err(|error| DbError::io(path, error))?;
            metadata::sync_parent(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|error| DbError::io(path, error))?;
            metadata::sync_parent(path)
        }
        Err(error) => Err(DbError::io(path, error)),
    }
}
fn gc(db: &Database, dry: bool, format: Format, yes: bool) -> Result<i32> {
    db.require_valid()?;
    let mut retained = std::collections::BTreeSet::new();
    if let Some(manifest) = &db.manifest {
        retained.extend(manifest.entries.values().map(|entry| entry.hash.clone()));
    }
    let provenance = db.root.join(".db/provenance");
    for entry in fs::read_dir(&provenance).map_err(|error| DbError::io(&provenance, error))? {
        let path = entry
            .map_err(|error| DbError::io(&provenance, error))?
            .path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let bytes = fs::read(&path).map_err(|error| DbError::io(&path, error))?;
        let revision: metadata::Provenance = crate::json::parse_as(&bytes).map_err(|error| {
            DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                format!("invalid provenance {}: {error}", path.display()),
                6,
            )
        })?;
        retained.extend(revision.entries.values().map(|entry| entry.hash.clone()));
    }

    let mut targets = Vec::<(PathBuf, u64, &'static str)>::new();
    let objects = db.root.join(".db/objects");
    if objects.exists() {
        for entry in fs::read_dir(&objects).map_err(|error| DbError::io(&objects, error))? {
            let path = entry.map_err(|error| DbError::io(&objects, error))?.path();
            let metadata =
                fs::symlink_metadata(&path).map_err(|error| DbError::io(&path, error))?;
            let hash = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".json"));
            if hash.is_none_or(|hash| !retained.contains(hash)) {
                targets.push((path, metadata.len(), "object"));
            }
        }
    }
    let transactions = db.root.join(".db/transactions");
    for entry in fs::read_dir(&transactions).map_err(|error| DbError::io(&transactions, error))? {
        let path = entry
            .map_err(|error| DbError::io(&transactions, error))?
            .path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| DbError::io(&path, error))?;
        if metadata.file_type().is_dir()
            && (!path.join("COMMITTING").exists() || path.join("COMPLETE").exists())
        {
            targets.push((path, 0, "transaction"));
        }
    }
    targets.sort_by(|left, right| left.0.cmp(&right.0));
    let mut records = targets
        .iter()
        .map(|(path, bytes, target_kind)| {
            obj([
                ("kind", Value::String("gc_candidate".into())),
                ("target_kind", Value::String((*target_kind).into())),
                (
                    "path",
                    Value::String(
                        path.strip_prefix(&db.root)
                            .unwrap_or(path)
                            .display()
                            .to_string(),
                    ),
                ),
                ("bytes", Value::from(*bytes)),
            ])
        })
        .collect::<Vec<_>>();
    let reclaimable_bytes = targets.iter().map(|target| target.1).sum::<u64>();
    if format == Format::Table {
        for record in &records {
            output::notice(&format!(
                "{} {} ({} bytes)",
                if dry { "would reclaim" } else { "reclaim" },
                record["path"].as_str().unwrap_or(""),
                record["bytes"]
            ));
        }
        output::notice(&format!(
            "{} item(s), {} byte(s) reclaimable",
            targets.len(),
            reclaimable_bytes
        ));
    } else {
        records.push(obj([
            ("kind", Value::String("gc_summary".into())),
            ("items", Value::from(targets.len())),
            ("bytes", Value::from(reclaimable_bytes)),
            ("dry_run", Value::Bool(dry)),
        ]));
        output::records(&records, format)?;
    }
    if !dry && !targets.is_empty() && !yes {
        return Err(DbError::new(
            "CONFIRMATION_REQUIRED",
            "garbage collection requires --yes after reviewing the reclaim plan",
            9,
        ));
    }
    if !dry {
        for (path, _, _) in &targets {
            let metadata = fs::symlink_metadata(path).map_err(|error| DbError::io(path, error))?;
            if metadata.file_type().is_dir() {
                fs::remove_dir_all(path).map_err(|error| DbError::io(path, error))?;
            } else {
                fs::remove_file(path).map_err(|error| DbError::io(path, error))?;
            }
        }
    }
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
    // The shell opens against whatever model the folder provides, including an
    // empty one. Refusing entry because the database is not yet valid would put
    // the ceremony back: each statement enforces its own requirements when it
    // runs, and a diagnostic statement is exactly what the user needs here.
    if !db.diagnostics.is_empty() {
        output::diagnostics(&db.diagnostics, format);
    }
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
    // Input history persists per database, so recall survives the session:
    // `.db/.gitignore` already excludes everything but `format` and `config`,
    // which keeps query text -- literals included -- out of the repository.
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
                // Leaving the shell is not a query worth recalling: recording
                // it would put `.quit` at the top of every later session's
                // history, one keystroke from ending that session too.
                if !matches!(query, ".quit" | ".exit") {
                    editor
                        .add_history_entry(query)
                        .map_err(|error| DbError::new("IO_ERROR", error.to_string(), 6))?;
                }
                if !shell_line(db, query, format, cli)? {
                    break;
                }
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            Err(error) => return Err(DbError::new("IO_ERROR", error.to_string(), 6)),
        }
    }
    if !cli.readonly {
        // rustyline rewrites the history file in place and does not create it,
        // so the first session on a database must put it there. Without this
        // the initial save fails with ENOENT and nothing is ever persisted.
        if !history.exists() {
            fs::write(&history, "")
                .map_err(|error| DbError::new("IO_ERROR", error.to_string(), 6))?;
        }
        // A session that answered every question has succeeded. Failing it here
        // would discard that work over a convenience file, so the inability to
        // record history is reported and the exit stays clean.
        if let Err(error) = editor.save_history(&history) {
            crate::output::notice_stderr(&format!(
                "warning: could not save shell history to {}: {error}",
                history.display()
            ));
        }
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
                    ObserveMode::READ_ONLY
                } else {
                    ObserveMode::RECORD
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
                    bytes: canonical::pretty_with_indent(
                        &serde_json::to_value(&mut s).unwrap(),
                        db.config.indentation_width,
                    ),
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
                        bytes: canonical::pretty_with_indent(&value, db.config.indentation_width),
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
                    crate::json::parse_str(&x)
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
                        bytes: canonical::pretty_with_indent(&value, db.config.indentation_width),
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
            let target = parse_type(&kind)?;
            let changes = declarative_migration_changes(
                db,
                MigrationDocument {
                    operations: vec![MigrationOperation::ChangeType {
                        table,
                        column,
                        kind: target,
                        using,
                    }],
                },
            )?;
            commit_changes(db, changes, "migration", format, cli)
        }
        MigrateCommand::AddConstraint { table, definition } => {
            let mut s = schema_for(db, &table)?.clone();
            let def: ConstraintDefinition = serde_json::from_value(
                crate::json::parse_str(&definition)
                    .map_err(|e| DbError::usage(format!("invalid constraint definition: {e}")))?,
            )
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
                event(
                    format,
                    obj([
                        ("kind", Value::String("no_change".into())),
                        ("message", Value::String("index already exists".into())),
                    ]),
                    "no change: index already exists",
                )?;
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
            let bytes = fs::read(&file).map_err(|e| DbError::io(&file, e))?;
            let document: MigrationDocument = serde_json::from_value(
                crate::json::parse(&bytes)
                    .map_err(|e| DbError::usage(format!("invalid migration JSON: {e}")))?,
            )
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
            bytes: canonical::pretty_with_indent(&value, db.config.indentation_width),
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
                schemas.insert(table.clone(), *schema);
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
                let old_schema = schemas
                    .get(&table)
                    .ok_or_else(|| {
                        DbError::new("UNKNOWN_TABLE", format!("unknown table {table}"), 4)
                    })?
                    .clone();
                let mut target_column =
                    old_schema.columns.get(&column).cloned().ok_or_else(|| {
                        DbError::new("UNKNOWN_COLUMN", format!("unknown {table}.{column}"), 4)
                    })?;
                retarget_column(&mut target_column, kind.clone())?;
                if let Some(expr) = using {
                    let cat = virtual_catalog(&schemas, &rows)?;
                    let s = old_schema.clone();
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
                        let converted = convert_query_value(value, &kind)?;
                        if !crate::value::matches_column(&converted, &target_column) {
                            return Err(DbError::new(
                                "TYPE_MISMATCH",
                                format!(
                                    "conversion expression produced a value incompatible with {table}.{column}"
                                ),
                                2,
                            ));
                        }
                        row.insert(column.clone(), converted);
                    }
                } else {
                    let mut offenders = Vec::new();
                    let table_rows = rows.get_mut(&table).ok_or_else(|| {
                        DbError::new("UNKNOWN_TABLE", format!("unknown table {table}"), 4)
                    })?;
                    for (index, row) in table_rows.iter_mut().enumerate() {
                        if let Some(value) = row.get(&column) {
                            if let Some(converted) =
                                crate::value::lossless_convert(value, &target_column)
                            {
                                row.insert(column.clone(), converted);
                            } else {
                                let path = canonical::filename(&old_schema, row)
                                    .map(|name| {
                                        PathBuf::from(&table).join(name).display().to_string()
                                    })
                                    .unwrap_or_else(|| format!("{table}/<row {}>", index + 1));
                                offenders.push(path);
                            }
                        }
                    }
                    if !offenders.is_empty() {
                        return Err(DbError::new(
                            "TYPE_MISMATCH",
                            format!(
                                "change-type cannot losslessly convert {table}.{column} in: {}; supply using with an explicit conversion expression",
                                offenders.join(", ")
                            ),
                            2,
                        ));
                    }
                }
                *schemas
                    .get_mut(&table)
                    .and_then(|s| s.columns.get_mut(&column))
                    .ok_or_else(|| {
                        DbError::new("UNKNOWN_COLUMN", format!("unknown {table}.{column}"), 4)
                    })? = target_column;
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
                bytes: canonical::pretty_with_indent(&value, db.config.indentation_width),
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
                bytes: canonical::pretty_with_indent(
                    &canonical::canonical_row(&row, &schemas[&t]),
                    db.config.indentation_width,
                ),
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
        bytes: canonical::pretty_with_indent(
            &serde_json::to_value(s)
                .map_err(|e| DbError::new("INTERNAL_METADATA_CORRUPT", e.to_string(), 6))?,
            db.config.indentation_width,
        ),
    }];
    for row in &db.catalog.rows[&table] {
        let mut r = row.value.clone();
        edit(&mut r)?;
        let new = PathBuf::from(&table).join(canonical::filename(s, &r).ok_or_else(|| {
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
            bytes: canonical::pretty_with_indent(
                &canonical::canonical_row(&r, s),
                db.config.indentation_width,
            ),
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
        event(
            format,
            obj([
                ("kind", Value::String("no_change".into())),
                ("message", Value::String("no change".into())),
            ]),
            "no change",
        )?;
        return Ok(0);
    }
    let paths = transaction::commit(
        &db.root,
        &db.config,
        &current_root(db)?,
        &changes,
        origin,
        cli.dry_run,
        db.resource_overrides(),
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
        let schema_files = paths
            .iter()
            .filter(|path| path.starts_with("schema"))
            .count();
        output::notice(&format!(
            "migration plan: {row_files} row file(s), {schema_files} schema file(s)"
        ));
    }
    print_mutation(
        &paths,
        resulting_revision(db, cli.dry_run)?,
        cli.dry_run,
        format,
    )?;
    Ok(0)
}
fn resulting_revision(db: &Database, dry_run: bool) -> Result<u64> {
    if dry_run {
        return Ok(db
            .manifest
            .as_ref()
            .map_or(1, |manifest| manifest.revision + 1));
    }
    Ok(metadata::load_manifest(&db.root)?.map_or(0, |manifest| manifest.revision))
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
    let rows = mutation_records(paths, revision, dry);
    if format == Format::Table {
        let suffix = if dry {
            String::new()
        } else {
            format!("; revision {revision}")
        };
        output::notice(&format!(
            "{} {} path(s){}",
            if dry { "would change" } else { "changed" },
            paths.len(),
            suffix
        ));
        for p in paths {
            output::notice(&format!("  {}", p.display()))
        }
    } else {
        output::records(&rows, format)?
    }
    Ok(())
}
fn mutation_records(paths: &[PathBuf], revision: u64, dry: bool) -> Vec<Map<String, Value>> {
    paths
        .iter()
        .map(|path| {
            obj([
                (
                    "kind",
                    Value::String(if dry { "planned_change" } else { "change" }.into()),
                ),
                ("path", Value::String(path.display().to_string())),
                ("revision", Value::from(revision)),
            ])
        })
        .collect()
}

fn doctor_diff_records(root: &Path, changes: &[Change]) -> Result<Vec<Map<String, Value>>> {
    changes
        .iter()
        .map(|change| {
            let (path, after) = match change {
                Change::Write { path, bytes } => (path, Some(bytes.as_slice())),
                Change::Delete { path } => (path, None),
            };
            let before = match fs::read(root.join(path)) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(DbError::io(&root.join(path), error)),
            };
            Ok(obj([
                ("kind", Value::String("doctor_diff".into())),
                ("path", Value::String(path.display().to_string())),
                (
                    "before",
                    before.as_deref().map(bytes_value).unwrap_or(Value::Null),
                ),
                ("after", after.map(bytes_value).unwrap_or(Value::Null)),
            ]))
        })
        .collect()
}

fn bytes_value(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(text) => Value::String(text.into()),
        Err(_) => Value::Array(bytes.iter().copied().map(Value::from).collect()),
    }
}

fn print_doctor_diffs(records: &[Map<String, Value>]) {
    for record in records {
        let path = record["path"].as_str().unwrap_or("<unknown>");
        println!("--- {path}");
        println!("+++ {path}");
        print_prefixed_content('-', &record["before"]);
        print_prefixed_content('+', &record["after"]);
    }
}

fn print_prefixed_content(prefix: char, value: &Value) {
    match value {
        Value::Null => println!("{prefix}<absent>"),
        Value::String(text) => {
            for line in text.lines() {
                println!("{prefix}{line}");
            }
        }
        Value::Array(bytes) => println!("{prefix}<binary bytes: {}>", bytes.len()),
        _ => unreachable!("doctor diff content has a fixed shape"),
    }
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
    let vals = key_values(key, s)?;
    let k = canonical::compact(&Value::Array(vals));
    crate::integrity::rows_by_key(&db.catalog, table)
        .get(&k)
        .copied()
        .ok_or_else(|| DbError::new("UNKNOWN_ROW", format!("no row with key {key}"), 4))
}
fn key_values(text: &str, schema: &Schema) -> Result<Vec<Value>> {
    if schema.primary_key.len() == 1 {
        let column = &schema.columns[&schema.primary_key[0]];
        let parsed = crate::json::parse_str(text).unwrap_or_else(|_| Value::String(text.into()));
        if crate::value::matches_column(&parsed, column) {
            return Ok(vec![parsed]);
        }
        let textual = Value::String(text.into());
        if crate::value::matches_column(&textual, column) {
            return Ok(vec![textual]);
        }
        return Err(DbError::new(
            "TYPE_MISMATCH",
            format!("primary key does not match type {:?}", column.kind),
            4,
        ));
    }
    let v = crate::json::parse_str(text)
        .map_err(|_| DbError::usage("composite primary keys must be a JSON array"))?;
    let a = v
        .as_array()
        .cloned()
        .ok_or_else(|| DbError::usage("composite primary keys must be a JSON array"))?;
    if a.len() != schema.primary_key.len() {
        return Err(DbError::usage(format!(
            "primary key requires {} values",
            schema.primary_key.len()
        )));
    }
    for (value, name) in a.iter().zip(&schema.primary_key) {
        if !crate::value::matches_column(value, &schema.columns[name]) {
            return Err(DbError::new(
                "TYPE_MISMATCH",
                format!("primary-key component {name:?} does not match its declared type"),
                4,
            ));
        }
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
        ColumnType::Array | ColumnType::Object | ColumnType::Json => crate::json::parse_str(v)
            .map_err(|e| DbError::new("TYPE_MISMATCH", e.to_string(), 2))?,
        _ => Value::String(v.into()),
    };
    Ok(parsed)
}
fn parse_type(s: &str) -> Result<ColumnType> {
    serde_json::from_str(&format!("\"{s}\""))
        .map_err(|_| DbError::new("SCHEMA_TYPE_UNKNOWN", format!("unknown type {s:?}"), 2))
}

fn retarget_column(column: &mut Column, target: ColumnType) -> Result<()> {
    let previous = column.kind.clone();
    if previous != target {
        if target == ColumnType::Enum && previous != ColumnType::Enum {
            return Err(DbError::new(
                "SCHEMA_MISSING_REQUIRED",
                "change-type to enum requires enum values, which this operation cannot infer",
                2,
            ));
        }
        if target == ColumnType::Array && previous != ColumnType::Array {
            return Err(DbError::new(
                "SCHEMA_MISSING_REQUIRED",
                "change-type to array requires an items schema, which this operation cannot infer",
                2,
            ));
        }
    }

    column.kind = target;
    if column.kind != ColumnType::Enum {
        column.values = None;
    }
    if column.kind != ColumnType::Array {
        column.items = None;
    }
    if column.kind != ColumnType::Object {
        column.properties = None;
    }

    if let Some(generated) = &column.generated {
        let compatible = matches!(
            (&generated.kind, &column.kind),
            (GeneratedKind::Uuid, ColumnType::Uuid)
                | (GeneratedKind::Ulid, ColumnType::Ulid)
                | (GeneratedKind::Now, ColumnType::Timestamp)
                | (GeneratedKind::Sequence, ColumnType::Int)
        );
        if !compatible {
            return Err(DbError::new(
                "SCHEMA_DEFAULT_TYPE_MISMATCH",
                "change-type is incompatible with the column's generated value",
                2,
            ));
        }
    }
    if let Some(default) = column.default.clone() {
        column.default = Some(crate::value::lossless_convert(&default, column).ok_or_else(
            || {
                DbError::new(
                    "SCHEMA_DEFAULT_TYPE_MISMATCH",
                    "column default cannot be converted losslessly to the target type",
                    2,
                )
            },
        )?);
    }
    Ok(())
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
fn event(format: Format, record: Map<String, Value>, human: &str) -> Result<()> {
    if matches!(format, Format::Table | Format::Sqlite) {
        // Informational confirmation: suppressed by --quiet (Section 49).
        // Machine-readable output is a contract and is never suppressed.
        output::notice(human);
        Ok(())
    } else {
        output::records(&[record], format)
    }
}

fn query_limits(db: &Database, cli: &Cli) -> crate::sql::QueryLimits {
    query_limits_with_timeout(
        db,
        cli.timeout
            .or(db.config.timeout_seconds)
            .map(std::time::Duration::from_secs),
    )
}

fn query_limits_with_timeout(
    db: &Database,
    timeout: Option<std::time::Duration>,
) -> crate::sql::QueryLimits {
    crate::sql::QueryLimits {
        timeout,
        max_rows: db.config.max_result_rows,
        max_memory: db.config.max_query_memory,
        max_sort_memory: db.config.max_sort_memory,
        max_temporary_disk: db.config.max_temporary_disk,
    }
}

fn doctor_fix_matches(only: Option<&str>, fix: &str) -> bool {
    let Some(only) = only else {
        return true;
    };
    only == fix
        || matches!(
            (only, fix),
            ("IDENTITY_MISMATCH", "FIX_RENAME_TO_IDENTITY")
                | (
                    "ROW_UNKNOWN_FIELD",
                    "FIX_DROP_UNKNOWN_FIELD" | "FIX_RENAME_FIELD"
                )
                | ("TYPE_MISMATCH", "FIX_COERCE_VALUE")
                | (
                    "FOREIGN_KEY_VIOLATION",
                    "FIX_ORPHAN_SET_NULL" | "FIX_ORPHAN_DELETE_ROW"
                )
                | ("LINT_SCHEMA_UNREVIEWED", "FIX_ACCEPT_INFERRED")
                | ("LINT_NULLABLE_NEVER_NULL", "FIX_TIGHTEN_NULLABLE")
                | ("LINT_WIDER_TYPE", "FIX_NARROW_TYPE")
                | ("LINT_ENUM_CANDIDATE", "FIX_ADD_ENUM")
                | ("LINT_UNIQUE_CANDIDATE", "FIX_ADD_UNIQUE")
                | ("LINT_FK_CANDIDATE", "FIX_ADD_FK")
                | ("LINT_CHECK_CANDIDATE", "FIX_ADD_CHECK")
                | ("LINT_FK_NO_INDEX", "FIX_ADD_INDEX")
                | ("LINT_NON_CANONICAL_FORMATTING", "FIX_CANONICALIZE")
        )
}

fn serialized_record<T: serde::Serialize>(kind: &str, value: &T) -> Result<Map<String, Value>> {
    let mut object = serde_json::to_value(value)
        .map_err(|error| DbError::new("INTERNAL_METADATA_CORRUPT", error.to_string(), 6))?
        .as_object()
        .cloned()
        .ok_or_else(|| {
            DbError::new(
                "INTERNAL_METADATA_CORRUPT",
                "serialized command record is not an object",
                6,
            )
        })?;
    object.insert("kind".into(), Value::String(kind.into()));
    Ok(object)
}

fn output_schemas<'a>(schemas: impl Iterator<Item = &'a Schema>, format: Format) -> Result<()> {
    if format == Format::Table {
        for schema in schemas {
            println!("{}", serde_json::to_string_pretty(schema).unwrap());
        }
        return Ok(());
    }
    let records = schemas
        .map(|schema| serialized_record("schema", schema))
        .collect::<Result<Vec<_>>>()?;
    output::records(&records, format)
}

fn obj<const N: usize>(items: [(&str, Value); N]) -> Map<String, Value> {
    items.into_iter().map(|(k, v)| (k.into(), v)).collect()
}
fn valid_snapshot(s: &str) -> Result<()> {
    let lower = s.to_ascii_lowercase();
    let base = lower.split('.').next().unwrap_or(&lower);
    let windows_reserved = matches!(base, "con" | "prn" | "aux" | "nul")
        || base
            .strip_prefix("com")
            .or_else(|| base.strip_prefix("lpt"))
            .is_some_and(|number| number.len() == 1 && matches!(number.as_bytes()[0], b'1'..=b'9'));
    if s.is_empty()
        || s.contains(['/', '\\', '\0'])
        || s == "."
        || s == ".."
        || s.ends_with(['.', ' '])
        || windows_reserved
    {
        return Err(DbError::new("PATH_VIOLATION", "invalid snapshot name", 2));
    }
    Ok(())
}

fn ensure_snapshot_base(base: &Path, create: bool) -> Result<bool> {
    match fs::symlink_metadata(base) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("snapshot path {} is not a real directory", base.display()),
            6,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            fs::create_dir(base).map_err(|error| DbError::io(base, error))?;
            metadata::sync_parent(base)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(DbError::io(base, error)),
    }
}

fn snapshot_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(DbError::new(
            "INTERNAL_METADATA_CORRUPT",
            format!("snapshot {} is not a real directory", path.display()),
            6,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(DbError::io(path, error)),
    }
}

fn reject_snapshot_collision(base: &Path, candidate: &str) -> Result<()> {
    use unicode_normalization::UnicodeNormalization;
    let normalized: String = candidate.nfc().flat_map(char::to_lowercase).collect();
    for entry in fs::read_dir(base).map_err(|error| DbError::io(base, error))? {
        let entry = entry.map_err(|error| DbError::io(base, error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let existing: String = name.nfc().flat_map(char::to_lowercase).collect();
        if existing == normalized {
            return Err(DbError::new(
                "PATH_COLLISION",
                format!("snapshot name {candidate:?} collides with existing {name:?}"),
                2,
            ));
        }
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
    valid_snapshot(name)?;
    ensure_snapshot_base(&base, true)?;
    reject_snapshot_collision(&base, name)?;
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
    let mut snapshot_paths = std::collections::BTreeSet::new();
    snapshot_paths.extend(
        snap.schemas
            .keys()
            .map(|table| PathBuf::from(format!("schema/{table}.json"))),
    );
    snapshot_paths.extend(snap.rows.values().flatten().map(|row| row.relative.clone()));
    let mut current_paths = std::collections::BTreeSet::new();
    let schema_dir = db.root.join("schema");
    for entry in fs::read_dir(&schema_dir).map_err(|error| DbError::io(&schema_dir, error))? {
        let path = entry
            .map_err(|error| DbError::io(&schema_dir, error))?
            .path();
        let relative = path
            .strip_prefix(&db.root)
            .map_err(|_| DbError::new("PATH_VIOLATION", "schema path escaped database", 6))?
            .to_path_buf();
        current_paths.insert(relative);
    }
    let table_names = db
        .catalog
        .schemas
        .keys()
        .chain(snap.schemas.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    for table in table_names {
        let directory = db.root.join(&table);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                for entry in
                    fs::read_dir(&directory).map_err(|error| DbError::io(&directory, error))?
                {
                    let path = entry
                        .map_err(|error| DbError::io(&directory, error))?
                        .path();
                    current_paths.insert(
                        path.strip_prefix(&db.root)
                            .map_err(|_| {
                                DbError::new("PATH_VIOLATION", "row path escaped database", 6)
                            })?
                            .to_path_buf(),
                    );
                }
            }
            Ok(_) => {
                current_paths.insert(PathBuf::from(&table));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(DbError::io(&directory, error)),
        }
    }
    for path in current_paths {
        if !snapshot_paths.contains(&path) {
            changes.push(Change::Delete { path });
        }
    }
    for t in snap.schemas.keys() {
        let p = PathBuf::from(format!("schema/{t}.json"));
        let bytes = fs::read(src.join(&p)).map_err(|e| DbError::io(&src.join(&p), e))?;
        if fs::read(db.root.join(&p)).ok().as_deref() != Some(bytes.as_slice()) {
            changes.push(Change::Write {
                path: p.clone(),
                bytes,
            });
        }
        for r in &snap.rows[t] {
            if fs::read(db.root.join(&r.relative)).ok().as_deref() != Some(r.raw.as_slice()) {
                changes.push(Change::Write {
                    path: r.relative.clone(),
                    bytes: r.raw.clone(),
                })
            }
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
