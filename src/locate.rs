//! Where, in a file's bytes, a JSON Pointer lands.
//!
//! Every diagnostic about a value inside a JSON document names the value by an
//! RFC 6901 pointer (`/feedback_rules/0/when_choice_index`). A person fixes the
//! file, not the pointer, so the diagnostic also carries the line and column of
//! that value. This module computes both from the bytes the document was read
//! from, once per document, so the answer is exact: searching for the first
//! occurrence of `"name"` -- which is what diagnostics used to do -- points at
//! the wrong place whenever a name repeats, which in real documents it always
//! does.
//!
//! The scan is iterative, so a deeply nested document cannot exhaust the stack,
//! and tolerant: on malformed input it records what it reached and stops. Its
//! callers only ask about documents the parser has already accepted.

use crate::diagnostic::Location;
use std::collections::HashMap;

/// Pointer → location of every value in one document.
#[derive(Debug, Default, Clone)]
pub struct Spans {
    /// Where each value begins.
    values: HashMap<String, Location>,
    /// Where the member name that introduces each object member begins.
    keys: HashMap<String, Location>,
}

impl Spans {
    /// Scan a document's bytes.
    pub fn of(raw: &[u8]) -> Self {
        let mut scanner = Scanner {
            raw,
            at: 0,
            line: 1,
            column: 1,
        };
        let mut spans = Self::default();
        scanner.document(&mut spans);
        spans
    }

    /// The location a diagnostic about `pointer` should point at.
    ///
    /// An object member is located at its name, because that is the line a
    /// reader scans for; an array element or the root at the value itself.
    pub fn location(&self, pointer: &str) -> Option<Location> {
        self.keys
            .get(pointer)
            .or_else(|| self.values.get(pointer))
            .cloned()
    }

    /// The location of the value itself, never of the member name.
    pub fn value_location(&self, pointer: &str) -> Option<Location> {
        self.values.get(pointer).cloned()
    }
}

enum Frame {
    Object { pointer: String, key: Option<String> },
    Array { pointer: String, index: usize, started: bool },
}

struct Scanner<'a> {
    raw: &'a [u8],
    at: usize,
    line: usize,
    column: usize,
}

