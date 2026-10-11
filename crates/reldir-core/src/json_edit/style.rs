//! How a JSON file is written, measured from the file.
//!
//! New content -- an inserted member, a replaced value -- is written the way
//! the content around it already is, so a file edited by reldir still looks
//! like it was written by one hand. A table records the style most of its
//! rows share in its schema (`x-reldir.style`), so a new row looks like its
//! siblings; that is why a [`Style`] serializes, and why it refuses any value
//! it could not write back.
//!
//! The serialized shape:
//!
//! ```json
//! {"indent": "  ", "colon": ": ", "comma": ", ", "newline": "\n",
//!  "final_newline": true, "bom": false, "empty": "{}", "padding": false,
//!  "inline_scalar_arrays": false}
//! ```
//!
//! `indent` is one level of indentation -- spaces or tabs -- or `""` for a
//! document written on one line. `comma` and `padding` describe containers
//! written on one line (`[1, 2]` against `[1,2]`, `{ "a": 1 }` against
//! `{"a": 1}`); `empty` is how an empty container is written; and
//! `inline_scalar_arrays` records that an indented document still writes an
//! array of plain values on one line, as hand-written files often do.
//!
//! Detection is a lexical pass that tolerates any bytes, so it can be asked
//! of a file that does not parse. Each property is decided by the majority of
//! the places that show it, ties going to the first seen. A property the file
//! shows nowhere -- the comma spacing of a document with no one-line
//! container, say -- takes a fallback, and [`Evidence`] says which properties
//! were observed and how often, so a caller combining many files can weigh
//! them.

use super::scan::{BOM, is_whitespace, line_indent, string_end, token_end};
use serde::{Deserialize, Serialize};

/// How a document is written.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Style {
    /// One level of indentation; empty for a one-line document.
    pub indent: Indent,
    /// The text between a member's name and its value.
    pub colon: Colon,
    /// The separator between members or elements of a one-line container.
    pub comma: Comma,
    pub newline: Newline,
    /// Whether the file ends with a line break.
    pub final_newline: bool,
    /// Whether the file begins with a UTF-8 byte order mark.
    pub bom: bool,
    /// How an empty object or array is written.
    pub empty: Empty,
    /// Whether a one-line container has a space inside its brackets.
    pub padding: bool,
    /// Whether an indented document writes a non-empty array of scalars on
    /// one line.
    pub inline_scalar_arrays: bool,
}

/// One level of indentation: up to 16 spaces or up to 4 tabs, or nothing for
/// a document written on one line.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Indent(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StyleError {
    #[error(
        "an indentation unit is 1 to {spaces} spaces or 1 to {tabs} tabs, or empty for one-line documents, not {0:?}",
        spaces = Indent::MAX_SPACES,
        tabs = Indent::MAX_TABS
    )]
    Indent(String),
}

impl Indent {
    pub const MAX_SPACES: usize = 16;
    pub const MAX_TABS: usize = 4;

    /// No indentation: the document is written on one line.
    pub fn compact() -> Self {
        Self(String::new())
    }

    pub fn spaces(count: usize) -> Result<Self, StyleError> {
        Self::new(&" ".repeat(count))
    }

    pub fn tabs(count: usize) -> Result<Self, StyleError> {
        Self::new(&"\t".repeat(count))
    }

