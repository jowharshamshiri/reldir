//! The concrete syntax of one JSON document: where, in its bytes, every value,
//! member name, colon and separator sits.
//!
//! Validity is decided in one place for the whole crate. The bytes after an
//! optional byte order mark are first handed to [`crate::json::parse`], which
//! applies the strict rules -- duplicate member names, trailing content and
//! nesting beyond the configured depth limit are refused -- and only bytes it
//! accepts are scanned here. The scanner adds one rule of its own: a number
//! with no finite value (`1e999`) is refused, the rule the strict parser states.
//! The scanner therefore never decides what JSON is, and could not drift from
//! the parser; finding anything it does not expect in accepted bytes is a
//! defect, reported as [`EditError::Internal`].
//!
//! The scan is iterative and so is building the value from it, so a document
//! as deep as the depth limit allows cannot exhaust the stack here.

use super::{EditError, finite};
use serde_json::{Map, Value};
use std::{collections::HashMap, ops::Range};

/// The UTF-8 byte order mark. JSON forbids it, editors write it, and a file
/// that starts with one keeps it.
pub const BOM: &[u8] = b"\xEF\xBB\xBF";

/// A value's index in [`Document::node`]. The root is 0, and every container
/// precedes its children.
pub type NodeId = usize;

/// What kind of value a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Object,
    Array,
    String,
    Number,
    Bool,
    Null,
}

impl Kind {
    /// The kind as an error message names it.
    pub fn describe(self) -> &'static str {
        match self {
            Kind::Object => "an object",
            Kind::Array => "an array",
            Kind::String => "a string",
            Kind::Number => "a number",
            Kind::Bool => "a boolean",
            Kind::Null => "null",
        }
    }
}

/// One object member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The decoded name.
    pub name: String,
    /// The name as written, quotes included.
    pub name_span: Range<usize>,
    /// The position of the colon.
    pub colon: usize,
    /// The member's value.
    pub value: NodeId,
    /// From just after the `{` or `,` before the member -- so its leading
    /// whitespace is included -- to the end of its value.
    pub entry: Range<usize>,
}

/// One array element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub value: NodeId,
    /// From just after the `[` or `,` before the element to the end of its
    /// value.
    pub entry: Range<usize>,
}

/// The children of a node, in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Children {
    Scalar,
    Members(Vec<Member>),
    Elements(Vec<Element>),
}

/// One value in the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub kind: Kind,
    /// The value as written: from its first byte to just past its last.
    pub span: Range<usize>,
    /// The container holding this value and the value's position among that
    /// container's children; `None` for the root.
    pub parent: Option<(NodeId, usize)>,
    pub children: Children,
}

impl Node {
    /// How many members or elements the node has.
    pub fn len(&self) -> usize {
        match &self.children {
            Children::Scalar => 0,
            Children::Members(members) => members.len(),
            Children::Elements(elements) => elements.len(),
        }
    }

    /// Whether the node is a scalar or an empty container.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The entry of child `index`: its leading whitespace through its value.
    pub(super) fn child_entry(&self, index: usize) -> Range<usize> {
        match &self.children {
            Children::Scalar => unreachable!("a scalar has no children"),
            Children::Members(members) => members[index].entry.clone(),
            Children::Elements(elements) => elements[index].entry.clone(),
        }
    }

    /// Where child `index` begins: at its name for a member, at its value for
    /// an element.
    pub(super) fn child_start(&self, index: usize, nodes: &[Node]) -> usize {
        match &self.children {
            Children::Scalar => unreachable!("a scalar has no children"),
            Children::Members(members) => members[index].name_span.start,
            Children::Elements(elements) => nodes[elements[index].value].span.start,
        }
    }

    /// The value of child `index`.
    pub(super) fn child_value(&self, index: usize) -> NodeId {
        match &self.children {
            Children::Scalar => unreachable!("a scalar has no children"),
            Children::Members(members) => members[index].value,
            Children::Elements(elements) => elements[index].value,
        }
    }
}

/// A child placed in a rebuilt container: an original child by position, or
/// a new one.
#[derive(Debug, Clone, Copy)]
pub(super) enum Placed<'e> {
    Kept(usize),
    New {
        name: Option<&'e str>,
        value: &'e Value,
    },
}