impl Scanner<'_> {
    fn here(&self) -> Location {
        Location {
            line: self.line,
            column: self.column,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.raw.get(self.at).copied()
    }

    fn bump(&mut self) {
        let Some(byte) = self.peek() else { return };
        self.at += 1;
        if byte == b'\n' {
            self.line += 1;
            self.column = 1;
        } else if byte & 0xC0 != 0x80 {
            // Columns count characters: only a UTF-8 lead byte starts one.
            self.column += 1;
        }
    }

    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.bump();
        }
    }

    /// Consume a string and return its decoded text.
    fn string(&mut self) -> Option<String> {
        let start = self.at;
        self.bump(); // opening quote
        let mut escaped = false;
        while let Some(byte) = self.peek() {
            self.bump();
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                return serde_json::from_slice(&self.raw[start..self.at]).ok();
            }
        }
        None
    }

    /// Consume a number, `true`, `false` or `null`.
    fn scalar(&mut self) {
        while self.peek().is_some_and(|byte| {
            !matches!(byte, b',' | b']' | b'}' | b' ' | b'\t' | b'\n' | b'\r')
        }) {
            self.bump();
        }
    }

    fn document(&mut self, spans: &mut Spans) {
        let mut stack: Vec<Frame> = vec![];
        self.whitespace();
        let mut pointer = String::new();
        loop {
            // A value begins here, at `pointer`.
            self.whitespace();
            spans.values.insert(pointer.clone(), self.here());
            match self.peek() {
                Some(b'{') => {
                    self.bump();
                    stack.push(Frame::Object {
                        pointer: pointer.clone(),
                        key: None,
                    });
                }
                Some(b'[') => {
                    self.bump();
                    stack.push(Frame::Array {
                        pointer: pointer.clone(),
                        index: 0,
                        started: false,
                    });
                }
                Some(b'"') => {
                    if self.string().is_none() {
                        return;
                    }
                }
                Some(_) => self.scalar(),
                None => return,
            }
            // Find where the next value begins, closing containers on the way.
            loop {
                self.whitespace();
                let Some(frame) = stack.last_mut() else {
                    return;
                };
                match frame {
                    Frame::Object { pointer: parent, key } => {
                        match self.peek() {
                            Some(b'}') => {
                                self.bump();
                                stack.pop();
                                continue;
                            }
                            Some(b',') => {
                                self.bump();
                                self.whitespace();
                            }
                            // The first member follows the brace directly.
                            Some(b'"') if key.is_none() => {}
                            _ => return,
                        }
                        if self.peek() != Some(b'"') {
                            return;
                        }
                        let name_at = self.here();
                        let Some(name) = self.string() else { return };
                        let child = format!("{parent}/{}", crate::schema::path::escape_pointer(&name));
                        spans.keys.insert(child.clone(), name_at);
                        *key = Some(name);
                        self.whitespace();
                        if self.peek() != Some(b':') {
                            return;
                        }
                        self.bump();
                        pointer = child;
                        break;
                    }
                    Frame::Array {
                        pointer: parent,
                        index,
                        started,
                    } => {
                        match self.peek() {
                            Some(b']') => {
                                self.bump();
                                stack.pop();
                                continue;
                            }
                            Some(b',') if *started => {
                                self.bump();
                                *index += 1;
                            }
                            // The first element follows the bracket directly.
                            Some(_) if !*started => *started = true,
                            _ => return,
                        }
                        pointer = format!("{parent}/{index}");
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(spans: &Spans, pointer: &str) -> (usize, usize) {
        let location = spans
            .location(pointer)
            .unwrap_or_else(|| panic!("no location for {pointer}"));
        (location.line, location.column)
    }

    #[test]
    fn test2030_pointers_resolve_to_the_exact_member_or_element() {
        let raw = r#"{
  "id": "a",
  "feedback_rules": [
    { "message": "m", "when_choice_index": 1 },
    { "message": "n",
      "id": "nested" }
  ],
  "a/b": { "c~d": [10, [20, 30]] },
  "café": "é"
}"#.as_bytes();
        let spans = Spans::of(raw);
        assert_eq!(at(&spans, ""), (1, 1));
        assert_eq!(at(&spans, "/id"), (2, 3));
        assert_eq!(at(&spans, "/feedback_rules"), (3, 3));
        assert_eq!(spans.value_location("/feedback_rules/0").map(|l| (l.line, l.column)), Some((4, 5)));
        assert_eq!(at(&spans, "/feedback_rules/0/when_choice_index"), (4, 23));
        // A repeated member name resolves to its own occurrence, not the first.
        assert_eq!(at(&spans, "/feedback_rules/1/id"), (6, 7));
        assert_eq!(at(&spans, "/a~1b/c~0d/1"), (8, 24));
        assert_eq!(at(&spans, "/a~1b/c~0d/1/1"), (8, 29));
        // Escaped member names are decoded before they become pointer tokens.
        assert_eq!(at(&spans, "/café"), (9, 3));
        assert!(spans.location("/missing").is_none());
    }

    #[test]
    fn test2031_columns_count_characters_not_bytes() {
        let raw = "{\"é\": 1, \"x\": 2}".as_bytes();
        let spans = Spans::of(raw);
        assert_eq!(at(&spans, "/x"), (1, 10));
    }

    #[test]
    fn test2032_malformed_input_yields_what_was_reached() {
        let spans = Spans::of(br#"{"a": 1, "b": "#);
        assert_eq!(at(&spans, "/a"), (1, 2));
        assert_eq!(at(&spans, "/b"), (1, 10));
        let empty = Spans::of(b"");
        assert!(empty.location("/a").is_none());
    }
}
