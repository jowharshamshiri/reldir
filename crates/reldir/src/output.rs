//! Rendering what a command produced.
//!
//! Commands never print. They hand what they produce to a [`Sink`] -- data
//! records, diagnostics, events -- and finish with a [`Finish`]. This module
//! turns that one description into each output format:
//!
//! - `table`: records as an aligned table on stdout, diagnostics
//!   compiler-style on stderr, events and notes as prose on stderr;
//! - `json`: one `command_result` envelope holding everything;
//! - `jsonl`: every record, diagnostic and event as its own line as it is
//!   produced, then the `command_result` line;
//! - `csv`: records only, streamed;
//! - `sarif`: diagnostics as a SARIF 2.1.0 log, for code-scanning tools.
//!
//! The MCP server uses the same envelope, so an agent and a script read one
//! contract.

use reldir_core::code::Code;
use reldir_core::diagnostic::{DbError, Diagnostic, Result, Severity};
use serde_json::{Map, Value, json};
use std::io::{self, IsTerminal, Write};
use std::sync::OnceLock;

/// Presentation settings shared by every human-facing writer. Process-wide
/// because notices come from layers that never see the command line.
#[derive(Debug, Clone, Copy)]
pub struct Presentation {
    /// Colour human diagnostics: on a terminal, unless `--no-color` or a
    /// non-empty `NO_COLOR` says otherwise.
    pub color: bool,
    /// Suppress informational prose. Diagnostics, results and exit codes are
    /// never suppressed.
    pub quiet: bool,
    /// Report progress from the first file rather than after a second.
    pub verbose: bool,
}

impl Default for Presentation {
    fn default() -> Self {
        Self {
            color: io::stderr().is_terminal()
                && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()),
            quiet: false,
            verbose: false,
        }
    }
}

static PRESENTATION: OnceLock<Presentation> = OnceLock::new();

/// Settle the presentation for this process; later calls are ignored.
pub fn set_presentation(presentation: Presentation) {
    let _ = PRESENTATION.set(presentation);
}

pub fn presentation() -> Presentation {
    *PRESENTATION.get_or_init(Presentation::default)
}

/// One line of prose on stderr, unless `--quiet`.
pub fn notice_stderr(text: &str) {
    if !presentation().quiet {
        eprintln!("{text}");
    }
}

/// A progress line for an operation over many files. Silent for the first
/// second, off a terminal, and under `--quiet`; erased when dropped.
/// Draws a progress line on a terminal's stderr for long engine operations,
/// once an operation has run for a second (immediately under `--verbose`), and
/// clears it when the operation ends.
pub struct TerminalProgress {
    state: std::sync::Mutex<ProgressLine>,
}

#[derive(Default)]
struct ProgressLine {
    started: Option<std::time::Instant>,
    active: bool,
}

const PROGRESS_AFTER: std::time::Duration = std::time::Duration::from_secs(1);

impl TerminalProgress {
    /// A reporter for this process's presentation settings, or none when the
    /// settings or the terminal say progress must not be drawn.
    pub fn for_terminal() -> Option<Self> {
        let settings = presentation();
        (!settings.quiet && io::stderr().is_terminal()).then(|| Self {
            state: std::sync::Mutex::new(ProgressLine::default()),
        })
    }
}

impl reldir_core::progress::Reporter for TerminalProgress {
    fn begin(&self, _label: &'static str, _total: usize) {
        let mut line = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        line.started = Some(std::time::Instant::now());
    }

    fn advance(&self, label: &'static str, done: usize, total: usize) {
        let mut line = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let waited = line
            .started
            .is_none_or(|started| started.elapsed() >= PROGRESS_AFTER);
        if !presentation().verbose && !waited {
            return;
        }
        line.active = true;
        eprint!("\r\u{1b}[2K{label}: {done} / {total} files");
        let _ = io::stderr().flush();
    }

    fn end(&self, _label: &'static str) {
        let mut line = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if line.active {
            eprint!("\r\u{1b}[2K");
            let _ = io::stderr().flush();
        }
        *line = ProgressLine::default();
    }
}

