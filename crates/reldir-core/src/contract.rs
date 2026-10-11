//! The observed contract: what a table's files may hold.
//!
//! Every table has a contract -- its schema document, pinned by a person or
//! inferred from the files when the folder was adopted. Programs outside
//! reldir rely on the shape of those files, and reldir never changes it on its
//! own authority:
//!
//! - A value written by reldir -- a statement, a row command, an import, a
//!   cascade, a repair -- that breaks the contract where the file was within it
//!   is refused with `CONTRACT_WIDENING_REQUIRED`, naming the least widening
//!   that would admit it. Widening is a decision: `--widen` on the change,
//!   `reldir schema widen`, or doctor's `FIX_WIDEN_CONTRACT`.
//! - A value written *outside* reldir into a table whose contract was
//!   inferred is evidence about the contract nobody wrote down, not damage:
//!   it is reported as a `CONTRACT_EXCEEDED` warning, left exactly as it is,
//!   and the database stays usable. It never widens the contract by itself.
//!   A pinned contract was written by a person, so breaking it stays an
//!   error.
//!
//! A fault is *new* when the change writes the file it is in and the same
//! fault -- the same rule refusing the same value at the same place -- was
//! not there before. Editing one member of a row leaves a fault in another
//! member as it was; renaming a row carries its faults with it.

use crate::{
    code::Code,
    diagnostic::{Diagnostic, Severity},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

/// The codes by which a row breaks its schema document. Keys, references,
/// checks and file names are integrity, not shape, and stay errors anywhere.
pub const SHAPE_CODES: &[Code] = &[
    Code::TYPE_MISMATCH,
    Code::ROW_MISSING_FIELD,
    Code::ROW_UNKNOWN_FIELD,
    Code::NOT_NULL_VIOLATION,
    Code::SCHEMA_VIOLATION,
];

/// Whether a diagnostic says a row breaks its contract: a shape fault, or one
/// already reported as exceeding an inferred contract.
pub fn is_fault(diagnostic: &Diagnostic) -> bool {
    diagnostic.code == Code::CONTRACT_EXCEEDED || SHAPE_CODES.contains(&diagnostic.code)
}

/// A shape fault in a table whose contract was inferred, as the warning it
/// is: the value is what an outside program wrote, and the contract has not
/// heard of it.
pub fn exceeded(fault: Diagnostic) -> Diagnostic {
    let table = fault.table.clone().unwrap_or_default();
    Diagnostic {
        code: Code::CONTRACT_EXCEEDED,
        severity: Severity::Warning,
        message: format!(
            "{} -- outside the contract inferred for {table} ({})",
            fault.message, fault.code
        ),
        fixes: vec!["FIX_WIDEN_CONTRACT".into()],
        help: Some(
            "the file was written outside reldir; reldir leaves the value as it is and never \
             writes one like it. `reldir doctor --only FIX_WIDEN_CONTRACT` or `reldir schema \
             widen` makes the contract admit it"
                .into(),
        ),
        ..fault
    }
}

/// What identifies a fault across a change: the rule, the place and the
/// value. The path is where the row stood before the change.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Identity {
    path: Option<PathBuf>,
    pointer: Option<String>,
    field: Option<String>,
    constraint: Option<String>,
    observed: Option<String>,
    /// The shape code underneath, so a `CONTRACT_EXCEEDED` and the error it
    /// was made from are one fault.
    message: String,
}

fn identity(diagnostic: &Diagnostic, path: Option<PathBuf>) -> Identity {
    // A warning's message wraps the fault's; the wrapped part is the fault.
    let message = match diagnostic.code {
        Code::CONTRACT_EXCEEDED => diagnostic
            .message
            .rsplit_once(" -- outside the contract inferred for ")
            .map_or(diagnostic.message.clone(), |(fault, _)| fault.to_string()),
        _ => diagnostic.message.clone(),
    };
    Identity {
        path,
        pointer: diagnostic.pointer.clone(),
        field: diagnostic.field.clone(),
        constraint: diagnostic.constraint.clone(),
        observed: diagnostic.observed.clone(),
        message,
    }
}

/// The files a change writes, and where each of their rows stood before.
#[derive(Debug, Default)]
pub struct Written {
    pub paths: BTreeSet<PathBuf>,
    /// A row moved by the change: its new path, and its path before.
    pub moved_from: BTreeMap<PathBuf, PathBuf>,
}

