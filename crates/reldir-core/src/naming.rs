//! How a table's file names relate to the identity of its rows.
//!
//! Adopting a folder must never rename or rewrite a file another program
//! relies on, so reldir learns the folder's naming convention instead of
//! imposing its own. A row's identity comes from one of two sources:
//!
//! - **the file stem** ([`Identity::FileStem`]): the name *is* the key, and the
//!   body need not mention it -- `people/ada.json` is the row whose key is
//!   `ada`. The stem is exposed as a column of its own.
//! - **the body** ([`Identity::Body`]): one or more columns hold the key, and a
//!   [`NamingRule`] says how their values spell the file name.
//!
//! Three rules exist. [`NamingRule::Reldir`] is reldir's own percent-encoding,
//! which [`render`] writes and [`parse`] reads. [`NamingRule::Verbatim`]
//! uses the key text as the name, and admits only characters the folder's
//! names were seen to use: a key needing any other is refused
//! ([`NamingError::OutsideAlphabet`]) rather than escaped by a convention the
//! folder never had. [`NamingRule::Template`] wraps either in a fixed prefix
//! and suffix, as in `user-17.json`.
//!
//! # Spelling, not equality
//!
//! A file name is built from the key *as it is spelled*: a string exactly as
//! written -- no Unicode normalization, so a key in NFD names an NFD file -- an
//! integer by its exact lexeme, a timestamp as written rather than in UTC, a
//! decimal in the canonical text its column already requires. Whether two keys
//! are *the same key* is a different question, answered by uniqueness over the
//! normalized values elsewhere; this module only maps spellings to names and
//! back.

use crate::canonical::MAX_FILENAME_BYTES;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, fmt, str::FromStr};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// The extension every row file has.
pub const EXTENSION: &str = ".json";

/// Why a key cannot be rendered as a file name, or a name read as a key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NamingError {
    #[error(
        "{value:?} holds {char:?}, which no file name in this table uses; reldir does not invent \
         an escaping convention the folder never had"
    )]
    OutsideAlphabet { value: String, char: char },
    #[error(
        "the file name would be {bytes} bytes, more than the {MAX_FILENAME_BYTES} every supported \
         filesystem allows"
    )]
    TooLong { bytes: usize },
    #[error("{stem:?} is a Windows device name, which no portable file can have")]
    DeviceName { stem: String },
    #[error("an empty key names no file")]
    Empty,
    #[error("{value:?} holds a path separator or NUL, which no file name can")]
    ContainsSeparator { value: String },
    #[error(
        "{stem:?} begins with '.', which would make the row a hidden file that many programs skip"
    )]
    Hidden { stem: String },
    #[error("this naming rule takes {expected} key value(s), not {got}")]
    Arity { expected: usize, got: usize },
    #[error("{found} cannot name a file: a key is a string or an integer")]
    Unkeyable { found: &'static str },
    #[error("{stem:?} is not a name this rule produces: {reason}")]
    NotThisRule { stem: String, reason: String },
    #[error("invalid naming rule: {reason}")]
    InvalidRule { reason: String },
}

fn invalid(reason: impl Into<String>) -> NamingError {
    NamingError::InvalidRule {
        reason: reason.into(),
    }
}

/// Whether a file name fits a filesystem whose names hold at most `name_max`
/// bytes.
pub fn fits(name: &str, name_max: usize) -> bool {
    name.len() <= name_max
}

/// The text of a key value, spelled as the value is written. See the module
/// documentation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyText(String);

