//! The command line: arguments in, a command run, its result rendered.
//!
//! Parsing lives here and nowhere else. Each subcommand becomes one call into
//! [`crate::command`], whose output goes through a [`Terminal`] sink in the
//! chosen format.

use crate::{
    command::{
        self, Context, health::DoctorOptions, history, rows::ListOptions, schema::OnConflict,
    },
    output::{self, Finish, Format, Sink, Terminal},
};
use clap::{Args, CommandFactory, Parser, Subcommand};
use reldir_core::{
    config::ResourceOverrides,
    diagnostic::{DbError, Result},
    migrate::{Constraint, Migration, Operation},
};
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
    /// Name this change so a retry is recognised: if history already holds a
    /// revision with this key, the change is not made again and that revision
    /// is reported. Makes retrying after an uncertain failure always safe.
    #[arg(long, global = true, value_name = "KEY")]
    pub idempotency_key: Option<String>,
    /// Widen a table's contract, as part of this change, to admit a value
    /// the change writes that the contract has never admitted. Without it,
    /// such a change is refused with CONTRACT_WIDENING_REQUIRED.
    #[arg(long, global = true)]
    pub widen: bool,
    /// Commit the change only if the database still holds the state a dry
    /// run reported as `data.state`; otherwise refuse with PLAN_CHANGED.
    #[arg(long, global = true, value_name = "STATE")]
    pub expect_state: Option<String>,
    /// Seconds a query may run; fractions are allowed (0.5).
    #[arg(long, global = true, value_name = "SECONDS")]
    pub timeout: Option<f64>,
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
    #[command(
        after_help = "Examples:\n  reldir init ./data\n  reldir init ./content --adopt\n  reldir init ./content --adopt --ignore 'books/*.txt'"
    )]
    Init {
        /// The folder; created when missing. Defaults to --db, then the working directory.
        path: Option<PathBuf>,
        /// Govern the data already in the folder, inferring its schemas.
        #[arg(long)]
        adopt: bool,
        /// Keep recorded history in version control alongside the rows.
        #[arg(long)]
        track_provenance: bool,
        /// A glob for files and directories the database does not govern,
        /// added to `ignore` in `.db/config` before adoption reads the folder.
        /// Repeatable.
        #[arg(long, value_name = "PATTERN")]
        ignore: Vec<String>,
    },
    /// Convert a format 2 database to format 3: its history, snapshots,
    /// schemas and settings. No row file is changed.
    #[command(
        after_help = "Example: reldir upgrade\n\nFormat 2's metadata is kept in .db/format-2/, and each pin's original in .db/format-2/pins/."
    )]
    Upgrade {
        /// Begin format 3 history from the files rather than bringing format
        /// 2's over -- for history that does not verify. It is kept aside.
        #[arg(long)]
        new_lineage: bool,
    },
    /// The documentation: a topic's page, or the list of topics. Needs no
    /// database.
    #[command(after_help = "Examples:\n  reldir docs\n  reldir docs schemas")]
    Docs {
        /// The topic; omit to list them.
        topic: Option<String>,
    },
    /// The machine protocol: the JSON Schema of everything `--format json`
    /// and `--format jsonl` print. Needs no database.
    #[command(subcommand)]
    Protocol(ProtocolCommand),
    /// Show what a folder holds and what reldir would make of it, writing nothing.
    #[command(after_help = "Example: reldir inspect ./data")]
    Inspect {
        /// The folder. Defaults to --db, then the working directory.
        path: Option<PathBuf>,
        /// A glob for files and directories to leave out, as `init --ignore`
        /// would. Repeatable.
        #[arg(long, value_name = "PATTERN")]
        ignore: Vec<String>,
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
        /// Also verify every object recorded history holds, and the mirror's
        /// own consistency: everything reldir keeps, not only what it reads.
        #[arg(long)]
        deep: bool,
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
    #[command(after_help = "Example: reldir tables")]
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
        #[arg(long, value_name = "COLUMN", allow_hyphen_values = true)]
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
    /// Run SQL: a query, an INSERT, UPDATE or DELETE, or a script of several, which is one transaction.
    #[command(
        after_help = "Examples:\n  reldir sql 'SELECT * FROM users WHERE id = ?' --param '\"u1\"'\n  echo 'SELECT count(*) FROM users' | reldir sql -\n  reldir sql \"UPDATE accounts SET balance = balance - 5 WHERE id = 'a'; UPDATE accounts SET balance = balance + 5 WHERE id = 'b'\"\n  reldir sql -f transfer.sql"
    )]
    Sql {
        /// The statement, or several separated by `;`; `-` reads them from stdin.
        #[arg(required_unless_present = "file", conflicts_with = "file")]
        statement: Option<String>,
        /// Read the statements from a file.
        #[arg(short = 'f', long = "file", value_name = "PATH")]
        file: Option<PathBuf>,
        /// A parameter, as JSON: `value` for the next `?`, or `name=value` for `:name`. Text is quoted: '"Ada"'.
        #[arg(long = "param", value_name = "VALUE")]
        params: Vec<String>,
    },
    /// Show how SQLite would run a statement, without running it.
    #[command(after_help = "Example: reldir explain \"SELECT * FROM posts WHERE user_id = 'ada'\"")]
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
        /// State statistical facts as constraints too: any of enums, unique,
        /// checks, min-items, or all. The inferred contract states only what
        /// every row structurally is; these are coincidences of the data until
        /// you say otherwise.
        #[arg(long, value_name = "WHAT", default_value = "")]
        tighten: String,
        /// The key to use, for one table: columns whose values already name
        /// its files. Without it, the key is read from the file names.
        #[arg(long, value_delimiter = ',', value_name = "COLUMNS")]
        pk: Vec<String>,
        /// When a table already has a schema: ask, fail, compare (show the difference), or reinfer (replace it).
        #[arg(long, default_value = "ask", value_parser = ["ask", "fail", "compare", "reinfer"])]
        on_schema_conflict: String,
    },
    /// Change table structure.
    #[command(subcommand)]
    Migrate(MigrateCommand),
    /// Import rows from a JSON array, JSON lines or CSV, as one change.
    #[command(
        after_help = "Examples:\n  reldir import users --from users.jsonl\n  reldir import users --from users.csv --mode upsert\n  curl -s https://example.com/users.json | reldir import users --from - --input-format json --mode replace\n\nModes: insert (every row new), upsert (rows with a key the table holds set what they state), replace (the table becomes exactly these rows)."
    )]
    Import {
        /// The table.
        table: String,
        /// The file; `-` for stdin.
        #[arg(long, value_name = "FILE")]
        from: String,
        /// json, jsonl or csv; by default, the file's extension.
        #[arg(long, value_name = "FORMAT")]
        input_format: Option<String>,
        /// How the rows meet the table's: insert, upsert or replace.
        #[arg(long, default_value = "insert", value_parser = ["insert", "upsert", "replace"])]
        mode: String,
    },
    /// Export a table's rows, exactly as their files hold them.
    #[command(
        after_help = "Examples:\n  reldir export users --out users.jsonl\n  reldir export users --out adults.parquet --where 'age >= 18' --order -age\n  reldir export users --format jsonl"
    )]
    Export {
        /// The table.
        table: String,
        /// Write to this new file, which must not exist, instead of printing rows.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// The file's format -- json, jsonl, csv or parquet; by default, its extension.
        #[arg(long = "as", value_name = "FORMAT", requires = "out")]
        file_format: Option<String>,
        /// Only rows matching this SQL condition.
        #[arg(long = "where", value_name = "CONDITION")]
        filter: Option<String>,
        /// Order by this column (`-column` for descending); by key otherwise.
        #[arg(long, value_name = "COLUMN", allow_hyphen_values = true)]
        order: Option<String>,
    },
    /// What changed: since the last revision, or between two.
    #[command(
        after_help = "Examples:\n  reldir diff\n  reldir diff users\n  reldir diff --from 3 --to 5 --schema\n  reldir diff --git main"
    )]
    Diff {
        /// Only this table.
        table: Option<String>,
        /// The older revision.
        #[arg(long, requires = "to")]
        from: Option<String>,
        /// The newer revision.
        #[arg(long, requires = "from")]
        to: Option<String>,
        /// Compare the files with a git revision -- a commit, branch or tag -- row by row.
        #[arg(long, value_name = "REV", conflicts_with_all = ["from", "to"])]
        git: Option<String>,
        /// Only schema changes.
        #[arg(long)]
        schema: bool,
    },
    /// Recorded revisions, newest first.
    #[command(after_help = "Examples:\n  reldir log\n  reldir log --limit 5")]
    Log {
        /// Most revisions to show.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
    /// One revision.
    #[command(after_help = "Examples:\n  reldir show 12\n  reldir show '#12'")]
    Show {
        /// The revision: its number in this clone (`12` or `#12`), or the
        /// beginning of its id.
        revision: String,
    },
    /// Put row files back as history recorded them: a deleted row, or a row
    /// as it was before a change. The files are written byte for byte, as a
    /// change validated like any other.
    #[command(
        after_help = "Examples:\n  reldir restore users/ada.json\n  reldir restore posts/p1.json posts/p2.json --at 12"
    )]
    Restore {
        /// Row files, as `show` and `diff` name them: `<table>/<file>.json`.
        #[arg(required = true)]
        paths: Vec<String>,
        /// The revision to take every file from: its number (`12` or `#12`) or
        /// the beginning of its id. By default, each file comes from the
        /// newest revision that holds it.
        #[arg(long, value_name = "REVISION")]
        at: Option<String>,
    },
    /// Named copies of the whole database.
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
    /// Git integration: row files that merge member by member and diff as JSON.
    #[command(subcommand)]
    Git(GitCommand),
    /// Finish interrupted transactions; or begin a new history lineage.
    #[command(after_help = "Example: reldir recover --history new-lineage --allow-destructive")]
    Recover {
        /// `new-lineage`: move unverifiable history aside and begin again from the current files.
        #[arg(long, value_name = "MODE", value_parser = ["new-lineage"])]
        history: Option<String>,
    },
    /// Remove history objects no revision references.
    #[command(after_help = "Examples:\n  reldir gc --dry-run\n  reldir gc")]
    Gc,
    /// Refresh the query planner's statistics.
    #[command(after_help = "Example: reldir analyze")]
    Analyze,
    /// An interactive SQL shell.
    #[command(after_help = "Example: reldir shell")]
    Shell,
    /// Serve the database to AI agents over the Model Context Protocol, on stdio.
    #[command(after_help = "Example: claude mcp add reldir -- reldir --db ./data mcp")]
    Mcp,
    /// Watch the folder, so every process that opens the database reads only what changed.
    #[command(
        after_help = "Example: reldir --db ./data serve\n\nRun it in the background, or under a service manager. While it runs, opening the database examines the paths that changed since the last time, not every file: what keeps a large folder's commands fast. Stop it with Ctrl-C; without it, every command examines the folder whole."
    )]
    Serve,
    /// Shell completion script.
    #[command(
        after_help = "Example: reldir completions bash > ~/.local/share/bash-completion/completions/reldir"
    )]
    Completions {
        /// bash, zsh, fish, elvish or powershell.
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum SchemaCommand {
    /// A table's schema document.
    #[command(after_help = "Example: reldir schema show users")]
    Show {
        /// The table.
        table: String,
    },
    /// Declare a new table with a string key `id`, as a pin to edit.
    #[command(after_help = "Example: reldir schema new notes")]
    New {
        /// The table.
        table: String,
    },
    /// Make an inferred schema the table's declaration in `schema/`.
    #[command(after_help = "Example: reldir schema pin users")]
    Pin {
        /// The table.
        table: String,
    },
    /// Widen a table's contract: to admit what its files hold, or at one
    /// member, as stated.
    #[command(
        after_help = "Examples:\n  reldir schema widen users                     # admit what the files hold\n  reldir schema widen users.nick --null --optional\n  reldir schema widen 'posts.tags[]' --type integer"
    )]
    Widen {
        /// A table, or a member of one: `users`, `users.nick`, `posts.tags[]`.
        target: String,
        /// Admit null.
        #[arg(long)]
        null: bool,
        /// Let rows leave the member out.
        #[arg(long)]
        optional: bool,
        /// Admit values of a JSON type: null, boolean, integer, number,
        /// string, array or object.
        #[arg(long = "type", value_name = "TYPE")]
        types: Vec<String>,
        /// Admit any value at all.
        #[arg(long)]
        any: bool,
    },
    /// Report faults in the schemas alone.
    #[command(after_help = "Example: reldir schema validate")]
    Validate,
    /// Print the schema dialect, for editors.
    #[command(after_help = "Example: reldir schema dialect > reldir.schema.json")]
    Dialect,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ProtocolCommand {
    /// Print the JSON Schema of every record, event, diagnostic and result.
    #[command(after_help = "Example: reldir protocol schema --format json > reldir-protocol.json")]
    Schema,
}

