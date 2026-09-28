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

use crate::diagnostic::{DbError, Diagnostic, Result, Severity};
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
            color: io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()),
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
pub struct Progress {
    label: &'static str,
    total: usize,
    scanned: usize,
    started: std::time::Instant,
    active: bool,
    enabled: bool,
    immediate: bool,
}

const PROGRESS_AFTER: std::time::Duration = std::time::Duration::from_secs(1);

impl Progress {
    pub fn new(label: &'static str, total: usize) -> Self {
        let settings = presentation();
        Self {
            label,
            total,
            scanned: 0,
            started: std::time::Instant::now(),
            active: false,
            enabled: !settings.quiet && io::stderr().is_terminal(),
            immediate: settings.verbose,
        }
    }

    pub fn advance(&mut self) {
        self.scanned += 1;
        if !self.enabled || (!self.immediate && self.started.elapsed() < PROGRESS_AFTER) {
            return;
        }
        self.active = true;
        eprint!("\r\u{1b}[2K{}: {} / {} files", self.label, self.scanned, self.total);
        let _ = io::stderr().flush();
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if self.active {
            eprint!("\r\u{1b}[2K");
            let _ = io::stderr().flush();
        }
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
    if presentation().color { (BOLD, RESET) } else { ("", "") }
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

/// Where a command puts what it produces.
pub trait Sink {
    /// A data record: a row, a table, a revision. Without a `kind` member it
    /// is a row.
    fn record(&mut self, record: Map<String, Value>) -> Result<()>;
    fn diagnostic(&mut self, diagnostic: &Diagnostic) -> Result<()>;
    /// Something the command did on its own: a recovery, a recorded revision.
    fn event(&mut self, kind: &str, detail: Value, prose: &str) -> Result<()>;
    /// Prose for a person, which machine formats omit.
    fn note(&mut self, text: &str);
    /// A document for a person to read, printed as-is in table format; machine
    /// formats carry it as a record.
    fn document(&mut self, kind: &str, value: Value) -> Result<()>;
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
        Self { exit: 0, summary: summary.into(), fields: Map::new() }
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

/// The `command_result` envelope.
pub fn envelope(
    command: &str,
    outcome: &std::result::Result<Finish, DbError>,
    records: Vec<Value>,
    diagnostics: Vec<Value>,
    events: Vec<Value>,
) -> Value {
    let mut out = Map::new();
    out.insert("kind".into(), json!("command_result"));
    out.insert("command".into(), json!(command));
    match outcome {
        Ok(finish) => {
            out.insert("ok".into(), json!(finish.exit == 0));
            out.insert("exit".into(), json!(finish.exit));
            out.insert("summary".into(), json!(finish.summary));
            for (key, value) in &finish.fields {
                out.insert(key.clone(), value.clone());
            }
        }
        Err(error) => {
            out.insert("ok".into(), json!(false));
            out.insert("exit".into(), json!(error.exit));
            out.insert("summary".into(), json!(error.diagnostic.message));
            out.insert(
                "error".into(),
                serde_json::to_value(&*error.diagnostic).unwrap_or(Value::Null),
            );
        }
    }
    out.insert("records".into(), Value::Array(records));
    let mut all = diagnostics;
    if let Err(error) = outcome {
        all.extend(error.related.iter().filter_map(|d| serde_json::to_value(d).ok()));
    }
    out.insert("diagnostics".into(), Value::Array(all));
    out.insert("events".into(), Value::Array(events));
    Value::Object(out)
}

fn with_kind(mut record: Map<String, Value>, kind: &str) -> Map<String, Value> {
    if !record.contains_key("kind") {
        record.insert("kind".into(), json!(kind));
    }
    record
}

fn io_error(error: impl std::fmt::Display) -> DbError {
    DbError::new("IO_ERROR", format!("cannot write output: {error}"), 6)
}

fn write_line(value: &Value) -> Result<()> {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, value).map_err(io_error)?;
    lock.write_all(b"\n").map_err(io_error)
}

/// A sink that writes to the terminal in one format.
pub struct Terminal {
    format: Format,
    command: String,
    records: Vec<Value>,
    table: Vec<Map<String, Value>>,
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
            Err(error) => error.exit,
        };
        let printed = (|| -> Result<()> {
            match self.format {
                Format::Json => {
                    let value = envelope(&self.command, &outcome, self.records, self.diagnostics, self.events);
                    let stdout = io::stdout();
                    let mut lock = stdout.lock();
                    serde_json::to_writer_pretty(&mut lock, &value).map_err(io_error)?;
                    lock.write_all(b"\n").map_err(io_error)?;
                }
                Format::Jsonl => {
                    if let Err(error) = &outcome {
                        for diagnostic in error.diagnostics() {
                            write_line(&serde_json::to_value(&diagnostic).map_err(io_error)?)?;
                        }
                    }
                    let mut value = envelope(&self.command, &outcome, vec![], vec![], vec![]);
                    if let Value::Object(map) = &mut value {
                        map.remove("records");
                        map.remove("diagnostics");
                        map.remove("events");
                    }
                    write_line(&value)?;
                }
                Format::Csv => {
                    if let Some((mut writer, _)) = self.csv.take() {
                        writer.flush().map_err(io_error)?;
                    }
                    if let Err(error) = &outcome {
                        error.render_human();
                    }
                }
                Format::Sarif => {
                    let mut diagnostics = self.sarif.clone();
                    if let Err(error) = &outcome {
                        diagnostics.extend(error.diagnostics());
                    }
                    let log = sarif(&diagnostics);
                    let stdout = io::stdout();
                    let mut lock = stdout.lock();
                    serde_json::to_writer_pretty(&mut lock, &log).map_err(io_error)?;
                    lock.write_all(b"\n").map_err(io_error)?;
                }
                Format::Table => {
                    if !self.table.is_empty() {
                        table(&self.table);
                    }
                    match &outcome {
                        Ok(finish) => {
                            if !presentation().quiet && !finish.summary.is_empty() {
                                println!("{}", finish.summary);
                            }
                        }
                        Err(error) => error.render_human(),
                    }
                }
            }
            Ok(())
        })();
        match printed {
            Ok(()) => exit,
            Err(error) => {
                error.render_human();
                error.exit
            }
        }
    }
}