const RED: &str = "\u{1b}[31m";
const YELLOW: &str = "\u{1b}[33m";
const CYAN: &str = "\u{1b}[36m";
const BOLD: &str = "\u{1b}[1m";
const RESET: &str = "\u{1b}[0m";

pub fn severity_style(severity: &Severity) -> (&'static str, &'static str) {
    if !presentation().color {
        return ("", "");
    }
    let colour = match severity {
        Severity::Error => RED,
        Severity::Warning => YELLOW,
        Severity::Suggestion | Severity::Info => CYAN,
    };
    (colour, RESET)
}

pub fn emphasis() -> (&'static str, &'static str) {
    if presentation().color {
        (BOLD, RESET)
    } else {
        ("", "")
    }
}

pub fn severity_label(severity: &Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Suggestion => "suggestion",
        Severity::Info => "info",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Table,
    Json,
    Jsonl,
    Csv,
    Sarif,
}

impl Format {
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "table" => Ok(Self::Table),
            "json" => Ok(Self::Json),
            "jsonl" => Ok(Self::Jsonl),
            "csv" => Ok(Self::Csv),
            "sarif" => Ok(Self::Sarif),
            _ => Err(DbError::usage(format!(
                "unknown output format {text:?}; the formats are table, json, jsonl, csv and sarif"
            ))),
        }
    }

    pub fn is_machine(self) -> bool {
        !matches!(self, Self::Table)
    }
}

/// The version of the machine protocol: the shape of every record, event,
/// diagnostic and result `--format json` and `--format jsonl` print. It
/// changes only when a shape does, and then only as a new version.
pub const PROTOCOL: u32 = 1;

/// Where a command puts what it produces.
pub trait Sink {
    /// A row of a table, exactly as the database holds it. Machine formats
    /// nest it -- `{"type": "row", "row": {...}}` -- so no column name can
    /// ever be mistaken for a member of the protocol.
    fn row(&mut self, row: Map<String, Value>) -> Result<()>;
    /// A record of a kind reldir defines -- a table, a revision, a fix --
    /// named by its `kind` member.
    fn record(&mut self, record: Map<String, Value>) -> Result<()>;
    fn diagnostic(&mut self, diagnostic: &Diagnostic) -> Result<()>;
    /// Something the command did on its own: a recovery, a recorded revision.
    fn event(&mut self, kind: &str, detail: Value, prose: &str) -> Result<()>;
    /// Prose for a person, which machine formats omit.
    fn note(&mut self, text: &str);
    /// A document for a person to read, printed as-is in table format; machine
    /// formats carry it as a record.
    fn document(&mut self, kind: &str, value: Value) -> Result<()>;
    /// A description of something, such as a table: in table format its
    /// members that are lists of objects are tables of their own, and the
    /// rest one record; machine formats carry it as a document.
    fn description(&mut self, kind: &str, value: Value) -> Result<()> {
        self.document(kind, value)
    }
    /// What one statement of a script did, after the rows it returned.
    fn statement(&mut self, record: Map<String, Value>) -> Result<()> {
        self.record(record)
    }
    /// Prose to read -- a page of the documentation -- printed as it is in
    /// table format; machine formats carry it as a record naming it.
    fn text(&mut self, kind: &str, topic: &str, text: &str) -> Result<()> {
        let mut record = Map::new();
        record.insert("kind".into(), json!(kind));
        record.insert("topic".into(), json!(topic));
        record.insert("text".into(), json!(text));
        self.record(record)
    }
}

/// How a command ended.
#[derive(Debug, Clone)]
pub struct Finish {
    pub exit: i32,
    /// One line for a person.
    pub summary: String,
    /// Members the envelope carries at its top level: `valid`, `state`,
    /// `revision`, counts.
    pub fields: Map<String, Value>,
}

impl Finish {
    pub fn ok(summary: impl Into<String>) -> Self {
        Self {
            exit: 0,
            summary: summary.into(),
            fields: Map::new(),
        }
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.fields.insert(key.to_string(), value.into());
        self
    }

