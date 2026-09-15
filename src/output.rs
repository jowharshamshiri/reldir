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
    /// Report progress from the first file scanned rather than waiting out the
    /// threshold that keeps short operations silent (Section 49).
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

/// Progress reporting for an operation that walks many files.
///
/// Section 49: an operation expected to exceed one second on a TTY shows a
/// progress line (files scanned / total) on stderr. Nothing is emitted before
/// that threshold, so a fast command stays silent, and nothing is ever emitted
/// off a terminal or under `--quiet`, so machine-readable output and captured
/// stderr are byte-for-byte unaffected.
///
/// The reporter owns its own threshold and its own cleanup: the line is erased
/// when it is dropped, including on an early return or a panic, so a partial
/// progress line can never be left behind on the user's terminal.
pub struct Progress {
    label: &'static str,
    total: usize,
    scanned: usize,
    started: std::time::Instant,
    active: bool,
    enabled: bool,
    immediate: bool,
}

/// An operation must run longer than this before it is worth reporting.
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
            // `--verbose` asks to see the operation's progress, so it reports
            // from the first file rather than waiting out the threshold that
            // keeps short commands silent.
            immediate: settings.verbose,
        }
    }

    /// Record one more scanned file, drawing or refreshing the line once the
    /// operation has run long enough to deserve one.
    pub fn advance(&mut self) {
        self.scanned += 1;
        if !self.enabled || (!self.immediate && self.started.elapsed() < PROGRESS_AFTER) {
            return;
        }
        self.active = true;
        // A carriage return keeps the report on one refreshed line rather than
        // scrolling the terminal.
        eprint!(
            "\r\u{1b}[2K{}: {} / {} files scanned",
            self.label, self.scanned, self.total
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::Severity;

    /// Section 52: the severity label is part of the human diagnostic contract
    /// and must name each severity exactly.
    #[test]
    fn test1073_severity_labels_are_stable() {
        assert_eq!(severity_label(&Severity::Error), "error");
        assert_eq!(severity_label(&Severity::Warning), "warning");
        assert_eq!(severity_label(&Severity::Suggestion), "suggestion");
        assert_eq!(severity_label(&Severity::Info), "info");
    }

    /// Section 49: presentation is settled once per process. Tests run with
    /// output redirected, so colour must be off and every style must be empty,
    /// leaving diagnostics byte-for-byte parseable by tooling.
    #[test]
    fn test1074_redirected_output_carries_no_escape_sequences() {
        // Whatever the ambient environment, a non-terminal stderr means no
        // colour: the default derivation requires a terminal.
        let settings = Presentation {
            color: false,
            quiet: false,
            verbose: false,
        };
        set_presentation(settings);

        let (colour, reset) = severity_style(&Severity::Error);
        let (bold, bold_reset) = emphasis();
        for piece in [colour, reset, bold, bold_reset] {
            assert!(
                piece.is_empty(),
                "no styling may be emitted when colour is off, got {piece:?}"
            );
        }
    }

    /// Section 49: progress is a terminal affordance. Redirected stderr must
    /// stay byte-for-byte clean, because machine consumers and the diagnostic
    /// contract read it; a progress line leaking into a pipe would corrupt both.
    #[test]
    fn test1075_progress_is_silent_off_a_terminal() {
        set_presentation(Presentation {
            color: false,
            quiet: false,
            verbose: true,
        });
        // Tests never own a terminal, so even a verbose reporter that advances
        // past its threshold must emit nothing and must claim nothing to clean
        // up when dropped.
        let mut progress = Progress::new("scanning", 3);
        for _ in 0..3 {
            progress.advance();
        }
        assert!(
            !progress.active,
            "a reporter must never draw when stderr is not a terminal"
        );
        assert_eq!(progress.scanned, 3, "counting continues regardless");
    }

    /// Format parsing accepts exactly the documented encodings (Section 60) and
    /// rejects anything else as a usage error rather than falling back.
    #[test]
    fn test1076_output_formats_are_a_closed_set() {
        for (text, expected) in [
            ("table", Format::Table),
            ("json", Format::Json),
            ("jsonl", Format::Jsonl),
            ("csv", Format::Csv),
            ("sqlite", Format::Sqlite),
        ] {
            assert_eq!(Format::parse(text).unwrap(), expected);
        }
        for invalid in ["", "JSON", "yaml", "tsv", "table "] {
            let error = Format::parse(invalid).expect_err(&format!("{invalid:?} must be refused"));
            assert_eq!(error.diagnostic.code, "USAGE");
        }
    }
}
