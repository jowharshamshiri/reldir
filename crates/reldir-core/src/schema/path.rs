//! Reference paths: where, inside a row, a reference lives.
//!
//! A foreign key names the values that must point at existing rows. A value can
//! sit anywhere in a row -- a top-level column, each element of an array column,
//! a member of an object nested in an array of objects -- so a key names it by
//! a path rather than by a column:
//!
//! ```text
//! path     = segment *( "." segment )
//! segment  = name *( "[]" / filter )
//! filter   = "[?" name "=" literal "]"
//! name     = identifier / quoted
//! identifier = ( ALPHA / "_" ) *( ALPHA / DIGIT / "_" )
//! quoted   = DQUOTE *json-char DQUOTE          ; a JSON string
//! literal  = "'" *( %x00-26 / %x28-10FFFF / "''" ) "'"
//!          / number / "true" / "false" / "null"
//! ```
//!
//! `[]` visits every element of an array. `[?field='value']` visits the
//! elements that are objects whose `field` equals the literal, so a labelled
//! edge list can say which of its edges are references of one kind:
//! `relations[?relation='requires'].target`.
//!
//! A path always ends at a scalar. Null and absent values along the way make no
//! reference, exactly as a null column does not.

use serde_json::{Map, Value};
use std::fmt;

/// One step of a path.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Descend into an object member.
    Member(String),
    /// Visit every element of an array.
    Each,
    /// Visit the elements of an array that are objects whose member equals a
    /// literal.
    Where { member: String, equals: Value },
}

/// A parsed reference path.
#[derive(Debug, Clone, PartialEq)]
pub struct RefPath {
    steps: Vec<Step>,
}

/// One place a path reaches inside a row.
#[derive(Debug, Clone, PartialEq)]
pub struct Occurrence<'a> {
    /// RFC 6901 pointer to the value.
    pub pointer: String,
    /// The referencing value itself.
    pub value: &'a Value,
    /// Pointer to the array element that holds this occurrence, when the path
    /// crossed an array. Removing the reference removes that element: for
    /// `relations[].target` it is the whole edge, for `tags[]` the tag.
    pub element: Option<String>,
}

/// A path that does not parse, with the byte offset of the fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at offset {}", self.message, self.offset)
    }
}

impl RefPath {
    pub fn parse(text: &str) -> Result<Self, PathError> {
        let mut parser = Parser {
            text,
            bytes: text.as_bytes(),
            at: 0,
        };
        let steps = parser.path()?;
        Ok(Self { steps })
    }

    /// A path from its steps; `None` unless it begins with a member, as every
    /// path does.
    pub fn from_steps(steps: Vec<Step>) -> Option<Self> {
        matches!(steps.first(), Some(Step::Member(_))).then_some(Self { steps })
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// The top-level column the path starts from.
    pub fn column(&self) -> &str {
        match self.steps.first() {
            Some(Step::Member(name)) => name,
            // `parse` only ever produces a path that begins with a member.
            _ => unreachable!("a reference path always begins with a column name"),
        }
    }

    /// Whether the path visits array elements, so one row can hold many
    /// references through it.
    pub fn iterates(&self) -> bool {
        self.steps
            .iter()
            .any(|step| !matches!(step, Step::Member(_)))
    }

    /// The same path with its first member renamed. Used when a column is
    /// renamed, so a key that names it keeps naming it.
    pub fn with_column(&self, column: &str) -> Self {
        let mut steps = self.steps.clone();
        steps[0] = Step::Member(column.to_string());
        Self { steps }
    }

    /// Every non-null value this path reaches in a row, in document order.
    pub fn occurrences<'a>(&self, row: &'a Map<String, Value>) -> Vec<Occurrence<'a>> {
        let mut out = vec![];
        let Some(Step::Member(first)) = self.steps.first() else {
            return out;
        };
        let Some(value) = row.get(first) else {
            return out;
        };
        let pointer = format!("/{}", escape_pointer(first));
        walk(&self.steps[1..], value, pointer, None, &mut out);
        out
    }
}

fn walk<'a>(
    steps: &[Step],
    value: &'a Value,
    pointer: String,
    element: Option<String>,
    out: &mut Vec<Occurrence<'a>>,
) {
    if value.is_null() {
        return;
    }
    let Some((step, rest)) = steps.split_first() else {
        out.push(Occurrence {
            pointer,
            value,
            element,
        });
        return;
    };
    match step {
        Step::Member(name) => {
            if let Some(child) = value.as_object().and_then(|object| object.get(name)) {
                walk(
                    rest,
                    child,
                    format!("{pointer}/{}", escape_pointer(name)),
                    element,
                    out,
                );
            }
        }
        Step::Each => {
            if let Some(items) = value.as_array() {
                for (index, item) in items.iter().enumerate() {
                    let at = format!("{pointer}/{index}");
                    walk(rest, item, at.clone(), Some(at), out);
                }
            }
        }
        Step::Where { member, equals } => {
            if let Some(items) = value.as_array() {
                for (index, item) in items.iter().enumerate() {
                    let selected = item
                        .as_object()
                        .and_then(|object| object.get(member))
                        .is_some_and(|candidate| candidate == equals);
                    if selected {
                        let at = format!("{pointer}/{index}");
                        walk(rest, item, at.clone(), Some(at), out);
                    }
                }
            }
        }
    }
}