    pub fn exit(mut self, exit: i32) -> Self {
        self.exit = exit;
        self
    }
}

/// The result: the last thing a command prints, saying how it ended.
pub fn envelope(
    command: &str,
    outcome: &std::result::Result<Finish, DbError>,
    records: Vec<Value>,
    diagnostics: Vec<Value>,
    events: Vec<Value>,
) -> Value {
    let mut out = Map::new();
    out.insert("type".into(), json!("result"));
    out.insert("protocol".into(), json!(PROTOCOL));
    out.insert("reldir".into(), json!(reldir_core::VERSION));
    out.insert("command".into(), json!(command));
    match outcome {
        Ok(finish) => {
            out.insert("ok".into(), json!(finish.exit == 0));
            out.insert("exit".into(), json!(finish.exit));
            out.insert("summary".into(), json!(finish.summary));
            out.insert("data".into(), Value::Object(finish.fields.clone()));
        }
        Err(error) => {
            out.insert("ok".into(), json!(false));
            out.insert("exit".into(), json!(error.exit().code()));
            out.insert("summary".into(), json!(error.diagnostic.message));
            out.insert("data".into(), json!({}));
            out.insert(
                "error".into(),
                serde_json::to_value(&*error.diagnostic).unwrap_or(Value::Null),
            );
        }
    }
    out.insert("records".into(), Value::Array(records));
    let mut all = diagnostics;
    if let Err(error) = outcome {
        all.extend(
            error
                .related
                .iter()
                .filter_map(|d| serde_json::to_value(d).ok()),
        );
    }
    out.insert("diagnostics".into(), Value::Array(all));
    out.insert("events".into(), Value::Array(events));
    Value::Object(out)
}

/// Report a failure that ended a command before it produced anything: a
/// refused argument, a defect. Machine formats get a result, as every
/// command's output ends with one; a person gets the error.
pub fn report_failure(format: Format, command: &str, error: &DbError) {
    let copy =
        || Err(DbError::from_diag((*error.diagnostic).clone()).with_related(error.related.clone()));
    match format {
        Format::Json => {
            let value = envelope(command, &copy(), vec![], vec![], vec![]);
            let _ = serde_json::to_writer_pretty(io::stdout(), &value);
            println!();
        }
        Format::Jsonl => {
            let mut value = envelope(command, &copy(), vec![], vec![], vec![]);
            if let Value::Object(map) = &mut value {
                map.shift_remove("records");
                map.shift_remove("diagnostics");
                map.shift_remove("events");
            }
            let _ = write_line(&value);
        }
        Format::Table | Format::Csv | Format::Sarif => render_error(error),
    }
}

/// A row, as the machine protocol carries it.
fn row_record(row: Map<String, Value>) -> Value {
    json!({"type": "row", "row": Value::Object(row)})
}

/// A record of a kind reldir defines, as the machine protocol carries it: its
/// `kind` member becomes its `type`.
fn typed_record(mut record: Map<String, Value>) -> Result<Value> {
    let kind = match record.shift_remove("kind") {
        Some(Value::String(kind)) => kind,
        _ => {
            return Err(DbError::new(
                Code::INTERNAL_ERROR,
                "a record was given without saying what kind it is",
            ));
        }
    };
    let mut out = Map::new();
    out.insert("type".into(), json!(kind));
    out.extend(record);
    Ok(Value::Object(out))
}

/// An event, as the machine protocol carries it.
fn event_record(kind: &str, detail: Value) -> Value {
    let mut out = Map::new();
    out.insert("type".into(), json!("event"));
    out.insert("event".into(), json!(kind));
    if let Value::Object(members) = detail {
        out.extend(members);
    }
    Value::Object(out)
}

fn io_error(error: impl std::fmt::Display) -> DbError {
    DbError::new(Code::IO_ERROR, format!("cannot write output: {error}"))
}

