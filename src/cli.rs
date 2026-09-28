//! The command line: arguments in, a command run, its result rendered.
//!
//! Parsing lives here and nowhere else. Each subcommand becomes one call into
//! [`crate::command`], whose output goes through a [`Terminal`] sink in the
//! chosen format.

use crate::{
    command::{
        self, Context, health::DoctorOptions, history, rows::ListOptions, schema::OnConflict,
    },
    config::ResourceOverrides,
    diagnostic::{DbError, Result},
    migrate::{Constraint, Migration, Operation},
    output::{self, Finish, Format, Sink, Terminal},
};
use clap::{Args, CommandFactory, Parser, Subcommand};
use std::{io::IsTerminal, path::PathBuf};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "reldir",
    version,
    about = "A relational database whose tables are directories of JSON files",
    long_about = "reldir governs a folder of JSON files as a relational database: every file is a row, \
                  every schema is a JSON Schema document, and no command -- SQL, row edits, imports, \
                  migrations, repairs -- can leave the files invalid.\n\n\
                  With no subcommand, `reldir 'SELECT ...'` runs one statement and bare `reldir` opens a shell.",
    subcommand_negates_reqs = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,
    /// A SQL statement to run when no subcommand is given; `-` reads it from stdin.
    #[arg(value_name = "SQL")]
    pub sql: Option<String>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct Global {
    /// The database root, exactly as named. It must exist. Without it, reldir
    /// uses the nearest ancestor of the working directory that holds `.db/`.
    #[arg(long, global = true, env = "RELDIR_DB", value_name = "DIR")]
    pub db: Option<PathBuf>,
    /// Write nothing at all -- not rows, not history, not derived state.
    #[arg(long, global = true)]
    pub readonly: bool,
    /// Output format: table (for people), json (one envelope), jsonl (a stream
    /// of records), csv (records only), sarif (diagnostics, for code scanning).
    /// Defaults to table on a terminal and jsonl otherwise.
    #[arg(long, global = true, value_name = "FORMAT", value_parser = ["table", "json", "jsonl", "csv", "sarif"])]
    pub format: Option<String>,
    /// Shorthand for `--format json`.
    #[arg(long, global = true, conflicts_with = "format")]
    pub json: bool,
    /// Print no informational prose; results, diagnostics and exit codes are unaffected.
    #[arg(long, short, global = true)]
    pub quiet: bool,
    /// Report progress from the first file, not after a second.
    #[arg(long, short, global = true)]
    pub verbose: bool,
    /// Never colour output. `NO_COLOR` in the environment does the same.
    #[arg(long, global = true)]
    pub no_color: bool,
    /// Answer yes to confirmations. Needed to confirm anything when there is no terminal to ask on.
    #[arg(long, short, global = true)]
    pub yes: bool,
    /// Plan and validate every change, show it, and write nothing.
    #[arg(long, global = true)]
    pub dry_run: bool,
    /// Never create a database implicitly; report what is missing instead.
    #[arg(long, global = true)]
    pub no_auto: bool,
    /// Allow discarding an unlabelled `.db/` that holds history, to establish it again from the files.
    #[arg(long, global = true)]
    pub rebuild_metadata: bool,
    /// Answer read-only queries from an INVALID database, reporting its faults first.
    #[arg(long, global = true)]
    pub allow_invalid: bool,
    /// Allow operations that discard data or history: replacing a pin by
    /// re-inference, beginning a new history lineage.
    #[arg(long, global = true)]
    pub allow_destructive: bool,
    /// Seconds a query may run.
    #[arg(long, global = true, value_name = "SECONDS")]
    pub timeout: Option<u64>,
    /// Seconds to wait for the writer lock; 0 tries once.
    #[arg(long, global = true, value_name = "SECONDS")]
    pub wait: Option<f64>,
    /// Largest JSON file read, in bytes.
    #[arg(long, global = true, value_name = "BYTES")]
    pub max_json_file_size: Option<u64>,
    /// Deepest JSON nesting read.
    #[arg(long, global = true, value_name = "DEPTH")]
    pub max_nesting_depth: Option<usize>,
    /// Bytes of memory a query may use, sorting included.
    #[arg(long, global = true, value_name = "BYTES")]
    pub max_query_memory: Option<u64>,
    /// Most rows a query may return; more is an error, never a silent truncation.
    #[arg(long, global = true, value_name = "ROWS")]
    pub max_result_rows: Option<usize>,
    /// Most bytes one transaction may write.
    #[arg(long, global = true, value_name = "BYTES")]
    pub max_transaction_size: Option<u64>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Make a folder a database.
    #[command(after_help = "Examples:\n  reldir init ./data\n  reldir init ./content --adopt")]
    Init {
        /// The folder; created when missing. Defaults to --db, then the working directory.
        path: Option<PathBuf>,
        /// Govern the data already in the folder, inferring its schemas.
        #[arg(long)]
        adopt: bool,
        /// Keep recorded history in version control alongside the rows.
        #[arg(long)]
        track_provenance: bool,
    },
    /// Show what a folder holds and what reldir would make of it, writing nothing.
    #[command(after_help = "Example: reldir inspect ./data")]
    Inspect {
        /// The folder. Defaults to --db, then the working directory.
        path: Option<PathBuf>,
    },
    /// Validity, unrecorded changes, and the current revision.
    #[command(after_help = "Example: reldir status")]
    Status,
    /// Validate everything; exit 2 when invalid.
    #[command(
        after_help = "Examples:\n  reldir check\n  reldir --readonly check --format sarif > reldir.sarif"
    )]
    Check {
        /// Also fail, with exit 7, on warnings and lint findings.
        #[arg(long)]
        strict: bool,
    },
    /// Report how the schemas could say more than they do.
    #[command(after_help = "Example: reldir lint --strict")]
    Lint {
        /// Only this table.
        table: Option<String>,
        /// Exit 7 when there are findings.
        #[arg(long)]
        strict: bool,
        /// Also report missing descriptions.
        #[arg(long)]
        descriptions: bool,
    },
    /// Diagnose, and repair least destructively.
    #[command(
        after_help = "Examples:\n  reldir doctor\n  reldir doctor --fix\n  reldir doctor --fix --allow-data --only FIX_REMOVE_REFERENCE"
    )]
    Doctor {
        /// Apply the default fix for each problem.
        #[arg(long)]
        fix: bool,
        /// Also apply fixes that rewrite rows.
        #[arg(long)]
        allow_data: bool,
        /// Only this fix id or problem code; naming an alternative chooses it over the default.
        #[arg(long, value_name = "FIX_ID|CODE")]
        only: Option<String>,
        /// Explain what a fix does and exit.
        #[arg(long, value_name = "FIX_ID")]
        explain: Option<String>,
        /// Do not snapshot before rewriting rows.
        #[arg(long)]
        no_snapshot: bool,
    },
    /// List the tables.
    Tables,
    /// A table's columns, key, references in and out, and size.
    #[command(after_help = "Example: reldir describe users")]
    Describe {
        /// The table.
        table: String,
    },
    /// One row, by primary key.
    #[command(
        after_help = "Examples:\n  reldir get users u1\n  reldir get memberships '[\"team1\",\"u1\"]'"
    )]
    Get {
        /// The table.
        table: String,
        /// The key: its value, or a JSON array for a composite key.
        key: String,
    },
    /// Rows of a table.
    #[command(
        after_help = "Example: reldir list users --where \"role = 'admin'\" --order -created_at --limit 10"
    )]
    List {
        /// The table.
        table: String,
        /// A SQL condition over the table's columns.
        #[arg(long = "where", value_name = "CONDITION")]
        filter: Option<String>,
        /// A column to order by; prefix `-` for descending. Defaults to the key.
        #[arg(long, value_name = "COLUMN")]
        order: Option<String>,
        /// Most rows to show.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
    /// Insert a row, or an array of rows, as one transaction.
    #[command(
        after_help = "Examples:\n  reldir insert users '{\"id\":\"u3\",\"name\":\"Ada\"}'\n  reldir insert users --from rows.json"
    )]
    Insert {
        /// The table.
        table: String,
        /// The row as JSON, or an array of rows.
        #[arg(required_unless_present = "from")]
        row: Option<String>,
        /// Read the JSON from a file, or `-` for stdin.
        #[arg(long, value_name = "FILE", conflicts_with = "row")]
        from: Option<String>,
    },
    /// Set columns of one row.
    #[command(after_help = "Example: reldir update users u1 '{\"name\":\"Ada Lovelace\"}'")]
    Update {
        /// The table.
        table: String,
        /// The row's key.
        key: String,
        /// A JSON object of the columns to set.
        patch: String,
    },
    /// Delete one row; referencing rows follow their `onDelete` actions.
    #[command(after_help = "Example: reldir delete subjects subject.calculus --dry-run")]
    Delete {
        /// The table.
        table: String,
        /// The row's key.
        key: String,
    },
    /// Run one SQL statement: a query, or an INSERT, UPDATE or DELETE.
    #[command(
        after_help = "Examples:\n  reldir sql 'SELECT * FROM users WHERE id = ?' --param '\"u1\"'\n  echo 'SELECT count(*) FROM users' | reldir sql -"
    )]
    Sql {
        /// The statement; `-` reads it from stdin.
        statement: String,
        /// A parameter, as JSON: `value` for the next `?`, or `name=value` for `:name`. Text is quoted: '"Ada"'.
        #[arg(long = "param", value_name = "VALUE")]
        params: Vec<String>,
    },
    /// Show how SQLite would run a statement, without running it.
    Explain {
        /// The statement; `-` reads it from stdin.
        statement: String,
        /// A parameter, as for `sql`.
        #[arg(long = "param", value_name = "VALUE")]
        params: Vec<String>,
    },
    /// Show, declare, pin, validate schemas; print the dialect.
    #[command(subcommand)]
    Schema(SchemaCommand),
    /// Infer schemas from rows.
    #[command(
        after_help = "Examples:\n  reldir infer users\n  reldir infer --all --write\n  reldir infer users --write --on-schema-conflict compare"
    )]
    Infer {
        /// Tables to infer; all when none are named.
        tables: Vec<String>,
        /// Every table directory.
        #[arg(long)]
        all: bool,
        /// Keep the inferred schemas.
        #[arg(long)]
        write: bool,
        /// strict refuses anything uncertain; balanced is the default; loose accepts mixed kinds as json.
        #[arg(long, default_value = "balanced", value_parser = ["strict", "balanced", "loose"])]
        strictness: String,
        /// The primary key to use, for one table.
        #[arg(long, value_delimiter = ',', value_name = "COLUMNS")]
        pk: Vec<String>,
        /// When a table already has a schema: ask, fail, compare (show the difference), or reinfer (replace it).
        #[arg(long, default_value = "ask", value_parser = ["ask", "fail", "compare", "reinfer"])]
        on_schema_conflict: String,
    },
    /// Change table structure.
    #[command(subcommand)]
    Migrate(MigrateCommand),
    /// Insert rows from a JSON, JSON Lines or CSV file.
    #[command(after_help = "Example: reldir import users --from users.jsonl")]
    Import {
        /// The table.
        table: String,
        /// The file; `-` for stdin (read as JSON).
        #[arg(long, value_name = "FILE")]
        from: String,
    },
    /// Every row of a table in canonical form.
    #[command(after_help = "Example: reldir export users --out users.jsonl")]
    Export {
        /// The table.
        table: String,
        /// Write to this new file (JSON Lines, or CSV for a .csv name) instead of stdout.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },
    /// What changed: since the last revision, or between two.
    #[command(
        after_help = "Examples:\n  reldir diff\n  reldir diff users\n  reldir diff --from 3 --to 5 --schema"
    )]
    Diff {
        /// Only this table.
        table: Option<String>,
        /// The older revision.
        #[arg(long, requires = "to")]
        from: Option<u64>,
        /// The newer revision.
        #[arg(long, requires = "from")]
        to: Option<u64>,
        /// Only schema changes.
        #[arg(long)]
        schema: bool,
    },
    /// Recorded revisions, newest first.
    Log {
        /// Most revisions to show.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
    /// One revision.
    Show {
        /// The revision number.
        revision: u64,
    },
    /// Named copies of the whole database.
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
    /// Finish interrupted transactions; or begin a new history lineage.
    #[command(after_help = "Example: reldir recover --history new-lineage --allow-destructive")]
    Recover {
        /// `new-lineage`: move unverifiable history aside and begin again from the current files.
        #[arg(long, value_name = "MODE", value_parser = ["new-lineage"])]
        history: Option<String>,
    },
    /// Remove history objects no revision references.
    Gc,
    /// Refresh the query planner's statistics.
    Analyze,
    /// An interactive SQL shell.
    Shell,
    /// Serve the database to AI agents over the Model Context Protocol, on stdio.
    Mcp,
    /// Shell completion script.
    Completions {
        /// bash, zsh, fish, elvish or powershell.
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum SchemaCommand {
    /// A table's schema document.
    Show {
        /// The table.
        table: String,
    },
    /// Declare a new table with a string key `id`, as a pin to edit.
    New {
        /// The table.
        table: String,
    },
    /// Make an inferred schema the table's declaration in `schema/`.
    Pin {
        /// The table.
        table: String,
    },
    /// Report faults in the schemas alone.
    Validate,
    /// Print the schema dialect, for editors.
    #[command(after_help = "Example: reldir schema dialect > reldir.schema.json")]
    Dialect,
}

#[derive(Subcommand, Debug, Clone)]
pub enum SnapshotCommand {
    /// Copy the whole database under a name.
    Create {
        /// The name.
        name: String,
    },
    /// The snapshots.
    List,
    /// Make the database what a snapshot holds, as one validated transaction.
    Restore {
        /// The name.
        name: String,
    },
    /// Delete a snapshot.
    Delete {
        /// The name.
        name: String,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum MigrateCommand {
    /// Declare a table from a schema document.
    AddTable {
        /// The table.
        table: String,
        /// The schema document.
        #[arg(long, value_name = "FILE")]
        from: String,
    },
    /// Remove a table and every row.
    DropTable {
        /// The table.
        table: String,
    },
    /// Rename a table, its directory, and every reference to it.
    RenameTable {
        /// The table.
        table: String,
        /// Its new name.
        new: String,
    },
    /// Add a column.
    AddColumn {
        /// The table.
        table: String,
        /// The column.
        column: String,
        /// Its type.
        #[arg(long = "type", value_name = "TYPE")]
        kind: String,
        /// Admit null, and leave existing rows without it.
        #[arg(long)]
        nullable: bool,
        /// A JSON default, written into every existing row.
        #[arg(long, value_name = "JSON")]
        default: Option<String>,
    },
    /// Remove a column from the schema and every row.
    DropColumn {
        /// The table.
        table: String,
        /// The column.
        column: String,
    },
    /// Rename a column everywhere.
    RenameColumn {
        /// The table.
        table: String,
        /// The column.
        column: String,
        /// Its new name.
        new: String,
    },
    /// Change a column's type, converting every value.
    ChangeType {
        /// The table.
        table: String,
        /// The column.
        column: String,
        /// The new type.
        kind: String,
        /// A SQL expression over the row computing each new value.
        #[arg(long, value_name = "EXPRESSION")]
        using: Option<String>,
    },
    /// Add a constraint, as JSON with a `kind` of unique, foreign_key, check, acyclic or assertion.
    #[command(
        after_help = "Example: reldir migrate add-constraint posts '{\"kind\":\"foreign_key\",\"from\":[\"user_id\"],\"to\":{\"table\":\"users\"}}'"
    )]
    AddConstraint {
        /// The table.
        table: String,
        /// The constraint.
        definition: String,
    },
    /// Remove a named constraint.
    DropConstraint {
        /// The table.
        table: String,
        /// The constraint's name.
        name: String,
    },
    /// Index columns for faster queries.
    AddIndex {
        /// The table.
        table: String,
        /// Comma-separated columns.
        #[arg(value_delimiter = ',')]
        columns: Vec<String>,
    },
    /// Remove an index.
    DropIndex {
        /// The table.
        table: String,
        /// Comma-separated columns.
        #[arg(value_delimiter = ',')]
        columns: Vec<String>,
    },
    /// Join a table to an identity domain, whose tables share one key namespace.
    SetDomain {
        /// The table.
        table: String,
        /// The domain; omit to leave the current one.
        domain: Option<String>,
    },
    /// Run a migration file: {"operations": [...]}.
    Apply {
        /// The file; `-` for stdin.
        file: String,
    },
}

impl Cli {
    fn format(&self) -> Result<Format> {
        if self.global.json {
            return Ok(Format::Json);
        }
        match &self.global.format {
            Some(text) => Format::parse(text),
            None if std::io::stdout().is_terminal() => Ok(Format::Table),
            None => Ok(Format::Jsonl),
        }
    }

    fn context(&self) -> Context {
        let global = &self.global;
        Context {
            db: global.db.clone(),
            readonly: global.readonly,
            no_auto: global.no_auto,
            rebuild_metadata: global.rebuild_metadata,
            dry_run: global.dry_run,
            yes: global.yes,
            allow_invalid: global.allow_invalid,
            allow_destructive: global.allow_destructive,
            overrides: ResourceOverrides {
                max_json_file_size: global.max_json_file_size,
                max_nesting_depth: global.max_nesting_depth,
                max_result_rows: global.max_result_rows,
                max_query_memory: global.max_query_memory,
                max_transaction_size: global.max_transaction_size,
                timeout_seconds: global.timeout,
                wait_seconds: global.wait,
            },
        }
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Init { .. } => "init",
        Command::Inspect { .. } => "inspect",
        Command::Status => "status",
        Command::Check { .. } => "check",
        Command::Lint { .. } => "lint",
        Command::Doctor { .. } => "doctor",
        Command::Tables => "tables",
        Command::Describe { .. } => "describe",
        Command::Get { .. } => "get",
        Command::List { .. } => "list",
        Command::Insert { .. } => "insert",
        Command::Update { .. } => "update",
        Command::Delete { .. } => "delete",
        Command::Sql { .. } => "sql",
        Command::Explain { .. } => "explain",
        Command::Schema(_) => "schema",
        Command::Infer { .. } => "infer",
        Command::Migrate(_) => "migrate",
        Command::Import { .. } => "import",
        Command::Export { .. } => "export",
        Command::Diff { .. } => "diff",
        Command::Log { .. } => "log",
        Command::Show { .. } => "show",
        Command::Snapshot(_) => "snapshot",
        Command::Recover { .. } => "recover",
        Command::Gc => "gc",
        Command::Analyze => "analyze",
        Command::Shell => "shell",
        Command::Mcp => "mcp",
        Command::Completions { .. } => "completions",
    }
}

/// Run a parsed command line, rendering its result; returns the exit status.
pub fn run(cli: Cli) -> i32 {
    output::set_presentation(output::Presentation {
        color: !cli.global.no_color
            && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
            && std::io::stderr().is_terminal(),
        quiet: cli.global.quiet,
        verbose: cli.global.verbose,
    });
    let format = match cli.format() {
        Ok(format) => format,
        Err(error) => {
            error.render_human();
            return error.exit;
        }
    };
    let context = cli.context();
    let command = match (&cli.command, &cli.sql) {
        (Some(command), _) => command.clone(),
        (None, Some(statement)) => Command::Sql {
            statement: statement.clone(),
            params: vec![],
        },
        (None, None) if std::io::stdin().is_terminal() => Command::Shell,
        (None, None) => Command::Sql {
            statement: "-".into(),
            params: vec![],
        },
    };
    match command {
        Command::Shell => return shell(&context),
        Command::Mcp => {
            return match crate::mcp::serve(context) {
                Ok(()) => 0,
                Err(error) => {
                    error.render_human();
                    error.exit
                }
            };
        }
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "reldir", &mut std::io::stdout());
            return 0;
        }
        _ => {}
    }
    let name = command_name(&command);
    if format == Format::Sarif
        && !matches!(
            command,
            Command::Check { .. } | Command::Lint { .. } | Command::Doctor { .. }
        )
    {
        let error = DbError::usage(
            "--format sarif reports diagnostics, so it applies to check, lint and doctor",
        );
        error.render_human();
        return error.exit;
    }
    let mut terminal = Terminal::new(format, name);
    let outcome = dispatch(&context, &mut terminal, command);
    terminal.finish(outcome)
}

