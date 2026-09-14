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
        eprintln!("error[{}]: {}", d.code, d.message);
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