/// Whether whoever reads the output stopped reading.
static OUTPUT_CLOSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A reader that stopped reading -- `reldir ... | head` -- has what it asked
/// for: nothing more is printed. The command itself runs to the end -- a
/// change it was making is still made -- and exits with its own status.
fn closed(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        OUTPUT_CLOSED.store(true, std::sync::atomic::Ordering::Relaxed);
        return true;
    }
    false
}

/// Write to stdout, unless its reader has gone.
fn print(write: impl FnOnce(&mut io::StdoutLock<'_>) -> std::io::Result<()>) -> Result<()> {
    if OUTPUT_CLOSED.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    match write(&mut lock).and_then(|()| lock.flush()) {
        Ok(()) => Ok(()),
        Err(error) if closed(&error) => Ok(()),
        Err(error) => Err(io_error(error)),
    }
}

fn write_line(value: &Value) -> Result<()> {
    print(|out| {
        serde_json::to_writer(&mut *out, value).map_err(std::io::Error::from)?;
        out.write_all(b"\n")
    })
}

/// A sink that writes to the terminal in one format.
pub struct Terminal {
    format: Format,
    command: String,
    records: Vec<Value>,
    /// What table format prints, by kind: `row` for rows, numbered by the
    /// statement of a script that returned them.
    table: Vec<(String, Map<String, Value>)>,
    /// How many statements of a script have ended.
    statements: usize,
    diagnostics: Vec<Value>,
    sarif: Vec<Diagnostic>,
    events: Vec<Value>,
    csv: Option<(csv::Writer<io::Stdout>, Vec<String>)>,
}

impl Terminal {
    pub fn new(format: Format, command: &str) -> Self {
        Self {
            format,
            command: command.to_string(),
            records: vec![],
            table: vec![],
            statements: 0,
            diagnostics: vec![],
            sarif: vec![],
            events: vec![],
            csv: None,
        }
    }

    /// Print the end of the command's output and return its exit status.
    pub fn finish(mut self, outcome: std::result::Result<Finish, DbError>) -> i32 {
        let exit = match &outcome {
            Ok(finish) => finish.exit,
            Err(error) => error.exit().code(),
        };
        let printed = (|| -> Result<()> {
            match self.format {
                Format::Json => {
                    let value = envelope(
                        &self.command,
                        &outcome,
                        self.records,
                        self.diagnostics,
                        self.events,
                    );
                    print(|out| {
                        serde_json::to_writer_pretty(&mut *out, &value)
                            .map_err(std::io::Error::from)?;
                        out.write_all(b"\n")
                    })?;
                }
                Format::Jsonl => {
                    if let Err(error) = &outcome {
                        for diagnostic in error.diagnostics() {
                            write_line(&serde_json::to_value(&diagnostic).map_err(io_error)?)?;
                        }
                    }
                    let mut value = envelope(&self.command, &outcome, vec![], vec![], vec![]);
                    if let Value::Object(map) = &mut value {
                        map.shift_remove("records");
                        map.shift_remove("diagnostics");
                        map.shift_remove("events");
                    }
                    write_line(&value)?;
                }
                Format::Csv => {
                    if let Some((mut writer, _)) = self.csv.take()
                        && let Err(error) = writer.flush()
                        && !closed(&error)
                    {
                        return Err(io_error(error));
                    }
                    if let Err(error) = &outcome {
                        render_error(error);
                    }
                }
                Format::Sarif => {
                    let mut diagnostics = self.sarif.clone();
                    if let Err(error) = &outcome {
                        diagnostics.extend(error.diagnostics());
                    }
                    let log = sarif(&diagnostics);
                    print(|out| {
                        serde_json::to_writer_pretty(&mut *out, &log)
                            .map_err(std::io::Error::from)?;
                        out.write_all(b"\n")
                    })?;
                }
                Format::Table => {
                    // One table per kind of record, in the order the kinds
                    // first appear: a fix and a file change have nothing in
                    // common to align.
                    let mut kinds: Vec<&String> = vec![];
                    for (kind, _) in &self.table {
                        if !kinds.contains(&kind) {
                            kinds.push(kind);
                        }
                    }
                    for (index, kind) in kinds.iter().enumerate() {
                        let group: Vec<Map<String, Value>> = self
                            .table
                            .iter()
                            .filter(|(of, _)| of == *kind)
                            .map(|(_, fields)| fields.clone())
                            .collect();
                        if index > 0 {
                            println!();
                        }
                        table(&group);
                    }
                    match &outcome {
                        Ok(finish) => {
                            if !presentation().quiet && !finish.summary.is_empty() {
                                println!("{}", finish.summary);
                            }
                        }
                        Err(error) => render_error(error),
                    }
                }
            }
            Ok(())
        })();
        match printed {
            Ok(()) => exit,
            Err(error) => {
                render_error(&error);
                error.exit().code()
            }
        }
    }
}

impl Terminal {
    /// One line of CSV: every line shares the first line's columns.
    fn csv_line(&mut self, fields: &Map<String, Value>) -> Result<()> {
        if self.csv.is_none() {
            let headers: Vec<String> = fields.keys().cloned().collect();
            let mut writer = csv::Writer::from_writer(io::stdout());
            writer.write_record(&headers).map_err(io_error)?;
            self.csv = Some((writer, headers));
        }
        if OUTPUT_CLOSED.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        let (writer, headers) = self.csv.as_mut().expect("set above");
        if let Some(extra) = fields.keys().find(|key| !headers.contains(key)) {
            return Err(DbError::usage(format!(
                "records do not share columns ({extra:?} is not among the first record's), so they \
                 cannot be one CSV table; use --format jsonl"
            )));
        }
        writer
            .write_record(
                headers
                    .iter()
                    .map(|header| csv_cell(fields.get(header).unwrap_or(&Value::Null))),
            )
            .or_else(|error| match error.kind() {
                csv::ErrorKind::Io(io) if closed(io) => Ok(()),
                _ => Err(io_error(error)),
            })
    }
}

impl Sink for Terminal {
    fn row(&mut self, row: Map<String, Value>) -> Result<()> {
        match self.format {
            Format::Json => self.records.push(row_record(row)),
            Format::Jsonl => write_line(&row_record(row))?,
            Format::Table => self.table.push((format!("row {}", self.statements), row)),
            Format::Csv => self.csv_line(&row)?,
            Format::Sarif => {}
        }
        Ok(())
    }

    fn statement(&mut self, record: Map<String, Value>) -> Result<()> {
        match self.format {
            Format::Json | Format::Jsonl => self.record(record),
            // A table, or CSV, is the rows; each statement's rows are a table
            // of their own.
            Format::Table | Format::Csv | Format::Sarif => {
                self.statements += 1;
                Ok(())
            }
        }
    }

    fn record(&mut self, record: Map<String, Value>) -> Result<()> {
        let Value::Object(mut typed) = typed_record(record)? else {
            unreachable!("a typed record is an object");
        };
        match self.format {
            Format::Json => self.records.push(Value::Object(typed)),
            Format::Jsonl => write_line(&Value::Object(typed))?,
            Format::Table | Format::Csv => {
                let kind = match typed.shift_remove("type") {
                    Some(Value::String(kind)) => kind,
                    _ => unreachable!("a typed record has a type"),
                };
                if self.format == Format::Table {
                    for value in typed.values_mut() {
                        if let Some(text) = revision_names(value) {
                            *value = Value::String(text);
                        }
                    }
                    self.table.push((kind, typed));
                } else {
                    self.csv_line(&typed)?;
                }
            }
            Format::Sarif => {}
        }
        Ok(())
    }

    fn diagnostic(&mut self, diagnostic: &Diagnostic) -> Result<()> {
        match self.format {
            Format::Json => self
                .diagnostics
                .push(serde_json::to_value(diagnostic).map_err(io_error)?),
            Format::Jsonl => write_line(&serde_json::to_value(diagnostic).map_err(io_error)?)?,
            Format::Sarif => self.sarif.push(diagnostic.clone()),
            Format::Table | Format::Csv => render_diagnostic(diagnostic),
        }
        Ok(())
    }

    fn event(&mut self, kind: &str, detail: Value, prose: &str) -> Result<()> {
        match self.format {
            Format::Json => self.events.push(event_record(kind, detail)),
            Format::Jsonl => write_line(&event_record(kind, detail))?,
            Format::Table | Format::Csv | Format::Sarif => notice_stderr(prose),
        }
        Ok(())
    }

    fn note(&mut self, text: &str) {
        if !self.format.is_machine() {
            notice_stderr(text);
        }
    }

    fn text(&mut self, kind: &str, topic: &str, text: &str) -> Result<()> {
        match self.format {
            Format::Table => print(|out| out.write_all(text.as_bytes())),
            _ => {
                let mut record = Map::new();
                record.insert("kind".into(), json!(kind));
                record.insert("topic".into(), json!(topic));
                record.insert("text".into(), json!(text));
                self.record(record)
            }
        }
    }

    fn description(&mut self, kind: &str, value: Value) -> Result<()> {
        let (Format::Table, Value::Object(members)) = (self.format, &value) else {
            return self.document(kind, value);
        };
        let mut head = Map::new();
        let mut lists = vec![];
        for (name, member) in members {
            match member {
                Value::Array(items) if items.iter().all(Value::is_object) => {
                    lists.push((name.clone(), items.clone()));
                }
                other => {
                    head.insert(name.clone(), other.clone());
                }
            }
        }
        self.table.push((kind.to_string(), head));
        for (name, items) in lists {
            for item in items {
                let Value::Object(fields) = item else {
                    unreachable!("only lists of objects are tables");
                };
                self.table.push((name.clone(), fields));
            }
        }
        Ok(())
    }

    fn document(&mut self, kind: &str, value: Value) -> Result<()> {
        match self.format {
            Format::Table => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).map_err(io_error)?
                );
                Ok(())
            }
            _ => {
                let mut record = Map::new();
                record.insert("kind".into(), json!(kind));
                record.insert("document".into(), value);
                self.record(record)
            }
        }
    }
}

