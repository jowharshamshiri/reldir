//! Changing a JSON file without rewriting it.
//!
//! A row file is shared: outside programs read it, write it, diff it and keep
//! it under version control. When reldir changes a row, the file must change
//! exactly where the row changed and nowhere else -- whitespace, member order,
//! the spelling of every number (`1.50`, `1e2`), the escapes and Unicode
//! normalization form of every string, line endings, a byte order mark and a
//! trailing newline all stay as their author left them. Re-serializing the
//! whole value cannot keep that promise, so this module edits the bytes.
//!
//! The pieces, in the order a change flows through them:
//!
//! - [`Document`] is the concrete syntax of one file: the byte span of every
//!   value, member name, colon and separator.
//! - [`Style`] is how the file is written -- its indentation unit, colon and
//!   comma spacing, line ending and the rest -- measured from the file itself,
//!   so new content looks like the content around it.
//! - [`render`] writes a value in a style.
//! - [`apply`] splices a set of [`Edit`]s into the file, each one touching only
//!   the span it owns, and proves the result reads back as intended.
//! - [`diff`] works out the edits that turn one value into another, and
//!   [`edit_document`] is the whole round: the file and the value it should
//!   now hold in, the new file out.

mod diff;
mod edit;
mod fuzz;
mod render;
mod scan;
mod style;

#[cfg(test)]
mod tests;

pub use diff::{diff, edit_document};
pub use edit::{Edit, apply};
pub use fuzz::fuzz_roundtrip;
pub use render::{render, render_document};
pub use scan::{BOM, Children, Document, Element, Kind, Member, Node, NodeId};
pub use style::{Colon, Comma, Detection, Empty, Evidence, Indent, Newline, Style, StyleError};

use serde_json::Value;

/// Why a document could not be read or edited.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    /// The bytes are not one strict JSON document, by the rules of
    /// [`crate::json::parse`]. A depth-limit refusal is recognisable with
    /// [`crate::json::is_depth_limit`].
    #[error("the document is not valid JSON: {0}")]
    Invalid(serde_json::Error),
    /// The text is not an RFC 6901 JSON Pointer.
    #[error("{pointer:?} is not a JSON Pointer: {reason}")]
    Pointer {
        pointer: String,
        reason: &'static str,
    },
    /// The pointer is well formed but the document holds nothing there.
    #[error("the document holds nothing at {pointer:?}")]
    Missing { pointer: String },
    /// The pointer names an existing array element past the array's end.
    #[error("{pointer:?} has {len} elements, so index {index} is past its end")]
    Index {
        pointer: String,
        index: usize,
        len: usize,
    },
    /// An edit addresses a value of the wrong kind, such as a member insertion
    /// into an array.
    #[error("{pointer:?} is {found}, where the edit needs {expected}")]
    Kind {
        pointer: String,
        expected: &'static str,
        found: &'static str,
    },
    /// A member insertion names a member the object already has.
    #[error("the object at {pointer:?} already has a member {name:?}")]
    Exists { pointer: String, name: String },
    /// The document root has no container to be removed from.
    #[error("the document root cannot be removed")]
    RootRemoval,
    /// Two edits claim the same bytes, or one edit depends on a member another
    /// removes, so no single result honours both.
    #[error("conflicting edits: {0}")]
    Conflict(String),
    /// An edit carries a number with no finite value, which no strict reader
    /// would accept back.
    #[error("the number {0} has no finite value and cannot be written")]
    NonFinite(String),
    /// The edited document would nest deeper than the configured limit.
    #[error("the edited document exceeds the nesting limit: {0}")]
    TooDeep(serde_json::Error),
    /// The edit produced bytes that do not read back as the intended value.
    /// This is a defect in this module, reported rather than written.
    #[error("internal error while editing JSON: {0}")]
    Internal(String),
}

impl EditError {
    pub(crate) fn internal(detail: impl Into<String>) -> Self {
        Self::Internal(detail.into())
    }
}

/// The pointer of the first place two values differ, comparing member order
/// too; `None` when they are identical.
///
/// `Value`'s own equality treats objects as unordered maps, which is right for
/// deciding whether a row changed and wrong for checking a file edit, where a
/// member landing in the wrong place is a defect.
pub(crate) fn first_difference(left: &Value, right: &Value) -> Option<String> {
    let mut work = vec![(String::new(), left, right)];
    while let Some((pointer, left, right)) = work.pop() {
        match (left, right) {
            (Value::Object(left), Value::Object(right)) => {
                if left.len() != right.len() || !left.keys().eq(right.keys()) {
                    return Some(pointer);
                }
                for ((name, left), right) in left.iter().zip(right.values()) {
                    work.push((
                        format!("{pointer}/{}", crate::schema::path::escape_pointer(name)),
                        left,
                        right,
                    ));
                }
            }
            (Value::Array(left), Value::Array(right)) => {
                if left.len() != right.len() {
                    return Some(pointer);
                }
                for (index, (left, right)) in left.iter().zip(right).enumerate() {
                    work.push((format!("{pointer}/{index}"), left, right));
                }
            }
            (Value::Object(_) | Value::Array(_), _) | (_, Value::Object(_) | Value::Array(_)) => {
                return Some(pointer);
            }
            (left, right) if left != right => return Some(pointer),
            _ => {}
        }
    }
    None
}

/// Whether a number's text denotes a finite value. The rule is the one the
/// strict parser applies: a literal too large for a double -- `1e999`, or an
/// integer with hundreds of digits -- has no finite value.
pub(crate) fn finite(lexeme: &str) -> bool {
    lexeme.parse::<f64>().is_ok_and(f64::is_finite)
}

/// The bytes to write for `value` at a path that held `previous`: the previous
/// file edited in place -- every byte the change does not touch kept, new
/// content in the file's own style -- when it is a JSON document; written
/// afresh in `style` when there is no previous file, or it is not JSON and so
/// has no members to keep.
pub fn write_document(
    previous: Option<&[u8]>,
    value: &serde_json::Value,
    style: &Style,
) -> Result<Vec<u8>, EditError> {
    match previous {
        Some(raw) => match edit_document(raw, value) {
            Err(EditError::Invalid(_)) => Ok(render_document(value, style)),
            other => other,
        },
        None => Ok(render_document(value, style)),
    }
}
