use crate::diagnostic::{DbError, Diagnostic, Result};
use serde_json::{Map, Value};
use std::io::{self, IsTerminal, Write};
use std::sync::OnceLock;

/// Presentation settings shared by every human-facing writer (Section 49).
///
/// These are process-wide because informational notices are emitted from layers
/// that never see the parsed command line -- derived-state rebuild notices in
/// `db`, staging notices in `transaction` -- and because `DbError::render_human`
/// runs from `main` after a command has already failed. Storing one immutable
/// value settled once at startup keeps a single source of truth rather than
/// threading a parameter through every call site.
#[derive(Debug, Clone, Copy)]
pub struct Presentation {
    /// Colour human diagnostics. Section 49 requires colour on a TTY, disabled
    /// by `--no-color` and by a non-empty `NO_COLOR` environment variable.
    pub color: bool,
    /// Suppress informational stdout and progress. Diagnostics, machine-readable
    /// output, and exit codes are never suppressed.
    pub quiet: bool,
    /// Emit progress for operations that exceed one second (Section 49).
    pub verbose: bool,
}

impl Default for Presentation {
    fn default() -> Self {
        // Before `run` settles the flags -- for example a usage error rejected
        // during argument parsing -- fall back to the environment alone.
        Self {
            color: io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
            quiet: false,
            verbose: false,
        }
    }
}

static PRESENTATION: OnceLock<Presentation> = OnceLock::new();

/// Settle the presentation for this process. Called once from `run`; later calls
/// are ignored so that the value can never change mid-command.
pub fn set_presentation(presentation: Presentation) {
    let _ = PRESENTATION.set(presentation);
}

pub fn presentation() -> Presentation {
    *PRESENTATION.get_or_init(Presentation::default)
}

/// Write one informational line to stdout unless `--quiet` is in effect.
/// Diagnostics never travel this path.
pub fn notice(text: &str) {
    if !presentation().quiet {
        println!("{text}");
    }
}

/// Write one informational notice to stderr unless `--quiet` is in effect.
/// Used for derived-state rebuild and recovery notices, which Section 48
/// requires the binary to report while leaving stdout free for results.
pub fn notice_stderr(text: &str) {
    if !presentation().quiet {
        eprintln!("{text}");
    }
}

const RED: &str = "\u{1b}[31m";
const YELLOW: &str = "\u{1b}[33m";
const CYAN: &str = "\u{1b}[36m";
const BOLD: &str = "\u{1b}[1m";
const RESET: &str = "\u{1b}[0m";

/// Colour codes for a severity, empty when colour is disabled.
pub fn severity_style(severity: &crate::diagnostic::Severity) -> (&'static str, &'static str) {
    if !presentation().color {
        return ("", "");
    }
    let colour = match severity {
        crate::diagnostic::Severity::Error => RED,
        crate::diagnostic::Severity::Warning => YELLOW,
        crate::diagnostic::Severity::Suggestion | crate::diagnostic::Severity::Info => CYAN,
    };
    (colour, RESET)
}

/// Emphasis for the headline of a diagnostic, empty when colour is disabled.
pub fn emphasis() -> (&'static str, &'static str) {
    if presentation().color {
        (BOLD, RESET)
    } else {
        ("", "")
    }
}

pub fn severity_label(severity: &crate::diagnostic::Severity) -> &'static str {
    match severity {
        crate::diagnostic::Severity::Error => "error",
        crate::diagnostic::Severity::Warning => "warning",
        crate::diagnostic::Severity::Suggestion => "suggestion",
        crate::diagnostic::Severity::Info => "info",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Table,
    Json,
    Jsonl,
    Csv,
    Sqlite,
}
impl Format {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "table" => Ok(Self::Table),
            "json" => Ok(Self::Json),
            "jsonl" => Ok(Self::Jsonl),
            "csv" => Ok(Self::Csv),
            "sqlite" => Ok(Self::Sqlite),
            _ => Err(DbError::usage(format!("unknown output format {s:?}"))),
        }
    }
}

