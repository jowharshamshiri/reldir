use crate::code::{Code, Exit};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Suggestion,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub line: usize,
    pub column: usize,
}

fn diagnostic_kind() -> String {
    "diagnostic".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    /// Always `"diagnostic"`: what the record is, in the machine stream.
    #[serde(rename = "type", default = "diagnostic_kind")]
    pub kind: String,
    pub severity: Severity,
    pub code: Code,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    /// RFC 6901 pointer to the value the diagnostic is about, inside the file
    /// named by `path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
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
    /// Read a diagnostic back from its machine-readable form.
    pub fn from_json(value: &serde_json::Value) -> std::result::Result<Self, serde_json::Error> {
        Self::deserialize(value)
    }
    pub fn error(code: Code, message: impl Into<String>) -> Self {
        Self {
            kind: diagnostic_kind(),
            severity: Severity::Error,
            code,
            message: message.into(),
            table: None,
            path: None,
            location: None,
            pointer: None,
            source_line: None,
            field: None,
            constraint: None,
            expected: None,
            observed: None,
            fixes: vec![],
            help: None,
        }
    }
    pub fn warning(code: Code, message: impl Into<String>) -> Self {
        let mut d = Self::error(code, message);
        d.severity = Severity::Warning;
        d
    }
    pub fn suggestion(code: Code, message: impl Into<String>) -> Self {
        let mut d = Self::error(code, message);
        d.severity = Severity::Suggestion;
        d
    }
    pub fn info(code: Code, message: impl Into<String>) -> Self {
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
    pub fn constraint(mut self, constraint: impl Into<String>) -> Self {
        self.constraint = Some(constraint.into());
        self
    }
    pub fn pointer(mut self, pointer: impl Into<String>) -> Self {
        self.pointer = Some(pointer.into());
        self
    }
    /// Resolve `pointer` against the bytes of the file the diagnostic is about,
    /// filling in the location and the source line a reader needs.
    pub fn locate_in(mut self, raw: &[u8], spans: &crate::locate::Spans) -> Self {
        // A fault in a member's name is shown at the name; every other fault
        // is in a value, and is shown at the value.
        let about_the_name = matches!(
            self.code,
            Code::ROW_UNKNOWN_FIELD | Code::SCHEMA_UNKNOWN_KEY | Code::KEY_COLLISION
        );
        let found = self.pointer.as_deref().and_then(|pointer| {
            if about_the_name {
                spans.location(pointer)
            } else {
                spans
                    .value_location(pointer)
                    .or_else(|| spans.location(pointer))
            }
        });
        if let Some(location) = found {
            self.source_line = std::str::from_utf8(raw)
                .ok()
                .and_then(|text| text.lines().nth(location.line.saturating_sub(1)))
                .map(String::from);
            self.location = Some(location);
        }
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

/// The exit status a set of diagnostics about the database's state implies.
/// Only errors count: a warning never fails a command. Faults that make the
/// metadata untrustworthy outrank an interrupted transaction, which outranks
/// an ordinary invalid state.
pub fn exit_for_diagnostics(diagnostics: &[Diagnostic]) -> Exit {
    let errors: Vec<&Diagnostic> = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    if errors.is_empty() {
        Exit::Ok
    } else if errors.iter().any(|d| d.code.exit() == Exit::Untrusted) {
        Exit::Untrusted
    } else if errors.iter().any(|d| d.code.exit() == Exit::Recovery) {
        Exit::Recovery
    } else {
        Exit::Invalid
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{}: {}", diagnostic.code, diagnostic.message)]
pub struct DbError {
    pub diagnostic: Box<Diagnostic>,
    /// Further faults behind the same failure: a refused change names every
    /// fault it would have introduced, not only the first.
    pub related: Vec<Diagnostic>,
}

impl DbError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self::from_diag(Diagnostic::error(code, message))
    }

    pub fn from_diag(diagnostic: Diagnostic) -> Self {
        Self {
            diagnostic: Box::new(diagnostic),
            related: vec![],
        }
    }

    pub fn with_related(mut self, related: Vec<Diagnostic>) -> Self {
        self.related = related;
        self
    }

    /// Every diagnostic the error carries, the lead first.
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        std::iter::once((*self.diagnostic).clone())
            .chain(self.related.iter().cloned())
            .collect()
    }

    /// The same error, with advice on what to do about it.
    pub fn with_help(mut self, text: impl Into<String>) -> Self {
        self.diagnostic.help = Some(text.into());
        self
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(Code::USAGE, message)
    }

    pub fn io(path: &std::path::Path, e: std::io::Error) -> Self {
        Self::from_diag(Diagnostic::error(Code::IO_ERROR, e.to_string()).at(path))
    }

    pub fn code(&self) -> Code {
        self.diagnostic.code
    }

    /// The exit status this failure carries: its code's, the one place an exit
    /// status is decided.
    pub fn exit(&self) -> Exit {
        self.diagnostic.code.exit()
    }
}

pub type Result<T> = std::result::Result<T, DbError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Machine-readable diagnostics are a stable contract. Every record carries
    /// its kind, severity, code and message, and optional fields are omitted
    /// rather than serialised as null.
    #[test]
    fn test1026_diagnostics_serialise_to_the_documented_shape() {
        let diagnostic =
            Diagnostic::error(Code::FOREIGN_KEY_VIOLATION, "posts.user_id is an orphan")
                .at("posts/42.json")
                .table("posts")
                .field("user_id")
                .expected("existing users.id")
                .observed("\"missing\"")
                .fix("FIX_REMOVE_REFERENCE")
                .fix("FIX_ORPHAN_DELETE_ROW")
                .help("run `reldir doctor`");
        let value = serde_json::to_value(&diagnostic).unwrap();
        assert_eq!(value["type"], "diagnostic");
        assert_eq!(value["severity"], "error");
        assert_eq!(value["code"], "FOREIGN_KEY_VIOLATION");
        assert_eq!(value["table"], "posts");
        assert_eq!(value["path"], "posts/42.json");
        assert_eq!(value["field"], "user_id");
        assert_eq!(value["expected"], "existing users.id");
        assert_eq!(value["fixes"][0], "FIX_REMOVE_REFERENCE");
        assert_eq!(value["fixes"][1], "FIX_ORPHAN_DELETE_ROW");
        let minimal =
            serde_json::to_value(Diagnostic::error(Code::UNKNOWN_TABLE, "no such table")).unwrap();
        for absent in [
            "table",
            "path",
            "location",
            "field",
            "constraint",
            "expected",
            "observed",
            "help",
            "fixes",
        ] {
            assert!(
                minimal.get(absent).is_none(),
                "{absent} must be omitted when unset"
            );
        }
        // A diagnostic read back is the diagnostic written, and an unknown code
        // is refused rather than carried as text nobody can dispatch on.
        assert_eq!(
            Diagnostic::from_json(&value).unwrap().code,
            Code::FOREIGN_KEY_VIOLATION
        );
        let mut unknown = value.clone();
        unknown["code"] = serde_json::json!("NO_SUCH_CODE");
        assert!(Diagnostic::from_json(&unknown).is_err());
    }

    /// Each severity serialises to its documented lowercase name.
    #[test]
    fn test1027_severities_serialise_in_lowercase() {
        for (diagnostic, expected) in [
            (Diagnostic::error(Code::USAGE, "m"), "error"),
            (Diagnostic::warning(Code::USAGE, "m"), "warning"),
            (Diagnostic::suggestion(Code::USAGE, "m"), "suggestion"),
            (Diagnostic::info(Code::USAGE, "m"), "info"),
        ] {
            assert_eq!(
                serde_json::to_value(&diagnostic).unwrap()["severity"],
                expected
            );
        }
    }

    /// Metadata and format faults outrank an interrupted transaction, which
    /// outranks an ordinary invalid database; warnings never fail a command.
    #[test]
    fn test1028_diagnostic_exit_codes_follow_the_documented_precedence() {
        assert_eq!(exit_for_diagnostics(&[]), Exit::Ok);
        assert_eq!(
            exit_for_diagnostics(&[Diagnostic::warning(Code::ASSERTION_VIOLATION, "m")]),
            Exit::Ok,
            "a warning never fails a command"
        );
        assert_eq!(
            exit_for_diagnostics(&[Diagnostic::error(Code::FOREIGN_KEY_VIOLATION, "m")]),
            Exit::Invalid
        );
        assert_eq!(
            exit_for_diagnostics(&[Diagnostic::error(Code::TRANSACTION_INCOMPLETE, "m")]),
            Exit::Recovery
        );
        for code in [Code::FORMAT_UNSUPPORTED, Code::INTERNAL_METADATA_CORRUPT] {
            assert_eq!(
                exit_for_diagnostics(&[Diagnostic::error(code, "m")]),
                Exit::Untrusted
            );
        }
        let mixed = vec![
            Diagnostic::error(Code::UNIQUE_VIOLATION, "m"),
            Diagnostic::error(Code::TRANSACTION_INCOMPLETE, "m"),
            Diagnostic::error(Code::FORMAT_UNSUPPORTED, "m"),
        ];
        assert_eq!(exit_for_diagnostics(&mixed), Exit::Untrusted);
        assert_eq!(exit_for_diagnostics(&mixed[..2]), Exit::Recovery);
    }

    /// An error's exit status is its code's, and an I/O error names the path.
    #[test]
    fn test1029_error_constructors_carry_their_exit_status() {
        assert_eq!(DbError::usage("bad flag").exit(), Exit::Usage);
        assert_eq!(DbError::usage("bad flag").code(), Code::USAGE);
        assert_eq!(
            DbError::new(Code::QUERY_UNSUPPORTED, "m").exit(),
            Exit::Unresolved
        );
        let io = DbError::io(
            std::path::Path::new("users/u1.json"),
            std::io::Error::other("denied"),
        );
        assert_eq!(io.exit(), Exit::Untrusted);
        assert_eq!(io.code(), Code::IO_ERROR);
        assert_eq!(
            io.diagnostic.path.as_deref(),
            Some(std::path::Path::new("users/u1.json"))
        );
    }
}