/// One parsed document, with the bytes it was read from.
#[derive(Debug, Clone)]
pub struct Document<'a> {
    raw: &'a [u8],
    bom: bool,
    nodes: Vec<Node>,
    value: Value,
}

impl<'a> Document<'a> {
    /// Read a document, accepting a leading byte order mark.
    pub fn parse(raw: &'a [u8]) -> Result<Self, EditError> {
        let bom = raw.starts_with(BOM);
        let start = if bom { BOM.len() } else { 0 };
        // The strict parser reads past one mark itself: given the whole file,
        // it accepts exactly what the scanner below does.
        crate::json::parse(raw).map_err(EditError::Invalid)?;
        let nodes = scan(raw, start)?;
        let mut document = Self {
            raw,
            bom,
            nodes,
            value: Value::Null,
        };
        document.value = document.assemble(&HashMap::new(), &HashMap::new())?;
        Ok(document)
    }

    /// The bytes the document was read from.
    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }

    /// Whether the file begins with a byte order mark.
    pub fn bom(&self) -> bool {
        self.bom
    }

    /// The value the document holds. Numbers keep their written lexeme, and
    /// members their written order.
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// The root value.
    pub fn root(&self) -> NodeId {
        0
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    /// Every node, the root first and each container before its children.
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// The node an RFC 6901 pointer names.
    pub fn lookup(&self, pointer: &str) -> Result<NodeId, EditError> {
        let mut at = self.root();
        for token in pointer_tokens(pointer)? {
            let node = &self.nodes[at];
            at = match &node.children {
                Children::Members(members) => members
                    .iter()
                    .find(|member| member.name == token)
                    .map(|member| member.value)
                    .ok_or_else(|| EditError::Missing {
                        pointer: pointer.to_string(),
                    })?,
                Children::Elements(elements) => {
                    let index = array_index(&token, pointer)?;
                    elements
                        .get(index)
                        .map(|element| element.value)
                        .ok_or_else(|| EditError::Index {
                            pointer: pointer.to_string(),
                            index,
                            len: elements.len(),
                        })?
                }
                Children::Scalar => {
                    return Err(EditError::Missing {
                        pointer: pointer.to_string(),
                    });
                }
            };
        }
        Ok(at)
    }

    /// The RFC 6901 pointer of a node.
    pub fn pointer(&self, mut id: NodeId) -> String {
        let mut tokens = vec![];
        while let Some((parent, index)) = self.nodes[id].parent {
            tokens.push(match &self.nodes[parent].children {
                Children::Members(members) => {
                    crate::schema::path::escape_pointer(&members[index].name)
                }
                _ => index.to_string(),
            });
            id = parent;
        }
        tokens
            .iter()
            .rev()
            .fold(String::new(), |pointer, token| format!("{pointer}/{token}"))
    }

    /// The leading spaces and tabs of the line that `position` is on.
    pub(super) fn line_indent(&self, position: usize) -> &'a [u8] {
        line_indent(self.raw, position)
    }

    /// Whether no line break falls inside `span`.
    pub(super) fn single_line(&self, span: Range<usize>) -> bool {
        !self.raw[span].contains(&b'\n')
    }

    /// Whether child `index` of `container` begins its own line.
    pub(super) fn starts_own_line(&self, container: NodeId, index: usize) -> bool {
        let node = &self.nodes[container];
        let lead = node.child_entry(index).start..node.child_start(index, &self.nodes);
        self.raw[lead].contains(&b'\n')
    }

    /// The value the document holds after replacing the values in `replaced`
    /// and rearranging the containers in `arranged`. Built from the leaves up,
    /// without recursion.
    pub(super) fn assemble(
        &self,
        replaced: &HashMap<NodeId, &Value>,
        arranged: &HashMap<NodeId, Vec<Placed<'_>>>,
    ) -> Result<Value, EditError> {
        let mut built: Vec<Option<Value>> = vec![None; self.nodes.len()];
        for id in (0..self.nodes.len()).rev() {
            let node = &self.nodes[id];
            let value = if let Some(value) = replaced.get(&id) {
                (*value).clone()
            } else {
                match &node.children {
                    Children::Scalar => self.scalar(node)?,
                    Children::Members(members) => {
                        let mut object = Map::new();
                        let mut add = |name: String, value: Value| {
                            if object.contains_key(&name) {
                                return Err(EditError::internal(format!(
                                    "member {name:?} placed twice in {}",
                                    self.pointer(id)
                                )));
                            }
                            object.insert(name, value);
                            Ok(())
                        };
                        match arranged.get(&id) {
                            None => {
                                for member in members {
                                    add(member.name.clone(), take(&mut built, member.value)?)?;
                                }
                            }
                            Some(placed) => {
                                for place in placed {
                                    match place {
                                        Placed::Kept(index) => {
                                            let member = &members[*index];
                                            add(
                                                member.name.clone(),
                                                take(&mut built, member.value)?,
                                            )?;
                                        }
                                        Placed::New {
                                            name: Some(name),
                                            value,
                                        } => add((*name).to_string(), (*value).clone())?,
                                        Placed::New { name: None, .. } => {
                                            return Err(EditError::internal(
                                                "an unnamed member was placed in an object",
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        Value::Object(object)
                    }
                    Children::Elements(elements) => {
                        let mut array = Vec::new();
                        match arranged.get(&id) {
                            None => {
                                for element in elements {
                                    array.push(take(&mut built, element.value)?);
                                }
                            }
                            Some(placed) => {
                                for place in placed {
                                    match place {
                                        Placed::Kept(index) => {
                                            array.push(take(&mut built, elements[*index].value)?)
                                        }
                                        Placed::New { name: None, value } => {
                                            array.push((*value).clone())
                                        }
                                        Placed::New { name: Some(_), .. } => {
                                            return Err(EditError::internal(
                                                "a named member was placed in an array",
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        Value::Array(array)
                    }
                }
            };
            built[id] = Some(value);
        }
        take(&mut built, self.root())
    }

    fn scalar(&self, node: &Node) -> Result<Value, EditError> {
        let text = &self.raw[node.span.clone()];
        match node.kind {
            Kind::String => serde_json::from_slice::<String>(text)
                .map(Value::String)
                .map_err(|error| {
                    EditError::internal(format!("an accepted string does not decode: {error}"))
                }),
            // Read as serde_json reads a value, which keeps the lexeme -- `-0`
            // included, which `Number::from_str` would turn into `0`.
            Kind::Number => match serde_json::from_slice::<Value>(text) {
                Ok(number @ Value::Number(_)) => Ok(number),
                _ => Err(EditError::internal("an accepted number does not read back")),
            },
            Kind::Bool => Ok(Value::Bool(text == b"true")),
            Kind::Null => Ok(Value::Null),
            Kind::Object | Kind::Array => {
                Err(EditError::internal("a container was read as a scalar"))
            }
        }
    }
}

fn take(built: &mut [Option<Value>], id: NodeId) -> Result<Value, EditError> {
    built[id]
        .take()
        .ok_or_else(|| EditError::internal(format!("node {id} was used before it was built")))
}

/// The unescaped reference tokens of an RFC 6901 pointer: empty for the
/// whole document, otherwise `/`-separated, with `~1` for `/` and `~0` for
/// `~`. Any other use of `~` is refused rather than read loosely.
pub(super) fn pointer_tokens(pointer: &str) -> Result<Vec<String>, EditError> {
    if pointer.is_empty() {
        return Ok(vec![]);
    }
    let Some(rest) = pointer.strip_prefix('/') else {
        return Err(EditError::Pointer {
            pointer: pointer.to_string(),
            reason: "a non-empty pointer begins with '/'",
        });
    };
    rest.split('/')
        .map(|token| {
            let mut out = String::with_capacity(token.len());
            let mut chars = token.chars();
            while let Some(c) = chars.next() {
                if c != '~' {
                    out.push(c);
                    continue;
                }
                match chars.next() {
                    Some('0') => out.push('~'),
                    Some('1') => out.push('/'),
                    _ => {
                        return Err(EditError::Pointer {
                            pointer: pointer.to_string(),
                            reason: "'~' is followed by '0' or '1'",
                        });
                    }
                }
            }
            Ok(out)
        })
        .collect()
}

/// An array index token: decimal digits without a leading zero. `-`, which
/// RFC 6901 reserves for the position past the end, names no element.
pub(super) fn array_index(token: &str, pointer: &str) -> Result<usize, EditError> {
    let digits = !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit());
    if !digits || (token.len() > 1 && token.starts_with('0')) {
        return Err(EditError::Pointer {
            pointer: pointer.to_string(),
            reason: "an array element is named by a decimal index without leading zeros",
        });
    }
    token.parse().map_err(|_| EditError::Pointer {
        pointer: pointer.to_string(),
        reason: "the array index does not fit in memory",
    })
}

pub(super) fn line_indent(raw: &[u8], position: usize) -> &[u8] {
    let start = raw[..position]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1);
    let len = raw[start..]
        .iter()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count();
    &raw[start..start + len]
}

pub(super) fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_whitespace(raw: &[u8], mut at: usize) -> usize {
    while raw.get(at).copied().is_some_and(is_whitespace) {
        at += 1;
    }
    at
}

/// Just past the string that opens at `at`.
pub(super) fn string_end(raw: &[u8], at: usize) -> Option<usize> {
    let mut escaped = false;
    for (offset, byte) in raw.get(at + 1..)?.iter().enumerate() {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            return Some(at + 1 + offset + 1);
        }
    }
    None
}

/// Just past the number or literal that begins at `at`.
pub(super) fn token_end(raw: &[u8], mut at: usize) -> usize {
    while raw
        .get(at)
        .is_some_and(|byte| !is_whitespace(*byte) && !matches!(byte, b',' | b']' | b'}'))
    {
        at += 1;
    }
    at
}

/// Where the value about to be read goes.
enum Slot {
    Root,
    Member {
        name: String,
        name_span: Range<usize>,
        colon: usize,
        entry_start: usize,
    },
    Element {
        entry_start: usize,
    },
}

fn unexpected(raw: &[u8], at: usize, wanted: &str) -> EditError {
    EditError::internal(format!(
        "the scanner expected {wanted} at byte {at} of an accepted document, found {:?}",
        raw.get(at).map(|byte| *byte as char)
    ))
}

/// The slot of the child that begins after the `{`, `[` or `,` just before
/// `entry_start`; for an object, reads the member's name and colon.
fn child_slot(raw: &[u8], object: bool, entry_start: usize) -> Result<(Slot, usize), EditError> {
    if !object {
        return Ok((Slot::Element { entry_start }, entry_start));
    }
    let name_start = skip_whitespace(raw, entry_start);
    if raw.get(name_start) != Some(&b'"') {
        return Err(unexpected(raw, name_start, "a member name"));
    }
    let name_end =
        string_end(raw, name_start).ok_or_else(|| unexpected(raw, name_start, "a closed name"))?;
    let name = serde_json::from_slice::<String>(&raw[name_start..name_end]).map_err(|error| {
        EditError::internal(format!("an accepted name does not decode: {error}"))
    })?;
    let colon = skip_whitespace(raw, name_end);
    if raw.get(colon) != Some(&b':') {
        return Err(unexpected(raw, colon, "a colon"));
    }
    Ok((
        Slot::Member {
            name,
            name_span: name_start..name_end,
            colon,
            entry_start,
        },
        colon + 1,
    ))
}

fn scan(raw: &[u8], start: usize) -> Result<Vec<Node>, EditError> {
    let mut nodes: Vec<Node> = vec![];
    // The containers opened and not yet closed.
    let mut open: Vec<NodeId> = vec![];
    let mut slot = Slot::Root;
    let mut at = start;
    loop {
        // A value begins at the next non-whitespace byte.
        at = skip_whitespace(raw, at);
        let begin = at;
        let byte = *raw
            .get(begin)
            .ok_or_else(|| unexpected(raw, begin, "a value"))?;
        let (kind, children, end) = match byte {
            b'{' => (Kind::Object, Children::Members(vec![]), None),
            b'[' => (Kind::Array, Children::Elements(vec![]), None),
            b'"' => (
                Kind::String,
                Children::Scalar,
                Some(string_end(raw, begin).ok_or_else(|| unexpected(raw, begin, "a string"))?),
            ),
            b't' | b'f' => (Kind::Bool, Children::Scalar, Some(token_end(raw, begin))),
            b'n' => (Kind::Null, Children::Scalar, Some(token_end(raw, begin))),
            b'-' | b'0'..=b'9' => {
                let end = token_end(raw, begin);
                let lexeme = std::str::from_utf8(&raw[begin..end])
                    .map_err(|_| unexpected(raw, begin, "an ASCII number"))?;
                if !finite(lexeme) {
                    return Err(EditError::Invalid(serde::de::Error::custom(format!(
                        "JSON number is not finite: {lexeme} at byte {begin}"
                    ))));
                }
                (Kind::Number, Children::Scalar, Some(end))
            }
            _ => return Err(unexpected(raw, begin, "a value")),
        };
        let id = nodes.len();
        let parent = match (std::mem::replace(&mut slot, Slot::Root), open.last()) {
            (Slot::Root, None) => None,
            (
                Slot::Member {
                    name,
                    name_span,
                    colon,
                    entry_start,
                },
                Some(&container),
            ) => match &mut nodes[container].children {
                Children::Members(members) => {
                    members.push(Member {
                        name,
                        name_span,
                        colon,
                        value: id,
                        entry: entry_start..entry_start,
                    });
                    Some((container, members.len() - 1))
                }
                _ => return Err(EditError::internal("a member was read into an array")),
            },
            (Slot::Element { entry_start }, Some(&container)) => {
                match &mut nodes[container].children {
                    Children::Elements(elements) => {
                        elements.push(Element {
                            value: id,
                            entry: entry_start..entry_start,
                        });
                        Some((container, elements.len() - 1))
                    }
                    _ => return Err(EditError::internal("an element was read into an object")),
                }
            }
            _ => {
                return Err(EditError::internal(
                    "a value was read outside its container",
                ));
            }
        };
        nodes.push(Node {
            kind,
            span: begin..end.unwrap_or(begin),
            parent,
            children,
        });
        match end {
            Some(end) => at = end,
            None => {
                let inner = skip_whitespace(raw, begin + 1);
                let close = if byte == b'{' { b'}' } else { b']' };
                if raw.get(inner) == Some(&close) {
                    nodes[id].span.end = inner + 1;
                    at = inner + 1;
                } else {
                    open.push(id);
                    let (next, value_at) = child_slot(raw, byte == b'{', begin + 1)?;
                    slot = next;
                    at = value_at;
                    continue;
                }
            }
        }
        // After a value: separators and closing brackets, until the next
        // value begins or the document ends.
        loop {
            let Some(&container) = open.last() else {
                if skip_whitespace(raw, at) != raw.len() {
                    return Err(unexpected(raw, at, "the end of the document"));
                }
                return Ok(finish(nodes));
            };
            at = skip_whitespace(raw, at);
            match raw.get(at) {
                Some(b',') => {
                    let object = nodes[container].kind == Kind::Object;
                    let (next, value_at) = child_slot(raw, object, at + 1)?;
                    slot = next;
                    at = value_at;
                    break;
                }
                Some(b'}' | b']') => {
                    nodes[container].span.end = at + 1;
                    open.pop();
                    at += 1;
                }
                _ => return Err(unexpected(raw, at, "',' or a closing bracket")),
            }
        }
    }
}

/// Close every entry at the end of its value, now that every value's end is
/// known.
fn finish(mut nodes: Vec<Node>) -> Vec<Node> {
    let ends: Vec<usize> = nodes.iter().map(|node| node.span.end).collect();
    for node in &mut nodes {
        match &mut node.children {
            Children::Scalar => {}
            Children::Members(members) => {
                for member in members {
                    member.entry.end = ends[member.value];
                }
            }
            Children::Elements(elements) => {
                for element in elements {
                    element.entry.end = ends[element.value];
                }
            }
        }
    }
    nodes
}