pub fn diagnostics(items: &[Diagnostic], format: Format) {
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(items).unwrap()),
        Format::Jsonl => {
            for x in items {
                println!("{}", serde_json::to_string(x).unwrap())
            }
        }
        _ => {
            for d in items {
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
                        eprintln!("  --> {}", p.display())
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
                if let Some(e) = &d.expected {
                    eprintln!("   = expected: {e}")
                }
                if let Some(o) = &d.observed {
                    eprintln!("   = observed: {o}")
                }
                if let Some(constraint) = &d.constraint {
                    eprintln!("   = constraint: {constraint}")
                }
                if !d.fixes.is_empty() {
                    eprintln!("   = fixes: {}", d.fixes.join(", "))
                }
                if let Some(h) = &d.help {
                    eprintln!("   = help: {h}")
                }
            }
        }
    }
}
pub fn diagnostic_notice(diagnostic: &Diagnostic, format: Format) {
    if matches!(format, Format::Json | Format::Jsonl) {
        match serde_json::to_string(diagnostic) {
            Ok(value) => eprintln!("{value}"),
            Err(error) => {
                eprintln!("error[INTERNAL_METADATA_CORRUPT]: cannot serialize diagnostic: {error}")
            }
        }
    } else {
        eprintln!("warning[{}]: {}", diagnostic.code, diagnostic.message);
    }
}
pub fn records(rows: &[Map<String, Value>], format: Format) -> Result<()> {
    match format {
        Format::Json => {
            let rows = machine_rows(rows);
            println!("{}", serde_json::to_string_pretty(&rows).unwrap())
        }
        Format::Jsonl => {
            for r in machine_rows(rows) {
                println!("{}", serde_json::to_string(&r).unwrap())
            }
        }
        Format::Csv => {
            let headers: Vec<_> = rows
                .iter()
                .flat_map(|r| r.keys())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .cloned()
                .collect();
            let mut w = csv::Writer::from_writer(io::stdout());
            w.write_record(&headers).map_err(out)?;
            for r in rows {
                w.write_record(
                    headers
                        .iter()
                        .map(|h| cell(r.get(h).unwrap_or(&Value::Null))),
                )
                .map_err(out)?
            }
            w.flush()
                .map_err(|e| DbError::new("IO_ERROR", e.to_string(), 6))?
        }
        Format::Table => table(rows),
        Format::Sqlite => {
            return Err(DbError::usage(
                "sqlite output requires `db export --out <path>`",
            ));
        }
    }
    Ok(())
}
pub fn jsonl_record(mut row: Map<String, Value>) -> Result<()> {
    if !row.contains_key("kind") {
        row.insert("kind".into(), Value::String("row".into()));
    }
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, &row)
        .map_err(|error| DbError::new("IO_ERROR", error.to_string(), 6))?;
    lock.write_all(b"\n")
        .map_err(|error| DbError::io(std::path::Path::new("stdout"), error))
}
pub fn check_result(
    diagnostics: &[Diagnostic],
    summary: Map<String, Value>,
    format: Format,
) -> Result<()> {
    match format {
        Format::Json => {
            let mut values = diagnostics
                .iter()
                .map(serde_json::to_value)
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| DbError::new("INTERNAL_METADATA_CORRUPT", error.to_string(), 6))?;
            values.push(Value::Object(summary));
            serde_json::to_writer_pretty(io::stdout().lock(), &values)
                .map_err(|error| DbError::new("IO_ERROR", error.to_string(), 6))?;
            println!();
        }
        Format::Jsonl => {
            for diagnostic in diagnostics {
                let value = serde_json::to_value(diagnostic).map_err(|error| {
                    DbError::new("INTERNAL_METADATA_CORRUPT", error.to_string(), 6)
                })?;
                let object = value.as_object().cloned().ok_or_else(|| {
                    DbError::new(
                        "INTERNAL_METADATA_CORRUPT",
                        "diagnostic is not an object",
                        6,
                    )
                })?;
                jsonl_record(object)?;
            }
            jsonl_record(summary)?;
        }
        _ => return Err(DbError::usage("check_result requires JSON output")),
    }
    Ok(())
}
fn machine_rows(rows: &[Map<String, Value>]) -> Vec<Map<String, Value>> {
    rows.iter()
        .map(|r| {
            let mut r = r.clone();
            if !r.contains_key("kind") {
                r.insert("kind".into(), Value::String("row".into()));
            }
            r
        })
        .collect()
}
fn table(rows: &[Map<String, Value>]) {
    if rows.is_empty() {
        println!("(0 rows)");
        return;
    }
    let headers: Vec<_> = rows
        .iter()
        .flat_map(|r| r.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .cloned()
        .collect();
    let widths: Vec<_> = headers
        .iter()
        .map(|h| {
            std::iter::once(h.len())
                .chain(
                    rows.iter()
                        .map(|r| cell(r.get(h).unwrap_or(&Value::Null)).len()),
                )
                .max()
                .unwrap()
        })
        .collect();
    for (i, h) in headers.iter().enumerate() {
        if i > 0 {
            print!(" | ")
        }
        print!("{h:width$}", width = widths[i])
    }
    println!();
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            print!("-+-")
        }
        print!("{}", "-".repeat(*w))
    }
    println!();
    for r in rows {
        for (i, h) in headers.iter().enumerate() {
            if i > 0 {
                print!(" | ")
            }
            print!(
                "{:width$}",
                cell(r.get(h).unwrap_or(&Value::Null)),
                width = widths[i]
            )
        }
        println!()
    }
    println!("({} rows)", rows.len());
    let _ = io::stdout().flush();
}
fn cell(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::String(s) => s.clone(),
        _ => serde_json::to_string(v).unwrap(),
    }
}
fn out(e: csv::Error) -> DbError {
    DbError::new("IO_ERROR", e.to_string(), 6)
}