#[derive(Subcommand, Debug, Clone)]
pub enum GitCommand {
    /// Route the tables' row files through reldir's merge and diff drivers.
    #[command(
        after_help = "Examples:\n  reldir git setup\n  reldir git setup --hooks\n\nWrites the database's .gitattributes and the repository's local git config."
    )]
    Setup {
        /// Also check the database after every merge and checkout.
        #[arg(long)]
        hooks: bool,
    },
    /// The merge driver git runs: merge a row file's three versions into OURS.
    #[command(
        after_help = "Example (as git runs it): reldir git merge-file base.json ours.json theirs.json --path users/ada.json --labels base HEAD feature"
    )]
    MergeFile {
        /// The common ancestor's version; empty when both sides added the file.
        base: PathBuf,
        /// Our version, which receives the merge.
        ours: PathBuf,
        /// Their version.
        theirs: PathBuf,
        /// The file's path in the repository, to name it.
        #[arg(long)]
        path: Option<String>,
        /// The labels of the base, ours and theirs, for conflict markers.
        #[arg(long, num_args = 3, value_names = ["BASE", "OURS", "THEIRS"])]
        labels: Vec<String>,
        /// The length of the conflict markers.
        #[arg(long, default_value_t = 7)]
        marker_size: usize,
    },
    /// The diff driver git runs: a row file as one line per value.
    #[command(after_help = "Example: reldir --format table git textconv users/ada.json")]
    Textconv {
        /// The file.
        file: PathBuf,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum SnapshotCommand {
    /// Copy the whole database under a name.
    #[command(after_help = "Example: reldir snapshot create before-import")]
    Create {
        /// The name.
        name: String,
    },
    /// The snapshots.
    #[command(after_help = "Example: reldir snapshot list")]
    List,
    /// Make the database what a snapshot holds, as one validated transaction.
    #[command(after_help = "Example: reldir snapshot restore before-import --yes")]
    Restore {
        /// The name.
        name: String,
    },
    /// Delete a snapshot.
    #[command(after_help = "Example: reldir snapshot delete before-import")]
    Delete {
        /// The name.
        name: String,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum MigrateCommand {
    /// Declare a table from a schema document.
    #[command(after_help = "Example: reldir migrate add-table notes --from notes.schema.json")]
    AddTable {
        /// The table.
        table: String,
        /// The schema document.
        #[arg(long, value_name = "FILE")]
        from: String,
    },
    /// Remove a table and every row.
    #[command(after_help = "Example: reldir migrate drop-table notes --yes")]
    DropTable {
        /// The table.
        table: String,
    },
    /// Rename a table, its directory, and every reference to it.
    #[command(after_help = "Example: reldir migrate rename-table posts articles")]
    RenameTable {
        /// The table.
        table: String,
        /// Its new name.
        new: String,
    },
    /// Add a column.
    #[command(
        after_help = "Examples:\n  reldir migrate add-column users bio --type string --nullable\n  reldir migrate add-column users active --type bool --default true"
    )]
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
    #[command(after_help = "Example: reldir migrate drop-column users bio --yes")]
    DropColumn {
        /// The table.
        table: String,
        /// The column.
        column: String,
    },
    /// Rename a column everywhere.
    #[command(after_help = "Example: reldir migrate rename-column users name full_name")]
    RenameColumn {
        /// The table.
        table: String,
        /// The column.
        column: String,
        /// Its new name.
        new: String,
    },
    /// Change a column's type, converting every value.
    #[command(
        after_help = "Examples:\n  reldir migrate change-type items price decimal\n  reldir migrate change-type items state enum --values open,closed\n  reldir migrate change-type items n string --drop-constraint minimum"
    )]
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
        /// The members, for `enum`: a,b,c.
        #[arg(long, value_delimiter = ',', value_name = "VALUES")]
        values: Option<Vec<String>>,
        /// Drop a rule that means nothing for the new type, by keyword (repeatable).
        #[arg(long = "drop-constraint", value_name = "KEYWORD")]
        drop_constraints: Vec<String>,
    },
    /// Re-key a table: rename every file to its new key, and carry each
    /// reference by its onUpdate action.
    #[command(
        after_help = "Examples:\n  reldir migrate change-identity people '{\"source\":\"body\",\"columns\":[\"email\"],\"naming\":{\"rule\":\"reldir\"}}'\n  reldir migrate change-identity notes '{\"source\":\"filename\",\"column\":\"slug\",\"type\":\"string\"}'"
    )]
    ChangeIdentity {
        /// The table.
        table: String,
        /// The new `x-reldir.identity`, as JSON.
        identity: String,
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
    #[command(after_help = "Example: reldir migrate drop-constraint posts fk_user_id")]
    DropConstraint {
        /// The table.
        table: String,
        /// The constraint's name.
        name: String,
    },
    /// Index columns for faster queries.
    #[command(after_help = "Example: reldir migrate add-index posts user_id")]
    AddIndex {
        /// The table.
        table: String,
        /// Comma-separated columns.
        #[arg(value_delimiter = ',')]
        columns: Vec<String>,
    },
    /// Remove an index.
    #[command(after_help = "Example: reldir migrate drop-index posts user_id")]
    DropIndex {
        /// The table.
        table: String,
        /// Comma-separated columns.
        #[arg(value_delimiter = ',')]
        columns: Vec<String>,
    },
    /// Join a table to an identity domain, whose tables share one key namespace.
    #[command(
        after_help = "Examples:\n  reldir migrate set-domain articles content\n  reldir migrate set-domain articles"
    )]
    SetDomain {
        /// The table.
        table: String,
        /// The domain; omit to leave the current one.
        domain: Option<String>,
    },
    /// Run a migration file: {"operations": [...]}.
    #[command(after_help = "Example: reldir migrate apply migrations/0003-rename.json")]
    Apply {
        /// The file; `-` for stdin.
        file: String,
    },
}

