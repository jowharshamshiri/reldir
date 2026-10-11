//! Writing a value in a style.
//!
//! Only new content is rendered; everything already in a file keeps its own
//! bytes. What is rendered is exact: members in the order the value holds them
//! (the crate's maps preserve order), numbers as their lexeme (the crate reads
//! numbers with arbitrary precision, so `1.50` stays `1.50`), and strings as
//! their characters, escaping only what JSON requires -- the quote, the
//! backslash and control characters -- so no character is ever escaped or
//! normalized that the value did not ask for.
//!
//! Rendering walks the value with an explicit stack, so any value the depth
//! limit admits renders without recursion.

use super::style::Style;
use serde_json::Value;

/// A value in `style`, its nested lines indented `base_indent_level` levels
/// deeper than the line it starts on.
pub fn render(value: &Value, style: &Style, base_indent_level: usize) -> Vec<u8> {
    let base = style.indent.as_str().repeat(base_indent_level);
    let mut out = vec![];
    write_value(&mut out, value, style, base.as_bytes());
    out
}

/// A whole file holding `value`: the byte order mark and final newline the
/// style calls for around the value.
pub fn render_document(value: &Value, style: &Style) -> Vec<u8> {
    let mut out = vec![];
    if style.bom {
        out.extend_from_slice(super::BOM);
    }
    write_value(&mut out, value, style, b"");
    if style.final_newline {
        out.extend_from_slice(style.newline.text().as_bytes());
    }
    out
}

enum Task<'v> {
    Value {
        value: &'v Value,
        depth: usize,
        inline: bool,
    },
    Name(&'v str),
    Separator {
        depth: usize,
        inline: bool,
    },
    Close {
        bracket: u8,
        depth: usize,
        inline: bool,
    },
}

/// Write `value` with nested lines starting with `base` and then one
/// indentation unit per level.
pub(super) fn write_value(out: &mut Vec<u8>, value: &Value, style: &Style, base: &[u8]) {
    let line = |out: &mut Vec<u8>, depth: usize| {
        out.extend_from_slice(style.newline.text().as_bytes());
        out.extend_from_slice(base);
        for _ in 0..depth {
            out.extend_from_slice(style.indent.as_str().as_bytes());
        }
    };
    let mut work = vec![Task::Value {
        value,
        depth: 0,
        inline: style.is_compact(),
    }];
    while let Some(task) = work.pop() {
        match task {
            Task::Name(name) => {
                write_string(out, name);
                out.extend_from_slice(style.colon.text().as_bytes());
            }
            Task::Separator { depth, inline } => {
                if inline {
                    out.extend_from_slice(style.comma.text().as_bytes());
                } else {
                    out.push(b',');
                    line(out, depth);
                }
            }
            Task::Close {
                bracket,
                depth,
                inline,
            } => {
                if inline {
                    if style.padding {
                        out.push(b' ');
                    }
                } else {
                    line(out, depth);
                }
                out.push(bracket);
            }
            Task::Value {
                value,
                depth,
                inline,
            } => match value {
                Value::Null => out.extend_from_slice(b"null"),
                Value::Bool(true) => out.extend_from_slice(b"true"),
                Value::Bool(false) => out.extend_from_slice(b"false"),
                Value::Number(number) => out.extend_from_slice(number.to_string().as_bytes()),
                Value::String(text) => write_string(out, text),
                Value::Object(members) if members.is_empty() => {
                    out.push(b'{');
                    out.extend_from_slice(style.empty.inner().as_bytes());
                    out.push(b'}');
                }
                Value::Array(elements) if elements.is_empty() => {
                    out.push(b'[');
                    out.extend_from_slice(style.empty.inner().as_bytes());
                    out.push(b']');
                }
                Value::Object(members) => {
                    open(out, b'{', inline, depth, style, &line);
                    work.push(Task::Close {
                        bracket: b'}',
                        depth,
                        inline,
                    });
                    for (index, (name, value)) in members.iter().enumerate().rev() {
                        work.push(Task::Value {
                            value,
                            depth: depth + 1,
                            inline,
                        });
                        work.push(Task::Name(name));
                        if index > 0 {
                            work.push(Task::Separator {
                                depth: depth + 1,
                                inline,
                            });
                        }
                    }
                }
                Value::Array(elements) => {
                    let inline = inline
                        || (style.inline_scalar_arrays
                            && elements
                                .iter()
                                .all(|element| !element.is_object() && !element.is_array()));
                    open(out, b'[', inline, depth, style, &line);
                    work.push(Task::Close {
                        bracket: b']',
                        depth,
                        inline,
                    });
                    for (index, value) in elements.iter().enumerate().rev() {
                        work.push(Task::Value {
                            value,
                            depth: depth + 1,
                            inline,
                        });
                        if index > 0 {
                            work.push(Task::Separator {
                                depth: depth + 1,
                                inline,
                            });
                        }
                    }
                }
            },
        }
    }
}

fn open(
    out: &mut Vec<u8>,
    bracket: u8,
    inline: bool,
    depth: usize,
    style: &Style,
    line: &impl Fn(&mut Vec<u8>, usize),
) {
    out.push(bracket);
    if inline {
        if style.padding {
            out.push(b' ');
        }
    } else {
        line(out, depth + 1);
    }
}

/// A JSON string holding exactly `text`: `"` and `\` escaped, control
/// characters escaped by their short form where JSON has one and as `\u00XX`
/// otherwise, and every other character -- non-ASCII included -- as itself.
pub(super) fn write_string(out: &mut Vec<u8>, text: &str) {
    out.push(b'"');
    for byte in text.bytes() {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0C => out.extend_from_slice(b"\\f"),
            0x00..=0x1F => out.extend_from_slice(format!("\\u{byte:04x}").as_bytes()),
            // Bytes of a multi-byte character are all 0x80 or above, so
            // copying them one at a time copies the character unchanged.
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}