/// Escape one reference token for a JSON Pointer.
pub fn escape_pointer(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// Split a JSON Pointer into its unescaped reference tokens.
pub fn pointer_tokens(pointer: &str) -> Vec<String> {
    if pointer.is_empty() {
        return vec![];
    }
    pointer
        .trim_start_matches('/')
        .split('/')
        .map(|token| token.replace("~1", "/").replace("~0", "~"))
        .collect()
}

impl fmt::Display for RefPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, step) in self.steps.iter().enumerate() {
            match step {
                Step::Member(name) => {
                    if index > 0 {
                        f.write_str(".")?;
                    }
                    write_name(f, name)?;
                }
                Step::Each => f.write_str("[]")?,
                Step::Where { member, equals } => {
                    f.write_str("[?")?;
                    write_name(f, member)?;
                    f.write_str("=")?;
                    write_literal(f, equals)?;
                    f.write_str("]")?;
                }
            }
        }
        Ok(())
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn write_name(f: &mut fmt::Formatter<'_>, name: &str) -> fmt::Result {
    if is_identifier(name) {
        f.write_str(name)
    } else {
        f.write_str(&serde_json::to_string(name).map_err(|_| fmt::Error)?)
    }
}

fn write_literal(f: &mut fmt::Formatter<'_>, value: &Value) -> fmt::Result {
    match value {
        Value::String(text) => write!(f, "'{}'", text.replace('\'', "''")),
        other => f.write_str(&other.to_string()),
    }
}

struct Parser<'t> {
    text: &'t str,
    bytes: &'t [u8],
    at: usize,
}