/// A sink that keeps everything, for the MCP server and for tests.
#[derive(Default)]
pub struct Collect {
    pub records: Vec<Value>,
    pub diagnostics: Vec<Value>,
    pub events: Vec<Value>,
    pub notes: Vec<String>,
}

impl Collect {
    pub fn envelope(self, command: &str, outcome: &std::result::Result<Finish, DbError>) -> Value {
        envelope(
            command,
            outcome,
            self.records,
            self.diagnostics,
            self.events,
        )
    }
}

impl Sink for Collect {
    fn row(&mut self, row: Map<String, Value>) -> Result<()> {
        self.records.push(row_record(row));
        Ok(())
    }
    fn record(&mut self, record: Map<String, Value>) -> Result<()> {
        self.records.push(typed_record(record)?);
        Ok(())
    }
    fn diagnostic(&mut self, diagnostic: &Diagnostic) -> Result<()> {
        self.diagnostics
            .push(serde_json::to_value(diagnostic).map_err(io_error)?);
        Ok(())
    }
    fn event(&mut self, kind: &str, detail: Value, _prose: &str) -> Result<()> {
        self.events.push(event_record(kind, detail));
        Ok(())
    }
    fn note(&mut self, text: &str) {
        self.notes.push(text.to_string());
    }
    fn document(&mut self, kind: &str, value: Value) -> Result<()> {
        let mut record = Map::new();
        record.insert("kind".into(), json!(kind));
        record.insert("document".into(), value);
        self.record(record)
    }
}