impl KeyText {
    /// The key text of a JSON value: a string as written, an integer by its
    /// exact lexeme. Anything else -- null, a boolean, a number with a
    /// fraction or exponent, an array, an object -- names no file.
    pub fn of(value: &Value) -> Result<Self, NamingError> {
        match value {
            Value::String(text) => Ok(Self(text.clone())),
            Value::Number(number) => {
                let lexeme = number.to_string();
                if lexeme.contains(['.', 'e', 'E']) {
                    Err(NamingError::Unkeyable {
                        found: "a number with a fraction or exponent",
                    })
                } else {
                    Ok(Self(lexeme))
                }
            }
            Value::Null => Err(NamingError::Unkeyable { found: "null" }),
            Value::Bool(_) => Err(NamingError::Unkeyable { found: "a boolean" }),
            Value::Array(_) => Err(NamingError::Unkeyable { found: "an array" }),
            Value::Object(_) => Err(NamingError::Unkeyable { found: "an object" }),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for KeyText {
    fn from(text: &str) -> Self {
        Self(text.to_string())
    }
}

impl From<String> for KeyText {
    fn from(text: String) -> Self {
        Self(text)
    }
}

/// A set of characters, written as a compact character class: single
/// characters and `lo-hi` ranges, in code point order, e.g. `0-9_a-z`.
///
/// `\\` and `\-` stand for a backslash and a hyphen; `\u{HEX}` stands for any
/// character, and is how control characters, whitespace and combining marks
/// are written so the description stays legible. An alphabet is never empty
/// and never holds `/` or NUL, which no file name can.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Alphabet {
    /// Sorted, disjoint, non-adjacent inclusive ranges.
    ranges: Vec<(char, char)>,
}

fn successor(c: char) -> Option<char> {
    char::from_u32(c as u32 + 1)
}

impl Alphabet {
    /// The alphabet of exactly these characters.
    pub fn from_chars(chars: impl IntoIterator<Item = char>) -> Result<Self, NamingError> {
        Self::from_ranges(chars.into_iter().map(|c| (c, c)).collect())
    }

    /// The alphabet a folder's names show it uses. A name is written in
    /// kinds of characters, not in a fixed set of them: an ASCII letter or
    /// digit stands for its whole class (`a-z`, `A-Z`, `0-9`), and any
    /// character beyond ASCII for every character beyond ASCII and its
    /// controls. Anything else -- punctuation, a space -- is admitted only
    /// where it was seen, since it is what decides how a name reads.
    pub fn observed(chars: impl IntoIterator<Item = char>) -> Result<Self, NamingError> {
        Self::from_ranges(
            chars
                .into_iter()
                .map(|c| match c {
                    'a'..='z' => ('a', 'z'),
                    'A'..='Z' => ('A', 'Z'),
                    '0'..='9' => ('0', '9'),
                    '\u{a0}'.. => ('\u{a0}', char::MAX),
                    other => (other, other),
                })
                .collect(),
        )
    }

    fn from_ranges(mut ranges: Vec<(char, char)>) -> Result<Self, NamingError> {
        if ranges.is_empty() {
            return Err(invalid("an alphabet holds at least one character"));
        }
        for forbidden in ['/', '\0'] {
            if ranges
                .iter()
                .any(|(lo, hi)| (*lo..=*hi).contains(&forbidden))
            {
                return Err(invalid(format!(
                    "an alphabet cannot hold {forbidden:?}, which no file name can"
                )));
            }
        }
        ranges.sort();
        let mut merged: Vec<(char, char)> = Vec::with_capacity(ranges.len());
        for (lo, hi) in ranges {
            match merged.last_mut() {
                Some(last) if lo <= last.1 || successor(last.1) == Some(lo) => {
                    last.1 = last.1.max(hi);
                }
                _ => merged.push((lo, hi)),
            }
        }
        Ok(Self { ranges: merged })
    }

    pub fn contains(&self, c: char) -> bool {
        self.ranges
            .binary_search_by(|(lo, hi)| {
                if *hi < c {
                    std::cmp::Ordering::Less
                } else if *lo > c {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    /// Read a character-class description.
    pub fn parse(description: &str) -> Result<Self, NamingError> {
        enum Atom {
            Char(char),
            Dash,
        }
        let mut atoms = vec![];
        let mut chars = description.chars();
        while let Some(c) = chars.next() {
            atoms.push(match c {
                '\\' => match chars.next() {
                    Some('\\') => Atom::Char('\\'),
                    Some('-') => Atom::Char('-'),
                    Some('u') => {
                        if chars.next() != Some('{') {
                            return Err(invalid("\\u is written \\u{HEX}"));
                        }
                        let hex: String = chars.by_ref().take_while(|c| *c != '}').collect();
                        if hex.is_empty()
                            || hex.len() > 6
                            || !hex.chars().all(|c| c.is_ascii_hexdigit())
                        {
                            return Err(invalid(format!("\\u{{{hex}}} is not a code point")));
                        }
                        let code = u32::from_str_radix(&hex, 16)
                            .map_err(|_| invalid(format!("\\u{{{hex}}} is not a code point")))?;
                        Atom::Char(
                            char::from_u32(code).ok_or_else(|| {
                                invalid(format!("\\u{{{hex}}} is not a character"))
                            })?,
                        )
                    }
                    Some(other) => {
                        return Err(invalid(format!(
                            "\\{other} is not an escape (write \\\\, \\- or \\u{{HEX}})"
                        )));
                    }
                    None => return Err(invalid("a description cannot end in a lone \\")),
                },
                '-' => Atom::Dash,
                c => Atom::Char(c),
            });
        }
        let mut ranges = vec![];
        let mut index = 0;
        while index < atoms.len() {
            let Atom::Char(lo) = atoms[index] else {
                return Err(invalid(
                    "a '-' must join two characters (write \\- for a hyphen)",
                ));
            };
            match (atoms.get(index + 1), atoms.get(index + 2)) {
                (Some(Atom::Dash), Some(Atom::Char(hi))) => {
                    if *hi < lo {
                        return Err(invalid(format!("the range {lo}-{hi} is reversed")));
                    }
                    ranges.push((lo, *hi));
                    index += 3;
                }
                (Some(Atom::Dash), _) => {
                    return Err(invalid(
                        "a '-' must join two characters (write \\- for a hyphen)",
                    ));
                }
                _ => {
                    ranges.push((lo, lo));
                    index += 1;
                }
            }
        }
        Self::from_ranges(ranges)
    }

    /// The canonical description: ranges of three or more characters as
    /// `lo-hi`, everything in code point order.
    pub fn describe(&self) -> String {
        fn escape(c: char, out: &mut String) {
            match c {
                '\\' => out.push_str("\\\\"),
                '-' => out.push_str("\\-"),
                c if c.is_control() || c.is_whitespace() || is_combining_mark(c) => {
                    out.push_str(&format!("\\u{{{:X}}}", c as u32))
                }
                c => out.push(c),
            }
        }
        let mut out = String::new();
        for (lo, hi) in &self.ranges {
            escape(*lo, &mut out);
            if lo == hi {
                continue;
            }
            if successor(*lo) != Some(*hi) {
                out.push('-');
            }
            escape(*hi, &mut out);
        }
        out
    }
}

impl fmt::Display for Alphabet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

impl FromStr for Alphabet {
    type Err = NamingError;
    fn from_str(description: &str) -> Result<Self, Self::Err> {
        Self::parse(description)
    }
}

impl Serialize for Alphabet {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.describe())
    }
}

impl<'de> Deserialize<'de> for Alphabet {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let description = String::deserialize(deserializer)?;
        Self::parse(&description).map_err(serde::de::Error::custom)
    }
}

/// How key values spell a file stem.
///
/// Serialized as `x-reldir.identity.naming`: `{"rule": "reldir"}`,
/// `{"rule": "verbatim", "alphabet": "<class>"}`, or `{"rule": "template",
/// "prefix": .., "suffix": .., "inner": {..}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawRule", into = "RawRule")]
pub enum NamingRule {
    /// reldir's percent-encoding, as [`render`] writes it:
    /// ASCII letters, digits, `_`, `-` and `.` literal, except a leading or
    /// trailing `.`; every other byte `%HH` in upper case; a key that is a
    /// Windows device name has its first byte encoded; the components of a
    /// composite key joined with `,`.
    Reldir,
    /// The key text itself, which may use only the characters of `alphabet`.
    /// Takes exactly one key value.
    Verbatim { alphabet: Alphabet },
    /// `prefix`, the inner rule's name, `suffix`. The inner rule is `reldir`
    /// or `verbatim`, and at least one of prefix and suffix is non-empty.
    Template {
        prefix: String,
        suffix: String,
        inner: Box<NamingRule>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "lowercase", deny_unknown_fields)]
enum RawRule {
    // A struct variant, not a unit one: serde lets a unit variant of an
    // internally tagged enum carry unknown members silently.
    Reldir {},
    Verbatim {
        alphabet: Alphabet,
    },
    Template {
        prefix: String,
        suffix: String,
        inner: Box<NamingRule>,
    },
}

impl TryFrom<RawRule> for NamingRule {
    type Error = NamingError;
    fn try_from(raw: RawRule) -> Result<Self, Self::Error> {
        match raw {
            RawRule::Reldir {} => Ok(Self::Reldir),
            RawRule::Verbatim { alphabet } => Ok(Self::Verbatim { alphabet }),
            RawRule::Template {
                prefix,
                suffix,
                inner,
            } => Self::template(prefix, suffix, *inner),
        }
    }
}

impl From<NamingRule> for RawRule {
    fn from(rule: NamingRule) -> Self {
        match rule {
            NamingRule::Reldir => Self::Reldir {},
            NamingRule::Verbatim { alphabet } => Self::Verbatim { alphabet },
            NamingRule::Template {
                prefix,
                suffix,
                inner,
            } => Self::Template {
                prefix,
                suffix,
                inner,
            },
        }
    }
}

impl NamingRule {
    /// A template, refusing one that could not name a file or that adds
    /// nothing to its inner rule.
    pub fn template(
        prefix: String,
        suffix: String,
        inner: NamingRule,
    ) -> Result<Self, NamingError> {
        if prefix.is_empty() && suffix.is_empty() {
            return Err(invalid(
                "a template with neither prefix nor suffix is its inner rule",
            ));
        }
        if let Some(text) = [&prefix, &suffix]
            .into_iter()
            .find(|text| text.contains(['/', '\0']))
        {
            return Err(invalid(format!(
                "{text:?} holds a path separator or NUL, which no file name can"
            )));
        }
        if matches!(inner, Self::Template { .. }) {
            return Err(invalid("a template's inner rule is reldir or verbatim"));
        }
        Ok(Self::Template {
            prefix,
            suffix,
            inner: Box::new(inner),
        })
    }

    /// How many key values the rule takes: `None` for any positive number.
    fn arity(&self) -> Option<usize> {
        match self {
            Self::Reldir => None,
            Self::Verbatim { .. } => Some(1),
            Self::Template { inner, .. } => inner.arity(),
        }
    }

    /// The stem before the checks that apply to a whole file name.
    fn render_part(&self, values: &[KeyText]) -> Result<String, NamingError> {
        match self {
            Self::Reldir => {
                if values.is_empty() {
                    return Err(NamingError::Arity {
                        expected: 1,
                        got: 0,
                    });
                }
                let single = values.len() == 1;
                Ok(values
                    .iter()
                    .map(|value| reldir_encode(value.as_str(), single))
                    .collect::<Vec<_>>()
                    .join(","))
            }
            Self::Verbatim { alphabet } => {
                let [value] = values else {
                    return Err(NamingError::Arity {
                        expected: 1,
                        got: values.len(),
                    });
                };
                let text = value.as_str();
                if text.contains(['/', '\0']) {
                    return Err(NamingError::ContainsSeparator {
                        value: text.to_string(),
                    });
                }
                if let Some(outside) = text.chars().find(|c| !alphabet.contains(*c)) {
                    return Err(NamingError::OutsideAlphabet {
                        value: text.to_string(),
                        char: outside,
                    });
                }
                Ok(text.to_string())
            }
            Self::Template {
                prefix,
                suffix,
                inner,
            } => Ok(format!("{prefix}{}{suffix}", inner.render_part(values)?)),
        }
    }

    /// The key values a stem spells, before checking that they spell it back.
    fn parse_part(&self, stem: &str) -> Result<Vec<String>, NamingError> {
        let not_this = |reason: String| NamingError::NotThisRule {
            stem: stem.to_string(),
            reason,
        };
        match self {
            Self::Reldir => stem
                .split(',')
                .map(|component| {
                    crate::canonical::percent_decode(component).ok_or_else(|| {
                        not_this(format!("{component:?} is not valid percent-encoded UTF-8"))
                    })
                })
                .collect(),
            Self::Verbatim { .. } => Ok(vec![stem.to_string()]),
            Self::Template {
                prefix,
                suffix,
                inner,
            } => {
                let middle = stem
                    .strip_prefix(prefix.as_str())
                    .and_then(|rest| rest.strip_suffix(suffix.as_str()))
                    .ok_or_else(|| not_this(format!("it is not {prefix:?} … {suffix:?}")))?;
                inner.parse_part(middle)
            }
        }
    }
}

/// The file stem (without `.json`) the key values name under a rule.
pub fn render(rule: &NamingRule, key_values: &[KeyText]) -> Result<String, NamingError> {
    if let Some(expected) = rule.arity()
        && expected != key_values.len()
    {
        return Err(NamingError::Arity {
            expected,
            got: key_values.len(),
        });
    }
    let stem = rule.render_part(key_values)?;
    if stem.is_empty() {
        return Err(NamingError::Empty);
    }
    if stem.starts_with('.') {
        return Err(NamingError::Hidden { stem });
    }
    if windows_device_name(&stem) {
        return Err(NamingError::DeviceName { stem });
    }
    let bytes = stem.len() + EXTENSION.len();
    if !fits(&format!("{stem}{EXTENSION}"), MAX_FILENAME_BYTES) {
        return Err(NamingError::TooLong { bytes });
    }
    Ok(stem)
}

/// The file stem a file-stem key names: the key's text itself, refused when no
/// portable file could have that name.
pub fn stem_name(key: &KeyText) -> Result<String, NamingError> {
    let stem = key.as_str().to_string();
    if stem.is_empty() {
        return Err(NamingError::Empty);
    }
    if stem.contains(['/', '\\', '\0']) {
        return Err(NamingError::ContainsSeparator { value: stem });
    }
    if stem.starts_with('.') {
        return Err(NamingError::Hidden { stem });
    }
    if windows_device_name(&stem) {
        return Err(NamingError::DeviceName { stem });
    }
    if !fits(&format!("{stem}{EXTENSION}"), MAX_FILENAME_BYTES) {
        return Err(NamingError::TooLong {
            bytes: stem.len() + EXTENSION.len(),
        });
    }
    Ok(stem)
}

impl Identity {
    /// The columns a row's key is made of, in order.
    pub fn columns(&self) -> Vec<String> {
        match self {
            Self::FileStem { column, .. } => vec![column.clone()],
            Self::Body { columns, .. } => columns.clone(),
        }
    }

    /// The column the file stem is exposed as, when the stem is the key.
    pub fn stem_column(&self) -> Option<(&str, StemKind)> {
        match self {
            Self::FileStem { column, kind } => Some((column.as_str(), *kind)),
            Self::Body { .. } => None,
        }
    }

    /// The file stem a row's key names.
    pub fn stem(&self, row: &Map<String, Value>) -> Result<String, NamingError> {
        match self {
            Self::FileStem { column, .. } => {
                let value = row.get(column).ok_or(NamingError::Empty)?;
                stem_name(&KeyText::of(value)?)
            }
            Self::Body { columns, rule } => {
                let keys = columns
                    .iter()
                    .map(|column| KeyText::of(row.get(column).unwrap_or(&Value::Null)))
                    .collect::<Result<Vec<_>, _>>()?;
                render(rule, &keys)
            }
        }
    }

    /// The value a file stem gives the stem column, when the stem is the key.
    pub fn stem_value(&self, stem: &str) -> Option<Value> {
        match self {
            Self::FileStem {
                kind: StemKind::Int,
                ..
            } => stem
                .parse::<i64>()
                .ok()
                .map(|number| Value::Number(number.into())),
            Self::FileStem {
                kind: StemKind::String,
                ..
            } => Some(Value::String(stem.to_string())),
            Self::Body { .. } => None,
        }
    }
}

/// The key values a file stem (without `.json`) spells under a rule: exactly
/// the values whose [`render`] is this stem. A composite stem yields one value
/// per component; matching them to columns is the caller's.
pub fn parse(rule: &NamingRule, stem: &str) -> Result<Vec<String>, NamingError> {
    if stem.is_empty() {
        return Err(NamingError::Empty);
    }
    let values = rule.parse_part(stem)?;
    let keys: Vec<KeyText> = values
        .iter()
        .map(|value| KeyText::from(value.as_str()))
        .collect();
    let rendered = render(rule, &keys)?;
    if rendered != stem {
        return Err(NamingError::NotThisRule {
            stem: stem.to_string(),
            reason: format!("its key values are spelled {rendered:?} under this rule"),
        });
    }
    Ok(values)
}

/// [`crate::canonical`]'s encoding of one key component.
fn reldir_encode(text: &str, single: bool) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    for (index, byte) in bytes.iter().enumerate() {
        let literal = byte.is_ascii_alphanumeric()
            || matches!(*byte, b'_' | b'-')
            || (*byte == b'.' && index != 0 && index + 1 < bytes.len());
        if literal {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    if single && windows_device_name(text) {
        // A device name is ASCII, so its first byte is one literal character.
        out = format!("%{:02X}{}", bytes[0], &out[1..]);
    }
    out
}

/// Whether a name is a Windows device name: `con`, `prn`, `aux`, `nul`,
/// `com1`-`com9` or `lpt1`-`lpt9`, in any case, before its first `.`.
fn windows_device_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name).to_ascii_lowercase();
    matches!(base.as_str(), "con" | "prn" | "aux" | "nul")
        || base
            .strip_prefix("com")
            .or_else(|| base.strip_prefix("lpt"))
            .is_some_and(|number| number.len() == 1 && matches!(number.as_bytes()[0], b'1'..=b'9'))
}

/// What the virtual file-stem column holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StemKind {
    String,
    /// Every stem is an integer in canonical `i64` text: no sign but `-`, no
    /// leading zeros, no `-0`.
    Int,
}

/// Where a row's identity comes from.
///
/// Serialized as `x-reldir.identity`: `{"source": "filename", "column": "id",
/// "type": "string" | "int"}` or `{"source": "body", "columns": [..],
/// "naming": {..}}`. Unknown members are refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawIdentity", into = "RawIdentity")]
pub enum Identity {
    /// The file stem is the key, exposed as the column `column`.
    FileStem { column: String, kind: StemKind },
    /// The key columns are in the body, and `rule` spells the file name from
    /// their values.
    Body {
        columns: Vec<String>,
        rule: NamingRule,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "source", deny_unknown_fields)]
enum RawIdentity {
    #[serde(rename = "filename")]
    FileStem {
        column: String,
        #[serde(rename = "type")]
        kind: StemKind,
    },
    #[serde(rename = "body")]
    Body {
        columns: Vec<String>,
        naming: NamingRule,
    },
}

impl TryFrom<RawIdentity> for Identity {
    type Error = NamingError;
    fn try_from(raw: RawIdentity) -> Result<Self, Self::Error> {
        match raw {
            RawIdentity::FileStem { column, kind } => {
                if column.is_empty() {
                    return Err(invalid("the file-stem column needs a name"));
                }
                Ok(Self::FileStem { column, kind })
            }
            RawIdentity::Body { columns, naming } => {
                if columns.is_empty() || columns.iter().any(String::is_empty) {
                    return Err(invalid("a body identity names one or more columns"));
                }
                let mut seen = std::collections::HashSet::new();
                if let Some(repeated) = columns.iter().find(|column| !seen.insert(*column)) {
                    return Err(invalid(format!("{repeated:?} is named twice")));
                }
                if let Some(expected) = naming.arity()
                    && expected != columns.len()
                {
                    return Err(invalid(format!(
                        "the naming rule takes {expected} key column(s), not {}",
                        columns.len()
                    )));
                }
                Ok(Self::Body {
                    columns,
                    rule: naming,
                })
            }
        }
    }
}

impl From<Identity> for RawIdentity {
    fn from(identity: Identity) -> Self {
        match identity {
            Identity::FileStem { column, kind } => Self::FileStem { column, kind },
            Identity::Body { columns, rule } => Self::Body {
                columns,
                naming: rule,
            },
        }
    }
}

/// What inference concluded about one column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The column's value names every file under this rule.
    Names(NamingRule),
    /// Why it does not.
    Rejected(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnEvidence {
    pub column: String,
    pub verdict: Verdict,
}

/// An inferred identity, with the evidence for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredIdentity {
    pub identity: Identity,
    /// One entry per column tried, in column order.
    pub evidence: Vec<ColumnEvidence>,
}

/// The identity a table's files already have.
///
/// `rows` pairs each file's stem (its name without `.json`) with its body;
/// `columns_in_order` is the table's columns in member order.
///
/// A column present in every row with a string or integer value is tried
/// under each rule, and matches when the rule renders every row's value as
/// exactly that row's stem: verbatim (with the alphabet the stems are written
/// in, [`Alphabet::observed`]), then
/// reldir's encoding, then a template around either. Verbatim is preferred to
/// reldir's encoding because when both match, no stem needed encoding. Among
/// matching columns `id` is preferred, then `<singular table>_id`, then the
/// first in column order. Composite keys are only ever declared.
///
/// When no column matches, the stem itself is the key, exposed as a column
/// named `id` -- or `_file`, `_file_2`, ... when the body already uses that
/// name -- typed `int` when every stem is a canonical 64-bit integer. A table
/// with no rows has no evidence of numeric names, so its stem is a string.
///
/// Inference never fails: whatever the files are called, the stems are
/// distinct, because one directory cannot hold two files of one name.
pub fn infer_identity(
    table: &str,
    rows: &[(String, &Map<String, Value>)],
    columns_in_order: &[String],
) -> InferredIdentity {
    let mut evidence = vec![];
    let mut matches: Vec<(String, NamingRule)> = vec![];
    if !rows.is_empty() {
        for column in columns_in_order {
            let verdict = match key_texts(rows, column) {
                Err(reason) => Verdict::Rejected(reason),
                Ok(texts) => match naming_rule(rows, &texts) {
                    Ok(rule) => {
                        matches.push((column.clone(), rule.clone()));
                        Verdict::Names(rule)
                    }
                    Err(reason) => Verdict::Rejected(reason),
                },
            };
            evidence.push(ColumnEvidence {
                column: column.clone(),
                verdict,
            });
        }
    }
    let conventional = format!("{}_id", singular(table));
    let chosen = matches
        .iter()
        .find(|(column, _)| column == "id")
        .or_else(|| matches.iter().find(|(column, _)| *column == conventional))
        .or_else(|| matches.first());
    let identity = match chosen {
        Some((column, rule)) => Identity::Body {
            columns: vec![column.clone()],
            rule: rule.clone(),
        },
        None => {
            let used = |name: &str| rows.iter().any(|(_, row)| row.contains_key(name));
            let column = if !used("id") {
                "id".to_string()
            } else if !used("_file") {
                "_file".to_string()
            } else {
                (2..)
                    .map(|n| format!("_file_{n}"))
                    .find(|name| !used(name))
                    .expect("finitely many rows use finitely many names")
            };
            let numeric = !rows.is_empty()
                && rows.iter().all(|(stem, _)| {
                    stem.parse::<i64>()
                        .is_ok_and(|number| number.to_string() == *stem)
                });
            Identity::FileStem {
                column,
                kind: if numeric {
                    StemKind::Int
                } else {
                    StemKind::String
                },
            }
        }
    };
    InferredIdentity { identity, evidence }
}

fn key_texts(rows: &[(String, &Map<String, Value>)], column: &str) -> Result<Vec<KeyText>, String> {
    rows.iter()
        .map(|(stem, row)| match row.get(column) {
            None => Err(format!("{stem}{EXTENSION} has no {column:?}")),
            Some(value) => {
                KeyText::of(value).map_err(|error| format!("{stem}{EXTENSION}: {error}"))
            }
        })
        .collect()
}

/// The rule under which every row's key text renders as its stem, preferring
/// verbatim, then reldir's encoding, then a template.
fn naming_rule(
    rows: &[(String, &Map<String, Value>)],
    texts: &[KeyText],
) -> Result<NamingRule, String> {
    let names_all = |rule: &NamingRule| -> Result<(), String> {
        for ((stem, _), text) in rows.iter().zip(texts) {
            match render(rule, std::slice::from_ref(text)) {
                Ok(rendered) if rendered == *stem => {}
                Ok(rendered) => {
                    return Err(format!(
                        "{stem}{EXTENSION} would be named {rendered}{EXTENSION}"
                    ));
                }
                Err(error) => return Err(format!("{stem}{EXTENSION}: {error}")),
            }
        }
        Ok(())
    };
    let verbatim = Alphabet::observed(rows.iter().flat_map(|(stem, _)| stem.chars()))
        .map_err(|error| error.to_string())
        .and_then(|alphabet| {
            let rule = NamingRule::Verbatim { alphabet };
            names_all(&rule).map(|()| rule)
        });
    let verbatim_reason = match verbatim {
        Ok(rule) => return Ok(rule),
        Err(reason) => reason,
    };
    let reldir_reason = match names_all(&NamingRule::Reldir) {
        Ok(()) => return Ok(NamingRule::Reldir),
        Err(reason) => reason,
    };
    match template(rows, texts, &names_all) {
        Some(rule) => Ok(rule),
        None => Err(format!(
            "verbatim: {verbatim_reason}; reldir encoding: {reldir_reason}; template: no fixed \
             prefix and suffix surround the value in every name"
        )),
    }
}

/// A template around the value: the prefix and suffix are read off the first
/// row, at each place its value occurs -- the longest prefix first -- and kept
/// if they surround every other row's value too.
fn template(
    rows: &[(String, &Map<String, Value>)],
    texts: &[KeyText],
    names_all: &dyn Fn(&NamingRule) -> Result<(), String>,
) -> Option<NamingRule> {
    let (first_stem, _) = rows.first()?;
    let first = texts.first()?;
    for verbatim_inner in [true, false] {
        let needle = if verbatim_inner {
            first.as_str().to_string()
        } else {
            reldir_encode(first.as_str(), true)
        };
        if needle.is_empty() {
            continue;
        }
        let mut starts: Vec<usize> = (0..first_stem.len())
            .filter(|start| first_stem.is_char_boundary(*start))
            .filter(|start| first_stem[*start..].starts_with(&needle))
            .collect();
        starts.reverse();
        for start in starts {
            let prefix = &first_stem[..start];
            let suffix = &first_stem[start + needle.len()..];
            let inner = if verbatim_inner {
                let middles: Option<Vec<&str>> = rows
                    .iter()
                    .map(|(stem, _)| {
                        stem.strip_prefix(prefix)
                            .and_then(|rest| rest.strip_suffix(suffix))
                    })
                    .collect();
                let Some(middles) = middles else { continue };
                match Alphabet::observed(middles.iter().flat_map(|middle| middle.chars())) {
                    Ok(alphabet) => NamingRule::Verbatim { alphabet },
                    Err(_) => continue,
                }
            } else {
                NamingRule::Reldir
            };
            let Ok(rule) = NamingRule::template(prefix.to_string(), suffix.to_string(), inner)
            else {
                continue;
            };
            if names_all(&rule).is_ok() {
                return Some(rule);
            }
        }
    }
    None
}

/// The conventional singular of a table name, for `<singular>_id`: `ies` to
/// `y`, a plural `es` after `s`, `x`, `z`, `ch` or `sh` dropped, otherwise a
/// final `s` that is not `ss` dropped.
fn singular(table: &str) -> String {
    if let Some(stem) = table.strip_suffix("ies")
        && !stem.is_empty()
    {
        return format!("{stem}y");
    }
    for plural in ["ses", "xes", "zes", "ches", "shes"] {
        if let Some(stem) = table.strip_suffix(plural)
            && !stem.is_empty()
        {
            return format!("{stem}{}", &plural[..plural.len() - 2]);
        }
    }
    match table.strip_suffix('s') {
        Some(stem) if !stem.is_empty() && !stem.ends_with('s') => stem.to_string(),
        _ => table.to_string(),
    }
}

/// Pairs of stems that name one file on a filesystem that ignores case or
/// normalizes Unicode -- the same folding reldir's scan reports as a path
/// collision. A portability warning: on this filesystem they are two files.
/// Each colliding stem is paired with the least of its group; pairs are
/// sorted.
pub fn fold_collisions<'a>(stems: impl IntoIterator<Item = &'a str>) -> Vec<(String, String)> {
    let mut groups: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for stem in stems {
        groups.entry(fold(stem)).or_default().push(stem);
    }
    let mut out = vec![];
    for mut group in groups.into_values() {
        group.sort_unstable();
        group.dedup();
        if let Some((first, rest)) = group.split_first() {
            out.extend(
                rest.iter()
                    .map(|other| (first.to_string(), other.to_string())),
            );
        }
    }
    out.sort();
    out
}