impl Sink for Terminal {
    fn record(&mut self, record: Map<String, Value>) -> Result<()> {
        match self.format {
            Format::Json => self.records.push(Value::Object(with_kind(record, "row"))),
            Format::Jsonl => write_line(&Value::Object(with_kind(record, "row")))?,
            Format::Table => self.table.push(record),
            Format::Csv => {
                if self.csv.is_none() {
                    let headers: Vec<String> = record.keys().filter(|key| *key != "kind").cloned().collect();
                    let mut writer = csv::Writer::from_writer(io::stdout());
                    writer.write_record(&headers).map_err(io_error)?;
                    self.csv = Some((writer, headers));
                }
                let (writer, headers) = self.csv.as_mut().expect("set above");
                if let Some(extra) = record.keys().find(|key| *key != "kind" && !headers.contains(key)) {
                    return Err(DbError::usage(format!(
                        "records do not share columns ({extra:?} is not among the first record's), so they \
                         cannot be one CSV table; use --format jsonl"
                    )));
                }
                writer
                    .write_record(headers.iter().map(|header| csv_cell(record.get(header).unwrap_or(&Value::Null))))
                    .map_err(io_error)?;
            }
            Format::Sarif => {}
        }
        Ok(())
    }

    fn diagnostic(&mut self, diagnostic: &Diagnostic) -> Result<()> {
        match self.format {
            Format::Json => self.diagnostics.push(serde_json::to_value(diagnostic).map_err(io_error)?),
            Format::Jsonl => write_line(&serde_json::to_value(diagnostic).map_err(io_error)?)?,
            Format::Sarif => self.sarif.push(diagnostic.clone()),
            Format::Table | Format::Csv => crate::diagnostic::render_human(diagnostic),
        }
        Ok(())
    }

    fn event(&mut self, kind: &str, detail: Value, prose: &str) -> Result<()> {
        let mut record = Map::new();
        record.insert("kind".into(), json!(kind));
        if let Value::Object(members) = detail {
            record.extend(members);
        }
        match self.format {
            Format::Json => self.events.push(Value::Object(record)),
            Format::Jsonl => write_line(&Value::Object(record))?,
            Format::Table | Format::Csv | Format::Sarif => notice_stderr(prose),
        }
        Ok(())
    }