/// Run one command against a sink.
pub fn dispatch(context: &Context, sink: &mut dyn Sink, command: Command) -> Result<Finish> {
    use command::{health, query, rows, schema, setup};
    match command {
        Command::Init {
            path,
            adopt,
            track_provenance,
        } => setup::init(context, sink, path.as_deref(), adopt, track_provenance),
        Command::Inspect { path } => setup::inspect(context, sink, path.as_deref()),
        Command::Status => health::status(context, sink),
        Command::Check { strict } => health::check(context, sink, strict),
        Command::Lint {
            table,
            strict,
            descriptions,
        } => health::lint(context, sink, table.as_deref(), strict, descriptions),
        Command::Doctor {
            fix,
            allow_data,
            only,
            explain,
            no_snapshot,
        } => health::doctor(
            context,
            sink,
            DoctorOptions {
                fix,
                allow_data,
                only: only.as_deref(),
                explain: explain.as_deref(),
                no_snapshot,
            },
        ),
        Command::Tables => rows::tables(context, sink),
        Command::Describe { table } => rows::describe(context, sink, &table),
        Command::Get { table, key } => rows::get(context, sink, &table, &key),
        Command::List {
            table,
            filter,
            order,
            limit,
        } => rows::list(
            context,
            sink,
            &table,
            ListOptions {
                filter: filter.as_deref(),
                order: order.as_deref(),
                limit,
            },
        ),
        Command::Insert { table, row, from } => {
            let text = match (row, from) {
                (Some(row), None) => row,
                (None, Some(from)) => command::read_input(&from, 1 << 30)?,
                _ => return Err(DbError::usage("give the row as JSON, or --from a file")),
            };
            rows::insert(context, sink, &table, &text)
        }
        Command::Update { table, key, patch } => rows::update(context, sink, &table, &key, &patch),
        Command::Delete { table, key } => rows::delete(context, sink, &table, &key),
        Command::Sql { statement, params } => query::sql(context, sink, &statement, &params),
        Command::Explain { statement, params } => {
            query::explain(context, sink, &statement, &params)
        }
        Command::Schema(SchemaCommand::Show { table }) => schema::show(context, sink, &table),
        Command::Schema(SchemaCommand::New { table }) => schema::new(context, sink, &table),
        Command::Schema(SchemaCommand::Pin { table }) => schema::pin(context, sink, &table),
        Command::Schema(SchemaCommand::Validate) => schema::validate(context, sink),
        Command::Schema(SchemaCommand::Dialect) => schema::dialect(sink),
        Command::Infer {
            tables,
            all,
            write,
            strictness,
            pk,
            on_schema_conflict,
        } => schema::infer(
            context,
            sink,
            schema::InferOptions {
                tables,
                all,
                write,
                strictness: schema::parse_strictness(&strictness)?,
                primary_key: (!pk.is_empty()).then_some(pk.as_slice()),
                on_conflict: OnConflict::parse(&on_schema_conflict)?,
            },
        ),
        Command::Migrate(migrate) => {
            let migration = match migrate {
                MigrateCommand::Apply { file } => schema::read_migration(&file)?,
                other => Migration {
                    operations: vec![operation(other)?],
                },
            };
            schema::migrate(context, sink, migration)
        }
        Command::Import { table, from } => rows::import(context, sink, &table, &from),
        Command::Export { table, out } => {
            let as_csv = out
                .as_deref()
                .and_then(|path| path.extension())
                .is_some_and(|ext| ext == "csv");
            rows::export(context, sink, &table, out.as_deref(), as_csv)
        }
        Command::Diff {
            table,
            from,
            to,
            schema,
        } => history::diff(
            context,
            sink,
            history::DiffOptions {
                revisions: from.zip(to),
                table,
                schema_only: schema,
            },
        ),
        Command::Log { limit } => history::log(context, sink, limit),
        Command::Show { revision } => history::show(context, sink, revision),
        Command::Snapshot(snapshot) => history::snapshot(
            context,
            sink,
            match snapshot {
                SnapshotCommand::Create { name } => history::SnapshotAction::Create(name),
                SnapshotCommand::List => history::SnapshotAction::List,
                SnapshotCommand::Restore { name } => history::SnapshotAction::Restore(name),
                SnapshotCommand::Delete { name } => history::SnapshotAction::Delete(name),
            },
        ),
        Command::Recover { history: mode } => history::recover(context, sink, mode.is_some()),
        Command::Gc => history::gc(context, sink),
        Command::Analyze => history::analyze(context, sink),
        Command::Shell | Command::Mcp | Command::Completions { .. } => Err(DbError::usage(
            "this command runs interactively and has no result to report",
        )),
    }
}