/// The output format: `--json`, then `--format`, then `RELDIR_FORMAT`, then
/// table for a terminal and the line protocol for anything else.
fn chosen_format(json: bool, format: Option<&str>) -> Result<Format> {
    if json {
        return Ok(Format::Json);
    }
    if let Some(text) = format {
        return Format::parse(text);
    }
    match std::env::var("RELDIR_FORMAT") {
        Ok(text) if !text.is_empty() => Format::parse(&text)
            .map_err(|error| error.with_help("RELDIR_FORMAT names one of the output formats")),
        _ if std::io::stdout().is_terminal() => Ok(Format::Table),
        _ => Ok(Format::Jsonl),
    }
}

/// The format a command line asked for, read from the raw arguments: what an
/// argument error that stopped parsing is reported in.
fn format_of_arguments() -> Format {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let json = arguments.iter().any(|argument| argument == "--json");
    let format = arguments.iter().enumerate().find_map(|(index, argument)| {
        argument
            .strip_prefix("--format=")
            .map(String::from)
            .or_else(|| {
                (argument == "--format")
                    .then(|| arguments.get(index + 1).cloned())
                    .flatten()
            })
    });
    chosen_format(json, format.as_deref()).unwrap_or(Format::Jsonl)
}

/// The command a panic is reported against, and in which format.
static RUNNING: std::sync::Mutex<Option<(Format, &'static str)>> = std::sync::Mutex::new(None);

/// Report a defect -- a panic -- as an `INTERNAL_ERROR` result, in the format
/// the command line chose.
pub fn install_panic_report() {
    std::panic::set_hook(Box::new(|info| {
        let (format, command) = RUNNING
            .lock()
            .ok()
            .and_then(|running| *running)
            .unwrap_or_else(|| (format_of_arguments(), "reldir"));
        let location = info
            .location()
            .map(|location| format!(" at {}:{}", location.file(), location.line()))
            .unwrap_or_default();
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| text.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a defect".into());
        let error = DbError::new(
            reldir_core::code::Code::INTERNAL_ERROR,
            format!("reldir failed{location}: {what}"),
        )
        .with_help("this is a defect in reldir; please report it with the command that caused it");
        output::report_failure(format, command, &error);
    }));
}

/// An argument clap refused: help and version as asked, anything else a
/// usage error in the format the command line asked for.
pub fn refuse_arguments(error: clap::Error) -> i32 {
    use clap::error::ErrorKind;
    if matches!(
        error.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        let _ = error.print();
        return 0;
    }
    let format = format_of_arguments();
    if !format.is_machine() {
        let _ = error.print();
        return reldir_core::code::Exit::Usage.code();
    }
    let rendered = error.render().to_string();
    let message = rendered
        .lines()
        .next()
        .unwrap_or_default()
        .trim_start_matches("error: ")
        .to_string();
    let usage = DbError::usage(message).with_help(
        rendered
            .lines()
            .skip(1)
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("; "),
    );
    output::report_failure(format, "reldir", &usage);
    usage.exit().code()
}

/// The first word a statement may begin with, for text given in place of a
/// subcommand.
const STATEMENT_WORDS: &[&str] = &[
    "select", "with", "insert", "update", "delete", "replace", "explain", "values", "-",
];

/// The subcommand a mistyped word most likely meant.
fn nearest_subcommand(word: &str) -> Option<String> {
    let distance = |left: &str, right: &str| -> usize {
        let right: Vec<char> = right.chars().collect();
        let mut previous: Vec<usize> = (0..=right.len()).collect();
        for (i, a) in left.chars().enumerate() {
            let mut current = vec![i + 1];
            for (j, b) in right.iter().enumerate() {
                current.push(
                    (previous[j] + usize::from(a != *b))
                        .min(previous[j + 1] + 1)
                        .min(current[j] + 1),
                );
            }
            previous = current;
        }
        previous[right.len()]
    };
    Cli::command()
        .get_subcommands()
        .map(|subcommand| subcommand.get_name().to_string())
        .map(|name| (distance(&word.to_lowercase(), &name), name))
        .filter(|(distance, _)| *distance <= 2)
        .min()
        .map(|(_, name)| name)
}

impl Cli {
    fn format(&self) -> Result<Format> {
        chosen_format(self.global.json, self.global.format.as_deref())
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
            idempotency_key: global.idempotency_key.clone(),
            widen: global.widen,
            expect_state: global.expect_state.clone(),
            cancel: None,
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

pub fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Init { .. } => "init",
        Command::Upgrade { .. } => "upgrade",
        Command::Protocol(_) => "protocol",
        Command::Docs { .. } => "docs",
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
        Command::Restore { .. } => "restore",
        Command::Snapshot(_) => "snapshot",
        Command::Git(_) => "git",
        Command::Recover { .. } => "recover",
        Command::Gc => "gc",
        Command::Analyze => "analyze",
        Command::Shell => "shell",
        Command::Mcp => "mcp",
        Command::Serve => "serve",
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
    if let Some(progress) = output::TerminalProgress::for_terminal() {
        reldir_core::progress::install(std::sync::Arc::new(progress));
    }
    let format = match cli.format() {
        Ok(format) => format,
        Err(error) => {
            output::report_failure(format_of_arguments(), "reldir", &error);
            return error.exit().code();
        }
    };
    let context = cli.context();
    let command = match (&cli.command, &cli.sql) {
        (Some(command), _) => command.clone(),
        (None, Some(statement)) => {
            // Text in place of a subcommand is a statement only when it reads
            // as one; otherwise it is most likely a mistyped subcommand.
            let first = statement
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_lowercase();
            if !STATEMENT_WORDS.contains(&first.as_str()) {
                let mut error = DbError::usage(format!(
                    "{first:?} is neither a subcommand nor the start of a SQL statement"
                ));
                if let Some(meant) = nearest_subcommand(&first) {
                    error = error.with_help(format!("did you mean `reldir {meant}`?"));
                }
                output::report_failure(format, "reldir", &error);
                return error.exit().code();
            }
            Command::Sql {
                statement: Some(statement.clone()),
                file: None,
                params: vec![],
            }
        }
        (None, None) if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => {
            Command::Shell
        }
        // Without a terminal there is no one to open a shell for, and reading
        // a statement from whatever stdin happens to be could wait forever.
        (None, None) => {
            let error = DbError::usage(
                "no subcommand, and no terminal to open a shell on; pipe a statement with \
                 `reldir sql -`, or name a subcommand",
            )
            .with_help("`reldir --help` lists the subcommands");
            output::report_failure(format, "reldir", &error);
            return error.exit().code();
        }
    };
    match command {
        Command::Shell => return crate::shell::run(&context, format),
        Command::Mcp => {
            return match crate::mcp::serve(context) {
                Ok(()) => 0,
                Err(error) => {
                    output::render_error(&error);
                    error.exit().code()
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
        output::render_error(&error);
        return error.exit().code();
    }
    if let Ok(mut running) = RUNNING.lock() {
        *running = Some((format, name));
    }
    let mut terminal = Terminal::new(format, name);
    let outcome = dispatch(&context, &mut terminal, command);
    terminal.finish(outcome)
}

/// Run one command against a sink.
pub fn dispatch(context: &Context, sink: &mut dyn Sink, command: Command) -> Result<Finish> {
    use command::{health, query, rows, schema, setup};
    match command {
        Command::Upgrade { new_lineage } => setup::upgrade(context, sink, new_lineage),
        Command::Docs { topic: None } => {
            for (topic, source) in crate::docs::PAGES {
                let mut record = serde_json::Map::new();
                record.insert("kind".into(), serde_json::json!("doc_topic"));
                record.insert("topic".into(), serde_json::json!(topic));
                record.insert(
                    "title".into(),
                    serde_json::json!(crate::docs::title(source)),
                );
                sink.record(record)?;
            }
            Ok(Finish::ok(String::new()))
        }
        Command::Docs { topic: Some(topic) } => {
            let text = crate::docs::page(&topic).ok_or_else(|| {
                DbError::usage(format!("there is no page on {topic:?}"))
                    .with_help("`reldir docs` lists the topics")
            })?;
            sink.text("doc", &topic, &text)?;
            Ok(Finish::ok(String::new()))
        }
        Command::Protocol(ProtocolCommand::Schema) => {
            sink.document("protocol", crate::protocol::schema())?;
            Ok(Finish::ok(String::new()))
        }
        Command::Init {
            path,
            adopt,
            track_provenance,
            ignore,
        } => setup::init(
            context,
            sink,
            path.as_deref(),
            adopt,
            track_provenance,
            &ignore,
        ),
        Command::Inspect { path, ignore } => {
            setup::inspect(context, sink, path.as_deref(), &ignore)
        }
        Command::Status => health::status(context, sink),
        Command::Check { strict, deep } => health::check(context, sink, strict, deep),
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
        Command::Sql {
            statement,
            file,
            params,
        } => query::sql(
            context,
            sink,
            statement.as_deref(),
            file.as_deref(),
            &params,
        ),
        Command::Explain { statement, params } => {
            query::explain(context, sink, &statement, &params)
        }
        Command::Schema(SchemaCommand::Show { table }) => schema::show(context, sink, &table),
        Command::Schema(SchemaCommand::New { table }) => schema::new(context, sink, &table),
        Command::Schema(SchemaCommand::Pin { table }) => schema::pin(context, sink, &table),
        Command::Schema(SchemaCommand::Widen {
            target,
            null,
            optional,
            types,
            any,
        }) => schema::widen(
            context,
            sink,
            &target,
            reldir_core::schema::widen::Stated {
                null,
                optional,
                types,
                any,
            },
        ),
        Command::Schema(SchemaCommand::Validate) => schema::validate(context, sink),
        Command::Schema(SchemaCommand::Dialect) => schema::dialect(sink),
        Command::Infer {
            tables,
            all,
            write,
            tighten,
            pk,
            on_schema_conflict,
        } => schema::infer(
            context,
            sink,
            schema::InferOptions {
                tables,
                all,
                write,
                tighten: reldir_core::infer::Tighten::parse(&tighten).map_err(DbError::usage)?,
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
        Command::Import {
            table,
            from,
            input_format,
            mode,
        } => command::transfer::import(
            context,
            sink,
            &table,
            command::transfer::ImportOptions {
                source: &from,
                format: input_format
                    .as_deref()
                    .map(command::transfer::Format::parse)
                    .transpose()?,
                mode: reldir_core::rows::ImportMode::parse(&mode)?,
            },
        ),
        Command::Export {
            table,
            out,
            file_format,
            filter,
            order,
        } => command::transfer::export(
            context,
            sink,
            &table,
            command::transfer::ExportOptions {
                out: out.as_deref(),
                format: file_format
                    .as_deref()
                    .map(command::transfer::Format::parse)
                    .transpose()?,
                filter: filter.as_deref(),
                order: order.as_deref(),
            },
        ),
        Command::Diff {
            table,
            from,
            to,
            git,
            schema,
        } => history::diff(
            context,
            sink,
            history::DiffOptions {
                revisions: from.zip(to),
                git,
                table,
                schema_only: schema,
            },
        ),
        Command::Log { limit } => history::log(context, sink, limit),
        Command::Show { revision } => history::show(context, sink, revision),
        Command::Restore { paths, at } => history::restore(context, sink, &paths, at.as_deref()),
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
        Command::Git(GitCommand::Setup { hooks }) => command::git::setup(context, sink, hooks),
        Command::Git(GitCommand::MergeFile {
            base,
            ours,
            theirs,
            path,
            labels,
            marker_size,
        }) => command::git::merge_file(
            sink,
            command::git::MergeFile {
                base: &base,
                ours: &ours,
                theirs: &theirs,
                path: path.as_deref(),
                labels: &labels,
                marker_size,
            },
        ),
        Command::Git(GitCommand::Textconv { file }) => command::git::textconv(sink, &file),
        Command::Recover { history: mode } => history::recover(context, sink, mode.is_some()),
        Command::Gc => history::gc(context, sink),
        Command::Serve => crate::serve::serve(context, sink),
        Command::Analyze => history::analyze(context, sink),
        Command::Shell | Command::Mcp | Command::Completions { .. } => Err(DbError::usage(
            "this command runs interactively and has no result to report",
        )),
    }
}

fn operation(command: MigrateCommand) -> Result<Operation> {
    let json = |text: &str, what: &str| {
        reldir_core::json::parse_str(text)
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
            values,
            drop_constraints,
        } => Operation::ChangeType {
            table,
            column,
            kind,
            using,
            values,
            drop_constraints,
        },
        MigrateCommand::ChangeIdentity { table, identity } => Operation::ChangeIdentity {
            table,
            identity: json(&identity, "the identity")?,
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
#[cfg(test)]
mod tests {
    use super::*;

    /// Words of a shell command line: spaces separate them, and single or
    /// double quotes (with `\"` inside double quotes) keep them together.
    fn shell_words(line: &str) -> Vec<String> {
        let mut words = vec![];
        let mut word = String::new();
        let mut quote: Option<char> = None;
        let mut started = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match (quote, c) {
                (None, ' ') => {
                    if started {
                        words.push(std::mem::take(&mut word));
                        started = false;
                    }
                }
                (None, '\'' | '"') => {
                    quote = Some(c);
                    started = true;
                }
                (Some('"'), '\\') if chars.peek() == Some(&'"') => {
                    word.push(chars.next().expect("peeked"));
                }
                (Some(open), c) if c == open => quote = None,
                (_, c) => {
                    word.push(c);
                    started = true;
                }
            }
        }
        if started {
            words.push(word);
        }
        words
    }

    /// Every example a command's help shows is a command line reldir accepts.
    #[test]
    fn test2232_every_example_in_the_help_parses() {
        fn walk(command: &clap::Command, examples: &mut Vec<String>) {
            if let Some(text) = command.get_after_help() {
                for line in text.to_string().lines() {
                    let line = line.trim().trim_start_matches("Example:").trim();
                    if let Some(rest) = line.strip_prefix("reldir ") {
                        let rest = rest.split(" > ").next().unwrap_or(rest);
                        let rest = rest.split("  #").next().unwrap_or(rest);
                        examples.push(rest.to_string());
                    }
                }
            }
            for sub in command.get_subcommands() {
                walk(sub, examples);
            }
        }
        let mut examples = vec![];
        walk(&Cli::command(), &mut examples);
        assert!(examples.len() > 40, "found {}", examples.len());
        let refused: Vec<String> = examples
            .iter()
            .filter(|example| {
                let mut words = vec!["reldir".to_string()];
                words.extend(shell_words(example));
                Cli::try_parse_from(words).is_err()
            })
            .cloned()
            .collect();
        assert!(refused.is_empty(), "examples reldir refuses: {refused:#?}");
    }

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
                // A command a person runs shows how to run it.
                let leaf = sub.get_subcommands().next().is_none();
                let shows = sub
                    .get_after_help()
                    .is_some_and(|text| text.to_string().contains("Example"));
                if leaf && !shows {
                    missing.push(format!("{path} {} (example)", sub.get_name()));
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