    fn note(&mut self, text: &str) {
        if !self.format.is_machine() {
            notice_stderr(text);
        }
    }

    fn document(&mut self, kind: &str, value: Value) -> Result<()> {
        match self.format {
            Format::Table => {
                println!("{}", serde_json::to_string_pretty(&value).map_err(io_error)?);
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
        envelope(command, outcome, self.records, self.diagnostics, self.events)
    }
}

impl Sink for Collect {
    fn record(&mut self, record: Map<String, Value>) -> Result<()> {
        self.records.push(Value::Object(with_kind(record, "row")));
        Ok(())
    }
    fn diagnostic(&mut self, diagnostic: &Diagnostic) -> Result<()> {
        self.diagnostics.push(serde_json::to_value(diagnostic).map_err(io_error)?);
        Ok(())
    }
    fn event(&mut self, kind: &str, detail: Value, _prose: &str) -> Result<()> {
        let mut record = Map::new();
        record.insert("kind".into(), json!(kind));
        if let Value::Object(members) = detail {
            record.extend(members);
        }
        self.events.push(Value::Object(record));
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
    let shared_kind = rows.windows(2).all(|pair| pair[0].get("kind") == pair[1].get("kind"));
    let mut headers: Vec<String> = vec![];
    for row in rows {
        for key in row.keys() {
            if !(shared_kind && key == "kind") && !headers.contains(key) {
                headers.push(key.clone());
            }
        }
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| headers.iter().map(|header| table_cell(row.get(header).unwrap_or(&Value::Null))).collect())
        .collect();
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            cells.iter().map(|row| row[index].chars().count()).chain([header.chars().count()]).max().unwrap_or(0)
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
    let _ = writeln!(out, "{}", widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("-+-"));
    for row in &cells {
        let _ = writeln!(out, "{}", line(row));
    }
    let _ = writeln!(out, "({} row{})", rows.len(), if rows.len() == 1 { "" } else { "s" });
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
    let mut rules: Vec<String> = diagnostics.iter().map(|d| d.code.clone()).collect();
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
                "version": crate::VERSION,
                "informationUri": "https://jowharshamshiri.github.io/reldir/",
                "rules": rules.iter().map(|code| json!({
                    "id": code,
                    "helpUri": format!("https://jowharshamshiri.github.io/reldir/errors#{}", code.to_lowercase()),
                })).collect::<Vec<_>>(),
            }},
            "results": results,
        }]
    })
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
            assert_eq!(Format::parse(invalid).unwrap_err().diagnostic.code, "USAGE");
        }
    }

    #[test]
    fn test2210_the_envelope_carries_success_and_failure_alike() {
        let ok = envelope("sql", &Ok(Finish::ok("2 rows").with("database_valid", true)), vec![json!({"id": 1})], vec![], vec![]);
        assert_eq!(ok["kind"], "command_result");
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["database_valid"], true);
        assert_eq!(ok["records"][0]["id"], 1);
        let error = DbError::new("UNKNOWN_TABLE", "unknown table", 4)
            .with_related(vec![Diagnostic::error("UNKNOWN_COLUMN", "also")]);
        let failed = envelope("sql", &Err(error), vec![], vec![], vec![]);
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["exit"], 4);
        assert_eq!(failed["error"]["code"], "UNKNOWN_TABLE");
        assert_eq!(failed["diagnostics"][0]["code"], "UNKNOWN_COLUMN", "every fault is carried");
    }

    #[test]
    fn test2211_sarif_locates_each_result() {
        let diagnostic = Diagnostic::error("FOREIGN_KEY_VIOLATION", "dangling")
            .at("posts/p1.json")
            .fix("FIX_RESTORE_TARGET");
        let mut located = diagnostic.clone();
        located.location = Some(crate::diagnostic::Location { line: 3, column: 14 });
        let log = sarif(&[located]);
        let result = &log["runs"][0]["results"][0];
        assert_eq!(result["ruleId"], "FOREIGN_KEY_VIOLATION");
        assert_eq!(result["level"], "error");
        assert_eq!(result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"], "posts/p1.json");
        assert_eq!(result["locations"][0]["physicalLocation"]["region"]["startLine"], 3);
        assert_eq!(log["version"], "2.1.0");
    }
}