/// Records as an aligned table. A `kind` column shared by every record says
/// nothing and is left out.
fn table(rows: &[Map<String, Value>]) {
    let mut headers: Vec<String> = vec![];
    for row in rows {
        for key in row.keys() {
            if !headers.contains(key) {
                headers.push(key.clone());
            }
        }
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            headers
                .iter()
                .map(|header| table_cell(row.get(header).unwrap_or(&Value::Null)))
                .collect()
        })
        .collect();
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            cells
                .iter()
                .map(|row| row[index].chars().count())
                .chain([header.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let line = |values: &[String]| -> String {
        values
            .iter()
            .zip(&widths)
            .map(|(value, width)| format!("{value:<width$}"))
            .collect::<Vec<_>>()
            .join(" | ")
            .trim_end()
            .to_string()
    };
    let _ = writeln!(out, "{}", line(&headers));
    let _ = writeln!(
        out,
        "{}",
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("-+-")
    );
    for row in &cells {
        let _ = writeln!(out, "{}", line(row));
    }
    let _ = writeln!(
        out,
        "({} row{})",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
}

/// A record's revision name -- `{"seq": 12, "id": "<64 hex>"}`, as the
/// protocol carries it -- or a list of them, as a terminal shows one:
/// `#12 (ab12cd34ef56)`. Only reldir's own records hold these; a row's values
/// are shown as they are.
fn revision_names(value: &Value) -> Option<String> {
    let name = |value: &Value| -> Option<String> {
        let Value::Object(members) = value else {
            return None;
        };
        let seq = members.get("seq")?.as_u64()?;
        let id = members.get("id")?.as_str()?;
        (members.len() == 2 && id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| format!("#{seq} ({})", &id[..12]))
    };
    match value {
        Value::Array(items) if !items.is_empty() => items
            .iter()
            .map(name)
            .collect::<Option<Vec<String>>>()
            .map(|names| names.join(", ")),
        other => name(other),
    }
}

fn table_cell(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::String(text) => text.replace(['\n', '\r', '\t'], " "),
        other => other.to_string(),
    }
}

fn csv_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Diagnostics as a SARIF 2.1.0 log.
pub fn sarif(diagnostics: &[Diagnostic]) -> Value {
    let mut rules: Vec<reldir_core::code::Code> = diagnostics.iter().map(|d| d.code).collect();
    rules.sort();
    rules.dedup();
    let results: Vec<Value> = diagnostics
        .iter()
        .map(|d| {
            let level = match d.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Suggestion | Severity::Info => "note",
            };
            let mut message = d.message.clone();
            if let Some(help) = &d.help {
                message.push_str(&format!(" ({help})"));
            }
            let mut result = json!({
                "ruleId": d.code,
                "level": level,
                "message": { "text": message },
            });
            if let Some(path) = &d.path {
                let mut physical = json!({ "artifactLocation": { "uri": path.to_string_lossy().replace('\\', "/") } });
                if let Some(location) = &d.location {
                    physical["region"] = json!({ "startLine": location.line, "startColumn": location.column });
                }
                result["locations"] = json!([{ "physicalLocation": physical }]);
            }
            if !d.fixes.is_empty() {
                result["properties"] = json!({ "fixes": d.fixes, "pointer": d.pointer });
            }
            result
        })
        .collect();
    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": { "driver": {
                "name": "reldir",
                "version": reldir_core::VERSION,
                "informationUri": "https://jowharshamshiri.github.io/reldir/",
                "rules": rules.iter().map(|code| json!({
                    "id": code,
                    "helpUri": format!("https://jowharshamshiri.github.io/reldir/errors#{}", code.as_str().to_lowercase()),
                })).collect::<Vec<_>>(),
            }},
            "results": results,
        }]
    })
}