fn operation(command: MigrateCommand) -> Result<Operation> {
    let json = |text: &str, what: &str| {
        crate::json::parse_str(text)
            .map_err(|error| DbError::usage(format!("{what} is not JSON: {error}")))
    };
    Ok(match command {
        MigrateCommand::AddTable { table, from } => {
            let text = command::read_input(&from, 64 * 1024 * 1024)?;
            Operation::AddTable {
                table,
                schema: Box::new(json(&text, "the schema")?),
            }
        }
        MigrateCommand::DropTable { table } => Operation::DropTable { table },
        MigrateCommand::RenameTable { table, new } => Operation::RenameTable { table, new },
        MigrateCommand::AddColumn {
            table,
            column,
            kind,
            nullable,
            default,
        } => Operation::AddColumn {
            table,
            column,
            kind,
            nullable,
            default: default.map(|text| json(&text, "the default")).transpose()?,
        },
        MigrateCommand::DropColumn { table, column } => Operation::DropColumn { table, column },
        MigrateCommand::RenameColumn { table, column, new } => {
            Operation::RenameColumn { table, column, new }
        }
        MigrateCommand::ChangeType {
            table,
            column,
            kind,
            using,
        } => Operation::ChangeType {
            table,
            column,
            kind,
            using,
        },
        MigrateCommand::AddConstraint { table, definition } => {
            let definition: Constraint =
                serde_json::from_value(json(&definition, "the constraint")?).map_err(|error| {
                    DbError::usage(format!("the constraint is not one reldir knows: {error}"))
                })?;
            Operation::AddConstraint { table, definition }
        }
        MigrateCommand::DropConstraint { table, name } => Operation::DropConstraint { table, name },
        MigrateCommand::AddIndex { table, columns } => Operation::AddIndex { table, columns },
        MigrateCommand::DropIndex { table, columns } => Operation::DropIndex { table, columns },
        MigrateCommand::SetDomain { table, domain } => {
            Operation::SetIdentityDomain { table, domain }
        }
        MigrateCommand::Apply { .. } => unreachable!("handled by the caller"),
    })
}