fn fold(name: &str) -> String {
    if name.is_ascii() {
        name.to_ascii_lowercase()
    } else {
        name.nfc().flat_map(char::to_lowercase).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn keys(values: &[&str]) -> Vec<KeyText> {
        values.iter().map(|value| KeyText::from(*value)).collect()
    }

    fn verbatim(description: &str) -> NamingRule {
        NamingRule::Verbatim {
            alphabet: Alphabet::parse(description).unwrap(),
        }
    }

    /// A folder as `(stem, body)` pairs, the way a table's files would be read.
    fn folder(files: &[(&str, Value)]) -> Vec<(String, Map<String, Value>)> {
        files
            .iter()
            .map(|(stem, body)| (stem.to_string(), body.as_object().unwrap().clone()))
            .collect()
    }

    fn infer(table: &str, files: &[(&str, Value)]) -> InferredIdentity {
        let owned = folder(files);
        let rows: Vec<(String, &Map<String, Value>)> = owned
            .iter()
            .map(|(stem, body)| (stem.clone(), body))
            .collect();
        let mut columns: Vec<String> = vec![];
        for (_, body) in &owned {
            for name in body.keys() {
                if !columns.contains(name) {
                    columns.push(name.clone());
                }
            }
        }
        infer_identity(table, &rows, &columns)
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, .. ProptestConfig::default() })]

        #[test]
        fn test8701_parse_inverts_render_under_every_rule(
            values in prop::collection::vec("\\PC{1,6}", 1..4),
            prefix in "[a-z]{0,3}-?", suffix in "(\\.v[0-9])?"
        ) {
            let texts: Vec<&str> = values.iter().map(String::as_str).collect();
            let reldir = render(&NamingRule::Reldir, &keys(&texts)).unwrap();
            prop_assert_eq!(parse(&NamingRule::Reldir, &reldir).unwrap(), values.clone());

            let alphabet = Alphabet::from_chars(values[0].chars().filter(|c| !matches!(c, '/' | '\0')))
                .unwrap_or_else(|_| Alphabet::parse("a").unwrap());
            let plain = NamingRule::Verbatim { alphabet };
            if let Ok(stem) = render(&plain, &keys(&texts[..1])) {
                prop_assert_eq!(&stem, &values[0]);
                prop_assert_eq!(parse(&plain, &stem).unwrap(), vec![values[0].clone()]);
            }
            if let Ok(rule) = NamingRule::template(prefix, suffix, NamingRule::Reldir) {
                match render(&rule, &keys(&texts)) {
                    Ok(stem) => prop_assert_eq!(parse(&rule, &stem).unwrap(), values.clone()),
                    // A prefix such as "com" before the value "1".
                    Err(NamingError::DeviceName { stem }) => prop_assert!(windows_device_name(&stem)),
                    Err(other) => prop_assert!(false, "{}", other),
                }
            }
        }

        #[test]
        fn test8702_an_alphabet_description_reads_back_as_the_same_alphabet(
            chars in prop::collection::vec(any::<char>().prop_filter("file name", |c| !matches!(c, '/' | '\0')), 1..30)
        ) {
            let alphabet = Alphabet::from_chars(chars.iter().copied()).unwrap();
            let description = alphabet.describe();
            let read = Alphabet::parse(&description).unwrap();
            prop_assert_eq!(&read, &alphabet);
            for c in &chars {
                prop_assert!(read.contains(*c));
            }
            let json = serde_json::to_string(&NamingRule::Verbatim { alphabet: alphabet.clone() }).unwrap();
            prop_assert_eq!(
                serde_json::from_str::<NamingRule>(&json).unwrap(),
                NamingRule::Verbatim { alphabet }
            );
        }

        #[test]
        fn test8703_inference_on_any_two_rows_is_one_deterministic_answer(
            a in "[a-z0-9]{1,6}", b in "[a-z0-9]{1,6}", x in "[a-z0-9]{1,6}", y in "[a-z0-9]{1,6}"
        ) {
            prop_assume!(a != b);
            // A device name cannot be a portable file name, so no folder has one.
            prop_assume!(!windows_device_name(&a) && !windows_device_name(&b));
            let files = [
                (a.as_str(), json!({"slug": a, "other": x})),
                (b.as_str(), json!({"slug": b, "other": y})),
            ];
            let forward = infer("things", &files);
            let backward = {
                let owned = folder(&files);
                let rows: Vec<(String, &Map<String, Value>)> =
                    owned.iter().rev().map(|(stem, body)| (stem.clone(), body)).collect();
                infer_identity("things", &rows, &["slug".into(), "other".into()])
            };
            prop_assert_eq!(&forward.identity, &backward.identity);
            let Identity::Body { columns, rule } = &forward.identity else {
                return Err(TestCaseError::fail(format!("{:?}", forward.identity)));
            };
            prop_assert_eq!(columns, &vec!["slug".to_string()]);
            for (stem, value) in [(&a, &a), (&b, &b)] {
                prop_assert_eq!(&render(rule, &keys(&[value])).unwrap(), stem);
            }
        }
    }

    #[test]
    fn test8704_a_folder_whose_names_are_not_in_the_body_keys_rows_by_file_stem() {
        let found = infer(
            "people",
            &[
                ("ada", json!({"name": "Ada Lovelace", "born": 1815})),
                ("grace", json!({"name": "Grace Hopper", "born": 1906})),
            ],
        );
        assert_eq!(
            found.identity,
            Identity::FileStem {
                column: "id".into(),
                kind: StemKind::String
            }
        );
        assert!(
            found
                .evidence
                .iter()
                .all(|evidence| matches!(evidence.verdict, Verdict::Rejected(_))),
            "{:?}",
            found.evidence
        );
        let Verdict::Rejected(reason) = &found.evidence[0].verdict else {
            unreachable!()
        };
        assert!(
            reason.contains("ada.json would be named Ada%20Lovelace.json"),
            "{reason}"
        );

        let numbered = infer(
            "events",
            &[("1", json!({"id": "x"})), ("20", json!({"id": "y"}))],
        );
        assert_eq!(
            numbered.identity,
            Identity::FileStem {
                column: "_file".into(),
                kind: StemKind::Int
            },
            "the body's id is not the name, so the stem takes another column name"
        );
        let crowded = infer(
            "events",
            &[
                ("007", json!({"id": "x", "_file": 1})),
                ("8", json!({"_file_2": 2})),
            ],
        );
        assert_eq!(
            crowded.identity,
            Identity::FileStem {
                column: "_file_3".into(),
                kind: StemKind::String
            },
            "007 is not a canonical integer"
        );
        assert_eq!(
            infer_identity("empty", &[], &[]).identity,
            Identity::FileStem {
                column: "id".into(),
                kind: StemKind::String
            }
        );
    }

    #[test]
    fn test8705_a_body_key_spelled_as_the_name_is_verbatim_with_the_observed_alphabet() {
        let found = infer(
            "users",
            &[
                ("u1", json!({"id": "u1", "name": "Ann"})),
                ("u2", json!({"id": "u2", "name": "Bob"})),
            ],
        );
        let Identity::Body { columns, rule } = &found.identity else {
            panic!("{:?}", found.identity)
        };
        assert_eq!(columns, &["id"]);
        assert_eq!(
            rule,
            &verbatim("0-9a-z"),
            "names written in lower-case letters and digits admit any of them"
        );
        assert_eq!(render(rule, &keys(&["u21"])).unwrap(), "u21");
        assert_eq!(render(rule, &keys(&["alan3"])).unwrap(), "alan3");
        assert_eq!(
            render(rule, &keys(&["u 3"])),
            Err(NamingError::OutsideAlphabet {
                value: "u 3".into(),
                char: ' '
            }),
            "no name holds a space, so reldir has no evidence of how one is written"
        );
        assert_eq!(
            render(rule, &keys(&["U3"])),
            Err(NamingError::OutsideAlphabet {
                value: "U3".into(),
                char: 'U'
            }),
            "no name holds an upper-case letter"
        );
        assert_eq!(
            serde_json::to_value(&found.identity).unwrap(),
            json!({"source": "body", "columns": ["id"], "naming": {"rule": "verbatim", "alphabet": "0-9a-z"}})
        );
    }

    #[test]
    fn test8706_a_fixed_prefix_around_the_key_is_a_template() {
        let found = infer(
            "users",
            &[
                ("user-17", json!({"id": 17, "name": "a"})),
                ("user-18", json!({"id": 18, "name": "b"})),
                ("user-170", json!({"id": 170, "name": "c"})),
            ],
        );
        let Identity::Body { columns, rule } = &found.identity else {
            panic!("{:?}", found.identity)
        };
        assert_eq!(columns, &["id"]);
        assert_eq!(
            rule,
            &NamingRule::template("user-".into(), String::new(), verbatim("0-9")).unwrap()
        );
        assert_eq!(
            render(rule, &[KeyText::of(&json!(1078)).unwrap()]).unwrap(),
            "user-1078"
        );
        assert_eq!(parse(rule, "user-17").unwrap(), ["17"]);
        assert!(matches!(
            parse(rule, "member-17"),
            Err(NamingError::NotThisRule { .. })
        ));
        assert_eq!(
            serde_json::to_value(rule).unwrap(),
            json!({"rule": "template", "prefix": "user-", "suffix": "", "inner": {"rule": "verbatim", "alphabet": "0-9"}})
        );

        // One row whose value occurs twice: the longest prefix is taken.
        let repeated = infer("pairs", &[("17-17", json!({"id": 17}))]);
        assert_eq!(
            repeated.identity,
            Identity::Body {
                columns: vec!["id".into()],
                rule: NamingRule::template("17-".into(), String::new(), verbatim("0-9")).unwrap()
            }
        );

        let suffixed = infer(
            "notes",
            &[
                ("a.v1", json!({"note_id": "a"})),
                ("bb.v1", json!({"note_id": "bb"})),
            ],
        );
        assert!(matches!(
            &suffixed.identity,
            Identity::Body { columns, rule: NamingRule::Template { prefix, suffix, .. } }
                if columns == &["note_id"] && prefix.is_empty() && suffix == ".v1"
        ));
    }

    #[test]
    fn test8707_percent_encoded_names_are_reldirs_rule_and_verbatim_wins_a_tie() {
        let found = infer(
            "files",
            &[
                ("a%2Fb", json!({"path": "a/b"})),
                ("plain", json!({"path": "plain"})),
                ("%2Ehidden", json!({"path": ".hidden"})),
            ],
        );
        assert_eq!(
            found.identity,
            Identity::Body {
                columns: vec!["path".into()],
                rule: NamingRule::Reldir
            }
        );
        assert_eq!(
            serde_json::to_value(&found.identity).unwrap(),
            json!({"source": "body", "columns": ["path"], "naming": {"rule": "reldir"}})
        );
        let tie = infer(
            "files",
            &[("a", json!({"path": "a"})), ("b", json!({"path": "b"}))],
        );
        assert!(matches!(
            &tie.identity,
            Identity::Body {
                rule: NamingRule::Verbatim { .. },
                ..
            }
        ));
    }

    #[test]
    fn test8708_the_conventional_key_is_preferred_over_an_earlier_column() {
        let files = [
            ("a", json!({"slug": "a", "order_id": "a", "id": "a"})),
            ("b", json!({"slug": "b", "order_id": "b", "id": "b"})),
        ];
        let Identity::Body { columns, .. } = infer("orders", &files).identity else {
            panic!()
        };
        assert_eq!(columns, ["id"]);
        let files = [
            ("a", json!({"slug": "a", "order_id": "a"})),
            ("b", json!({"slug": "b", "order_id": "b"})),
        ];
        let Identity::Body { columns, .. } = infer("orders", &files).identity else {
            panic!()
        };
        assert_eq!(columns, ["order_id"]);
        let Identity::Body { columns, .. } = infer("things", &files).identity else {
            panic!()
        };
        assert_eq!(columns, ["slug"], "otherwise the first matching column");
        assert_eq!(singular("categories"), "category");
        assert_eq!(singular("boxes"), "box");
        assert_eq!(singular("class"), "class");
        assert_eq!(singular("people"), "people");
    }

    #[test]
    fn test8709_keys_are_spelled_as_written() {
        let nfd = "e\u{301}cole";
        let rule = verbatim(&Alphabet::from_chars(nfd.chars()).unwrap().describe());
        let stem = render(&rule, &keys(&[nfd])).unwrap();
        assert_eq!(stem, nfd, "an NFD key names an NFD file");
        assert_ne!(stem, "\u{e9}cole");
        assert_eq!(
            render(&NamingRule::Reldir, &keys(&[nfd])).unwrap(),
            "e%CC%81cole",
            "reldir encodes the combining mark it is given, not a composed form"
        );
        assert_eq!(
            Alphabet::from_chars(nfd.chars()).unwrap().describe(),
            "celo\\u{301}",
            "a combining mark is written as an escape"
        );

        let parse_value = |text: &str| -> Value { serde_json::from_str(text).unwrap() };
        assert_eq!(KeyText::of(&parse_value("17")).unwrap().as_str(), "17");
        assert_eq!(
            KeyText::of(&parse_value("123456789012345678901234567890"))
                .unwrap()
                .as_str(),
            "123456789012345678901234567890",
            "an integer is named by its exact lexeme"
        );
        assert_eq!(
            KeyText::of(&json!("2026-09-14T12:00:00+02:00"))
                .unwrap()
                .as_str(),
            "2026-09-14T12:00:00+02:00",
            "a timestamp is named as written, not in UTC"
        );
        for (value, found) in [
            (parse_value("1.5"), "a number with a fraction or exponent"),
            (parse_value("1e3"), "a number with a fraction or exponent"),
            (json!(null), "null"),
            (json!(true), "a boolean"),
            (json!([1]), "an array"),
            (json!({}), "an object"),
        ] {
            assert_eq!(KeyText::of(&value), Err(NamingError::Unkeyable { found }));
        }
    }

    #[test]
    fn test8700_reldir_names_escape_exactly_what_is_unsafe_and_read_back() {
        for (key, expected) in [
            ("a/b", "a%2Fb"),
            ("a,b", "a%2Cb"),
            (".hidden", "%2Ehidden"),
            ("..", "%2E%2E"),
            ("plain-name_1.v2", "plain-name_1.v2"),
            ("é", "%C3%A9"),
            ("console", "console"),
            ("COM10", "COM10"),
        ] {
            let stem = render(&NamingRule::Reldir, &keys(&[key])).unwrap();
            assert_eq!(stem, expected);
            assert_eq!(parse(&NamingRule::Reldir, &stem).unwrap(), [key]);
        }
        for reserved in ["CON", "con", "PRN", "aux", "NUL", "COM1", "lpt9"] {
            let stem = render(&NamingRule::Reldir, &keys(&[reserved])).unwrap();
            assert!(
                stem.starts_with('%'),
                "{reserved} must be escaped, got {stem}"
            );
            assert_eq!(parse(&NamingRule::Reldir, &stem).unwrap(), [reserved]);
        }
        assert_eq!(
            render(&NamingRule::Reldir, &keys(&["x,y", "z"])).unwrap(),
            "x%2Cy,z",
            "a composite key joins its encoded components"
        );
        // 100 two-byte characters encode to 600 bytes of name.
        assert_eq!(
            render(&NamingRule::Reldir, &keys(&[&"é".repeat(100)])),
            Err(NamingError::TooLong {
                bytes: 600 + EXTENSION.len()
            })
        );
    }

    #[test]
    fn test8710_names_no_filesystem_could_hold_are_refused() {
        let any = verbatim("\\u{20}-.0-~");
        assert_eq!(render(&any, &keys(&[""])), Err(NamingError::Empty));
        assert_eq!(
            render(&NamingRule::Reldir, &keys(&[""])),
            Err(NamingError::Empty)
        );
        assert_eq!(
            render(&any, &keys(&["a/b"])),
            Err(NamingError::ContainsSeparator {
                value: "a/b".into()
            })
        );
        assert_eq!(
            render(&any, &keys(&["CON"])),
            Err(NamingError::DeviceName { stem: "CON".into() })
        );
        assert_eq!(
            render(&any, &keys(&["lpt1.txt"])),
            Err(NamingError::DeviceName {
                stem: "lpt1.txt".into()
            })
        );
        assert_eq!(render(&any, &keys(&["COM10"])).unwrap(), "COM10");
        assert_eq!(
            render(&NamingRule::Reldir, &keys(&["con"])).unwrap(),
            "%63on"
        );
        assert_eq!(
            render(&any, &keys(&[".x"])),
            Err(NamingError::Hidden { stem: ".x".into() })
        );
        let longest = "a".repeat(MAX_FILENAME_BYTES - EXTENSION.len());
        assert_eq!(render(&any, &keys(&[&longest])).unwrap(), longest);
        assert_eq!(
            render(&any, &keys(&[&format!("{longest}a")])),
            Err(NamingError::TooLong { bytes: 256 })
        );
        assert!(fits("abc.json", 8));
        assert!(!fits("abcd.json", 8));
        assert_eq!(
            render(&any, &keys(&["a", "b"])),
            Err(NamingError::Arity {
                expected: 1,
                got: 2
            })
        );
        assert!(
            matches!(
                parse(&NamingRule::Reldir, "%61"),
                Err(NamingError::NotThisRule { .. })
            ),
            "a needlessly encoded letter is not reldir's spelling"
        );
        assert!(
            matches!(
                parse(&NamingRule::Reldir, "%2e"),
                Err(NamingError::NotThisRule { .. })
            ),
            "reldir's hex is upper case"
        );
        assert!(matches!(
            parse(&NamingRule::Reldir, "%FF"),
            Err(NamingError::NotThisRule { .. })
        ));
        assert_eq!(parse(&NamingRule::Reldir, "x%2Cy,z").unwrap(), ["x,y", "z"]);
    }

    #[test]
    fn test8711_identity_and_rule_documents_are_parsed_strictly() {
        let read = |value: Value| serde_json::from_value::<Identity>(value);
        assert_eq!(
            read(json!({"source": "filename", "column": "id", "type": "int"})).unwrap(),
            Identity::FileStem {
                column: "id".into(),
                kind: StemKind::Int
            }
        );
        let round = Identity::Body {
            columns: vec!["a".into(), "b".into()],
            rule: NamingRule::template("x-".into(), String::new(), NamingRule::Reldir).unwrap(),
        };
        assert_eq!(read(serde_json::to_value(&round).unwrap()).unwrap(), round);
        for (label, bad) in [
            (
                "unknown member",
                json!({"source": "filename", "column": "id", "type": "int", "x": 1}),
            ),
            (
                "unknown source",
                json!({"source": "path", "column": "id", "type": "int"}),
            ),
            (
                "unknown stem type",
                json!({"source": "filename", "column": "id", "type": "uuid"}),
            ),
            (
                "empty column",
                json!({"source": "filename", "column": "", "type": "string"}),
            ),
            (
                "no columns",
                json!({"source": "body", "columns": [], "naming": {"rule": "reldir"}}),
            ),
            (
                "repeated column",
                json!({"source": "body", "columns": ["a", "a"], "naming": {"rule": "reldir"}}),
            ),
            (
                "verbatim composite",
                json!({"source": "body", "columns": ["a", "b"], "naming": {"rule": "verbatim", "alphabet": "a-z"}}),
            ),
            (
                "unknown rule member",
                json!({"source": "body", "columns": ["a"], "naming": {"rule": "reldir", "x": 1}}),
            ),
            (
                "unknown rule",
                json!({"source": "body", "columns": ["a"], "naming": {"rule": "slug"}}),
            ),
            (
                "empty template",
                json!({"source": "body", "columns": ["a"], "naming": {"rule": "template", "prefix": "", "suffix": "", "inner": {"rule": "reldir"}}}),
            ),
            (
                "nested template",
                json!({"source": "body", "columns": ["a"], "naming": {"rule": "template", "prefix": "a", "suffix": "", "inner": {"rule": "template", "prefix": "b", "suffix": "", "inner": {"rule": "reldir"}}}}),
            ),
            (
                "separator in prefix",
                json!({"source": "body", "columns": ["a"], "naming": {"rule": "template", "prefix": "a/", "suffix": "", "inner": {"rule": "reldir"}}}),
            ),
            (
                "slash in alphabet",
                json!({"source": "body", "columns": ["a"], "naming": {"rule": "verbatim", "alphabet": "+-0"}}),
            ),
            (
                "reversed range",
                json!({"source": "body", "columns": ["a"], "naming": {"rule": "verbatim", "alphabet": "z-a"}}),
            ),
            (
                "dangling hyphen",
                json!({"source": "body", "columns": ["a"], "naming": {"rule": "verbatim", "alphabet": "a-"}}),
            ),
            (
                "empty alphabet",
                json!({"source": "body", "columns": ["a"], "naming": {"rule": "verbatim", "alphabet": ""}}),
            ),
            (
                "bad escape",
                json!({"source": "body", "columns": ["a"], "naming": {"rule": "verbatim", "alphabet": "\\n"}}),
            ),
        ] {
            assert!(read(bad.clone()).is_err(), "{label} must be refused: {bad}");
        }
        assert_eq!(
            Alphabet::parse("a-zA-Z0-9_\\-").unwrap().describe(),
            "\\-0-9A-Z_a-z",
            "any valid description is read, and written canonically"
        );
        assert_eq!(Alphabet::parse("ab").unwrap().describe(), "ab");
        assert_eq!(Alphabet::parse("a-c").unwrap().describe(), "a-c");
    }

    #[test]
    fn test8712_names_that_collide_when_folded_are_reported() {
        assert_eq!(
            fold_collisions([
                "Readme", "readme", "README", "other", "\u{e9}", "e\u{301}", "\u{c9}"
            ]),
            vec![
                ("README".to_string(), "Readme".to_string()),
                ("README".to_string(), "readme".to_string()),
                ("e\u{301}".to_string(), "\u{c9}".to_string()),
                ("e\u{301}".to_string(), "\u{e9}".to_string()),
            ]
        );
        assert!(fold_collisions(["a", "b"]).is_empty());
    }
}