/// Render one diagnostic compiler-style on stderr.
pub fn render_diagnostic(d: &Diagnostic) {
    {
        let (colour, reset) = severity_style(&d.severity);
        let (bold, bold_reset) = emphasis();
        eprintln!(
            "{colour}{}[{}]{reset}: {bold}{}{bold_reset}",
            severity_label(&d.severity),
            d.code,
            d.message
        );
        if let Some(p) = &d.path {
            if let Some(l) = &d.location {
                eprintln!("  --> {}:{}:{}", p.display(), l.line, l.column)
            } else {
                eprintln!("  --> {}", p.display());
            }
        }
        if let (Some(line), Some(loc)) = (&d.source_line, &d.location) {
            eprintln!(
                "   |\n{:>2} | {}\n   | {}^",
                loc.line,
                line,
                " ".repeat(loc.column.saturating_sub(1))
            );
        }
        if let Some(pointer) = &d.pointer
            && !pointer.is_empty()
        {
            eprintln!("   = at: {pointer}");
        }
        if let Some(c) = &d.constraint {
            eprintln!("   = constraint: {c}");
        }
        if let Some(v) = &d.expected {
            eprintln!("   = expected: {v}");
        }
        if let Some(v) = &d.observed {
            eprintln!("   = observed: {v}");
        }
        if !d.fixes.is_empty() {
            eprintln!("   = fixes: {}", d.fixes.join(", "));
        }
        if let Some(h) = &d.help {
            eprintln!("   = help: {h}");
        }
    }
}

