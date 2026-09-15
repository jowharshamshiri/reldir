use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Suggestion,
    Info,
}

#[derive(Debug, Clone, Serialize)]
pub struct Location {
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    pub kind: &'static str,
    pub severity: Severity,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constraint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: "diagnostic",
            severity: Severity::Error,
            code: code.into(),
            message: message.into(),
            table: None,
            path: None,
            location: None,
            source_line: None,
            field: None,
            constraint: None,
            expected: None,
            observed: None,
            fixes: vec![],
            help: None,
        }
    }
    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        let mut d = Self::error(code, message);
        d.severity = Severity::Warning;
        d
    }
    pub fn suggestion(code: impl Into<String>, message: impl Into<String>) -> Self {
        let mut d = Self::error(code, message);
        d.severity = Severity::Suggestion;
        d
    }
    pub fn info(code: impl Into<String>, message: impl Into<String>) -> Self {
        let mut d = Self::error(code, message);
        d.severity = Severity::Info;
        d
    }
    pub fn at(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }
    pub fn table(mut self, table: impl Into<String>) -> Self {
        self.table = Some(table.into());
        self
    }
    pub fn field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }
    pub fn expected(mut self, text: impl Into<String>) -> Self {
        self.expected = Some(text.into());
        self
    }
    pub fn observed(mut self, text: impl Into<String>) -> Self {
        self.observed = Some(text.into());
        self
    }
    pub fn help(mut self, text: impl Into<String>) -> Self {
        self.help = Some(text.into());
        self
    }
    pub fn fix(mut self, id: impl Into<String>) -> Self {
        self.fixes.push(id.into());
        self
    }
}

pub fn exit_code_for_diagnostics(diagnostics: &[Diagnostic]) -> i32 {
    if diagnostics.iter().any(|diagnostic| {
        matches!(
            diagnostic.code.as_str(),
            "FORMAT_UNSUPPORTED" | "INTERNAL_METADATA_CORRUPT"
        )
    }) {
        6
    } else if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "TRANSACTION_INCOMPLETE")
    {
        5
    } else if diagnostics.is_empty() {
        0
    } else {
        2
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{diagnostic:?}")]
pub struct DbError {
    pub diagnostic: Box<Diagnostic>,
    pub exit: i32,
}

impl DbError {
    pub fn new(code: &str, message: impl Into<String>, exit: i32) -> Self {
        Self {
            diagnostic: Box::new(Diagnostic::error(code, message)),
            exit,
        }
    }
    pub fn from_diag(diagnostic: Diagnostic, exit: i32) -> Self {
        Self {
            diagnostic: Box::new(diagnostic),
            exit,
        }
    }
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new("USAGE", message, 1)
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("DATABASE_INVALID", message, 2)
    }
    pub fn io(path: &std::path::Path, e: std::io::Error) -> Self {
        Self::from_diag(Diagnostic::error("IO_ERROR", e.to_string()).at(path), 6)
    }
    pub fn exit_code(&self) -> i32 {
        self.exit
    }
    pub fn render_human(&self) {
        let d = &self.diagnostic;
        let (colour, reset) = crate::output::severity_style(&d.severity);
        let (bold, bold_reset) = crate::output::emphasis();
        eprintln!(
            "{colour}{}[{}]{reset}: {bold}{}{bold_reset}",
            crate::output::severity_label(&d.severity),
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

pub type Result<T> = std::result::Result<T, DbError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Section 52: machine-readable diagnostics are a stable contract. Every
    /// record carries its kind, severity, code, and message, and optional fields
    /// are omitted rather than serialised as null.
    #[test]
    fn test9999_diagnostics_serialise_to_the_documented_shape() {
        let diagnostic = Diagnostic::error("FOREIGN_KEY_VIOLATION", "posts.user_id is an orphan")
            .at("posts/42.json")
            .table("posts")
            .field("user_id")
            .expected("existing users.id")
            .observed("\"missing\"")
            .fix("FIX_ORPHAN_SET_NULL")
            .fix("FIX_ORPHAN_DELETE_ROW")
            .help("run `db doctor`");
        let value = serde_json::to_value(&diagnostic).unwrap();

        assert_eq!(value["kind"], "diagnostic");
        assert_eq!(value["severity"], "error");
        assert_eq!(value["code"], "FOREIGN_KEY_VIOLATION");
        assert_eq!(value["table"], "posts");
        assert_eq!(value["path"], "posts/42.json");
        assert_eq!(value["field"], "user_id");
        assert_eq!(value["expected"], "existing users.id");
        assert_eq!(value["fixes"][0], "FIX_ORPHAN_SET_NULL");
        assert_eq!(value["fixes"][1], "FIX_ORPHAN_DELETE_ROW");

        // Absent optional members are omitted entirely.
        let minimal =
            serde_json::to_value(Diagnostic::error("UNKNOWN_TABLE", "no such table")).unwrap();
        for absent in [
            "table",
            "path",
            "location",
            "field",
            "constraint",
            "expected",
            "observed",
            "help",
        ] {
            assert!(
                minimal.get(absent).is_none(),
                "{absent} must be omitted when unset"
            );
        }
        assert!(
            minimal.get("fixes").is_none(),
            "an empty fix list is omitted"
        );
    }

    /// Section 52: each severity serialises to its documented lowercase name.
    #[test]
    fn test9999_severities_serialise_in_lowercase() {
        for (diagnostic, expected) in [
            (Diagnostic::error("C", "m"), "error"),
            (Diagnostic::warning("C", "m"), "warning"),
            (Diagnostic::suggestion("C", "m"), "suggestion"),
            (Diagnostic::info("C", "m"), "info"),
        ] {
            assert_eq!(
                serde_json::to_value(&diagnostic).unwrap()["severity"],
                expected
            );
        }
    }

    /// Section 51: exit codes are a machine-readable contract. Metadata and
    /// format faults outrank an incomplete transaction, which outranks an
    /// ordinary invalid database, and a clean run exits zero.
    #[test]
    fn test9999_diagnostic_exit_codes_follow_the_documented_precedence() {
        assert_eq!(exit_code_for_diagnostics(&[]), 0);
        assert_eq!(
            exit_code_for_diagnostics(&[Diagnostic::error("FOREIGN_KEY_VIOLATION", "m")]),
            2
        );
        assert_eq!(
            exit_code_for_diagnostics(&[Diagnostic::error("TRANSACTION_INCOMPLETE", "m")]),
            5
        );
        for code in ["FORMAT_UNSUPPORTED", "INTERNAL_METADATA_CORRUPT"] {
            assert_eq!(
                exit_code_for_diagnostics(&[Diagnostic::error(code, "m")]),
                6
            );
        }

        // Precedence holds when several faults are present at once.
        let mixed = vec![
            Diagnostic::error("UNIQUE_VIOLATION", "m"),
            Diagnostic::error("TRANSACTION_INCOMPLETE", "m"),
            Diagnostic::error("FORMAT_UNSUPPORTED", "m"),
        ];
        assert_eq!(exit_code_for_diagnostics(&mixed), 6);
        assert_eq!(exit_code_for_diagnostics(&mixed[..2]), 5);
    }

    /// Errors raised by the CLI carry the exit status their kind implies.
    #[test]
    fn test9999_error_constructors_carry_their_exit_status() {
        assert_eq!(DbError::usage("bad flag").exit_code(), 1);
        assert_eq!(DbError::usage("bad flag").diagnostic.code, "USAGE");
        assert_eq!(DbError::invalid("bad state").exit_code(), 2);
        assert_eq!(DbError::new("QUERY_UNSUPPORTED", "m", 4).exit_code(), 4);
    }
}