/// The interactive shell: SQL statements ending in `;`, and dot commands.
fn shell(context: &Context) -> i32 {
    use rustyline::{DefaultEditor, error::ReadlineError};
    let Ok(mut editor) = DefaultEditor::new() else {
        eprintln!("the shell needs a terminal");
        return 1;
    };
    eprintln!("reldir {} -- SQL ending in `;`, or .help", crate::VERSION);
    let mut buffer = String::new();
    loop {
        let prompt = if buffer.is_empty() {
            "reldir> "
        } else {
            "   ...> "
        };
        match editor.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim();
                if buffer.is_empty() && trimmed.starts_with('.') {
                    let _ = editor.add_history_entry(trimmed);
                    let mut words = trimmed.split_whitespace();
                    let command = match (words.next(), words.next()) {
                        (Some(".quit" | ".exit"), _) => return 0,
                        (Some(".help"), _) => {
                            eprintln!(
                                ".tables  .describe TABLE  .schema TABLE  .check  .status  .quit"
                            );
                            continue;
                        }
                        (Some(".tables"), _) => Command::Tables,
                        (Some(".describe"), Some(table)) => Command::Describe {
                            table: table.into(),
                        },
                        (Some(".schema"), Some(table)) => Command::Schema(SchemaCommand::Show {
                            table: table.into(),
                        }),
                        (Some(".check"), _) => Command::Check { strict: false },
                        (Some(".status"), _) => Command::Status,
                        _ => {
                            eprintln!("unknown command {trimmed:?}; .help lists them");
                            continue;
                        }
                    };
                    let mut terminal = Terminal::new(Format::Table, command_name(&command));
                    let outcome = dispatch(context, &mut terminal, command);
                    terminal.finish(outcome);
                    continue;
                }
                buffer.push_str(&line);
                buffer.push('\n');
                if !trimmed.ends_with(';') {
                    continue;
                }
                let statement = std::mem::take(&mut buffer);
                let _ = editor.add_history_entry(statement.trim());
                let mut terminal = Terminal::new(Format::Table, "sql");
                let outcome = command::query::sql(
                    context,
                    &mut terminal,
                    statement.trim().trim_end_matches(';'),
                    &[],
                );
                terminal.finish(outcome);
            }
            Err(ReadlineError::Interrupted) => buffer.clear(),
            Err(ReadlineError::Eof) => return 0,
            Err(error) => {
                eprintln!("{error}");
                return 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every argument and command says what it is for.
    #[test]
    fn test2230_every_argument_has_help() {
        fn walk(command: &clap::Command, path: &str, missing: &mut Vec<String>) {
            for argument in command.get_arguments() {
                let id = argument.get_id().as_str();
                if matches!(id, "help" | "version") {
                    continue;
                }
                if argument.get_help().is_none() && argument.get_long_help().is_none() {
                    missing.push(format!("{path} {id}"));
                }
            }
            for sub in command.get_subcommands() {
                if sub.get_about().is_none() && sub.get_long_about().is_none() {
                    missing.push(format!("{path} {} (about)", sub.get_name()));
                }
                walk(sub, &format!("{path} {}", sub.get_name()), missing);
            }
        }
        let mut missing = vec![];
        walk(&Cli::command(), "reldir", &mut missing);
        assert!(missing.is_empty(), "undocumented: {missing:#?}");
        Cli::command().debug_assert();
    }
}