/// Render an error and every fault behind it, compiler-style, on stderr.
pub fn render_error(error: &DbError) {
    render_diagnostic(&error.diagnostic);
    for related in &error.related {
        render_diagnostic(related);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test1073_severity_labels_are_stable() {
        assert_eq!(severity_label(&Severity::Error), "error");
        assert_eq!(severity_label(&Severity::Warning), "warning");
        assert_eq!(severity_label(&Severity::Suggestion), "suggestion");
        assert_eq!(severity_label(&Severity::Info), "info");
    }

    #[test]
    fn test1076_output_formats_are_a_closed_set() {
        for (text, expected) in [
            ("table", Format::Table),
            ("json", Format::Json),
            ("jsonl", Format::Jsonl),
            ("csv", Format::Csv),
            ("sarif", Format::Sarif),
        ] {
            assert_eq!(Format::parse(text).unwrap(), expected);
        }
        for invalid in ["", "JSON", "yaml", "sqlite", "table "] {
            assert_eq!(
                Format::parse(invalid).unwrap_err().diagnostic.code,
                Code::USAGE
            );
        }
    }

    #[test]
    fn test2210_the_envelope_carries_success_and_failure_alike() {
        let ok = envelope(
            "sql",
            &Ok(Finish::ok("2 rows").with("database_valid", true)),
            vec![row_record(
                json!({"id": 1, "type": "a column, not the protocol's"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )],
            vec![],
            vec![],
        );
        assert_eq!(ok["type"], "result");
        assert_eq!(ok["protocol"], PROTOCOL);
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["data"]["database_valid"], true);
        assert_eq!(ok["records"][0]["type"], "row");
        assert_eq!(
            ok["records"][0]["row"]["type"], "a column, not the protocol's",
            "a column named like a protocol member stays the row's own"
        );
        let error = DbError::new(Code::UNKNOWN_TABLE, "unknown table")
            .with_related(vec![Diagnostic::error(Code::UNKNOWN_COLUMN, "also")]);
        let failed = envelope("sql", &Err(error), vec![], vec![], vec![]);
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["exit"], 4);
        assert_eq!(failed["error"]["code"], "UNKNOWN_TABLE");
        assert_eq!(
            failed["diagnostics"][0]["code"], "UNKNOWN_COLUMN",
            "every fault is carried"
        );
    }

    #[test]
    fn test2211_sarif_locates_each_result() {
        let diagnostic = Diagnostic::error(Code::FOREIGN_KEY_VIOLATION, "dangling")
            .at("posts/p1.json")
            .fix("FIX_RESTORE_TARGET");
        let mut located = diagnostic.clone();
        located.location = Some(reldir_core::diagnostic::Location {
            line: 3,
            column: 14,
        });
        let log = sarif(&[located]);
        let result = &log["runs"][0]["results"][0];
        assert_eq!(result["ruleId"], "FOREIGN_KEY_VIOLATION");
        assert_eq!(result["level"], "error");
        assert_eq!(
            result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "posts/p1.json"
        );
        assert_eq!(
            result["locations"][0]["physicalLocation"]["region"]["startLine"],
            3
        );
        assert_eq!(log["version"], "2.1.0");
    }
}