/// The contract faults a change would introduce: those in a file it writes
/// that the same row did not already have.
pub fn introduced(
    before: &[Diagnostic],
    after: &[Diagnostic],
    written: &Written,
) -> Vec<Diagnostic> {
    let known: BTreeSet<Identity> = before
        .iter()
        .filter(|diagnostic| is_fault(diagnostic))
        .map(|diagnostic| identity(diagnostic, diagnostic.path.clone()))
        .collect();
    after
        .iter()
        .filter(|diagnostic| is_fault(diagnostic))
        .filter(|diagnostic| {
            diagnostic
                .path
                .as_ref()
                .is_some_and(|path| written.paths.contains(path))
        })
        .filter(|diagnostic| {
            let path = diagnostic.path.as_ref().map(|path| {
                written
                    .moved_from
                    .get(path)
                    .cloned()
                    .unwrap_or_else(|| path.clone())
            });
            !known.contains(&identity(diagnostic, path))
        })
        .cloned()
        .collect()
}

/// A table's contract widened just enough to admit what its files hold where
/// they break it -- each file named by one of `faults` -- and the edits made.
/// Refused, with the reason, when the contract cannot be widened by rule.
pub fn admitting_files(
    catalog: &crate::catalog::Catalog,
    table: &str,
    faults: &[Diagnostic],
) -> crate::diagnostic::Result<(serde_json::Value, Vec<crate::schema::widen::Widened>)> {
    let schema = catalog
        .schemas
        .get(table)
        .ok_or_else(|| catalog.unknown_table(table))?;
    let paths: BTreeSet<&PathBuf> = faults
        .iter()
        .filter(|fault| fault.table.as_deref() == Some(table) && is_fault(fault))
        .filter_map(|fault| fault.path.as_ref())
        .collect();
    let mut bodies = vec![];
    for path in paths {
        let full = catalog.root.join(path);
        let bytes =
            std::fs::read(&full).map_err(|error| crate::diagnostic::DbError::io(&full, error))?;
        bodies.push(crate::json::parse(&bytes).map_err(|error| {
            crate::diagnostic::DbError::new(
                Code::CONCURRENT_MODIFICATION,
                format!("{} no longer parses: {error}", path.display()),
            )
        })?);
    }
    crate::schema::widen::admit(schema.document(), &bodies).map_err(|reason| {
        crate::diagnostic::DbError::from_diag(
            Diagnostic::error(
                Code::CONTRACT_WIDENING_REQUIRED,
                format!("the contract of {table} cannot be widened by rule: {reason}"),
            )
            .table(table)
            .help("edit the schema to say what it should admit"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fault(path: &str, pointer: &str, observed: &str) -> Diagnostic {
        Diagnostic::error(
            Code::TYPE_MISMATCH,
            format!("the value at {pointer} is wrong"),
        )
        .at(PathBuf::from(path))
        .pointer(pointer.to_string())
        .observed(observed.to_string())
        .table("t")
    }

    #[test]
    fn test8940_a_fault_is_new_only_where_the_change_writes_and_it_was_not_before() {
        let before = vec![exceeded(fault("t/a.json", "/n", "\"x\""))];
        let written = Written {
            paths: ["t/a.json", "t/c.json"]
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            moved_from: [(PathBuf::from("t/c.json"), PathBuf::from("t/b.json"))]
                .into_iter()
                .collect(),
        };
        let carried = exceeded(fault("t/a.json", "/n", "\"x\""));
        let changed = exceeded(fault("t/a.json", "/n", "\"y\""));
        let elsewhere = exceeded(fault("t/a.json", "/m", "\"x\""));
        let untouched_file = exceeded(fault("t/z.json", "/n", "\"q\""));
        let places = |faults: Vec<Diagnostic>| -> Vec<(Option<String>, Option<String>)> {
            faults
                .into_iter()
                .map(|fault| (fault.pointer, fault.observed))
                .collect()
        };
        assert_eq!(
            places(introduced(
                &before,
                &[carried, changed.clone(), elsewhere.clone(), untouched_file],
                &written
            )),
            places(vec![changed, elsewhere]),
            "a fault the row already had is carried, a new value or place is new, and a file \
             the change does not write is not the change's doing"
        );

        let before = vec![exceeded(fault("t/b.json", "/n", "\"x\""))];
        let renamed = exceeded(fault("t/c.json", "/n", "\"x\""));
        assert!(
            introduced(&before, &[renamed], &written).is_empty(),
            "a renamed row keeps the faults it had"
        );

        let error = fault("t/a.json", "/n", "\"x\"");
        assert!(
            introduced(&[exceeded(error.clone())], &[error], &written).is_empty(),
            "pinning a contract turns its warnings into errors, and they are the same faults"
        );
    }
}