impl Parser<'_> {
    fn fail<T>(&self, message: impl Into<String>) -> Result<T, PathError> {
        Err(PathError {
            offset: self.at,
            message: message.into(),
        })
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn eat(&mut self, literal: &str) -> bool {
        if self.text[self.at..].starts_with(literal) {
            self.at += literal.len();
            true
        } else {
            false
        }
    }

    fn path(&mut self) -> Result<Vec<Step>, PathError> {
        if self.text.is_empty() {
            return self.fail("a reference path cannot be empty");
        }
        let mut steps = vec![Step::Member(self.name()?)];
        self.suffixes(&mut steps)?;
        while self.at < self.bytes.len() {
            if !self.eat(".") {
                return self.fail(format!(
                    "expected \".\", \"[]\" or \"[?\", found {:?}",
                    &self.text[self.at..]
                ));
            }
            steps.push(Step::Member(self.name()?));
            self.suffixes(&mut steps)?;
        }
        Ok(steps)
    }

    fn suffixes(&mut self, steps: &mut Vec<Step>) -> Result<(), PathError> {
        loop {
            if self.eat("[]") {
                steps.push(Step::Each);
            } else if self.eat("[?") {
                let member = self.name()?;
                if !self.eat("=") {
                    return self.fail("a filter compares a member with \"=\"");
                }
                let equals = self.literal()?;
                if !self.eat("]") {
                    return self.fail("a filter ends with \"]\"");
                }
                steps.push(Step::Where { member, equals });
            } else if self.peek() == Some(b'[') {
                return self.fail("an array step is \"[]\" or a filter \"[?member=literal]\"");
            } else {
                return Ok(());
            }
        }
    }

    fn name(&mut self) -> Result<String, PathError> {
        match self.peek() {
            Some(b'"') => {
                let start = self.at;
                let mut end = start + 1;
                let mut escaped = false;
                while let Some(&byte) = self.bytes.get(end) {
                    end += 1;
                    if escaped {
                        escaped = false;
                    } else if byte == b'\\' {
                        escaped = true;
                    } else if byte == b'"' {
                        let name: String = serde_json::from_str(&self.text[start..end])
                            .or_else(|error| self.fail(format!("invalid quoted name: {error}")))?;
                        if name.is_empty() {
                            return self.fail("a name cannot be empty");
                        }
                        self.at = end;
                        return Ok(name);
                    }
                }
                self.fail("unterminated quoted name")
            }
            Some(byte) if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = self.at;
                while self
                    .peek()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    self.at += 1;
                }
                Ok(self.text[start..self.at].to_string())
            }
            _ => self.fail("expected a member name"),
        }
    }

    fn literal(&mut self) -> Result<Value, PathError> {
        if self.eat("'") {
            let mut out = String::new();
            loop {
                let Some(ch) = self.text[self.at..].chars().next() else {
                    return self.fail("unterminated string literal");
                };
                self.at += ch.len_utf8();
                if ch == '\'' {
                    if self.eat("'") {
                        out.push('\'');
                    } else {
                        return Ok(Value::String(out));
                    }
                } else {
                    out.push(ch);
                }
            }
        }
        for (word, value) in [
            ("true", Value::Bool(true)),
            ("false", Value::Bool(false)),
            ("null", Value::Null),
        ] {
            if self.eat(word) {
                return Ok(value);
            }
        }
        let start = self.at;
        while self.peek().is_some_and(|byte| {
            byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E')
        }) {
            self.at += 1;
        }
        if start == self.at {
            return self.fail("expected a literal: a quoted string, number, true, false or null");
        }
        match serde_json::from_str::<Value>(&self.text[start..self.at]) {
            Ok(value @ Value::Number(_)) => Ok(value),
            _ => {
                self.at = start;
                self.fail("invalid number literal")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn test2000_paths_parse_and_print_canonically() {
        for text in [
            "user_id",
            "objective_refs[]",
            "modules[].lessons[].lesson_ref",
            "relations[?relation='requires'].target",
            "scoring.rubric_ref",
            "matrix[][]",
            "\"odd name\".x",
            "edges[?weight=2].to",
            "edges[?note='it''s'].to",
            "flags[?on=true].name",
        ] {
            let parsed = RefPath::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(parsed.to_string(), text, "canonical rendering of {text}");
            assert_eq!(RefPath::parse(&parsed.to_string()).unwrap(), parsed);
        }
    }

    #[test]
    fn test2001_malformed_paths_are_refused_with_an_offset() {
        for (text, offset) in [
            ("", 0),
            ("a.", 2),
            ("a[", 1),
            ("a[x]", 1),
            ("a[?b]", 4),
            ("a[?b='x'", 8),
            ("1abc", 0),
            ("a..b", 2),
            ("a[?b=bogus]", 5),
        ] {
            let error = RefPath::parse(text).expect_err(text);
            assert_eq!(error.offset, offset, "{text}: {error}");
        }
    }

    #[test]
    fn test2002_occurrences_follow_arrays_filters_and_skip_nulls() {
        let data = row(json!({
            "id": "c1",
            "tags": ["a", null, "b"],
            "modules": [
                {"lessons": [{"lesson_ref": "l1"}, {"lesson_ref": null}]},
                {"lessons": [{"lesson_ref": "l2"}]},
                {}
            ],
            "relations": [
                {"target": "x", "relation": "requires"},
                {"target": "y", "relation": "related_to"},
                {"target": "z", "relation": "requires"}
            ],
            "scoring": {"rubric_ref": "r1"}
        }));

        let tags: Vec<_> = RefPath::parse("tags[]").unwrap().occurrences(&data);
        assert_eq!(
            tags.iter()
                .map(|o| (o.pointer.as_str(), o.value.as_str().unwrap()))
                .collect::<Vec<_>>(),
            vec![("/tags/0", "a"), ("/tags/2", "b")]
        );
        assert_eq!(tags[1].element.as_deref(), Some("/tags/2"));

        let lessons = RefPath::parse("modules[].lessons[].lesson_ref")
            .unwrap()
            .occurrences(&data);
        assert_eq!(
            lessons
                .iter()
                .map(|o| o.pointer.as_str())
                .collect::<Vec<_>>(),
            vec![
                "/modules/0/lessons/0/lesson_ref",
                "/modules/1/lessons/0/lesson_ref"
            ]
        );
        // The innermost array element is what a removal takes out.
        assert_eq!(lessons[0].element.as_deref(), Some("/modules/0/lessons/0"));

        let requires = RefPath::parse("relations[?relation='requires'].target")
            .unwrap()
            .occurrences(&data);
        assert_eq!(
            requires
                .iter()
                .map(|o| o.value.as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["x", "z"]
        );
        assert_eq!(requires[1].element.as_deref(), Some("/relations/2"));

        let rubric = RefPath::parse("scoring.rubric_ref")
            .unwrap()
            .occurrences(&data);
        assert_eq!(rubric.len(), 1);
        assert_eq!(rubric[0].element, None, "no array was crossed");

        // A path whose column is absent reaches nothing.
        assert!(
            RefPath::parse("missing[]")
                .unwrap()
                .occurrences(&data)
                .is_empty()
        );
    }

    #[test]
    fn test2003_pointer_tokens_round_trip_through_escaping() {
        for token in ["plain", "a/b", "a~b", "~/~1"] {
            let pointer = format!("/{}", escape_pointer(token));
            assert_eq!(pointer_tokens(&pointer), vec![token.to_string()]);
        }
        assert!(pointer_tokens("").is_empty());
    }

    #[test]
    fn test2004_renaming_the_column_keeps_the_rest_of_the_path() {
        let path = RefPath::parse("rels[?kind='x'].to").unwrap();
        assert_eq!(path.with_column("links").to_string(), "links[?kind='x'].to");
        assert_eq!(path.column(), "rels");
        assert!(path.iterates());
        assert!(!RefPath::parse("a.b").unwrap().iterates());
    }
}