    pub fn new(text: &str) -> Result<Self, StyleError> {
        let spaces = text.bytes().all(|byte| byte == b' ') && text.len() <= Self::MAX_SPACES;
        let tabs = text.bytes().all(|byte| byte == b'\t') && text.len() <= Self::MAX_TABS;
        if spaces || tabs {
            Ok(Self(text.to_string()))
        } else {
            Err(StyleError::Indent(text.to_string()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_compact(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<String> for Indent {
    type Error = StyleError;

    fn try_from(text: String) -> Result<Self, StyleError> {
        Self::new(&text)
    }
}

impl From<Indent> for String {
    fn from(indent: Indent) -> String {
        indent.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Colon {
    #[serde(rename = ":")]
    Tight,
    #[serde(rename = ": ")]
    Spaced,
    #[serde(rename = " : ")]
    Surrounded,
}

impl Colon {
    pub fn text(self) -> &'static str {
        match self {
            Colon::Tight => ":",
            Colon::Spaced => ": ",
            Colon::Surrounded => " : ",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Comma {
    #[serde(rename = ",")]
    Tight,
    #[serde(rename = ", ")]
    Spaced,
}

impl Comma {
    pub fn text(self) -> &'static str {
        match self {
            Comma::Tight => ",",
            Comma::Spaced => ", ",
        }
    }

    /// The whitespace after the comma.
    pub fn gap(self) -> &'static str {
        &self.text()[1..]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Newline {
    #[serde(rename = "\n")]
    Lf,
    #[serde(rename = "\r\n")]
    CrLf,
}

impl Newline {
    pub fn text(self) -> &'static str {
        match self {
            Newline::Lf => "\n",
            Newline::CrLf => "\r\n",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Empty {
    #[serde(rename = "{}")]
    Tight,
    #[serde(rename = "{ }")]
    Spaced,
}

impl Empty {
    /// The text between the brackets of an empty container.
    pub fn inner(self) -> &'static str {
        match self {
            Empty::Tight => "",
            Empty::Spaced => " ",
        }
    }
}

/// How many places in a file showed each property. A count of zero means the
/// property in the detected [`Style`] is a fallback, not an observation. The
/// byte order mark and final newline are always observable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Evidence {
    /// Members and elements that begin their own line, indented past the line
    /// their container opens on.
    pub indent: usize,
    /// Non-empty containers that span lines; any at all makes the document
    /// indented rather than one-line.
    pub multi_line: usize,
    pub colon: usize,
    pub comma: usize,
    pub newline: usize,
    pub empty: usize,
    pub padding: usize,
    pub inline_scalar_arrays: usize,
}

/// A detected style and what it was detected from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub style: Style,
    pub evidence: Evidence,
}

impl Style {
    /// The style of a table that has no rows to learn from: two-space
    /// indentation, `": "`, LF line endings and a final newline.
    pub fn default_for_new_table() -> Self {
        Self {
            indent: Indent("  ".to_string()),
            colon: Colon::Spaced,
            comma: Comma::Spaced,
            newline: Newline::Lf,
            final_newline: true,
            bom: false,
            empty: Empty::Tight,
            padding: false,
            inline_scalar_arrays: false,
        }
    }

    /// The style a file is written in.
    pub fn detect(bytes: &[u8]) -> Self {
        Self::detect_detailed(bytes).style
    }

    /// The style a file is written in, with the evidence for each property.
    pub fn detect_detailed(bytes: &[u8]) -> Detection {
        Survey::of(bytes).conclude(bytes)
    }

    /// Whether the document is written on one line.
    pub fn is_compact(&self) -> bool {
        self.indent.is_compact()
    }

    /// The same style with everything on one line: how content is written
    /// into a container that is itself on one line.
    pub fn single_line(&self) -> Self {
        Self {
            indent: Indent::compact(),
            ..self.clone()
        }
    }
}

/// Counts of the variants of one property, in the order first seen.
struct Tally<K>(Vec<(K, usize)>);

impl<K: PartialEq> Tally<K> {
    fn new() -> Self {
        Self(vec![])
    }

    fn add(&mut self, key: K) {
        match self.0.iter_mut().find(|(seen, _)| *seen == key) {
            Some((_, count)) => *count += 1,
            None => self.0.push((key, 1)),
        }
    }

    fn total(&self) -> usize {
        self.0.iter().map(|(_, count)| count).sum()
    }

    /// The most frequent variant; on a tie, the first seen.
    fn winner(self) -> Option<K> {
        let mut best: Option<(K, usize)> = None;
        for (key, count) in self.0 {
            if best.as_ref().is_none_or(|(_, top)| count > *top) {
                best = Some((key, count));
            }
        }
        best.map(|(key, _)| key)
    }
}

/// An open container during the survey.
struct Frame {
    open: usize,
    object: bool,
    /// The indentation of the line the container opens on.
    line_indent: Vec<u8>,
    children: usize,
    first_gap: Option<std::ops::Range<usize>>,
    comma_gaps: Vec<std::ops::Range<usize>>,
    all_scalar: bool,
    expect_name: bool,
}

enum Pending {
    Nothing,
    Colon(std::ops::Range<usize>),
    Comma,
}

struct Survey {
    bom: bool,
    steps: Tally<Indent>,
    multi_line: usize,
    colon: Tally<Colon>,
    comma: Tally<Comma>,
    empty: Tally<Empty>,
    padding: Tally<bool>,
    inline: Tally<bool>,
    lf: usize,
    crlf: usize,
}

impl Survey {
    fn of(bytes: &[u8]) -> Self {
        let bom = bytes.starts_with(BOM);
        let mut survey = Self {
            bom,
            steps: Tally::new(),
            multi_line: 0,
            colon: Tally::new(),
            comma: Tally::new(),
            empty: Tally::new(),
            padding: Tally::new(),
            inline: Tally::new(),
            lf: 0,
            crlf: 0,
        };
        for (at, byte) in bytes.iter().enumerate() {
            if *byte == b'\n' {
                if at > 0 && bytes[at - 1] == b'\r' {
                    survey.crlf += 1;
                } else {
                    survey.lf += 1;
                }
            }
        }
        let mut stack: Vec<Frame> = vec![];
        let mut pending = Pending::Nothing;
        let mut at = if bom { BOM.len() } else { 0 };
        loop {
            let gap_start = at;
            while bytes.get(at).copied().is_some_and(is_whitespace) {
                at += 1;
            }
            let gap = gap_start..at;
            let Some(&byte) = bytes.get(at) else { break };
            match std::mem::replace(&mut pending, Pending::Nothing) {
                Pending::Colon(before) => match (&bytes[before], &bytes[gap.clone()]) {
                    (b"", b"") => survey.colon.add(Colon::Tight),
                    (b"", b" ") => survey.colon.add(Colon::Spaced),
                    (b" ", b" ") => survey.colon.add(Colon::Surrounded),
                    _ => {}
                },
                Pending::Comma => {
                    if let Some(frame) = stack.last_mut()
                        && !bytes[gap.clone()].contains(&b'\n')
                    {
                        frame.comma_gaps.push(gap.clone());
                    }
                }
                Pending::Nothing => {}
            }
            match byte {
                b'{' | b'[' => {
                    survey.child(&mut stack, bytes, gap, true, false);
                    stack.push(Frame {
                        open: at,
                        object: byte == b'{',
                        line_indent: line_indent(bytes, at).to_vec(),
                        children: 0,
                        first_gap: None,
                        comma_gaps: vec![],
                        all_scalar: true,
                        expect_name: byte == b'{',
                    });
                    at += 1;
                }
                b'}' | b']' => {
                    if let Some(frame) = stack.pop() {
                        survey.close(frame, bytes, at, gap);
                    }
                    at += 1;
                }
                b':' => {
                    pending = Pending::Colon(gap);
                    at += 1;
                }
                b',' => {
                    pending = Pending::Comma;
                    if let Some(frame) = stack.last_mut()
                        && frame.object
                    {
                        frame.expect_name = true;
                    }
                    at += 1;
                }
                b'"' => {
                    let name = stack
                        .last()
                        .is_some_and(|frame| frame.object && frame.expect_name);
                    survey.child(&mut stack, bytes, gap, false, name);
                    at = string_end(bytes, at).unwrap_or(bytes.len());
                }
                _ => {
                    survey.child(&mut stack, bytes, gap, false, false);
                    at = token_end(bytes, at).max(at + 1);
                }
            }
        }
        survey
    }

    /// A value or member name begins, after whitespace `gap`.
    fn child(
        &mut self,
        stack: &mut [Frame],
        bytes: &[u8],
        gap: std::ops::Range<usize>,
        container: bool,
        name: bool,
    ) {
        let Some(frame) = stack.last_mut() else {
            return;
        };
        if frame.object {
            // In an object a member begins at its name; its value does not
            // begin a child.
            if !name {
                return;
            }
            frame.expect_name = false;
        }
        frame.children += 1;
        if frame.children == 1 {
            frame.first_gap = Some(gap.clone());
        }
        if container {
            frame.all_scalar = false;
        }
        let gap = &bytes[gap];
        if let Some(newline) = gap.iter().rposition(|byte| *byte == b'\n') {
            let indentation = &gap[newline + 1..];
            if let Some(step) = indentation.strip_prefix(frame.line_indent.as_slice())
                && let Ok(step) = std::str::from_utf8(step)
                && !step.is_empty()
                && let Ok(step) = Indent::new(step)
            {
                self.steps.add(step);
            }
        }
    }

    fn close(&mut self, frame: Frame, bytes: &[u8], close: usize, gap: std::ops::Range<usize>) {
        let single_line = !bytes[frame.open..close].contains(&b'\n');
        if frame.children == 0 {
            match &bytes[gap] {
                b"" => self.empty.add(Empty::Tight),
                b" " => self.empty.add(Empty::Spaced),
                _ => {}
            }
            return;
        }
        if single_line {
            if let Some(first) = frame.first_gap {
                match (&bytes[first], &bytes[gap]) {
                    (b"", b"") => self.padding.add(false),
                    (b" ", b" ") => self.padding.add(true),
                    _ => {}
                }
            }
            for comma in frame.comma_gaps {
                match &bytes[comma] {
                    b"" => self.comma.add(Comma::Tight),
                    b" " => self.comma.add(Comma::Spaced),
                    _ => {}
                }
            }
        } else {
            self.multi_line += 1;
        }
        if !frame.object && frame.all_scalar {
            self.inline.add(single_line);
        }
    }

    fn conclude(self, bytes: &[u8]) -> Detection {
        let pretty = self.multi_line > 0;
        let default = Style::default_for_new_table();
        let mut evidence = Evidence {
            indent: self.steps.total(),
            multi_line: self.multi_line,
            colon: self.colon.total(),
            comma: self.comma.total(),
            newline: self.lf + self.crlf,
            empty: self.empty.total(),
            padding: self.padding.total(),
            inline_scalar_arrays: 0,
        };
        let indent = if pretty {
            // An indented document whose members begin lines without any
            // measurable indentation step takes the default unit.
            self.steps.winner().unwrap_or(default.indent)
        } else {
            evidence.indent = 0;
            Indent::compact()
        };
        let comma = self.comma.winner();
        // Where one spacing was never observed, the other suggests it: a file
        // that spaces its colons spaces its commas.
        let colon = self
            .colon
            .winner()
            .or(comma.map(|comma| match comma {
                Comma::Tight => Colon::Tight,
                Comma::Spaced => Colon::Spaced,
            }))
            .unwrap_or(if pretty { Colon::Spaced } else { Colon::Tight });
        let comma = comma.unwrap_or(match colon {
            Colon::Tight => Comma::Tight,
            Colon::Spaced | Colon::Surrounded => Comma::Spaced,
        });
        let inline_scalar_arrays = if pretty {
            evidence.inline_scalar_arrays = self.inline.total();
            self.inline.winner().unwrap_or(false)
        } else {
            false
        };
        Detection {
            style: Style {
                indent,
                colon,
                comma,
                newline: if self.crlf > self.lf {
                    Newline::CrLf
                } else {
                    Newline::Lf
                },
                final_newline: bytes.last() == Some(&b'\n'),
                bom: self.bom,
                empty: self.empty.winner().unwrap_or(Empty::Tight),
                padding: self.padding.winner().unwrap_or(false),
                inline_scalar_arrays,
            },
            evidence,
        }
    }
}
